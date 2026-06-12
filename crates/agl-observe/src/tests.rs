use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use crate::*;

static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_root(name: &str) -> PathBuf {
    let id = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("agl-observe-{name}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn run_id() -> InferenceRunId {
    InferenceRunId::new("run-001").unwrap()
}

fn attempt_id() -> InferenceAttemptId {
    InferenceAttemptId::new("attempt-001").unwrap()
}

#[test]
fn builds_deterministic_artifact_paths_from_caller_root() {
    let root_path = temp_root("paths");
    let root = InferenceArtifactRoot::new(&root_path);

    let paths = root.paths(&run_id(), &attempt_id());

    assert_eq!(root.root(), root_path.as_path());
    assert_eq!(
        paths.run_dir(),
        root_path.join("inference-runs").join("run-001").as_path()
    );
    assert_eq!(
        paths.events_jsonl(),
        root_path
            .join("inference-runs")
            .join("run-001")
            .join("events.jsonl")
            .as_path()
    );
    assert_eq!(
        paths.attempt_dir(),
        root_path
            .join("inference-runs")
            .join("run-001")
            .join("attempts")
            .join("attempt-001")
            .as_path()
    );
    assert_eq!(
        paths.request_json(),
        paths.attempt_dir().join("request.json")
    );
    assert_eq!(
        paths.response_json(),
        paths.attempt_dir().join("response.json")
    );
    assert_eq!(paths.stderr_log(), paths.attempt_dir().join("stderr.log"));
}

#[test]
fn writes_request_response_and_stderr_artifacts() {
    let root_path = temp_root("artifacts");
    let root = InferenceArtifactRoot::new(&root_path);
    let paths = root.paths(&run_id(), &attempt_id());

    let request_path = paths
        .write_request_json(&json!({
            "prompt": "hello",
            "temperature": 0
        }))
        .unwrap();
    let response_path = paths
        .write_response_json(&json!({
            "finish_reason": "stop",
            "text": "world"
        }))
        .unwrap();
    let stderr_path = paths.write_stderr_log("loaded model\n").unwrap();

    assert_eq!(request_path, paths.request_json());
    assert_eq!(response_path, paths.response_json());
    assert_eq!(stderr_path, paths.stderr_log());
    assert_eq!(
        std::fs::read_to_string(paths.request_json()).unwrap(),
        "{\n  \"prompt\": \"hello\",\n  \"temperature\": 0\n}\n"
    );
    assert_eq!(
        std::fs::read_to_string(paths.response_json()).unwrap(),
        "{\n  \"finish_reason\": \"stop\",\n  \"text\": \"world\"\n}\n"
    );
    assert_eq!(
        std::fs::read_to_string(paths.stderr_log()).unwrap(),
        "loaded model\n"
    );

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
fn appends_observation_events_as_jsonl() {
    let root_path = temp_root("events");
    let root = InferenceArtifactRoot::new(&root_path);
    let run_id = run_id();
    let attempt_id = attempt_id();
    let paths = root.paths(&run_id, &attempt_id);
    let writer = InferenceEventWriter::new(paths.events_jsonl());

    let events = [
        InferenceObservationEvent::AttemptStarted {
            run_id: run_id.clone(),
            attempt_id: attempt_id.clone(),
            backend: "fake-backend".to_string(),
            request_path: paths.request_json().to_path_buf(),
        },
        InferenceObservationEvent::RequestRecorded {
            run_id: run_id.clone(),
            attempt_id: attempt_id.clone(),
            path: paths.request_json().to_path_buf(),
        },
        InferenceObservationEvent::ResponseRecorded {
            run_id: run_id.clone(),
            attempt_id: attempt_id.clone(),
            path: paths.response_json().to_path_buf(),
        },
        InferenceObservationEvent::AttemptFinished {
            run_id,
            attempt_id,
            finish_status: InferenceFinishStatus::Succeeded,
        },
    ];

    for event in &events {
        writer.append(event).unwrap();
    }

    let request_path_json = serde_json::to_string(paths.request_json()).unwrap();
    let response_path_json = serde_json::to_string(paths.response_json()).unwrap();
    let expected = format!(
        concat!(
            "{{\"kind\":\"inference.attempt_started\",\"run_id\":\"run-001\",",
            "\"attempt_id\":\"attempt-001\",\"backend\":\"fake-backend\",",
            "\"request_path\":{request_path_json}}}\n",
            "{{\"kind\":\"inference.request_recorded\",\"run_id\":\"run-001\",",
            "\"attempt_id\":\"attempt-001\",\"path\":{request_path_json}}}\n",
            "{{\"kind\":\"inference.response_recorded\",\"run_id\":\"run-001\",",
            "\"attempt_id\":\"attempt-001\",\"path\":{response_path_json}}}\n",
            "{{\"kind\":\"inference.attempt_finished\",\"run_id\":\"run-001\",",
            "\"attempt_id\":\"attempt-001\",\"finish_status\":\"succeeded\"}}\n"
        ),
        request_path_json = request_path_json,
        response_path_json = response_path_json
    );

    assert_eq!(
        std::fs::read_to_string(paths.events_jsonl()).unwrap(),
        expected
    );

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
fn serializes_every_observation_event_as_single_line_json() {
    let run_id = run_id();
    let attempt_id = attempt_id();
    let path = PathBuf::from("/tmp/agl-observe/request.json");
    let events = [
        InferenceObservationEvent::AttemptStarted {
            run_id: run_id.clone(),
            attempt_id: attempt_id.clone(),
            backend: "local-qwen".to_string(),
            request_path: path.clone(),
        },
        InferenceObservationEvent::RequestRecorded {
            run_id: run_id.clone(),
            attempt_id: attempt_id.clone(),
            path: path.clone(),
        },
        InferenceObservationEvent::ResponseRecorded {
            run_id: run_id.clone(),
            attempt_id: attempt_id.clone(),
            path: PathBuf::from("/tmp/agl-observe/response.json"),
        },
        InferenceObservationEvent::AttemptFinished {
            run_id: run_id.clone(),
            attempt_id: attempt_id.clone(),
            finish_status: InferenceFinishStatus::Failed,
        },
        InferenceObservationEvent::AttemptFailed {
            run_id,
            attempt_id,
            message: "model process exited".to_string(),
        },
    ];

    for event in events {
        let line = event.to_jsonl_line().unwrap();
        assert!(!line.contains('\n'), "{line}");
        let decoded: InferenceObservationEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(decoded, event);
    }
}

#[test]
fn ids_reject_values_that_are_not_path_segments() {
    assert!(InferenceRunId::new("").is_err());
    assert!(InferenceRunId::new(".").is_err());
    assert!(InferenceRunId::new("..").is_err());
    assert!(InferenceRunId::new("../run").is_err());
    assert!(InferenceAttemptId::new("attempt/001").is_err());
    assert!(InferenceAttemptId::new("attempt 001").is_err());
}
