use agl_config::LocalInferenceConfig;
use anyhow::{Result, bail, ensure};
use std::time::Instant;

mod ffi;
mod runtime;
mod session;

use crate::evidence::{InferenceArtifactRoot, InferenceEventWriter, InferenceFinishStatus};
use crate::{
    InferenceAttemptMachine, InferenceAttemptTransition, InferenceAttemptTransitionRecord,
};
use crate::{InferenceBackend, InferenceRequest, InferenceResponse, InferenceResponseMetadata};
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

    pub fn clear_context(&mut self) {
        self.runtime.clear_context();
    }
}

impl InferenceBackend for LlamaCppBackend {
    fn backend_name(&self) -> &'static str {
        self.runtime.config().backend.kind.as_str()
    }

    fn generate(&mut self, request: InferenceRequest) -> Result<InferenceResponse> {
        ensure!(
            request.run_id == request.rendered.run_id,
            "inference request run ID does not match its rendered model request"
        );
        ensure!(
            request.turn_id == request.rendered.turn_id,
            "inference request turn ID does not match its rendered model request"
        );
        let paths = self
            .artifact_root
            .paths(&request.run_id, &request.attempt_id);
        let writer = InferenceEventWriter::open(
            paths.events_jsonl(),
            request.session_id.clone(),
            request.request_id.clone(),
        )?;
        let mut machine = InferenceAttemptMachine::new(
            request.run_id.clone(),
            request.turn_id.clone(),
            request.attempt_id.clone(),
        );
        let backend = self.backend_name();
        apply_inference_transition(
            &writer,
            &mut machine,
            InferenceAttemptTransition::StartAttempt {
                backend: backend.to_string(),
                request_path: paths.request_json().to_path_buf(),
            },
        )?;
        paths.write_request_json(&request)?;
        apply_inference_transition(
            &writer,
            &mut machine,
            InferenceAttemptTransition::RecordRequest {
                path: paths.request_json().to_path_buf(),
            },
        )?;

        let runtime_started = Instant::now();
        apply_inference_transition(
            &writer,
            &mut machine,
            InferenceAttemptTransition::StartRuntime,
        )?;
        let output = match self.runtime.generate(&request.rendered) {
            Ok(output) => output,
            Err(err) => {
                let duration_ms = elapsed_ms(runtime_started);
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
                apply_inference_transition(
                    &writer,
                    &mut machine,
                    InferenceAttemptTransition::RecordRuntimeLog {
                        path: paths.runtime_log().to_path_buf(),
                    },
                )?;
                apply_inference_transition(
                    &writer,
                    &mut machine,
                    InferenceAttemptTransition::FailAttempt {
                        message: message.clone(),
                    },
                )?;
                apply_inference_transition(
                    &writer,
                    &mut machine,
                    InferenceAttemptTransition::FinishAttempt {
                        status: InferenceFinishStatus::Failed,
                    },
                )?;
                tracing::error!(
                    target: "agentlibre::inference",
                    run_id = %request.run_id,
                    attempt_id = %request.attempt_id,
                    backend,
                    duration_ms,
                    error = %err_text,
                    "llama.cpp inference attempt failed"
                );
                bail!("{message}");
            }
        };
        paths.write_runtime_log(output.log)?;
        apply_inference_transition(
            &writer,
            &mut machine,
            InferenceAttemptTransition::RecordRuntimeLog {
                path: paths.runtime_log().to_path_buf(),
            },
        )?;

        let response = InferenceResponse {
            attempt_id: request.attempt_id.clone(),
            content: output.content,
            finish_reason: output.finish_reason,
            metadata: InferenceResponseMetadata {
                model_state: Some(output.model_state),
                selected_device: output.selected_device,
                duration_ms: elapsed_ms(runtime_started),
            },
        };
        paths.write_response_json(&response)?;
        apply_inference_transition(
            &writer,
            &mut machine,
            InferenceAttemptTransition::RecordResponse {
                path: paths.response_json().to_path_buf(),
            },
        )?;
        apply_inference_transition(
            &writer,
            &mut machine,
            InferenceAttemptTransition::FinishAttempt {
                status: InferenceFinishStatus::Succeeded,
            },
        )?;
        tracing::info!(
            target: "agentlibre::inference",
            run_id = %request.run_id,
            attempt_id = %request.attempt_id,
            backend,
            finish_reason = ?response.finish_reason,
            model_state = %response.metadata.model_state.as_deref().unwrap_or("unknown"),
            selected_device = %response.metadata.selected_device.as_deref().unwrap_or(""),
            duration_ms = response.metadata.duration_ms,
            content_bytes = response.content.len(),
            "llama.cpp inference attempt succeeded"
        );
        Ok(response)
    }
}

fn apply_inference_transition(
    writer: &InferenceEventWriter,
    machine: &mut InferenceAttemptMachine,
    transition: InferenceAttemptTransition,
) -> Result<InferenceAttemptTransitionRecord> {
    let record = machine.apply(transition)?;
    writer.append_transition(&record)?;
    Ok(record)
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
impl LlamaCppBackend {
    pub(crate) fn new_with_test_runtime(
        config: LocalInferenceConfig,
        artifact_root: InferenceArtifactRoot,
        responses: Vec<&str>,
    ) -> Result<Self> {
        Self::new_with_test_runtime_and_auto_device(config, artifact_root, responses, None)
    }

    pub(crate) fn new_with_test_runtime_and_auto_device(
        config: LocalInferenceConfig,
        artifact_root: InferenceArtifactRoot,
        responses: Vec<&str>,
        auto_selected_device: Option<&str>,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            artifact_root,
            runtime: LlamaCppRuntime::new_test(
                config,
                DEFAULT_MAX_OUTPUT_TOKENS,
                responses,
                auto_selected_device,
            ),
        })
    }
}
