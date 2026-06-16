use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agl_config::{
    BackendKind, InferenceBackendConfig, InferenceRuntimeConfig, KvCacheType, LocalInferenceConfig,
    ModelConfig, ModelDialect, RuntimeSwitch, ToolCallFormat, load_local_inference_config,
};
use agl_oven::{RenderedMessage, RenderedMessageRole, RenderedModelRequest, RenderedTool};

use crate::evidence::{InferenceArtifactRoot, InferenceAttemptId, InferenceRunId};
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

fn inference_request_with_messages(
    attempt_id: &str,
    request_index: usize,
    messages: Vec<RenderedMessage>,
) -> InferenceRequest {
    InferenceRequest {
        run_id: InferenceRunId::new("run-001").unwrap(),
        attempt_id: InferenceAttemptId::new(attempt_id).unwrap(),
        rendered: RenderedModelRequest {
            turn_id: "turn-1".to_string(),
            request_index,
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
            messages,
            tools: Vec::new(),
        },
    }
}

fn local_config() -> LocalInferenceConfig {
    LocalInferenceConfig {
        backend: InferenceBackendConfig {
            kind: BackendKind::LlamaCpp,
            model: PathBuf::from("/models/qwen3.6.gguf"),
        },
        runtime: InferenceRuntimeConfig {
            gpu_layers: 999,
            context_tokens: 32768,
            threads: 8,
            device: Some("Vulkan0".to_string()),
            batch_size: Some(1024),
            ubatch_size: Some(256),
            flash_attention: Some(RuntimeSwitch::On),
            cache_type_k: Some(KvCacheType::Q8_0),
            cache_type_v: Some(KvCacheType::Q8_0),
            mmap: Some(false),
        },
        model: ModelConfig {
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
        },
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
}

#[test]
fn llama_cpp_backend_records_invalid_runtime_request_without_panicking() {
    let root_path = temp_root("llama-invalid-invocation");
    let artifact_root = InferenceArtifactRoot::new(&root_path);
    let request = inference_request();
    let paths = artifact_root.paths(&request.run_id, &request.attempt_id);
    let mut backend = LlamaCppBackend::new(local_config(), artifact_root)
        .unwrap()
        .with_max_output_tokens(0);

    let err = backend.generate(request).unwrap_err();

    assert!(err.to_string().contains("llama.cpp runtime failed"));
    assert!(paths.request_json().exists());
    assert!(!paths.response_json().exists());
    assert!(
        std::fs::read_to_string(paths.runtime_log())
            .unwrap()
            .contains("llama.cpp max_output_tokens cannot be zero")
    );
    let events = std::fs::read_to_string(paths.events_jsonl()).unwrap();
    assert!(events.contains("\"kind\":\"inference.attempt_failed\""));
    assert!(events.contains("\"finish_status\":\"failed\""));

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
fn llama_cpp_backend_reuses_test_runtime_session_and_records_artifacts() {
    let root_path = temp_root("llama-session-reuse");
    let artifact_root = InferenceArtifactRoot::new(&root_path);
    let run_id = InferenceRunId::new("run-001").unwrap();
    let first_attempt = InferenceAttemptId::new("attempt-0001").unwrap();
    let second_attempt = InferenceAttemptId::new("attempt-0002").unwrap();
    let mut backend = LlamaCppBackend::new_with_test_runtime(
        local_config(),
        artifact_root.clone(),
        vec!["first answer", "second answer\n\nUser:\ncontinuation"],
    )
    .unwrap();

    let first = inference_request_with_messages(
        first_attempt.as_str(),
        1,
        vec![RenderedMessage {
            role: RenderedMessageRole::User,
            content: "first".to_string(),
            name: None,
            tool_calls: Vec::new(),
        }],
    );
    let second = inference_request_with_messages(
        second_attempt.as_str(),
        2,
        vec![
            RenderedMessage {
                role: RenderedMessageRole::User,
                content: "first".to_string(),
                name: None,
                tool_calls: Vec::new(),
            },
            RenderedMessage {
                role: RenderedMessageRole::Assistant,
                content: "first answer".to_string(),
                name: None,
                tool_calls: Vec::new(),
            },
            RenderedMessage {
                role: RenderedMessageRole::User,
                content: "second".to_string(),
                name: None,
                tool_calls: Vec::new(),
            },
        ],
    );

    let first_response = backend.generate(first).unwrap();
    let second_response = backend.generate(second).unwrap();

    assert_eq!(first_response.content, "first answer");
    assert_eq!(
        first_response.metadata.model_state.as_deref(),
        Some("loaded")
    );
    assert_eq!(
        first_response.metadata.selected_device.as_deref(),
        Some("Vulkan0")
    );
    assert_eq!(second_response.content, "second answer\n");
    assert_eq!(
        second_response.metadata.model_state.as_deref(),
        Some("reused")
    );
    assert_eq!(
        second_response.metadata.selected_device.as_deref(),
        Some("Vulkan0")
    );
    assert!(second_response.metadata.duration_ms < 60_000);

    let first_paths = artifact_root.paths(&run_id, &first_attempt);
    let second_paths = artifact_root.paths(&run_id, &second_attempt);
    assert!(first_paths.request_json().exists());
    assert!(first_paths.response_json().exists());
    assert!(first_paths.runtime_log().exists());
    assert!(second_paths.request_json().exists());
    assert!(second_paths.response_json().exists());
    assert!(second_paths.runtime_log().exists());
    assert!(second_paths.events_jsonl().exists());

    let first_runtime_log = std::fs::read_to_string(first_paths.runtime_log()).unwrap();
    let second_runtime_log = std::fs::read_to_string(second_paths.runtime_log()).unwrap();
    assert!(first_runtime_log.contains("model_state = loaded"));
    assert!(first_runtime_log.contains("thinking_prefill = disabled"));
    assert!(first_runtime_log.contains("llama_cpp_prompt_append:\nUser: first\n"));
    assert!(second_runtime_log.contains("model_state = reused"));
    assert!(second_runtime_log.contains("llama_cpp_session_load_log:"));
    assert!(second_runtime_log.contains("load_tensors: offloaded 66/66 layers to GPU"));
    assert!(second_runtime_log.contains("rendered_message_history_len = 2"));
    assert!(second_runtime_log.contains("llama_cpp_prompt_append:\nUser: second\n"));
    assert!(!second_runtime_log.contains("User: first\n"));

    let events = std::fs::read_to_string(second_paths.events_jsonl()).unwrap();
    assert!(events.contains("\"backend\":\"llama_cpp\""));
    assert!(events.contains("\"kind\":\"inference.response_recorded\""));

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
fn llama_cpp_invocation_module_does_not_return() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("llama_cpp")
        .join("invocation.rs");

    assert!(!path.exists(), "{} must stay removed", path.display());
}

#[test]
fn local_llama_cpp_config_has_no_executable_path() {
    let config = serde_json::to_string(&local_config()).unwrap();

    assert!(!config.contains("binary"));
    assert!(!config.contains("llama-completion"));
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
        LlamaCppBackend::new(config, InferenceArtifactRoot::new(artifact_root)).unwrap();
    let response = backend.generate(request).unwrap();

    assert!(!response.content.trim().is_empty());
}
