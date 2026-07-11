use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use agl_chat::{ChatInferenceJob, InferenceClient, InferenceClientHandle, InferenceOptions};
use agl_config::LocalInferenceConfig;
use agl_cron::{CronJob, CronJobDraft, CronRepository, CronRunStatus, CronTargetKind};
use agl_ids::{RequestId, RunId, SessionId};
use agl_inference::{
    InferenceFinishReason, InferenceResponse, InferenceResponseMetadata, ModelManagerStatus,
};
use agl_protocol::{
    DaemonCapability, DaemonEvent, DaemonEventKind, DaemonRequest, DaemonRequestKind, HelloRequest,
    PROTOCOL_VERSION, ProtocolErrorCode, ProtocolRunState, ProtocolToolMode, RunBudgetRequest,
    RunCancelRequest, RunEventsRequest, RunStatusRequest, RunSubmitRequest, SessionListRequest,
    SessionOpenRequest, SessionStatus, SessionStatusRequest,
};
use agl_runtime::{
    AgentLibreHistoryConfig, AgentLibreLoggingConfig, AgentLibrePaths, AgentLibreRuntimeConfig,
    AgentLibreWorkspaceConfig,
};
use agl_store::RunState;

use super::*;

static TEST_RUNTIME_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TestRuntime {
    runtime: AgentLibreRuntimeConfig,
    inference: InferenceOptions,
}

impl TestRuntime {
    fn new() -> Self {
        let index = TEST_RUNTIME_COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "agl-daemon-test-{}-{}-{index}",
            std::process::id(),
            std::thread::current().name().unwrap_or("main")
        ));
        let paths = AgentLibrePaths::from_agl_home(root.clone());
        std::fs::create_dir_all(&root).unwrap();
        let config = root.join("inference.toml");
        std::fs::write(
            &config,
            format!(
                r#"[backend]
kind = "llama_cpp"
model = "{}"

[runtime]
gpu_layers = 0
context_tokens = 128
threads = 1
batch_size = 16
ubatch_size = 16

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#,
                root.join("unused-test-model.gguf").display()
            ),
        )
        .unwrap();
        Self {
            runtime: AgentLibreRuntimeConfig {
                paths,
                logging: AgentLibreLoggingConfig::from_env(),
                history: AgentLibreHistoryConfig::default(),
                workspace: AgentLibreWorkspaceConfig::default(),
            },
            inference: InferenceOptions {
                config: Some(config),
                ..InferenceOptions::default()
            },
        }
    }
}

impl Drop for TestRuntime {
    fn drop(&mut self) {
        if let Some(root) = self.runtime.paths.config_dir.parent() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

#[derive(Default)]
struct InferenceControl {
    calls: AtomicUsize,
    blocked: AtomicBool,
}

#[derive(Clone)]
struct ControlledInferenceClient {
    control: Arc<InferenceControl>,
}

impl InferenceClient for ControlledInferenceClient {
    fn generate(&self, job: ChatInferenceJob) -> anyhow::Result<InferenceResponse> {
        self.control.calls.fetch_add(1, Ordering::SeqCst);
        while self.control.blocked.load(Ordering::Acquire) && !job.cancellation.is_cancelled() {
            std::thread::sleep(Duration::from_millis(2));
        }
        if job.cancellation.is_cancelled() {
            return Err(agl_inference::ModelManagerError::Cancelled.into());
        }
        Ok(InferenceResponse {
            attempt_id: job.request.attempt_id,
            content: "durable answer\n\nVerification: fake inference.".to_string(),
            finish_reason: InferenceFinishReason::Stop,
            metadata: InferenceResponseMetadata {
                model_state: Some("daemon-test".to_string()),
                selected_device: None,
                duration_ms: 0,
                input_tokens: 4,
                output_tokens: 2,
            },
        })
    }

    fn clear_context(
        &self,
        _config: &LocalInferenceConfig,
        _session_id: &SessionId,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn release_context(
        &self,
        _config: &LocalInferenceConfig,
        _session_id: &SessionId,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn status(&self) -> anyhow::Result<ModelManagerStatus> {
        Ok(ModelManagerStatus::default())
    }
}

fn daemon(test: &TestRuntime, control: Arc<InferenceControl>) -> DaemonState {
    DaemonState::new(
        test.runtime.clone(),
        test.inference.clone(),
        InferenceClientHandle::new(ControlledInferenceClient { control }),
    )
}

fn request(kind: DaemonRequestKind) -> DaemonRequest {
    DaemonRequest::new(RequestId::generate(), kind)
}

fn open_session(state: &mut DaemonState) -> SessionId {
    let event = state.handle_request(request(DaemonRequestKind::SessionOpen(
        SessionOpenRequest {
            session_id: None,
            new_session: true,
            workspace_root: None,
            skills: Vec::new(),
            tool_mode: ProtocolToolMode::ReadOnly,
        },
    )));
    match event.kind {
        DaemonEventKind::SessionOpened(opened) => opened.session_id,
        other => panic!("unexpected open event: {other:?}"),
    }
}

fn submit(
    state: &mut DaemonState,
    session_id: &SessionId,
    text: &str,
    idempotency_key: Option<&str>,
) -> agl_protocol::RunAcceptedEvent {
    let event = state.handle_request(request(DaemonRequestKind::RunSubmit(RunSubmitRequest {
        session_id: session_id.clone(),
        content: agl_content::Content::text(text).unwrap(),
        idempotency_key: idempotency_key.map(str::to_string),
        budget: RunBudgetRequest::default(),
    })));
    match event.kind {
        DaemonEventKind::RunAccepted(accepted) => accepted,
        other => panic!("unexpected admission event: {other:?}"),
    }
}

fn wait_for_calls(control: &InferenceControl, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while control.calls.load(Ordering::Acquire) < expected {
        assert!(Instant::now() < deadline, "inference did not start");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_for_terminal(state: &DaemonState, run_id: &RunId) -> agl_supervisor::RunOutcome {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let outcome = state.run_outcome(run_id.clone()).unwrap();
        if outcome.status.state.is_terminal() {
            return outcome;
        }
        assert!(
            Instant::now() < deadline,
            "run did not reach terminal state"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn hello_declares_strict_run_capabilities() {
    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));

    let event = state.handle_request(request(DaemonRequestKind::Hello(HelloRequest {
        client_name: Some("test".to_string()),
        accepted_protocol_versions: vec![PROTOCOL_VERSION.to_string()],
    })));

    match event.kind {
        DaemonEventKind::Hello(hello) => {
            assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
            assert!(hello.capabilities.contains(&DaemonCapability::RunSubmit));
            assert!(hello.capabilities.contains(&DaemonCapability::RunStatus));
            assert!(hello.capabilities.contains(&DaemonCapability::RunCancel));
            assert!(hello.capabilities.contains(&DaemonCapability::RunReplay));
            assert!(hello.capabilities.contains(&DaemonCapability::RunSubscribe));
        }
        other => panic!("unexpected hello event: {other:?}"),
    }
}

#[test]
fn admission_status_and_cancel_stay_responsive_while_model_blocks() {
    let test = TestRuntime::new();
    let control = Arc::new(InferenceControl::default());
    control.blocked.store(true, Ordering::Release);
    let mut state = daemon(&test, control.clone());
    let session_id = open_session(&mut state);

    let started = Instant::now();
    let accepted = submit(&mut state, &session_id, "block", None);
    assert!(started.elapsed() < Duration::from_millis(250));
    assert_eq!(accepted.state, ProtocolRunState::Queued);
    wait_for_calls(&control, 1);

    let status = state.handle_request(request(DaemonRequestKind::RunStatus(RunStatusRequest {
        run_id: accepted.run_id.clone(),
    })));
    assert!(matches!(
        status.kind,
        DaemonEventKind::RunStatus(ref status) if status.state == ProtocolRunState::Running
    ));

    let cancelled = state.handle_request(request(DaemonRequestKind::RunCancel(RunCancelRequest {
        run_id: accepted.run_id.clone(),
    })));
    assert!(matches!(
        cancelled.kind,
        DaemonEventKind::RunStatus(ref status) if status.cancellation_requested
    ));
    let outcome = wait_for_terminal(&state, &accepted.run_id);
    assert_eq!(outcome.status.state, RunState::Cancelled);
}

#[test]
fn replay_is_contiguous_and_idempotent_admission_returns_original_run() {
    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));
    let session_id = open_session(&mut state);

    let accepted = submit(&mut state, &session_id, "hello", Some("event-1"));
    let replayed = submit(&mut state, &session_id, "hello", Some("event-1"));
    assert_eq!(replayed.run_id, accepted.run_id);
    assert_eq!(replayed.turn_id, accepted.turn_id);
    assert!(replayed.replayed);

    let outcome = wait_for_terminal(&state, &accepted.run_id);
    assert_eq!(outcome.status.state, RunState::Succeeded);
    assert_eq!(outcome.status.usage.model_attempts, 1);
    assert_eq!(outcome.status.usage.model_input_tokens, 4);
    assert_eq!(outcome.status.usage.model_output_tokens, 2);
    assert_eq!(
        outcome.terminal_result.as_ref().unwrap()["status"],
        "answered"
    );

    let replay = state.handle_request(request(DaemonRequestKind::RunEvents(RunEventsRequest {
        run_id: accepted.run_id.clone(),
        after_sequence: 0,
        limit: 1_000,
    })));
    let events = match replay.kind {
        DaemonEventKind::RunEvents(replay) => replay.events,
        other => panic!("unexpected replay event: {other:?}"),
    };
    assert!(!events.is_empty());
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.scope.run_id(), &accepted.run_id);
        assert_eq!(event.scope.session_id(), Some(&session_id));
        assert_eq!(event.scope.turn_id(), Some(&accepted.turn_id));
        assert_eq!(event.sequence, u64::try_from(index).unwrap() + 1);
    }

    let suffix = state.handle_request(request(DaemonRequestKind::RunEvents(RunEventsRequest {
        run_id: accepted.run_id,
        after_sequence: 1,
        limit: 1_000,
    })));
    assert!(matches!(
        suffix.kind,
        DaemonEventKind::RunEvents(ref replay)
            if replay.events.first().is_none_or(|event| event.sequence == 2)
    ));
}

#[test]
fn conflicting_idempotency_fingerprint_fails_without_second_run() {
    let test = TestRuntime::new();
    let control = Arc::new(InferenceControl::default());
    control.blocked.store(true, Ordering::Release);
    let mut state = daemon(&test, control.clone());
    let session_id = open_session(&mut state);
    let accepted = submit(&mut state, &session_id, "first", Some("same-key"));

    let conflict = state.handle_request(request(DaemonRequestKind::RunSubmit(RunSubmitRequest {
        session_id,
        content: agl_content::Content::text("different").unwrap(),
        idempotency_key: Some("same-key".to_string()),
        budget: RunBudgetRequest::default(),
    })));
    assert!(matches!(
        conflict.kind,
        DaemonEventKind::Error(ref error) if error.code == ProtocolErrorCode::InvalidRequest
    ));

    state
        .supervisor_handle()
        .cancel(accepted.run_id.clone())
        .unwrap();
    wait_for_terminal(&state, &accepted.run_id);
}

#[test]
fn turns_for_one_session_execute_in_submission_order() {
    let test = TestRuntime::new();
    let control = Arc::new(InferenceControl::default());
    control.blocked.store(true, Ordering::Release);
    let mut state = daemon(&test, control.clone());
    let session_id = open_session(&mut state);

    let first = submit(&mut state, &session_id, "first", None);
    let second = submit(&mut state, &session_id, "second", None);
    wait_for_calls(&control, 1);
    std::thread::sleep(Duration::from_millis(75));
    assert_eq!(control.calls.load(Ordering::Acquire), 1);

    control.blocked.store(false, Ordering::Release);
    let first_outcome = wait_for_terminal(&state, &first.run_id);
    let second_outcome = wait_for_terminal(&state, &second.run_id);
    assert_eq!(control.calls.load(Ordering::Acquire), 2);
    let first_finished = first_outcome.status.finished_at_ms.unwrap();
    let second_started = second_outcome.status.started_at_ms.unwrap();
    assert!(
        first_finished <= second_started,
        "first finished at {first_finished}, second started at {second_started}"
    );
}

#[test]
fn session_queries_and_unknown_runs_have_typed_responses() {
    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));
    let session_id = open_session(&mut state);

    let status = state.handle_request(request(DaemonRequestKind::SessionStatus(
        SessionStatusRequest {
            session_id: session_id.clone(),
        },
    )));
    assert!(matches!(
        status.kind,
        DaemonEventKind::SessionStatus(ref status)
            if status.session_id == session_id && status.status == SessionStatus::Open
    ));
    let list = state.handle_request(request(DaemonRequestKind::SessionList(
        SessionListRequest::default(),
    )));
    assert!(matches!(
        list.kind,
        DaemonEventKind::SessionList(ref list) if list.sessions.len() == 1
    ));

    let missing = state.handle_request(request(DaemonRequestKind::RunStatus(RunStatusRequest {
        run_id: RunId::generate(),
    })));
    assert!(matches!(
        missing.kind,
        DaemonEventKind::Error(ref error) if error.code == ProtocolErrorCode::NotFound
    ));
}

#[test]
fn daemon_event_constructor_keeps_current_schema() {
    let event = DaemonEvent::new(
        None,
        DaemonEventKind::SessionList(agl_protocol::SessionListEvent {
            sessions: Vec::new(),
        }),
    );
    assert_eq!(event.schema, agl_protocol::EVENT_SCHEMA);
}

#[test]
fn cron_tick_admits_supervised_work_and_notifies_only_after_terminal() {
    let test = TestRuntime::new();
    let control = Arc::new(InferenceControl::default());
    control.blocked.store(true, Ordering::Release);
    let state = SharedDaemonState::new(
        test.runtime.clone(),
        test.inference.clone(),
        InferenceClientHandle::new(ControlledInferenceClient {
            control: control.clone(),
        }),
    );
    let store = agl_store::AglStore::open_current_at(test.runtime.paths.store_root()).unwrap();
    let repository = CronRepository::new(&store);
    let mut draft = CronJobDraft::new(
        "supervised cron",
        CronTargetKind::Skill,
        "repo-status",
        "hourly",
    );
    draft.prompt = Some("Report repository status.".to_string());
    draft.notify_ref = Some("matrix-room:!cron:test".to_string());
    let job = repository.add_job(draft).unwrap();
    let mut executor = SharedCronExecutor {
        state: state.clone(),
    };
    let mut notifier = NoopCronNotifier;

    let started = Instant::now();
    let first = run_cron_tick(&store, 0, &mut executor, &mut notifier).unwrap();
    assert!(started.elapsed() < Duration::from_millis(250));
    assert_eq!(first.recorded_runs[0].status, CronRunStatus::Queued);
    assert_eq!(first.notifications, 0);
    wait_for_calls(&control, 1);

    let second = run_cron_tick(&store, 0, &mut executor, &mut notifier).unwrap();
    assert_eq!(second.recorded_runs[0].id, first.recorded_runs[0].id);
    assert_eq!(control.calls.load(Ordering::Acquire), 1);
    assert!(store.queued_matrix_notifications(10).unwrap().is_empty());

    control.blocked.store(false, Ordering::Release);
    crate::server::link_cron_run(
        &test.runtime.paths.store_root(),
        &state,
        first.recorded_runs[0].clone(),
        job,
    )
    .unwrap();
    let history = repository.history(&first.recorded_runs[0].job_id).unwrap();
    assert_eq!(history[0].status, CronRunStatus::Succeeded);
    assert_eq!(store.queued_matrix_notifications(10).unwrap().len(), 1);
}

struct SharedCronExecutor {
    state: SharedDaemonState,
}

impl CronTargetExecutor for SharedCronExecutor {
    fn execute(&mut self, job: &CronJob, scheduled_for: &str) -> CronExecution {
        match self.state.submit_cron_job(job, scheduled_for) {
            Ok(accepted) => CronExecution::queued(accepted.status.run_id),
            Err(error) => CronExecution::failed(error.message),
        }
    }
}
