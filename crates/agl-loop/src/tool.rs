use crate::VisibleTool;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct ToolDispatchRequest {
    pub turn_id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDispatchResponse {
    pub observation: String,
}

pub(crate) fn validate_tool_arguments(
    tool: &VisibleTool,
    arguments: &Value,
) -> std::result::Result<(), String> {
    let Some(object) = arguments.as_object() else {
        return Err("tool arguments must be an object".to_string());
    };

    for required in &tool.required_arguments {
        if !object.contains_key(required) {
            return Err(format!("missing required argument `{required}`"));
        }
    }

    Ok(())
}
