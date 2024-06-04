// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use arrow::ipc::writer::IpcWriteOptions;
use arrow::record_batch::RecordBatch;
use arrow_flight::decode::FlightRecordBatchStream;
use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::error::FlightError;
use arrow_flight::flight_descriptor::DescriptorType;
use arrow_flight::flight_service_server::FlightService;
use arrow_flight::sql::server::{FlightSqlService, PeekableFlightDataStream};
use arrow_flight::sql::{
    ActionBeginSavepointRequest, ActionBeginSavepointResult, ActionBeginTransactionRequest,
    ActionBeginTransactionResult, ActionCancelQueryRequest, ActionCancelQueryResult,
    ActionClosePreparedStatementRequest, ActionCreatePreparedStatementRequest,
    ActionCreatePreparedStatementResult, ActionCreatePreparedSubstraitPlanRequest,
    ActionEndSavepointRequest, ActionEndTransactionRequest, Any, CommandGetCatalogs,
    CommandGetCrossReference, CommandGetDbSchemas, CommandGetExportedKeys, CommandGetImportedKeys,
    CommandGetPrimaryKeys, CommandGetSqlInfo, CommandGetTableTypes, CommandGetTables,
    CommandGetXdbcTypeInfo, CommandPreparedStatementQuery, CommandPreparedStatementUpdate,
    CommandStatementQuery, CommandStatementSubstraitPlan, CommandStatementUpdate, ProstMessageExt,
    SqlInfo, TicketStatementQuery,
};
use arrow_flight::{
    Action, FlightDescriptor, FlightEndpoint, FlightInfo, HandshakeRequest, HandshakeResponse,
    IpcMessage, SchemaAsIpc, Ticket,
};
use arrow_schema::{ArrowError, DataType, Field, Schema};
use base64::Engine;
use dashmap::DashMap;
use datafusion::common::cast::as_string_array;
use datafusion::common::DFSchema;
use datafusion::error::DataFusionError;
use datafusion::execution::context::SessionState;
use datafusion::execution::runtime_env::RuntimeEnv;
use datafusion::logical_expr::{LogicalPlan, Values, Volatility};
use datafusion::physical_plan::ColumnarValue;
use datafusion::prelude::{DataFrame, Expr, SessionConfig, SessionContext};
use datafusion::scalar::ScalarValue;
use datafusion_iceberg::catalog::catalog_list::IcebergCatalogList;
use futures::{stream, Stream, StreamExt, TryStreamExt};
use iceberg_rust::catalog::CatalogList;
use log::{debug, info};
use mimalloc::MiMalloc;
use prost::Message;
use std::env;
use std::pin::Pin;
use std::sync::Arc;
use tonic::metadata::MetadataValue;
use tonic::{Request, Response, Status, Streaming};
use udf::create_no_arg_udf;
use uuid::Uuid;

mod udf;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

macro_rules! status {
    ($desc:expr, $err:expr) => {
        Status::internal(format!("{}: {} at {}:{}", $desc, $err, file!(), line!()))
    };
}

pub struct FlightSqlServiceImpl {
    pub contexts: Arc<DashMap<String, Arc<SessionContext>>>,
    pub statements: Arc<DashMap<String, LogicalPlan>>,
    pub results: Arc<DashMap<String, Vec<RecordBatch>>>,
    pub catalog_list: Arc<dyn CatalogList>,
}

impl FlightSqlServiceImpl {
    async fn create_ctx(&self) -> Result<String, Status> {
        let uuid = Uuid::new_v4().hyphenated().to_string();
        let session_config = SessionConfig::from_env()
            .map_err(|e| Status::internal(format!("Error building plan: {e}")))?
            .with_create_default_catalog_and_schema(true)
            .with_information_schema(true);
        let runtime_env = Arc::new(RuntimeEnv::default());
        let catalog_list = Arc::new(
            IcebergCatalogList::new(self.catalog_list.clone())
                .await
                .map_err(|e| Status::internal(format!("Error creating catalogs: {e}")))?,
        );
        let state = SessionState::new_with_config_rt_and_catalog_list(
            session_config,
            runtime_env,
            catalog_list,
        );
        let ctx = SessionContext::new_with_state(state);

        ctx.register_udf(create_no_arg_udf(
            "current_schema",
            vec![],
            Arc::new(DataType::Utf8),
            Volatility::Immutable,
            Arc::new(|_| {
                let current_schema = env::var("CURRENT_SCHEMA").ok();

                scalar_utf8(current_schema.as_deref().unwrap_or("public"))
            }),
        ));

        ctx.register_udf(create_no_arg_udf(
            "current_database",
            vec![],
            Arc::new(DataType::Utf8),
            Volatility::Immutable,
            Arc::new(|_| {
                let current_database = env::var("CURRENT_DATABASE").ok();

                scalar_utf8(current_database.as_deref().unwrap_or("datafusion"))
            }),
        ));

        self.contexts.insert(uuid.clone(), Arc::new(ctx));
        Ok(uuid)
    }

    fn get_ctx<T>(&self, req: &Request<T>) -> Result<Arc<SessionContext>, Status> {
        // get the token from the authorization header on Request
        let auth = req
            .metadata()
            .get("authorization")
            .ok_or_else(|| Status::internal("No authorization header!"))?;
        let str = auth
            .to_str()
            .map_err(|e| Status::internal(format!("Error parsing header: {e}")))?;
        let authorization = str.to_string();
        let bearer = "Bearer ";
        if !authorization.starts_with(bearer) {
            Err(Status::internal("Invalid auth header!"))?;
        }
        let auth = authorization[bearer.len()..].to_string();

        if let Some(context) = self.contexts.get(&auth) {
            Ok(context.clone())
        } else {
            Err(Status::internal(format!(
                "Context handle not found: {auth}"
            )))?
        }
    }

    fn get_plan(&self, handle: &str) -> Result<LogicalPlan, Status> {
        if let Some(plan) = self.statements.get(handle) {
            Ok(plan.clone())
        } else {
            Err(Status::internal(format!("Plan handle not found: {handle}")))?
        }
    }

    fn get_result(&self, handle: &str) -> Result<Vec<RecordBatch>, Status> {
        if let Some(result) = self.results.get(handle) {
            Ok(result.clone())
        } else {
            Err(Status::internal(format!(
                "Request handle not found: {handle}"
            )))?
        }
    }

    fn remove_plan(&self, handle: &str) -> Result<(), Status> {
        self.statements.remove(&handle.to_string());
        Ok(())
    }

    fn remove_result(&self, handle: &str) -> Result<(), Status> {
        self.results.remove(&handle.to_string());
        Ok(())
    }
}

fn scalar_utf8(schema: &str) -> Result<ColumnarValue, DataFusionError> {
    Ok(ColumnarValue::Scalar(ScalarValue::Utf8(Some(
        schema.to_string(),
    ))))
}

#[tonic::async_trait]
impl FlightSqlService for FlightSqlServiceImpl {
    type FlightService = FlightSqlServiceImpl;

    async fn do_handshake(
        &self,
        request: Request<Streaming<HandshakeRequest>>,
    ) -> Result<
        Response<Pin<Box<dyn Stream<Item = Result<HandshakeResponse, Status>> + Send>>>,
        Status,
    > {
        debug!("do_handshake");
        for md in request.metadata().iter() {
            debug!("{:?}", md);
        }

        let basic = "Basic ";
        let authorization = request
            .metadata()
            .get("authorization")
            .ok_or_else(|| Status::invalid_argument("authorization field not present"))?
            .to_str()
            .map_err(|_| Status::invalid_argument("authorization not parsable"))?;
        if !authorization.starts_with(basic) {
            Err(Status::invalid_argument(format!(
                "Auth type not implemented: {authorization}"
            )))?;
        }

        let username = env::var("FLIGHT_USER")
            .map_err(|_| Status::invalid_argument("Environment variable USERNAME not set."))?;
        let password = env::var("FLIGHT_PASSWORD")
            .map_err(|_| Status::invalid_argument("Environment variable PASSWORD not set."))?;
        let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(&authorization[basic.len()..])
            .map_err(|err| {
                Status::invalid_argument(format!("Failed to convert base64 to bytes: {err}"))
            })?;
        let str = String::from_utf8(bytes)
            .map_err(|_| Status::invalid_argument("Failed to convert auth bytes to utf8."))?;
        let parts: Vec<_> = str.split(':').collect();
        if parts.len() != 2 {
            Err(Status::invalid_argument("Invalid authorization header"))?;
        }
        let user = parts[0];
        let pass = parts[1];
        if user != username || pass != password {
            Err(Status::unauthenticated("Invalid credentials!"))?
        }

        let token = self.create_ctx().await?;

        let result = HandshakeResponse {
            protocol_version: 0,
            payload: token.as_bytes().to_vec().into(),
        };
        let result = Ok(result);
        let output = futures::stream::iter(vec![result]);
        let str = format!("Bearer {token}");
        let mut resp: Response<Pin<Box<dyn Stream<Item = Result<_, _>> + Send>>> =
            Response::new(Box::pin(output));
        let md = MetadataValue::try_from(str)
            .map_err(|_| Status::invalid_argument("authorization not parsable"))?;
        resp.metadata_mut().insert("authorization", md);
        Ok(resp)
    }

    async fn do_get_fallback(
        &self,
        _request: Request<Ticket>,
        message: Any,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        if !message.is::<FetchResults>() {
            Err(Status::unimplemented(format!(
                "do_get: The defined request is invalid: {}",
                message.type_url
            )))?
        }

        let fr: FetchResults = message
            .unpack()
            .map_err(|e| Status::internal(format!("{e:?}")))?
            .ok_or_else(|| Status::internal("Expected FetchResults but got None!"))?;

        let handle = fr.handle;

        info!("getting results for {handle}");
        let result = self.get_result(&handle)?;
        // if we get an empty result, create an empty schema
        let (schema, batches) = match result.first() {
            None => (Arc::new(Schema::empty()), vec![]),
            Some(batch) => (batch.schema(), result.clone()),
        };

        let batch_stream = futures::stream::iter(batches).map(Ok);

        let stream = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .build(batch_stream)
            .map_err(Status::from);

        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_flight_info_statement(
        &self,
        query: CommandStatementQuery,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        info!("get_flight_info_statement query:\n{}", query.query);

        Err(Status::unimplemented("Implement get_flight_info_statement"))
    }

    async fn get_flight_info_substrait_plan(
        &self,
        _query: CommandStatementSubstraitPlan,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        info!("get_flight_info_substrait_plan");
        Err(Status::unimplemented(
            "Implement get_flight_info_substrait_plan",
        ))
    }

    async fn get_flight_info_prepared_statement(
        &self,
        cmd: CommandPreparedStatementQuery,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        info!("get_flight_info_prepared_statement");
        let handle = std::str::from_utf8(&cmd.prepared_statement_handle)
            .map_err(|e| status!("Unable to parse uuid", e))?;

        let ctx = self.get_ctx(&request)?;
        let plan = self.get_plan(handle)?;

        let state = ctx.state();
        let df = DataFrame::new(state, plan);
        let result = df
            .collect()
            .await
            .map_err(|e| status!("Error executing query", e))?;

        // if we get an empty result, create an empty schema
        let schema = match result.first() {
            None => Schema::empty(),
            Some(batch) => (*batch.schema()).clone(),
        };

        self.results.insert(handle.to_string(), result);

        // if we had multiple endpoints to connect to, we could use this Location
        // but in the case of standalone DataFusion, we don't
        // let loc = Location {
        //     uri: "grpc+tcp://127.0.0.1:50051".to_string(),
        // };
        let fetch = FetchResults {
            handle: handle.to_string(),
        };
        let buf = fetch.as_any().encode_to_vec().into();
        let ticket = Ticket { ticket: buf };
        let endpoint = FlightEndpoint {
            ticket: Some(ticket),
            location: vec![],
            expiration_time: None,
            app_metadata: Vec::new().into(),
        };

        let flight_desc = FlightDescriptor {
            r#type: DescriptorType::Cmd.into(),
            cmd: Default::default(),
            path: vec![],
        };
        // send -1 for total_records and total_bytes instead of iterating over all the
        // batches to get num_rows() and total byte size.

        let info = FlightInfo::new()
            .try_with_schema(&schema)
            .map_err(|e| status!("Unable to serialize schema", e))?
            .with_descriptor(flight_desc)
            .with_endpoint(endpoint)
            .with_ordered(false);

        let resp = Response::new(info);
        Ok(resp)
    }

    async fn get_flight_info_catalogs(
        &self,
        _query: CommandGetCatalogs,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        info!("get_flight_info_catalogs");
        Err(Status::unimplemented("Implement get_flight_info_catalogs"))
    }

    async fn get_flight_info_schemas(
        &self,
        _query: CommandGetDbSchemas,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        info!("get_flight_info_schemas");
        Err(Status::unimplemented("Implement get_flight_info_schemas"))
    }

    async fn get_flight_info_tables(
        &self,
        _query: CommandGetTables,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        info!("get_flight_info_tables");
        Err(Status::unimplemented("Implement get_flight_info_tables"))
    }

    async fn get_flight_info_table_types(
        &self,
        _query: CommandGetTableTypes,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        info!("get_flight_info_table_types");
        Err(Status::unimplemented(
            "Implement get_flight_info_table_types",
        ))
    }

    async fn get_flight_info_sql_info(
        &self,
        _query: CommandGetSqlInfo,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        info!("get_flight_info_sql_info");
        Err(Status::unimplemented("Implement CommandGetSqlInfo"))
    }

    async fn get_flight_info_primary_keys(
        &self,
        _query: CommandGetPrimaryKeys,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        info!("get_flight_info_primary_keys");
        Err(Status::unimplemented(
            "Implement get_flight_info_primary_keys",
        ))
    }

    async fn get_flight_info_exported_keys(
        &self,
        _query: CommandGetExportedKeys,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        info!("get_flight_info_exported_keys");
        Err(Status::unimplemented(
            "Implement get_flight_info_exported_keys",
        ))
    }

    async fn get_flight_info_imported_keys(
        &self,
        _query: CommandGetImportedKeys,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        info!("get_flight_info_imported_keys");
        Err(Status::unimplemented(
            "Implement get_flight_info_imported_keys",
        ))
    }

    async fn get_flight_info_cross_reference(
        &self,
        _query: CommandGetCrossReference,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        info!("get_flight_info_cross_reference");
        Err(Status::unimplemented(
            "Implement get_flight_info_cross_reference",
        ))
    }

    async fn get_flight_info_xdbc_type_info(
        &self,
        _query: CommandGetXdbcTypeInfo,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        info!("get_flight_info_xdbc_type_info");
        Err(Status::unimplemented(
            "Implement get_flight_info_xdbc_type_info",
        ))
    }

    async fn do_get_statement(
        &self,
        _ticket: TicketStatementQuery,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        info!("do_get_statement");
        Err(Status::unimplemented("Implement do_get_statement"))
    }

    async fn do_get_prepared_statement(
        &self,
        _query: CommandPreparedStatementQuery,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        info!("do_get_prepared_statement");
        Err(Status::unimplemented("Implement do_get_prepared_statement"))
    }

    async fn do_get_catalogs(
        &self,
        _query: CommandGetCatalogs,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        info!("do_get_catalogs");
        Err(Status::unimplemented("Implement do_get_catalogs"))
    }

    async fn do_get_schemas(
        &self,
        _query: CommandGetDbSchemas,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        info!("do_get_schemas");
        Err(Status::unimplemented("Implement do_get_schemas"))
    }

    async fn do_get_tables(
        &self,
        _query: CommandGetTables,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        info!("do_get_tables");
        Err(Status::unimplemented("Implement do_get_tables"))
    }

    async fn do_get_table_types(
        &self,
        _query: CommandGetTableTypes,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        info!("do_get_table_types");
        Err(Status::unimplemented("Implement do_get_table_types"))
    }

    async fn do_get_sql_info(
        &self,
        _query: CommandGetSqlInfo,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        info!("do_get_sql_info");
        Err(Status::unimplemented("Implement do_get_sql_info"))
    }

    async fn do_get_primary_keys(
        &self,
        _query: CommandGetPrimaryKeys,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        info!("do_get_primary_keys");
        Err(Status::unimplemented("Implement do_get_primary_keys"))
    }

    async fn do_get_exported_keys(
        &self,
        _query: CommandGetExportedKeys,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        info!("do_get_exported_keys");
        Err(Status::unimplemented("Implement do_get_exported_keys"))
    }

    async fn do_get_imported_keys(
        &self,
        _query: CommandGetImportedKeys,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        info!("do_get_imported_keys");
        Err(Status::unimplemented("Implement do_get_imported_keys"))
    }

    async fn do_get_cross_reference(
        &self,
        _query: CommandGetCrossReference,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        info!("do_get_cross_reference");
        Err(Status::unimplemented("Implement do_get_cross_reference"))
    }

    async fn do_get_xdbc_type_info(
        &self,
        _query: CommandGetXdbcTypeInfo,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        info!("do_get_xdbc_type_info");
        Err(Status::unimplemented("Implement do_get_xdbc_type_info"))
    }

    async fn do_put_statement_update(
        &self,
        _ticket: CommandStatementUpdate,
        _request: Request<PeekableFlightDataStream>,
    ) -> Result<i64, Status> {
        info!("do_put_statement_update");
        Err(Status::unimplemented("Implement do_put_statement_update"))
    }

    async fn do_put_prepared_statement_query(
        &self,
        query: CommandPreparedStatementQuery,
        request: Request<PeekableFlightDataStream>,
    ) -> Result<Response<<Self as FlightService>::DoPutStream>, Status> {
        info!("do_put_prepared_statement_query");
        let parameters = FlightRecordBatchStream::new_from_flight_data(
            request.into_inner().map_err(FlightError::from),
        )
        .try_collect::<Vec<_>>()
        .await?;

        let parameters: Vec<ScalarValue> = parameters
            .into_iter()
            .map(|record_batch| {
                Ok(ScalarValue::from(
                    as_string_array(record_batch.column(0))?.value(0),
                ))
            })
            .collect::<Result<_, DataFusionError>>()
            .map_err(ArrowError::from)
            .map_err(FlightError::from)?;

        let handle = std::str::from_utf8(&query.prepared_statement_handle)
            .map_err(|e| status!("Unable to parse uuid", e))?;

        let prepared_plan = self.get_plan(&handle)?;

        let plan = prepared_plan
            .with_param_values(parameters)
            .map_err(ArrowError::from)
            .map_err(FlightError::from)?;

        *self
            .statements
            .get_mut(handle)
            .ok_or(status!("Handle {} not found", &handle))? = plan;

        Ok(Response::new(Box::pin(stream::empty())))
    }

    async fn do_put_prepared_statement_update(
        &self,
        _handle: CommandPreparedStatementUpdate,
        _request: Request<PeekableFlightDataStream>,
    ) -> Result<i64, Status> {
        info!("do_put_prepared_statement_update");
        // statements like "CREATE TABLE.." or "SET datafusion.nnn.." call this function
        // and we are required to return some row count here
        Ok(-1)
    }

    async fn do_put_substrait_plan(
        &self,
        _query: CommandStatementSubstraitPlan,
        _request: Request<PeekableFlightDataStream>,
    ) -> Result<i64, Status> {
        info!("do_put_prepared_statement_update");
        Err(Status::unimplemented(
            "Implement do_put_prepared_statement_update",
        ))
    }

    async fn do_action_create_prepared_statement(
        &self,
        query: ActionCreatePreparedStatementRequest,
        request: Request<Action>,
    ) -> Result<ActionCreatePreparedStatementResult, Status> {
        let user_query = query.query.as_str();
        info!("do_action_create_prepared_statement: {user_query}");

        let ctx = self.get_ctx(&request)?;

        let plan = if user_query != "rollback" {
            ctx.sql(user_query)
                .await
                .and_then(|df| df.into_optimized_plan())
                .map_err(|e| Status::internal(format!("Error building plan: {e}")))?
        } else {
            LogicalPlan::Values(Values {
                schema: Arc::new(
                    DFSchema::try_from(Schema::new(vec![Field::new(
                        "rollback",
                        DataType::Utf8,
                        false,
                    )]))
                    .map_err(|e| Status::internal(format!("Error building plan: {e}")))?,
                ),
                values: vec![vec![Expr::Literal(ScalarValue::Utf8(Some(
                    "ROLLBACK".to_owned(),
                )))]],
            })
        };

        // store a copy of the plan,  it will be used for execution
        let plan_uuid = Uuid::new_v4().hyphenated().to_string();
        self.statements.insert(plan_uuid.clone(), plan.clone());

        let plan_schema = plan.schema();

        let arrow_schema = (&**plan_schema).into();
        let message = SchemaAsIpc::new(&arrow_schema, &IpcWriteOptions::default())
            .try_into()
            .map_err(|e| status!("Unable to serialize schema", e))?;
        let IpcMessage(schema_bytes) = message;

        let res = ActionCreatePreparedStatementResult {
            prepared_statement_handle: plan_uuid.into(),
            dataset_schema: schema_bytes,
            parameter_schema: Default::default(),
        };
        Ok(res)
    }

    async fn do_action_close_prepared_statement(
        &self,
        handle: ActionClosePreparedStatementRequest,
        _request: Request<Action>,
    ) -> Result<(), Status> {
        let handle = std::str::from_utf8(&handle.prepared_statement_handle);
        if let Ok(handle) = handle {
            info!("do_action_close_prepared_statement: removing plan and results for {handle}");
            let _ = self.remove_plan(handle);
            let _ = self.remove_result(handle);
        }
        Ok(())
    }

    async fn do_action_create_prepared_substrait_plan(
        &self,
        _query: ActionCreatePreparedSubstraitPlanRequest,
        _request: Request<Action>,
    ) -> Result<ActionCreatePreparedStatementResult, Status> {
        info!("do_action_create_prepared_substrait_plan");
        Err(Status::unimplemented(
            "Implement do_action_create_prepared_substrait_plan",
        ))
    }

    async fn do_action_begin_transaction(
        &self,
        _query: ActionBeginTransactionRequest,
        _request: Request<Action>,
    ) -> Result<ActionBeginTransactionResult, Status> {
        info!("do_action_begin_transaction");
        Err(Status::unimplemented(
            "Implement do_action_begin_transaction",
        ))
    }

    async fn do_action_end_transaction(
        &self,
        _query: ActionEndTransactionRequest,
        _request: Request<Action>,
    ) -> Result<(), Status> {
        info!("do_action_end_transaction");
        Err(Status::unimplemented("Implement do_action_end_transaction"))
    }

    async fn do_action_begin_savepoint(
        &self,
        _query: ActionBeginSavepointRequest,
        _request: Request<Action>,
    ) -> Result<ActionBeginSavepointResult, Status> {
        info!("do_action_begin_savepoint");
        Err(Status::unimplemented("Implement do_action_begin_savepoint"))
    }

    async fn do_action_end_savepoint(
        &self,
        _query: ActionEndSavepointRequest,
        _request: Request<Action>,
    ) -> Result<(), Status> {
        info!("do_action_end_savepoint");
        Err(Status::unimplemented("Implement do_action_end_savepoint"))
    }

    async fn do_action_cancel_query(
        &self,
        _query: ActionCancelQueryRequest,
        _request: Request<Action>,
    ) -> Result<ActionCancelQueryResult, Status> {
        info!("do_action_cancel_query");
        Err(Status::unimplemented("Implement do_action_cancel_query"))
    }

    async fn register_sql_info(&self, _id: i32, _result: &SqlInfo) {}
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FetchResults {
    #[prost(string, tag = "1")]
    pub handle: ::prost::alloc::string::String,
}

impl ProstMessageExt for FetchResults {
    fn type_url() -> &'static str {
        "type.googleapis.com/datafusion.example.com.sql.FetchResults"
    }

    fn as_any(&self) -> Any {
        Any {
            type_url: FetchResults::type_url().to_string(),
            value: ::prost::Message::encode_to_vec(self).into(),
        }
    }
}
