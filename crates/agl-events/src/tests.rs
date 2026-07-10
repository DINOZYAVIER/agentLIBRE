use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agl_ids::{AttemptId, EventId, MessageId, RequestId, RunId, SessionId, StepId, TurnId};
use serde_json::json;

use crate::*;

const RUN_ID: &str = "run_01890f17-4a00-7000-8000-000000000001";
const TURN_ID: &str = "turn_01890f17-4a00-7000-8000-000000000002";
const ATTEMPT_ID: &str = "attempt_01890f17-4a00-7000-8000-000000000003";
const EVENT_ID: &str = "evt_01890f17-4a00-7000-8000-000000000004";
const REQUEST_ID: &str = "req_01890f17-4a00-7000-8000-000000000005";
const MESSAGE_ID: &str = "msg_01890f17-4a00-7000-8000-000000000006";
const CAUSED_BY: &str = "evt_01890f17-4a00-7000-8000-000000000007";
const SESSION_ID: &str = "ses_01890f17-4a00-7000-8000-000000000008";
const STEP_ID: &str = "step_01890f17-4a00-7000-8000-000000000009";

fn run_id() -> RunId {
    RunId::parse(RUN_ID).unwrap()
}

fn turn_id() -> TurnId {
    TurnId::parse(TURN_ID).unwrap()
}

fn event_scope() -> EventScope {
    EventScope::builder(run_id())
        .session_id(SessionId::parse(SESSION_ID).unwrap())
        .turn_id(turn_id())
        .build()
        .unwrap()
}

fn envelope(payload: RuntimeEvent) -> RuntimeEventEnvelope {
    EventEnvelope {
        schema: EVENT_SCHEMA.to_string(),
        event_id: EventId::parse(EVENT_ID).unwrap(),
        sequence: 7,
        occurred_at_unix_ms: 1_700_000_000_123,
        scope: event_scope(),
        request_id: Some(RequestId::parse(REQUEST_ID).unwrap()),
        caused_by: Some(EventId::parse(CAUSED_BY).unwrap()),
        payload,
    }
}

#[test]
fn event_scope_requires_a_run_and_rejects_attempt_without_turn() {
    let scope = EventScope::builder(run_id())
        .session_id(SessionId::parse(SESSION_ID).unwrap())
        .turn_id(turn_id())
        .step_id(StepId::parse(STEP_ID).unwrap())
        .attempt_id(AttemptId::parse(ATTEMPT_ID).unwrap())
        .build()
        .unwrap();

    assert_eq!(scope.run_id().as_str(), RUN_ID);
    assert_eq!(scope.session_id().unwrap().as_str(), SESSION_ID);
    assert_eq!(scope.turn_id().unwrap().as_str(), TURN_ID);
    assert_eq!(scope.step_id().unwrap().as_str(), STEP_ID);
    assert_eq!(scope.attempt_id().unwrap().as_str(), ATTEMPT_ID);

    let error = EventScope::builder(run_id())
        .attempt_id(AttemptId::parse(ATTEMPT_ID).unwrap())
        .build()
        .unwrap_err();
    assert_eq!(error, EventScopeError::AttemptWithoutTurn);

    let invalid = json!({
        "run_id": RUN_ID,
        "attempt_id": ATTEMPT_ID,
    });
    assert!(serde_json::from_value::<EventScope>(invalid).is_err());
}

#[test]
fn envelope_has_a_stable_serde_shape() {
    let envelope = envelope(RuntimeEvent::TurnStarted {
        user_input: "read README".to_string(),
    });

    let value = serde_json::to_value(&envelope).unwrap();
    assert_eq!(
        value,
        json!({
            "schema": "agentlibre.event.v1alpha",
            "event_id": EVENT_ID,
            "sequence": 7,
            "occurred_at_unix_ms": 1_700_000_000_123_u64,
            "scope": {
                "run_id": RUN_ID,
                "session_id": SESSION_ID,
                "turn_id": TURN_ID,
            },
            "request_id": REQUEST_ID,
            "caused_by": CAUSED_BY,
            "payload": {
                "kind": "turn.started",
                "user_input": "read README",
            },
        })
    );

    let decoded: RuntimeEventEnvelope = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, envelope);
}

#[test]
fn envelope_deserialization_rejects_wrong_schema_and_zero_sequence() {
    let mut value =
        serde_json::to_value(envelope(RuntimeEvent::ModelRequested { request_index: 0 })).unwrap();
    value["schema"] = json!("agentlibre.event.old");
    assert!(serde_json::from_value::<RuntimeEventEnvelope>(value).is_err());

    let mut value =
        serde_json::to_value(envelope(RuntimeEvent::ModelRequested { request_index: 0 })).unwrap();
    value["sequence"] = json!(0);
    assert!(serde_json::from_value::<RuntimeEventEnvelope>(value).is_err());

    let mut value =
        serde_json::to_value(envelope(RuntimeEvent::ModelRequested { request_index: 0 })).unwrap();
    value["unexpected"] = json!(true);
    assert!(serde_json::from_value::<RuntimeEventEnvelope>(value).is_err());

    let mut value =
        serde_json::to_value(envelope(RuntimeEvent::ModelRequested { request_index: 0 })).unwrap();
    value["payload"]["turn_id"] = json!(TURN_ID);
    assert!(serde_json::from_value::<RuntimeEventEnvelope>(value).is_err());
}

#[test]
fn redaction_preserves_every_envelope_field() {
    let full = envelope(RuntimeEvent::ModelRequestFailed {
        request_index: 2,
        message: "backend stderr secret".to_string(),
    });
    let safe = full.redacted();

    assert_eq!(safe.schema, full.schema);
    assert_eq!(safe.event_id, full.event_id);
    assert_eq!(safe.sequence, full.sequence);
    assert_eq!(safe.occurred_at_unix_ms, full.occurred_at_unix_ms);
    assert_eq!(safe.scope, full.scope);
    assert_eq!(safe.request_id, full.request_id);
    assert_eq!(safe.caused_by, full.caused_by);
    assert_eq!(safe.payload.kind(), full.payload.kind());
    assert_eq!(
        safe.payload,
        SafeRuntimeEvent::ModelRequestFailed {
            request_index: 2,
            message_bytes: 21,
        }
    );
}

#[test]
fn redaction_covers_turn_transcript_and_inference_content() {
    let message_id = MessageId::parse(MESSAGE_ID).unwrap();
    let events = [
        RuntimeEvent::ToolArgsInvalid {
            name: "read_file".to_string(),
            message: "secret argument error".to_string(),
        },
        RuntimeEvent::AssistantToolCall {
            message_id,
            name: "read_file".to_string(),
            arguments: json!({"path": "SECRET.md", "line": 42}),
        },
        RuntimeEvent::InferenceAttemptStarted {
            backend: "llama.cpp".to_string(),
            request_path: PathBuf::from("/private/user/request.json"),
        },
        RuntimeEvent::InferenceAttemptFailed {
            message: "secret backend failure".to_string(),
        },
    ];

    for event in events {
        let full = serde_json::to_string(&event).unwrap();
        let safe_event = SafeRuntimeEvent::from(&event);
        let safe = serde_json::to_string(&safe_event).unwrap();
        assert_eq!(safe_event.kind(), event.kind());
        assert_ne!(safe, full);
        for forbidden in [
            "secret argument error",
            "SECRET.md",
            "/private/user/request.json",
            "secret backend failure",
        ] {
            assert!(
                !safe.contains(forbidden),
                "redaction leaked {forbidden}: {safe}"
            );
        }
    }
}

#[test]
fn invalid_capability_names_are_not_copied_into_safe_denial_events() {
    let model_controlled_name = "SECRET invalid capability name";
    let event = RuntimeEvent::CapabilityCallDenied {
        policy_hash: format!("sha256:{}", "0".repeat(64)),
        capability_id: None,
        reason_code: "invalid_capability_id".to_string(),
    };

    let safe = SafeRuntimeEvent::from(&event);
    let encoded = serde_json::to_string(&safe).unwrap();

    assert!(!encoded.contains(model_controlled_name));
    assert!(encoded.contains(r#""capability_id":null"#));
    assert!(encoded.contains("invalid_capability_id"));
}

#[test]
fn json_redaction_preserves_only_value_shape() {
    let safe = SafeRuntimeEvent::from(&RuntimeEvent::ToolCallStarted {
        name: "read_file".to_string(),
        arguments: json!({"path": "SECRET.md", "line": 42}),
    });
    let line = serde_json::to_string(&safe).unwrap();

    assert!(line.contains(r#""keys":["line","path"]"#), "{line}");
    assert!(!line.contains("SECRET.md"), "{line}");
    assert!(!line.contains("42"), "{line}");
}

#[test]
fn payloads_cannot_duplicate_envelope_scope_ids() {
    let payload = RuntimeEvent::InferenceAttemptStarted {
        backend: "test".to_string(),
        request_path: PathBuf::from("request.json"),
    };
    let value = serde_json::to_value(payload).unwrap();

    assert!(value.get("run_id").is_none());
    assert!(value.get("session_id").is_none());
    assert!(value.get("turn_id").is_none());
    assert!(value.get("step_id").is_none());
    assert!(value.get("attempt_id").is_none());
}

#[test]
fn writer_assigns_sequence_causation_and_resumes_after_restart() {
    let path = temp_event_path("resume");
    let first;
    {
        let writer = RuntimeEventWriter::open(&path).unwrap();
        first = writer
            .append(EventDraft::new(
                event_scope(),
                RuntimeEvent::TurnStarted {
                    user_input: "secret prompt".to_string(),
                },
            ))
            .unwrap();
        let second = writer
            .append(
                EventDraft::new(
                    event_scope(),
                    RuntimeEvent::ModelRequested { request_index: 0 },
                )
                .with_causation(first.event_id.clone()),
            )
            .unwrap();

        assert_eq!(first.sequence, 1);
        assert_eq!(second.sequence, 2);
        assert_eq!(second.caused_by.as_ref(), Some(&first.event_id));
        assert_ne!(first.event_id, second.event_id);
        assert!(first.occurred_at_unix_ms > 0);
    }

    let writer = RuntimeEventWriter::open(&path).unwrap();
    let third = writer
        .append(EventDraft::new(
            event_scope(),
            RuntimeEvent::TurnFinished {
                status: TurnFinishStatus::Answered,
            },
        ))
        .unwrap();
    assert_eq!(third.sequence, 3);

    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content.lines().count(), 3);
    assert!(!content.contains("secret prompt"), "{content}");
    for line in content.lines() {
        let decoded: SafeRuntimeEventEnvelope = serde_json::from_str(line).unwrap();
        assert_eq!(decoded.schema, EVENT_SCHEMA);
        assert_eq!(decoded.scope, event_scope());
    }

    std::fs::remove_file(path).unwrap();
}

#[test]
fn writer_returns_full_and_safe_views_of_one_envelope() {
    let path = temp_event_path("full-safe");
    let writer = RuntimeEventWriter::open(&path).unwrap();
    let (full, safe) = writer
        .append_with_full(
            EventDraft::new(
                event_scope(),
                RuntimeEvent::AssistantMessage {
                    message_id: MessageId::parse(MESSAGE_ID).unwrap(),
                    content: "private answer".to_string(),
                },
            )
            .with_request_id(RequestId::parse(REQUEST_ID).unwrap()),
        )
        .unwrap();

    assert_eq!(safe.schema, full.schema);
    assert_eq!(safe.event_id, full.event_id);
    assert_eq!(safe.sequence, full.sequence);
    assert_eq!(safe.occurred_at_unix_ms, full.occurred_at_unix_ms);
    assert_eq!(safe.scope, full.scope);
    assert_eq!(safe.request_id, full.request_id);
    assert_eq!(safe.caused_by, full.caused_by);
    assert_eq!(safe.payload.kind(), full.payload.kind());

    let persisted = std::fs::read_to_string(&path).unwrap();
    assert!(!persisted.contains("private answer"));
    let decoded: SafeRuntimeEventEnvelope = serde_json::from_str(persisted.trim()).unwrap();
    assert_eq!(decoded, safe);

    std::fs::remove_file(path).unwrap();
}

#[test]
fn concurrent_opens_and_appends_have_unique_ids_and_sequences() {
    const EVENT_COUNT: usize = 32;

    let path = temp_event_path("concurrent");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(EVENT_COUNT));
    let mut threads = Vec::new();
    for request_index in 0..EVENT_COUNT {
        let path = path.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            RuntimeEventWriter::open(path)
                .unwrap()
                .append(EventDraft::new(
                    event_scope(),
                    RuntimeEvent::ModelRequested { request_index },
                ))
                .unwrap()
        }));
    }

    let envelopes = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    let event_ids = envelopes
        .iter()
        .map(|envelope| envelope.event_id.clone())
        .collect::<HashSet<_>>();
    let mut sequences = envelopes
        .iter()
        .map(|envelope| envelope.sequence)
        .collect::<Vec<_>>();
    sequences.sort_unstable();

    assert_eq!(event_ids.len(), EVENT_COUNT);
    assert_eq!(sequences, (1..=EVENT_COUNT as u64).collect::<Vec<_>>());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap().lines().count(),
        EVENT_COUNT
    );

    std::fs::remove_file(path).unwrap();
}

#[test]
fn restart_fails_loudly_on_non_resumable_sequence() {
    let path = temp_event_path("invalid-sequence");
    let invalid: SafeRuntimeEventEnvelope = EventEnvelope {
        schema: EVENT_SCHEMA.to_string(),
        event_id: EventId::parse(EVENT_ID).unwrap(),
        sequence: 2,
        occurred_at_unix_ms: 1_700_000_000_123,
        scope: event_scope(),
        request_id: None,
        caused_by: None,
        payload: SafeRuntimeEvent::ModelRequested { request_index: 0 },
    };
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&invalid).unwrap()),
    )
    .unwrap();

    let error = RuntimeEventWriter::open(&path).unwrap_err();
    assert!(error.to_string().contains("expected 1"), "{error:#}");

    std::fs::remove_file(path).unwrap();
}

fn temp_event_path(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "agl-events-{name}-{}-{}.jsonl",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}
