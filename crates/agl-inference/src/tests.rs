use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agl_config::{ModelDialect, ToolCallFormat};
use agl_model::{RenderedMessage, RenderedMessageRole, RenderedModelRequest, RenderedTool};
use agl_observe::{InferenceArtifactRoot, InferenceAttemptId, InferenceRunId};

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
