use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agl_config::{
    load_local_inference_config, BackendKind, InferenceBackendConfig, InferenceRuntimeConfig,
    LocalInferenceConfig, ModelConfig, ModelDialect, ToolCallFormat,
};
use agl_model::{
    RenderedMessage, RenderedMessageRole, RenderedModelRequest, RenderedTool, RenderedToolCall,
};
use agl_observe::{InferenceArtifactRoot, InferenceAttemptId, InferenceRunId};
use serde_json::json;

use crate::*;

static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_root(name: &str) -> PathBuf {
    let id = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("agl-inference-{name}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn rendered_request() -> RenderedModelRequest {
    RenderedModelRequest {
        turn_id: "turn-1".to_string(),
        request_index: 2,
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
        messages: vec![RenderedMessage {
            role: RenderedMessageRole::User,
            content: "use the rendered request".to_string(),
            name: None,
            tool_calls: Vec::new(),
        }],
        tools: vec![RenderedTool {
            name: "read_file".to_string(),
            required_arguments: vec!["path".to_string()],
        }],
    }
}

fn inference_request() -> InferenceRequest {
    InferenceRequest {
        run_id: InferenceRunId::new("run-001").unwrap(),
        attempt_id: InferenceAttemptId::new("attempt-001").unwrap(),
        rendered: rendered_request(),
    }
}

fn local_config(binary: impl Into<PathBuf>) -> LocalInferenceConfig {
    LocalInferenceConfig {
        backend: InferenceBackendConfig {
            kind: BackendKind::LlamaCpp,
            binary: binary.into(),
            model: PathBuf::from("/models/qwen3.6.gguf"),
        },
        runtime: InferenceRuntimeConfig {
            gpu_layers: 999,
            context_tokens: 32768,
            threads: 8,
        },
        model: ModelConfig {
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
        },
    }
}

#[test]
fn fake_backend_returns_configured_response_and_records_rendered_request() {
    let request = inference_request();
    let mut backend =
        FakeInferenceBackend::new("final answer").with_finish_reason(InferenceFinishReason::Length);

    let response = backend.generate(request.clone()).unwrap();

    assert_eq!(
        response,
        InferenceResponse {
            content: "final answer".to_string(),
            finish_reason: InferenceFinishReason::Length,
        }
    );
    assert_eq!(backend.recorded_requests(), &[request]);
}

#[test]
fn fake_backend_records_success_attempt_with_observation_artifacts() {
    let root_path = temp_root("success");
    let artifact_root = InferenceArtifactRoot::new(&root_path);
    let request = inference_request();
    let paths = artifact_root.paths(&request.run_id, &request.attempt_id);
    let mut backend = FakeInferenceBackend::new("observed response")
        .with_backend_name("fake-observed")
        .with_artifact_root(artifact_root);

    let response = backend.generate(request).unwrap();

    assert_eq!(response.content, "observed response");
    assert!(std::fs::read_to_string(paths.request_json())
        .unwrap()
        .contains("\"rendered\": {"));
    assert_eq!(
        std::fs::read_to_string(paths.response_json()).unwrap(),
        "{\n  \"content\": \"observed response\",\n  \"finish_reason\": \"stop\"\n}\n"
    );
    let events = std::fs::read_to_string(paths.events_jsonl()).unwrap();
    assert!(events.contains("\"kind\":\"inference.attempt_started\""));
    assert!(events.contains("\"backend\":\"fake-observed\""));
    assert!(events.contains("\"kind\":\"inference.request_recorded\""));
    assert!(events.contains("\"kind\":\"inference.response_recorded\""));
    assert!(events.contains("\"finish_status\":\"succeeded\""));

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
fn fake_backend_records_failed_attempt_with_controlled_error() {
    let root_path = temp_root("failure");
    let artifact_root = InferenceArtifactRoot::new(&root_path);
    let request = inference_request();
    let paths = artifact_root.paths(&request.run_id, &request.attempt_id);
    let mut backend = FakeInferenceBackend::new("ignored")
        .failing("controlled failure")
        .with_artifact_root(artifact_root);

    let err = backend.generate(request.clone()).unwrap_err();

    assert_eq!(err.to_string(), "controlled failure");
    assert_eq!(backend.recorded_requests(), &[request]);
    assert!(paths.request_json().exists());
    assert!(!paths.response_json().exists());
    let events = std::fs::read_to_string(paths.events_jsonl()).unwrap();
    assert!(events.contains("\"kind\":\"inference.attempt_failed\""));
    assert!(events.contains("\"message\":\"controlled failure\""));
    assert!(events.contains("\"finish_status\":\"failed\""));

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
fn rendered_model_request_round_trips_for_artifacts() {
    let rendered = rendered_request();

    let encoded = serde_json::to_string(&rendered).unwrap();
    let decoded: RenderedModelRequest = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded, rendered);
    assert!(encoded.contains("\"dialect\":\"qwen3\""));
    assert!(encoded.contains("\"role\":\"user\""));
}

#[test]
fn llama_cpp_backend_builds_cli_arguments_from_config() {
    let root_path = temp_root("args");
    let backend = LlamaCppCliBackend::new(
        local_config("/opt/llama.cpp/build/bin/llama-cli"),
        InferenceArtifactRoot::new(&root_path),
    )
    .unwrap()
    .with_max_output_tokens(64);

    let args = backend
        .command_args("User:\nhello\n\nAssistant:\n")
        .into_iter()
        .map(|value| value.to_string_lossy().to_string())
        .collect::<Vec<_>>();

    assert_eq!(
        args,
        [
            "-m",
            "/models/qwen3.6.gguf",
            "-p",
            "User:\nhello\n\nAssistant:\n",
            "-n",
            "64",
            "-c",
            "32768",
            "-ngl",
            "999",
            "-t",
            "8",
        ]
    );
}

#[test]
fn llama_cpp_prompt_uses_rendered_request_fields() {
    let mut rendered = rendered_request();
    rendered.messages.push(RenderedMessage {
        role: RenderedMessageRole::Assistant,
        content: String::new(),
        name: None,
        tool_calls: vec![RenderedToolCall {
            name: "read_file".to_string(),
            arguments: json!({"path": "README.MD"}),
        }],
    });

    let prompt = crate::llama_cpp::render_llama_cli_prompt(&rendered).unwrap();

    assert!(prompt.contains("User:\nuse the rendered request\n"));
    assert!(prompt
        .contains("Assistant:\n{\"arguments\":{\"path\":\"README.MD\"},\"name\":\"read_file\"}\n"));
    assert!(prompt.contains("Available tools:\n- read_file required: path\n"));
    assert!(prompt.ends_with("Assistant:\n"));
}

#[test]
fn llama_cpp_backend_records_launch_failure_with_artifacts() {
    let root_path = temp_root("llama-failure");
    let artifact_root = InferenceArtifactRoot::new(&root_path);
    let request = inference_request();
    let paths = artifact_root.paths(&request.run_id, &request.attempt_id);
    let mut backend = LlamaCppCliBackend::new(
        local_config("/definitely/not/installed/llama-cli"),
        artifact_root,
    )
    .unwrap();

    let err = backend.generate(request).unwrap_err();

    assert!(err
        .to_string()
        .contains("failed to launch llama.cpp binary"));
    assert!(paths.request_json().exists());
    assert!(!paths.response_json().exists());
    assert!(std::fs::read_to_string(paths.request_json())
        .unwrap()
        .contains("\"tool_call_format\": \"hermes_json\""));
    assert!(std::fs::read_to_string(paths.stderr_log())
        .unwrap()
        .contains("failed to launch llama.cpp binary"));
    let events = std::fs::read_to_string(paths.events_jsonl()).unwrap();
    assert!(events.contains("\"backend\":\"llama_cpp_cli\""));
    assert!(events.contains("\"kind\":\"inference.request_recorded\""));
    assert!(events.contains("\"kind\":\"inference.attempt_failed\""));
    assert!(events.contains("\"finish_status\":\"failed\""));

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
#[ignore = "requires AGL_LOCAL_INFERENCE_CONFIG and AGL_INFERENCE_ARTIFACT_ROOT"]
fn manual_llama_cpp_smoke_from_env() {
    let config_path = std::env::var("AGL_LOCAL_INFERENCE_CONFIG")
        .expect("AGL_LOCAL_INFERENCE_CONFIG must point to a local inference TOML file");
    let artifact_root = std::env::var("AGL_INFERENCE_ARTIFACT_ROOT")
        .expect("AGL_INFERENCE_ARTIFACT_ROOT must point to an artifact directory");
    let mut request = inference_request();
    request.run_id = InferenceRunId::new("manual-smoke").unwrap();
    request.attempt_id = InferenceAttemptId::new("attempt-001").unwrap();
    request.rendered = RenderedModelRequest {
        turn_id: "manual-smoke-turn".to_string(),
        request_index: 0,
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
        messages: vec![RenderedMessage {
            role: RenderedMessageRole::User,
            content: "Reply with one short sentence.".to_string(),
            name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };

    let config = load_local_inference_config(config_path).unwrap();
    let mut backend =
        LlamaCppCliBackend::new(config, InferenceArtifactRoot::new(artifact_root)).unwrap();
    let response = backend.generate(request).unwrap();

    assert!(!response.content.trim().is_empty());
}
