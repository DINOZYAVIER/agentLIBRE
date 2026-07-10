use std::fmt::{self, Display, Formatter};

use jsonschema::Validator;
use schemars::{JsonSchema, generate::SchemaSettings};
use serde_json::Value;

#[derive(Debug)]
pub struct ActionSchema {
    schema: Value,
    validator: Validator,
}

impl ActionSchema {
    pub fn compile(schema: &Value) -> Result<Self, SchemaValidationError> {
        jsonschema::draft202012::meta::validate(schema)
            .map_err(|error| SchemaValidationError::InvalidSchema(error.to_string()))?;
        let validator = jsonschema::draft202012::new(schema)
            .map_err(|error| SchemaValidationError::InvalidSchema(error.to_string()))?;
        Ok(Self {
            schema: schema.clone(),
            validator,
        })
    }

    pub fn schema(&self) -> &Value {
        &self.schema
    }

    pub fn validate(&self, arguments: &Value) -> Result<(), ArgumentValidationError> {
        let errors = self
            .validator
            .iter_errors(arguments)
            .map(|error| ArgumentViolation {
                instance_path: error.instance_path().as_str().to_owned(),
                schema_path: error.schema_path().as_str().to_owned(),
                message: error.to_string(),
            })
            .collect::<Vec<_>>();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ArgumentValidationError { violations: errors })
        }
    }
}

pub fn draft202012_schema_for<T: JsonSchema>() -> Value {
    let schema = SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<T>();
    let mut value = Value::from(schema);
    close_implicit_objects(&mut value);
    value
}

fn close_implicit_objects(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                close_implicit_objects(value);
            }
        }
        Value::Object(object) => {
            for value in object.values_mut() {
                close_implicit_objects(value);
            }
            let is_object = object.get("type").and_then(Value::as_str) == Some("object")
                || object.contains_key("properties");
            if is_object
                && !object.contains_key("additionalProperties")
                && !object.contains_key("unevaluatedProperties")
            {
                object.insert("additionalProperties".to_owned(), Value::Bool(false));
            }
        }
        _ => {}
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchemaValidationError {
    InvalidSchema(String),
}

impl Display for SchemaValidationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSchema(message) => {
                write!(formatter, "invalid Draft 2020-12 schema: {message}")
            }
        }
    }
}

impl std::error::Error for SchemaValidationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArgumentViolation {
    pub instance_path: String,
    pub schema_path: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArgumentValidationError {
    violations: Vec<ArgumentViolation>,
}

impl ArgumentValidationError {
    pub fn violations(&self) -> &[ArgumentViolation] {
        &self.violations
    }
}

impl Display for ArgumentValidationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "action arguments failed schema validation")?;
        for violation in &self.violations {
            write!(
                formatter,
                "; {}: {}",
                if violation.instance_path.is_empty() {
                    "/"
                } else {
                    &violation.instance_path
                },
                violation.message
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for ArgumentValidationError {}
