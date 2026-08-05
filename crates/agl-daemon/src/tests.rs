use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agl_app::{
    ContinueActionView, ContinueUnavailableReason, IncompleteAssistantItemView,
    SessionPresentationItem,
};
use agl_chat::{
    ChatInferenceJob, ChatRunInput, InferenceClient, InferenceClientHandle, InferenceOptions,
    ToolAccessMode,
};
use agl_config::ResolvedInferenceConfig;
use agl_cron::{CronJob, CronJobDraft, CronRepository, CronRunStatus, CronTargetKind};
use agl_ids::{MessageId, RequestId, RunId, SessionId, TurnId};
use agl_inference::{
    InferenceFinishReason, InferenceOutputEvent, InferenceProductStage, InferenceProgressUnit,
    InferenceResponse, InferenceResponseMetadata, InferenceStageEvent, ModelManagerError,
    ModelManagerStatus, ModelManagerStatusDetail, ModelUnloadOutcome as ManagerUnloadOutcome,
    ModelUnloadResult, ModelUnloadTarget as ManagerUnloadTarget, OutputDelivery,
    WorkerRuntimeStatusHandle,
};
use agl_protocol::{
    DaemonCapability, DaemonEvent, DaemonEventKind, DaemonRequest, DaemonRequestKind, HelloRequest,
    InferenceInventoryRequest, InferenceStatusRequest, ModelUnloadRequest, ModelUnloadTarget,
    PROTOCOL_VERSION, ProtocolErrorCode, ProtocolErrorDetails, ProtocolInferenceWorkerState,
    ProtocolRunKind, ProtocolRunState, ProtocolToolMode, RunBudgetRequest, RunCancelRequest,
    RunEventsRequest, RunStatusRequest, RunSubmitRequest, RunTreeRequest, SessionFinishReason,
    SessionFinishRequest, SessionListRequest, SessionOpenRequest, SessionStatus,
    SessionStatusRequest, SetupSmokeSessionOpenRequest,
};
use agl_runtime::{
    AgentLibreHistoryConfig, AgentLibreLoggingConfig, AgentLibrePaths, AgentLibreRuntimeConfig,
    AgentLibreWorkspaceConfig,
};
use agl_session::ChatSessionStore;
use agl_store::{AglStore, RunState};
use anyhow::Context;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use super::*;

static TEST_RUNTIME_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn test_runtime_plan_model_identity(id: &str) -> agl_model::RuntimePlanModelIdentity {
    serde_json::from_value(serde_json::json!({
        "provenance": {
            "reference": format!("model:{id}@=1.0.0"),
            "source_id": "test",
            "source_tier": "workspace",
            "source_kind": "directory",
            "package_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        },
        "weights": [{
            "role": "main",
            "model_id": id,
            "filename": format!("{id}.gguf"),
            "byte_size": 18,
            "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
            "required": true
        }]
    }))
    .unwrap()
}

#[test]
fn default_root_run_budget_is_identical_across_protocol_application_and_store() {
    let protocol = RunBudgetRequest::default();
    let application = agl_app::PromptBudget::default();
    let store = agl_store::RunBudget::default();

    assert_eq!(protocol.wall_time_ms, 600_000);
    assert_eq!(application.wall_time_ms, protocol.wall_time_ms);
    assert_eq!(store.wall_time_ms, protocol.wall_time_ms);
    assert_eq!(
        (
            application.model_input_tokens,
            application.model_output_tokens,
            application.model_attempts,
            application.capability_calls,
        ),
        (
            protocol.model_input_tokens,
            protocol.model_output_tokens,
            protocol.model_attempts,
            protocol.capability_calls,
        )
    );
    assert_eq!(
        (
            store.model_input_tokens,
            store.model_output_tokens,
            store.model_attempts,
            store.capability_calls,
        ),
        (
            protocol.model_input_tokens,
            protocol.model_output_tokens,
            protocol.model_attempts,
            protocol.capability_calls,
        )
    );
}

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
                inference: agl_runtime::AgentLibreInferenceConfig::default(),
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
    finish_with_length: AtomicBool,
    emit_scripted_progress: AtomicBool,
    requests: Mutex<Vec<agl_inference::InferenceRequest>>,
    configs: Mutex<Vec<ResolvedInferenceConfig>>,
    manager_status: Mutex<Option<ModelManagerStatus>>,
    unload_result: Mutex<Option<Result<ModelUnloadResult, ModelManagerError>>>,
    unload_targets: Mutex<Vec<ManagerUnloadTarget>>,
}

struct InferenceUnblockGuard(Arc<InferenceControl>);

impl Drop for InferenceUnblockGuard {
    fn drop(&mut self) {
        self.0.blocked.store(false, Ordering::Release);
    }
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
                resource_admission: None,
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

    fn device_inventory(&self) -> anyhow::Result<Vec<agl_inference::InferenceDeviceInfo>> {
        Ok(Vec::new())
    }
}

impl InferenceClient for ControlledInferenceClient {
    fn generate(&self, job: ChatInferenceJob) -> anyhow::Result<InferenceResponse> {
        self.control
            .configs
            .lock()
            .unwrap()
            .push(job.config.clone());
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
        let emit_scripted_progress = self.control.emit_scripted_progress.load(Ordering::Acquire);
        if emit_scripted_progress {
            let terminal_stage = if self.control.finish_with_length.load(Ordering::Acquire) {
                InferenceProductStage::Incomplete
            } else {
                InferenceProductStage::Completed
            };
            let stages = [
                (InferenceProductStage::Queued, None, None, None),
                (InferenceProductStage::Admission, None, None, None),
                (InferenceProductStage::ModelLoad, None, None, None),
                (InferenceProductStage::ContextRebuild, None, None, None),
                (
                    InferenceProductStage::Prefill,
                    Some(4),
                    Some(4),
                    Some(InferenceProgressUnit::Tokens),
                ),
                (
                    InferenceProductStage::Generation,
                    Some(1),
                    Some(2),
                    Some(InferenceProgressUnit::Tokens),
                ),
                (
                    InferenceProductStage::Generation,
                    Some(2),
                    Some(2),
                    Some(InferenceProgressUnit::Tokens),
                ),
                (InferenceProductStage::OutputParse, None, None, None),
                (terminal_stage, None, None, None),
            ];
            for (index, (stage, completed, total, unit)) in stages.into_iter().enumerate() {
                assert_eq!(
                    job.output_sink
                        .try_emit(InferenceOutputEvent::Stage(InferenceStageEvent {
                            attempt_id: job.request.attempt_id.clone(),
                            stage_sequence: u64::try_from(index).unwrap() + 1,
                            stage,
                            completed,
                            total,
                            unit,
                        },)),
                    OutputDelivery::Delivered
                );
            }
            for (sequence, text) in ["durable ", "answer ☃\n\nVerification: fake inference."]
                .into_iter()
                .enumerate()
            {
                assert_eq!(
                    job.output_sink.try_emit(InferenceOutputEvent::TextDelta {
                        attempt_id: job.request.attempt_id.clone(),
                        sequence: u64::try_from(sequence).unwrap() + 1,
                        text: text.to_owned(),
                    }),
                    OutputDelivery::Delivered
                );
            }
        }
        Ok(InferenceResponse {
            attempt_id: job.request.attempt_id,
            content: if emit_scripted_progress {
                "durable answer ☃\n\nVerification: fake inference."
            } else {
                "durable answer\n\nVerification: fake inference."
            }
            .to_string(),
            finish_reason: if self.control.finish_with_length.load(Ordering::Acquire) {
                InferenceFinishReason::Length
            } else {
                InferenceFinishReason::Stop
            },
            metadata: InferenceResponseMetadata {
                model_state: Some("daemon-test".to_string()),
                selected_device: None,
                duration_ms: 0,
                input_tokens: 4,
                output_tokens: 2,
                resource_admission: None,
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
        Ok(self
            .control
            .manager_status
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_default())
    }

    fn status_with_detail(
        &self,
        _detail: ModelManagerStatusDetail,
    ) -> anyhow::Result<ModelManagerStatus> {
        self.status()
    }

    fn unload(&self, target: ManagerUnloadTarget) -> anyhow::Result<ModelUnloadResult> {
        self.control.unload_targets.lock().unwrap().push(target);
        self.control
            .unload_result
            .lock()
            .unwrap()
            .clone()
            .unwrap_or({
                Ok(ModelUnloadResult {
                    matched_models: 0,
                    released_models: 0,
                    released_contexts: 0,
                    outcome: ManagerUnloadOutcome::NotResident,
                })
            })
            .map_err(Into::into)
    }

    fn device_inventory(&self) -> anyhow::Result<Vec<agl_inference::InferenceDeviceInfo>> {
        Ok(Vec::new())
    }
}

fn daemon(test: &TestRuntime, control: Arc<InferenceControl>) -> DaemonState {
    DaemonState::new(
        test.runtime.clone(),
        test.inference.clone(),
        InferenceClientHandle::new(ControlledInferenceClient { control }),
        WorkerRuntimeStatusHandle::default(),
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
        WorkerRuntimeStatusHandle::default(),
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
                    client_runtime: None,
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
            client_runtime: None,
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

#[test]
fn setup_smoke_session_uses_inline_staged_state_without_publishing_it() {
    let test = TestRuntime::new();
    let control = Arc::new(InferenceControl::default());
    let mut state = daemon(&test, Arc::clone(&control));
    let root = test.runtime.paths.config_dir.parent().unwrap();
    let workspace = root.join("workspace");
    let function_root = workspace.join(".agl/functions/setup-smoke");
    std::fs::create_dir_all(&function_root).unwrap();
    std::fs::write(
        function_root.join("FUNCTION.md"),
        r#"---
artifact:
  schema: agentlibre.artifact/v1
  type: function
  id: setup-smoke
  version: 1.0.0
  payload_schema: agentlibre.function/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires:
    - model:setup-smoke-model@^1.0
title: Setup smoke
model:
  config: inference.toml
runtime:
  tool_mode: read-only
  max_output_tokens: 32
skills:
  use: []
subagents:
  use: []
doctor:
  smoke_prompt: "Reply with setup smoke ready."
---
"#,
    )
    .unwrap();
    std::fs::write(function_root.join("SYSTEM.md"), "Run the setup smoke.\n").unwrap();
    let model_root = workspace.join(".agl/models/setup-smoke-model");
    std::fs::create_dir_all(model_root.join("evidence")).unwrap();
    std::fs::write(
        model_root.join("MODEL.toml"),
        r#"artifact = { schema = "agentlibre.artifact/v1", type = "model", id = "setup-smoke-model", version = "1.0.0", payload_schema = "agentlibre.model/v2", agl = { compatible = ">=1.0.0-alpha.12", tested = ["1.0.0-alpha.12"] }, requires = [] }

display_name = "Setup smoke fixture"
capabilities = ["text", "tools"]
license = "test-only"
license_url = "https://example.invalid/license"
repository = "agentlibre/setup-smoke-fixture"
upstream_revision = "0000000000000000000000000000000000000000"

[[weights]]
role = "main"
model_id = "setup-smoke-model"
filename = "setup-smoke-model.gguf"
byte_size = 18
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
required = true

[[profiles]]
id = "cpu-test"
device = "cpu"
benchmark_evidence = "evidence/cpu.md"
required_total_ram_bytes = 1024
required_available_ram_bytes = 512
required_vram_bytes = 0
gpu_layers = 0
context_tokens = 4096
batch_size = 128
ubatch_size = 64
threads = 2
smoke_timeout_seconds = 30
expected_speed = "test"
"#,
    )
    .unwrap();
    std::fs::write(model_root.join("evidence/cpu.md"), "Test-only evidence.\n").unwrap();
    std::fs::write(
        function_root.join("inference.toml"),
        r#"[backend]
kind = "llama_cpp"
model_id = "setup-smoke-model"

[runtime]
mode = "fixed"
gpu_layers = 0
context_tokens = 4096
threads = 2
batch_size = 128
ubatch_size = 64

[model]
dialect = "gemma4"
tool_call_format = "gemma_function_call"
"#,
    )
    .unwrap();
    let staged_model = root.join("staged-model.gguf");
    std::fs::write(&staged_model, b"test model fixture").unwrap();
    let published_bindings_path = agl_config::model_bindings_path(&test.runtime.paths.config_dir);
    agl_config::write_model_bindings(
        &published_bindings_path,
        &agl_config::ModelBindings::empty(),
    )
    .unwrap();

    let event = state.handle_request(request(DaemonRequestKind::SetupSmokeSessionOpen(Box::new(
        SetupSmokeSessionOpenRequest {
            workspace_root: workspace.to_string_lossy().into_owned(),
            function_ref: "setup-smoke".to_owned(),
            staged_bindings: agl_config::ModelBindings {
                version: 1,
                models: std::collections::BTreeMap::from([(
                    agl_config::ModelId::new("setup-smoke-model").unwrap(),
                    agl_config::ModelBinding {
                        path: staged_model.clone(),
                    },
                )]),
            },
            runtime_plan: agl_protocol::SetupSmokeRuntimePlan {
                profile_id: "setup-cpu".to_owned(),
                selected_device: None,
                selected_device_identity: None,
                model: test_runtime_plan_model_identity("setup-smoke-model"),
                runtime: agl_config::InferenceRuntimeConfig {
                    gpu_layers: 0,
                    context_tokens: 4_096,
                    threads: 2,
                    device: None,
                    batch_size: Some(128),
                    ubatch_size: Some(64),
                    flash_attention: Some(agl_config::RuntimeSwitch::Off),
                    cache_type_k: None,
                    cache_type_v: None,
                    mmap: Some(true),
                    kv_unified: Some(true),
                    structured_decoding: agl_config::StructuredDecodingMode::Auto,
                    repair_malformed_tool_calls: true,
                    mtp: agl_config::MtpRuntimeConfig::default(),
                },
                smoke_timeout_seconds: 30,
                expected_speed: "test".to_owned(),
            },
            max_output_tokens: 32,
        },
    ))));
    let session_id = match event.kind {
        DaemonEventKind::SessionOpened(opened) => {
            assert!(!opened.resumed);
            opened.session_id
        }
        other => panic!("unexpected setup smoke open event: {other:?}"),
    };
    let snapshot = state.application_snapshot(&session_id).unwrap();
    assert_eq!(snapshot.header.operation_mode, ToolAccessMode::ReadOnly);
    assert!(snapshot.header.selected_skills.is_empty());
    assert!(!ChatSessionStore::exists(
        test.runtime.paths.sessions_root(),
        &session_id
    ));

    let accepted = submit(
        &mut state,
        &session_id,
        "Run the bounded smoke.",
        Some("setup-smoke-submit"),
    );
    wait_for_calls(&control, 1);
    let outcome = wait_for_terminal(&state, &accepted.run_id);
    assert_eq!(outcome.status.state, RunState::Succeeded);
    assert_eq!(
        control.configs.lock().unwrap()[0].backend.model,
        staged_model
    );
    assert!(
        agl_config::load_model_bindings(&published_bindings_path)
            .unwrap()
            .models
            .is_empty(),
        "daemon setup smoke must not publish staged bindings"
    );
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
    // The workspace CI runs several native-heavy test binaries concurrently;
    // keep this functional assertion independent from scheduler latency.
    let deadline = Instant::now() + Duration::from_secs(10);
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

fn incomplete_item_identity(state: &DaemonState, session_id: &SessionId) -> (MessageId, u64) {
    let snapshot = state.application_snapshot(session_id).unwrap();
    let message_id = snapshot
        .items
        .iter()
        .find_map(|item| match item {
            SessionPresentationItem::IncompleteAssistant { item } => Some(item.message_id.clone()),
            _ => None,
        })
        .expect("length-finished run must project an incomplete assistant item");
    (message_id, snapshot.header.execution_context_revision)
}

fn continue_incomplete(
    state: &mut DaemonState,
    session_id: &SessionId,
    message_id: &MessageId,
    execution_context_revision: u64,
    client_submission_id: &str,
) -> agl_app::PromptAdmission {
    let result = state
        .application_invoke(agl_app::ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: client_submission_id.to_owned(),
            action: agl_app::ApplicationAction::IncompleteTurnContinue {
                message_id: message_id.clone(),
                expected_execution_context_revision: execution_context_revision,
            },
        })
        .unwrap();
    let agl_app::ApplicationToolResult::IncompleteTurnContinued { admission } = result else {
        panic!("unexpected continuation result: {result:?}");
    };
    admission
}

#[test]
fn incomplete_continue_replays_same_run_after_reconnect_and_daemon_restart() {
    let test = TestRuntime::new();
    let control = Arc::new(InferenceControl::default());
    control.finish_with_length.store(true, Ordering::Release);
    let mut state = daemon(&test, Arc::clone(&control));
    let session_id = open_session(&mut state);
    let source = submit(&mut state, &session_id, "bounded answer", None);
    assert_eq!(
        wait_for_terminal(&state, &source.run_id).status.state,
        RunState::Incomplete
    );
    let (message_id, execution_context_revision) = incomplete_item_identity(&state, &session_id);

    let first = continue_incomplete(
        &mut state,
        &session_id,
        &message_id,
        execution_context_revision,
        "continue-reconnect-restart",
    );
    let retry_after_reconnect = continue_incomplete(
        &mut state,
        &session_id,
        &message_id,
        execution_context_revision,
        "continue-reconnect-restart",
    );
    assert_eq!(retry_after_reconnect.run_id, first.run_id);
    assert_eq!(retry_after_reconnect.turn_id, first.turn_id);
    assert!(retry_after_reconnect.replayed);
    let competing = state
        .application_invoke(agl_app::ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: "continue-competing-claim".to_owned(),
            action: agl_app::ApplicationAction::IncompleteTurnContinue {
                message_id: message_id.clone(),
                expected_execution_context_revision: execution_context_revision,
            },
        })
        .unwrap_err();
    assert_eq!(
        competing.code,
        agl_app::ApplicationErrorCode::ContinuationAlreadyClaimed
    );
    wait_for_terminal(&state, &first.run_id);
    drop(state);

    let mut restarted = daemon(&test, control);
    let opened = restarted.handle_request(request(DaemonRequestKind::SessionOpen(
        SessionOpenRequest {
            session_id: Some(session_id.clone()),
            new_session: false,
            workspace_root: None,
            function_ref: None,
            skills: Vec::new(),
            tool_mode: ProtocolToolMode::ReadOnly,
        },
    )));
    assert!(
        matches!(
            opened.kind,
            DaemonEventKind::SessionOpened(ref event)
                if event.session_id == session_id && event.resumed
        ),
        "unexpected restart open event: {:?}",
        opened.kind
    );
    let retry_after_restart = continue_incomplete(
        &mut restarted,
        &session_id,
        &message_id,
        execution_context_revision,
        "continue-reconnect-restart",
    );
    assert_eq!(retry_after_restart.run_id, first.run_id);
    assert_eq!(retry_after_restart.turn_id, first.turn_id);
    assert!(retry_after_restart.replayed);
}

#[test]
fn daemon_resume_reconciles_a_synced_claim_without_an_admitted_run() {
    let test = TestRuntime::new();
    let control = Arc::new(InferenceControl::default());
    control.finish_with_length.store(true, Ordering::Release);
    let mut state = daemon(&test, Arc::clone(&control));
    let session_id = open_session(&mut state);
    let source = submit(&mut state, &session_id, "crash window", None);
    assert_eq!(
        wait_for_terminal(&state, &source.run_id).status.state,
        RunState::Incomplete
    );
    let (message_id, execution_context_revision) = incomplete_item_identity(&state, &session_id);
    let continuation_run_id = RunId::generate();
    let continuation_turn_id = TurnId::generate();
    let continuation_request_id = RequestId::generate();
    let mut transcript =
        ChatSessionStore::open(test.runtime.paths.sessions_root(), session_id.clone()).unwrap();
    transcript
        .append_incomplete_continuation_claim(
            message_id.clone(),
            "continue-prepared-before-crash".to_owned(),
            continuation_run_id.clone(),
            continuation_turn_id.clone(),
            continuation_request_id.clone(),
        )
        .unwrap();
    let store = AglStore::open_current_read_only_at(test.runtime.paths.store_root()).unwrap();
    assert!(
        store
            .safe_run_status(&continuation_run_id)
            .unwrap()
            .is_none()
    );
    drop(store);
    drop(state);

    let mut restarted = daemon(&test, control);
    let opened = restarted.handle_request(request(DaemonRequestKind::SessionOpen(
        SessionOpenRequest {
            session_id: Some(session_id.clone()),
            new_session: false,
            workspace_root: None,
            function_ref: None,
            skills: Vec::new(),
            tool_mode: ProtocolToolMode::ReadOnly,
        },
    )));
    assert!(matches!(opened.kind, DaemonEventKind::SessionOpened(_)));
    let store = AglStore::open_current_read_only_at(test.runtime.paths.store_root()).unwrap();
    let recovered = store
        .safe_run_status(&continuation_run_id)
        .unwrap()
        .expect("session resume must reconcile its prepared continuation claim");
    assert_eq!(recovered.session_id.as_ref(), Some(&session_id));
    assert_eq!(recovered.turn_id.as_ref(), Some(&continuation_turn_id));
    let record = store.run(&continuation_run_id).unwrap().unwrap();
    let input: ChatRunInput = serde_json::from_value(record.input).unwrap();
    assert!(matches!(
        input,
        ChatRunInput::Continuation {
            request_id: Some(actual),
            ..
        } if actual == continuation_request_id
    ));
    drop(store);

    let replay = continue_incomplete(
        &mut restarted,
        &session_id,
        &message_id,
        execution_context_revision,
        "continue-prepared-before-crash",
    );
    assert_eq!(replay.run_id, continuation_run_id);
    assert_eq!(replay.turn_id, continuation_turn_id);
    assert!(replay.replayed);
    wait_for_terminal(&restarted, &replay.run_id);
}

#[test]
fn incomplete_continue_revalidates_context_and_policy_before_durable_claim() {
    let test = TestRuntime::new();
    let control = Arc::new(InferenceControl::default());
    control.finish_with_length.store(true, Ordering::Release);
    let mut state = daemon(&test, Arc::clone(&control));
    let session_id = open_session(&mut state);
    let source = submit(&mut state, &session_id, "race continuation admission", None);
    assert_eq!(
        wait_for_terminal(&state, &source.run_id).status.state,
        RunState::Incomplete
    );
    let (message_id, execution_context_revision) = incomplete_item_identity(&state, &session_id);

    state.set_execution_context_revision_for_test(&session_id, execution_context_revision + 1);
    let stale = state
        .application_invoke(agl_app::ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: "continue-after-restored-guards".to_owned(),
            action: agl_app::ApplicationAction::IncompleteTurnContinue {
                message_id: message_id.clone(),
                expected_execution_context_revision: execution_context_revision,
            },
        })
        .unwrap_err();
    assert_eq!(
        stale.code,
        agl_app::ApplicationErrorCode::StaleContinuationContext
    );
    let snapshot = state.application_snapshot(&session_id).unwrap();
    assert!(snapshot.items.iter().any(|item| matches!(
        item,
        SessionPresentationItem::IncompleteAssistant {
            item: IncompleteAssistantItemView {
                message_id: actual,
                continue_action: ContinueActionView::Unavailable {
                    reason: ContinueUnavailableReason::StaleContext,
                },
                ..
            }
        } if actual == &message_id
    )));
    state.set_execution_context_revision_for_test(&session_id, execution_context_revision);

    state
        .select_chat_service_mode_for_test(&session_id, agl_chat::ToolAccessMode::Write)
        .unwrap();
    let denied = state
        .application_invoke(agl_app::ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: "continue-after-restored-guards".to_owned(),
            action: agl_app::ApplicationAction::IncompleteTurnContinue {
                message_id: message_id.clone(),
                expected_execution_context_revision: execution_context_revision,
            },
        })
        .unwrap_err();
    assert_eq!(denied.code, agl_app::ApplicationErrorCode::NotAuthorized);
    let snapshot = state.application_snapshot(&session_id).unwrap();
    assert!(snapshot.items.iter().any(|item| matches!(
        item,
        SessionPresentationItem::IncompleteAssistant {
            item: IncompleteAssistantItemView {
                message_id: actual,
                continue_action: ContinueActionView::Unavailable {
                    reason: ContinueUnavailableReason::PolicyDenied,
                },
                ..
            }
        } if actual == &message_id
    )));

    let transcript =
        ChatSessionStore::open(test.runtime.paths.sessions_root(), session_id.clone()).unwrap();
    let replay = transcript.read_replay().unwrap();
    assert!(!replay.events.iter().any(|event| matches!(
        event,
        agl_session::ChatSessionEvent::IncompleteContinuationClaimed { .. }
    )));

    state
        .select_chat_service_mode_for_test(&session_id, agl_chat::ToolAccessMode::ReadOnly)
        .unwrap();
    let snapshot = state.application_snapshot(&session_id).unwrap();
    assert!(snapshot.items.iter().any(|item| matches!(
        item,
        SessionPresentationItem::IncompleteAssistant {
            item: IncompleteAssistantItemView {
                message_id: actual,
                continue_action: ContinueActionView::Available,
                ..
            }
        } if actual == &message_id
    )));

    control.finish_with_length.store(false, Ordering::Release);
    let continued = continue_incomplete(
        &mut state,
        &session_id,
        &message_id,
        execution_context_revision,
        "continue-after-restored-guards",
    );
    let outcome = wait_for_terminal(&state, &continued.run_id);
    assert_eq!(outcome.status.state, RunState::Succeeded);
}

#[test]
fn incomplete_continue_joins_fifo_behind_prompts_already_queued() {
    let test = TestRuntime::new();
    let control = Arc::new(InferenceControl::default());
    control.finish_with_length.store(true, Ordering::Release);
    let mut state = daemon(&test, Arc::clone(&control));
    let session_id = open_session(&mut state);
    let source = submit(&mut state, &session_id, "bounded source", None);
    assert_eq!(
        wait_for_terminal(&state, &source.run_id).status.state,
        RunState::Incomplete
    );
    let (message_id, execution_context_revision) = incomplete_item_identity(&state, &session_id);

    control.finish_with_length.store(false, Ordering::Release);
    control.blocked.store(true, Ordering::Release);
    let unblock = InferenceUnblockGuard(Arc::clone(&control));
    let active = submit(&mut state, &session_id, "active prompt", None);
    wait_for_calls(&control, 2);
    let queued_before_continue = submit(&mut state, &session_id, "already queued prompt", None);
    std::thread::sleep(Duration::from_millis(2));
    let continuation = continue_incomplete(
        &mut state,
        &session_id,
        &message_id,
        execution_context_revision,
        "continue-after-queued-prompt",
    );

    let key = agl_store::RunConcurrencyKey::session(&session_id).unwrap();
    let store = AglStore::open_current_read_only_at(test.runtime.paths.store_root()).unwrap();
    let ordered = store.safe_runs_for_concurrency_key(&key, false).unwrap();
    let ordered_ids = ordered
        .iter()
        .map(|status| status.run_id.clone())
        .collect::<Vec<_>>();
    drop(unblock);

    assert_eq!(
        ordered_ids,
        vec![
            active.run_id.clone(),
            queued_before_continue.run_id.clone(),
            continuation.run_id.clone(),
        ]
    );
    assert!(continuation.queued);
    assert_eq!(continuation.ordinal, 3);
    assert_eq!(
        wait_for_terminal(&state, &active.run_id).status.state,
        RunState::Succeeded
    );
    assert_eq!(
        wait_for_terminal(&state, &queued_before_continue.run_id)
            .status
            .state,
        RunState::Succeeded
    );
    assert_eq!(
        wait_for_terminal(&state, &continuation.run_id).status.state,
        RunState::Succeeded
    );
}

#[test]
fn hello_declares_strict_run_capabilities() {
    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));
    let expected_runtime = agl_runtime::current_runtime_identity().unwrap();

    let event = state.handle_request(request(DaemonRequestKind::Hello(HelloRequest {
        client_name: Some("test".to_string()),
        accepted_protocol_versions: vec![PROTOCOL_VERSION.to_string()],
        client_runtime: None,
    })));

    match event.kind {
        DaemonEventKind::Hello(hello) => {
            assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
            assert_eq!(
                hello.worker_build_id,
                agl_inference::worker_protocol::WORKER_BUILD_ID
            );
            assert_eq!(
                hello.daemon_runtime.generation_id,
                expected_runtime.generation_id
            );
            assert_eq!(
                hello.daemon_runtime.builtin_catalog_digest,
                expected_runtime.builtin_catalog_digest
            );
            assert!(hello.capabilities.contains(&DaemonCapability::RunSubmit));
            assert!(hello.capabilities.contains(&DaemonCapability::RunStatus));
            assert!(hello.capabilities.contains(&DaemonCapability::RunTree));
            assert!(hello.capabilities.contains(&DaemonCapability::RunCancel));
            assert!(hello.capabilities.contains(&DaemonCapability::RunReplay));
            assert!(hello.capabilities.contains(&DaemonCapability::RunSubscribe));
            assert!(
                hello
                    .capabilities
                    .contains(&DaemonCapability::InferenceInventory)
            );
            assert!(
                hello
                    .capabilities
                    .contains(&DaemonCapability::InferenceStatus)
            );
            assert!(hello.capabilities.contains(&DaemonCapability::ModelUnload));
        }
        other => panic!("unexpected hello event: {other:?}"),
    }
}

#[test]
fn first_party_hello_rejects_runtime_mismatch_before_session_creation() {
    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));
    let observed_daemon = match state
        .handle_request(request(DaemonRequestKind::Hello(HelloRequest {
            client_name: Some("identity-observer".to_owned()),
            accepted_protocol_versions: vec![PROTOCOL_VERSION.to_owned()],
            client_runtime: None,
        })))
        .kind
    {
        DaemonEventKind::Hello(hello) => hello.daemon_runtime,
        other => panic!("unexpected identity observation event: {other:?}"),
    };
    let mut client = observed_daemon.clone();
    client.executable_digest = format!("sha256:{}", "d".repeat(64));

    let event = state.handle_request(request(DaemonRequestKind::Hello(HelloRequest {
        client_name: Some("first-party-test".to_owned()),
        accepted_protocol_versions: vec![PROTOCOL_VERSION.to_owned()],
        client_runtime: Some(client.clone()),
    })));

    match event.kind {
        DaemonEventKind::Error(error) => {
            assert_eq!(error.code, ProtocolErrorCode::RuntimeIdentityMismatch);
            match error.details.map(|details| *details) {
                Some(ProtocolErrorDetails::RuntimeIdentityMismatch {
                    client: actual_client,
                    daemon: returned_daemon,
                }) => {
                    assert_eq!(actual_client, client);
                    assert_eq!(returned_daemon, observed_daemon);
                }
                other => panic!("unexpected mismatch details: {other:?}"),
            }
        }
        other => panic!("unexpected mismatch event: {other:?}"),
    }
    let event = state.handle_request(request(DaemonRequestKind::SessionList(
        SessionListRequest::default(),
    )));
    match event.kind {
        DaemonEventKind::SessionList(list) => assert!(list.sessions.is_empty()),
        other => panic!("unexpected post-mismatch session list event: {other:?}"),
    }
}

#[test]
fn inference_inventory_uses_the_daemon_owned_inference_client() {
    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));

    let event = state.handle_request(request(DaemonRequestKind::InferenceInventory(
        InferenceInventoryRequest::default(),
    )));
    match event.kind {
        DaemonEventKind::InferenceInventory(inventory) => assert!(inventory.devices.is_empty()),
        other => panic!("unexpected inventory event: {other:?}"),
    }
}

#[test]
fn inference_status_uses_the_captured_worker_status_handle() {
    let test = TestRuntime::new();
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));

    let event = state.handle_request(request(DaemonRequestKind::InferenceStatus(
        InferenceStatusRequest::default(),
    )));
    match event.kind {
        DaemonEventKind::InferenceStatus(status) => {
            assert!(!status.worker_build_id.is_empty());
            assert_eq!(status.worker_state, ProtocolInferenceWorkerState::Cold);
            assert_eq!(status.worker_pid, None);
            assert_eq!(status.launch_generation, None);
            assert_eq!(status.reserved_bytes, 0);
            assert_eq!(status.cooldown_not_before_unix_ms, None);
        }
        other => panic!("unexpected inference status event: {other:?}"),
    }
}

#[test]
fn inference_status_joins_explicit_manager_detail_without_private_payloads() {
    let test = TestRuntime::new();
    let control = Arc::new(InferenceControl::default());
    *control.manager_status.lock().unwrap() = Some(ModelManagerStatus {
        resident_models: 2,
        resident_contexts: 3,
        next_residency_deadline_after_ms: Some(1_234),
        resident_model_digests: vec!["0".repeat(64), "1".repeat(64)],
        resident_model_digests_truncated: true,
        automatic_context_unloads: 4,
        automatic_model_unloads: 5,
        manual_unloads: 6,
        unload_failures: 7,
        ..ModelManagerStatus::default()
    });
    let mut state = daemon(&test, control);

    let event = state.handle_request(request(DaemonRequestKind::InferenceStatus(
        InferenceStatusRequest { detail: true },
    )));
    match event.kind {
        DaemonEventKind::InferenceStatus(status) => {
            assert_eq!(status.resident_models, 2);
            assert_eq!(status.resident_contexts, 3);
            assert_eq!(status.next_residency_deadline_after_ms, Some(1_234));
            assert_eq!(
                status.resident_model_digests,
                Some(vec!["0".repeat(64), "1".repeat(64)])
            );
            assert_eq!(status.resident_model_digests_truncated, Some(true));
            assert_eq!(status.automatic_context_unloads, 4);
            assert_eq!(status.automatic_model_unloads, 5);
            assert_eq!(status.manual_unloads, 6);
            assert_eq!(status.unload_failures, 7);
            let wire = serde_json::to_string(&status).unwrap();
            assert!(!wire.contains("model_path"));
            assert!(!wire.contains("prompt"));
            assert!(!wire.contains("backend_log"));
        }
        other => panic!("unexpected inference status event: {other:?}"),
    }
}

#[test]
fn model_unload_dispatches_typed_targets_results_and_busy() {
    let test = TestRuntime::new();
    let control = Arc::new(InferenceControl::default());
    *control.unload_result.lock().unwrap() = Some(Ok(ModelUnloadResult {
        matched_models: 1,
        released_models: 1,
        released_contexts: 2,
        outcome: ManagerUnloadOutcome::Released,
    }));
    let mut state = daemon(&test, Arc::clone(&control));
    let digest = "a".repeat(64);

    let event = state.handle_request(request(DaemonRequestKind::ModelUnload(
        ModelUnloadRequest {
            target: ModelUnloadTarget::Digest {
                digest: digest.clone(),
            },
        },
    )));
    match event.kind {
        DaemonEventKind::ModelUnload(result) => {
            assert_eq!(result.matched_models, 1);
            assert_eq!(result.released_models, 1);
            assert_eq!(result.released_contexts, 2);
            assert_eq!(result.outcome, agl_protocol::ModelUnloadOutcome::Released);
        }
        other => panic!("unexpected model unload event: {other:?}"),
    }
    assert_eq!(
        control.unload_targets.lock().unwrap().as_slice(),
        [ManagerUnloadTarget::Digest(digest)]
    );

    *control.unload_result.lock().unwrap() = Some(Ok(ModelUnloadResult {
        matched_models: 0,
        released_models: 0,
        released_contexts: 0,
        outcome: ManagerUnloadOutcome::Busy,
    }));
    let event = state.handle_request(request(DaemonRequestKind::ModelUnload(
        ModelUnloadRequest {
            target: ModelUnloadTarget::All,
        },
    )));
    match event.kind {
        DaemonEventKind::Error(error) => {
            assert_eq!(error.code, ProtocolErrorCode::Busy);
            assert!(error.retryable);
        }
        other => panic!("unexpected busy event: {other:?}"),
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
artifact:
  schema: agentlibre.artifact/v1
  type: function
  id: coordinator
  version: 1.0.0
  payload_schema: agentlibre.function/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires:
    - function:reviewer@^1.0
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
    let reviewer_function_root = workspace.join(".agl/functions/reviewer");
    std::fs::create_dir_all(&reviewer_function_root).unwrap();
    std::fs::write(
        reviewer_function_root.join("FUNCTION.md"),
        r#"---
artifact:
  schema: agentlibre.artifact/v1
  type: function
  id: reviewer
  version: 1.0.0
  payload_schema: agentlibre.function/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires: []
title: Reviewer
description: Reviews one daemon task.
---
"#,
    )
    .unwrap();
    std::fs::write(
        reviewer_function_root.join("SYSTEM.md"),
        "Review one daemon task.\n",
    )
    .unwrap();
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
        WorkerRuntimeStatusHandle::default(),
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
fn oversized_activity_budget_is_rejected_before_root_run_persistence() {
    let mut test = TestRuntime::new();
    let workspace = test
        .runtime
        .paths
        .config_dir
        .parent()
        .unwrap()
        .join("activity-capacity-workspace");
    let function_root = workspace.join(".agl/functions/wide-coordinator");
    std::fs::create_dir_all(function_root.join("subagents")).unwrap();
    std::fs::write(
        function_root.join("FUNCTION.md"),
        r#"---
artifact:
  schema: agentlibre.artifact/v1
  type: function
  id: wide-coordinator
  version: 1.0.0
  payload_schema: agentlibre.function/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires:
    - function:reviewer@^1.0
title: Wide Coordinator
subagents:
  use:
    - reviewer
delegation:
  max_depth: 9
  max_children_per_run: 64
  max_descendants: 50
  max_total_output_tokens: 512
  timeout_seconds: 30
---
"#,
    )
    .unwrap();
    std::fs::write(function_root.join("SYSTEM.md"), "Delegate bounded work.\n").unwrap();
    let reviewer_function_root = workspace.join(".agl/functions/reviewer");
    std::fs::create_dir_all(&reviewer_function_root).unwrap();
    std::fs::write(
        reviewer_function_root.join("FUNCTION.md"),
        r#"---
artifact:
  schema: agentlibre.artifact/v1
  type: function
  id: reviewer
  version: 1.0.0
  payload_schema: agentlibre.function/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires: []
title: Reviewer
description: Reviews bounded work.
---
"#,
    )
    .unwrap();
    std::fs::write(
        reviewer_function_root.join("SYSTEM.md"),
        "Review bounded work.\n",
    )
    .unwrap();
    std::fs::write(
        function_root.join("subagents/reviewer.md"),
        r#"---
schema: agentlibre/subagent/v1
id: reviewer
title: Reviewer
description: Reviews bounded work.
model:
  inherit: true
tools:
  mode: read-only
  allow: []
  deny: []
subagents:
  use: []
limits:
  max_model_attempts: 1
  max_output_tokens: 32
  max_capability_calls: 1
  timeout_seconds: 10
---

Return a verdict.
"#,
    )
    .unwrap();
    test.inference.function_ref = Some("wide-coordinator".to_owned());
    test.inference.workspace_root = Some(workspace.clone());
    let mut state = daemon(&test, Arc::new(InferenceControl::default()));
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

    let event = state.handle_request(request(DaemonRequestKind::RunSubmit(RunSubmitRequest {
        session_id: session_id.clone(),
        content: agl_content::Content::text("Do not persist this run").unwrap(),
        client_submission_id: "activity-capacity-rejected".to_owned(),
        budget: RunBudgetRequest::default(),
    })));
    assert!(matches!(
        event.kind,
        DaemonEventKind::Error(ref error)
            if error.code == ProtocolErrorCode::ActivityCapacityExceeded && !error.retryable
    ));

    let key = agl_store::RunConcurrencyKey::session(&session_id).unwrap();
    let store = AglStore::open_current_read_only_at(test.runtime.paths.store_root()).unwrap();
    assert!(
        store
            .safe_runs_for_concurrency_key(&key, true)
            .unwrap()
            .is_empty(),
        "activity-capacity rejection must happen before durable run persistence"
    );
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
        agl_app::ApplicationToolResult::SessionExited {
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
        agl_app::ApplicationToolResult::SessionExited {
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
fn daemon_event_constructor_keeps_current_schema() {
    let event = DaemonEvent::new(
        None,
        DaemonEventKind::SessionList(agl_protocol::SessionListEvent {
            sessions: Vec::new(),
        }),
    );
    assert_eq!(event.schema, agl_protocol::EVENT_SCHEMA);
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
        WorkerRuntimeStatusHandle::default(),
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
        WorkerRuntimeStatusHandle::default(),
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
