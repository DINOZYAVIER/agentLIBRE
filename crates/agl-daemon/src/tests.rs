use agl_chat::InferenceOptions;
use agl_client::{AgentLibreClient, ClientError, DaemonTransport};
use agl_cron::{CronJob, CronJobDraft, CronRunStatus, CronTargetKind};
use agl_events::{
    EVENT_SCHEMA as RUNTIME_EVENT_SCHEMA, EventEnvelope, EventScope, RuntimeEvent,
    RuntimeEventEnvelope, SafeRuntimeEvent, SafeRuntimeEventEnvelope, TurnFinishStatus,
};
use agl_ids::{AttemptId, EventId, MessageId, RequestId, RunId, SessionId, TurnId};
use agl_protocol::{
    AssistantMessageEvent, DaemonCapability, DaemonEvent, DaemonEventKind, DaemonRequest,
    DaemonRequestKind, EVENT_SCHEMA, HelloRequest, PROTOCOL_VERSION, ProtocolErrorCode,
    REQUEST_SCHEMA, SessionListEvent, SessionListRequest, SessionStatus, SessionStatusRequest,
    SessionTranscriptRequest, SessionTurnRequest, TranscriptEvent, TurnTerminalStatus,
};
use agl_runtime::{
    AgentLibreHistoryConfig, AgentLibreLoggingConfig, AgentLibrePaths, AgentLibreRuntimeConfig,
    AgentLibreWorkspaceConfig,
};
use agl_session::ChatSessionStore;
use agl_store::AglStore;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::*;

static TEST_RUNTIME_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn runtime() -> AgentLibreRuntimeConfig {
    let index = TEST_RUNTIME_COUNTER.fetch_add(1, Ordering::SeqCst);
    let root = std::env::temp_dir().join(format!(
        "agl-daemon-test-{}-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("main"),
        index
    ));
    AgentLibreRuntimeConfig {
        paths: AgentLibrePaths::from_agl_home(root),
        logging: AgentLibreLoggingConfig::from_env(),
        history: AgentLibreHistoryConfig::default(),
        workspace: AgentLibreWorkspaceConfig::default(),
    }
}

fn request(kind: DaemonRequestKind) -> DaemonRequest {
    DaemonRequest::new(RequestId::generate(), kind)
}

fn event_scope(
    session_id: &SessionId,
    run_id: &RunId,
    turn_id: &TurnId,
    attempt_id: Option<&AttemptId>,
) -> EventScope {
    let mut scope = EventScope::builder(run_id.clone())
        .session_id(session_id.clone())
        .turn_id(turn_id.clone());
    if let Some(attempt_id) = attempt_id {
        scope = scope.attempt_id(attempt_id.clone());
    }
    scope.build().unwrap()
}

fn runtime_envelope(
    session_id: &SessionId,
    run_id: &RunId,
    turn_id: &TurnId,
    attempt_id: Option<&AttemptId>,
    sequence: u64,
    payload: RuntimeEvent,
) -> RuntimeEventEnvelope {
    EventEnvelope {
        schema: RUNTIME_EVENT_SCHEMA.to_string(),
        event_id: EventId::generate(),
        sequence,
        occurred_at_unix_ms: sequence,
        scope: event_scope(session_id, run_id, turn_id, attempt_id),
        request_id: None,
        caused_by: None,
        payload,
    }
}

fn safe_runtime_envelope(
    session_id: &SessionId,
    run_id: &RunId,
    turn_id: &TurnId,
    request_id: &RequestId,
    sequence: u64,
    payload: SafeRuntimeEvent,
) -> SafeRuntimeEventEnvelope {
    EventEnvelope {
        schema: RUNTIME_EVENT_SCHEMA.to_string(),
        event_id: EventId::generate(),
        sequence,
        occurred_at_unix_ms: sequence,
        scope: event_scope(session_id, run_id, turn_id, None),
        request_id: Some(request_id.clone()),
        caused_by: None,
        payload,
    }
}

struct StateTransport {
    state: DaemonState,
    responses: VecDeque<String>,
}

impl StateTransport {
    fn new(state: DaemonState) -> Self {
        Self {
            state,
            responses: VecDeque::new(),
        }
    }
}

impl DaemonTransport for StateTransport {
    fn write_line(&mut self, line: &str) -> Result<(), ClientError> {
        let request = serde_json::from_str::<DaemonRequest>(line)?;
        self.responses.extend(
            self.state
                .handle_request(request)
                .into_iter()
                .map(|event| serde_json::to_string(&event))
                .collect::<Result<Vec<_>, _>>()?,
        );
        Ok(())
    }

    fn read_line(&mut self) -> Result<String, ClientError> {
        self.responses.pop_front().ok_or(ClientError::EmptyResponse)
    }
}

#[test]
fn hello_reports_alpha_capabilities_without_loading_model() {
    let mut state = DaemonState::new(runtime(), InferenceOptions::default());

    let events = state.handle_request(request(DaemonRequestKind::Hello(HelloRequest {
        client_name: Some("test".to_string()),
        accepted_protocol_versions: vec![PROTOCOL_VERSION.to_string()],
    })));

    assert_eq!(events.len(), 1);
    match &events[0].kind {
        DaemonEventKind::Hello(event) => {
            assert_eq!(event.protocol_version, PROTOCOL_VERSION);
            assert!(event.capabilities.contains(&DaemonCapability::SessionOpen));
            assert!(event.capabilities.contains(&DaemonCapability::SessionList));
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn status_for_unknown_session_returns_not_found() {
    let mut state = DaemonState::new(runtime(), InferenceOptions::default());
    let missing_session_id = SessionId::generate();

    let events = state.handle_request(request(DaemonRequestKind::SessionStatus(
        SessionStatusRequest {
            session_id: missing_session_id,
        },
    )));

    match &events[0].kind {
        DaemonEventKind::Error(error) => assert_eq!(error.code, ProtocolErrorCode::NotFound),
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn session_list_starts_empty() {
    let mut state = DaemonState::new(runtime(), InferenceOptions::default());

    let events = state.handle_request(request(DaemonRequestKind::SessionList(
        SessionListRequest::default(),
    )));

    match &events[0].kind {
        DaemonEventKind::SessionList(event) => assert!(event.sessions.is_empty()),
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn session_turn_idempotency_replays_without_rerunning_turn() {
    let mut state = DaemonState::new(runtime(), InferenceOptions::default());
    let session_id = SessionId::generate();
    state.insert_test_session(
        session_id.clone(),
        vec![agl_chat::ChatTurnStatus::Answered {
            answer: "hello".to_string(),
        }],
    );

    let first = state.handle_request(request(DaemonRequestKind::SessionTurn(
        SessionTurnRequest {
            session_id: session_id.clone(),
            text: "say hi".to_string(),
            idempotency_key: Some("matrix-event-1".to_string()),
        },
    )));
    let second = state.handle_request(DaemonRequest::new(
        RequestId::generate(),
        DaemonRequestKind::SessionTurn(SessionTurnRequest {
            session_id: session_id.clone(),
            text: "say hi".to_string(),
            idempotency_key: Some("matrix-event-1".to_string()),
        }),
    ));

    assert_eq!(state.test_session_turns(&session_id), 1);
    let started = match &first[0].kind {
        DaemonEventKind::TurnStarted(started) => started,
        other => panic!("unexpected event: {other:?}"),
    };
    match &second[0].kind {
        DaemonEventKind::TurnStarted(event) => {
            assert_eq!(event.session_id, started.session_id);
            assert_eq!(event.run_id, started.run_id);
            assert_eq!(event.turn_id, started.turn_id);
        }
        other => panic!("unexpected event: {other:?}"),
    }
    assert!(matches!(
        second[1].kind,
        DaemonEventKind::AssistantMessage(AssistantMessageEvent { .. })
    ));
    match &second[1].kind {
        DaemonEventKind::AssistantMessage(event) => {
            assert_eq!(event.session_id, started.session_id);
            assert_eq!(event.run_id, started.run_id);
            assert_eq!(event.turn_id, started.turn_id);
        }
        other => panic!("unexpected event: {other:?}"),
    }
    match &second[2].kind {
        DaemonEventKind::TurnFinished(event) => {
            assert_eq!(event.status, TurnTerminalStatus::Answered);
            assert_eq!(event.session_id, started.session_id);
            assert_eq!(event.run_id, started.run_id);
            assert_eq!(event.turn_id, started.turn_id);
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn sequential_turns_share_session_and_have_distinct_run_and_turn_ids() {
    let mut state = DaemonState::new(runtime(), InferenceOptions::default());
    let session_id = SessionId::generate();
    state.insert_test_session(
        session_id.clone(),
        vec![
            agl_chat::ChatTurnStatus::Answered {
                answer: "first".to_string(),
            },
            agl_chat::ChatTurnStatus::Answered {
                answer: "second".to_string(),
            },
        ],
    );

    let first = state.handle_request(request(DaemonRequestKind::SessionTurn(
        SessionTurnRequest {
            session_id: session_id.clone(),
            text: "first".to_string(),
            idempotency_key: None,
        },
    )));
    let second = state.handle_request(request(DaemonRequestKind::SessionTurn(
        SessionTurnRequest {
            session_id: session_id.clone(),
            text: "second".to_string(),
            idempotency_key: None,
        },
    )));

    let first_started = match &first[0].kind {
        DaemonEventKind::TurnStarted(event) => event,
        other => panic!("unexpected event: {other:?}"),
    };
    let second_started = match &second[0].kind {
        DaemonEventKind::TurnStarted(event) => event,
        other => panic!("unexpected event: {other:?}"),
    };
    assert_eq!(first_started.session_id, session_id);
    assert_eq!(second_started.session_id, session_id);
    assert_ne!(first_started.run_id, second_started.run_id);
    assert_ne!(first_started.turn_id, second_started.turn_id);

    for (events, started) in [(&first, first_started), (&second, second_started)] {
        let assistant = events
            .iter()
            .find_map(|event| match &event.kind {
                DaemonEventKind::AssistantMessage(event) => Some(event),
                _ => None,
            })
            .expect("turn must emit an assistant message");
        assert_eq!(assistant.session_id, started.session_id);
        assert_eq!(assistant.run_id, started.run_id);
        assert_eq!(assistant.turn_id, started.turn_id);

        let finished = events
            .iter()
            .find_map(|event| match &event.kind {
                DaemonEventKind::TurnFinished(event) => Some(event),
                _ => None,
            })
            .expect("turn must emit a terminal control event");
        assert_eq!(finished.session_id, started.session_id);
        assert_eq!(finished.run_id, started.run_id);
        assert_eq!(finished.turn_id, started.turn_id);
    }
}

#[test]
fn client_json_transport_keeps_two_turn_envelopes_exact() {
    let session_id = SessionId::generate();
    let mut state = DaemonState::new(runtime(), InferenceOptions::default());
    state.insert_test_session_with_runtime_events(
        session_id.clone(),
        vec![
            agl_chat::ChatTurnStatus::Answered {
                answer: "first answer".to_string(),
            },
            agl_chat::ChatTurnStatus::Answered {
                answer: "second answer".to_string(),
            },
        ],
    );
    let mut client = AgentLibreClient::new(StateTransport::new(state));

    let first = client
        .send_turn(SessionTurnRequest {
            session_id: session_id.clone(),
            text: "first".to_string(),
            idempotency_key: None,
        })
        .unwrap();
    let second = client
        .send_turn(SessionTurnRequest {
            session_id: session_id.clone(),
            text: "second".to_string(),
            idempotency_key: None,
        })
        .unwrap();

    assert_eq!(first.session_id, session_id);
    assert_eq!(second.session_id, session_id);
    assert_ne!(first.run_id, second.run_id);
    assert_ne!(first.turn_id, second.turn_id);
    assert_eq!(first.assistant_text, "first answer");
    assert_eq!(second.assistant_text, "second answer");

    let mut runtime_event_ids = HashSet::new();
    for response in [&first, &second] {
        assert_eq!(response.status, TurnTerminalStatus::Answered);
        assert_eq!(response.events.len(), 5);
        assert!(matches!(
            response.events[0].kind,
            DaemonEventKind::TurnStarted(_)
        ));
        assert!(matches!(
            response.events[1].kind,
            DaemonEventKind::RuntimeEvent(_)
        ));
        assert!(matches!(
            response.events[2].kind,
            DaemonEventKind::RuntimeEvent(_)
        ));
        assert!(matches!(
            response.events[3].kind,
            DaemonEventKind::AssistantMessage(_)
        ));
        assert!(matches!(
            response.events[4].kind,
            DaemonEventKind::TurnFinished(_)
        ));

        let request_id = response.events[0]
            .request_id
            .as_ref()
            .expect("turn events must retain outer request correlation");
        assert!(
            response
                .events
                .iter()
                .all(|event| event.request_id.as_ref() == Some(request_id))
        );
        let runtime_events = response
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                DaemonEventKind::RuntimeEvent(event) => Some(event.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(runtime_events.len(), 2);
        for (index, envelope) in runtime_events.iter().enumerate() {
            assert_eq!(envelope.sequence, u64::try_from(index + 1).unwrap());
            assert_eq!(envelope.request_id.as_ref(), Some(request_id));
            assert_eq!(envelope.scope.session_id(), Some(&session_id));
            assert_eq!(envelope.scope.run_id(), &response.run_id);
            assert_eq!(envelope.scope.turn_id(), Some(&response.turn_id));
            assert!(runtime_event_ids.insert(envelope.event_id.clone()));
        }
        assert!(matches!(
            &runtime_events[0].payload,
            SafeRuntimeEvent::TurnStarted { .. }
        ));
        assert!(matches!(
            &runtime_events[1].payload,
            SafeRuntimeEvent::TurnFinished {
                status: TurnFinishStatus::Answered
            }
        ));
    }
}

#[test]
fn runtime_events_follow_admission_and_keep_outer_request_correlation() {
    let mut state = DaemonState::new(runtime(), InferenceOptions::default());
    let session_id = SessionId::generate();
    let request_id = RequestId::generate();
    state.insert_test_session_with_runtime_events(
        session_id.clone(),
        vec![agl_chat::ChatTurnStatus::Answered {
            answer: "hello".to_string(),
        }],
    );

    let events = state.handle_request(DaemonRequest::new(
        request_id.clone(),
        DaemonRequestKind::SessionTurn(SessionTurnRequest {
            session_id: session_id.clone(),
            text: "say hi".to_string(),
            idempotency_key: None,
        }),
    ));

    assert_eq!(events.len(), 5);
    let started = match &events[0].kind {
        DaemonEventKind::TurnStarted(event) => event,
        other => panic!("unexpected event: {other:?}"),
    };
    assert!(matches!(events[1].kind, DaemonEventKind::RuntimeEvent(_)));
    assert!(matches!(events[2].kind, DaemonEventKind::RuntimeEvent(_)));
    assert!(matches!(
        events[3].kind,
        DaemonEventKind::AssistantMessage(_)
    ));
    assert!(matches!(events[4].kind, DaemonEventKind::TurnFinished(_)));
    assert_eq!(events[1].request_id.as_ref(), Some(&request_id));
    match &events[1].kind {
        DaemonEventKind::RuntimeEvent(event) => {
            assert_eq!(event.request_id.as_ref(), Some(&request_id));
            assert_eq!(event.scope.session_id(), Some(&session_id));
            assert_eq!(event.scope.run_id(), &started.run_id);
            assert_eq!(event.scope.turn_id(), Some(&started.turn_id));
        }
        other => panic!("unexpected event: {other:?}"),
    }
    match &events[2].kind {
        DaemonEventKind::RuntimeEvent(event) => {
            assert_eq!(event.sequence, 2);
            assert!(matches!(
                event.payload,
                SafeRuntimeEvent::TurnFinished {
                    status: TurnFinishStatus::Answered
                }
            ));
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn failed_chat_output_emits_runtime_then_failed_control_terminal() {
    let mut state = DaemonState::new(runtime(), InferenceOptions::default());
    let session_id = SessionId::generate();
    state.insert_test_session(
        session_id.clone(),
        vec![agl_chat::ChatTurnStatus::Failed {
            message: "model failed".to_string(),
        }],
    );

    let events = state.handle_request(request(DaemonRequestKind::SessionTurn(
        SessionTurnRequest {
            session_id: session_id.clone(),
            text: "fail".to_string(),
            idempotency_key: None,
        },
    )));

    assert_eq!(events.len(), 4);
    assert!(matches!(events[0].kind, DaemonEventKind::TurnStarted(_)));
    assert!(matches!(events[1].kind, DaemonEventKind::RuntimeEvent(_)));
    match &events[2].kind {
        DaemonEventKind::TurnFailed(event) => assert_eq!(event.message, "model failed"),
        other => panic!("unexpected event: {other:?}"),
    }
    match &events[3].kind {
        DaemonEventKind::TurnFinished(event) => {
            assert_eq!(event.status, TurnTerminalStatus::Failed);
        }
        other => panic!("unexpected event: {other:?}"),
    }

    let status = state.handle_request(request(DaemonRequestKind::SessionStatus(
        SessionStatusRequest { session_id },
    )));
    match &status[0].kind {
        DaemonEventKind::SessionStatus(event) => assert_eq!(event.status, SessionStatus::Failed),
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn runtime_envelope_validation_rejects_identity_drift() {
    let session_id = SessionId::generate();
    let run_id = RunId::generate();
    let turn_id = TurnId::generate();
    let request_id = RequestId::generate();
    let valid = vec![
        safe_runtime_envelope(
            &session_id,
            &run_id,
            &turn_id,
            &request_id,
            1,
            SafeRuntimeEvent::TurnStarted {
                user_input_bytes: 5,
            },
        ),
        safe_runtime_envelope(
            &session_id,
            &run_id,
            &turn_id,
            &request_id,
            2,
            SafeRuntimeEvent::TurnFinished {
                status: TurnFinishStatus::Answered,
            },
        ),
    ];
    assert!(
        crate::state::validate_runtime_envelopes(
            &valid,
            &request_id,
            &session_id,
            &run_id,
            &turn_id,
            TurnFinishStatus::Answered,
        )
        .is_ok()
    );

    let mut wrong_request = valid.clone();
    wrong_request[0].request_id = Some(RequestId::generate());
    let error = crate::state::validate_runtime_envelopes(
        &wrong_request,
        &request_id,
        &session_id,
        &run_id,
        &turn_id,
        TurnFinishStatus::Answered,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("request ID"));

    let mut wrong_scope = valid;
    wrong_scope[1].scope = event_scope(&SessionId::generate(), &run_id, &turn_id, None);
    let error = crate::state::validate_runtime_envelopes(
        &wrong_scope,
        &request_id,
        &session_id,
        &run_id,
        &turn_id,
        TurnFinishStatus::Answered,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("scope"));
}

#[test]
fn runtime_envelope_validation_rejects_stream_integrity_drift() {
    let session_id = SessionId::generate();
    let run_id = RunId::generate();
    let turn_id = TurnId::generate();
    let request_id = RequestId::generate();
    let valid = vec![
        safe_runtime_envelope(
            &session_id,
            &run_id,
            &turn_id,
            &request_id,
            1,
            SafeRuntimeEvent::TurnStarted {
                user_input_bytes: 5,
            },
        ),
        safe_runtime_envelope(
            &session_id,
            &run_id,
            &turn_id,
            &request_id,
            2,
            SafeRuntimeEvent::TurnFinished {
                status: TurnFinishStatus::Answered,
            },
        ),
    ];

    let mut sequence_gap = valid.clone();
    sequence_gap[1].sequence = 3;
    let error = crate::state::validate_runtime_envelopes(
        &sequence_gap,
        &request_id,
        &session_id,
        &run_id,
        &turn_id,
        TurnFinishStatus::Answered,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("not contiguous"));

    let mut duplicate_id = valid.clone();
    duplicate_id[1].event_id = duplicate_id[0].event_id.clone();
    let error = crate::state::validate_runtime_envelopes(
        &duplicate_id,
        &request_id,
        &session_id,
        &run_id,
        &turn_id,
        TurnFinishStatus::Answered,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("duplicate event ID"));

    let mut wrong_status = valid.clone();
    wrong_status[1].payload = SafeRuntimeEvent::TurnFinished {
        status: TurnFinishStatus::Failed,
    };
    let error = crate::state::validate_runtime_envelopes(
        &wrong_status,
        &request_id,
        &session_id,
        &run_id,
        &turn_id,
        TurnFinishStatus::Answered,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("does not match"));

    let mut terminal_not_last = valid;
    terminal_not_last[0].payload = SafeRuntimeEvent::TurnFinished {
        status: TurnFinishStatus::Answered,
    };
    terminal_not_last[1].payload = SafeRuntimeEvent::AnswerFinal { answer_bytes: 2 };
    let error = crate::state::validate_runtime_envelopes(
        &terminal_not_last,
        &request_id,
        &session_id,
        &run_id,
        &turn_id,
        TurnFinishStatus::Answered,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("is not last"));

    let error = crate::state::validate_runtime_envelopes(
        &[],
        &request_id,
        &session_id,
        &run_id,
        &turn_id,
        TurnFinishStatus::Answered,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("did not produce runtime events"));
}

#[test]
fn session_turn_idempotency_rejects_key_reuse_with_different_text() {
    let mut state = DaemonState::new(runtime(), InferenceOptions::default());
    let session_id = SessionId::generate();
    state.insert_test_session(
        session_id.clone(),
        vec![agl_chat::ChatTurnStatus::Answered {
            answer: "hello".to_string(),
        }],
    );

    let _first = state.handle_request(request(DaemonRequestKind::SessionTurn(
        SessionTurnRequest {
            session_id: session_id.clone(),
            text: "say hi".to_string(),
            idempotency_key: Some("matrix-event-1".to_string()),
        },
    )));
    let conflict = state.handle_request(request(DaemonRequestKind::SessionTurn(
        SessionTurnRequest {
            session_id: session_id.clone(),
            text: "different".to_string(),
            idempotency_key: Some("matrix-event-1".to_string()),
        },
    )));

    assert_eq!(state.test_session_turns(&session_id), 1);
    match &conflict[0].kind {
        DaemonEventKind::Error(error) => {
            assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
            assert!(!error.retryable);
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn session_turn_idempotency_reports_in_progress_key_as_busy() {
    let mut state = DaemonState::new(runtime(), InferenceOptions::default());
    let session_id = SessionId::generate();
    state.insert_test_session(session_id.clone(), Vec::new());
    state.begin_test_turn_idempotency(&session_id, "say hi", "matrix-event-1");

    let events = state.handle_request(request(DaemonRequestKind::SessionTurn(
        SessionTurnRequest {
            session_id: session_id.clone(),
            text: "say hi".to_string(),
            idempotency_key: Some("matrix-event-1".to_string()),
        },
    )));

    assert_eq!(state.test_session_turns(&session_id), 0);
    match &events[0].kind {
        DaemonEventKind::Error(error) => {
            assert_eq!(error.code, ProtocolErrorCode::Busy);
            assert!(error.retryable);
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn shared_daemon_state_reports_busy_while_turn_runs() {
    let state = SharedDaemonState::new(runtime(), InferenceOptions::default());
    let session_id = SessionId::generate();
    state.insert_slow_test_session(
        session_id.clone(),
        vec![agl_chat::ChatTurnStatus::Answered {
            answer: "done".to_string(),
        }],
        Duration::from_millis(300),
    );
    let worker_state = state.clone();
    let worker_session_id = session_id.clone();
    let worker = std::thread::spawn(move || {
        worker_state.handle_request(DaemonRequest::new(
            RequestId::generate(),
            DaemonRequestKind::SessionTurn(SessionTurnRequest {
                session_id: worker_session_id,
                text: "slow".to_string(),
                idempotency_key: None,
            }),
        ))
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut observed_busy = false;
    while Instant::now() < deadline {
        let events = state.handle_request(DaemonRequest::new(
            RequestId::generate(),
            DaemonRequestKind::SessionStatus(SessionStatusRequest {
                session_id: session_id.clone(),
            }),
        ));
        match &events[0].kind {
            DaemonEventKind::SessionStatus(event) if event.status == SessionStatus::Busy => {
                observed_busy = true;
                break;
            }
            DaemonEventKind::SessionStatus(_) => std::thread::sleep(Duration::from_millis(10)),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    let turn_events = worker.join().unwrap();

    assert!(observed_busy, "status should be readable while turn runs");
    assert!(matches!(
        turn_events.last().map(|event| &event.kind),
        Some(DaemonEventKind::TurnFinished(_))
    ));
}

#[test]
fn transcript_read_omits_content_by_default() {
    let runtime = runtime();
    let session_id = SessionId::generate();
    let run_id = RunId::generate();
    let turn_id = TurnId::generate();
    let attempt_id = AttemptId::generate();
    let user_message_id = MessageId::generate();
    let assistant_message_id = MessageId::generate();
    let mut store = ChatSessionStore::start(
        runtime.paths.sessions_root(),
        session_id.clone(),
        "/tmp/local.toml",
        "test",
    )
    .unwrap();
    store
        .append_user_message(runtime_envelope(
            &session_id,
            &run_id,
            &turn_id,
            None,
            2,
            RuntimeEvent::UserMessage {
                message_id: user_message_id.clone(),
                content: "secret".to_string(),
            },
        ))
        .unwrap();
    store
        .link_attempt(runtime_envelope(
            &session_id,
            &run_id,
            &turn_id,
            Some(&attempt_id),
            7,
            RuntimeEvent::ModelAttemptLinked,
        ))
        .unwrap();
    store
        .append_assistant_message(runtime_envelope(
            &session_id,
            &run_id,
            &turn_id,
            None,
            11,
            RuntimeEvent::AssistantMessage {
                message_id: assistant_message_id.clone(),
                content: "also secret".to_string(),
            },
        ))
        .unwrap();
    let mut state = DaemonState::new(runtime.clone(), InferenceOptions::default());

    let events = state.handle_request(request(DaemonRequestKind::SessionTranscript(
        SessionTranscriptRequest {
            session_id,
            include_content: false,
        },
    )));

    match &events[0].kind {
        DaemonEventKind::SessionTranscript(event) => {
            assert!(!event.content_included);
            assert_eq!(
                event.events,
                vec![
                    TranscriptEvent::UserMessage {
                        run_id: run_id.clone(),
                        turn_id: turn_id.clone(),
                        message_id: user_message_id,
                        content: None,
                    },
                    TranscriptEvent::ModelAttemptLinked {
                        run_id: run_id.clone(),
                        turn_id: turn_id.clone(),
                        attempt_id,
                    },
                    TranscriptEvent::AssistantMessage {
                        run_id,
                        turn_id,
                        message_id: assistant_message_id,
                        content: None,
                    },
                ]
            );
        }
        other => panic!("unexpected event: {other:?}"),
    }
    let _ = std::fs::remove_dir_all(runtime.paths.config_dir.parent().unwrap());
}

#[test]
fn transcript_read_rejects_session_metadata_identity_drift() {
    let runtime = runtime();
    let session_id = SessionId::generate();
    let store = ChatSessionStore::start(
        runtime.paths.sessions_root(),
        session_id.clone(),
        "/tmp/local.toml",
        "test",
    )
    .unwrap();
    let metadata_path = store.session_dir().join("session.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&metadata_path).unwrap()).unwrap();
    metadata["session_id"] = serde_json::json!(SessionId::generate());
    std::fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    let mut state = DaemonState::new(runtime.clone(), InferenceOptions::default());

    let events = state.handle_request(request(DaemonRequestKind::SessionTranscript(
        SessionTranscriptRequest {
            session_id,
            include_content: false,
        },
    )));

    match &events[0].kind {
        DaemonEventKind::Error(error) => {
            assert_eq!(error.code, ProtocolErrorCode::RuntimeFailure);
            assert!(error.message.contains("does not match requested session"));
        }
        other => panic!("unexpected event: {other:?}"),
    }
    let _ = std::fs::remove_dir_all(runtime.paths.config_dir.parent().unwrap());
}

#[test]
fn wrong_schema_returns_protocol_version_error() {
    let mut state = DaemonState::new(runtime(), InferenceOptions::default());
    let mut req = request(DaemonRequestKind::SessionList(SessionListRequest::default()));
    req.schema = "agentlibre.daemon.request.v2".to_string();

    let events = state.handle_request(req);

    match &events[0].kind {
        DaemonEventKind::Error(error) => {
            assert_eq!(error.code, ProtocolErrorCode::UnsupportedProtocolVersion);
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn strict_protocol_decode_rejects_previous_alpha_and_untyped_ids() {
    let request_id = RequestId::generate();
    let session_id = SessionId::generate();
    let previous_alpha = serde_json::json!({
        "schema": "agentlibre.daemon.request.v1alpha",
        "request_id": request_id,
        "kind": "session_turn",
        "payload": {
            "session_id": session_id,
            "text": "hello",
        },
    });
    assert!(serde_json::from_value::<DaemonRequest>(previous_alpha).is_err());

    let untyped = serde_json::json!({
        "schema": REQUEST_SCHEMA,
        "request_id": "req-1",
        "kind": "session_turn",
        "payload": {
            "session_id": "session-1",
            "text": "hello",
        },
    });
    assert!(serde_json::from_value::<DaemonRequest>(untyped).is_err());

    let unknown_payload_field = serde_json::json!({
        "schema": REQUEST_SCHEMA,
        "request_id": RequestId::generate(),
        "kind": "session_list",
        "payload": { "legacy": true },
    });
    assert!(serde_json::from_value::<DaemonRequest>(unknown_payload_field).is_err());
}

#[test]
fn daemon_events_keep_current_schema() {
    let event = DaemonEvent::new(
        None,
        DaemonEventKind::SessionList(SessionListEvent {
            sessions: Vec::new(),
        }),
    );

    assert_eq!(event.schema, EVENT_SCHEMA);
}

#[test]
fn cron_tick_records_due_run_and_notifies_once() {
    let root = std::env::temp_dir().join(format!("agl-daemon-cron-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = AglStore::open_at(&root).unwrap();
    let repo = agl_cron::CronRepository::new(&store);
    let mut draft = CronJobDraft::new(
        "Store status",
        CronTargetKind::Builtin,
        "store-status",
        "hourly",
    );
    draft.notify_ref = Some("matrix-room:!room".to_string());
    let job = repo.add_job(draft).unwrap();
    let mut executor = FakeCronExecutor::default();
    let mut notifier = FakeCronNotifier::default();

    let first = run_cron_tick(&store, 0, &mut executor, &mut notifier).unwrap();
    let second = run_cron_tick(&store, 0, &mut executor, &mut notifier).unwrap();

    assert_eq!(first.due_jobs, 1);
    assert_eq!(first.recorded_runs.len(), 1);
    assert_eq!(first.recorded_runs[0].job_id, job.id);
    assert_eq!(first.recorded_runs[0].status, CronRunStatus::Succeeded);
    assert_eq!(first.notifications, 1);
    assert_eq!(second.recorded_runs[0].id, first.recorded_runs[0].id);
    assert_eq!(second.notifications, 0);
    assert_eq!(executor.executions, 1);
    assert_eq!(notifier.notifications.len(), 1);
    assert_eq!(notifier.notifications[0].notify_ref, "matrix-room:!room");
    assert_eq!(notifier.notifications[0].run_id, first.recorded_runs[0].id);

    std::fs::remove_dir_all(root).unwrap();
}

#[derive(Default)]
struct FakeCronExecutor {
    executions: usize,
}

impl CronTargetExecutor for FakeCronExecutor {
    fn execute(&mut self, job: &CronJob) -> CronExecution {
        self.executions += 1;
        CronExecution::succeeded(format!("fake:{}", job.target_ref))
    }
}

#[derive(Default)]
struct FakeCronNotifier {
    notifications: Vec<CronNotification>,
}

impl CronNotifier for FakeCronNotifier {
    fn notify(&mut self, notification: CronNotification) -> anyhow::Result<()> {
        self.notifications.push(notification);
        Ok(())
    }
}
