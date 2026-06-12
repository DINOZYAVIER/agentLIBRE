use agl_model::RenderedModelRequest;
use agl_observe::{InferenceAttemptId, InferenceRunId};
use anyhow::Result;
use serde::{Deserialize, Serialize};

pub trait InferenceBackend {
    fn generate(&mut self, request: InferenceRequest) -> Result<InferenceResponse>;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InferenceRequest {
    pub run_id: InferenceRunId,
    pub attempt_id: InferenceAttemptId,
    pub rendered: RenderedModelRequest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceResponse {
    pub content: String,
    pub finish_reason: InferenceFinishReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceFinishReason {
    Stop,
    Length,
}
