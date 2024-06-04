use arrow_schema::DataType;
use datafusion::{
    error::DataFusionError,
    logical_expr::{
        ColumnarValue, ScalarFunctionImplementation, ScalarUDF, ScalarUDFImpl, Signature,
        Volatility,
    },
};
use std::{any::Any, fmt::Debug, sync::Arc};

/// Convenience method to create a new user defined scalar function (UDF) with a
/// specific signature and specific return type.
///
/// Note this function does not expose all available features of [`ScalarUDF`],
/// such as
///
/// * computing return types based on input types
/// * multiple [`Signature`]s
/// * aliases
///
/// See [`ScalarUDF`] for details and examples on how to use the full
/// functionality.
pub fn create_no_arg_udf(
    name: &str,
    input_types: Vec<DataType>,
    return_type: Arc<DataType>,
    volatility: Volatility,
    fun: ScalarFunctionImplementation,
) -> ScalarUDF {
    let return_type = Arc::try_unwrap(return_type).unwrap_or_else(|t| t.as_ref().clone());
    ScalarUDF::from(NoArgScalarUDF::new(
        name,
        input_types,
        return_type,
        volatility,
        fun,
    ))
}

/// Implements [`ScalarUDFImpl`] for functions that have a single signature and
/// return type.
pub struct NoArgScalarUDF {
    name: String,
    signature: Signature,
    return_type: DataType,
    fun: ScalarFunctionImplementation,
}

impl Debug for NoArgScalarUDF {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("ScalarUDF")
            .field("name", &self.name)
            .field("signature", &self.signature)
            .field("fun", &"<FUNC>")
            .finish()
    }
}

impl NoArgScalarUDF {
    /// Create a new `SimpleScalarUDF` from a name, input types, return type and
    /// implementation. Implementing [`ScalarUDFImpl`] allows more flexibility
    pub fn new(
        name: impl Into<String>,
        input_types: Vec<DataType>,
        return_type: DataType,
        volatility: Volatility,
        fun: ScalarFunctionImplementation,
    ) -> Self {
        let name = name.into();
        let signature = Signature::exact(input_types, volatility);
        Self {
            name,
            signature,
            return_type,
            fun,
        }
    }
}

impl ScalarUDFImpl for NoArgScalarUDF {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType, DataFusionError> {
        Ok(self.return_type.clone())
    }

    fn invoke(&self, args: &[ColumnarValue]) -> Result<ColumnarValue, DataFusionError> {
        (self.fun)(args)
    }

    fn invoke_no_args(&self, _num_rows: usize) -> Result<ColumnarValue, DataFusionError> {
        (self.fun)(&[])
    }
}
