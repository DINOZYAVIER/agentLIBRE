use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::fsm::{ChatSessionMachine, ChatSessionPhase, ChatSessionTransition};
use crate::*;

static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);
const TEST_SESSION_ID: &str = "session-001";
const TEST_RUN_ID: &str = "run-001";
const NEXT_RUN_ID: &str = "run-002";
const TEST_CONFIG_PATH: &str = "/tmp/local.toml";
const TEST_BACKEND: &str = "llama_cpp";

fn temp_root(name: &str) -> PathBuf {
    let id = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("agl-session-{name}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn test_session_id() -> AgentLibreSessionId {
    AgentLibreSessionId::new(TEST_SESSION_ID).unwrap()
}

fn start_test_session(root: impl AsRef<std::path::Path>) -> ChatSessionStore {
    start_session(root, test_session_id(), TEST_RUN_ID)
}

fn start_session(
    root: impl AsRef<std::path::Path>,
    session_id: AgentLibreSessionId,
    run_id: &str,
) -> ChatSessionStore {
    ChatSessionStore::start(root, session_id, run_id, TEST_CONFIG_PATH, TEST_BACKEND).unwrap()
}

#[test]
fn generated_session_ids_are_unique_path_segments() {
    let first = AgentLibreSessionId::generate();
    let second = AgentLibreSessionId::generate();

    assert_ne!(first, second);
    AgentLibreSessionId::new(first.as_str()).unwrap();
    AgentLibreSessionId::new(second.as_str()).unwrap();
}

#[test]
fn chat_session_machine_accepts_answer_turn_path() {
    let session_id = test_session_id();
    let mut machine = ChatSessionMachine::new(session_id);

    assert_eq!(
        machine
            .apply(ChatSessionTransition::StartNewSession {
                run_id: TEST_RUN_ID.to_string(),
            })
            .unwrap()
            .to,
        ChatSessionPhase::Started
    );
    assert_eq!(
        machine
            .apply(ChatSessionTransition::PromptForInput)
            .unwrap()
            .to,
        ChatSessionPhase::AwaitingInput
    );
    assert_eq!(
        machine
            .apply(ChatSessionTransition::ReadUserMessage {
                content: "hello".to_string(),
            })
            .unwrap()
            .to,
        ChatSessionPhase::RecordingUserMessage
    );
    assert_eq!(
        machine
            .apply(ChatSessionTransition::RecordUserMessage {
                message_id: AgentLibreMessageId::indexed(1),
                content: "hello".to_string(),
            })
            .unwrap()
            .to,
        ChatSessionPhase::RunningTurn
    );
    assert_eq!(
        machine
            .apply(ChatSessionTransition::LinkModelAttempt {
                run_id: TEST_RUN_ID.to_string(),
                attempt_id: "attempt-0001".to_string(),
            })
            .unwrap()
            .to,
        ChatSessionPhase::RunningTurn
    );
    assert_eq!(
        machine
            .apply(ChatSessionTransition::RecordAssistantAnswer {
                message_id: AgentLibreMessageId::indexed(2),
                content: "hi".to_string(),
            })
            .unwrap()
            .to,
        ChatSessionPhase::RecordingAssistantMessage
    );
    assert_eq!(
        machine
            .apply(ChatSessionTransition::PromptForInput)
            .unwrap()
            .to,
        ChatSessionPhase::AwaitingInput
    );
}

#[test]
fn chat_session_machine_rejects_illegal_transition_and_finished_is_terminal() {
    let session_id = test_session_id();
    let mut machine = ChatSessionMachine::new(session_id);

    let err = machine
        .apply(ChatSessionTransition::RecordAssistantAnswer {
            message_id: AgentLibreMessageId::indexed(1),
            content: "hi".to_string(),
        })
        .unwrap_err();
    assert_eq!(err.phase, ChatSessionPhase::Uninitialized);
    assert_eq!(err.transition, "record_assistant_answer");

    machine
        .apply(ChatSessionTransition::StartNewSession {
            run_id: TEST_RUN_ID.to_string(),
        })
        .unwrap();
    machine
        .apply(ChatSessionTransition::PromptForInput)
        .unwrap();
    machine
        .apply(ChatSessionTransition::FinishSession {
            reason: AgentLibreSessionFinishReason::Eof,
        })
        .unwrap();
    let err = machine
        .apply(ChatSessionTransition::PromptForInput)
        .unwrap_err();
    assert_eq!(err.phase, ChatSessionPhase::Finished);
}

#[test]
fn writes_chat_session_metadata_and_transcript_from_transitions() {
    let root = temp_root("session");
    let mut store = start_test_session(&root);

    store
        .append_user_message(AgentLibreMessageId::indexed(1), "hello".to_string())
        .unwrap();
    store.link_attempt("attempt-0001").unwrap();
    store
        .append_assistant_message(AgentLibreMessageId::indexed(2), "hi".to_string())
        .unwrap();
    store.append_context_cleared().unwrap();
    store.finish_eof().unwrap();

    assert!(store.session_dir().join("session.json").exists());
    let metadata = std::fs::read_to_string(store.session_dir().join("session.json")).unwrap();
    assert!(metadata.contains("\"local_inference_config_path\""));
    assert!(!metadata.contains("\"model_config_path\""));
    let transcript = std::fs::read_to_string(store.transcript_jsonl()).unwrap();
    assert!(transcript.contains("\"kind\":\"session_started\""));
    assert!(transcript.contains("\"kind\":\"user_message\""));
    assert!(transcript.contains("\"kind\":\"assistant_message\""));
    assert!(transcript.contains("\"kind\":\"model_attempt_linked\""));
    assert!(transcript.contains("\"kind\":\"context_cleared\""));
    assert!(transcript.contains("\"kind\":\"session_finished\""));
    assert!(transcript.contains("\"reason\":\"eof\""));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn stopped_turn_marker_is_recorded_as_assistant_message() {
    let root = temp_root("stopped-marker");
    let mut store = start_test_session(&root);

    store
        .append_user_message(AgentLibreMessageId::indexed(1), "tool bait".to_string())
        .unwrap();
    store.link_attempt("attempt-0001").unwrap();
    store
        .append_assistant_stop_marker(
            AgentLibreMessageId::indexed(2),
            "The previous turn stopped.".to_string(),
        )
        .unwrap();

    let replay = store.read_replay().unwrap();

    assert!(matches!(
        replay.events[2],
        ChatSessionEvent::ModelAttemptLinked { .. }
    ));
    assert!(matches!(
        replay.events[3],
        ChatSessionEvent::AssistantMessage { .. }
    ));
    assert_eq!(replay.next_message_index, 3);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn tool_messages_are_recorded_and_replayed() {
    let root = temp_root("tool-message");
    let mut store = start_test_session(&root);

    store
        .append_user_message(AgentLibreMessageId::indexed(1), "read".to_string())
        .unwrap();
    store.link_attempt("attempt-0001").unwrap();
    store
        .append_assistant_tool_call(
            AgentLibreMessageId::indexed(2),
            "read_file".to_string(),
            serde_json::json!({"path": "README.MD"}),
        )
        .unwrap();
    store
        .append_tool_message(
            AgentLibreMessageId::indexed(3),
            "read_file".to_string(),
            "file content".to_string(),
        )
        .unwrap();
    store
        .append_assistant_message(AgentLibreMessageId::indexed(4), "done".to_string())
        .unwrap();

    let replay = store.read_replay().unwrap();

    assert!(matches!(
        replay.events[3],
        ChatSessionEvent::AssistantToolCall { .. }
    ));
    assert!(matches!(
        replay.events[4],
        ChatSessionEvent::ToolMessage { .. }
    ));
    assert_eq!(replay.next_message_index, 5);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn session_failures_are_recorded() {
    let root = temp_root("session-failure");
    let mut store = start_test_session(&root);

    store
        .append_user_message(AgentLibreMessageId::indexed(1), "hello".to_string())
        .unwrap();
    store.fail("model request failed").unwrap();

    assert_eq!(store.machine().phase(), ChatSessionPhase::Failed);
    let transcript = std::fs::read_to_string(store.transcript_jsonl()).unwrap();
    assert!(transcript.contains("\"kind\":\"session_failed\""));
    assert!(transcript.contains("model request failed"));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn exit_command_finishes_session_with_reason() {
    let root = temp_root("exit-command");
    let mut store = start_test_session(&root);

    store.request_exit().unwrap();

    assert_eq!(store.machine().phase(), ChatSessionPhase::Finished);
    let transcript = std::fs::read_to_string(store.transcript_jsonl()).unwrap();
    assert!(transcript.contains("\"kind\":\"session_finished\""));
    assert!(transcript.contains("\"reason\":\"exit_command\""));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn session_finished_without_reason_is_rejected() {
    let session_id = test_session_id();
    let line = serde_json::json!({
        "kind": "session_finished",
        "session_id": session_id,
    })
    .to_string();

    let err = serde_json::from_str::<ChatSessionEvent>(&line).unwrap_err();

    assert!(err.to_string().contains("missing field `reason`"));
}

#[test]
fn session_metadata_model_config_path_is_rejected() {
    let err = serde_json::from_value::<SessionMetadata>(serde_json::json!({
        "session_id": "session-001",
        "created_at_unix_ms": 1,
        "updated_at_unix_ms": 2,
        "model_config_path": "/tmp/local.toml",
        "backend": "llama_cpp"
    }))
    .unwrap_err();

    assert!(err.to_string().contains("local_inference_config_path"));
}

#[test]
fn start_refuses_existing_chat_session() {
    let root = temp_root("session-collision");
    let session_id = test_session_id();
    let _store = start_session(&root, session_id.clone(), TEST_RUN_ID);

    let err = ChatSessionStore::start(
        &root,
        session_id,
        NEXT_RUN_ID,
        TEST_CONFIG_PATH,
        TEST_BACKEND,
    )
    .unwrap_err();

    assert!(format!("{err:#}").contains("chat session already exists"));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn start_allows_precreated_artifact_run_directory() {
    let root = temp_root("session-artifact-precreate");
    let session_id = test_session_id();
    std::fs::create_dir_all(
        root.join(session_id.as_str())
            .join("runs")
            .join(TEST_RUN_ID),
    )
    .unwrap();

    let store = ChatSessionStore::start(
        &root,
        session_id,
        TEST_RUN_ID,
        TEST_CONFIG_PATH,
        TEST_BACKEND,
    )
    .unwrap();

    assert!(store.session_dir().join("session.json").exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn opens_existing_session_and_reads_replay_without_appending_start() {
    let root = temp_root("session-replay");
    let session_id = test_session_id();
    let mut store = start_session(&root, session_id.clone(), TEST_RUN_ID);
    store
        .append_user_message(AgentLibreMessageId::indexed(1), "hello".to_string())
        .unwrap();
    store.link_attempt("attempt-0001").unwrap();
    store
        .append_assistant_message(AgentLibreMessageId::indexed(2), "hi".to_string())
        .unwrap();
    let before = std::fs::read_to_string(store.transcript_jsonl()).unwrap();

    let opened = ChatSessionStore::open(&root, session_id, NEXT_RUN_ID).unwrap();
    let replay = opened.read_replay().unwrap();
    let after = std::fs::read_to_string(opened.transcript_jsonl()).unwrap();

    assert_eq!(after, before);
    assert_eq!(replay.events.len(), 4);
    assert_eq!(replay.next_message_index, 3);
    assert_eq!(replay.next_attempt_index, 2);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_transcript_reports_line_number() {
    let root = temp_root("session-malformed");
    let session_id = test_session_id();
    let store = start_session(&root, session_id.clone(), TEST_RUN_ID);
    let mut transcript = std::fs::read_to_string(store.transcript_jsonl()).unwrap();
    transcript.push_str("not-json\n");
    std::fs::write(store.transcript_jsonl(), transcript).unwrap();
    let opened = ChatSessionStore::open(&root, session_id, NEXT_RUN_ID).unwrap();

    let err = opened.read_replay().unwrap_err();

    assert!(format!("{err:#}").contains("line 2"));

    std::fs::remove_dir_all(root).unwrap();
}
