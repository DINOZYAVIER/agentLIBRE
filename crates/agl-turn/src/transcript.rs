use agl_capabilities::ActionResult;
use agl_content::Content;
use agl_ids::{RunId, TurnId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::VisibleTool;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnMessage {
    System { content: Content },
    User { content: Content },
    Assistant { content: Content },
    AssistantToolCall { name: String, arguments: Value },
    ToolObservation { name: String, result: ActionResult },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequest {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub request_index: usize,
    pub messages: Vec<TurnMessage>,
    pub visible_tools: Vec<VisibleTool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelResponse {
    pub content: Content,
}
