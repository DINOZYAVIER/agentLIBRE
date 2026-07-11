use std::path::PathBuf;

use agl_config::{
    BackendKind, InferenceBackendConfig, InferenceRuntimeConfig, LocalInferenceConfig, ModelConfig,
    ModelDialect, MtpRuntimeConfig, PromptConfig, ToolCallFormat, load_local_inference_config,
};
use agl_ids::{AttemptId, RequestId, RunId, SessionId, TurnId};
use agl_oven::{RenderedMessage, RenderedMessageRole, RenderedModelRequest, RenderedTool};

use crate::evidence::InferenceArtifactRoot;
use crate::*;

const TEST_RUN_ID: &str = "run_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b31";
const TEST_TURN_ID: &str = "turn_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b32";
const TEST_SESSION_ID: &str = "ses_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b33";
const TEST_REQUEST_ID: &str = "req_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b34";
const TEST_ATTEMPT_ID: &str = "attempt_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b35";

fn run_id() -> RunId {
    RunId::parse(TEST_RUN_ID).unwrap()
}

fn turn_id() -> TurnId {
    TurnId::parse(TEST_TURN_ID).unwrap()
}

fn session_id() -> SessionId {
    SessionId::parse(TEST_SESSION_ID).unwrap()
}

fn request_id() -> RequestId {
    RequestId::parse(TEST_REQUEST_ID).unwrap()
}

fn attempt_id() -> AttemptId {
    AttemptId::parse(TEST_ATTEMPT_ID).unwrap()
}

fn rendered_request() -> RenderedModelRequest {
    RenderedModelRequest {
        run_id: run_id(),
        turn_id: turn_id(),
        request_index: 2,
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
        messages: vec![RenderedMessage {
            role: RenderedMessageRole::User,
            content: Some(agl_content::Content::text("use the rendered request").unwrap()),
            name: None,
            tool_calls: Vec::new(),
        }],
        tools: vec![RenderedTool {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            input_schema: serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }),
        }],
    }
}

fn inference_request() -> InferenceRequest {
    InferenceRequest {
        run_id: run_id(),
        turn_id: turn_id(),
        attempt_id: attempt_id(),
        session_id: None,
        request_id: None,
        rendered: rendered_request(),
    }
}

fn local_config() -> LocalInferenceConfig {
    LocalInferenceConfig {
        backend: InferenceBackendConfig {
            kind: BackendKind::LlamaCpp,
            model: PathBuf::from("/models/qwen.gguf"),
            multimodal_projector: None,
        },
        runtime: InferenceRuntimeConfig {
            gpu_layers: 0,
            context_tokens: 4096,
            threads: 4,
            device: None,
            batch_size: None,
            ubatch_size: None,
            flash_attention: None,
            cache_type_k: None,
            cache_type_v: None,
            mmap: Some(true),
            kv_unified: None,
            mtp: MtpRuntimeConfig::default(),
        },
        model: ModelConfig {
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
        },
        prompt: PromptConfig::default(),
    }
}

#[test]
fn rendered_model_request_round_trips_for_artifacts() {
    let rendered = rendered_request();
    let encoded = serde_json::to_string(&rendered).unwrap();
    let decoded: RenderedModelRequest = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded, rendered);
    assert!(encoded.contains("\"dialect\":\"qwen3\""));
    assert!(encoded.contains("\"role\":\"user\""));
    assert_eq!(decoded.tools[0].input_schema["required"][0], "path");
}

#[test]
fn inference_request_round_trips_correlation_and_rejects_unknown_fields() {
    let uncorrelated = serde_json::to_value(inference_request()).unwrap();
    assert!(uncorrelated.get("session_id").is_none());
    assert!(uncorrelated.get("request_id").is_none());

    let mut request = inference_request();
    request.session_id = Some(session_id());
    request.request_id = Some(request_id());
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(
        serde_json::from_value::<InferenceRequest>(value.clone()).unwrap(),
        request
    );

    let mut unknown = value;
    unknown["legacy_correlation"] = serde_json::Value::Bool(true);
    assert!(serde_json::from_value::<InferenceRequest>(unknown).is_err());
}

#[test]
fn invalid_job_scope_is_rejected_before_manager_admission() {
    let config = local_config();
    let mut request = inference_request();
    request.rendered.turn_id = TurnId::generate();
    let context = ContextKey::for_conversation(&config, TEST_SESSION_ID).unwrap();

    assert!(matches!(
        InferenceJob::new(
            config,
            request,
            context,
            InferenceArtifactRoot::new("/tmp/unused"),
            PathBuf::from("/tmp/unused-store"),
            32,
        ),
        Err(ModelManagerError::ProfileInvalid { .. })
    ));
}

#[test]
fn local_llama_cpp_config_has_no_executable_path() {
    let value = serde_json::to_value(local_config()).unwrap();

    assert!(value["backend"].get("executable").is_none());
    assert!(value["backend"].get("args").is_none());
}

#[test]
#[ignore = "requires AGL_LOCAL_INFERENCE_CONFIG, AGL_INFERENCE_ARTIFACT_ROOT, and AGL_STORE_ROOT"]
fn manual_llama_cpp_smoke_from_env() -> anyhow::Result<()> {
    let config_path = std::env::var("AGL_LOCAL_INFERENCE_CONFIG")?;
    let artifact_root = std::env::var("AGL_INFERENCE_ARTIFACT_ROOT")?;
    let config = load_local_inference_config(config_path)?;
    let context = ContextKey::for_conversation(&config, "manual-smoke")?;
    let job = InferenceJob::new(
        config,
        inference_request(),
        context,
        InferenceArtifactRoot::new(artifact_root),
        PathBuf::from(std::env::var("AGL_STORE_ROOT")?),
        64,
    )?;
    let mut manager =
        ModelManager::spawn(ModelManagerOptions::default(), LlamaCppModelRuntime::new())?;
    let response = manager.handle().generate(job);
    let shutdown = manager.shutdown();

    let response = response?;
    shutdown?;
    assert!(!response.content.trim().is_empty());
    assert_eq!(response.attempt_id, attempt_id());
    Ok(())
}
