use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agl_config::{
    BackendKind, InferenceBackendConfig, InferenceRuntimeConfig, KvCacheType, LocalInferenceConfig,
    ModelConfig, ModelDialect, MtpProbability, MtpRuntimeConfig, PromptConfig, RuntimeSwitch,
    ToolCallFormat, load_local_inference_config,
};
use agl_ids::{AttemptId, RequestId, RunId, SessionId, TurnId};
use agl_oven::{RenderedMessage, RenderedMessageRole, RenderedModelRequest, RenderedTool};

use crate::evidence::InferenceArtifactRoot;
use crate::*;

static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);
const TEST_RUN_ID: &str = "run_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b31";
const TEST_TURN_ID: &str = "turn_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b32";
const TEST_SESSION_ID: &str = "ses_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b33";
const TEST_REQUEST_ID: &str = "req_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b34";

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

fn test_attempt_id(value: &str) -> AttemptId {
    let index = value
        .strip_prefix("attempt-")
        .unwrap()
        .parse::<u64>()
        .unwrap();
    AttemptId::parse(&format!("attempt_01890f3b-6d7a-7c1f-b4b5-{index:012x}")).unwrap()
}

fn temp_root(name: &str) -> PathBuf {
    let id = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("agl-inference-{name}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
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
            content: "use the rendered request".to_string(),
            name: None,
            tool_calls: Vec::new(),
        }],
        tools: vec![RenderedTool {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            input_schema: serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "options": {
                        "type": "object",
                        "properties": {
                            "line_limit": {"type": "integer", "minimum": 1}
                        },
                        "additionalProperties": false
                    }
                },
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
        attempt_id: test_attempt_id("attempt-001"),
        session_id: None,
        request_id: None,
        rendered: rendered_request(),
    }
}

fn inference_request_with_messages(
    attempt_id: AttemptId,
    request_index: usize,
    messages: Vec<RenderedMessage>,
) -> InferenceRequest {
    InferenceRequest {
        run_id: run_id(),
        turn_id: turn_id(),
        attempt_id,
        session_id: None,
        request_id: None,
        rendered: RenderedModelRequest {
            run_id: run_id(),
            turn_id: turn_id(),
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

fn local_config_without_device() -> LocalInferenceConfig {
    let mut config = local_config();
    config.runtime.device = None;
    config
}

#[test]
fn llama_cpp_backend_reports_backend_name() {
    let root_path = temp_root("backend-name");
    let backend = LlamaCppBackend::new(
        local_config(),
        InferenceArtifactRoot::new(root_path.as_path()),
    )
    .unwrap();

    assert_eq!(backend.backend_name(), "llama_cpp");

    let _ = std::fs::remove_dir_all(root_path);
}

#[test]
fn rendered_model_request_round_trips_for_artifacts() {
    let rendered = rendered_request();

    let encoded = serde_json::to_string(&rendered).unwrap();
    let decoded: RenderedModelRequest = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded, rendered);
    assert!(encoded.contains("\"dialect\":\"qwen3\""));
    assert!(encoded.contains("\"role\":\"user\""));
    assert_eq!(
        decoded.tools[0].input_schema["properties"]["options"]["properties"]["line_limit"]["minimum"],
        1
    );
}

#[test]
fn inference_request_round_trips_optional_correlation_and_rejects_unknown_fields() {
    let uncorrelated = serde_json::to_value(inference_request()).unwrap();
    assert!(uncorrelated.get("session_id").is_none());
    assert!(uncorrelated.get("request_id").is_none());

    let mut request = inference_request();
    request.session_id = Some(session_id());
    request.request_id = Some(request_id());

    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(value["session_id"], TEST_SESSION_ID);
    assert_eq!(value["request_id"], TEST_REQUEST_ID);
    assert_eq!(
        serde_json::from_value::<InferenceRequest>(value.clone()).unwrap(),
        request
    );

    let mut unknown = value;
    unknown["legacy_correlation"] = serde_json::Value::Bool(true);
    assert!(serde_json::from_value::<InferenceRequest>(unknown).is_err());

    let mut invalid_id = serde_json::to_value(&request).unwrap();
    invalid_id["session_id"] = serde_json::Value::String("session-legacy".to_string());
    assert!(serde_json::from_value::<InferenceRequest>(invalid_id).is_err());
}

#[test]
fn llama_cpp_backend_records_invalid_runtime_request_without_panicking() {
    let root_path = temp_root("llama-invalid-invocation");
    let artifact_root = InferenceArtifactRoot::new(&root_path);
    let mut request = inference_request();
    request.session_id = Some(session_id());
    request.request_id = Some(request_id());
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
    for event in events
        .lines()
        .map(|line| serde_json::from_str::<agl_events::SafeRuntimeEventEnvelope>(line).unwrap())
    {
        assert_eq!(event.scope.session_id(), Some(&session_id()));
        assert_eq!(event.request_id.as_ref(), Some(&request_id()));
    }

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
fn llama_cpp_test_runtime_records_enabled_mtp_config() {
    let root_path = temp_root("llama-mtp-test-runtime");
    let artifact_root = InferenceArtifactRoot::new(&root_path);
    let request = inference_request();
    let paths = artifact_root.paths(&request.run_id, &request.attempt_id);
    let mut config = local_config_without_device();
    config.runtime.mtp = MtpRuntimeConfig {
        enabled: true,
        draft_model: Some(PathBuf::from("/models/gemma4-mtp-q4_0.gguf")),
        draft_tokens: 4,
        p_min: MtpProbability::from_f32(0.2).unwrap(),
        gpu_layers: Some(999),
        cache_type_k: Some(KvCacheType::Q8_0),
        cache_type_v: Some(KvCacheType::Q8_0),
    };
    let mut backend =
        LlamaCppBackend::new_with_test_runtime(config, artifact_root, vec!["mtp test"]).unwrap();

    let response = backend.generate(request).unwrap();

    assert_eq!(response.content, "mtp test");
    let runtime_log = std::fs::read_to_string(paths.runtime_log()).unwrap();
    assert!(runtime_log.contains("mtp_enabled = true"));
    assert!(runtime_log.contains("mtp_draft_model = /models/gemma4-mtp-q4_0.gguf"));
    assert!(runtime_log.contains("mtp_draft_tokens = 4"));
    assert!(runtime_log.contains("mtp_p_min = 0.2"));
    assert!(paths.response_json().exists());

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
fn llama_cpp_backend_reuses_test_runtime_session_and_records_artifacts() {
    let root_path = temp_root("llama-session-reuse");
    let artifact_root = InferenceArtifactRoot::new(&root_path);
    let run_id = run_id();
    let first_attempt = test_attempt_id("attempt-0001");
    let second_attempt = test_attempt_id("attempt-0002");
    let mut backend = LlamaCppBackend::new_with_test_runtime(
        local_config(),
        artifact_root.clone(),
        vec![
            "first answer",
            "second answer\n\nUser:\ncontinuation",
            "third answer",
        ],
    )
    .unwrap();

    let first = inference_request_with_messages(
        first_attempt.clone(),
        1,
        vec![RenderedMessage {
            role: RenderedMessageRole::User,
            content: "first".to_string(),
            name: None,
            tool_calls: Vec::new(),
        }],
    );
    let second = inference_request_with_messages(
        second_attempt.clone(),
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
    let third_attempt = test_attempt_id("attempt-0003");
    let third = inference_request_with_messages(
        third_attempt.clone(),
        3,
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
                content: "third".to_string(),
                name: None,
                tool_calls: Vec::new(),
            },
        ],
    );

    let first_response = backend.generate(first).unwrap();
    let second_response = backend.generate(second).unwrap();
    let third_response = backend.generate(third).unwrap();

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
    assert_eq!(third_response.content, "third answer");
    assert_eq!(
        third_response.metadata.model_state.as_deref(),
        Some("loaded")
    );

    let first_paths = artifact_root.paths(&run_id, &first_attempt);
    let second_paths = artifact_root.paths(&run_id, &second_attempt);
    let third_paths = artifact_root.paths(&run_id, &third_attempt);
    assert!(first_paths.request_json().exists());
    assert!(first_paths.response_json().exists());
    assert!(first_paths.runtime_log().exists());
    assert!(second_paths.request_json().exists());
    assert!(second_paths.response_json().exists());
    assert!(second_paths.runtime_log().exists());
    assert!(second_paths.events_jsonl().exists());
    assert!(third_paths.response_json().exists());

    let first_runtime_log = std::fs::read_to_string(first_paths.runtime_log()).unwrap();
    let second_runtime_log = std::fs::read_to_string(second_paths.runtime_log()).unwrap();
    let third_runtime_log = std::fs::read_to_string(third_paths.runtime_log()).unwrap();
    assert!(first_runtime_log.contains("model_state = loaded"));
    assert!(first_runtime_log.contains("thinking_prefill = disabled"));
    assert!(first_runtime_log.contains("llama_cpp_prompt_append:\nUser: first\n"));
    assert!(second_runtime_log.contains("model_state = reused"));
    assert!(second_runtime_log.contains("llama_cpp_session_load_log:"));
    assert!(second_runtime_log.contains("load_tensors: offloaded 66/66 layers to GPU"));
    assert!(second_runtime_log.contains("rendered_message_history_len = 2"));
    assert!(second_runtime_log.contains("llama_cpp_prompt_append:\nUser: second\n"));
    assert!(!second_runtime_log.contains("User: first\n"));
    assert!(third_runtime_log.contains("model_state = loaded"));
    assert!(third_runtime_log.contains("rendered_message_history_len = 0"));
    assert!(third_runtime_log.contains("User: first\n"));
    assert!(third_runtime_log.contains("Assistant: first answer\n"));
    assert!(third_runtime_log.contains("User: third\n"));

    let events = std::fs::read_to_string(second_paths.events_jsonl()).unwrap();
    assert!(events.contains("\"backend\":\"llama_cpp\""));
    assert!(events.contains("\"kind\":\"inference.response_recorded\""));

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
fn llama_cpp_backend_resets_session_when_rendered_prefix_changes() {
    let root_path = temp_root("llama-session-prefix-change");
    let artifact_root = InferenceArtifactRoot::new(&root_path);
    let mut backend = LlamaCppBackend::new_with_test_runtime(
        local_config(),
        artifact_root,
        vec!["first answer", "second answer"],
    )
    .unwrap();

    let first = inference_request_with_messages(
        test_attempt_id("attempt-0001"),
        1,
        vec![RenderedMessage {
            role: RenderedMessageRole::User,
            content: "first".to_string(),
            name: None,
            tool_calls: Vec::new(),
        }],
    );
    let second = inference_request_with_messages(
        test_attempt_id("attempt-0002"),
        2,
        vec![
            RenderedMessage {
                role: RenderedMessageRole::User,
                content: "edited first".to_string(),
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

    assert_eq!(
        first_response.metadata.model_state.as_deref(),
        Some("loaded")
    );
    assert_eq!(
        second_response.metadata.model_state.as_deref(),
        Some("loaded")
    );

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
fn llama_cpp_backend_reuses_session_when_only_tool_message_names_are_added() {
    let root_path = temp_root("llama-tool-session-reuse");
    let artifact_root = InferenceArtifactRoot::new(&root_path);
    let generated_tool_call = r#"<tool_call>{"name":"fs.read","arguments":{"path":"facts.txt","limit_lines":20}}</tool_call>"#;
    let canonical_tool_call = r#"<tool_call>{"arguments":{"limit_lines":20,"path":"facts.txt"},"name":"fs.read"}</tool_call>"#;
    let mut backend = LlamaCppBackend::new_with_test_runtime(
        local_config(),
        artifact_root,
        vec![generated_tool_call, "done"],
    )
    .unwrap();

    let first = inference_request_with_messages(
        test_attempt_id("attempt-0001"),
        1,
        vec![RenderedMessage {
            role: RenderedMessageRole::User,
            content: "read facts".to_string(),
            name: None,
            tool_calls: Vec::new(),
        }],
    );
    let second = inference_request_with_messages(
        test_attempt_id("attempt-0002"),
        2,
        vec![
            RenderedMessage {
                role: RenderedMessageRole::User,
                content: "read facts".to_string(),
                name: None,
                tool_calls: Vec::new(),
            },
            RenderedMessage {
                role: RenderedMessageRole::Assistant,
                content: canonical_tool_call.to_string(),
                name: Some("fs.read".to_string()),
                tool_calls: Vec::new(),
            },
            RenderedMessage {
                role: RenderedMessageRole::Tool,
                content: "facts".to_string(),
                name: Some("fs.read".to_string()),
                tool_calls: Vec::new(),
            },
        ],
    );

    let first_response = backend.generate(first).unwrap();
    let second_response = backend.generate(second).unwrap();

    assert_eq!(
        first_response.metadata.model_state.as_deref(),
        Some("loaded")
    );
    assert_eq!(
        second_response.metadata.model_state.as_deref(),
        Some("reused")
    );

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
fn llama_cpp_backend_records_auto_selected_device_metadata() {
    let root_path = temp_root("llama-auto-selected-device");
    let artifact_root = InferenceArtifactRoot::new(&root_path);
    let request = inference_request_with_messages(
        test_attempt_id("attempt-0001"),
        1,
        vec![RenderedMessage {
            role: RenderedMessageRole::User,
            content: "first".to_string(),
            name: None,
            tool_calls: Vec::new(),
        }],
    );
    let paths = artifact_root.paths(&request.run_id, &request.attempt_id);
    let mut backend = LlamaCppBackend::new_with_test_runtime_and_auto_device(
        local_config_without_device(),
        artifact_root,
        vec!["answer"],
        Some("Vulkan0"),
    )
    .unwrap();

    let response = backend.generate(request).unwrap();

    assert_eq!(
        response.metadata.selected_device.as_deref(),
        Some("Vulkan0")
    );
    let response_json: InferenceResponse =
        serde_json::from_str(&std::fs::read_to_string(paths.response_json()).unwrap()).unwrap();
    assert_eq!(
        response_json.metadata.selected_device.as_deref(),
        Some("Vulkan0")
    );
    assert!(
        std::fs::read_to_string(paths.runtime_log())
            .unwrap()
            .contains("llama_prepare_model_devices: using device Vulkan0")
    );

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
fn llama_cpp_backend_leaves_device_metadata_empty_when_unknown() {
    let root_path = temp_root("llama-no-selected-device");
    let artifact_root = InferenceArtifactRoot::new(&root_path);
    let request = inference_request_with_messages(
        test_attempt_id("attempt-0001"),
        1,
        vec![RenderedMessage {
            role: RenderedMessageRole::User,
            content: "first".to_string(),
            name: None,
            tool_calls: Vec::new(),
        }],
    );
    let paths = artifact_root.paths(&request.run_id, &request.attempt_id);
    let mut backend = LlamaCppBackend::new_with_test_runtime(
        local_config_without_device(),
        artifact_root,
        vec!["answer"],
    )
    .unwrap();

    let response = backend.generate(request).unwrap();

    assert_eq!(response.metadata.selected_device, None);
    let response_json: InferenceResponse =
        serde_json::from_str(&std::fs::read_to_string(paths.response_json()).unwrap()).unwrap();
    assert_eq!(response_json.metadata.selected_device, None);

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
fn llama_cpp_backend_clear_context_resets_test_runtime_session() {
    let root_path = temp_root("llama-session-clear");
    let artifact_root = InferenceArtifactRoot::new(&root_path);
    let mut backend = LlamaCppBackend::new_with_test_runtime(
        local_config(),
        artifact_root,
        vec!["first answer", "second answer"],
    )
    .unwrap();

    let first = inference_request_with_messages(
        test_attempt_id("attempt-0001"),
        1,
        vec![RenderedMessage {
            role: RenderedMessageRole::User,
            content: "first".to_string(),
            name: None,
            tool_calls: Vec::new(),
        }],
    );
    let second = inference_request_with_messages(
        test_attempt_id("attempt-0002"),
        2,
        vec![RenderedMessage {
            role: RenderedMessageRole::User,
            content: "second".to_string(),
            name: None,
            tool_calls: Vec::new(),
        }],
    );

    let first_response = backend.generate(first).unwrap();
    backend.clear_context();
    let second_response = backend.generate(second).unwrap();

    assert_eq!(
        first_response.metadata.model_state.as_deref(),
        Some("loaded")
    );
    assert_eq!(
        second_response.metadata.model_state.as_deref(),
        Some("loaded")
    );

    std::fs::remove_dir_all(root_path).unwrap();
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
    request.run_id = RunId::generate();
    request.turn_id = TurnId::generate();
    request.attempt_id = AttemptId::generate();
    request.rendered = RenderedModelRequest {
        run_id: request.run_id.clone(),
        turn_id: request.turn_id.clone(),
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
