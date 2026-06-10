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
