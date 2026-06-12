use std::ffi::OsString;
use std::process::Command;

use agl_config::{LocalInferenceConfig, RuntimeSwitch};
use agl_llama_cpp::{LlamaCppCliInvocation, LlamaCppSwitch};
use agl_model::{RenderedMessageRole, RenderedModelRequest};
use agl_observe::{
    InferenceArtifactRoot, InferenceEventWriter, InferenceFinishStatus, InferenceObservationEvent,
};
use anyhow::{bail, Context, Result};

use crate::{InferenceBackend, InferenceFinishReason, InferenceRequest, InferenceResponse};

const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 256;

#[derive(Clone, Debug)]
pub struct LlamaCppCliBackend {
    config: LocalInferenceConfig,
    artifact_root: InferenceArtifactRoot,
    max_output_tokens: u32,
}

impl LlamaCppCliBackend {
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

    pub(crate) fn command_args(&self, prompt: &str) -> Result<Vec<OsString>> {
        LlamaCppCliInvocation {
            model: self.config.backend.model.clone(),
            prompt: prompt.to_string(),
            max_output_tokens: self.max_output_tokens,
            context_tokens: self.config.runtime.context_tokens,
            gpu_layers: self.config.runtime.gpu_layers,
            threads: self.config.runtime.threads,
            device: self.config.runtime.device.clone(),
            batch_size: self.config.runtime.batch_size,
            ubatch_size: self.config.runtime.ubatch_size,
            flash_attention: self.config.runtime.flash_attention.map(map_runtime_switch),
            cache_type_k: self
                .config
                .runtime
                .cache_type_k
                .map(|cache_type| cache_type.as_llama_arg().to_string()),
            cache_type_v: self
                .config
                .runtime
                .cache_type_v
                .map(|cache_type| cache_type.as_llama_arg().to_string()),
            mmap: self.config.runtime.mmap,
            jinja: self.config.runtime.jinja,
            conversation: self.config.runtime.conversation,
            simple_io: self.config.runtime.simple_io,
            display_prompt: self.config.runtime.display_prompt,
        }
        .command_args()
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
            backend: "llama_cpp_cli".to_string(),
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

fn map_runtime_switch(value: RuntimeSwitch) -> LlamaCppSwitch {
    match value {
        RuntimeSwitch::On => LlamaCppSwitch::On,
        RuntimeSwitch::Off => LlamaCppSwitch::Off,
        RuntimeSwitch::Auto => LlamaCppSwitch::Auto,
    }
}

impl InferenceBackend for LlamaCppCliBackend {
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

        let prompt = render_llama_cli_prompt(&request.rendered)?;
        let args = match self.command_args(&prompt) {
            Ok(args) => args,
            Err(err) => {
                let message = format!("failed to build llama.cpp CLI arguments: {err}");
                paths.write_stderr_log(format!("{message}\n"))?;
                self.append_failure(&writer, &request, &message)?;
                bail!("{message}");
            }
        };
        let output = Command::new(&self.config.backend.binary)
            .args(args)
            .output();

        let output = match output {
            Ok(output) => output,
            Err(err) => {
                let message = format!(
                    "failed to launch llama.cpp binary {}: {err}",
                    self.config.backend.binary.display()
                );
                paths.write_stderr_log(format!("{message}\n"))?;
                self.append_failure(&writer, &request, &message)?;
                bail!("{message}");
            }
        };

        paths.write_stderr_log(&output.stderr)?;
        if !output.status.success() {
            let message = format!("llama.cpp process exited with status {}", output.status);
            self.append_failure(&writer, &request, &message)?;
            bail!("{message}");
        }

        let response = InferenceResponse {
            content: String::from_utf8_lossy(&output.stdout).to_string(),
            finish_reason: InferenceFinishReason::Stop,
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

pub(crate) fn render_llama_cli_prompt(rendered: &RenderedModelRequest) -> Result<String> {
    let mut prompt = String::new();

    for message in &rendered.messages {
        let role = match message.role {
            RenderedMessageRole::User => "User",
            RenderedMessageRole::Assistant => "Assistant",
            RenderedMessageRole::Tool => "Tool",
        };
        prompt.push_str(role);
        if let Some(name) = &message.name {
            prompt.push(' ');
            prompt.push_str(name);
        }
        prompt.push_str(":\n");
        if !message.content.is_empty() {
            prompt.push_str(&message.content);
            prompt.push('\n');
        }
        for tool_call in &message.tool_calls {
            let payload = serde_json::json!({
                "name": tool_call.name,
                "arguments": tool_call.arguments,
            });
            prompt.push_str(&serde_json::to_string(&payload).context(
                "failed to serialize rendered structured tool call for llama.cpp prompt",
            )?);
            prompt.push('\n');
        }
        prompt.push('\n');
    }

    if !rendered.tools.is_empty() {
        prompt.push_str("Available tools:\n");
        for tool in &rendered.tools {
            prompt.push_str("- ");
            prompt.push_str(&tool.name);
            if !tool.required_arguments.is_empty() {
                prompt.push_str(" required: ");
                prompt.push_str(&tool.required_arguments.join(", "));
            }
            prompt.push('\n');
        }
        prompt.push('\n');
    }

    prompt.push_str("Assistant:\n");
    Ok(prompt)
}
