use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::HookId;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    ContextPrepare,
    ModelRequest,
    ModelResponse,
    ArtifactWrite,
    TurnFinish,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContextPrepare => "context.prepare",
            Self::ModelRequest => "model.request",
            Self::ModelResponse => "model.response",
            Self::ArtifactWrite => "artifact.write",
            Self::TurnFinish => "turn.finish",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookStatus {
    Pass,
    Warn,
    Fail,
    Repair,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookMessage {
    pub code: String,
    pub message: String,
    pub fix: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookInput {
    pub hook_id: HookId,
    pub event: HookEvent,
    pub payload: Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookResult {
    pub hook_id: HookId,
    pub status: HookStatus,
    pub messages: Vec<HookMessage>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookBatchRequest {
    pub event: HookEvent,
    pub hooks: Vec<HookId>,
    pub payload: Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookBatchResult {
    pub event: HookEvent,
    pub results: Vec<HookResult>,
}

impl HookBatchResult {
    pub fn status(&self) -> HookStatus {
        if self
            .results
            .iter()
            .any(|result| result.status == HookStatus::Fail)
        {
            HookStatus::Fail
        } else if self
            .results
            .iter()
            .any(|result| result.status == HookStatus::Repair)
        {
            HookStatus::Repair
        } else if self
            .results
            .iter()
            .any(|result| result.status == HookStatus::Warn)
        {
            HookStatus::Warn
        } else {
            HookStatus::Pass
        }
    }
}
