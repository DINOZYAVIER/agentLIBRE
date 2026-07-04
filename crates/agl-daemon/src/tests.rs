use agl_chat::InferenceOptions;
use agl_cron::{CronJob, CronJobDraft, CronRunStatus, CronTargetKind};
use agl_protocol::{
    AssistantMessageEvent, DaemonCapability, DaemonEvent, DaemonEventKind, DaemonRequest,
    DaemonRequestKind, EVENT_SCHEMA, HelloRequest, PROTOCOL_VERSION, ProtocolErrorCode,
    SessionListEvent, SessionListRequest, SessionStatus, SessionStatusRequest,
    SessionTranscriptRequest, SessionTurnRequest, TranscriptEvent, TurnTerminalStatus,
};
use agl_runtime::{
    AgentLibreHistoryConfig, AgentLibreLoggingConfig, AgentLibrePaths, AgentLibreRuntimeConfig,
    AgentLibreWorkspaceConfig,
};
use agl_session::{AgentLibreSessionId, ChatSessionEvent};
use agl_store::AglStore;
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
    DaemonRequest::new("req-1", kind)
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

    let events = state.handle_request(request(DaemonRequestKind::SessionStatus(
        SessionStatusRequest {
            session_id: "missing".to_string(),
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
    state.insert_test_session(
        "s1",
        vec![agl_chat::ChatTurnStatus::Answered {
            answer: "hello".to_string(),
        }],
    );

    let first = state.handle_request(request(DaemonRequestKind::SessionTurn(
        SessionTurnRequest {
            session_id: "s1".to_string(),
            text: "say hi".to_string(),
            idempotency_key: Some("matrix-event-1".to_string()),
        },
    )));
    let second = state.handle_request(DaemonRequest::new(
        "req-2",
        DaemonRequestKind::SessionTurn(SessionTurnRequest {
            session_id: "s1".to_string(),
            text: "say hi".to_string(),
            idempotency_key: Some("matrix-event-1".to_string()),
        }),
    ));

    assert_eq!(state.test_session_turns("s1"), 1);
    assert!(matches!(first[0].kind, DaemonEventKind::TurnStarted(_)));
    assert!(matches!(
        second[0].kind,
        DaemonEventKind::AssistantMessage(AssistantMessageEvent { .. })
    ));
    match &second[1].kind {
        DaemonEventKind::TurnFinished(event) => {
            assert_eq!(event.status, TurnTerminalStatus::Answered);
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn session_turn_idempotency_rejects_key_reuse_with_different_text() {
    let mut state = DaemonState::new(runtime(), InferenceOptions::default());
    state.insert_test_session(
        "s1",
        vec![agl_chat::ChatTurnStatus::Answered {
            answer: "hello".to_string(),
        }],
    );

    let _first = state.handle_request(request(DaemonRequestKind::SessionTurn(
        SessionTurnRequest {
            session_id: "s1".to_string(),
            text: "say hi".to_string(),
            idempotency_key: Some("matrix-event-1".to_string()),
        },
    )));
    let conflict = state.handle_request(request(DaemonRequestKind::SessionTurn(
        SessionTurnRequest {
            session_id: "s1".to_string(),
            text: "different".to_string(),
            idempotency_key: Some("matrix-event-1".to_string()),
        },
    )));

    assert_eq!(state.test_session_turns("s1"), 1);
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
    state.insert_test_session("s1", Vec::new());
    state.begin_test_turn_idempotency("s1", "say hi", "matrix-event-1");

    let events = state.handle_request(request(DaemonRequestKind::SessionTurn(
        SessionTurnRequest {
            session_id: "s1".to_string(),
            text: "say hi".to_string(),
            idempotency_key: Some("matrix-event-1".to_string()),
        },
    )));

    assert_eq!(state.test_session_turns("s1"), 0);
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
    state.insert_slow_test_session(
        "s1",
        vec![agl_chat::ChatTurnStatus::Answered {
            answer: "done".to_string(),
        }],
        Duration::from_millis(300),
    );
    let worker_state = state.clone();
    let worker = std::thread::spawn(move || {
        worker_state.handle_request(DaemonRequest::new(
            "turn",
            DaemonRequestKind::SessionTurn(SessionTurnRequest {
                session_id: "s1".to_string(),
                text: "slow".to_string(),
                idempotency_key: None,
            }),
        ))
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut observed_busy = false;
    while Instant::now() < deadline {
        let events = state.handle_request(DaemonRequest::new(
            "status",
            DaemonRequestKind::SessionStatus(SessionStatusRequest {
                session_id: "s1".to_string(),
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
    let session_id = "session-transcript";
    let session_dir = runtime.paths.session_dir(session_id);
    std::fs::create_dir_all(&session_dir).unwrap();
    let transcript = ChatSessionEvent::UserMessage {
        session_id: AgentLibreSessionId::new(session_id).unwrap(),
        message_id: agl_session::AgentLibreMessageId::indexed(1),
        content: "secret".to_string(),
    };
    std::fs::write(
        session_dir.join("transcript.jsonl"),
        format!("{}\n", serde_json::to_string(&transcript).unwrap()),
    )
    .unwrap();
    let mut state = DaemonState::new(runtime.clone(), InferenceOptions::default());

    let events = state.handle_request(request(DaemonRequestKind::SessionTranscript(
        SessionTranscriptRequest {
            session_id: session_id.to_string(),
            include_content: false,
        },
    )));

    match &events[0].kind {
        DaemonEventKind::SessionTranscript(event) => {
            assert!(!event.content_included);
            assert_eq!(
                event.events,
                vec![TranscriptEvent::UserMessage {
                    message_id: "message-0001".to_string(),
                    content: None,
                }]
            );
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
