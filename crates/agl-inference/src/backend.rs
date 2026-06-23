use agl_oven::RenderedModelRequest;
use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::evidence::{InferenceAttemptId, InferenceRunId};

pub trait InferenceBackend {
    fn backend_name(&self) -> &'static str;

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
