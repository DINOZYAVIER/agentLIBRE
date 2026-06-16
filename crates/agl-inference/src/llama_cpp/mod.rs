use agl_config::LocalInferenceConfig;
use anyhow::{Result, bail};

mod ffi;
mod runtime;
mod session;

use crate::evidence::{
    InferenceArtifactRoot, InferenceEventWriter, InferenceFinishStatus, InferenceObservationEvent,
};
use crate::{InferenceBackend, InferenceRequest, InferenceResponse};
use runtime::{LlamaCppRuntime, LlamaCppRuntimeError};

const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 256;

pub struct LlamaCppBackend {
    artifact_root: InferenceArtifactRoot,
    runtime: LlamaCppRuntime,
}

impl LlamaCppBackend {
    pub fn new(config: LocalInferenceConfig, artifact_root: InferenceArtifactRoot) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            artifact_root,
            runtime: LlamaCppRuntime::new(config, DEFAULT_MAX_OUTPUT_TOKENS),
        })
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.runtime.set_max_output_tokens(max_output_tokens);
        self
    }

    pub fn config(&self) -> &LocalInferenceConfig {
        self.runtime.config()
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

        let output = match self.runtime.generate(&request.rendered) {
            Ok(output) => output,
            Err(err) => {
                let err_text = err.to_string();
                let message = format!("llama.cpp runtime failed: {err_text}");
                let mut runtime_log = err
                    .downcast_ref::<LlamaCppRuntimeError>()
                    .map(|err| err.log().to_string())
                    .unwrap_or_default();
                if !runtime_log.is_empty() && !runtime_log.ends_with('\n') {
                    runtime_log.push('\n');
                }
                runtime_log.push_str("error = ");
                runtime_log.push_str(&err_text);
                runtime_log.push('\n');
                paths.write_runtime_log(runtime_log)?;
                self.append_failure(&writer, &request, &message)?;
                tracing::error!(
                    target: "agentlibre::inference",
                    run_id = %request.run_id,
                    attempt_id = %request.attempt_id,
                    backend = "llama_cpp",
                    error = %err_text,
                    "llama.cpp inference attempt failed"
                );
                bail!("{message}");
            }
        };
        paths.write_runtime_log(output.log)?;

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
        tracing::info!(
            target: "agentlibre::inference",
            run_id = %request.run_id,
            attempt_id = %request.attempt_id,
            backend = "llama_cpp",
            finish_reason = ?response.finish_reason,
            content_bytes = response.content.len(),
            "llama.cpp inference attempt succeeded"
        );
        Ok(response)
    }
}

#[cfg(test)]
impl LlamaCppBackend {
    pub(crate) fn new_with_test_runtime(
        config: LocalInferenceConfig,
        artifact_root: InferenceArtifactRoot,
        responses: Vec<&str>,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            artifact_root,
            runtime: LlamaCppRuntime::new_test(config, DEFAULT_MAX_OUTPUT_TOKENS, responses),
        })
    }
}
