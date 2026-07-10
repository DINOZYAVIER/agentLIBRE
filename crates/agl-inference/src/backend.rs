use agl_ids::{AttemptId, RequestId, RunId, SessionId, TurnId};
use agl_oven::RenderedModelRequest;
use anyhow::Result;
use serde::{Deserialize, Serialize};

pub trait InferenceBackend {
    fn backend_name(&self) -> &'static str;

    fn generate(&mut self, request: InferenceRequest) -> Result<InferenceResponse>;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceRequest {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub attempt_id: AttemptId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub rendered: RenderedModelRequest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceResponse {
    pub attempt_id: AttemptId,
    pub content: String,
    pub finish_reason: InferenceFinishReason,
    pub metadata: InferenceResponseMetadata,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceResponseMetadata {
    pub model_state: Option<String>,
    pub selected_device: Option<String>,
    pub duration_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceFinishReason {
    Stop,
    Length,
}
