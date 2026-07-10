use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agl_events::{SafeRuntimeEvent, SafeRuntimeEventEnvelope};
use agl_ids::{AttemptId, RequestId, RunId, SessionId, TurnId};
use serde_json::json;

use crate::{InferenceAttemptMachine, InferenceAttemptTransition};

use super::*;

static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);
const TEST_RUN_ID: &str = "run_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b31";
const TEST_TURN_ID: &str = "turn_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b32";
const TEST_ATTEMPT_ID: &str = "attempt_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b33";
const TEST_SESSION_ID: &str = "ses_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b34";
const TEST_REQUEST_ID: &str = "req_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b35";

fn temp_root(name: &str) -> PathBuf {
    let id = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "agl-inference-evidence-{name}-{}-{id}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn run_id() -> RunId {
    RunId::parse(TEST_RUN_ID).unwrap()
}

fn turn_id() -> TurnId {
    TurnId::parse(TEST_TURN_ID).unwrap()
}

fn attempt_id() -> AttemptId {
    AttemptId::parse(TEST_ATTEMPT_ID).unwrap()
}

fn session_id() -> SessionId {
    SessionId::parse(TEST_SESSION_ID).unwrap()
}

fn request_id() -> RequestId {
    RequestId::parse(TEST_REQUEST_ID).unwrap()
}

fn append_transition(
    writer: &InferenceEventWriter,
    machine: &mut InferenceAttemptMachine,
    transition: InferenceAttemptTransition,
) {
    let record = machine.apply(transition).unwrap();
    writer.append_transition(&record).unwrap();
}

#[test]
fn builds_typed_artifact_paths_under_the_run_directory() {
    let root_path = temp_root("paths");
    let root = InferenceArtifactRoot::new(&root_path);
    let paths = root.paths(&run_id(), &attempt_id());

    assert_eq!(root.root(), root_path.as_path());
    assert_eq!(
        paths.run_dir(),
        root_path.join("runs").join(TEST_RUN_ID).as_path()
    );
    assert_eq!(paths.events_jsonl(), paths.run_dir().join("events.jsonl"));
    assert_eq!(
        paths.attempt_dir(),
        paths.run_dir().join("attempts").join(TEST_ATTEMPT_ID)
    );
    assert_eq!(
        paths.request_json(),
        paths.attempt_dir().join("request.json")
    );
    assert_eq!(
        paths.response_json(),
        paths.attempt_dir().join("response.json")
    );
    assert_eq!(paths.runtime_log(), paths.attempt_dir().join("runtime.log"));
}

#[test]
fn writes_request_response_and_runtime_artifacts() {
    let root_path = temp_root("artifacts");
    let paths = InferenceArtifactRoot::new(&root_path).paths(&run_id(), &attempt_id());

    paths
        .write_request_json(&json!({"prompt": "hello", "temperature": 0}))
        .unwrap();
    paths
        .write_response_json(&json!({"finish_reason": "stop", "text": "world"}))
        .unwrap();
    paths.write_runtime_log("loaded model\n").unwrap();

    assert_eq!(
        std::fs::read_to_string(paths.request_json()).unwrap(),
        "{\n  \"prompt\": \"hello\",\n  \"temperature\": 0\n}\n"
    );
    assert_eq!(
        std::fs::read_to_string(paths.response_json()).unwrap(),
        "{\n  \"finish_reason\": \"stop\",\n  \"text\": \"world\"\n}\n"
    );
    assert_eq!(
        std::fs::read_to_string(paths.runtime_log()).unwrap(),
        "loaded model\n"
    );

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
fn observation_events_preserve_full_request_correlation() {
    let root_path = temp_root("events");
    let root = InferenceArtifactRoot::new(&root_path);
    let paths = root.paths(&run_id(), &attempt_id());
    let writer =
        InferenceEventWriter::open(paths.events_jsonl(), Some(session_id()), Some(request_id()))
            .unwrap();
    let mut machine = InferenceAttemptMachine::new(run_id(), turn_id(), attempt_id());

    append_transition(
        &writer,
        &mut machine,
        InferenceAttemptTransition::StartAttempt {
            backend: "fake-backend".to_string(),
            request_path: paths.request_json().to_path_buf(),
        },
    );
    append_transition(
        &writer,
        &mut machine,
        InferenceAttemptTransition::RecordRequest {
            path: paths.request_json().to_path_buf(),
        },
    );
    append_transition(
        &writer,
        &mut machine,
        InferenceAttemptTransition::StartRuntime,
    );
    append_transition(
        &writer,
        &mut machine,
        InferenceAttemptTransition::RecordRuntimeLog {
            path: paths.runtime_log().to_path_buf(),
        },
    );
    append_transition(
        &writer,
        &mut machine,
        InferenceAttemptTransition::RecordResponse {
            path: paths.response_json().to_path_buf(),
        },
    );
    append_transition(
        &writer,
        &mut machine,
        InferenceAttemptTransition::FinishAttempt {
            status: InferenceFinishStatus::Succeeded,
        },
    );

    let events = std::fs::read_to_string(paths.events_jsonl())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<SafeRuntimeEventEnvelope>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 4);
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.sequence, index as u64 + 1);
        assert_eq!(event.scope.run_id(), &run_id());
        assert_eq!(event.scope.session_id(), Some(&session_id()));
        assert_eq!(event.scope.turn_id(), Some(&turn_id()));
        assert_eq!(event.scope.attempt_id(), Some(&attempt_id()));
        assert_eq!(event.request_id.as_ref(), Some(&request_id()));
    }
    assert!(matches!(
        events[0].payload,
        SafeRuntimeEvent::InferenceAttemptStarted { .. }
    ));
    assert!(matches!(
        events[1].payload,
        SafeRuntimeEvent::InferenceRequestRecorded { .. }
    ));
    assert!(matches!(
        events[2].payload,
        SafeRuntimeEvent::InferenceResponseRecorded { .. }
    ));
    assert!(matches!(
        events[3].payload,
        SafeRuntimeEvent::InferenceAttemptFinished { .. }
    ));

    std::fs::remove_dir_all(root_path).unwrap();
}

#[test]
fn typed_ids_keep_runs_and_attempts_in_separate_directories() {
    let root_path = temp_root("separate-scopes");
    let root = InferenceArtifactRoot::new(&root_path);
    let other_run = RunId::generate();
    let other_attempt = AttemptId::generate();

    assert_ne!(
        root.paths(&run_id(), &attempt_id()).attempt_dir(),
        root.paths(&other_run, &other_attempt).attempt_dir()
    );
}
