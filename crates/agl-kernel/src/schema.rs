use std::fmt::{self, Display, Formatter};

use jsonschema::{ValidationError, Validator, error::ValidationErrorKind};
use schemars::{JsonSchema, generate::SchemaSettings};
use serde_json::Value;

#[derive(Debug)]
pub struct ToolSchema {
    schema: Value,
    validator: Validator,
}

impl ToolSchema {
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
        let mut errors = Vec::new();
        for error in self.validator.iter_errors(arguments) {
            collect_actionable_violations(&error, &mut errors);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ArgumentValidationError { violations: errors })
        }
    }
}

fn collect_actionable_violations(
    error: &ValidationError<'_>,
    violations: &mut Vec<ArgumentViolation>,
) {
    let branch_context = match error.kind() {
        ValidationErrorKind::AnyOf { context } | ValidationErrorKind::OneOfNotValid { context } => {
            Some(context)
        }
        _ => None,
    };
    if let Some(context) = branch_context
        && let Some(closest) = context
            .iter()
            .filter(|branch| !branch.is_empty())
            .min_by_key(|branch| branch.len())
    {
        for nested in closest {
            collect_actionable_violations(nested, violations);
        }
        return;
    }
    violations.push(ArgumentViolation {
        instance_path: error.instance_path().as_str().to_owned(),
        schema_path: error.schema_path().as_str().to_owned(),
        message: error.to_string(),
    });
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn one_of_failure_reports_the_closest_branch_violations() {
        let schema = ToolSchema::compile(&json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "operation": {
                    "oneOf": [
                        {
                            "type": "object",
                            "properties": {
                                "op": {"const": "create"},
                                "path": {"type": "string"},
                                "expected_absent": {"type": "boolean"}
                            },
                            "required": ["op", "path", "expected_absent"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": {
                                "op": {"const": "delete"},
                                "path": {"type": "string"},
                                "expected_digest": {"type": "string"}
                            },
                            "required": ["op", "path", "expected_digest"],
                            "additionalProperties": false
                        }
                    ]
                }
            },
            "required": ["operation"],
            "additionalProperties": false
        }))
        .unwrap();

        let error = schema
            .validate(&json!({
                "operation": {
                    "action": "create",
                    "path": "test.file"
                }
            }))
            .unwrap_err();
        let message = error.to_string();

        assert!(message.contains("'action' was unexpected"));
        assert!(message.contains("\"expected_absent\" is a required property"));
        assert!(message.contains("\"op\" is a required property"));
        assert!(!message.contains("not valid under any of the schemas"));
    }
}
