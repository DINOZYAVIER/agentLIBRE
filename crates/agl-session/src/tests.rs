use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agl_events::{EVENT_SCHEMA, EventEnvelope, EventScope, RuntimeEvent, RuntimeEventEnvelope};
use agl_ids::{AttemptId, EventId, MessageId, RunId, SessionId, TurnId};

use crate::fsm::{ChatSessionMachine, ChatSessionPhase, ChatSessionTransition};
use crate::*;

static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);
const TEST_SESSION_ID: &str = "ses_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b30";
const TEST_RUN_ID: &str = "run_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b31";
const NEXT_RUN_ID: &str = "run_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b32";
const TEST_TURN_ID: &str = "turn_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b33";
const NEXT_TURN_ID: &str = "turn_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b34";
const TEST_ATTEMPT_ID: &str = "attempt_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b35";
const NEXT_ATTEMPT_ID: &str = "attempt_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b36";
const TEST_CONFIG_PATH: &str = "/tmp/local.toml";
const TEST_BACKEND: &str = "llama_cpp";

fn temp_root(name: &str) -> PathBuf {
    let id = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("agl-session-{name}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn session_id() -> SessionId {
    SessionId::parse(TEST_SESSION_ID).unwrap()
}

fn run_id() -> RunId {
    RunId::parse(TEST_RUN_ID).unwrap()
}

fn next_run_id() -> RunId {
    RunId::parse(NEXT_RUN_ID).unwrap()
}

fn turn_id() -> TurnId {
    TurnId::parse(TEST_TURN_ID).unwrap()
}

fn next_turn_id() -> TurnId {
    TurnId::parse(NEXT_TURN_ID).unwrap()
}

fn attempt_id() -> AttemptId {
    AttemptId::parse(TEST_ATTEMPT_ID).unwrap()
}

fn next_attempt_id() -> AttemptId {
    AttemptId::parse(NEXT_ATTEMPT_ID).unwrap()
}

fn message_id(last_hex: char) -> MessageId {
    MessageId::parse(&format!(
        "msg_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b3{last_hex}"
    ))
    .unwrap()
}

fn event_id(last_hex: char) -> EventId {
    EventId::parse(&format!(
        "evt_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b3{last_hex}"
    ))
    .unwrap()
}

fn runtime_envelope(
    session_id: SessionId,
    run_id: RunId,
    turn_id: TurnId,
    sequence: u64,
    event_suffix: char,
    attempt_id: Option<AttemptId>,
    payload: RuntimeEvent,
) -> RuntimeEventEnvelope {
    let mut scope = EventScope::builder(run_id)
        .session_id(session_id)
        .turn_id(turn_id);
    if let Some(attempt_id) = attempt_id {
        scope = scope.attempt_id(attempt_id);
    }
    EventEnvelope {
        schema: EVENT_SCHEMA.to_string(),
        event_id: event_id(event_suffix),
        sequence,
        occurred_at_unix_ms: 1_700_000_000_000 + sequence,
        scope: scope.build().unwrap(),
        request_id: None,
        caused_by: None,
        payload,
    }
}

fn user_envelope(
    run_id: RunId,
    turn_id: TurnId,
    sequence: u64,
    event_suffix: char,
    message_suffix: char,
    content: &str,
) -> RuntimeEventEnvelope {
    runtime_envelope(
        session_id(),
        run_id,
        turn_id,
        sequence,
        event_suffix,
        None,
        RuntimeEvent::UserMessage {
            message_id: message_id(message_suffix),
            content: content.to_string(),
        },
    )
}

fn assistant_envelope(
    run_id: RunId,
    turn_id: TurnId,
    sequence: u64,
    event_suffix: char,
    message_suffix: char,
    content: &str,
) -> RuntimeEventEnvelope {
    runtime_envelope(
        session_id(),
        run_id,
        turn_id,
        sequence,
        event_suffix,
        None,
        RuntimeEvent::AssistantMessage {
            message_id: message_id(message_suffix),
            content: content.to_string(),
        },
    )
}

fn start_session(root: impl AsRef<std::path::Path>, session_id: SessionId) -> ChatSessionStore {
    ChatSessionStore::start(root, session_id, TEST_CONFIG_PATH, TEST_BACKEND).unwrap()
}

fn start_test_session(root: impl AsRef<std::path::Path>) -> ChatSessionStore {
    start_session(root, session_id())
}

#[test]
fn chat_session_machine_accepts_answer_turn_path() {
    let mut machine = ChatSessionMachine::new(session_id());

    assert_eq!(
        machine
            .apply(ChatSessionTransition::StartNewSession)
            .unwrap()
            .to,
        ChatSessionPhase::Started
    );
    machine
        .apply(ChatSessionTransition::PromptForInput)
        .unwrap();
    machine
        .apply(ChatSessionTransition::ReadUserMessage {
            content: "hello".to_string(),
        })
        .unwrap();
    machine
        .apply(ChatSessionTransition::RecordUserMessage {
            run_id: run_id(),
            turn_id: turn_id(),
            message_id: message_id('7'),
            content: "hello".to_string(),
        })
        .unwrap();
    machine
        .apply(ChatSessionTransition::LinkModelAttempt {
            run_id: run_id(),
            turn_id: turn_id(),
            attempt_id: attempt_id(),
        })
        .unwrap();
    assert_eq!(
        machine
            .apply(ChatSessionTransition::RecordAssistantAnswer {
                run_id: run_id(),
                turn_id: turn_id(),
                message_id: message_id('8'),
                content: "hi".to_string(),
            })
            .unwrap()
            .to,
        ChatSessionPhase::RecordingAssistantMessage
    );
}

#[test]
fn chat_session_machine_rejects_illegal_transition_and_finished_is_terminal() {
    let mut machine = ChatSessionMachine::new(session_id());
    let err = machine
        .apply(ChatSessionTransition::RecordAssistantAnswer {
            run_id: run_id(),
            turn_id: turn_id(),
            message_id: message_id('7'),
            content: "hi".to_string(),
        })
        .unwrap_err();
    assert_eq!(err.phase, ChatSessionPhase::Uninitialized);

    machine
        .apply(ChatSessionTransition::StartNewSession)
        .unwrap();
    machine
        .apply(ChatSessionTransition::PromptForInput)
        .unwrap();
    machine
        .apply(ChatSessionTransition::FinishSession {
            reason: AgentLibreSessionFinishReason::Eof,
        })
        .unwrap();
    assert!(
        machine
            .apply(ChatSessionTransition::PromptForInput)
            .is_err()
    );
}

#[test]
fn two_turn_replay_preserves_distinct_run_and_turn_correlations() {
    let root = temp_root("two-turns");
    let mut store = start_test_session(&root);

    store
        .append_user_message(user_envelope(run_id(), turn_id(), 1, '0', '7', "one"))
        .unwrap();
    store
        .link_attempt(runtime_envelope(
            session_id(),
            run_id(),
            turn_id(),
            4,
            '1',
            Some(attempt_id()),
            RuntimeEvent::ModelAttemptLinked,
        ))
        .unwrap();
    store
        .append_assistant_message(assistant_envelope(
            run_id(),
            turn_id(),
            9,
            '2',
            '8',
            "first",
        ))
        .unwrap();
    store
        .append_user_message(user_envelope(
            next_run_id(),
            next_turn_id(),
            2,
            '3',
            '9',
            "two",
        ))
        .unwrap();
    store
        .link_attempt(runtime_envelope(
            session_id(),
            next_run_id(),
            next_turn_id(),
            6,
            '4',
            Some(next_attempt_id()),
            RuntimeEvent::ModelAttemptLinked,
        ))
        .unwrap();
    store
        .append_assistant_message(assistant_envelope(
            next_run_id(),
            next_turn_id(),
            11,
            '5',
            'a',
            "second",
        ))
        .unwrap();

    let replay = store.read_replay().unwrap();
    assert_eq!(replay.events.len(), 7);
    let first = runtime_event(&replay.events[1]);
    assert_eq!(first.scope.run_id(), &run_id());
    assert_eq!(first.scope.turn_id(), Some(&turn_id()));
    assert_eq!(first.sequence, 1);

    let second = runtime_event(&replay.events[4]);
    assert_eq!(second.scope.run_id(), &next_run_id());
    assert_eq!(second.scope.turn_id(), Some(&next_turn_id()));
    assert_eq!(second.sequence, 2);

    let linked = runtime_event(&replay.events[5]);
    assert_eq!(linked.scope.run_id(), &next_run_id());
    assert_eq!(linked.scope.turn_id(), Some(&next_turn_id()));
    assert_eq!(linked.scope.attempt_id(), Some(&next_attempt_id()));
    assert_eq!(linked.sequence, 6);

    std::fs::remove_dir_all(root).unwrap();
}

fn runtime_event(event: &ChatSessionEvent) -> &RuntimeEventEnvelope {
    let ChatSessionEvent::Runtime { envelope } = event else {
        panic!("expected runtime transcript envelope, got {event:?}");
    };
    envelope
}

#[test]
fn tool_messages_and_session_lifecycle_are_recorded() {
    let root = temp_root("tool-message");
    let mut store = start_test_session(&root);

    store
        .append_user_message(user_envelope(run_id(), turn_id(), 1, '0', '7', "read"))
        .unwrap();
    store
        .append_assistant_tool_call(runtime_envelope(
            session_id(),
            run_id(),
            turn_id(),
            3,
            '1',
            None,
            RuntimeEvent::AssistantToolCall {
                message_id: message_id('8'),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "README.MD"}),
            },
        ))
        .unwrap();
    store
        .append_tool_message(runtime_envelope(
            session_id(),
            run_id(),
            turn_id(),
            5,
            '2',
            None,
            RuntimeEvent::ToolMessage {
                message_id: message_id('9'),
                name: "read_file".to_string(),
                data: serde_json::json!({"text": "file content"}),
            },
        ))
        .unwrap();
    store
        .append_assistant_stop_marker(assistant_envelope(
            run_id(),
            turn_id(),
            8,
            '3',
            'a',
            "stopped",
        ))
        .unwrap();
    store.append_context_cleared().unwrap();
    store.finish_eof().unwrap();

    let replay = store.read_replay().unwrap();
    assert!(matches!(
        runtime_event(&replay.events[2]).payload,
        RuntimeEvent::AssistantToolCall { .. }
    ));
    assert!(matches!(
        runtime_event(&replay.events[3]).payload,
        RuntimeEvent::ToolMessage { .. }
    ));
    assert!(matches!(
        runtime_event(&replay.events[4]).payload,
        RuntimeEvent::AssistantMessage { .. }
    ));
    assert!(matches!(
        replay.events[5],
        ChatSessionEvent::ContextCleared { .. }
    ));
    assert!(matches!(
        replay.events[6],
        ChatSessionEvent::SessionFinished { .. }
    ));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn session_failures_and_exit_reason_are_recorded() {
    let failure_root = temp_root("session-failure");
    let mut failed = start_test_session(&failure_root);
    failed
        .append_user_message(user_envelope(run_id(), turn_id(), 1, '0', '7', "hello"))
        .unwrap();
    failed.fail("model request failed").unwrap();
    assert_eq!(failed.machine().phase(), ChatSessionPhase::Failed);

    let exit_root = temp_root("exit-command");
    let mut exited = start_test_session(&exit_root);
    exited.request_exit().unwrap();
    let transcript = std::fs::read_to_string(exited.transcript_jsonl()).unwrap();
    assert!(transcript.contains("\"reason\":\"exit_command\""));

    std::fs::remove_dir_all(failure_root).unwrap();
    std::fs::remove_dir_all(exit_root).unwrap();
}

#[test]
fn previous_transcript_shapes_are_rejected_strictly() {
    let old_start = serde_json::json!({
        "kind": "session_started",
        "session_id": TEST_SESSION_ID,
        "run_id": TEST_RUN_ID,
    });
    assert!(serde_json::from_value::<ChatSessionEvent>(old_start).is_err());

    let old_message = serde_json::json!({
        "kind": "user_message",
        "session_id": TEST_SESSION_ID,
        "message_id": "message-0001",
        "content": "hello",
    });
    assert!(serde_json::from_value::<ChatSessionEvent>(old_message).is_err());

    let missing_reason = serde_json::json!({
        "kind": "session_finished",
        "session_id": TEST_SESSION_ID,
    });
    assert!(serde_json::from_value::<ChatSessionEvent>(missing_reason).is_err());
}

#[test]
fn replay_accepts_monotonic_runtime_sequence_gaps() {
    let root = temp_root("sequence-gaps");
    let id = session_id();
    let mut store = start_session(&root, id.clone());
    store
        .append_user_message(user_envelope(run_id(), turn_id(), 2, '0', '7', "hello"))
        .unwrap();
    store
        .append_assistant_message(assistant_envelope(run_id(), turn_id(), 10, '1', '8', "hi"))
        .unwrap();

    let replay = ChatSessionStore::open(&root, id)
        .unwrap()
        .read_replay()
        .unwrap();
    assert_eq!(runtime_event(&replay.events[1]).sequence, 2);
    assert_eq!(runtime_event(&replay.events[2]).sequence, 10);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn replay_rejects_runtime_envelope_from_another_session() {
    let root = temp_root("session-drift");
    let id = session_id();
    let store = start_session(&root, id.clone());
    let foreign = ChatSessionEvent::Runtime {
        envelope: Box::new(runtime_envelope(
            SessionId::generate(),
            run_id(),
            turn_id(),
            1,
            '0',
            None,
            RuntimeEvent::UserMessage {
                message_id: message_id('7'),
                content: "foreign".to_string(),
            },
        )),
    };
    let mut transcript = std::fs::read_to_string(store.transcript_jsonl()).unwrap();
    transcript.push_str(&serde_json::to_string(&foreign).unwrap());
    transcript.push('\n');
    std::fs::write(store.transcript_jsonl(), transcript).unwrap();

    let error = ChatSessionStore::open(&root, id).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("line 2"), "{message}");
    assert!(message.contains("different session"), "{message}");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn open_rejects_metadata_identity_drift_and_unknown_fields() {
    let drift_root = temp_root("metadata-drift");
    let id = session_id();
    let store = start_session(&drift_root, id.clone());
    let metadata_path = store.session_dir().join("session.json");
    let mut metadata: SessionMetadata =
        serde_json::from_slice(&std::fs::read(&metadata_path).unwrap()).unwrap();
    metadata.session_id = SessionId::generate();
    std::fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();

    let error = ChatSessionStore::open(&drift_root, id).unwrap_err();
    assert!(
        format!("{error:#}").contains("does not match requested session"),
        "{error:#}"
    );

    let old_root = temp_root("metadata-old-field");
    let id = session_id();
    let store = start_session(&old_root, id.clone());
    let metadata_path = store.session_dir().join("session.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&metadata_path).unwrap()).unwrap();
    metadata["run_id"] = serde_json::json!(TEST_RUN_ID);
    std::fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();

    let error = ChatSessionStore::open(&old_root, id).unwrap_err();
    assert!(format!("{error:#}").contains("unknown field"), "{error:#}");

    std::fs::remove_dir_all(drift_root).unwrap();
    std::fs::remove_dir_all(old_root).unwrap();
}

#[test]
fn start_refuses_existing_session_but_allows_precreated_run_directory() {
    let root = temp_root("session-collision");
    let id = session_id();
    std::fs::create_dir_all(root.join(id.as_str()).join("runs").join(TEST_RUN_ID)).unwrap();
    let _store = start_session(&root, id.clone());

    let err = ChatSessionStore::start(&root, id, TEST_CONFIG_PATH, TEST_BACKEND).unwrap_err();
    assert!(format!("{err:#}").contains("chat session already exists"));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn open_reads_replay_without_appending_session_start() {
    let root = temp_root("session-replay");
    let id = session_id();
    let mut store = start_session(&root, id.clone());
    store
        .append_user_message(user_envelope(run_id(), turn_id(), 1, '0', '7', "hello"))
        .unwrap();
    store
        .append_assistant_message(assistant_envelope(run_id(), turn_id(), 7, '1', '8', "hi"))
        .unwrap();
    let before = std::fs::read_to_string(store.transcript_jsonl()).unwrap();

    let opened = ChatSessionStore::open(&root, id).unwrap();
    let replay = opened.read_replay().unwrap();
    let after = std::fs::read_to_string(opened.transcript_jsonl()).unwrap();

    assert_eq!(after, before);
    assert_eq!(replay.events.len(), 3);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_transcript_reports_line_number() {
    let root = temp_root("session-malformed");
    let id = session_id();
    let store = start_session(&root, id.clone());
    let mut transcript = std::fs::read_to_string(store.transcript_jsonl()).unwrap();
    transcript.push_str("not-json\n");
    std::fs::write(store.transcript_jsonl(), transcript).unwrap();

    let err = ChatSessionStore::open(&root, id).unwrap_err();
    assert!(format!("{err:#}").contains("line 2"));

    std::fs::remove_dir_all(root).unwrap();
}
