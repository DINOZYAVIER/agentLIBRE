use agl_ids::{RunId, TurnId};
use serde_json::Value;

use crate::VisibleTool;

#[derive(Clone, Debug, PartialEq)]
pub enum TurnMessage {
    System { content: String },
    User { content: String },
    Assistant { content: String },
    AssistantToolCall { name: String, arguments: Value },
    ToolObservation { name: String, content: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelRequest {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub request_index: usize,
    pub messages: Vec<TurnMessage>,
    pub visible_tools: Vec<VisibleTool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelResponse {
    pub content: String,
}
