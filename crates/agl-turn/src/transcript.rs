use serde_json::Value;

use crate::VisibleTool;

#[derive(Clone, Debug, PartialEq)]
pub enum TurnMessage {
    User { content: String },
    Assistant { content: String },
    AssistantToolCall { name: String, arguments: Value },
    ToolObservation { name: String, content: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelRequest {
    pub turn_id: String,
    pub request_index: usize,
    pub messages: Vec<TurnMessage>,
    pub visible_tools: Vec<VisibleTool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelResponse {
    pub content: String,
}
