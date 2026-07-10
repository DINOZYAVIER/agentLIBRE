use agl_capabilities::{ActionResult, CapabilityId};
use agl_ids::{RunId, TurnId};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct ToolDispatchRequest {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub capability_id: CapabilityId,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDispatchResponse {
    pub result: ActionResult,
}
