use agl_observe::{
    InferenceArtifactRoot, InferenceEventWriter, InferenceFinishStatus, InferenceObservationEvent,
};
use anyhow::{bail, Result};

use crate::{InferenceBackend, InferenceFinishReason, InferenceRequest, InferenceResponse};

#[derive(Clone, Debug)]
pub struct FakeInferenceBackend {
    backend_name: String,
    response: InferenceResponse,
    failure_message: Option<String>,
    artifact_root: Option<InferenceArtifactRoot>,
    recorded_requests: Vec<InferenceRequest>,
}

impl FakeInferenceBackend {
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            backend_name: "fake".to_string(),
            response: InferenceResponse {
                content: content.into(),
                finish_reason: InferenceFinishReason::Stop,
            },
            failure_message: None,
            artifact_root: None,
            recorded_requests: Vec::new(),
        }
    }

    pub fn with_backend_name(mut self, backend_name: impl Into<String>) -> Self {
        self.backend_name = backend_name.into();
        self
    }

    pub fn with_finish_reason(mut self, finish_reason: InferenceFinishReason) -> Self {
        self.response.finish_reason = finish_reason;
        self
    }

    pub fn failing(mut self, message: impl Into<String>) -> Self {
        self.failure_message = Some(message.into());
        self
    }

    pub fn with_artifact_root(mut self, artifact_root: InferenceArtifactRoot) -> Self {
        self.artifact_root = Some(artifact_root);
        self
    }

    pub fn recorded_requests(&self) -> &[InferenceRequest] {
        &self.recorded_requests
    }

    fn record_success(
        &self,
        request: &InferenceRequest,
        response: &InferenceResponse,
    ) -> Result<()> {
        let Some(artifact_root) = &self.artifact_root else {
            return Ok(());
        };

        let paths = artifact_root.paths(&request.run_id, &request.attempt_id);
        let writer = InferenceEventWriter::new(paths.events_jsonl());
        writer.append(&InferenceObservationEvent::AttemptStarted {
            run_id: request.run_id.clone(),
            attempt_id: request.attempt_id.clone(),
            backend: self.backend_name.clone(),
            request_path: paths.request_json().to_path_buf(),
        })?;
        paths.write_request_json(request)?;
        writer.append(&InferenceObservationEvent::RequestRecorded {
            run_id: request.run_id.clone(),
            attempt_id: request.attempt_id.clone(),
            path: paths.request_json().to_path_buf(),
        })?;
        paths.write_response_json(response)?;
        writer.append(&InferenceObservationEvent::ResponseRecorded {
            run_id: request.run_id.clone(),
            attempt_id: request.attempt_id.clone(),
            path: paths.response_json().to_path_buf(),
        })?;
        writer.append(&InferenceObservationEvent::AttemptFinished {
            run_id: request.run_id.clone(),
            attempt_id: request.attempt_id.clone(),
            finish_status: InferenceFinishStatus::Succeeded,
        })
    }

    fn record_failure(&self, request: &InferenceRequest, message: &str) -> Result<()> {
        let Some(artifact_root) = &self.artifact_root else {
            return Ok(());
        };

        let paths = artifact_root.paths(&request.run_id, &request.attempt_id);
        let writer = InferenceEventWriter::new(paths.events_jsonl());
        writer.append(&InferenceObservationEvent::AttemptStarted {
            run_id: request.run_id.clone(),
            attempt_id: request.attempt_id.clone(),
            backend: self.backend_name.clone(),
            request_path: paths.request_json().to_path_buf(),
        })?;
        paths.write_request_json(request)?;
        writer.append(&InferenceObservationEvent::RequestRecorded {
            run_id: request.run_id.clone(),
            attempt_id: request.attempt_id.clone(),
            path: paths.request_json().to_path_buf(),
        })?;
        writer.append(&InferenceObservationEvent::AttemptFailed {
            run_id: request.run_id.clone(),
            attempt_id: request.attempt_id.clone(),
            message: message.to_string(),
        })?;
        writer.append(&InferenceObservationEvent::AttemptFinished {
            run_id: request.run_id.clone(),
            attempt_id: request.attempt_id.clone(),
            finish_status: InferenceFinishStatus::Failed,
        })
    }
}

impl InferenceBackend for FakeInferenceBackend {
    fn generate(&mut self, request: InferenceRequest) -> Result<InferenceResponse> {
        self.recorded_requests.push(request.clone());

        if let Some(message) = &self.failure_message {
            self.record_failure(&request, message)?;
            bail!("{message}");
        }

        let response = self.response.clone();
        self.record_success(&request, &response)?;
        Ok(response)
    }
}
