use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use agl_content::Content;
use agl_events::{EVENT_SCHEMA, EventEnvelope, EventScope, RuntimeEvent, RuntimeEventEnvelope};
use agl_ids::{AttemptId, EventId, MessageId, RequestId, RunId, SessionId, TurnId};

use crate::*;
use agl_kernel::{ChatSessionMachine, ChatSessionPhase, ChatSessionTransition};

static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);
const TEST_SESSION_ID: &str = "ses_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b30";
const TEST_RUN_ID: &str = "run_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b31";
const NEXT_RUN_ID: &str = "run_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b32";
const TEST_TURN_ID: &str = "turn_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b33";
const NEXT_TURN_ID: &str = "turn_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b34";
const TEST_ATTEMPT_ID: &str = "attempt_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b35";
const NEXT_ATTEMPT_ID: &str = "attempt_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b36";
const TEST_REQUEST_ID: &str = "req_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b37";
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

fn request_id() -> RequestId {
    RequestId::parse(TEST_REQUEST_ID).unwrap()
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

fn text(value: impl Into<String>) -> Content {
    Content::text(value).unwrap()
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
            content: text(content),
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
            content: text(content),
        },
    )
}

fn incomplete_assistant_envelope(
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
        RuntimeEvent::AssistantIncomplete {
            message_id: message_id(message_suffix),
            content: text(content),
            source_attempt_id: attempt_id(),
            reason: agl_events::IncompleteOutputReasonEvent::ModelLength,
            continuation_index: 0,
            execution_context_revision: 1,
            runtime_context_revision: 1,
            policy_hash: "sha256:test-policy".to_owned(),
        },
    )
}

fn start_session(root: impl AsRef<std::path::Path>, session_id: SessionId) -> ChatSessionStore {
    ChatSessionStore::start(
        root,
        session_id,
        TEST_CONFIG_PATH,
        TEST_BACKEND,
        execution_context(),
        runtime_selection(),
    )
    .unwrap()
}

fn execution_context() -> agl_exec::ExecutionContextSnapshot {
    let workspace = std::env::temp_dir().canonicalize().unwrap();
    agl_exec::ExecutionContextSnapshot {
        workspace_root: workspace.clone(),
        working_directory: workspace,
        private_execution_roots: Vec::new(),
        shell: agl_exec::ShellProfileSnapshot {
            program: PathBuf::from("/bin/sh"),
            command_args: vec!["-c".to_owned()],
            login_command_args: Some(vec!["-l".to_owned(), "-c".to_owned()]),
            environment_names: vec!["PATH".to_owned()],
            executable_digest: "sha256:test-shell".to_owned(),
            config_digest: "sha256:test-config".to_owned(),
        },
        revision: 1,
        profile_metadata: "workspace".to_owned(),
    }
}

fn runtime_selection() -> SessionRuntimeSelection {
    SessionRuntimeSelection {
        function_ref: None,
        model_id: Some("test-model".to_owned()),
        operation_mode: "read-only".to_owned(),
        skill_ids: Vec::new(),
        revision: 1,
    }
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
            content: text("hello"),
        })
        .unwrap();
    machine
        .apply(ChatSessionTransition::RecordUserMessage {
            run_id: run_id(),
            turn_id: turn_id(),
            message_id: message_id('7'),
            content: text("hello"),
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
                content: text("hi"),
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
            content: text("hi"),
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
fn reverse_replay_reader_bounds_each_scan_of_a_large_transcript() {
    use std::io::Write as _;

    let root = temp_root("bounded-reverse-replay");
    let id = session_id();
    let store = start_session(&root, id.clone());
    let repeated = ChatSessionEvent::ContextCleared {
        session_id: id.clone(),
    };
    let mut transcript = std::fs::OpenOptions::new()
        .append(true)
        .open(store.transcript_jsonl())
        .unwrap();
    for _ in 0..25_000 {
        serde_json::to_writer(&mut transcript, &repeated).unwrap();
        transcript.write_all(b"\n").unwrap();
    }
    transcript.flush().unwrap();
    drop(transcript);

    let scan_limit = 4 * 1024;
    let mut reader = ChatSessionStore::open_reverse_replay(&root, id.clone(), 1024).unwrap();
    assert!(reader.transcript_len() > 1024 * 1024);
    let captured_len = reader.transcript_len();
    let mut consumed = 0usize;
    let mut records = 0usize;
    loop {
        match reader.next_record(scan_limit - consumed).unwrap() {
            ChatSessionReverseRead::Record(record) => {
                consumed += record.transcript_bytes;
                records += 1;
            }
            ChatSessionReverseRead::ScanLimitReached => break,
            ChatSessionReverseRead::End => panic!("large transcript unexpectedly fit in one scan"),
        }
    }
    assert!(records > 0);
    assert!(consumed <= scan_limit);
    assert!(u64::try_from(consumed).unwrap() < captured_len);

    let continuation = reader.next_offset();
    assert!(continuation > 0);
    let mut resumed = ChatSessionStore::open_reverse_replay(&root, id, 1024).unwrap();
    resumed.set_end_offset(continuation).unwrap();
    let ChatSessionReverseRead::Record(next) = resumed.next_record(scan_limit).unwrap() else {
        panic!("bounded reverse replay did not resume at its record boundary");
    };
    assert_eq!(next.end_offset, continuation);

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
                content: text("foreign"),
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

    let err = ChatSessionStore::start(
        &root,
        id,
        TEST_CONFIG_PATH,
        TEST_BACKEND,
        execution_context(),
        runtime_selection(),
    )
    .unwrap_err();
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
fn incomplete_assistant_and_continuation_claim_survive_reopen() {
    let root = temp_root("incomplete-continuation");
    let id = session_id();
    let mut store = start_session(&root, id.clone());
    store
        .append_user_message(user_envelope(run_id(), turn_id(), 1, '0', '7', "hello"))
        .unwrap();
    store
        .link_attempt(runtime_envelope(
            session_id(),
            run_id(),
            turn_id(),
            2,
            '1',
            Some(attempt_id()),
            RuntimeEvent::ModelAttemptLinked,
        ))
        .unwrap();
    store
        .append_incomplete_assistant_message(incomplete_assistant_envelope(
            run_id(),
            turn_id(),
            3,
            '2',
            '8',
            "bounded partial",
        ))
        .unwrap();
    store
        .append_incomplete_continuation_claim(
            message_id('8'),
            "continue-stable-1".to_owned(),
            next_run_id(),
            next_turn_id(),
            request_id(),
        )
        .unwrap();
    store
        .append_incomplete_continuation_claim(
            message_id('8'),
            "continue-stable-1".to_owned(),
            next_run_id(),
            next_turn_id(),
            request_id(),
        )
        .unwrap();
    let conflicting = store
        .append_incomplete_continuation_claim(
            message_id('8'),
            "continue-stable-2".to_owned(),
            run_id(),
            turn_id(),
            request_id(),
        )
        .unwrap_err();
    assert!(
        conflicting
            .to_string()
            .contains("already has a different continuation claim")
    );

    let reopened = ChatSessionStore::open(&root, id).unwrap();
    let replay = reopened.read_replay().unwrap();
    assert!(replay.events.iter().any(|event| matches!(
        event,
        ChatSessionEvent::Runtime { envelope }
            if matches!(
                &envelope.payload,
                RuntimeEvent::AssistantIncomplete {
                    message_id: actual,
                    content,
                    continuation_index: 0,
                    ..
                } if actual == &message_id('8')
                    && content.text_only().as_deref() == Some("bounded partial")
            )
    )));
    assert_eq!(
        replay
            .events
            .iter()
            .filter(|event| matches!(
                event,
                ChatSessionEvent::IncompleteContinuationClaimed {
                    message_id: actual,
                    client_submission_id,
                    continuation_run_id,
                    continuation_turn_id,
                    continuation_request_id,
                    ..
                } if actual == &message_id('8')
                    && client_submission_id == "continue-stable-1"
                    && continuation_run_id == &next_run_id()
                    && continuation_turn_id == &next_turn_id()
                    && continuation_request_id == &request_id()
            ))
            .count(),
        1
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn incomplete_continuation_input_is_durable_idempotent_and_restart_safe() {
    let root = temp_root("incomplete-continuation-input");
    let id = session_id();
    let source_message_id = message_id('8');
    let continuation_run_id = RunId::generate();
    let continuation_turn_id = TurnId::generate();
    let mut store = start_session(&root, id.clone());
    store
        .append_user_message(user_envelope(run_id(), turn_id(), 1, '0', '7', "hello"))
        .unwrap();
    store
        .link_attempt(runtime_envelope(
            session_id(),
            run_id(),
            turn_id(),
            2,
            '1',
            Some(attempt_id()),
            RuntimeEvent::ModelAttemptLinked,
        ))
        .unwrap();
    store
        .append_incomplete_assistant_message(incomplete_assistant_envelope(
            run_id(),
            turn_id(),
            3,
            '2',
            '8',
            "bounded partial",
        ))
        .unwrap();
    store
        .append_user_message(user_envelope(
            next_run_id(),
            next_turn_id(),
            1,
            '3',
            '9',
            "later queued prompt",
        ))
        .unwrap();
    store
        .link_attempt(runtime_envelope(
            session_id(),
            next_run_id(),
            next_turn_id(),
            2,
            '4',
            Some(next_attempt_id()),
            RuntimeEvent::ModelAttemptLinked,
        ))
        .unwrap();
    store
        .append_assistant_message(assistant_envelope(
            next_run_id(),
            next_turn_id(),
            3,
            '5',
            'a',
            "later answer",
        ))
        .unwrap();

    let missing_claim = store
        .begin_incomplete_continuation_input(
            source_message_id.clone(),
            continuation_run_id.clone(),
            continuation_turn_id.clone(),
            &request_id(),
        )
        .unwrap_err();
    assert!(
        missing_claim
            .to_string()
            .contains("missing its durable continuation claim")
    );
    store
        .append_incomplete_continuation_claim(
            source_message_id.clone(),
            "continue-restart-safe".to_owned(),
            continuation_run_id.clone(),
            continuation_turn_id.clone(),
            request_id(),
        )
        .unwrap();
    store
        .begin_incomplete_continuation_input(
            source_message_id.clone(),
            continuation_run_id.clone(),
            continuation_turn_id.clone(),
            &request_id(),
        )
        .unwrap();
    assert_eq!(store.machine().phase(), ChatSessionPhase::RunningTurn);

    store
        .begin_incomplete_continuation_input(
            source_message_id.clone(),
            continuation_run_id.clone(),
            continuation_turn_id.clone(),
            &request_id(),
        )
        .unwrap();
    let wrong_request = store
        .begin_incomplete_continuation_input(
            source_message_id.clone(),
            continuation_run_id.clone(),
            continuation_turn_id.clone(),
            &RequestId::generate(),
        )
        .unwrap_err();
    assert!(
        wrong_request
            .to_string()
            .contains("identity differs from its durable continuation claim")
    );
    drop(store);

    let mut reopened = ChatSessionStore::open(&root, id).unwrap();
    assert_eq!(reopened.machine().phase(), ChatSessionPhase::AwaitingInput);
    reopened
        .begin_incomplete_continuation_input(
            source_message_id.clone(),
            continuation_run_id.clone(),
            continuation_turn_id.clone(),
            &request_id(),
        )
        .unwrap();
    assert_eq!(reopened.machine().phase(), ChatSessionPhase::RunningTurn);
    reopened
        .link_attempt(runtime_envelope(
            session_id(),
            continuation_run_id.clone(),
            continuation_turn_id.clone(),
            1,
            '6',
            Some(AttemptId::generate()),
            RuntimeEvent::ModelAttemptLinked,
        ))
        .unwrap();

    let replay = reopened.read_replay().unwrap();
    assert_eq!(
        replay
            .events
            .iter()
            .filter(|event| matches!(
                event,
                ChatSessionEvent::IncompleteContinuationInputStarted {
                    source_message_id: actual_source,
                    continuation_run_id: actual_run_id,
                    continuation_turn_id: actual_turn_id,
                    ..
                } if actual_source == &source_message_id
                    && actual_run_id == &continuation_run_id
                    && actual_turn_id == &continuation_turn_id
            ))
            .count(),
        1
    );
    assert_eq!(
        replay
            .events
            .iter()
            .filter(|event| matches!(
                event,
                ChatSessionEvent::Runtime { envelope }
                    if matches!(envelope.payload, RuntimeEvent::UserMessage { .. })
            ))
            .count(),
        2,
        "continuation input must not create a synthetic durable user message"
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn context_clear_revokes_incomplete_continuation_input() {
    let root = temp_root("incomplete-continuation-cleared");
    let source_message_id = message_id('8');
    let continuation_run_id = next_run_id();
    let continuation_turn_id = next_turn_id();
    let mut store = start_test_session(&root);
    store
        .append_user_message(user_envelope(run_id(), turn_id(), 1, '0', '7', "hello"))
        .unwrap();
    store
        .link_attempt(runtime_envelope(
            session_id(),
            run_id(),
            turn_id(),
            2,
            '1',
            Some(attempt_id()),
            RuntimeEvent::ModelAttemptLinked,
        ))
        .unwrap();
    store
        .append_incomplete_assistant_message(incomplete_assistant_envelope(
            run_id(),
            turn_id(),
            3,
            '2',
            '8',
            "bounded partial",
        ))
        .unwrap();
    store
        .append_incomplete_continuation_claim(
            source_message_id.clone(),
            "continue-before-clear".to_owned(),
            continuation_run_id.clone(),
            continuation_turn_id.clone(),
            request_id(),
        )
        .unwrap();
    store.append_context_cleared().unwrap();

    let error = store
        .begin_incomplete_continuation_input(
            source_message_id,
            continuation_run_id,
            continuation_turn_id,
            &request_id(),
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("available incomplete assistant message in the current context")
    );
    assert!(!store.read_replay().unwrap().events.iter().any(|event| {
        matches!(
            event,
            ChatSessionEvent::IncompleteContinuationInputStarted { .. }
        )
    }));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn concurrent_incomplete_claims_commit_exactly_one_identity() {
    let root = temp_root("incomplete-claim-race");
    let id = session_id();
    let mut store = start_session(&root, id.clone());
    store
        .append_user_message(user_envelope(run_id(), turn_id(), 1, '0', '7', "hello"))
        .unwrap();
    store
        .link_attempt(runtime_envelope(
            session_id(),
            run_id(),
            turn_id(),
            2,
            '1',
            Some(attempt_id()),
            RuntimeEvent::ModelAttemptLinked,
        ))
        .unwrap();
    store
        .append_incomplete_assistant_message(incomplete_assistant_envelope(
            run_id(),
            turn_id(),
            3,
            '2',
            '8',
            "bounded partial",
        ))
        .unwrap();
    drop(store);

    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();
    for suffix in ["left", "right"] {
        let root = root.clone();
        let id = id.clone();
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            let mut store = ChatSessionStore::open(root, id).unwrap();
            barrier.wait();
            store.append_incomplete_continuation_claim(
                message_id('8'),
                format!("continue-{suffix}"),
                RunId::generate(),
                TurnId::generate(),
                RequestId::generate(),
            )
        }));
    }
    barrier.wait();
    let results = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);

    let replay = ChatSessionStore::open(&root, id)
        .unwrap()
        .read_replay()
        .unwrap();
    assert_eq!(
        replay
            .events
            .iter()
            .filter(|event| matches!(
                event,
                ChatSessionEvent::IncompleteContinuationClaimed { .. }
            ))
            .count(),
        1
    );
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

#[test]
fn catalog_reports_durable_active_and_finished_sessions() {
    let root = temp_root("catalog");
    let id = session_id();
    let mut store = start_session(&root, id.clone());

    let catalog = ChatSessionStore::catalog(&root).unwrap();
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].metadata.session_id, id);
    assert_eq!(catalog[0].status, SessionCatalogStatus::Active);

    store.request_exit().unwrap();
    let catalog = ChatSessionStore::catalog(&root).unwrap();
    assert_eq!(catalog[0].status, SessionCatalogStatus::Finished);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(store.session_dir())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for name in ["session.json", "transcript.jsonl"] {
            assert_eq!(
                std::fs::metadata(store.session_dir().join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn catalog_ignores_directories_outside_the_current_session_id_format() {
    let root = temp_root("catalog-non-session-directory");
    let id = session_id();
    let _store = start_session(&root, id.clone());
    let obsolete = root.join("session-legacy");
    std::fs::create_dir_all(&obsolete).unwrap();
    std::fs::write(
        obsolete.join("session.json"),
        b"not-current-session-metadata",
    )
    .unwrap();

    let catalog = ChatSessionStore::catalog(&root).unwrap();
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].metadata.session_id, id);

    std::fs::remove_dir_all(root).unwrap();
}
