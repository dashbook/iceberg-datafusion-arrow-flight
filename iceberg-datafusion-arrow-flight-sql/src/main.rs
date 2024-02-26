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

use arrow_flight::flight_service_server::FlightServiceServer;

use iceberg_catalog_sql::SqlCatalogList;
use iceberg_datafusion_arrow_flight::FlightSqlServiceImpl;
use log::info;
use object_store::{aws::AmazonS3Builder, memory::InMemory, ObjectStore};
use std::{env, sync::Arc};
use tonic::transport::{Identity, Server, ServerTlsConfig};

/// This example shows how to wrap DataFusion with `FlightSqlService` to support connecting
/// to a standalone DataFusion-based server with a JDBC client, using the open source "JDBC Driver
/// for Arrow Flight SQL".
///
/// To install the JDBC driver in DBeaver for example, see these instructions:
/// https://docs.dremio.com/software/client-applications/dbeaver/
/// When configuring the driver, specify property "UseEncryption" = false
///
/// JDBC connection string: "jdbc:arrow-flight-sql://127.0.0.1:50051/"
///
/// Based heavily on Ballista's implementation: https://github.com/apache/arrow-ballista/blob/main/ballista/scheduler/src/flight_sql.rs
/// and the example in arrow-rs: https://github.com/apache/arrow-rs/blob/master/arrow-flight/examples/flight_sql_server.rs
///
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let addr = "0.0.0.0:31337".parse()?;

    let catalog_url = env::var("CATALOG_URL")?;

    let bucket = env::var("BUCKET");
    let aws_access_key_id = env::var("AWS_ACCESS_KEY_ID");
    let aws_secret_access_key = env::var("AWS_SECRET_ACCESS_KEY");
    let aws_endpoint = env::var("AWS_ENDPOINT").ok();
    let aws_allow_http = env::var("AWS_ALLOW_HTTP").ok();

    let cert_domain = env::var("TLS_DOMAIN");

    let object_store = match (bucket, aws_access_key_id, aws_secret_access_key) {
        (Ok(bucket), Ok(aws_access_key_id), Ok(aws_secret_access_key)) => {
            let mut builder = AmazonS3Builder::from_env()
                .with_bucket_name(bucket)
                .with_access_key_id(aws_access_key_id)
                .with_secret_access_key(aws_secret_access_key);
            if let Some(aws_endpoint) = aws_endpoint {
                builder = builder.with_endpoint(aws_endpoint);
            }
            if let Some("TRUE") = aws_allow_http.as_deref() {
                builder = builder.with_allow_http(true);
            }

            Arc::new(builder.build()?) as Arc<dyn ObjectStore>
        }
        _ => Arc::new(InMemory::new()),
    };

    let catalog_list = Arc::new(SqlCatalogList::new(&catalog_url, object_store).await?);
    let service = FlightSqlServiceImpl {
        contexts: Default::default(),
        statements: Default::default(),
        results: Default::default(),
        catalog_list,
    };
    info!("Listening on {addr:?}");
    let svc = FlightServiceServer::new(service);

    if let Ok(cert_domain) = cert_domain {
        let tls = rcgen::generate_simple_self_signed(vec![cert_domain])?;

        let config = ServerTlsConfig::new().identity(Identity::from_pem(
            tls.serialize_pem()?,
            tls.serialize_private_key_pem(),
        ));
        Server::builder()
            .tls_config(config)?
            .add_service(svc)
            .serve(addr)
            .await?;
    } else {
        Server::builder().add_service(svc).serve(addr).await?;
    };

    Ok(())
}
