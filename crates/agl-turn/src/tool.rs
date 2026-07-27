use agl_extension::ToolId;
use agl_ids::{RunId, TurnId};
use agl_kernel::ToolOutcome;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDispatchRequest {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub capability_id: ToolId,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDispatchResponse {
    pub result: ToolOutcome,
}
