use std::env;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agl_config::{ModelConfig, load_local_inference_config};
use agl_inference::evidence::{InferenceArtifactRoot, InferenceAttemptId, InferenceRunId};
use agl_inference::{InferenceBackend, InferenceRequest, InferenceResponse, LlamaCppBackend};
use agl_oven::render_model_request;
use agl_turn::{ModelRequest, TurnMessage};
use anyhow::{Context, Result};

use crate::args::RunOptions;

const CONFIG_ENV: &str = "AGL_LOCAL_INFERENCE_CONFIG";
const ARTIFACT_ROOT_ENV: &str = "AGL_INFERENCE_ARTIFACT_ROOT";

pub(crate) struct InferenceSession {
    backend: LlamaCppBackend,
    model_config: ModelConfig,
    run_id: InferenceRunId,
}

impl InferenceSession {
    pub(crate) fn new(options: RunOptions) -> Result<Self> {
        let config_path = options
            .config
            .or_else(|| env::var_os(CONFIG_ENV).map(PathBuf::from))
            .context("missing --config PATH or AGL_LOCAL_INFERENCE_CONFIG")?;
        let artifact_root = options
            .artifact_root
            .or_else(|| env::var_os(ARTIFACT_ROOT_ENV).map(PathBuf::from))
            .context("missing --artifact-root DIR or AGL_INFERENCE_ARTIFACT_ROOT")?;

        let config = load_local_inference_config(&config_path).with_context(|| {
            format!(
                "failed to load local inference config {}",
                config_path.display()
            )
        })?;
        let model_config = config.model.clone();
        let backend = LlamaCppBackend::new(config, InferenceArtifactRoot::new(artifact_root))?
            .with_max_output_tokens(options.max_output_tokens);
        let run_id = InferenceRunId::new(options.run_id.unwrap_or_else(default_run_id))?;

        Ok(Self {
            backend,
            model_config,
            run_id,
        })
    }

    pub(crate) fn generate(
        &mut self,
        messages: &[TurnMessage],
        request_index: usize,
    ) -> Result<InferenceResponse> {
        let request = build_inference_request(
            self.run_id.clone(),
            request_index,
            messages.to_vec(),
            &self.model_config,
        )?;
        self.backend.generate(request)
    }
}

fn build_inference_request(
    run_id: InferenceRunId,
    request_index: usize,
    messages: Vec<TurnMessage>,
    model_config: &ModelConfig,
) -> Result<InferenceRequest> {
    let model_request = ModelRequest {
        turn_id: run_id.to_string(),
        request_index,
        messages,
        visible_tools: Vec::new(),
    };
    let rendered = render_model_request(&model_request, model_config)?;
    Ok(InferenceRequest {
        run_id,
        attempt_id: InferenceAttemptId::new(format!("attempt-{request_index:04}"))?,
        rendered,
    })
}

fn default_run_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("manual-{millis}")
}

#[cfg(test)]
mod tests {
    use agl_config::{ModelDialect, ToolCallFormat};

    use super::*;

    #[test]
    fn build_request_uses_agentlibre_boundaries() {
        let run_id = InferenceRunId::new("manual-test").unwrap();
        let config = ModelConfig {
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
        };

        let request = build_inference_request(
            run_id.clone(),
            7,
            vec![TurnMessage::User {
                content: "hello".to_string(),
            }],
            &config,
        )
        .unwrap();

        assert_eq!(request.run_id, run_id);
        assert_eq!(request.attempt_id.as_str(), "attempt-0007");
        assert_eq!(request.rendered.turn_id, "manual-test");
        assert_eq!(request.rendered.request_index, 7);
        assert_eq!(request.rendered.messages.len(), 1);
        assert_eq!(request.rendered.dialect, ModelDialect::Qwen3);
        assert_eq!(
            request.rendered.tool_call_format,
            ToolCallFormat::HermesJson
        );
    }
}
