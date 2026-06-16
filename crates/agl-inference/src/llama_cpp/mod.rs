use agl_config::LocalInferenceConfig;
use anyhow::{Result, bail};

mod ffi;
mod runtime;

use crate::evidence::{
    InferenceArtifactRoot, InferenceEventWriter, InferenceFinishStatus, InferenceObservationEvent,
};
use crate::{InferenceBackend, InferenceRequest, InferenceResponse};
use runtime::LlamaCppRuntime;

const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 256;

#[derive(Clone, Debug)]
pub struct LlamaCppBackend {
    config: LocalInferenceConfig,
    artifact_root: InferenceArtifactRoot,
    max_output_tokens: u32,
}

impl LlamaCppBackend {
    pub fn new(config: LocalInferenceConfig, artifact_root: InferenceArtifactRoot) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            artifact_root,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        })
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.max_output_tokens = max_output_tokens;
        self
    }

    pub fn config(&self) -> &LocalInferenceConfig {
        &self.config
    }

    fn append_started(
        &self,
        writer: &InferenceEventWriter,
        request: &InferenceRequest,
    ) -> Result<()> {
        let paths = self
            .artifact_root
            .paths(&request.run_id, &request.attempt_id);
        writer.append(&InferenceObservationEvent::AttemptStarted {
            run_id: request.run_id.clone(),
            attempt_id: request.attempt_id.clone(),
            backend: "llama_cpp".to_string(),
            request_path: paths.request_json().to_path_buf(),
        })
    }

    fn append_failure(
        &self,
        writer: &InferenceEventWriter,
        request: &InferenceRequest,
        message: &str,
    ) -> Result<()> {
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

impl InferenceBackend for LlamaCppBackend {
    fn generate(&mut self, request: InferenceRequest) -> Result<InferenceResponse> {
        let paths = self
            .artifact_root
            .paths(&request.run_id, &request.attempt_id);
        let writer = InferenceEventWriter::new(paths.events_jsonl());
        self.append_started(&writer, &request)?;
        paths.write_request_json(&request)?;
        writer.append(&InferenceObservationEvent::RequestRecorded {
            run_id: request.run_id.clone(),
            attempt_id: request.attempt_id.clone(),
            path: paths.request_json().to_path_buf(),
        })?;

        let mut runtime = LlamaCppRuntime::new(self.config.clone(), self.max_output_tokens);
        let output = match runtime.generate(&request.rendered) {
            Ok(output) => output,
            Err(err) => {
                let message = format!("llama.cpp runtime failed: {err}");
                paths.write_stderr_log(format!("{message}\n"))?;
                self.append_failure(&writer, &request, &message)?;
                bail!("{message}");
            }
        };
        paths.write_stderr_log(output.log)?;

        let response = InferenceResponse {
            content: output.content,
            finish_reason: output.finish_reason,
        };
        paths.write_response_json(&response)?;
        writer.append(&InferenceObservationEvent::ResponseRecorded {
            run_id: request.run_id.clone(),
            attempt_id: request.attempt_id.clone(),
            path: paths.response_json().to_path_buf(),
        })?;
        writer.append(&InferenceObservationEvent::AttemptFinished {
            run_id: request.run_id.clone(),
            attempt_id: request.attempt_id.clone(),
            finish_status: InferenceFinishStatus::Succeeded,
        })?;
        Ok(response)
    }
}
