use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agl_chat::{ChatInferenceJob, InferenceClient, InferenceClientHandle, InferenceOptions};
use agl_config::ResolvedInferenceConfig;
use agl_cron::{CronJob, CronJobDraft, CronRepository, CronRunStatus, CronTargetKind};
use agl_ids::{ExecutionId, RequestId, RunId, SessionId};
use agl_inference::{
    InferenceFinishReason, InferenceResponse, InferenceResponseMetadata, ModelManagerStatus,
};
use agl_protocol::{
    DaemonCapability, DaemonEvent, DaemonEventKind, DaemonRequest, DaemonRequestKind,
    ExecutionListRequest, ExecutionReadRequest, ExecutionStatusRequest, HelloRequest,
    PROTOCOL_VERSION, ProtocolErrorCode, ProtocolRunKind, ProtocolRunState, ProtocolToolMode,
    RunBudgetRequest, RunCancelRequest, RunEventsRequest, RunStatusRequest, RunSubmitRequest,
    RunTreeRequest, SessionFinishReason, SessionFinishRequest, SessionListRequest,
    SessionOpenRequest, SessionStatus, SessionStatusRequest,
};
use agl_runtime::{
    AgentLibreHistoryConfig, AgentLibreLoggingConfig, AgentLibrePaths, AgentLibreRuntimeConfig,
    AgentLibreWorkspaceConfig,
};
use agl_store::RunState;
use anyhow::Context;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

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
                execution: agl_runtime::AgentLibreExecutionConfig::default(),
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
    requests: Mutex<Vec<agl_inference::InferenceRequest>>,
}

#[derive(Clone)]
struct ControlledInferenceClient {
    control: Arc<InferenceControl>,
}

struct ScriptedDelegationClient {
    responses: Mutex<VecDeque<String>>,
}

impl InferenceClient for ScriptedDelegationClient {
    fn generate(&self, job: ChatInferenceJob) -> anyhow::Result<InferenceResponse> {
        let content = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .context("daemon delegation response queue is empty")?;
        Ok(InferenceResponse {
            attempt_id: job.request.attempt_id,
            content,
            finish_reason: InferenceFinishReason::Stop,
            metadata: InferenceResponseMetadata {
                model_state: Some("daemon-delegation".to_string()),
                selected_device: None,
                duration_ms: 1,
                input_tokens: 4,
                output_tokens: 2,
            },
        })
    }

    fn clear_context(
        &self,
        _config: &ResolvedInferenceConfig,
        _session_id: &SessionId,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn release_context(
        &self,
        _config: &ResolvedInferenceConfig,
        _session_id: &SessionId,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn status(&self) -> anyhow::Result<ModelManagerStatus> {
        Ok(ModelManagerStatus::default())
    }
}

impl InferenceClient for ControlledInferenceClient {
    fn generate(&self, job: ChatInferenceJob) -> anyhow::Result<InferenceResponse> {
        self.control
            .requests
            .lock()
            .unwrap()
            .push(job.request.clone());
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
        _config: &ResolvedInferenceConfig,
        _session_id: &SessionId,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn release_context(
        &self,
        _config: &ResolvedInferenceConfig,
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
            function_ref: None,
            skills: Vec::new(),
            tool_mode: ProtocolToolMode::ReadOnly,
        },
    )));
    match event.kind {
        DaemonEventKind::SessionOpened(opened) => opened.session_id,
        other => panic!("unexpected open event: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aborted_requests_remain_charged_to_the_bounded_daemon_bridge() {
    let test = TestRuntime::new();
    let state = SharedDaemonState::new(
        test.runtime.clone(),
        test.inference.clone(),
        InferenceClientHandle::new(ControlledInferenceClient {
            control: Arc::new(InferenceControl::default()),
        }),
    );
    let (entered_sender, entered_receiver) = std::sync::mpsc::sync_channel(1);
    let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(1);
    let state_owner = Arc::clone(&state.inner);
    let blocker = std::thread::spawn(move || {
        state_owner
            .call(agl_app::ApplicationCallContext::new(), move |_, _| {
                entered_sender.send(()).unwrap();
                let _ = release_receiver.recv();
            })
            .unwrap();
    });
    entered_receiver.recv().unwrap();

    let mut requests = Vec::new();
    for _ in 0..32 {
        let state = state.clone();
        requests.push(tokio::spawn(async move {
            state
                .handle_request_async(request(DaemonRequestKind::Hello(HelloRequest {
                    client_name: Some("bounded-bridge-test".to_owned()),
                    accepted_protocol_versions: vec![PROTOCOL_VERSION.to_owned()],
                })))
                .await
        }));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.available_blocking_permits() != 0 || state.inner.pending_operations() != 32 {
        assert!(
            Instant::now() < deadline,
            "all bounded bridge permits and state queue slots must be charged before abort"
        );
        tokio::task::yield_now().await;
    }
    for request in &requests {
        request.abort();
    }
    for request in requests {
        let _ = request.await;
    }
    assert_eq!(
        state.available_blocking_permits(),
        0,
        "aborting awaiters must not release permits held by detached blocking closures"
    );

    let overflow = state
        .handle_request_async(request(DaemonRequestKind::Hello(HelloRequest {
            client_name: Some("bounded-bridge-overflow-test".to_owned()),
            accepted_protocol_versions: vec![PROTOCOL_VERSION.to_owned()],
        })))
        .await;
    assert!(matches!(
        overflow.kind,
        DaemonEventKind::Error(ref error) if error.code == ProtocolErrorCode::InputBackpressure
    ));

    release_sender.send(()).unwrap();
    blocker.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.available_blocking_permits() != 32 {
        assert!(
            Instant::now() < deadline,
            "cancelled queued calls must drain without executing owner work"
        );
        tokio::task::yield_now().await;
    }
}

#[test]
fn resume_of_an_already_loaded_session_is_idempotent() {
    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));
    let session_id = open_session(&mut state);

    let event = state.handle_request(request(DaemonRequestKind::SessionOpen(
        SessionOpenRequest {
            session_id: Some(session_id.clone()),
            new_session: false,
            workspace_root: None,
            function_ref: None,
            skills: Vec::new(),
            tool_mode: ProtocolToolMode::ReadOnly,
        },
    )));

    match event.kind {
        DaemonEventKind::SessionOpened(opened) => {
            assert_eq!(opened.session_id, session_id);
            assert!(opened.resumed);
        }
        other => panic!("unexpected resume event: {other:?}"),
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
        client_submission_id: idempotency_key
            .map(str::to_string)
            .unwrap_or_else(|| RequestId::generate().to_string()),
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
            assert!(hello.capabilities.contains(&DaemonCapability::RunTree));
            assert!(hello.capabilities.contains(&DaemonCapability::RunCancel));
            assert!(hello.capabilities.contains(&DaemonCapability::RunReplay));
            assert!(hello.capabilities.contains(&DaemonCapability::RunSubscribe));
        }
        other => panic!("unexpected hello event: {other:?}"),
    }
}

#[test]
fn daemon_delegation_uses_the_same_durable_child_path() {
    let mut test = TestRuntime::new();
    let workspace = test
        .runtime
        .paths
        .config_dir
        .parent()
        .unwrap()
        .join("delegation-workspace");
    let function_root = workspace.join(".agl/functions/coordinator");
    std::fs::create_dir_all(function_root.join("subagents")).unwrap();
    std::fs::write(
        function_root.join("FUNCTION.md"),
        r#"---
schema: agentfunction/v1
id: coordinator
title: Coordinator
subagents:
  use:
    - reviewer
delegation:
  max_depth: 2
  max_children_per_run: 2
  max_descendants: 4
  max_total_output_tokens: 512
  timeout_seconds: 30
---
"#,
    )
    .unwrap();
    std::fs::write(function_root.join("SYSTEM.md"), "Delegate the review.\n").unwrap();
    std::fs::write(
        function_root.join("subagents/reviewer.md"),
        r#"---
schema: agentlibre/subagent/v1
id: reviewer
title: Reviewer
description: Reviews one daemon task.
model:
  inherit: true
tools:
  mode: read-only
  allow: []
  deny: []
subagents:
  use: []
limits:
  max_model_attempts: 2
  max_output_tokens: 64
  max_capability_calls: 2
  timeout_seconds: 20
---

Return the daemon child verdict.
"#,
    )
    .unwrap();
    test.inference.function_ref = Some("coordinator".to_string());
    test.inference.workspace_root = Some(workspace.clone());
    let mut state = DaemonState::new(
        test.runtime.clone(),
        test.inference.clone(),
        InferenceClientHandle::new(ScriptedDelegationClient {
            responses: Mutex::new(VecDeque::from([
                r#"<tool_call>{"name":"agent.delegate","arguments":{"subagent_id":"reviewer","task":"Review daemon patch"}}</tool_call>"#.to_string(),
                "Daemon child verdict".to_string(),
                "Daemon parent answer".to_string(),
            ])),
        }),
    );
    let opened = state.handle_request(request(DaemonRequestKind::SessionOpen(
        SessionOpenRequest {
            session_id: None,
            new_session: true,
            workspace_root: Some(workspace.display().to_string()),
            function_ref: None,
            skills: Vec::new(),
            tool_mode: ProtocolToolMode::Execute,
        },
    )));
    let session_id = match opened.kind {
        DaemonEventKind::SessionOpened(opened) => opened.session_id,
        other => panic!("unexpected session event: {other:?}"),
    };
    let accepted = submit(&mut state, &session_id, "Coordinate daemon review", None);
    let outcome = wait_for_terminal(&state, &accepted.run_id);
    assert_eq!(outcome.status.state, RunState::Succeeded);

    let tree = state.handle_request(request(DaemonRequestKind::RunTree(RunTreeRequest {
        run_id: accepted.run_id.clone(),
    })));
    let child_run_id = match tree.kind {
        DaemonEventKind::RunTree(tree) => {
            assert_eq!(tree.runs.len(), 2);
            assert_eq!(tree.runs[1].run_kind, ProtocolRunKind::Subagent);
            assert_eq!(tree.runs[1].parent_run_id, Some(accepted.run_id));
            assert!(tree.runs[1].result_delivered);
            tree.runs[1].run_id.clone()
        }
        other => panic!("unexpected tree event: {other:?}"),
    };
    let child_status =
        state.handle_request(request(DaemonRequestKind::RunStatus(RunStatusRequest {
            run_id: child_run_id,
        })));
    assert!(matches!(
        child_status.kind,
        DaemonEventKind::RunStatus(ref status)
            if status.run_kind == ProtocolRunKind::Subagent
                && status.terminal_result.is_none()
                && status.error_message.is_none()
    ));
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
    let admission_elapsed = started.elapsed();
    assert!(
        admission_elapsed < Duration::from_millis(250),
        "cron admission took {admission_elapsed:?}"
    );
    assert_eq!(accepted.state, ProtocolRunState::Queued);
    wait_for_calls(&control, 1);

    let status = state.handle_request(request(DaemonRequestKind::RunStatus(RunStatusRequest {
        run_id: accepted.run_id.clone(),
    })));
    assert!(matches!(
        status.kind,
        DaemonEventKind::RunStatus(ref status) if status.state == ProtocolRunState::Running
    ));
    let tree = state.handle_request(request(DaemonRequestKind::RunTree(RunTreeRequest {
        run_id: accepted.run_id.clone(),
    })));
    assert!(matches!(
        tree.kind,
        DaemonEventKind::RunTree(ref tree)
            if tree.requested_run_id == accepted.run_id && tree.runs.len() == 1
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
        client_submission_id: "same-key".to_string(),
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
fn confirmed_session_exit_cancels_active_and_queued_roots_before_finish() {
    let test = TestRuntime::new();
    let control = Arc::new(InferenceControl::default());
    control.blocked.store(true, Ordering::Release);
    let mut state = daemon(&test, control.clone());
    let session_id = open_session(&mut state);
    let first = submit(&mut state, &session_id, "active", None);
    let second = submit(&mut state, &session_id, "queued", None);
    wait_for_calls(&control, 1);

    let confirmation = state
        .application_invoke(agl_app::ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: "exit-without-confirmation".to_owned(),
            action: agl_app::ApplicationAction::SessionExit {
                confirm_active: false,
            },
        })
        .unwrap_err();
    assert_eq!(
        confirmation.code,
        agl_app::ApplicationErrorCode::SessionBusy
    );
    assert!(
        confirmation
            .message
            .contains("2 active or queued root run(s)")
    );
    assert!(
        !state
            .supervisor_handle()
            .status(first.run_id.clone())
            .unwrap()
            .unwrap()
            .cancellation_requested
    );
    assert!(
        !state
            .supervisor_handle()
            .status(second.run_id.clone())
            .unwrap()
            .unwrap()
            .cancellation_requested
    );

    let exited = state
        .application_invoke(agl_app::ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: "confirmed-exit".to_owned(),
            action: agl_app::ApplicationAction::SessionExit {
                confirm_active: true,
            },
        })
        .unwrap();
    assert!(matches!(
        exited,
        agl_app::ApplicationActionResult::SessionExited {
            ref session_id,
            cancelled_runs: 2,
            terminated_terminals: 0,
            terminated_executions: 0,
        } if session_id == &first.session_id
    ));
    assert_eq!(
        wait_for_terminal(&state, &first.run_id).status.state,
        RunState::Cancelled
    );
    assert_eq!(
        wait_for_terminal(&state, &second.run_id).status.state,
        RunState::Cancelled
    );
    assert_eq!(control.calls.load(Ordering::Acquire), 1);
    let status = state.handle_request(request(DaemonRequestKind::SessionStatus(
        SessionStatusRequest { session_id },
    )));
    assert!(matches!(
        status.kind,
        DaemonEventKind::SessionStatus(ref status) if status.status == SessionStatus::Finished
    ));
}

#[test]
fn generic_session_finish_uses_the_same_cancel_and_wait_boundary() {
    let test = TestRuntime::new();
    let control = Arc::new(InferenceControl::default());
    control.blocked.store(true, Ordering::Release);
    let mut state = daemon(&test, control.clone());
    let session_id = open_session(&mut state);
    let first = submit(&mut state, &session_id, "active", None);
    let second = submit(&mut state, &session_id, "queued", None);
    wait_for_calls(&control, 1);

    let finished = state.handle_request(request(DaemonRequestKind::SessionFinish(
        SessionFinishRequest {
            session_id: session_id.clone(),
            reason: SessionFinishReason::ExitCommand,
        },
    )));
    assert!(matches!(
        finished.kind,
        DaemonEventKind::SessionFinished(ref event) if event.session_id == session_id
    ));
    assert_eq!(
        wait_for_terminal(&state, &first.run_id).status.state,
        RunState::Cancelled
    );
    assert_eq!(
        wait_for_terminal(&state, &second.run_id).status.state,
        RunState::Cancelled
    );
    assert_eq!(control.calls.load(Ordering::Acquire), 1);
}

#[test]
fn session_exit_does_not_count_an_already_terminal_root_as_cancelled() {
    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));
    let session_id = open_session(&mut state);
    let completed = submit(&mut state, &session_id, "complete first", None);
    assert_eq!(
        wait_for_terminal(&state, &completed.run_id).status.state,
        RunState::Succeeded
    );

    let exited = state
        .application_invoke(agl_app::ApplicationActionRequest {
            session_id: Some(session_id),
            client_submission_id: "idle-exit".to_owned(),
            action: agl_app::ApplicationAction::SessionExit {
                confirm_active: false,
            },
        })
        .unwrap();
    assert!(matches!(
        exited,
        agl_app::ApplicationActionResult::SessionExited {
            cancelled_runs: 0,
            terminated_terminals: 0,
            terminated_executions: 0,
            ..
        }
    ));
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
fn execution_operator_queries_have_typed_empty_not_found_and_bound_errors() {
    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));
    let missing = ExecutionId::generate();

    let list = state.handle_request(request(DaemonRequestKind::ExecutionList(
        ExecutionListRequest::default(),
    )));
    assert!(matches!(
        list.kind,
        DaemonEventKind::ExecutionList(ref event) if event.executions.is_empty()
    ));

    let status = state.handle_request(request(DaemonRequestKind::ExecutionStatus(
        ExecutionStatusRequest {
            execution_id: missing.clone(),
            include_private_command: true,
        },
    )));
    assert!(matches!(
        status.kind,
        DaemonEventKind::Error(ref error) if error.code == ProtocolErrorCode::NotFound
    ));

    let read = state.handle_request(request(DaemonRequestKind::ExecutionRead(
        ExecutionReadRequest {
            execution_id: missing,
            after_sequence: 0,
            max_bytes: 0,
        },
    )));
    assert!(matches!(
        read.kind,
        DaemonEventKind::Error(ref error) if error.code == ProtocolErrorCode::InvalidRequest
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
fn human_host_terminal_without_lifetime_grant_fails_closed() {
    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));
    let session_id = open_session(&mut state);
    let revision = state
        .application_snapshot(&session_id)
        .unwrap()
        .header
        .execution_context_revision;

    let error = state
        .application_ensure_human_terminal(agl_app::HumanTerminalEnsure {
            session_id,
            client_submission_id: "host-without-grant".to_owned(),
            execution_context_revision: revision,
            profile: agl_process::ExecutionProfile::Host,
            shell_profile_id: "bash-managed".to_owned(),
            terminal_size: agl_process::TerminalSize::default(),
            agl_env: agl_app::StructuredEnvironmentOverlay::default(),
            host_startup: agl_app::HostStartupPolicy::ManagedOnly,
        })
        .unwrap_err();

    assert_eq!(
        error.code,
        agl_app::ApplicationErrorCode::AuthorizationRequired
    );
}

fn human_host_terminal_request(
    session_id: SessionId,
    revision: u64,
    submission_id: &str,
) -> agl_app::HumanTerminalEnsure {
    agl_app::HumanTerminalEnsure {
        session_id,
        client_submission_id: submission_id.to_owned(),
        execution_context_revision: revision,
        profile: agl_process::ExecutionProfile::Host,
        shell_profile_id: "bash-managed".to_owned(),
        terminal_size: agl_process::TerminalSize::default(),
        agl_env: agl_app::StructuredEnvironmentOverlay::default(),
        host_startup: agl_app::HostStartupPolicy::ManagedOnly,
    }
}

#[test]
fn local_operator_host_authority_is_isolated_idempotent_and_reconciliation_safe() {
    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));
    let session_id = open_session(&mut state);
    let before = state.application_snapshot(&session_id).unwrap();
    let host_request = human_host_terminal_request(
        session_id.clone(),
        before.header.execution_context_revision,
        "operator-host-1",
    );
    let operator_uid = unsafe { libc::geteuid() };

    let wrong_uid = if operator_uid == u32::MAX {
        0
    } else {
        operator_uid + 1
    };
    let error = state
        .operator_ensure_human_host_terminal(host_request.clone(), wrong_uid, true)
        .unwrap_err();
    assert_eq!(error.code, agl_app::ApplicationErrorCode::NotAuthorized);

    let error = state
        .operator_ensure_human_host_terminal(host_request.clone(), operator_uid, false)
        .unwrap_err();
    assert_eq!(
        error.code,
        agl_app::ApplicationErrorCode::ConfirmationRequired
    );

    let ensured = state
        .operator_ensure_human_host_terminal(host_request.clone(), operator_uid, true)
        .unwrap();
    assert_eq!(
        ensured.disposition,
        agl_app::TerminalEnsureDisposition::Created
    );
    assert_eq!(
        ensured.terminal.profile,
        agl_process::ExecutionProfile::Host
    );

    let generic_error = state
        .application_ensure_human_terminal(host_request.clone())
        .unwrap_err();
    assert_eq!(
        generic_error.code,
        agl_app::ApplicationErrorCode::AuthorizationRequired
    );

    let process = state.process_handle();
    assert_eq!(
        process.terminate_inactive_grants(BTreeSet::new()).unwrap(),
        0,
        "capability reconciliation must not consume local-operator authority"
    );
    assert!(
        process
            .operator_status(&ensured.terminal.execution_id)
            .unwrap()
            .state
            .is_live()
    );

    let exact_replay = state
        .operator_ensure_human_host_terminal(host_request.clone(), operator_uid, true)
        .unwrap();
    assert_eq!(
        exact_replay.terminal.execution_id,
        ensured.terminal.execution_id
    );
    assert_eq!(
        exact_replay.disposition,
        agl_app::TerminalEnsureDisposition::Reused
    );

    let mut distinct = host_request.clone();
    distinct.client_submission_id = "operator-host-2".to_owned();
    distinct.terminal_size = agl_process::TerminalSize {
        columns: 173,
        rows: 51,
    };
    let distinct_replay = state
        .operator_ensure_human_host_terminal(distinct, operator_uid, true)
        .unwrap();
    assert_eq!(
        distinct_replay.terminal.execution_id,
        ensured.terminal.execution_id
    );
    assert_eq!(
        distinct_replay.disposition,
        agl_app::TerminalEnsureDisposition::Reused
    );

    let mut conflict = host_request.clone();
    conflict.client_submission_id = "operator-host-conflict".to_owned();
    conflict
        .agl_env
        .values
        .insert("AGL_HOST_TEST".to_owned(), "changed".to_owned());
    let error = state
        .operator_ensure_human_host_terminal(conflict, operator_uid, true)
        .unwrap_err();
    assert_eq!(error.code, agl_app::ApplicationErrorCode::InvalidArguments);

    let mut shell_conflict = host_request.clone();
    shell_conflict.client_submission_id = "operator-host-shell-conflict".to_owned();
    shell_conflict.shell_profile_id = "zsh-managed".to_owned();
    let error = state
        .operator_ensure_human_host_terminal(shell_conflict, operator_uid, true)
        .unwrap_err();
    assert_eq!(error.code, agl_app::ApplicationErrorCode::InvalidArguments);

    let mut startup_conflict = host_request;
    startup_conflict.client_submission_id = "operator-host-startup-conflict".to_owned();
    startup_conflict.host_startup = agl_app::HostStartupPolicy::SourceUserRc;
    let error = state
        .operator_ensure_human_host_terminal(startup_conflict, operator_uid, true)
        .unwrap_err();
    assert_eq!(error.code, agl_app::ApplicationErrorCode::InvalidArguments);

    let after = state.application_snapshot(&session_id).unwrap();
    assert_eq!(after.header.operation_mode, before.header.operation_mode);
    assert!(!after.command_context.host_shell_available);
    assert!(after.terminals.iter().any(|terminal| {
        terminal.execution_id == ensured.terminal.execution_id
            && terminal.profile == agl_process::ExecutionProfile::Host
    }));

    let finished = state.handle_request(request(DaemonRequestKind::SessionFinish(
        SessionFinishRequest {
            session_id,
            reason: SessionFinishReason::ExitCommand,
        },
    )));
    assert!(matches!(finished.kind, DaemonEventKind::SessionFinished(_)));
    assert!(
        process
            .operator_status(&ensured.terminal.execution_id)
            .unwrap()
            .state
            .is_terminal()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn human_host_terminal_survives_disconnect_and_finish_ends_its_authority() {
    let test = TestRuntime::new();
    let state = SharedDaemonState::new(
        test.runtime.clone(),
        test.inference.clone(),
        InferenceClientHandle::new(ControlledInferenceClient {
            control: Arc::new(InferenceControl::default()),
        }),
    );
    let application = state.application();
    let opened = application
        .open_session(agl_app::SessionOpen {
            launch: agl_app::SessionLaunchOptions {
                workspace_root: None,
                function_ref: None,
                model_id: None,
                operation_mode: Some(agl_chat::ToolAccessMode::ReadOnly),
                skill_ids: Vec::new(),
            },
        })
        .await
        .unwrap();

    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let server_state = state.clone();
    let server = tokio::spawn(async move {
        crate::server::serve_authenticated_test_connection(server_stream, &server_state).await
    });
    let (client_reader, mut client_writer) = tokio::io::split(client_stream);
    let mut client_reader = BufReader::new(client_reader);
    let hello_request =
        serde_json::to_string(&request(DaemonRequestKind::Hello(HelloRequest::default()))).unwrap();
    client_writer
        .write_all(format!("{hello_request}\n").as_bytes())
        .await
        .unwrap();
    client_writer.flush().await.unwrap();
    let mut hello_line = String::new();
    client_reader.read_line(&mut hello_line).await.unwrap();
    assert!(
        !hello_line.is_empty(),
        "authenticated connection closed during hello"
    );
    let hello: DaemonEvent = serde_json::from_str(&hello_line).unwrap();
    assert!(matches!(hello.kind, DaemonEventKind::Hello(_)));
    let host_request = serde_json::to_string(&request(DaemonRequestKind::HumanHostTerminalEnsure(
        agl_protocol::HumanHostTerminalEnsureRequest {
            terminal: agl_protocol::HumanTerminalEnsureRequest {
                session_id: opened.session_id.clone(),
                client_submission_id: "host-disconnect".to_owned(),
                execution_context_revision: opened.snapshot.header.execution_context_revision,
                profile: agl_process::ExecutionProfile::Host,
                shell_profile_id: "bash-managed".to_owned(),
                terminal_size: agl_process::TerminalSize::default(),
                agl_env: agl_protocol::StructuredEnvironmentOverlay::default(),
                host_startup: agl_protocol::HostStartupPolicy::ManagedOnly,
            },
            confirm_host_authority: true,
        },
    )))
    .unwrap();
    client_writer
        .write_all(format!("{host_request}\n").as_bytes())
        .await
        .unwrap();
    client_writer.flush().await.unwrap();
    let mut ensured_line = String::new();
    client_reader.read_line(&mut ensured_line).await.unwrap();
    let ensured: DaemonEvent = serde_json::from_str(&ensured_line).unwrap();
    let ensured = match ensured.kind {
        DaemonEventKind::HumanTerminalEnsured(event) => event,
        other => panic!("unexpected Host ensure event: {other:?}"),
    };
    drop(client_reader);
    drop(client_writer);
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let process = state.process_handle().unwrap();
    assert!(
        process
            .operator_status(&ensured.terminal.execution_id)
            .unwrap()
            .state
            .is_live(),
        "disconnect must release only the transport attachment"
    );
    let snapshot = application.snapshot(&opened.session_id).await.unwrap();
    assert!(snapshot.terminals.iter().any(|terminal| {
        terminal.execution_id == ensured.terminal.execution_id
            && terminal.profile == agl_process::ExecutionProfile::Host
    }));

    let finished = state.handle_request(request(DaemonRequestKind::SessionFinish(
        SessionFinishRequest {
            session_id: opened.session_id,
            reason: SessionFinishReason::ExitCommand,
        },
    )));
    assert!(matches!(finished.kind, DaemonEventKind::SessionFinished(_)));
    assert!(
        process
            .operator_status(&ensured.terminal.execution_id)
            .unwrap()
            .state
            .is_terminal(),
        "finish must terminate the terminal-scoped local authority"
    );
}

#[test]
fn human_terminal_rejects_a_stale_execution_context_before_launch() {
    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));
    let session_id = open_session(&mut state);
    let revision = state
        .application_snapshot(&session_id)
        .unwrap()
        .header
        .execution_context_revision;

    let error = state
        .application_ensure_human_terminal(agl_app::HumanTerminalEnsure {
            session_id,
            client_submission_id: "stale-terminal".to_owned(),
            execution_context_revision: revision + 1,
            profile: agl_process::ExecutionProfile::Workspace,
            shell_profile_id: "bash-managed".to_owned(),
            terminal_size: agl_process::TerminalSize::default(),
            agl_env: agl_app::StructuredEnvironmentOverlay::default(),
            host_startup: agl_app::HostStartupPolicy::ManagedOnly,
        })
        .unwrap_err();

    assert_eq!(
        error.code,
        agl_app::ApplicationErrorCode::StaleContextRevision
    );
}

#[test]
fn invalid_workspace_target_does_not_terminate_the_existing_terminal() {
    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));
    let session_id = open_session(&mut state);
    let revision = state
        .application_snapshot(&session_id)
        .unwrap()
        .header
        .execution_context_revision;
    let ensured = state
        .application_ensure_human_terminal(agl_app::HumanTerminalEnsure {
            session_id: session_id.clone(),
            client_submission_id: "workspace-preflight-terminal".to_owned(),
            execution_context_revision: revision,
            profile: agl_process::ExecutionProfile::Workspace,
            shell_profile_id: "bash-managed".to_owned(),
            terminal_size: agl_process::TerminalSize::default(),
            agl_env: agl_app::StructuredEnvironmentOverlay::default(),
            host_startup: agl_app::HostStartupPolicy::ManagedOnly,
        })
        .unwrap();
    let execution_id = ensured.terminal.execution_id.clone();

    let error = state
        .application_invoke(agl_app::ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: "invalid-workspace-target".to_owned(),
            action: agl_app::ApplicationAction::WorkspaceSet {
                path: test
                    .runtime
                    .paths
                    .state_dir
                    .join("missing-workspace")
                    .to_string_lossy()
                    .into_owned(),
                confirm_terminate_terminals: true,
            },
        })
        .unwrap_err();
    assert_eq!(error.code, agl_app::ApplicationErrorCode::InvalidArguments);
    assert!(
        state
            .process_handle()
            .operator_status(&execution_id)
            .unwrap()
            .state
            .is_live()
    );
    assert!(
        state
            .application_snapshot(&session_id)
            .unwrap()
            .terminals
            .iter()
            .any(|terminal| terminal.execution_id == execution_id)
    );
}

#[test]
fn human_terminal_secret_references_require_a_private_resolver() {
    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));
    let session_id = open_session(&mut state);
    let revision = state
        .application_snapshot(&session_id)
        .unwrap()
        .header
        .execution_context_revision;

    let error = state
        .application_ensure_human_terminal(agl_app::HumanTerminalEnsure {
            session_id,
            client_submission_id: "secret-without-resolver".to_owned(),
            execution_context_revision: revision,
            profile: agl_process::ExecutionProfile::Workspace,
            shell_profile_id: "bash-managed".to_owned(),
            terminal_size: agl_process::TerminalSize::default(),
            agl_env: agl_app::StructuredEnvironmentOverlay {
                values: Default::default(),
                inherited_names: Vec::new(),
                secret_refs: vec![agl_app::SecretEnvironmentReference {
                    name: "TOKEN".to_owned(),
                    reference_id: "vault:test-token".to_owned(),
                }],
            },
            host_startup: agl_app::HostStartupPolicy::ManagedOnly,
        })
        .unwrap_err();

    assert_eq!(error.code, agl_app::ApplicationErrorCode::InvalidArguments);
    assert!(error.message.contains("private resolver"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn human_terminal_monitor_reuses_one_reader_and_syncs_private_boundaries() {
    let test = TestRuntime::new();
    assert!(
        test.runtime.execution.runtime_read_only_roots.is_empty(),
        "the default-config test must rely only on standard Linux runtime roots"
    );
    let workspace = test
        .runtime
        .paths
        .config_dir
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let child = workspace.join("monitor-child");
    std::fs::create_dir(&child).unwrap();

    let control = Arc::new(InferenceControl::default());
    let state = SharedDaemonState::new(
        test.runtime.clone(),
        test.inference.clone(),
        InferenceClientHandle::new(ControlledInferenceClient {
            control: Arc::clone(&control),
        }),
    );
    let application = state.application();
    let opened = application
        .open_session(agl_app::SessionOpen {
            launch: agl_app::SessionLaunchOptions {
                workspace_root: Some(workspace.to_string_lossy().into_owned()),
                function_ref: None,
                model_id: None,
                operation_mode: Some(agl_chat::ToolAccessMode::ReadOnly),
                skill_ids: Vec::new(),
            },
        })
        .await
        .unwrap();
    let mut subscription = application
        .subscribe(agl_app::PresentationSubscribe {
            session_id: opened.session_id.clone(),
        })
        .await
        .unwrap();
    let request = agl_app::HumanTerminalEnsure {
        session_id: opened.session_id.clone(),
        client_submission_id: "monitor-idempotent".to_owned(),
        execution_context_revision: opened.snapshot.header.execution_context_revision,
        profile: agl_process::ExecutionProfile::Workspace,
        shell_profile_id: "bash-managed".to_owned(),
        terminal_size: agl_process::TerminalSize::default(),
        agl_env: agl_app::StructuredEnvironmentOverlay::default(),
        host_startup: agl_app::HostStartupPolicy::ManagedOnly,
    };
    let ensured = application
        .ensure_human_terminal(request.clone())
        .await
        .unwrap();
    let replayed = application
        .ensure_human_terminal(request.clone())
        .await
        .unwrap();
    assert_eq!(replayed.terminal.terminal_id, ensured.terminal.terminal_id);
    assert_eq!(
        state
            .inner
            .call(agl_app::ApplicationCallContext::new(), |state, _| {
                state.monitored_terminal_count()
            })
            .unwrap(),
        1,
        "idempotent ensure must not start a second private reader"
    );

    let process = state
        .inner
        .call(agl_app::ApplicationCallContext::new(), |state, _| {
            state.process_handle()
        })
        .unwrap();
    let ready_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = application.snapshot(&opened.session_id).await.unwrap();
        if snapshot.terminals.iter().any(|terminal| {
            terminal.terminal_id == ensured.terminal.terminal_id
                && terminal.prompt_state == agl_app::TerminalPromptState::Ready
        }) {
            break;
        }
        if Instant::now() >= ready_deadline {
            let terminal_output = process
                .operator_read(
                    &ensured.terminal.execution_id,
                    agl_process::ExecutionCursor { after_sequence: 0 },
                    64 * 1024,
                )
                .unwrap();
            panic!(
                "terminal prompt did not become trusted: {:?}; PTY output: {:?}",
                snapshot.terminals, terminal_output.chunks
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let lease = process
        .operator_attach(&ensured.terminal.execution_id, RequestId::generate(), true)
        .unwrap();
    process
        .operator_write(
            &ensured.terminal.execution_id,
            lease.clone(),
            agl_process::ProcessBytes::from_bytes(b"command -v ls\n"),
            false,
        )
        .unwrap();

    let path_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = application.snapshot(&opened.session_id).await.unwrap();
        if snapshot.terminals.iter().any(|terminal| {
            terminal.terminal_id == ensured.terminal.terminal_id
                && terminal.command_sequence == 1
                && terminal.prompt_state == agl_app::TerminalPromptState::Ready
        }) {
            break;
        }
        assert!(
            Instant::now() < path_deadline,
            "default-config terminal did not complete `command -v ls`"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let path_output = process
        .operator_read(
            &ensured.terminal.execution_id,
            agl_process::ExecutionCursor { after_sequence: 0 },
            64 * 1024,
        )
        .unwrap()
        .chunks
        .into_iter()
        .flat_map(|chunk| chunk.bytes.decode(64 * 1024).unwrap())
        .collect::<Vec<_>>();
    let path_output = String::from_utf8_lossy(&path_output);
    assert!(
        path_output.contains("/ls"),
        "default admitted PATH did not resolve ls: {path_output:?}"
    );
    assert!(!path_output.contains("command not found"));

    process
        .operator_write(
            &ensured.terminal.execution_id,
            lease.clone(),
            agl_process::ProcessBytes::from_bytes(b"cd monitor-child\n"),
            false,
        )
        .unwrap();

    let command_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = application.snapshot(&opened.session_id).await.unwrap();
        let terminal_ready = snapshot.terminals.iter().any(|terminal| {
            terminal.terminal_id == ensured.terminal.terminal_id
                && terminal.command_sequence == 2
                && terminal.prompt_state == agl_app::TerminalPromptState::Ready
                && terminal.cwd == child.to_string_lossy()
        });
        if terminal_ready
            && snapshot.header.cwd == child.to_string_lossy()
            && snapshot.header.execution_context_revision
                > opened.snapshot.header.execution_context_revision
        {
            break;
        }
        assert!(
            Instant::now() < command_deadline,
            "trusted command cwd was not synchronized"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let synchronized = application.snapshot(&opened.session_id).await.unwrap();
    let lost_response_retry = application
        .ensure_human_terminal(request.clone())
        .await
        .unwrap();
    assert_eq!(
        lost_response_retry.terminal.execution_id, ensured.terminal.execution_id,
        "an exact retry must win before the mutable cwd revision check"
    );
    let mut resized_retry = request.clone();
    resized_retry.client_submission_id = "monitor-new-window".to_owned();
    resized_retry.execution_context_revision = synchronized.header.execution_context_revision;
    resized_retry.terminal_size = agl_process::TerminalSize {
        columns: 161,
        rows: 47,
    };
    let resized_retry = application
        .ensure_human_terminal(resized_retry)
        .await
        .unwrap();
    assert_eq!(
        resized_retry.terminal.execution_id, ensured.terminal.execution_id,
        "cwd revisions and window sizes are not immutable terminal admission metadata"
    );

    const HUMAN_PTY_SENTINEL: &str = "AGL_HUMAN_PTY_CONTEXT_SENTINEL_148";
    let sentinel_command = format!("printf '{HUMAN_PTY_SENTINEL}\\n'\n");
    process
        .operator_write(
            &ensured.terminal.execution_id,
            lease.clone(),
            agl_process::ProcessBytes::from_bytes(sentinel_command.as_bytes()),
            false,
        )
        .unwrap();
    let sentinel_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = application.snapshot(&opened.session_id).await.unwrap();
        if snapshot.terminals.iter().any(|terminal| {
            terminal.terminal_id == ensured.terminal.terminal_id
                && terminal.command_sequence == 3
                && terminal.prompt_state == agl_app::TerminalPromptState::Ready
        }) {
            break;
        }
        assert!(
            Instant::now() < sentinel_deadline,
            "private Human PTY sentinel command did not complete"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let terminal_output = process
        .operator_read(
            &ensured.terminal.execution_id,
            agl_process::ExecutionCursor { after_sequence: 0 },
            64 * 1024,
        )
        .unwrap()
        .chunks
        .into_iter()
        .flat_map(|chunk| chunk.bytes.decode(64 * 1024).unwrap())
        .collect::<Vec<_>>();
    assert!(String::from_utf8_lossy(&terminal_output).contains(HUMAN_PTY_SENTINEL));

    application
        .submit_prompt(agl_app::PromptSubmission {
            session_id: opened.session_id.clone(),
            client_submission_id: "human-pty-context-isolation".to_owned(),
            content: agl_content::Content::text("answer without consulting my terminal").unwrap(),
            budget: agl_app::PromptBudget::default(),
        })
        .await
        .unwrap();
    wait_for_calls(&control, 1);
    let model_request = control.requests.lock().unwrap().first().cloned().unwrap();
    let encoded_model_request = serde_json::to_string(&model_request).unwrap();
    assert!(encoded_model_request.contains("answer without consulting my terminal"));
    assert!(!encoded_model_request.contains(HUMAN_PTY_SENTINEL));
    assert!(!encoded_model_request.contains(sentinel_command.trim()));

    let history = agl_process::HumanShellHistoryStore::with_defaults(
        test.runtime.paths.state_dir.join("terminal-history"),
    )
    .unwrap()
    .load(&workspace)
    .unwrap();
    assert_eq!(
        history.commands(),
        [
            "command -v ls",
            "cd monitor-child",
            "printf 'AGL_HUMAN_PTY_CONTEXT_SENTINEL_148\\n'"
        ]
    );

    let mut saw_started = false;
    let mut saw_finished = false;
    let mut saw_terminal_changed = false;
    let mut saw_header_changed = false;
    let event_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < event_deadline
        && !(saw_started && saw_finished && saw_terminal_changed && saw_header_changed)
    {
        let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(250), subscription.next()).await
        else {
            continue;
        };
        let encoded = serde_json::to_string(&envelope.event).unwrap();
        assert!(!encoded.contains("cd monitor-child"));
        match envelope.event {
            agl_app::SessionPresentationEvent::TerminalCommandStarted {
                terminal_id,
                sequence: 2,
            } if terminal_id == ensured.terminal.terminal_id => saw_started = true,
            agl_app::SessionPresentationEvent::TerminalCommandFinished {
                terminal_id,
                sequence: 2,
                exit_status: 0,
                ref cwd,
            } if terminal_id == ensured.terminal.terminal_id && cwd == &child.to_string_lossy() => {
                saw_finished = true;
            }
            agl_app::SessionPresentationEvent::TerminalChanged { terminal }
                if terminal.terminal_id == ensured.terminal.terminal_id =>
            {
                saw_terminal_changed = true;
            }
            agl_app::SessionPresentationEvent::HeaderChanged { header }
                if header.cwd == child.to_string_lossy() =>
            {
                saw_header_changed = true;
            }
            _ => {}
        }
    }
    assert!(saw_started && saw_finished && saw_terminal_changed && saw_header_changed);

    process
        .operator_detach(&ensured.terminal.execution_id, lease)
        .unwrap();
    process
        .operator_kill(
            &ensured.terminal.execution_id,
            agl_process::KillMode::Graceful,
        )
        .unwrap();

    let closed_deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_degraded = false;
    let mut saw_degraded_notice = false;
    while Instant::now() < closed_deadline && !(saw_degraded && saw_degraded_notice) {
        let snapshot = application.snapshot(&opened.session_id).await.unwrap();
        saw_degraded |= snapshot.terminals.iter().any(|terminal| {
            terminal.terminal_id == ensured.terminal.terminal_id
                && terminal.prompt_state == agl_app::TerminalPromptState::Degraded
        });
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(100), subscription.next()).await
            && matches!(
                envelope.event,
                agl_app::SessionPresentationEvent::Notice { ref code, .. }
                    if code == "shell_integration_degraded"
            )
        {
            saw_degraded_notice = true;
        }
    }
    assert!(
        saw_degraded && saw_degraded_notice,
        "closed private integration must degrade the terminal and publish a bounded notice"
    );
}

#[test]
fn application_execution_actions_reject_cross_session_ids() {
    use std::collections::BTreeMap;

    use agl_process::ExecutionRepository as _;

    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));
    let owner_session_id = open_session(&mut state);
    let requester_session_id = open_session(&mut state);
    let workspace = test
        .runtime
        .paths
        .config_dir
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let program = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("true"))
        .find(|candidate| candidate.is_file())
        .unwrap()
        .canonicalize()
        .unwrap();
    let root_run_id = RunId::generate();
    let execution_id = ExecutionId::generate();
    let request = agl_process::ExecutionRequest {
        owner: agl_process::ExecutionOwner::Session {
            session_id: owner_session_id,
            root_run_id: root_run_id.clone(),
        },
        creating_run_id: root_run_id,
        creating_step_id: agl_ids::StepId::generate(),
        kind: agl_process::ExecutionKind::Argv,
        program,
        program_digest: None,
        args: Vec::new(),
        workspace_root: workspace.clone(),
        cwd: workspace.clone(),
        read_only_roots: Vec::new(),
        environment: agl_process::EnvironmentOverride {
            values: BTreeMap::new(),
        },
        stdin: None,
        close_stdin_after_initial: true,
        io: agl_process::ExecutionIo::Pipes,
        terminal_size: None,
        profile: agl_process::ExecutionProfile::Workspace,
        authorization: agl_process::ExecutionAuthorization::default(),
        grant_lease: None,
        limits: agl_process::ExecutionLimits {
            timeout_ms: Some(1_000),
            max_input_bytes: 1,
            max_output_bytes: 1,
        },
    };
    let status = agl_process::ExecutionStatus {
        execution_id: execution_id.clone(),
        owner: request.owner.clone(),
        state: agl_process::ExecutionState::Starting,
        profile: request.profile,
        io: request.io,
        cwd: workspace,
        terminal_size: None,
        exit: None,
        first_retained_sequence: None,
        last_sequence: 0,
        retained_bytes: 0,
        discarded_output_bytes: 0,
        output_truncated: false,
        output_expired: false,
        started_at_unix_ms: None,
        finished_at_unix_ms: None,
        error_code: None,
    };
    let repository = agl_store::AglExecutionRepository::open_at(
        test.runtime.paths.store_root(),
        Duration::from_secs(60),
    )
    .unwrap();
    repository
        .admit(&status, &request, "cross-session-test-owner")
        .unwrap();

    for action in [
        agl_app::ApplicationAction::ExecutionAttach {
            execution_id: execution_id.clone(),
            read_only: true,
        },
        agl_app::ApplicationAction::ExecutionKill {
            execution_id: execution_id.clone(),
            mode: agl_process::KillMode::Graceful,
        },
    ] {
        let error = state
            .application_invoke(agl_app::ApplicationActionRequest {
                session_id: Some(requester_session_id.clone()),
                client_submission_id: format!("cross-session-{}", ExecutionId::generate()),
                action,
            })
            .unwrap_err();
        assert_eq!(
            error.code,
            agl_app::ApplicationErrorCode::TerminalOwnerMismatch
        );
    }

    let missing = state
        .application_invoke(agl_app::ApplicationActionRequest {
            session_id: Some(requester_session_id),
            client_submission_id: "missing-execution".to_owned(),
            action: agl_app::ApplicationAction::ExecutionAttach {
                execution_id: ExecutionId::generate(),
                read_only: true,
            },
        })
        .unwrap_err();
    assert_eq!(missing.code, agl_app::ApplicationErrorCode::NotFound);
}

#[cfg(target_os = "linux")]
async fn connect_headless_client(
    state: &SharedDaemonState,
) -> (
    agl_client::AgentLibreClient,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
    let server_state = state.clone();
    let server =
        tokio::spawn(
            async move { crate::server::serve_connection(server_stream, &server_state).await },
        );
    let client = agl_client::AgentLibreClient::from_stream(client_stream)
        .await
        .unwrap();
    (client, server)
}

#[cfg(target_os = "linux")]
async fn assert_authoritative_session_finish(
    mut subscription: agl_client::PresentationSubscription,
    expected_session_id: &SessionId,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw_finished_snapshot = false;
    let mut saw_session_finished = false;
    loop {
        let event = tokio::time::timeout_at(deadline, subscription.next())
            .await
            .expect("presentation subscription did not finish")
            .expect("presentation subscription failed")
            .expect("presentation subscription ended without a finish marker");
        match event {
            agl_client::PresentationSubscriptionEvent::SnapshotReplaced { snapshot, .. } => {
                assert_eq!(&snapshot.session_id, expected_session_id);
                saw_finished_snapshot |= matches!(
                    snapshot.header.status,
                    agl_protocol::SessionPresentationStatus::Finished
                );
            }
            agl_client::PresentationSubscriptionEvent::Event(envelope) => {
                assert_eq!(&envelope.session_id, expected_session_id);
                saw_session_finished |= matches!(
                    envelope.event,
                    agl_protocol::SessionPresentationEventPayload::SessionFinished
                );
            }
            agl_client::PresentationSubscriptionEvent::Finished(finished) => {
                assert_eq!(&finished.session_id, expected_session_id);
                assert_eq!(
                    finished.reason,
                    agl_protocol::PresentationSubscriptionFinishReason::SessionFinished
                );
                assert!(
                    saw_finished_snapshot,
                    "subscription must observe the authoritative finished snapshot"
                );
                assert!(
                    saw_session_finished,
                    "subscription must observe the session_finished presentation event"
                );
                break;
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_headless_clients_share_a_human_terminal_until_confirmed_session_exit() {
    let test = TestRuntime::new();
    let workspace = test
        .runtime
        .paths
        .config_dir
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let state = SharedDaemonState::new(
        test.runtime.clone(),
        test.inference.clone(),
        InferenceClientHandle::new(ControlledInferenceClient {
            control: Arc::new(InferenceControl::default()),
        }),
    );
    let process = state.process_handle().unwrap();

    let (first_client, first_server) = connect_headless_client(&state).await;
    let opened = first_client
        .open_session(SessionOpenRequest {
            session_id: None,
            new_session: true,
            workspace_root: Some(workspace.to_string_lossy().into_owned()),
            function_ref: None,
            skills: Vec::new(),
            tool_mode: ProtocolToolMode::ReadOnly,
        })
        .await
        .unwrap();
    let session_id = opened.session_id;
    let first_subscription = first_client
        .subscribe_presentation(agl_protocol::SessionPresentationSubscribeRequest {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();

    let (second_client, second_server) = connect_headless_client(&state).await;
    let resumed = second_client
        .open_session(SessionOpenRequest {
            session_id: Some(session_id.clone()),
            new_session: false,
            workspace_root: None,
            function_ref: None,
            skills: Vec::new(),
            tool_mode: ProtocolToolMode::ReadOnly,
        })
        .await
        .unwrap();
    assert_eq!(resumed.session_id, session_id);
    assert!(resumed.resumed);
    let second_subscription = second_client
        .subscribe_presentation(agl_protocol::SessionPresentationSubscribeRequest {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();

    let ensured = first_client
        .ensure_human_terminal(agl_protocol::HumanTerminalEnsureRequest {
            session_id: session_id.clone(),
            client_submission_id: "two-client-human-terminal".to_owned(),
            execution_context_revision: first_subscription
                .snapshot
                .header
                .execution_context_revision,
            profile: agl_process::ExecutionProfile::Workspace,
            shell_profile_id: "bash-managed".to_owned(),
            terminal_size: agl_process::TerminalSize::default(),
            agl_env: agl_protocol::StructuredEnvironmentOverlay::default(),
            host_startup: agl_protocol::HostStartupPolicy::ManagedOnly,
        })
        .await
        .unwrap();
    let execution_id = ensured.terminal.execution_id.clone();
    assert!(
        process
            .operator_status(&execution_id)
            .unwrap()
            .state
            .is_live()
    );

    drop(first_subscription);
    drop(first_client);
    tokio::time::timeout(Duration::from_secs(3), first_server)
        .await
        .expect("first daemon connection did not close")
        .unwrap()
        .unwrap();
    assert!(
        process
            .operator_status(&execution_id)
            .unwrap()
            .state
            .is_live(),
        "disconnect must not terminate the durable Human terminal"
    );

    let (reconnected_client, reconnected_server) = connect_headless_client(&state).await;
    let resumed = reconnected_client
        .open_session(SessionOpenRequest {
            session_id: Some(session_id.clone()),
            new_session: false,
            workspace_root: None,
            function_ref: None,
            skills: Vec::new(),
            tool_mode: ProtocolToolMode::ReadOnly,
        })
        .await
        .unwrap();
    assert_eq!(resumed.session_id, session_id);
    assert!(resumed.resumed);
    let reconnected_subscription = reconnected_client
        .subscribe_presentation(agl_protocol::SessionPresentationSubscribeRequest {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    assert!(
        reconnected_subscription
            .snapshot
            .terminals
            .iter()
            .any(|terminal| terminal.execution_id == execution_id)
    );

    let exited = second_client
        .application_action(agl_protocol::ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: "two-client-confirmed-exit".to_owned(),
            action: agl_protocol::ApplicationAction::SessionExit {
                confirm_active: true,
            },
        })
        .await
        .unwrap();
    assert!(matches!(
        exited.result,
        agl_protocol::ApplicationActionResult::SessionExited {
            ref session_id,
            terminated_terminals: 1,
            ..
        } if session_id == &resumed.session_id
    ));

    tokio::join!(
        assert_authoritative_session_finish(reconnected_subscription, &session_id),
        assert_authoritative_session_finish(second_subscription, &session_id),
    );
    assert!(
        process
            .operator_status(&execution_id)
            .unwrap()
            .state
            .is_terminal(),
        "confirmed SessionExit must terminate the Human terminal"
    );

    drop(reconnected_client);
    drop(second_client);
    tokio::time::timeout(Duration::from_secs(3), reconnected_server)
        .await
        .expect("reconnected daemon connection did not close")
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), second_server)
        .await
        .expect("second daemon connection did not close")
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_run_submit_uses_the_shared_prompt_projection() {
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
    let application = state.application();
    let opened = application
        .open_session(agl_app::SessionOpen {
            launch: agl_app::SessionLaunchOptions {
                workspace_root: None,
                function_ref: None,
                model_id: None,
                operation_mode: Some(agl_chat::ToolAccessMode::ReadOnly),
                skill_ids: Vec::new(),
            },
        })
        .await
        .unwrap();
    let mut presentation = application
        .subscribe(agl_app::PresentationSubscribe {
            session_id: opened.session_id.clone(),
        })
        .await
        .unwrap();

    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let server_state = state.clone();
    let server = tokio::spawn(async move {
        crate::server::serve_authenticated_test_connection(server_stream, &server_state).await
    });
    let (client_reader, mut client_writer) = tokio::io::split(client_stream);
    let mut client_reader = BufReader::new(client_reader);
    for kind in [
        DaemonRequestKind::Hello(HelloRequest::default()),
        DaemonRequestKind::RunSubmit(RunSubmitRequest {
            session_id: opened.session_id.clone(),
            content: agl_content::Content::text("shared projection prompt").unwrap(),
            client_submission_id: "shared-projection-prompt".to_owned(),
            budget: RunBudgetRequest::default(),
        }),
    ] {
        let encoded = serde_json::to_string(&request(kind)).unwrap();
        client_writer
            .write_all(format!("{encoded}\n").as_bytes())
            .await
            .unwrap();
    }
    client_writer.flush().await.unwrap();
    let mut line = String::new();
    client_reader.read_line(&mut line).await.unwrap();
    assert!(matches!(
        serde_json::from_str::<DaemonEvent>(&line).unwrap().kind,
        DaemonEventKind::Hello(_)
    ));
    line.clear();
    client_reader.read_line(&mut line).await.unwrap();
    let accepted = match serde_json::from_str::<DaemonEvent>(&line).unwrap().kind {
        DaemonEventKind::RunAccepted(accepted) => accepted,
        other => panic!("unexpected prompt admission event: {other:?}"),
    };

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw_snapshot = false;
    let mut saw_transition = false;
    while !(saw_snapshot && saw_transition) {
        let event = tokio::time::timeout_at(deadline, presentation.next())
            .await
            .expect("shared prompt projection did not advance")
            .unwrap();
        saw_snapshot |= matches!(
            &event.event,
            agl_app::SessionPresentationEvent::SnapshotReplaced { .. }
        );
        saw_transition |= match &event.event {
            agl_app::SessionPresentationEvent::PromptActivated { run_id } => {
                run_id == &accepted.run_id
            }
            agl_app::SessionPresentationEvent::PromptQueued { prompt } => {
                prompt.run_id == accepted.run_id
            }
            _ => false,
        };
    }

    let cancelled = state.handle_request(request(DaemonRequestKind::RunCancel(RunCancelRequest {
        run_id: accepted.run_id,
    })));
    assert!(matches!(
        cancelled.kind,
        DaemonEventKind::RunStatus(ref status) if status.cancellation_requested
    ));
    control.blocked.store(false, Ordering::Release);
    drop(client_reader);
    drop(client_writer);
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("prompt test daemon connection did not close")
        .unwrap()
        .unwrap();
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

    let first = run_cron_tick(&store, 0, &mut executor, &mut notifier).unwrap();
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
