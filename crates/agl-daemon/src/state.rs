use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
    mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agl_app::{
    ActiveRunView, ApplicationAction, ApplicationActionRequest, ApplicationCallContext,
    ApplicationError, ApplicationErrorCode, ApplicationToolResult, CommandContext,
    ContinueActionView, ContinueUnavailableReason, ExecutionView, HumanTerminalEnsure,
    IncompleteAssistantItemView, IncompleteOutputReason, MAX_ACTIVE_ACTIVITY_BYTES,
    MAX_ACTIVE_ACTIVITY_NODES, MAX_ACTIVITY_NODE_BYTES, MAX_ACTIVITY_PATH_NODES,
    MAX_PRESENTATION_CONTENT_BYTES, MAX_PRESENTATION_ITEMS, PresentationCursor,
    PresentationSnapshotPage, PromptAdmission, PromptAdmissionState, PromptSubmission,
    QueuedPromptView, SanitizedDisplayPath, SessionHeader, SessionOpen, SessionOpened,
    SessionPresentationItem, SessionPresentationSnapshot, SessionPresentationStatus,
    ShellProfileView, SuggestionPage, SuggestionRequest, TerminalEnsureDisposition,
    TerminalEnsured, TerminalOwnerView, TerminalSessionView, TerminalWriterView,
};
use agl_chat::{
    ChatOptions, ChatRunInput, ChatService, ChatSupervisorFactory, InferenceClientHandle,
    InferenceOptions, ToolAccessMode as ChatToolMode,
};
use agl_cron::{CronJob, CronTargetKind, STORE_STATUS_BUILTIN_CRON_TARGET};
use agl_exec::{
    CallerNamespace, CallerOwner, CallerOwnerKind, CallerRole, ExecutionAuthorization,
    ExecutionGrantLease, ExecutionId, ExecutionLeaseOrigin, ExecutionLimits, ExecutionListFilter,
    ExecutionOwner, ExecutionProfile, ExecutionState, KillMode,
    LOCAL_OPERATOR_TERMINAL_LEASE_DURATION, OpaqueOwnerId, ProcessError, ProcessErrorCode,
};
use agl_function::RuntimeDelegationPlan;
use agl_ids::{DaemonInstanceId, EventId, MessageId, RequestId, RunId, SessionId, StepId, TurnId};
use agl_inference::ENGINE_PROTOCOL_ID;
use agl_inference::worker_supervisor::WorkerLifecyclePhase;
use agl_inference::{
    EngineRuntimeStatusHandle, InferenceDeviceKind, ModelManagerStatus, ModelManagerStatusDetail,
    ModelReleaseOutcome as ManagerReleaseOutcome, ModelReleaseReason as ManagerReleaseReason,
    ModelUnloadOutcome as ManagerUnloadOutcome, ModelUnloadTarget as ManagerUnloadTarget,
};
use agl_protocol::{
    DaemonEvent, DaemonEventKind, DaemonRequest, DaemonRequestKind, DaemonTool, HelloEvent,
    HelloRequest, InferenceDeviceEvent, InferenceInventoryEvent, InferenceStatusEvent,
    InferenceStatusRequest, ModelReleaseOutcome, ModelReleaseReason, ModelUnloadEvent,
    ModelUnloadOutcome, ModelUnloadRequest, ModelUnloadTarget, PROTOCOL_VERSION, ProtocolError,
    ProtocolErrorCode, ProtocolErrorDetails, ProtocolInferenceDeviceKind,
    ProtocolInferenceEngineState, ProtocolRunKind, ProtocolRunState, ProtocolToolMode,
    RunAcceptedEvent, RunEventsEvent, RunStatusEvent, RunTreeEvent, RunTreeNodeEvent,
    RunUsageEvent, RuntimeGenerationIdentity, RuntimeGenerationKind,
    SessionActiveWorkCancelledEvent, SessionFinishedEvent, SessionListEvent, SessionOpenedEvent,
    SessionStatus, SessionStatusEvent, SessionSummary, SessionTranscriptEvent,
};
use agl_runtime::{
    AgentLibreRuntimeConfig, CurrentRuntimeIdentity, RuntimeIdentityKind, current_runtime_identity,
};
use agl_session::{
    ChatSessionReverseRead, ChatSessionReverseReader, ChatSessionStore, SessionCatalogStatus,
};
use agl_store::{AglStore, RunBudget, RunKind, RunState, SafeRunStatus};
use agl_supervisor::{
    IdempotentRunSpec, RunAccepted, RunOutcome, RunSpec, RunSubscription, Supervisor,
    SupervisorHandle, SupervisorOptions,
};
use agl_terminal::environment::{
    TerminalEnvironmentRequest, TerminalEnvironmentValue, TerminalSecretReference,
};
use agl_terminal::{
    AdmittedShellKind, AdmittedShellProfile, HostStartupPolicy as ProcessHostStartupPolicy,
    TerminalId, TerminalOwner, TerminalPromptState as ProcessTerminalPromptState, TerminalRecord,
    TerminalState, TerminalTopologyId,
};
use agl_terminal_protocol::TerminalAdmission;
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::run_factory::{BuiltinCronRunInput, DaemonRunFactory};
use crate::terminal_bridge::TerminalBridge;
use crate::transcript::transcript_event;

const RUN_SUBMIT_IDEMPOTENCY_NAMESPACE: &str = "daemon.run_submit";
const INCOMPLETE_CONTINUE_IDEMPOTENCY_NAMESPACE: &str = "daemon.incomplete_continue";
const CRON_RUN_IDEMPOTENCY_NAMESPACE: &str = "daemon.cron_run";
const MAX_QUEUED_PROMPTS_PER_SESSION: usize = 32;
const SESSION_EXIT_RUN_CANCEL_TIMEOUT: Duration = Duration::from_secs(30);
const SESSION_EXIT_RUN_POLL_INTERVAL: Duration = Duration::from_millis(10);
const PRESENTATION_PAGE_CURSOR_PREFIX: &str = "p1";
const MAX_PRESENTATION_TRANSCRIPT_RECORD_BYTES: usize = MAX_PRESENTATION_CONTENT_BYTES;
const MAX_PRESENTATION_TRANSCRIPT_SCAN_BYTES: usize = 2 * MAX_PRESENTATION_CONTENT_BYTES;
const MAX_PRESENTATION_TRANSCRIPT_SCAN_RECORDS: usize = 8 * MAX_PRESENTATION_ITEMS;
const DAEMON_STATE_QUEUE_CAPACITY: usize = 32;
const ROOT_ACTIVE_ACTIVITY_NODES: usize = 4;
const DESCENDANT_ACTIVE_ACTIVITY_NODES: usize = 5;
const ROOT_ACTIVITY_PATH_NODES: usize = 4;
const DESCENDANT_ACTIVITY_PATH_NODES: usize = 3;
const MAX_RESERVED_ACTIVITY_NODE_ID_BYTES: usize = 512;
const ACTIVE_ACTIVITY_ENCODING_OVERHEAD_BYTES: usize = 4 * 1024;

pub struct DaemonState {
    daemon_instance_id: DaemonInstanceId,
    runtime_identity: CurrentRuntimeIdentity,
    runtime: AgentLibreRuntimeConfig,
    inference_defaults: InferenceOptions,
    inference_client: InferenceClientHandle,
    inference_status: EngineRuntimeStatusHandle,
    sessions: BTreeMap<SessionId, SessionRuntime>,
    chat_factory: ChatSupervisorFactory,
    presentation_proxy: agl_app::TurnPresentationProxy,
    terminal_bridge: TerminalBridge,
    terminal_presentations: BTreeMap<TerminalId, TerminalPresentationMetadata>,
    human_terminal_submissions: BTreeMap<(SessionId, String), HumanTerminalSubmission>,
    exiting_sessions: BTreeSet<SessionId>,
    _supervisor: Supervisor,
    supervisor_handle: SupervisorHandle,
}

#[derive(Clone)]
struct TerminalPresentationMetadata {
    environment_names: Vec<String>,
    admission_fingerprint: String,
}

#[derive(Clone)]
struct HumanTerminalSubmission {
    fingerprint: String,
    terminal_id: TerminalId,
}

#[derive(Clone)]
struct IncompleteContinuationClaim {
    client_submission_id: String,
    continuation_run_id: RunId,
    continuation_turn_id: TurnId,
    continuation_request_id: RequestId,
}

struct IncompleteProjectionContext {
    status: SessionPresentationStatus,
    execution_context_revision: u64,
    runtime_context_revision: u64,
    current_policy_hash: String,
    claims: BTreeMap<agl_ids::MessageId, IncompleteContinuationClaim>,
    current_context_messages: BTreeSet<agl_ids::MessageId>,
}

struct IncompleteReplayIndex {
    claims: BTreeMap<agl_ids::MessageId, IncompleteContinuationClaim>,
    claim_order: Vec<agl_ids::MessageId>,
    current_context_messages: BTreeSet<agl_ids::MessageId>,
}

#[derive(Clone)]
struct IncompleteContinuationSource {
    message_id: MessageId,
    source_run_id: RunId,
    source_turn_id: TurnId,
    continuation_index: u16,
    execution_context_revision: u64,
    runtime_context_revision: u64,
    policy_hash: String,
}

#[derive(Clone, Copy)]
enum HumanTerminalAuthority {
    Workspace,
    LocalOperatorHost { operator_uid: u32 },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SessionTerminationCounts {
    terminals: u32,
    executions: u32,
}

struct SessionRunCancellation {
    run_ids: Vec<RunId>,
    supervisor: SupervisorHandle,
}

struct SessionExecutionTermination {
    live_terminals: Vec<(TerminalTopologyId, TerminalId)>,
    execution_ids: Vec<ExecutionId>,
    counts: SessionTerminationCounts,
    human_terminals: TerminalBridge,
    agent_executions: TerminalBridge,
    timeout: Duration,
    poll_interval: Duration,
}

struct SessionExitPlan {
    session_id: SessionId,
    reason: agl_protocol::SessionFinishReason,
    runs: SessionRunCancellation,
    executions: SessionExecutionTermination,
}

struct SessionExitOutcome {
    cancelled_runs: u32,
    terminated: SessionTerminationCounts,
}

impl SessionRunCancellation {
    fn wait(&self, context: &ApplicationCallContext) -> Result<u32, ApplicationError> {
        if self.run_ids.is_empty() {
            return Ok(0);
        }
        let deadline = Instant::now() + SESSION_EXIT_RUN_CANCEL_TIMEOUT;
        loop {
            ensure_application_call_live(context)?;
            let mut cancelled = 0usize;
            let mut pending = false;
            for run_id in &self.run_ids {
                let status = self
                    .supervisor
                    .status(run_id.clone())
                    .map_err(supervisor_error)
                    .map_err(application_protocol_error)?
                    .ok_or_else(|| {
                        ApplicationError::new(
                            ApplicationErrorCode::OutcomeUnknown,
                            format!("cancelled root run `{run_id}` disappeared"),
                        )
                    })?;
                if status.state.is_terminal() {
                    cancelled += usize::from(status.state == RunState::Cancelled);
                } else {
                    pending = true;
                }
            }
            if !pending {
                return Ok(bounded_count(cancelled));
            }
            if Instant::now() >= deadline {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::OutcomeUnknown,
                    "session root-run cancellation did not reach typed outcomes before the deadline",
                ));
            }
            std::thread::sleep(SESSION_EXIT_RUN_POLL_INTERVAL);
        }
    }
}

impl SessionExecutionTermination {
    fn wait(&self, context: &ApplicationCallContext) -> Result<(), ApplicationError> {
        if !self.execution_ids.is_empty() {
            let deadline = Instant::now() + self.timeout;
            loop {
                ensure_application_call_live(context)?;
                let mut pending = false;
                for execution_id in &self.execution_ids {
                    let status = self
                        .agent_executions
                        .execution_status(execution_id.clone())
                        .map_err(application_runtime_error)?;
                    if status.state == ExecutionState::OutcomeUnknown {
                        return Err(ApplicationError::new(
                            ApplicationErrorCode::OutcomeUnknown,
                            "execution termination outcome is unknown",
                        ));
                    }
                    pending |= !status.state.is_terminal();
                }
                if !pending {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(ApplicationError::new(
                        ApplicationErrorCode::OutcomeUnknown,
                        "execution termination did not reach a typed outcome before the deadline",
                    ));
                }
                std::thread::sleep(self.poll_interval);
            }
        }
        for (topology_id, terminal_id) in &self.live_terminals {
            ensure_application_call_live(context)?;
            let refreshed = self
                .human_terminals
                .record(topology_id.clone(), terminal_id)
                .map_err(application_runtime_error)?;
            if refreshed.state.is_live() || refreshed.state == TerminalState::OutcomeUnknown {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::OutcomeUnknown,
                    "terminal termination outcome is not confirmed",
                ));
            }
        }
        Ok(())
    }
}

impl SessionExitPlan {
    fn wait(
        &self,
        context: &ApplicationCallContext,
    ) -> Result<SessionExitOutcome, ApplicationError> {
        let cancelled_runs = self.runs.wait(context)?;
        self.executions.wait(context)?;
        Ok(SessionExitOutcome {
            cancelled_runs,
            terminated: self.executions.counts,
        })
    }
}

#[derive(Clone)]
struct SessionRuntime {
    status: SessionStatus,
    resumed: bool,
    options: ChatOptions,
    delegation_plan: Option<RuntimeDelegationPlan>,
    execution_context: agl_exec::ExecutionContextSnapshot,
    selected_model_id: Option<String>,
    runtime_context_revision: u64,
}

impl DaemonState {
    pub fn new(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
        inference_status: EngineRuntimeStatusHandle,
    ) -> Self {
        Self::open(
            runtime,
            inference_defaults,
            inference_client,
            inference_status,
        )
        .expect("test daemon state should initialize")
    }

    pub fn open(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
        inference_status: EngineRuntimeStatusHandle,
    ) -> Result<Self> {
        let runtime_identity =
            current_runtime_identity().context("failed to verify daemon runtime identity")?;
        Self::open_with_runtime_identity(
            runtime,
            inference_defaults,
            inference_client,
            inference_status,
            runtime_identity,
        )
    }

    #[doc(hidden)]
    pub fn open_with_runtime_identity(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
        inference_status: EngineRuntimeStatusHandle,
        runtime_identity: CurrentRuntimeIdentity,
    ) -> Result<Self> {
        if runtime_identity.sealed() {
            ensure!(
                runtime_identity.engine_protocol_id() == Some(ENGINE_PROTOCOL_ID),
                "sealed daemon runtime manifest engine protocol ID does not match this daemon"
            );
        }
        let store_root = runtime.paths.store_root();
        let terminal_bridge =
            TerminalBridge::daemon(runtime.execution.terminal_endpoint(&runtime.paths)?)
                .context("failed to configure terminal service adapter")?;
        let presentation_proxy = agl_app::TurnPresentationProxy::new();
        let chat_factory = ChatSupervisorFactory::with_runtime(
            &store_root,
            runtime.clone(),
            inference_client.clone(),
        )
        .with_presentation_sink(Arc::new(presentation_proxy.clone()));
        let supervisor = Supervisor::spawn(
            &store_root,
            Arc::new(DaemonRunFactory::new(chat_factory.clone(), &store_root)),
            SupervisorOptions::default(),
        )
        .context("failed to start durable run supervisor")?;
        let supervisor_handle = supervisor.handle();
        Ok(Self {
            daemon_instance_id: DaemonInstanceId::generate(),
            runtime_identity,
            runtime,
            inference_defaults,
            inference_client,
            inference_status,
            sessions: BTreeMap::new(),
            chat_factory,
            presentation_proxy,
            terminal_bridge,
            terminal_presentations: BTreeMap::new(),
            human_terminal_submissions: BTreeMap::new(),
            exiting_sessions: BTreeSet::new(),
            _supervisor: supervisor,
            supervisor_handle,
        })
    }

    pub fn handle_request(&mut self, request: DaemonRequest) -> DaemonEvent {
        self.handle_request_with_context(request, &ApplicationCallContext::new())
    }

    fn handle_request_with_context(
        &mut self,
        request: DaemonRequest,
        context: &ApplicationCallContext,
    ) -> DaemonEvent {
        let request_id = request.request_id;
        let result = match request.kind {
            DaemonRequestKind::Hello(request) => self.hello(request),
            DaemonRequestKind::SessionOpen(request) => self.open_session(request),
            DaemonRequestKind::SessionClear(request) => self.clear_session(request.session_id),
            DaemonRequestKind::SessionCancelActive(request) => self
                .cancel_active_session_work(request.session_id, context)
                .map_err(protocol_application_error),
            DaemonRequestKind::SessionFinish(request) => {
                self.finish_session(request.session_id, request.reason, context)
            }
            DaemonRequestKind::SessionStatus(request) => self.session_status(request.session_id),
            DaemonRequestKind::SessionList(_) => self.session_list(),
            DaemonRequestKind::SessionTranscript(request) => {
                self.session_transcript(request.session_id, request.include_content)
            }
            DaemonRequestKind::RunSubmit(request) => self.submit_run(request_id.clone(), request),
            DaemonRequestKind::RunStatus(request) => self.run_status(request.run_id),
            DaemonRequestKind::RunTree(request) => self.run_tree(request.run_id),
            DaemonRequestKind::RunCancel(request) => self.cancel_run(request.run_id),
            DaemonRequestKind::RunEvents(request) => {
                self.run_events(request.run_id, request.after_sequence, request.limit)
            }
            DaemonRequestKind::InferenceInventory(_) => self.inference_inventory(),
            DaemonRequestKind::InferenceStatus(request) => self.inference_status(request),
            DaemonRequestKind::ModelUnload(request) => self.model_unload(request),
            DaemonRequestKind::RunSubscribe(_) => Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "run_subscribe must be handled by the streaming transport",
                false,
            )),
            DaemonRequestKind::CommandCatalog(_)
            | DaemonRequestKind::CommandSuggestions(_)
            | DaemonRequestKind::ApplicationAction(_)
            | DaemonRequestKind::SessionPresentation(_)
            | DaemonRequestKind::SessionPresentationSubscribe(_)
            | DaemonRequestKind::SubscriptionCancel(_)
            | DaemonRequestKind::HumanTerminalEnsure(_)
            | DaemonRequestKind::HumanHostTerminalEnsure(_) => Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "interactive surface operations must be handled by the private connection adapter",
                false,
            )),
        };
        DaemonEvent::new(
            Some(request_id),
            result.unwrap_or_else(DaemonEventKind::Error),
        )
    }

    pub fn subscribe_run(
        &self,
        run_id: RunId,
        after_sequence: u64,
    ) -> Result<RunSubscription, ProtocolError> {
        self.supervisor_handle
            .subscribe(run_id, after_sequence)
            .map_err(supervisor_error)
    }

    pub fn run_outcome(&self, run_id: RunId) -> Result<RunOutcome, ProtocolError> {
        self.supervisor_handle
            .outcome(run_id.clone())
            .map_err(supervisor_error)?
            .ok_or_else(|| not_found(run_id.as_str()))
    }

    pub fn model_manager_status(&self) -> Result<ModelManagerStatus> {
        self.inference_client.status()
    }

    pub fn supervisor_handle(&self) -> SupervisorHandle {
        self.supervisor_handle.clone()
    }

    pub fn submit_cron_job(
        &mut self,
        job: &CronJob,
        scheduled_for: &str,
    ) -> Result<RunAccepted, ProtocolError> {
        let run_id = RunId::generate();
        let (session_id, turn_id, input, registered_session, execution_context) =
            match job.target_kind {
                CronTargetKind::Builtin => {
                    if job.target_ref != STORE_STATUS_BUILTIN_CRON_TARGET {
                        return Err(invalid(format!(
                            "unsupported builtin cron target {}",
                            job.target_ref
                        )));
                    }
                    let workspace = self
                        .runtime
                        .resolve_workspace_root(None)
                        .map_err(runtime_error)?;
                    (
                        None,
                        None,
                        serde_json::to_value(BuiltinCronRunInput {
                            builtin: job.target_ref.clone(),
                        })
                        .map_err(runtime_error)?,
                        None,
                        self.runtime
                            .execution
                            .context_snapshot(&workspace)
                            .map_err(runtime_error)?,
                    )
                }
                CronTargetKind::Skill => {
                    let prompt =
                        crate::scheduler::render_cron_skill_prompt(job).map_err(runtime_error)?;
                    let mut inference = self.inference_defaults.clone();
                    inference.skills.push(job.target_ref.clone());
                    inference.tool_mode = ChatToolMode::Write;
                    let options = ChatOptions {
                        inference,
                        workspace_root: None,
                        session_id: None,
                        no_history: false,
                        new_session: true,
                    };
                    let service = ChatService::open(
                        options.clone(),
                        &self.runtime,
                        self.inference_client.clone(),
                    )
                    .map_err(runtime_error)?;
                    let session_id = service.session_id().clone();
                    let execution_context = service.execution_context().clone();
                    let delegation_plan = service.delegation_plan();
                    validate_root_activity_capacity_protocol(delegation_plan.as_ref())?;
                    let turn_id = TurnId::generate();
                    self.chat_factory.register(service).map_err(runtime_error)?;
                    let persisted_options = ChatOptions {
                        session_id: Some(session_id.clone()),
                        new_session: false,
                        ..options
                    };
                    (
                        Some(session_id.clone()),
                        Some(turn_id),
                        serde_json::to_value(ChatRunInput::Root {
                            content: agl_content::Content::text(prompt).map_err(runtime_error)?,
                            request_id: None,
                            options: persisted_options,
                            delegation_plan,
                        })
                        .map_err(runtime_error)?,
                        Some(session_id),
                        execution_context,
                    )
                }
            };
        let accepted = self
            .supervisor_handle
            .submit(RunSpec {
                run: agl_store::DurableRunDraft {
                    run_id,
                    session_id,
                    turn_id,
                    kind: agl_store::RunKind::Cron,
                    priority: 0,
                    concurrency_key: None,
                    input,
                    checkpoint: None,
                    effective_policy_hash: None,
                    execution_context,
                    budget: RunBudget::default(),
                    not_before_ms: None,
                },
                idempotency: Some(IdempotentRunSpec {
                    namespace: CRON_RUN_IDEMPOTENCY_NAMESPACE.to_string(),
                    key: format!("{}:{scheduled_for}", job.id),
                    fingerprint: cron_fingerprint(job, scheduled_for),
                }),
            })
            .map_err(supervisor_error)?;
        if accepted.replayed
            && let Some(session_id) = registered_session
        {
            let _ = self.chat_factory.unregister(&session_id);
        }
        Ok(accepted)
    }

    fn hello(&self, request: HelloRequest) -> Result<DaemonEventKind, ProtocolError> {
        if !request.accepted_protocol_versions.is_empty()
            && !request
                .accepted_protocol_versions
                .iter()
                .any(|version| version == PROTOCOL_VERSION)
        {
            return Err(ProtocolError::new(
                ProtocolErrorCode::UnsupportedProtocolVersion,
                "client does not accept the daemon protocol version",
                false,
            ));
        }
        let daemon_runtime = protocol_runtime_identity(&self.runtime_identity);
        if let Some(client_runtime) = request.client_runtime
            && client_runtime != daemon_runtime
        {
            return Err(ProtocolError::new(
                ProtocolErrorCode::RuntimeIdentityMismatch,
                "first-party client and daemon runtime identities do not match",
                false,
            )
            .with_details(ProtocolErrorDetails::RuntimeIdentityMismatch {
                client: client_runtime,
                daemon: daemon_runtime,
            }));
        }
        Ok(DaemonEventKind::Hello(HelloEvent {
            protocol_version: PROTOCOL_VERSION.to_string(),
            product_version: env!("CARGO_PKG_VERSION").to_string(),
            daemon_instance_id: self.daemon_instance_id.clone(),
            daemon_runtime,
            engine_protocol_id: ENGINE_PROTOCOL_ID.to_string(),
            tools: vec![
                DaemonTool::SessionOpen,
                DaemonTool::SessionClear,
                DaemonTool::SessionCancelActive,
                DaemonTool::SessionFinish,
                DaemonTool::SessionStatus,
                DaemonTool::SessionList,
                DaemonTool::SessionTranscript,
                DaemonTool::FinalAssistantMessage,
                DaemonTool::RuntimeEvents,
                DaemonTool::RunSubmit,
                DaemonTool::RunStatus,
                DaemonTool::RunTree,
                DaemonTool::RunCancel,
                DaemonTool::RunReplay,
                DaemonTool::RunSubscribe,
                DaemonTool::InferenceInventory,
                DaemonTool::InferenceStatus,
                DaemonTool::ModelUnload,
                DaemonTool::CommandCatalog,
                DaemonTool::CommandSuggestions,
                DaemonTool::ApplicationActions,
                DaemonTool::SessionPresentation,
                DaemonTool::HumanTerminal,
                DaemonTool::AssistantDeltas,
            ],
        }))
    }

    fn inference_inventory(&self) -> Result<DaemonEventKind, ProtocolError> {
        let devices = self
            .inference_client
            .device_inventory()
            .map_err(runtime_error)?
            .into_iter()
            .map(|device| InferenceDeviceEvent {
                physical_device_id: device.physical_device_id,
                pci_device_id: device.pci_device_id,
                pci_subsystem_id: device.pci_subsystem_id,
                driver_build_id: device.driver_build_id,
                backend_name: device.backend_name,
                description: device.description,
                kind: match device.kind {
                    InferenceDeviceKind::Cpu => ProtocolInferenceDeviceKind::Cpu,
                    InferenceDeviceKind::DiscreteGpu => ProtocolInferenceDeviceKind::DiscreteGpu,
                    InferenceDeviceKind::IntegratedGpu => {
                        ProtocolInferenceDeviceKind::IntegratedGpu
                    }
                    InferenceDeviceKind::Accelerator => ProtocolInferenceDeviceKind::Accelerator,
                    InferenceDeviceKind::Metadata => ProtocolInferenceDeviceKind::Metadata,
                    InferenceDeviceKind::Unknown => ProtocolInferenceDeviceKind::Unknown,
                },
                free_memory_bytes: device.free_memory_bytes,
                total_memory_bytes: device.total_memory_bytes,
                usable: device.usable,
                supports_gpu_offload: device.supports_gpu_offload,
            })
            .collect();
        Ok(DaemonEventKind::InferenceInventory(
            InferenceInventoryEvent { devices },
        ))
    }

    fn inference_status(
        &self,
        request: InferenceStatusRequest,
    ) -> Result<DaemonEventKind, ProtocolError> {
        let manager = self
            .inference_client
            .status_with_detail(if request.detail {
                ModelManagerStatusDetail::ModelDigests
            } else {
                ModelManagerStatusDetail::Aggregate
            })
            .map_err(runtime_error)?;
        let worker = self.inference_status.snapshot();
        Ok(DaemonEventKind::InferenceStatus(InferenceStatusEvent {
            engine_protocol_id: worker.engine_protocol_id().to_owned(),
            engine_state: match worker.phase() {
                WorkerLifecyclePhase::Cold => ProtocolInferenceEngineState::Cold,
                WorkerLifecyclePhase::Starting => ProtocolInferenceEngineState::Starting,
                WorkerLifecyclePhase::Ready => ProtocolInferenceEngineState::Ready,
                WorkerLifecyclePhase::Busy => ProtocolInferenceEngineState::Busy,
                WorkerLifecyclePhase::CoolingDown => ProtocolInferenceEngineState::CoolingDown,
            },
            engine_pid: worker.engine_pid(),
            engine_generation: worker.launch_generation(),
            physical_device_id: worker.physical_device_id().map(str::to_owned),
            reserved_bytes: worker.reserved_bytes(),
            cooldown_not_before_unix_ms: worker.cooldown_not_before_unix_ms(),
            resident_models: u32::try_from(manager.resident_models).unwrap_or(u32::MAX),
            resident_contexts: u32::try_from(manager.resident_contexts).unwrap_or(u32::MAX),
            next_residency_deadline_after_ms: manager.next_residency_deadline_after_ms,
            last_release_reason: manager.last_release_reason.map(protocol_release_reason),
            last_release_outcome: manager.last_release_outcome.map(protocol_release_outcome),
            automatic_context_unloads: manager.automatic_context_unloads,
            automatic_model_unloads: manager.automatic_model_unloads,
            manual_unloads: manager.manual_unloads,
            unload_failures: manager.unload_failures,
            resident_model_digests: request.detail.then_some(manager.resident_model_digests),
            resident_model_digests_truncated: request
                .detail
                .then_some(manager.resident_model_digests_truncated),
        }))
    }

    fn model_unload(&self, request: ModelUnloadRequest) -> Result<DaemonEventKind, ProtocolError> {
        let target = match request.target {
            ModelUnloadTarget::All => ManagerUnloadTarget::All,
            ModelUnloadTarget::Digest { digest } => {
                ManagerUnloadTarget::digest(digest).map_err(runtime_error)?
            }
        };
        let result = self
            .inference_client
            .unload(target)
            .map_err(runtime_error)?;
        let outcome = match result.outcome {
            ManagerUnloadOutcome::Released => ModelUnloadOutcome::Released,
            ManagerUnloadOutcome::NotResident => ModelUnloadOutcome::NotResident,
            ManagerUnloadOutcome::Busy => {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::Busy,
                    "active model cannot be unloaded",
                    true,
                ));
            }
        };
        Ok(DaemonEventKind::ModelUnload(ModelUnloadEvent {
            matched_models: result.matched_models,
            released_models: result.released_models,
            released_contexts: result.released_contexts,
            outcome,
        }))
    }

    fn open_session(
        &mut self,
        request: agl_protocol::SessionOpenRequest,
    ) -> Result<DaemonEventKind, ProtocolError> {
        if request.new_session && request.session_id.is_some() {
            return Err(invalid("new session cannot include session_id"));
        }
        if !request.new_session
            && let Some(session_id) = request.session_id.as_ref()
            && self.sessions.contains_key(session_id)
            && self.chat_factory.has_session(session_id)
        {
            self.reconcile_incomplete_continuations_for_session(session_id)
                .map_err(protocol_application_error)?;
            return Ok(DaemonEventKind::SessionOpened(SessionOpenedEvent {
                session_id: session_id.clone(),
                resumed: true,
            }));
        }
        let persisted = if !request.new_session
            && let Some(session_id) = request.session_id.as_ref()
            && ChatSessionStore::exists(self.runtime.paths.sessions_root(), session_id)
        {
            Some(
                ChatSessionStore::open(self.runtime.paths.sessions_root(), session_id.clone())
                    .map_err(runtime_error)?,
            )
        } else {
            None
        };
        let persisted_selection = persisted
            .as_ref()
            .map(|store| store.runtime_selection().clone());
        let resumed_workspace = request
            .workspace_root
            .is_none()
            .then(|| {
                persisted
                    .as_ref()
                    .map(|store| store.execution_context().workspace_root.clone())
            })
            .flatten();
        let workspace_root = request
            .workspace_root
            .map(PathBuf::from)
            .or(resumed_workspace);
        let restored_mode = persisted_selection
            .as_ref()
            .map(|selection| protocol_tool_mode(&selection.operation_mode))
            .transpose()?
            .unwrap_or(request.tool_mode);
        let restored_skills = persisted_selection
            .as_ref()
            .map(|selection| selection.skill_ids.clone())
            .unwrap_or(request.skills);
        let restored_function = persisted_selection
            .as_ref()
            .and_then(|selection| selection.function_ref.clone())
            .or(request.function_ref)
            .or_else(|| self.inference_defaults.function_ref.clone());
        let options = ChatOptions {
            inference: InferenceOptions {
                skills: restored_skills,
                tool_mode: chat_tool_mode(restored_mode),
                workspace_root: workspace_root.clone(),
                function_ref: restored_function,
                ..self.inference_defaults.clone()
            },
            workspace_root,
            session_id: request.session_id,
            no_history: false,
            new_session: request.new_session,
        };
        let mut service = ChatService::open(
            options.clone(),
            &self.runtime,
            self.inference_client.clone(),
        )
        .map_err(runtime_error)?;
        if let Some(model_id) = persisted_selection
            .as_ref()
            .and_then(|selection| selection.model_id.as_deref())
            && service.selected_model_id().as_deref() != Some(model_id)
        {
            let parsed = agl_config::ModelId::new(model_id.to_owned()).map_err(runtime_error)?;
            let bindings = agl_config::load_model_bindings_or_empty(
                agl_config::model_bindings_path(&self.runtime.paths.config_dir),
            )
            .map_err(runtime_error)?;
            let binding = bindings
                .models
                .get(&parsed)
                .ok_or_else(|| invalid(format!("persisted model `{model_id}` is not installed")))?;
            service
                .select_model(model_id, binding.path.clone())
                .map_err(runtime_error)?;
        }
        let summary = service.summary();
        let selected_model_id = service.selected_model_id();
        let runtime_context_revision = service.runtime_selection_revision();
        let execution_context = service.execution_context().clone();
        let delegation_plan = service.delegation_plan();
        let session_id = summary.session_id.clone();
        self.chat_factory.register(service).map_err(runtime_error)?;
        self.sessions.insert(
            session_id.clone(),
            SessionRuntime {
                status: SessionStatus::Open,
                resumed: summary.resumed,
                options: ChatOptions {
                    session_id: Some(session_id.clone()),
                    new_session: false,
                    ..options
                },
                delegation_plan,
                execution_context,
                selected_model_id,
                runtime_context_revision,
            },
        );
        self.reconcile_incomplete_continuations_for_session(&session_id)
            .map_err(protocol_application_error)?;
        Ok(DaemonEventKind::SessionOpened(SessionOpenedEvent {
            session_id,
            resumed: summary.resumed,
        }))
    }

    fn clear_session(&mut self, session_id: SessionId) -> Result<DaemonEventKind, ProtocolError> {
        let session = self
            .sessions
            .get(&session_id)
            .ok_or_else(|| not_found(session_id.as_str()))?;
        if matches!(
            session.status,
            SessionStatus::Finished | SessionStatus::Failed
        ) {
            return Err(invalid("cannot clear a terminal session"));
        }
        self.chat_factory
            .with_session(&session_id, |service| service.clear_context().map(|_| ()))
            .map_err(|error| busy_or_runtime(error, "session has an active durable run"))?;
        Ok(DaemonEventKind::SessionStatus(SessionStatusEvent {
            session_id,
            status: SessionStatus::Open,
        }))
    }

    fn finish_session(
        &mut self,
        session_id: SessionId,
        reason: agl_protocol::SessionFinishReason,
        context: &ApplicationCallContext,
    ) -> Result<DaemonEventKind, ProtocolError> {
        let active_runs = self
            .active_session_root_runs(&session_id)
            .map_err(protocol_application_error)?;
        self.cancel_session_root_runs(active_runs, context)
            .map_err(protocol_application_error)?;
        self.finish_session_with_counts(session_id, reason, context)
            .map(|(event, _)| event)
            .map_err(protocol_application_error)
    }

    fn session_status(&self, session_id: SessionId) -> Result<DaemonEventKind, ProtocolError> {
        let status = self
            .sessions
            .get(&session_id)
            .map(|session| session.status)
            .ok_or_else(|| not_found(session_id.as_str()))?;
        Ok(DaemonEventKind::SessionStatus(SessionStatusEvent {
            session_id,
            status,
        }))
    }

    fn session_list(&self) -> Result<DaemonEventKind, ProtocolError> {
        let mut sessions = BTreeMap::new();
        for entry in
            ChatSessionStore::catalog(self.runtime.paths.sessions_root()).map_err(runtime_error)?
        {
            sessions.insert(
                entry.metadata.session_id.clone(),
                SessionSummary {
                    session_id: entry.metadata.session_id,
                    title: None,
                    status: match entry.status {
                        SessionCatalogStatus::Active => SessionStatus::Open,
                        SessionCatalogStatus::Finished => SessionStatus::Finished,
                        SessionCatalogStatus::Failed => SessionStatus::Failed,
                    },
                    updated_at_unix_ms: u64::try_from(entry.metadata.updated_at_unix_ms)
                        .map_err(|_| runtime_error("session timestamp exceeds the wire bound"))?,
                },
            );
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        for (session_id, session) in &self.sessions {
            sessions.insert(
                session_id.clone(),
                SessionSummary {
                    session_id: session_id.clone(),
                    title: None,
                    status: session.status,
                    updated_at_unix_ms: now,
                },
            );
        }
        Ok(DaemonEventKind::SessionList(SessionListEvent {
            sessions: sessions.into_values().collect(),
        }))
    }

    fn session_transcript(
        &self,
        session_id: SessionId,
        include_content: bool,
    ) -> Result<DaemonEventKind, ProtocolError> {
        if !ChatSessionStore::exists(self.runtime.paths.sessions_root(), &session_id) {
            return Err(not_found(session_id.as_str()));
        }
        let store = ChatSessionStore::open(self.runtime.paths.sessions_root(), session_id.clone())
            .map_err(runtime_error)?;
        let replay = store.read_replay().map_err(runtime_error)?;
        let events = replay
            .events
            .into_iter()
            .filter_map(|event| transcript_event(event, include_content))
            .collect();
        Ok(DaemonEventKind::SessionTranscript(SessionTranscriptEvent {
            session_id,
            events,
            content_included: include_content,
        }))
    }

    fn submit_run(
        &self,
        request_id: RequestId,
        request: agl_protocol::RunSubmitRequest,
    ) -> Result<DaemonEventKind, ProtocolError> {
        if !request.content.has_attachments()
            && request
                .content
                .text_only()
                .is_some_and(|text| text.trim().is_empty())
        {
            return Err(invalid("run content cannot be blank"));
        }
        self.ensure_session_accepts_work(&request.session_id)
            .map_err(protocol_application_error)?;
        let session = self
            .sessions
            .get(&request.session_id)
            .ok_or_else(|| not_found(request.session_id.as_str()))?;
        if matches!(
            session.status,
            SessionStatus::Finished | SessionStatus::Failed
        ) {
            return Err(invalid("cannot submit a run to a terminal session"));
        }
        let concurrency_key =
            agl_store::RunConcurrencyKey::session(&request.session_id).map_err(runtime_error)?;
        let fingerprint = run_fingerprint(&request.session_id, &request.content);
        let store = AglStore::open_current_read_only_at(self.runtime.paths.store_root())
            .map_err(runtime_error)?;
        let replay = store
            .idempotency_record(
                RUN_SUBMIT_IDEMPOTENCY_NAMESPACE,
                &request.client_submission_id,
            )
            .map_err(runtime_error)?;
        if replay.is_none() {
            validate_root_activity_capacity_protocol(session.delegation_plan.as_ref())?;
            let queued = store
                .safe_runs_for_concurrency_key(&concurrency_key, false)
                .map_err(runtime_error)?
                .into_iter()
                .filter(|status| matches!(status.state, RunState::Queued | RunState::Waiting))
                .count();
            if queued >= MAX_QUEUED_PROMPTS_PER_SESSION {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::Busy,
                    "input_backpressure: session prompt queue is full",
                    true,
                ));
            }
        }
        let run_id = RunId::generate();
        let turn_id = TurnId::generate();
        let input = serde_json::to_value(ChatRunInput::Root {
            content: request.content.clone(),
            request_id: Some(request_id),
            options: session.options.clone(),
            delegation_plan: session.delegation_plan.clone(),
        })
        .map_err(runtime_error)?;
        let idempotency = Some(IdempotentRunSpec {
            namespace: RUN_SUBMIT_IDEMPOTENCY_NAMESPACE.to_string(),
            fingerprint,
            key: request.client_submission_id,
        });
        let accepted = self
            .supervisor_handle
            .submit(RunSpec {
                run: agl_store::DurableRunDraft {
                    run_id,
                    session_id: Some(request.session_id.clone()),
                    turn_id: Some(turn_id),
                    kind: agl_store::RunKind::Turn,
                    priority: 0,
                    concurrency_key: Some(concurrency_key),
                    input,
                    checkpoint: None,
                    effective_policy_hash: None,
                    execution_context: session.execution_context.clone(),
                    budget: RunBudget {
                        wall_time_ms: request.budget.wall_time_ms,
                        model_input_tokens: request.budget.model_input_tokens,
                        model_output_tokens: request.budget.model_output_tokens,
                        model_attempts: request.budget.model_attempts,
                        tool_calls: request.budget.tool_calls,
                    },
                    not_before_ms: None,
                },
                idempotency,
            })
            .map_err(supervisor_error)?;
        Ok(DaemonEventKind::RunAccepted(RunAcceptedEvent {
            session_id: accepted
                .status
                .session_id
                .expect("turn admission has session"),
            run_id: accepted.status.run_id,
            turn_id: accepted.status.turn_id.expect("turn admission has turn"),
            state: protocol_run_state(accepted.status.state),
            replayed: accepted.replayed,
        }))
    }

    fn run_status(&self, run_id: RunId) -> Result<DaemonEventKind, ProtocolError> {
        let outcome = self
            .supervisor_handle
            .outcome(run_id.clone())
            .map_err(supervisor_error)?
            .ok_or_else(|| not_found(run_id.as_str()))?;
        Ok(DaemonEventKind::RunStatus(Box::new(run_status_event(
            outcome,
        ))))
    }

    fn cancel_run(&self, run_id: RunId) -> Result<DaemonEventKind, ProtocolError> {
        self.supervisor_handle
            .cancel(run_id.clone())
            .map_err(supervisor_error)?;
        self.run_status(run_id)
    }

    fn run_tree(&self, run_id: RunId) -> Result<DaemonEventKind, ProtocolError> {
        let runs = self
            .supervisor_handle
            .tree(run_id.clone())
            .map_err(supervisor_error)?
            .into_iter()
            .map(run_tree_node)
            .collect();
        Ok(DaemonEventKind::RunTree(RunTreeEvent {
            requested_run_id: run_id,
            runs,
        }))
    }

    fn run_events(
        &self,
        run_id: RunId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<DaemonEventKind, ProtocolError> {
        if limit == 0 || limit > 10_000 {
            return Err(invalid("run event limit must be between 1 and 10000"));
        }
        let events = self
            .supervisor_handle
            .events_after(run_id.clone(), after_sequence, limit)
            .map_err(supervisor_error)?;
        Ok(DaemonEventKind::RunEvents(RunEventsEvent {
            run_id,
            after_sequence,
            events,
        }))
    }

    pub(crate) fn application_open_session(
        &mut self,
        request: SessionOpen,
    ) -> Result<SessionOpened, ApplicationError> {
        let launch = request.launch;
        let selected_model_id = launch.model_id.clone();
        let request = agl_protocol::SessionOpenRequest {
            session_id: None,
            new_session: true,
            workspace_root: launch.workspace_root,
            function_ref: launch.function_ref,
            skills: launch.skill_ids,
            tool_mode: launch
                .operation_mode
                .map(protocol_tool_mode_from_app)
                .unwrap_or(ProtocolToolMode::ReadOnly),
        };
        let opened = match self
            .open_session(request)
            .map_err(application_protocol_error)?
        {
            DaemonEventKind::SessionOpened(opened) => opened,
            _ => unreachable!("session open has one response family"),
        };
        if let Some(model_id) = selected_model_id {
            self.select_session_model(&opened.session_id, &model_id)?;
        }
        let snapshot = self.application_snapshot(&opened.session_id)?;
        Ok(SessionOpened {
            session_id: opened.session_id,
            resumed: opened.resumed,
            snapshot,
        })
    }

    pub(crate) fn application_ensure_human_terminal(
        &mut self,
        request: HumanTerminalEnsure,
    ) -> Result<TerminalEnsured, ApplicationError> {
        request.validate()?;
        self.ensure_session_accepts_work(&request.session_id)?;
        if request.profile == ExecutionProfile::Host {
            return Err(ApplicationError::new(
                ApplicationErrorCode::AuthorizationRequired,
                "Human Host terminal creation requires explicit local-operator authority",
            ));
        }
        self.ensure_human_terminal(request, HumanTerminalAuthority::Workspace)
    }

    pub(crate) fn operator_ensure_human_host_terminal(
        &mut self,
        request: HumanTerminalEnsure,
        operator_uid: u32,
        confirm_host_authority: bool,
    ) -> Result<TerminalEnsured, ApplicationError> {
        request.validate()?;
        self.ensure_session_accepts_work(&request.session_id)?;
        if operator_uid != unsafe { libc::geteuid() } {
            return Err(ApplicationError::new(
                ApplicationErrorCode::NotAuthorized,
                "local operator UID does not match the daemon owner",
            ));
        }
        if request.profile != ExecutionProfile::Host {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "local-operator Host admission requires a Host terminal profile",
            ));
        }
        if !confirm_host_authority {
            return Err(ApplicationError::new(
                ApplicationErrorCode::ConfirmationRequired,
                "creating a Human Host terminal requires explicit confirmation",
            ));
        }
        self.ensure_human_terminal(
            request,
            HumanTerminalAuthority::LocalOperatorHost { operator_uid },
        )
    }

    fn ensure_human_terminal(
        &mut self,
        request: HumanTerminalEnsure,
        authority: HumanTerminalAuthority,
    ) -> Result<TerminalEnsured, ApplicationError> {
        let session = self
            .sessions
            .get(&request.session_id)
            .cloned()
            .ok_or_else(|| {
                ApplicationError::new(ApplicationErrorCode::NotFound, "session not found")
            })?;
        match authority {
            HumanTerminalAuthority::Workspace if request.profile != ExecutionProfile::Workspace => {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::AuthorizationRequired,
                    "workspace terminal admission cannot upgrade to Host authority",
                ));
            }
            HumanTerminalAuthority::LocalOperatorHost { .. }
                if request.profile != ExecutionProfile::Host =>
            {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "local-operator authority is scoped only to a Human Host terminal",
                ));
            }
            HumanTerminalAuthority::Workspace
            | HumanTerminalAuthority::LocalOperatorHost { .. } => {}
        }

        let submission_fingerprint = human_terminal_fingerprint(&request)?;
        let submission_key = (
            request.session_id.clone(),
            request.client_submission_id.clone(),
        );
        if let Some(previous) = self
            .human_terminal_submissions
            .get(&submission_key)
            .cloned()
        {
            if previous.fingerprint != submission_fingerprint {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "terminal submission ID was already used with different admission metadata",
                ));
            }
            let record = self
                .terminal_bridge
                .record(
                    terminal_topology_id(&request.session_id),
                    &previous.terminal_id,
                )
                .map_err(application_runtime_error)?;
            return Ok(TerminalEnsured {
                terminal: self.terminal_view(&record)?,
                disposition: TerminalEnsureDisposition::Reused,
            });
        }

        if matches!(
            session.status,
            SessionStatus::Finished | SessionStatus::Failed
        ) {
            return Err(ApplicationError::new(
                ApplicationErrorCode::SessionBusy,
                "cannot create a terminal for a finished session",
            ));
        }
        if request.execution_context_revision != session.execution_context.revision {
            return Err(ApplicationError::new(
                ApplicationErrorCode::StaleContextRevision,
                "terminal request does not match the current execution-context revision",
            ));
        }

        let admission_fingerprint = human_terminal_admission_fingerprint(&request)?;
        for record in self
            .terminal_bridge
            .list_topology(terminal_topology_id(&request.session_id))
            .map_err(application_runtime_error)?
        {
            if record.profile != request.profile || !record.owner.is_human() {
                continue;
            }
            let refreshed = record;
            if refreshed.state == TerminalState::OutcomeUnknown {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::OutcomeUnknown,
                    "cannot replace a Human terminal whose outcome is unknown",
                ));
            }
            if refreshed.state.is_live() {
                let matches_admission = self
                    .terminal_presentations
                    .get(&refreshed.terminal_id)
                    .is_some_and(|metadata| {
                        metadata.admission_fingerprint == admission_fingerprint
                    });
                if !matches_admission {
                    return Err(ApplicationError::new(
                        ApplicationErrorCode::InvalidArguments,
                        "the live Human terminal has different immutable admission metadata",
                    ));
                }
                self.human_terminal_submissions.insert(
                    submission_key,
                    HumanTerminalSubmission {
                        fingerprint: submission_fingerprint,
                        terminal_id: refreshed.terminal_id.clone(),
                    },
                );
                return Ok(TerminalEnsured {
                    terminal: self.terminal_view(&refreshed)?,
                    disposition: TerminalEnsureDisposition::Reused,
                });
            }
            self.terminal_bridge
                .retire(refreshed.terminal_id)
                .map_err(application_runtime_error)?;
        }
        let shell = admitted_terminal_shell(&session.execution_context, &request.shell_profile_id)?;
        let (environment, environment_names) =
            self.terminal_environment(&session.execution_context, &request.agl_env)?;
        let history_seed = Vec::new();
        let (host_startup, authorization, grant_lease) = match authority {
            HumanTerminalAuthority::Workspace => (
                ProcessHostStartupPolicy::ManagedOnly,
                ExecutionAuthorization {
                    workspace_write: true,
                    ..ExecutionAuthorization::default()
                },
                None,
            ),
            HumanTerminalAuthority::LocalOperatorHost { operator_uid } => {
                let host_startup =
                    resolve_host_startup(request.host_startup, shell.kind, operator_uid)?;
                let sources_user_rc =
                    matches!(host_startup, ProcessHostStartupPolicy::SourceUserRc { .. });
                (
                    host_startup,
                    ExecutionAuthorization {
                        host_process_execution: true,
                        shell_login_startup: sources_user_rc,
                        workspace_write: true,
                    },
                    Some(ExecutionGrantLease {
                        origin: ExecutionLeaseOrigin::LocalOperatorTerminal,
                        grant_id: format!("local-operator-terminal:{}", RequestId::generate()),
                        duration: LOCAL_OPERATOR_TERMINAL_LEASE_DURATION.to_owned(),
                        scope_digest: admission_fingerprint.clone(),
                    }),
                )
            }
        };
        let registered = self
            .terminal_bridge
            .list_topology(terminal_topology_id(&request.session_id))
            .map_err(application_runtime_error)?;
        if registered.len() >= agl_app::MAX_TERMINALS_PER_SESSION
            && !registered.iter().any(|record| {
                record.profile == request.profile
                    && record.state.is_live()
                    && record.owner.is_human()
            })
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InputBackpressure,
                "session reached its bounded retained-terminal limit",
            ));
        }
        let existing = registered
            .into_iter()
            .map(|record| record.terminal_id)
            .collect::<BTreeSet<_>>();
        let root_run_id = RunId::generate();
        let record = self
            .terminal_bridge
            .ensure(TerminalAdmission {
                topology_id: terminal_topology_id(&request.session_id),
                owner: TerminalOwner::new(agent_caller_owner(
                    request.session_id.as_str(),
                    CallerOwnerKind::Persistent,
                    CallerRole::Human,
                )),
                authority_scope: opaque_agent_id(root_run_id.as_str()),
                correlation: agl_exec::ExecutionCorrelation::new(
                    agent_caller_namespace(),
                    opaque_agent_id(root_run_id.as_str()),
                    opaque_agent_id(StepId::generate().as_str()),
                ),
                context: session.execution_context,
                profile: request.profile,
                shell,
                environment,
                runtime_read_only_roots: self.runtime.execution.runtime_read_only_roots.clone(),
                host_startup,
                authorization,
                grant_lease,
                terminal_size: request.terminal_size,
                limits: ExecutionLimits {
                    timeout_ms: None,
                    max_input_bytes: self.runtime.execution.max_input_bytes as u64,
                    max_output_bytes: self.runtime.execution.max_spool_bytes,
                },
                history_seed,
                authority_fingerprint: self.terminal_bridge.authority(),
                request_fingerprint: String::new(),
                operations: BTreeSet::new(),
            })
            .map_err(application_runtime_error)?;
        let disposition = if existing.contains(&record.terminal_id) {
            TerminalEnsureDisposition::Reused
        } else {
            TerminalEnsureDisposition::Created
        };
        self.terminal_presentations.insert(
            record.terminal_id.clone(),
            TerminalPresentationMetadata {
                environment_names,
                admission_fingerprint,
            },
        );
        self.human_terminal_submissions.insert(
            submission_key,
            HumanTerminalSubmission {
                fingerprint: submission_fingerprint,
                terminal_id: record.terminal_id.clone(),
            },
        );
        Ok(TerminalEnsured {
            terminal: self.terminal_view(&record)?,
            disposition,
        })
    }

    fn terminal_environment(
        &self,
        context: &agl_exec::ExecutionContextSnapshot,
        overlay: &agl_app::StructuredEnvironmentOverlay,
    ) -> Result<(TerminalEnvironmentRequest, Vec<String>), ApplicationError> {
        let admitted_path_roots =
            terminal_admitted_path_roots(&self.runtime.execution.runtime_read_only_roots)?;
        let admitted = self
            .runtime
            .execution
            .admitted_environment()
            .map_err(application_runtime_error)?;
        let mut admitted_base = admitted.values;
        let inherited_path = admitted_base.get("PATH").ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::CommandUnavailable,
                "persistent terminal requires an admitted PATH",
            )
        })?;
        let terminal_path =
            build_terminal_path(inherited_path, &context.shell.program, &admitted_path_roots)?;
        admitted_base.insert("PATH".to_owned(), terminal_path);

        let mut selected_parent = BTreeMap::new();
        for name in &overlay.inherited_names {
            let value = admitted_base.get(name).ok_or_else(|| {
                ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    format!("parent environment name `{name}` is not admitted or is unset"),
                )
            })?;
            selected_parent.insert(name.clone(), value.clone());
        }
        let mut agl_env = overlay
            .values
            .iter()
            .map(|(name, value)| (name.clone(), TerminalEnvironmentValue::Plain(value.clone())))
            .collect::<BTreeMap<_, _>>();
        for secret in &overlay.secret_refs {
            let reference = TerminalSecretReference::new(secret.reference_id.clone())
                .map_err(terminal_application_error)?;
            agl_env.insert(
                secret.name.clone(),
                TerminalEnvironmentValue::Secret(reference),
            );
        }
        let environment_names = admitted_base
            .keys()
            .chain(selected_parent.keys())
            .chain(agl_env.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Ok((
            TerminalEnvironmentRequest {
                admitted_base,
                selected_parent,
                agl_env,
                admitted_path_roots,
            },
            environment_names,
        ))
    }

    fn application_terminal_records(
        &self,
        session_id: &SessionId,
        include_finished: bool,
    ) -> Result<Vec<TerminalRecord>, ApplicationError> {
        let records = self
            .terminal_bridge
            .list_topology(terminal_topology_id(session_id))
            .map_err(application_runtime_error)?;
        if records.len() > agl_app::MAX_TERMINALS_PER_SESSION {
            return Err(ApplicationError::new(
                ApplicationErrorCode::Internal,
                "terminal registry exceeded the bounded session projection",
            ));
        }
        records
            .into_iter()
            .filter(|record| include_finished || record.state.is_live())
            .map(Ok)
            .collect()
    }

    fn terminal_view(
        &self,
        record: &TerminalRecord,
    ) -> Result<TerminalSessionView, ApplicationError> {
        let status = self
            .terminal_bridge
            .execution_status(record.execution_id.clone())
            .map_err(application_runtime_error)?;
        let session_id = session_id_from_topology(&record.topology_id)?;
        let promoted = record.owner.previous_owner().is_some();
        let owner = if let Some(previous) = record.owner.previous_owner() {
            TerminalOwnerView::SessionPromoted {
                session_id: session_id.clone(),
                previous_owner_run_id: RunId::parse(previous.owner_id().as_str()).map_err(
                    |_| {
                        ApplicationError::new(
                            ApplicationErrorCode::Internal,
                            "invalid promoted terminal owner mapping",
                        )
                    },
                )?,
            }
        } else if record.owner.is_human() {
            TerminalOwnerView::Human {
                session_id: session_id.clone(),
            }
        } else if record.owner.is_persistent() {
            TerminalOwnerView::MainAgent {
                session_id: session_id.clone(),
            }
        } else {
            TerminalOwnerView::Subagent {
                root_run_id: RunId::parse(record.authority_scope.as_str()).map_err(|_| {
                    ApplicationError::new(
                        ApplicationErrorCode::Internal,
                        "invalid terminal authority mapping",
                    )
                })?,
                owner_run_id: RunId::parse(record.owner.caller().owner_id().as_str()).map_err(
                    |_| {
                        ApplicationError::new(
                            ApplicationErrorCode::Internal,
                            "invalid ephemeral terminal owner mapping",
                        )
                    },
                )?,
            }
        };
        let view = TerminalSessionView {
            terminal_id: record.terminal_id.clone(),
            execution_id: record.execution_id.clone(),
            owner,
            profile: record.profile,
            shell: ShellProfileView {
                profile_id: terminal_shell_profile_id(record.shell_profile.kind).to_owned(),
                program: SanitizedDisplayPath::from_path(&record.shell_profile.snapshot.program),
                executable_digest: record.shell_profile.snapshot.executable_digest.clone(),
                config_digest: record.shell_profile.snapshot.config_digest.clone(),
            },
            workspace_root: SanitizedDisplayPath::from_path(&record.workspace_root),
            cwd: SanitizedDisplayPath::from_path(&record.cwd),
            initial_environment_digest: record.environment_digest.as_str().to_owned(),
            environment_names: self
                .terminal_presentations
                .get(&record.terminal_id)
                .map(|metadata| metadata.environment_names.clone())
                .unwrap_or_default(),
            command_sequence: record.command_sequence,
            prompt_generation: match &record.prompt_state {
                ProcessTerminalPromptState::Ready { sequence, .. } => Some(*sequence),
                _ => None,
            },
            prompt_state: application_terminal_prompt_state(&record.prompt_state),
            process_state: status.state,
            exit: status.exit,
            writer: if promoted {
                TerminalWriterView::HumanTakeover
            } else {
                TerminalWriterView::Owner
            },
            promoted,
        };
        view.validate_for_session(&session_id)?;
        Ok(view)
    }

    fn session_work_counts(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionTerminationCounts, ApplicationError> {
        let terminals = self.application_terminal_records(session_id, true)?;
        let terminal_execution_ids = terminals
            .iter()
            .map(|record| record.execution_id.clone())
            .collect::<BTreeSet<_>>();
        let terminal_count = terminals
            .iter()
            .filter(|record| {
                record.state.is_live() || record.state == TerminalState::OutcomeUnknown
            })
            .count();
        let execution_count = self
            .session_execution_bridge(session_id)?
            .execution_list(agent_execution_filter(Some(session_id), None, false))
            .map_err(application_runtime_error)?
            .into_iter()
            .filter(|status| !terminal_execution_ids.contains(&status.execution_id))
            .count();
        Ok(SessionTerminationCounts {
            terminals: bounded_count(terminal_count),
            executions: bounded_count(execution_count),
        })
    }

    fn terminate_session_work(
        &self,
        session_id: &SessionId,
        context: &ApplicationCallContext,
    ) -> Result<SessionTerminationCounts, ApplicationError> {
        let plan = self.begin_terminate_session_work(session_id, context)?;
        plan.wait(context)?;
        Ok(plan.counts)
    }

    fn begin_terminate_session_work(
        &self,
        session_id: &SessionId,
        context: &ApplicationCallContext,
    ) -> Result<SessionExecutionTermination, ApplicationError> {
        ensure_application_call_live(context)?;
        let terminals = self.application_terminal_records(session_id, true)?;
        if terminals
            .iter()
            .any(|record| record.state == TerminalState::OutcomeUnknown)
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::OutcomeUnknown,
                "cannot confirm termination of an outcome_unknown terminal",
            ));
        }
        let terminal_execution_ids = terminals
            .iter()
            .map(|record| record.execution_id.clone())
            .collect::<BTreeSet<_>>();
        let live_terminals = terminals
            .iter()
            .filter(|record| record.state.is_live())
            .collect::<Vec<_>>();
        let execution_bridge = self.session_execution_bridge(session_id)?;
        let other_executions = execution_bridge
            .execution_list(agent_execution_filter(Some(session_id), None, false))
            .map_err(application_runtime_error)?
            .into_iter()
            .filter(|status| !terminal_execution_ids.contains(&status.execution_id))
            .collect::<Vec<_>>();

        for record in &live_terminals {
            ensure_application_call_live(context)?;
            self.terminal_bridge
                .terminate(record.terminal_id.clone())
                .map_err(application_runtime_error)?;
        }
        for status in &other_executions {
            ensure_application_call_live(context)?;
            execution_bridge
                .terminate_execution(status.execution_id.clone(), KillMode::Graceful)
                .map_err(application_runtime_error)?;
        }
        Ok(SessionExecutionTermination {
            live_terminals: live_terminals
                .iter()
                .map(|record| (record.topology_id.clone(), record.terminal_id.clone()))
                .collect(),
            execution_ids: other_executions
                .iter()
                .map(|status| status.execution_id.clone())
                .collect(),
            counts: SessionTerminationCounts {
                terminals: bounded_count(live_terminals.len()),
                executions: bounded_count(other_executions.len()),
            },
            human_terminals: self.terminal_bridge.clone(),
            agent_executions: execution_bridge,
            timeout: Duration::from_millis(
                self.runtime
                    .execution
                    .termination_grace_ms
                    .saturating_add(self.runtime.execution.setup_timeout_ms),
            ),
            poll_interval: Duration::from_millis(
                self.runtime.execution.poll_interval_ms.clamp(1, 100),
            ),
        })
    }

    fn retire_workspace_terminal_slots(
        &self,
        session_id: &SessionId,
        workspace_root: &Path,
    ) -> Result<(), ApplicationError> {
        for record in self.application_terminal_records(session_id, true)? {
            if record.workspace_root != workspace_root {
                continue;
            }
            self.terminal_bridge
                .retire(record.terminal_id)
                .map_err(application_runtime_error)?;
        }
        Ok(())
    }

    fn active_session_root_runs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SafeRunStatus>, ApplicationError> {
        let key =
            agl_store::RunConcurrencyKey::session(session_id).map_err(application_runtime_error)?;
        let store = AglStore::open_current_read_only_at(self.runtime.paths.store_root())
            .map_err(application_runtime_error)?;
        store
            .safe_runs_for_concurrency_key(&key, false)
            .map_err(application_runtime_error)
            .map(|runs| {
                runs.into_iter()
                    .filter(|run| {
                        run.session_id.as_ref() == Some(session_id)
                            && run.kind == RunKind::Turn
                            && run.parent_run_id.is_none()
                            && run.root_run_id == run.run_id
                            && matches!(
                                run.state,
                                RunState::Queued | RunState::Waiting | RunState::Running
                            )
                    })
                    .collect()
            })
    }

    fn cancel_session_root_runs(
        &self,
        runs: Vec<SafeRunStatus>,
        context: &ApplicationCallContext,
    ) -> Result<u32, ApplicationError> {
        let plan = self.begin_cancel_session_root_runs(runs, context)?;
        plan.wait(context)
    }

    fn begin_cancel_session_root_runs(
        &self,
        mut runs: Vec<SafeRunStatus>,
        context: &ApplicationCallContext,
    ) -> Result<SessionRunCancellation, ApplicationError> {
        ensure_application_call_live(context)?;
        let run_ids = runs
            .iter()
            .map(|run| run.run_id.clone())
            .collect::<Vec<_>>();
        // Cancel the queued tail before the head and the active run last. This
        // prevents a newly freed session concurrency slot from activating a
        // prompt that was already part of the confirmed exit snapshot.
        runs.sort_by(|left, right| {
            exit_cancellation_rank(left.state)
                .cmp(&exit_cancellation_rank(right.state))
                .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
                .then_with(|| right.run_id.cmp(&left.run_id))
        });
        for run in runs {
            ensure_application_call_live(context)?;
            self.supervisor_handle
                .cancel(run.run_id)
                .map_err(supervisor_error)
                .map_err(application_protocol_error)?;
        }
        Ok(SessionRunCancellation {
            run_ids,
            supervisor: self.supervisor_handle.clone(),
        })
    }

    fn finish_session_with_counts(
        &mut self,
        session_id: SessionId,
        reason: agl_protocol::SessionFinishReason,
        context: &ApplicationCallContext,
    ) -> Result<(DaemonEventKind, SessionTerminationCounts), ApplicationError> {
        ensure_application_call_live(context)?;
        let already_finished = self
            .sessions
            .get(&session_id)
            .map(|session| session.status == SessionStatus::Finished)
            .ok_or_else(|| {
                ApplicationError::new(ApplicationErrorCode::NotFound, "session not found")
            })?;
        let counts = self.terminate_session_work(&session_id, context)?;
        ensure_application_call_live(context)?;
        if !already_finished {
            self.chat_factory
                .with_session(&session_id, |service| service.request_exit())
                .map_err(application_busy_error)?;
            self.sessions
                .get_mut(&session_id)
                .expect("session was checked above")
                .status = SessionStatus::Finished;
        }
        Ok((
            DaemonEventKind::SessionFinished(SessionFinishedEvent { session_id, reason }),
            counts,
        ))
    }

    fn cancel_active_session_work(
        &mut self,
        session_id: SessionId,
        context: &ApplicationCallContext,
    ) -> Result<DaemonEventKind, ApplicationError> {
        let plan = self.begin_active_work_cancel(session_id, context)?;
        let outcome = plan.wait(context)?;
        self.complete_active_work_cancel(plan, outcome, context)
    }

    fn begin_active_work_cancel(
        &mut self,
        session_id: SessionId,
        context: &ApplicationCallContext,
    ) -> Result<SessionExitPlan, ApplicationError> {
        ensure_application_call_live(context)?;
        self.sessions.get(&session_id).ok_or_else(|| {
            ApplicationError::new(ApplicationErrorCode::NotFound, "session not found")
        })?;
        self.exiting_sessions.insert(session_id.clone());
        let active_runs = self.active_session_root_runs(&session_id)?;
        let runs = self.begin_cancel_session_root_runs(active_runs, context)?;
        let executions = self.begin_terminate_session_work(&session_id, context)?;
        Ok(SessionExitPlan {
            session_id,
            reason: agl_protocol::SessionFinishReason::ExitCommand,
            runs,
            executions,
        })
    }

    fn complete_active_work_cancel(
        &mut self,
        plan: SessionExitPlan,
        outcome: SessionExitOutcome,
        context: &ApplicationCallContext,
    ) -> Result<DaemonEventKind, ApplicationError> {
        ensure_application_call_live(context)?;
        self.exiting_sessions.remove(&plan.session_id);
        Ok(DaemonEventKind::SessionActiveWorkCancelled(
            SessionActiveWorkCancelledEvent {
                session_id: plan.session_id,
                cancelled_runs: outcome.cancelled_runs,
                terminated_terminals: outcome.terminated.terminals,
                terminated_executions: outcome.terminated.executions,
            },
        ))
    }

    fn begin_session_exit(
        &mut self,
        session_id: SessionId,
        reason: agl_protocol::SessionFinishReason,
        confirm_active: bool,
        context: &ApplicationCallContext,
    ) -> Result<SessionExitPlan, ApplicationError> {
        ensure_application_call_live(context)?;
        self.sessions.get(&session_id).ok_or_else(|| {
            ApplicationError::new(ApplicationErrorCode::NotFound, "session not found")
        })?;
        let active_runs = self.active_session_root_runs(&session_id)?;
        let active = self.session_work_counts(&session_id)?;
        if (!active_runs.is_empty() || active.terminals != 0 || active.executions != 0)
            && !confirm_active
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::SessionBusy,
                format!(
                    "session has {} active or queued root run(s), {} terminal(s), and {} other execution(s); confirmation is required",
                    active_runs.len(),
                    active.terminals,
                    active.executions
                ),
            ));
        }
        self.exiting_sessions.insert(session_id.clone());
        let runs = self.begin_cancel_session_root_runs(active_runs, context)?;
        let executions = self.begin_terminate_session_work(&session_id, context)?;
        Ok(SessionExitPlan {
            session_id,
            reason,
            runs,
            executions,
        })
    }

    fn complete_session_exit(
        &mut self,
        plan: SessionExitPlan,
        context: &ApplicationCallContext,
    ) -> Result<DaemonEventKind, ApplicationError> {
        ensure_application_call_live(context)?;
        let session = self.sessions.get(&plan.session_id).ok_or_else(|| {
            ApplicationError::new(ApplicationErrorCode::NotFound, "session not found")
        })?;
        if session.status != SessionStatus::Finished {
            self.chat_factory
                .with_session(&plan.session_id, |service| service.request_exit())
                .map_err(application_busy_error)?;
            self.sessions
                .get_mut(&plan.session_id)
                .expect("session was checked above")
                .status = SessionStatus::Finished;
        }
        self.exiting_sessions.remove(&plan.session_id);
        Ok(DaemonEventKind::SessionFinished(SessionFinishedEvent {
            session_id: plan.session_id,
            reason: plan.reason,
        }))
    }

    pub(crate) fn ensure_session_accepts_work(
        &self,
        session_id: &SessionId,
    ) -> Result<(), ApplicationError> {
        if self.exiting_sessions.contains(session_id) {
            return Err(ApplicationError::new(
                ApplicationErrorCode::SessionBusy,
                "session exit is waiting for typed owner outcomes",
            ));
        }
        let session = self.sessions.get(session_id).ok_or_else(|| {
            ApplicationError::new(ApplicationErrorCode::NotFound, "session not found")
        })?;
        if matches!(
            session.status,
            SessionStatus::Finished | SessionStatus::Failed
        ) {
            return Err(ApplicationError::new(
                ApplicationErrorCode::SessionBusy,
                "session no longer accepts new work",
            ));
        }
        self.reconcile_incomplete_continuations_for_session(session_id)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn set_execution_context_revision_for_test(
        &mut self,
        session_id: &SessionId,
        revision: u64,
    ) {
        self.sessions
            .get_mut(session_id)
            .expect("test session must exist")
            .execution_context
            .revision = revision;
    }

    #[cfg(test)]
    pub(crate) fn select_chat_service_mode_for_test(
        &self,
        session_id: &SessionId,
        mode: ChatToolMode,
    ) -> anyhow::Result<()> {
        self.chat_factory
            .with_session(session_id, |service| service.select_operation_mode(mode))
    }

    pub(crate) fn application_snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionPresentationSnapshot, ApplicationError> {
        self.application_snapshot_page(session_id, None)
            .map(|page| page.snapshot)
    }

    pub(crate) fn application_snapshot_page(
        &self,
        session_id: &SessionId,
        page_cursor: Option<&str>,
    ) -> Result<PresentationSnapshotPage, ApplicationError> {
        let session = self.sessions.get(session_id).cloned().ok_or_else(|| {
            ApplicationError::new(ApplicationErrorCode::NotFound, "session not found")
        })?;
        let durable = !session.options.no_history;
        let (incomplete_replay, current_policy_hash, mut replay) = if durable {
            let incomplete_replay =
                ChatSessionStore::open(self.runtime.paths.sessions_root(), session_id.clone())
                    .and_then(|store| store.read_replay())
                    .map(|replay| incomplete_replay_index(&replay.events))
                    .map_err(application_runtime_error)?;
            let current_policy_hash = self
                .chat_factory
                .current_policy_hash(session_id)
                .map_err(application_runtime_error)?
                .ok_or_else(|| {
                    ApplicationError::new(
                        ApplicationErrorCode::Internal,
                        "session effective tool policy is unavailable",
                    )
                })?;
            let replay = ChatSessionStore::open_reverse_replay(
                self.runtime.paths.sessions_root(),
                session_id.clone(),
                MAX_PRESENTATION_TRANSCRIPT_RECORD_BYTES,
            )
            .map_err(application_runtime_error)?;
            (
                Some(incomplete_replay),
                Some(current_policy_hash),
                Some(replay),
            )
        } else {
            if page_cursor.is_some() {
                return Err(invalid_presentation_page_cursor());
            }
            (None, None, None)
        };
        let cursor_scope = presentation_page_cursor_scope(session_id, &self.daemon_instance_id);
        if let (Some(cursor), Some(replay)) = (page_cursor, replay.as_mut()) {
            let end_offset =
                parse_presentation_page_cursor(cursor, &cursor_scope, replay.transcript_len())?;
            replay
                .set_end_offset(end_offset)
                .map_err(|_| invalid_presentation_page_cursor())?;
        }

        let terminal_records = self.application_terminal_records(session_id, true)?;
        let terminals = terminal_records
            .iter()
            .map(|record| self.terminal_view(record))
            .collect::<Result<Vec<_>, _>>()?;
        let statuses = self
            .terminal_bridge
            .execution_list(agent_execution_filter(Some(session_id), None, true))
            .map_err(application_runtime_error)?;
        let mut executions = Vec::new();
        for status in statuses {
            if status.owner.caller().owner_kind() != CallerOwnerKind::Persistent {
                continue;
            }
            executions.push(ExecutionView {
                execution_id: status.execution_id,
                state: status.state,
                profile: status.profile,
                cwd: SanitizedDisplayPath::from_path(&status.cwd),
                exit: status.exit,
                last_sequence: status.last_sequence,
                output_truncated: status.output_truncated || status.output_expired,
            });
        }
        let mut active_execution_ids = executions
            .iter()
            .filter(|execution| execution.state.is_live())
            .map(|execution| execution.execution_id.clone())
            .collect::<BTreeSet<_>>();
        active_execution_ids.extend(
            terminal_records
                .iter()
                .filter(|record| record.state.is_live())
                .map(|record| record.execution_id.clone()),
        );
        let active_execution_count = bounded_count(active_execution_ids.len());
        let concurrency_key =
            agl_store::RunConcurrencyKey::session(session_id).map_err(application_runtime_error)?;
        let store = AglStore::open_current_read_only_at(self.runtime.paths.store_root())
            .map_err(application_runtime_error)?;
        let turn_runs = store
            .safe_runs_for_concurrency_key(&concurrency_key, false)
            .map_err(application_runtime_error)?;
        let active_run = turn_runs
            .iter()
            .find(|run| run.state == RunState::Running)
            .map(|run| ActiveRunView {
                run_id: run.run_id.clone(),
                turn_id: run.turn_id.clone(),
                state: run.state.as_str().to_owned(),
            });
        let active_offset = usize::from(active_run.is_some());
        let queued_prompts = turn_runs
            .iter()
            .filter(|run| matches!(run.state, RunState::Queued | RunState::Waiting))
            .enumerate()
            .map(|(index, run)| QueuedPromptView {
                run_id: run.run_id.clone(),
                ordinal: u32::try_from(active_offset + index + 1).unwrap_or(u32::MAX),
            })
            .collect::<Vec<_>>();
        let active_run_count = u32::from(active_run.is_some());
        let queued_prompt_count = u32::try_from(queued_prompts.len()).unwrap_or(u32::MAX);
        let status = match session.status {
            SessionStatus::Open | SessionStatus::Busy => SessionPresentationStatus::Active,
            SessionStatus::Finished => SessionPresentationStatus::Finished,
            SessionStatus::Failed => SessionPresentationStatus::Failed,
        };
        let operation_mode = session.options.inference.tool_mode;
        let command_context = CommandContext {
            session_id: Some(session_id.clone()),
            session_active: status == SessionPresentationStatus::Active,
            active_or_queued_turns: active_run_count.saturating_add(queued_prompt_count),
            active_executions: active_execution_count,
            host_shell_available: false,
            operation_mode,
        };
        let snapshot = SessionPresentationSnapshot {
            session_id: session_id.clone(),
            cursor: PresentationCursor {
                daemon_instance_id: self.daemon_instance_id.clone(),
                revision: 0,
            },
            header: SessionHeader {
                session_id: session_id.clone(),
                status,
                durable,
                resumed: session.resumed,
                title: None,
                function_name: session
                    .options
                    .inference
                    .function_ref
                    .clone()
                    .unwrap_or_else(|| "agentLIBRE".to_owned()),
                model_id: session.selected_model_id.clone(),
                operation_mode,
                selected_skills: session.options.inference.skills.clone(),
                runtime_context_revision: session.runtime_context_revision,
                workspace_root: SanitizedDisplayPath::from_path(
                    &session.execution_context.workspace_root,
                ),
                workspace_history_scope: workspace_history_scope(
                    &session.execution_context.workspace_root,
                ),
                cwd: SanitizedDisplayPath::from_path(&session.execution_context.working_directory),
                execution_context_revision: session.execution_context.revision,
                context_used_tokens: None,
                context_limit_tokens: None,
                active_run_count,
                queued_prompt_count,
                active_execution_count,
            },
            items: Vec::new(),
            active_run,
            queued_prompts,
            terminals,
            executions,
            activity: None,
            command_context,
        };
        let Some(replay) = replay else {
            return PresentationSnapshotPage {
                snapshot,
                older_page_cursor: None,
            }
            .validate();
        };
        let incomplete_replay = incomplete_replay
            .expect("durable presentation replay must include incomplete projection state");
        let incomplete_context = IncompleteProjectionContext {
            status,
            execution_context_revision: session.execution_context.revision,
            runtime_context_revision: session.runtime_context_revision,
            current_policy_hash: current_policy_hash
                .expect("durable presentation replay must include current policy hash"),
            claims: incomplete_replay.claims,
            current_context_messages: incomplete_replay.current_context_messages,
        };
        paginate_presentation_snapshot(snapshot, replay, &cursor_scope, &incomplete_context)
    }

    pub(crate) fn application_submit_prompt(
        &self,
        request: PromptSubmission,
    ) -> Result<PromptAdmission, ApplicationError> {
        let budget = request.budget;
        let response = self
            .submit_run(
                RequestId::generate(),
                agl_protocol::RunSubmitRequest {
                    session_id: request.session_id.clone(),
                    content: request.content,
                    client_submission_id: request.client_submission_id,
                    budget: agl_protocol::RunBudgetRequest {
                        wall_time_ms: budget.wall_time_ms,
                        model_input_tokens: budget.model_input_tokens,
                        model_output_tokens: budget.model_output_tokens,
                        model_attempts: budget.model_attempts,
                        tool_calls: budget.tool_calls,
                    },
                },
            )
            .map_err(application_protocol_error)?;
        let DaemonEventKind::RunAccepted(accepted) = response else {
            unreachable!("run submit has one response family")
        };
        let session_id = accepted.session_id;
        let run_id = accepted.run_id;
        let turn_id = accepted.turn_id;
        let state = match accepted.state {
            ProtocolRunState::Queued => PromptAdmissionState::Queued,
            ProtocolRunState::Running => PromptAdmissionState::Running,
            ProtocolRunState::Waiting => PromptAdmissionState::Waiting,
            ProtocolRunState::Succeeded => PromptAdmissionState::Succeeded,
            ProtocolRunState::Incomplete => PromptAdmissionState::Incomplete,
            ProtocolRunState::Failed => PromptAdmissionState::Failed,
            ProtocolRunState::Cancelled => PromptAdmissionState::Cancelled,
        };
        let ordinal = self.prompt_ordinal(&session_id, &run_id)?;
        let queued = state.is_queued();
        Ok(PromptAdmission {
            session_id,
            ordinal,
            queued,
            run_id,
            turn_id,
            state,
            replayed: accepted.replayed,
        })
    }

    fn application_continue_incomplete(
        &self,
        session_id: &SessionId,
        message_id: MessageId,
        client_submission_id: &str,
        expected_execution_context_revision: u64,
    ) -> Result<PromptAdmission, ApplicationError> {
        self.ensure_session_accepts_work(session_id)?;
        let session = self.sessions.get(session_id).cloned().ok_or_else(|| {
            ApplicationError::new(ApplicationErrorCode::NotFound, "session not found")
        })?;
        let mut transcript =
            ChatSessionStore::open(self.runtime.paths.sessions_root(), session_id.clone())
                .map_err(application_runtime_error)?;
        let replay = transcript
            .read_replay()
            .map_err(application_runtime_error)?;
        let source =
            incomplete_continuation_source(&replay.events, &message_id).ok_or_else(|| {
                ApplicationError::new(
                    ApplicationErrorCode::IncompleteOutputNotFound,
                    "durable incomplete assistant output was not found",
                )
            })?;
        let replay_index = incomplete_replay_index(&replay.events);

        if let Some(claim) = replay_index.claims.get(&message_id) {
            if claim.client_submission_id != client_submission_id {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::ContinuationAlreadyClaimed,
                    "incomplete assistant output already has a continuation run",
                ));
            }
            return self.reconcile_incomplete_continuation_claim(
                session_id, &session, &source, claim, None, true,
            );
        }

        if !replay_index.current_context_messages.contains(&message_id)
            || expected_execution_context_revision != source.execution_context_revision
            || session.execution_context.revision != source.execution_context_revision
            || session.runtime_context_revision != source.runtime_context_revision
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::StaleContinuationContext,
                "incomplete output no longer matches the current execution and runtime context",
            ));
        }
        let current_policy_hash = self
            .chat_factory
            .current_policy_hash(session_id)
            .map_err(application_runtime_error)?
            .ok_or_else(|| {
                ApplicationError::new(
                    ApplicationErrorCode::Internal,
                    "session effective tool policy is unavailable",
                )
            })?;
        if current_policy_hash != source.policy_hash {
            return Err(ApplicationError::new(
                ApplicationErrorCode::NotAuthorized,
                "incomplete output tool policy is no longer admitted",
            ));
        }

        let concurrency_key =
            agl_store::RunConcurrencyKey::session(session_id).map_err(application_runtime_error)?;
        let fingerprint = incomplete_continuation_fingerprint(session_id, &source);
        let store = AglStore::open_current_read_only_at(self.runtime.paths.store_root())
            .map_err(application_runtime_error)?;
        let idempotency = store
            .idempotency_record(
                INCOMPLETE_CONTINUE_IDEMPOTENCY_NAMESPACE,
                client_submission_id,
            )
            .map_err(application_runtime_error)?;
        if idempotency
            .as_ref()
            .is_some_and(|record| record.fingerprint != fingerprint)
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "client submission ID was already used for a different continuation",
            ));
        }
        if idempotency.is_some() {
            return Err(ApplicationError::new(
                ApplicationErrorCode::Internal,
                "continuation idempotency record exists without its durable session claim",
            ));
        }
        let queued = store
            .safe_runs_for_concurrency_key(&concurrency_key, false)
            .map_err(application_runtime_error)?
            .into_iter()
            .filter(|status| matches!(status.state, RunState::Queued | RunState::Waiting))
            .count();
        if queued >= MAX_QUEUED_PROMPTS_PER_SESSION {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InputBackpressure,
                "session prompt queue is full",
            ));
        }

        let claim = IncompleteContinuationClaim {
            client_submission_id: client_submission_id.to_owned(),
            continuation_run_id: RunId::generate(),
            continuation_turn_id: TurnId::generate(),
            continuation_request_id: RequestId::generate(),
        };
        let prepared =
            self.incomplete_continuation_run_spec(session_id, &session, &source, &claim)?;
        transcript
            .append_incomplete_continuation_claim(
                message_id,
                claim.client_submission_id.clone(),
                claim.continuation_run_id.clone(),
                claim.continuation_turn_id.clone(),
                claim.continuation_request_id.clone(),
            )
            .map_err(application_runtime_error)?;
        self.reconcile_incomplete_continuation_claim(
            session_id,
            &session,
            &source,
            &claim,
            Some(prepared),
            false,
        )
    }

    fn reconcile_incomplete_continuations_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), ApplicationError> {
        let session = self.sessions.get(session_id).cloned().ok_or_else(|| {
            ApplicationError::new(ApplicationErrorCode::NotFound, "session not found")
        })?;
        if session.options.no_history {
            return Ok(());
        }
        let transcript =
            ChatSessionStore::open(self.runtime.paths.sessions_root(), session_id.clone())
                .map_err(application_runtime_error)?;
        let replay = transcript
            .read_replay()
            .map_err(application_runtime_error)?;
        let index = incomplete_replay_index(&replay.events);
        for message_id in index.claim_order {
            let claim = index.claims.get(&message_id).ok_or_else(|| {
                ApplicationError::new(
                    ApplicationErrorCode::Internal,
                    "durable continuation replay lost its ordered claim",
                )
            })?;
            let source =
                incomplete_continuation_source(&replay.events, &message_id).ok_or_else(|| {
                    ApplicationError::new(
                        ApplicationErrorCode::Internal,
                        "durable continuation claim lost its incomplete assistant source",
                    )
                })?;
            self.reconcile_incomplete_continuation_claim(
                session_id, &session, &source, claim, None, true,
            )?;
        }
        Ok(())
    }

    fn reconcile_incomplete_continuation_claim(
        &self,
        session_id: &SessionId,
        session: &SessionRuntime,
        source: &IncompleteContinuationSource,
        claim: &IncompleteContinuationClaim,
        prepared: Option<RunSpec>,
        replayed: bool,
    ) -> Result<PromptAdmission, ApplicationError> {
        let fingerprint = incomplete_continuation_fingerprint(session_id, source);
        let store = AglStore::open_current_read_only_at(self.runtime.paths.store_root())
            .map_err(application_runtime_error)?;
        let idempotency = store
            .idempotency_record(
                INCOMPLETE_CONTINUE_IDEMPOTENCY_NAMESPACE,
                &claim.client_submission_id,
            )
            .map_err(application_runtime_error)?;
        if let Some(record) = idempotency.as_ref()
            && (record.fingerprint != fingerprint
                || record.admitted_run_id.as_ref() != Some(&claim.continuation_run_id))
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::Internal,
                "durable continuation claim conflicts with its idempotency record",
            ));
        }
        if let Some(status) = store
            .safe_run_status(&claim.continuation_run_id)
            .map_err(application_runtime_error)?
        {
            if idempotency.is_none() {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::Internal,
                    "durable continuation run is missing its idempotency record",
                ));
            }
            return self.continuation_admission_from_status(session_id, claim, status, replayed);
        }
        if idempotency.is_some() {
            return Err(ApplicationError::new(
                ApplicationErrorCode::Internal,
                "continuation idempotency record references a missing durable run",
            ));
        }

        let spec = match prepared {
            Some(spec) => spec,
            None => self.incomplete_continuation_run_spec(session_id, session, source, claim)?,
        };
        let accepted = self
            .supervisor_handle
            .submit(spec)
            .map_err(supervisor_error)
            .map_err(application_protocol_error)?;
        self.continuation_admission_from_status(
            session_id,
            claim,
            accepted.status,
            replayed || accepted.replayed,
        )
    }

    fn incomplete_continuation_run_spec(
        &self,
        session_id: &SessionId,
        session: &SessionRuntime,
        source: &IncompleteContinuationSource,
        claim: &IncompleteContinuationClaim,
    ) -> Result<RunSpec, ApplicationError> {
        validate_root_activity_capacity_application(session.delegation_plan.as_ref())?;
        let continuation_index = source.continuation_index.checked_add(1).ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "incomplete output continuation index is exhausted",
            )
        })?;
        let concurrency_key =
            agl_store::RunConcurrencyKey::session(session_id).map_err(application_runtime_error)?;
        let input = serde_json::to_value(ChatRunInput::Continuation {
            source_message_id: source.message_id.clone(),
            continuation_index,
            request_id: Some(claim.continuation_request_id.clone()),
            options: session.options.clone(),
            delegation_plan: session.delegation_plan.clone(),
        })
        .map_err(application_runtime_error)?;
        Ok(RunSpec {
            run: agl_store::DurableRunDraft {
                run_id: claim.continuation_run_id.clone(),
                session_id: Some(session_id.clone()),
                turn_id: Some(claim.continuation_turn_id.clone()),
                kind: RunKind::Turn,
                priority: 0,
                concurrency_key: Some(concurrency_key),
                input,
                checkpoint: None,
                effective_policy_hash: Some(source.policy_hash.clone()),
                execution_context: session.execution_context.clone(),
                budget: RunBudget::default(),
                not_before_ms: None,
            },
            idempotency: Some(IdempotentRunSpec {
                namespace: INCOMPLETE_CONTINUE_IDEMPOTENCY_NAMESPACE.to_owned(),
                key: claim.client_submission_id.clone(),
                fingerprint: incomplete_continuation_fingerprint(session_id, source),
            }),
        })
    }

    fn continuation_admission_from_status(
        &self,
        expected_session_id: &SessionId,
        claim: &IncompleteContinuationClaim,
        status: SafeRunStatus,
        replayed: bool,
    ) -> Result<PromptAdmission, ApplicationError> {
        if status.run_id != claim.continuation_run_id
            || status.session_id.as_ref() != Some(expected_session_id)
            || status.turn_id.as_ref() != Some(&claim.continuation_turn_id)
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::Internal,
                "continuation run identity differs from its durable claim",
            ));
        }
        let session_id = status.session_id.ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::Internal,
                "continuation run lost its session identity",
            )
        })?;
        let turn_id = status.turn_id.ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::Internal,
                "continuation run lost its turn identity",
            )
        })?;
        let state = match status.state {
            RunState::Queued => PromptAdmissionState::Queued,
            RunState::Running => PromptAdmissionState::Running,
            RunState::Waiting => PromptAdmissionState::Waiting,
            RunState::Succeeded => PromptAdmissionState::Succeeded,
            RunState::Incomplete => PromptAdmissionState::Incomplete,
            RunState::Failed => PromptAdmissionState::Failed,
            RunState::Cancelled => PromptAdmissionState::Cancelled,
        };
        let ordinal = self.prompt_ordinal(&session_id, &status.run_id)?;
        Ok(PromptAdmission {
            session_id,
            run_id: status.run_id,
            turn_id,
            ordinal,
            queued: state.is_queued(),
            state,
            replayed,
        })
    }

    fn prompt_ordinal(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
    ) -> Result<u32, ApplicationError> {
        let key =
            agl_store::RunConcurrencyKey::session(session_id).map_err(application_runtime_error)?;
        let store = AglStore::open_current_read_only_at(self.runtime.paths.store_root())
            .map_err(application_runtime_error)?;
        let runs = store
            .safe_runs_for_concurrency_key(&key, false)
            .map_err(application_runtime_error)?;
        if runs
            .iter()
            .any(|run| &run.run_id == run_id && run.state == RunState::Running)
        {
            return Ok(1);
        }
        let active_offset = usize::from(runs.iter().any(|run| run.state == RunState::Running));
        if let Some(index) = runs
            .iter()
            .filter(|run| matches!(run.state, RunState::Queued | RunState::Waiting))
            .position(|run| &run.run_id == run_id)
        {
            return Ok(u32::try_from(active_offset + index + 1).unwrap_or(u32::MAX));
        }
        store
            .safe_run_status(run_id)
            .map_err(application_runtime_error)?
            .map(|_| 1)
            .ok_or_else(|| {
                ApplicationError::new(
                    ApplicationErrorCode::Internal,
                    "admitted prompt disappeared",
                )
            })
    }

    pub(crate) fn application_suggestions(
        &self,
        request: SuggestionRequest,
    ) -> Result<SuggestionPage, ApplicationError> {
        let query = request.query.to_ascii_lowercase();
        let entries = match request.argument_id.as_str() {
            "selector" => ChatSessionStore::catalog(self.runtime.paths.sessions_root())
                .map_err(application_runtime_error)?
                .into_iter()
                .filter(|entry| entry.status == SessionCatalogStatus::Active)
                .filter(|entry| {
                    entry
                        .metadata
                        .session_id
                        .as_str()
                        .to_ascii_lowercase()
                        .contains(&query)
                })
                .map(|entry| agl_app::Suggestion {
                    value: entry.metadata.session_id.to_string(),
                    label: entry.metadata.session_id.to_string(),
                    detail: Some(
                        entry
                            .metadata
                            .execution_context
                            .workspace_root
                            .to_string_lossy()
                            .into_owned(),
                    ),
                })
                .collect(),
            "model_id" => {
                let bindings_path = agl_config::model_bindings_path(&self.runtime.paths.config_dir);
                agl_config::load_model_bindings_or_empty(&bindings_path)
                    .map_err(application_runtime_error)?
                    .models
                    .into_iter()
                    .filter(|(id, binding)| {
                        binding.path.is_file() && id.as_str().to_ascii_lowercase().contains(&query)
                    })
                    .map(|(id, binding)| agl_app::Suggestion {
                        value: id.to_string(),
                        label: id.to_string(),
                        detail: binding
                            .path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .map(str::to_owned),
                    })
                    .collect()
            }
            "mode" => ["read-only", "write", "execute", "approve", "admin"]
                .into_iter()
                .filter(|value| value.contains(&query))
                .map(|value| agl_app::Suggestion {
                    value: value.to_owned(),
                    label: value.replace('-', " "),
                    detail: None,
                })
                .collect(),
            "skill_id" => agl_skill::builtin_registry()
                .map_err(application_runtime_error)?
                .skills()
                .iter()
                .filter(|skill| {
                    skill
                        .harness
                        .id
                        .as_str()
                        .to_ascii_lowercase()
                        .contains(&query)
                        || skill.harness.name.to_ascii_lowercase().contains(&query)
                })
                .map(|skill| agl_app::Suggestion {
                    value: skill.harness.id.to_string(),
                    label: skill.harness.name.clone(),
                    detail: Some(skill.harness.description.clone()),
                })
                .collect(),
            "execution_id" => {
                let Some(session_id) = request.session_id else {
                    return Ok(SuggestionPage {
                        entries: Vec::new(),
                        next_cursor: None,
                    });
                };
                self.terminal_bridge
                    .execution_list(agent_execution_filter(Some(&session_id), None, true))
                    .map_err(application_runtime_error)?
                    .into_iter()
                    .filter(|status| {
                        status
                            .execution_id
                            .as_str()
                            .to_ascii_lowercase()
                            .contains(&query)
                    })
                    .map(|status| agl_app::Suggestion {
                        value: status.execution_id.to_string(),
                        label: status.execution_id.to_string(),
                        detail: Some(
                            format!("{:?} · {:?}", status.profile, status.state)
                                .to_ascii_lowercase(),
                        ),
                    })
                    .collect()
            }
            "path" => self.path_suggestions(request.session_id.as_ref(), &request.query)?,
            _ => Vec::new(),
        };
        Ok(SuggestionPage {
            entries,
            next_cursor: None,
        })
    }

    fn path_suggestions(
        &self,
        session_id: Option<&SessionId>,
        query: &str,
    ) -> Result<Vec<agl_app::Suggestion>, ApplicationError> {
        let session_id = session_id.ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "path suggestions require a session",
            )
        })?;
        let session = self.sessions.get(session_id).ok_or_else(|| {
            ApplicationError::new(ApplicationErrorCode::NotFound, "session not found")
        })?;
        let workspace = &session.execution_context.workspace_root;
        let requested = PathBuf::from(query);
        let candidate = if requested.is_absolute() {
            requested
        } else {
            session.execution_context.working_directory.join(requested)
        };
        let parent = if candidate.is_dir() {
            candidate.clone()
        } else {
            candidate.parent().unwrap_or(workspace).to_path_buf()
        };
        let prefix = if candidate.is_dir() {
            String::new()
        } else {
            candidate
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase()
        };
        let canonical_parent = parent.canonicalize().map_err(application_runtime_error)?;
        if !canonical_parent.starts_with(workspace) {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&canonical_parent).map_err(application_runtime_error)? {
            let entry = entry.map_err(application_runtime_error)?;
            let file_type = entry.file_type().map_err(application_runtime_error)?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.to_ascii_lowercase().starts_with(&prefix) {
                continue;
            }
            let path = entry.path();
            if !path.starts_with(workspace) {
                continue;
            }
            entries.push(agl_app::Suggestion {
                value: path.to_string_lossy().into_owned(),
                label: name,
                detail: Some("directory".to_owned()),
            });
            if entries.len() >= agl_app::MAX_SUGGESTIONS {
                break;
            }
        }
        Ok(entries)
    }

    #[cfg(test)]
    pub(crate) fn application_invoke(
        &mut self,
        request: ApplicationActionRequest,
    ) -> Result<ApplicationToolResult, ApplicationError> {
        self.application_invoke_with_context(request, &ApplicationCallContext::new())
    }

    pub(crate) fn application_invoke_with_context(
        &mut self,
        request: ApplicationActionRequest,
        context: &ApplicationCallContext,
    ) -> Result<ApplicationToolResult, ApplicationError> {
        ensure_application_call_live(context)?;
        if request.client_submission_id.is_empty() || request.client_submission_id.len() > 256 {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "client submission ID must be nonempty and bounded",
            ));
        }
        let client_submission_id = request.client_submission_id.clone();
        if let Some(session_id) = request.session_id.as_ref()
            && self.exiting_sessions.contains(session_id)
            && !matches!(&request.action, ApplicationAction::SessionExit { .. })
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::SessionBusy,
                "session exit is waiting for typed owner outcomes",
            ));
        }
        match request.action {
            ApplicationAction::SessionNew { mut launch } => {
                if let Some(source_session_id) = request.session_id.as_ref() {
                    self.ensure_turn_boundary_idle(source_session_id)?;
                    if launch.workspace_root.is_none() {
                        let workspace_root = self
                            .sessions
                            .get(source_session_id)
                            .ok_or_else(|| {
                                ApplicationError::new(
                                    ApplicationErrorCode::NotFound,
                                    "source session not found",
                                )
                            })?
                            .execution_context
                            .workspace_root
                            .to_str()
                            .ok_or_else(|| {
                                ApplicationError::new(
                                    ApplicationErrorCode::InvalidArguments,
                                    "source workspace cannot be represented by the current action protocol",
                                )
                            })?
                            .to_owned();
                        launch.workspace_root = Some(workspace_root);
                    }
                }
                self.application_open_session(SessionOpen { launch })
                    .map(|opened| ApplicationToolResult::SessionOpened {
                        opened: Box::new(opened),
                    })
            }
            ApplicationAction::SessionResume { selector } => {
                if let Some(source_session_id) = request.session_id.as_ref() {
                    self.ensure_turn_boundary_idle(source_session_id)?;
                }
                let session_id = match selector {
                    agl_app::SessionSelector::Id { session_id } => session_id,
                    agl_app::SessionSelector::Latest => {
                        ChatSessionStore::catalog(self.runtime.paths.sessions_root())
                            .map_err(application_runtime_error)?
                            .into_iter()
                            .filter(|entry| entry.status == SessionCatalogStatus::Active)
                            .max_by(|left, right| {
                                left.metadata
                                    .updated_at_unix_ms
                                    .cmp(&right.metadata.updated_at_unix_ms)
                                    .then_with(|| {
                                        left.metadata.session_id.cmp(&right.metadata.session_id)
                                    })
                            })
                            .map(|entry| entry.metadata.session_id)
                            .ok_or_else(|| {
                                ApplicationError::new(
                                    ApplicationErrorCode::NotFound,
                                    "no resumable session found",
                                )
                            })?
                    }
                };
                let opened = match self
                    .open_session(agl_protocol::SessionOpenRequest {
                        session_id: Some(session_id),
                        new_session: false,
                        workspace_root: None,
                        function_ref: None,
                        skills: Vec::new(),
                        tool_mode: ProtocolToolMode::ReadOnly,
                    })
                    .map_err(application_protocol_error)?
                {
                    DaemonEventKind::SessionOpened(opened) => opened,
                    _ => unreachable!("session resume has one response family"),
                };
                let snapshot = self.application_snapshot(&opened.session_id)?;
                Ok(ApplicationToolResult::SessionOpened {
                    opened: Box::new(SessionOpened {
                        session_id: opened.session_id,
                        resumed: true,
                        snapshot,
                    }),
                })
            }
            action => {
                let session_id = request.session_id.ok_or_else(|| {
                    ApplicationError::new(
                        ApplicationErrorCode::InvalidArguments,
                        "application action requires a current session",
                    )
                })?;
                self.reconcile_incomplete_continuations_for_session(&session_id)?;
                if matches!(
                    &action,
                    ApplicationAction::ModelSelect { .. }
                        | ApplicationAction::OperationModeSelect { .. }
                        | ApplicationAction::SkillsSelect { .. }
                        | ApplicationAction::WorkspaceSet { .. }
                        | ApplicationAction::RuntimeContextReload
                        | ApplicationAction::SessionClear
                ) {
                    self.ensure_turn_boundary_idle(&session_id)?;
                }
                match action {
                    ApplicationAction::SessionStatus | ApplicationAction::WorkspaceGet => self
                        .application_snapshot(&session_id)
                        .map(|snapshot| ApplicationToolResult::Status {
                            header: snapshot.header,
                        }),
                    ApplicationAction::WorkspaceSet {
                        path,
                        confirm_terminate_terminals,
                    } => {
                        let next_workspace = self
                            .chat_factory
                            .with_session(&session_id, |service| {
                                service.preflight_workspace_root(&path)
                            })
                            .map_err(application_workspace_preflight_error)?;
                        let old_workspace = self
                            .sessions
                            .get(&session_id)
                            .ok_or_else(|| {
                                ApplicationError::new(
                                    ApplicationErrorCode::NotFound,
                                    "session not found",
                                )
                            })?
                            .execution_context
                            .workspace_root
                            .clone();
                        let active = self.session_work_counts(&session_id)?;
                        if (active.terminals != 0 || active.executions != 0)
                            && !confirm_terminate_terminals
                        {
                            return Err(ApplicationError::new(
                                ApplicationErrorCode::ConfirmationRequired,
                                format!(
                                    "workspace change will terminate {} terminal(s) and {} other execution(s)",
                                    active.terminals, active.executions
                                ),
                            ));
                        }
                        if active.terminals != 0 || active.executions != 0 {
                            self.terminate_session_work(&session_id, context)?;
                        }
                        self.retire_workspace_terminal_slots(&session_id, &old_workspace)?;
                        self.chat_factory
                            .with_session(&session_id, |service| {
                                service.set_workspace_root(&next_workspace)
                            })
                            .map_err(application_busy_error)?;
                        self.refresh_session_execution_context(&session_id)?;
                        self.application_snapshot(&session_id).map(|snapshot| {
                            ApplicationToolResult::WorkspaceChanged {
                                header: snapshot.header,
                            }
                        })
                    }
                    ApplicationAction::TerminalList { include_finished } => self
                        .application_terminal_records(&session_id, include_finished)?
                        .iter()
                        .map(|record| self.terminal_view(record))
                        .collect::<Result<Vec<_>, _>>()
                        .map(|terminals| ApplicationToolResult::Terminals { terminals }),
                    ApplicationAction::TerminalPromote { terminal_id } => {
                        let current = self
                            .terminal_bridge
                            .record(terminal_topology_id(&session_id), &terminal_id)
                            .map_err(application_runtime_error)?;
                        if current.topology_id != terminal_topology_id(&session_id) {
                            return Err(ApplicationError::new(
                                ApplicationErrorCode::TerminalOwnerMismatch,
                                "terminal belongs to a different durable session",
                            ));
                        }
                        let promoted = self
                            .terminal_bridge
                            .promote(
                                terminal_id,
                                terminal_topology_id(&session_id),
                                agent_caller_owner(
                                    session_id.as_str(),
                                    CallerOwnerKind::Persistent,
                                    CallerRole::Agent,
                                ),
                            )
                            .map_err(application_runtime_error)?;
                        Ok(ApplicationToolResult::TerminalPromoted {
                            terminal: self.terminal_view(&promoted)?,
                        })
                    }
                    ApplicationAction::IncompleteTurnContinue {
                        message_id,
                        expected_execution_context_revision,
                    } => self
                        .application_continue_incomplete(
                            &session_id,
                            message_id,
                            &client_submission_id,
                            expected_execution_context_revision,
                        )
                        .map(|admission| ApplicationToolResult::IncompleteTurnContinued {
                            admission,
                        }),
                    ApplicationAction::RuntimeContextReload => {
                        let (visible_tools, revision) = self
                            .chat_factory
                            .with_session(&session_id, |service| {
                                let visible = service.reload_runtime_context()?;
                                Ok((visible, service.runtime_selection_revision()))
                            })
                            .map_err(application_busy_error)?;
                        self.sessions
                            .get_mut(&session_id)
                            .ok_or_else(|| {
                                ApplicationError::new(
                                    ApplicationErrorCode::NotFound,
                                    "session not found",
                                )
                            })?
                            .runtime_context_revision = revision;
                        Ok(ApplicationToolResult::Reloaded {
                            visible_tools: (0..visible_tools)
                                .map(|index| format!("visible-tool-{index}"))
                                .collect(),
                            context_revision: revision,
                        })
                    }
                    ApplicationAction::SessionClear => {
                        let removed_messages = self
                            .chat_factory
                            .with_session(&session_id, |service| service.clear_context())
                            .map_err(application_busy_error)?;
                        Ok(ApplicationToolResult::Cleared {
                            removed_messages: removed_messages as u64,
                            cursor: PresentationCursor {
                                daemon_instance_id: self.daemon_instance_id.clone(),
                                revision: 0,
                            },
                        })
                    }
                    ApplicationAction::SessionExit { confirm_active } => {
                        let active_runs = self.active_session_root_runs(&session_id)?;
                        let active = self.session_work_counts(&session_id)?;
                        if (!active_runs.is_empty()
                            || active.terminals != 0
                            || active.executions != 0)
                            && !confirm_active
                        {
                            return Err(ApplicationError::new(
                                ApplicationErrorCode::SessionBusy,
                                format!(
                                    "session has {} active or queued root run(s), {} terminal(s), and {} other execution(s); confirmation is required",
                                    active_runs.len(),
                                    active.terminals,
                                    active.executions
                                ),
                            ));
                        }
                        let cancelled_runs = self.cancel_session_root_runs(active_runs, context)?;
                        let (_, terminated) = self.finish_session_with_counts(
                            session_id.clone(),
                            agl_protocol::SessionFinishReason::ExitCommand,
                            context,
                        )?;
                        Ok(ApplicationToolResult::SessionExited {
                            session_id,
                            cancelled_runs,
                            terminated_terminals: terminated.terminals,
                            terminated_executions: terminated.executions,
                        })
                    }
                    ApplicationAction::ModelSelect { model_id } => {
                        self.select_session_model(&session_id, &model_id)?;
                        self.application_snapshot(&session_id).map(|snapshot| {
                            ApplicationToolResult::ModelChanged {
                                header: snapshot.header,
                            }
                        })
                    }
                    ApplicationAction::OperationModeSelect { mode } => {
                        let revision = self
                            .chat_factory
                            .with_session(&session_id, |service| {
                                service.select_operation_mode(mode)?;
                                Ok(service.runtime_selection_revision())
                            })
                            .map_err(application_mode_error)?;
                        let session = self.sessions.get_mut(&session_id).ok_or_else(|| {
                            ApplicationError::new(
                                ApplicationErrorCode::NotFound,
                                "session not found",
                            )
                        })?;
                        session.options.inference.tool_mode = mode;
                        session.runtime_context_revision = revision;
                        self.application_snapshot(&session_id).map(|snapshot| {
                            ApplicationToolResult::ModeChanged {
                                header: snapshot.header,
                            }
                        })
                    }
                    ApplicationAction::SkillsSelect { skill_ids } => {
                        let (selected, revision) = self
                            .chat_factory
                            .with_session(&session_id, |service| {
                                let selected = service.select_skills(skill_ids)?;
                                Ok((selected, service.runtime_selection_revision()))
                            })
                            .map_err(application_skill_error)?;
                        let session = self.sessions.get_mut(&session_id).ok_or_else(|| {
                            ApplicationError::new(
                                ApplicationErrorCode::NotFound,
                                "session not found",
                            )
                        })?;
                        session.options.inference.skills = selected;
                        session.runtime_context_revision = revision;
                        self.application_snapshot(&session_id).map(|snapshot| {
                            ApplicationToolResult::SkillsChanged {
                                header: snapshot.header,
                            }
                        })
                    }
                    ApplicationAction::SessionNew { .. }
                    | ApplicationAction::SessionResume { .. } => unreachable!(),
                }
            }
        }
    }

    fn session_execution_bridge(
        &self,
        session_id: &SessionId,
    ) -> Result<TerminalBridge, ApplicationError> {
        let policy_hash = self
            .chat_factory
            .current_policy_hash(session_id)
            .map_err(application_runtime_error)?
            .ok_or_else(|| {
                ApplicationError::new(
                    ApplicationErrorCode::NotAuthorized,
                    "session effective tool policy is unavailable",
                )
            })?;
        self.terminal_bridge
            .with_authority(policy_hash)
            .map_err(application_runtime_error)
    }

    fn refresh_session_execution_context(
        &mut self,
        session_id: &SessionId,
    ) -> Result<(), ApplicationError> {
        let execution_context = self
            .chat_factory
            .with_session(session_id, |service| {
                Ok(service.execution_context().clone())
            })
            .map_err(application_busy_error)?;
        self.sessions
            .get_mut(session_id)
            .ok_or_else(|| {
                ApplicationError::new(ApplicationErrorCode::NotFound, "session not found")
            })?
            .execution_context = execution_context;
        Ok(())
    }

    fn ensure_turn_boundary_idle(&self, session_id: &SessionId) -> Result<(), ApplicationError> {
        let key =
            agl_store::RunConcurrencyKey::session(session_id).map_err(application_runtime_error)?;
        let store = AglStore::open_current_read_only_at(self.runtime.paths.store_root())
            .map_err(application_runtime_error)?;
        if !store
            .safe_runs_for_concurrency_key(&key, false)
            .map_err(application_runtime_error)?
            .is_empty()
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::SessionBusy,
                "session has an active or queued prompt",
            ));
        }
        Ok(())
    }

    fn select_session_model(
        &mut self,
        session_id: &SessionId,
        model_id: &str,
    ) -> Result<(), ApplicationError> {
        self.ensure_turn_boundary_idle(session_id)?;
        let parsed = agl_config::ModelId::new(model_id.to_owned()).map_err(|error| {
            ApplicationError::new(ApplicationErrorCode::InvalidArguments, error.to_string())
        })?;
        let bindings_path = agl_config::model_bindings_path(&self.runtime.paths.config_dir);
        let bindings = agl_config::load_model_bindings_or_empty(&bindings_path)
            .map_err(application_runtime_error)?;
        let binding = bindings.models.get(&parsed).ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::ModelNotInstalled,
                format!("model `{model_id}` is not installed"),
            )
        })?;
        if !binding.path.is_file() {
            return Err(ApplicationError::new(
                ApplicationErrorCode::ModelNotInstalled,
                format!("model `{model_id}` binding is unavailable"),
            ));
        }
        let revision = self
            .chat_factory
            .with_session(session_id, |service| {
                service.select_model(model_id, binding.path.clone())?;
                Ok(service.runtime_selection_revision())
            })
            .map_err(application_model_error)?;
        let session = self.sessions.get_mut(session_id).ok_or_else(|| {
            ApplicationError::new(ApplicationErrorCode::NotFound, "session not found")
        })?;
        session.selected_model_id = Some(model_id.to_owned());
        session.runtime_context_revision = revision;
        Ok(())
    }
}

fn admitted_terminal_shell(
    context: &agl_exec::ExecutionContextSnapshot,
    requested_profile_id: &str,
) -> Result<AdmittedShellProfile, ApplicationError> {
    let executable = context
        .shell
        .program
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::CommandUnavailable,
                "configured shell executable has no UTF-8 basename",
            )
        })?;
    let kind = match executable {
        "bash" => AdmittedShellKind::Bash,
        "zsh" => AdmittedShellKind::Zsh,
        other => {
            return Err(ApplicationError::new(
                ApplicationErrorCode::CommandUnavailable,
                format!("configured shell `{other}` is not an admitted persistent shell"),
            ));
        }
    };
    let admitted_profile_id = terminal_shell_profile_id(kind);
    if requested_profile_id != admitted_profile_id {
        return Err(ApplicationError::new(
            ApplicationErrorCode::InvalidArguments,
            format!(
                "shell profile `{requested_profile_id}` does not match admitted profile `{admitted_profile_id}`"
            ),
        ));
    }
    let shell = AdmittedShellProfile {
        kind,
        snapshot: context.shell.clone(),
    };
    shell.validate().map_err(terminal_application_error)?;
    Ok(shell)
}

fn terminal_shell_profile_id(kind: AdmittedShellKind) -> &'static str {
    match kind {
        AdmittedShellKind::Bash => "bash-managed",
        AdmittedShellKind::Zsh => "zsh-managed",
    }
}

fn resolve_host_startup(
    requested: agl_app::HostStartupPolicy,
    shell: AdmittedShellKind,
    operator_uid: u32,
) -> Result<ProcessHostStartupPolicy, ApplicationError> {
    match requested {
        agl_app::HostStartupPolicy::ManagedOnly => Ok(ProcessHostStartupPolicy::ManagedOnly),
        agl_app::HostStartupPolicy::SourceUserRc => {
            let home = local_operator_home(operator_uid)?;
            source_user_rc_at(&home, shell)
        }
    }
}

fn source_user_rc_at(
    home: &Path,
    shell: AdmittedShellKind,
) -> Result<ProcessHostStartupPolicy, ApplicationError> {
    let file_name = match shell {
        AdmittedShellKind::Bash => ".bashrc",
        AdmittedShellKind::Zsh => ".zshrc",
    };
    let path = home.join(file_name).canonicalize().map_err(|_| {
        ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "the explicitly requested user shell rc is unavailable",
        )
    })?;
    if !path.is_file() {
        return Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "the explicitly requested user shell rc is not a regular file",
        ));
    }
    Ok(ProcessHostStartupPolicy::SourceUserRc { path })
}

#[cfg(unix)]
fn local_operator_home(operator_uid: u32) -> Result<PathBuf, ApplicationError> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt as _;

    const FALLBACK_BUFFER_BYTES: usize = 16 * 1024;
    const MAX_BUFFER_BYTES: usize = 1024 * 1024;

    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut buffer_bytes = usize::try_from(suggested)
        .ok()
        .filter(|size| *size > 0)
        .unwrap_or(FALLBACK_BUFFER_BYTES)
        .clamp(1024, MAX_BUFFER_BYTES);
    loop {
        let mut record = std::mem::MaybeUninit::<libc::passwd>::zeroed();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; buffer_bytes];
        let code = unsafe {
            libc::getpwuid_r(
                operator_uid,
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if code == libc::ERANGE && buffer_bytes < MAX_BUFFER_BYTES {
            buffer_bytes = buffer_bytes.saturating_mul(2).min(MAX_BUFFER_BYTES);
            continue;
        }
        if code != 0 || result.is_null() {
            return Err(ApplicationError::new(
                ApplicationErrorCode::NotAuthorized,
                "the local operator account could not be resolved",
            ));
        }
        let directory_pointer = unsafe { (*result).pw_dir };
        if directory_pointer.is_null() {
            return Err(ApplicationError::new(
                ApplicationErrorCode::NotAuthorized,
                "the local operator account has no home directory",
            ));
        }
        let directory = unsafe { CStr::from_ptr(directory_pointer) }.to_bytes();
        if directory.is_empty() {
            return Err(ApplicationError::new(
                ApplicationErrorCode::NotAuthorized,
                "the local operator account has no home directory",
            ));
        }
        let home = PathBuf::from(OsStr::from_bytes(directory))
            .canonicalize()
            .map_err(|_| {
                ApplicationError::new(
                    ApplicationErrorCode::CommandUnavailable,
                    "the local operator home directory is unavailable",
                )
            })?;
        if !home.is_dir() {
            return Err(ApplicationError::new(
                ApplicationErrorCode::CommandUnavailable,
                "the local operator home path is not a directory",
            ));
        }
        return Ok(home);
    }
}

#[cfg(not(unix))]
fn local_operator_home(_operator_uid: u32) -> Result<PathBuf, ApplicationError> {
    Err(ApplicationError::new(
        ApplicationErrorCode::CommandUnavailable,
        "Human Host terminals are available only on Unix",
    ))
}

#[cfg(test)]
mod host_startup_tests {
    use super::*;

    #[test]
    fn managed_host_startup_never_resolves_an_operator_account() {
        assert_eq!(
            resolve_host_startup(
                agl_app::HostStartupPolicy::ManagedOnly,
                AdmittedShellKind::Bash,
                u32::MAX,
            )
            .unwrap(),
            ProcessHostStartupPolicy::ManagedOnly
        );
    }

    #[test]
    fn explicit_source_user_rc_maps_only_the_admitted_shell_rc() {
        let home = std::env::temp_dir().join(format!(
            "agl-host-rc-test-{}-{}",
            std::process::id(),
            RequestId::generate()
        ));
        std::fs::create_dir(&home).unwrap();
        let bashrc = home.join(".bashrc");
        std::fs::write(&bashrc, b"export AGL_RC_TEST=1\n").unwrap();
        let bashrc = bashrc.canonicalize().unwrap();

        assert_eq!(
            source_user_rc_at(&home, AdmittedShellKind::Bash).unwrap(),
            ProcessHostStartupPolicy::SourceUserRc { path: bashrc }
        );
        assert_eq!(
            source_user_rc_at(&home, AdmittedShellKind::Zsh)
                .unwrap_err()
                .code,
            ApplicationErrorCode::CommandUnavailable
        );
        std::fs::remove_dir_all(home).unwrap();
    }
}

fn terminal_admitted_path_roots(
    configured_roots: &[PathBuf],
) -> Result<Vec<PathBuf>, ApplicationError> {
    let mut roots = agl_pty::standard_runtime_roots().map_err(terminal_application_error)?;
    for root in configured_roots {
        let canonical = root.canonicalize().map_err(application_runtime_error)?;
        if !canonical.is_dir() {
            return Err(ApplicationError::new(
                ApplicationErrorCode::CommandUnavailable,
                format!(
                    "configured terminal runtime root is not a directory: {}",
                    root.display()
                ),
            ));
        }
        roots.push(canonical);
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn build_terminal_path(
    inherited_path: &str,
    shell_program: &Path,
    admitted_roots: &[PathBuf],
) -> Result<String, ApplicationError> {
    if admitted_roots.is_empty() {
        return Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "persistent workspace terminal requires admitted Linux runtime roots",
        ));
    }
    if !admitted_roots
        .iter()
        .any(|root| shell_program.starts_with(root))
    {
        return Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "configured shell is outside admitted Linux runtime roots",
        ));
    }

    let mut seen = BTreeSet::new();
    let mut paths = Vec::new();
    let mut admit = |candidate: PathBuf| {
        if candidate.is_dir()
            && admitted_roots
                .iter()
                .any(|root| candidate.starts_with(root))
            && seen.insert(candidate.clone())
        {
            paths.push(candidate);
        }
    };
    for candidate in std::env::split_paths(inherited_path) {
        if let Ok(canonical) = candidate.canonicalize() {
            admit(canonical);
        }
    }
    if let Some(parent) = shell_program.parent() {
        admit(parent.to_path_buf());
    }
    for root in admitted_roots {
        if root.file_name().and_then(|name| name.to_str()) == Some("bin") {
            admit(root.clone());
        }
        if let Ok(bin) = root.join("bin").canonicalize() {
            admit(bin);
        }
    }
    if paths.is_empty() {
        return Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "admitted runtime roots provide no terminal PATH directories",
        ));
    }
    if !paths.iter().any(|path| path.join("ls").is_file()) {
        return Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "admitted terminal PATH does not provide the required `ls` utility",
        ));
    }
    std::env::join_paths(paths)
        .map_err(application_runtime_error)?
        .into_string()
        .map_err(|_| {
            ApplicationError::new(
                ApplicationErrorCode::CommandUnavailable,
                "admitted terminal PATH is not valid UTF-8",
            )
        })
}

fn application_terminal_prompt_state(
    state: &ProcessTerminalPromptState,
) -> agl_app::TerminalPromptState {
    match state {
        ProcessTerminalPromptState::Unknown => agl_app::TerminalPromptState::Starting,
        ProcessTerminalPromptState::Ready { .. } => agl_app::TerminalPromptState::Ready,
        ProcessTerminalPromptState::CommandRunning { .. } => {
            agl_app::TerminalPromptState::CommandRunning
        }
        ProcessTerminalPromptState::ForegroundProgram { .. } => {
            agl_app::TerminalPromptState::ForegroundProcess
        }
        ProcessTerminalPromptState::Degraded => agl_app::TerminalPromptState::Degraded,
    }
}

fn human_terminal_fingerprint(request: &HumanTerminalEnsure) -> Result<String, ApplicationError> {
    human_terminal_request_digest(b"agentlibre.human-terminal-submission.v1\0", request)
}

fn workspace_history_scope(workspace_root: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(b"agentlibre.cli.workspace-history.v1\0");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        digest.update(workspace_root.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    digest.update(workspace_root.to_string_lossy().as_bytes());
    let bytes = digest.finalize();
    let mut rendered = String::with_capacity(7 + bytes.len() * 2);
    rendered.push_str("sha256:");
    use std::fmt::Write as _;
    for byte in bytes {
        write!(&mut rendered, "{byte:02x}").expect("writing to a String cannot fail");
    }
    rendered
}

fn human_terminal_admission_fingerprint(
    request: &HumanTerminalEnsure,
) -> Result<String, ApplicationError> {
    let mut stable = request.clone();
    stable.client_submission_id.clear();
    stable.execution_context_revision = 0;
    stable.terminal_size = agl_exec::TerminalSize::default();
    human_terminal_request_digest(b"agentlibre.human-terminal-admission.v1\0", &stable)
}

fn human_terminal_request_digest(
    domain: &[u8],
    request: &HumanTerminalEnsure,
) -> Result<String, ApplicationError> {
    let encoded = serde_json::to_vec(request).map_err(application_runtime_error)?;
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(encoded);
    let bytes = digest.finalize();
    let mut rendered = String::with_capacity(7 + bytes.len() * 2);
    rendered.push_str("sha256:");
    use std::fmt::Write as _;
    for byte in bytes {
        write!(&mut rendered, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(rendered)
}

fn bounded_count(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

fn paginate_presentation_snapshot(
    mut snapshot: SessionPresentationSnapshot,
    mut replay: ChatSessionReverseReader,
    cursor_scope: &str,
    incomplete_context: &IncompleteProjectionContext,
) -> Result<PresentationSnapshotPage, ApplicationError> {
    debug_assert!(snapshot.items.is_empty());
    let empty_snapshot_bytes = serde_json::to_vec(&snapshot)
        .map_err(|_| {
            ApplicationError::new(
                ApplicationErrorCode::Internal,
                "session presentation page could not be encoded",
            )
        })?
        .len();
    let mut selected_item_bytes = 0usize;
    let mut selected_items = Vec::new();
    let mut scanned_bytes = 0usize;
    let mut scanned_records = 0usize;
    let continuation_offset = loop {
        if selected_items.len() >= MAX_PRESENTATION_ITEMS
            || scanned_records >= MAX_PRESENTATION_TRANSCRIPT_SCAN_RECORDS
            || scanned_bytes >= MAX_PRESENTATION_TRANSCRIPT_SCAN_BYTES
        {
            break (replay.next_offset() > 0).then_some(replay.next_offset());
        }
        let remaining_scan_bytes = MAX_PRESENTATION_TRANSCRIPT_SCAN_BYTES - scanned_bytes;
        let record = match replay
            .next_record(remaining_scan_bytes)
            .map_err(application_runtime_error)?
        {
            ChatSessionReverseRead::Record(record) => record,
            ChatSessionReverseRead::ScanLimitReached => {
                break (replay.next_offset() > 0).then_some(replay.next_offset());
            }
            ChatSessionReverseRead::End => break None,
        };
        scanned_bytes = scanned_bytes
            .checked_add(record.transcript_bytes)
            .ok_or_else(|| {
                ApplicationError::new(
                    ApplicationErrorCode::Internal,
                    "session presentation transcript scan byte count overflowed",
                )
            })?;
        scanned_records += 1;

        let Some(item) = presentation_item_from_transcript_event(record.event, incomplete_context)
        else {
            continue;
        };
        let item_bytes = serde_json::to_vec(&item)
            .map_err(|_| {
                ApplicationError::new(
                    ApplicationErrorCode::Internal,
                    "session presentation item could not be encoded",
                )
            })?
            .len();
        let separator_bytes = usize::from(!selected_items.is_empty());
        let candidate_item_bytes = selected_item_bytes
            .checked_add(separator_bytes)
            .and_then(|bytes| bytes.checked_add(item_bytes))
            .ok_or_else(|| {
                ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "session presentation page exceeds its byte bound",
                )
            })?;
        let older_cursor = (record.start_offset > 0)
            .then(|| format_presentation_page_cursor(cursor_scope, record.start_offset));
        let wire_bytes = empty_snapshot_bytes
            .checked_add(candidate_item_bytes)
            .and_then(|bytes| bytes.checked_add(presentation_wire_cursor_bytes(&older_cursor)))
            .ok_or_else(|| {
                ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "session presentation page exceeds its byte bound",
                )
            })?;
        if wire_bytes > MAX_PRESENTATION_CONTENT_BYTES {
            if selected_items.is_empty() {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "a session presentation item cannot fit in one bounded page",
                ));
            }
            break Some(record.end_offset);
        }
        selected_item_bytes = candidate_item_bytes;
        selected_items.push(item);
    };
    selected_items.reverse();
    snapshot.items = selected_items;
    snapshot.validate()?;
    let older_page_cursor =
        continuation_offset.map(|offset| format_presentation_page_cursor(cursor_scope, offset));
    let canonical_wire_bytes = serde_json::to_vec(&snapshot)
        .map_err(|_| {
            ApplicationError::new(
                ApplicationErrorCode::Internal,
                "session presentation page could not be encoded",
            )
        })?
        .len()
        .checked_add(presentation_wire_cursor_bytes(&older_page_cursor))
        .ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "session presentation page exceeds its byte bound",
            )
        })?;
    if canonical_wire_bytes > MAX_PRESENTATION_CONTENT_BYTES {
        return Err(ApplicationError::new(
            ApplicationErrorCode::InvalidArguments,
            "session presentation page exceeds its byte bound",
        ));
    }
    Ok(PresentationSnapshotPage {
        snapshot,
        older_page_cursor,
    })
}

fn presentation_item_from_transcript_event(
    event: agl_session::ChatSessionEvent,
    incomplete_context: &IncompleteProjectionContext,
) -> Option<SessionPresentationItem> {
    match event {
        agl_session::ChatSessionEvent::Runtime { envelope } => match envelope.payload {
            agl_events::RuntimeEvent::UserMessage {
                message_id,
                content,
            } => Some(SessionPresentationItem::UserMessage {
                message_id,
                content,
            }),
            agl_events::RuntimeEvent::AssistantMessage {
                message_id,
                content,
            } => Some(SessionPresentationItem::AssistantMessage {
                message_id,
                content,
                state: agl_app::AssistantItemState::Final,
            }),
            agl_events::RuntimeEvent::AssistantIncomplete {
                message_id,
                content,
                source_attempt_id,
                reason,
                continuation_index,
                execution_context_revision,
                runtime_context_revision,
                policy_hash,
            } => {
                let continue_action = if let Some(claim) =
                    incomplete_context.claims.get(&message_id)
                {
                    ContinueActionView::Claimed {
                        continuation_run_id: claim.continuation_run_id.clone(),
                    }
                } else if incomplete_context.status != SessionPresentationStatus::Active {
                    ContinueActionView::Unavailable {
                        reason: ContinueUnavailableReason::SessionFinished,
                    }
                } else if !incomplete_context
                    .current_context_messages
                    .contains(&message_id)
                    || execution_context_revision != incomplete_context.execution_context_revision
                    || runtime_context_revision != incomplete_context.runtime_context_revision
                {
                    ContinueActionView::Unavailable {
                        reason: ContinueUnavailableReason::StaleContext,
                    }
                } else if incomplete_context.current_policy_hash.as_str() != policy_hash.as_str() {
                    ContinueActionView::Unavailable {
                        reason: ContinueUnavailableReason::PolicyDenied,
                    }
                } else {
                    ContinueActionView::Available
                };
                Some(SessionPresentationItem::IncompleteAssistant {
                    item: IncompleteAssistantItemView {
                        message_id,
                        content,
                        source_run_id: envelope.scope.run_id().clone(),
                        source_turn_id: envelope
                            .scope
                            .turn_id()
                            .expect("session transcript runtime event has a turn")
                            .clone(),
                        source_attempt_id,
                        reason: match reason {
                            agl_events::IncompleteOutputReasonEvent::ModelLength => {
                                IncompleteOutputReason::ModelLength
                            }
                            agl_events::IncompleteOutputReasonEvent::ContentByteLimit => {
                                IncompleteOutputReason::ContentByteLimit
                            }
                        },
                        continuation_index,
                        continue_action,
                    },
                })
            }
            _ => None,
        },
        agl_session::ChatSessionEvent::ContextCleared { .. } => {
            Some(SessionPresentationItem::ContextBoundary {
                event_id: EventId::generate(),
                reason: "context_cleared".to_owned(),
            })
        }
        agl_session::ChatSessionEvent::SessionStarted { .. }
        | agl_session::ChatSessionEvent::SessionFinished { .. }
        | agl_session::ChatSessionEvent::SessionFailed { .. }
        | agl_session::ChatSessionEvent::IncompleteContinuationClaimed { .. }
        | agl_session::ChatSessionEvent::IncompleteContinuationInputStarted { .. } => None,
    }
}

fn incomplete_replay_index(events: &[agl_session::ChatSessionEvent]) -> IncompleteReplayIndex {
    let mut claims = BTreeMap::new();
    let mut claim_order = Vec::new();
    let mut current_context_messages = BTreeSet::new();
    for event in events {
        match event {
            agl_session::ChatSessionEvent::Runtime { envelope } => {
                if let agl_events::RuntimeEvent::AssistantIncomplete { message_id, .. } =
                    &envelope.payload
                {
                    current_context_messages.insert(message_id.clone());
                }
            }
            agl_session::ChatSessionEvent::ContextCleared { .. } => {
                current_context_messages.clear();
            }
            agl_session::ChatSessionEvent::IncompleteContinuationClaimed {
                message_id,
                client_submission_id,
                continuation_run_id,
                continuation_turn_id,
                continuation_request_id,
                ..
            } => {
                if !claims.contains_key(message_id) {
                    claim_order.push(message_id.clone());
                    claims.insert(
                        message_id.clone(),
                        IncompleteContinuationClaim {
                            client_submission_id: client_submission_id.clone(),
                            continuation_run_id: continuation_run_id.clone(),
                            continuation_turn_id: continuation_turn_id.clone(),
                            continuation_request_id: continuation_request_id.clone(),
                        },
                    );
                }
            }
            agl_session::ChatSessionEvent::SessionStarted { .. }
            | agl_session::ChatSessionEvent::SessionFinished { .. }
            | agl_session::ChatSessionEvent::SessionFailed { .. }
            | agl_session::ChatSessionEvent::IncompleteContinuationInputStarted { .. } => {}
        }
    }
    IncompleteReplayIndex {
        claims,
        claim_order,
        current_context_messages,
    }
}

fn incomplete_continuation_source(
    events: &[agl_session::ChatSessionEvent],
    requested_message_id: &MessageId,
) -> Option<IncompleteContinuationSource> {
    events.iter().find_map(|event| {
        let agl_session::ChatSessionEvent::Runtime { envelope } = event else {
            return None;
        };
        let agl_events::RuntimeEvent::AssistantIncomplete {
            message_id,
            continuation_index,
            execution_context_revision,
            runtime_context_revision,
            policy_hash,
            ..
        } = &envelope.payload
        else {
            return None;
        };
        if message_id != requested_message_id {
            return None;
        }
        Some(IncompleteContinuationSource {
            message_id: message_id.clone(),
            source_run_id: envelope.scope.run_id().clone(),
            source_turn_id: envelope.scope.turn_id()?.clone(),
            continuation_index: *continuation_index,
            execution_context_revision: *execution_context_revision,
            runtime_context_revision: *runtime_context_revision,
            policy_hash: policy_hash.clone(),
        })
    })
}

fn incomplete_continuation_fingerprint(
    session_id: &SessionId,
    source: &IncompleteContinuationSource,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"agentlibre.incomplete-continuation.v1\0");
    for value in [
        session_id.as_str(),
        source.message_id.as_str(),
        source.source_run_id.as_str(),
        source.source_turn_id.as_str(),
        source.policy_hash.as_str(),
    ] {
        digest.update(value.as_bytes());
        digest.update(b"\0");
    }
    digest.update(source.continuation_index.to_le_bytes());
    digest.update(source.execution_context_revision.to_le_bytes());
    digest.update(source.runtime_context_revision.to_le_bytes());
    let bytes = digest.finalize();
    let mut rendered = String::with_capacity(7 + bytes.len() * 2);
    rendered.push_str("sha256:");
    use std::fmt::Write as _;
    for byte in bytes {
        write!(&mut rendered, "{byte:02x}").expect("writing to a String cannot fail");
    }
    rendered
}

fn presentation_page_cursor_scope(
    session_id: &SessionId,
    daemon_instance_id: &DaemonInstanceId,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentlibre.presentation-page.v1\0");
    hasher.update(session_id.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(daemon_instance_id.as_str().as_bytes());
    let digest = hasher.finalize();
    let mut scope = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        write!(&mut scope, "{byte:02x}").expect("writing to a String cannot fail");
    }
    scope
}

fn format_presentation_page_cursor(scope: &str, end: u64) -> String {
    format!("{PRESENTATION_PAGE_CURSOR_PREFIX}.{scope}.{end:x}")
}

fn parse_presentation_page_cursor(
    cursor: &str,
    expected_scope: &str,
    transcript_len: u64,
) -> Result<u64, ApplicationError> {
    let mut components = cursor.split('.');
    let prefix = components.next();
    let scope = components.next();
    let end = components.next();
    if prefix != Some(PRESENTATION_PAGE_CURSOR_PREFIX)
        || scope != Some(expected_scope)
        || components.next().is_some()
    {
        return Err(invalid_presentation_page_cursor());
    }
    let end = end.ok_or_else(invalid_presentation_page_cursor)?;
    let parsed = u64::from_str_radix(end, 16).map_err(|_| invalid_presentation_page_cursor())?;
    if end != format!("{parsed:x}") {
        return Err(invalid_presentation_page_cursor());
    }
    if parsed == 0 || parsed > transcript_len {
        return Err(invalid_presentation_page_cursor());
    }
    Ok(parsed)
}

fn presentation_wire_cursor_bytes(older_page_cursor: &Option<String>) -> usize {
    const FIELD_PREFIX_BYTES: usize = b",\"older_page_cursor\":".len();
    FIELD_PREFIX_BYTES
        + serde_json::to_vec(older_page_cursor)
            .expect("a bounded generated presentation cursor must be JSON encodable")
            .len()
}

fn invalid_presentation_page_cursor() -> ApplicationError {
    ApplicationError::new(
        ApplicationErrorCode::InvalidArguments,
        "presentation page cursor is invalid or stale",
    )
}

#[cfg(test)]
mod presentation_paging_tests {
    use super::*;

    #[test]
    fn presentation_pages_are_contiguous_bounded_and_epoch_scoped() {
        let session_id = SessionId::generate();
        let daemon_instance_id = DaemonInstanceId::generate();
        let snapshot = empty_presentation_snapshot(&session_id, &daemon_instance_id);
        let items = (0..=MAX_PRESENTATION_ITEMS)
            .map(|index| message_item(format!("message-{index}")))
            .collect::<Vec<_>>();
        let root = write_presentation_transcript(&session_id, &items);
        let cursor_scope = presentation_page_cursor_scope(&session_id, &daemon_instance_id);
        let incomplete_context = empty_incomplete_context();
        let expected_keys = items
            .iter()
            .map(SessionPresentationItem::key)
            .collect::<Vec<_>>();

        let latest = paginate_presentation_snapshot(
            snapshot.clone(),
            open_presentation_replay(&root, &session_id, None, &cursor_scope),
            &cursor_scope,
            &incomplete_context,
        )
        .unwrap();
        assert_eq!(latest.snapshot.items.len(), MAX_PRESENTATION_ITEMS);
        assert_eq!(
            latest
                .snapshot
                .items
                .iter()
                .map(SessionPresentationItem::key)
                .collect::<Vec<_>>(),
            expected_keys[1..]
        );
        let cursor = latest.older_page_cursor.unwrap();
        let older = paginate_presentation_snapshot(
            snapshot.clone(),
            open_presentation_replay(&root, &session_id, Some(&cursor), &cursor_scope),
            &cursor_scope,
            &incomplete_context,
        )
        .unwrap();
        assert_eq!(older.snapshot.items.len(), 1);
        assert_eq!(older.snapshot.items[0].key(), expected_keys[0]);
        assert!(older.older_page_cursor.is_none());

        let replay = ChatSessionStore::open_reverse_replay(
            &root,
            session_id,
            MAX_PRESENTATION_TRANSCRIPT_RECORD_BYTES,
        )
        .unwrap();
        let stale = parse_presentation_page_cursor(
            &cursor,
            &presentation_page_cursor_scope(&snapshot.session_id, &DaemonInstanceId::generate()),
            replay.transcript_len(),
        )
        .unwrap_err();
        assert_eq!(stale.code, ApplicationErrorCode::InvalidArguments);
        assert!(stale.message.contains("invalid or stale"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn decoded_byte_budget_produces_wire_safe_pages_without_gaps() {
        let session_id = SessionId::generate();
        let daemon_instance_id = DaemonInstanceId::generate();
        let snapshot = empty_presentation_snapshot(&session_id, &daemon_instance_id);
        let large_content =
            agl_content::Content::text("x".repeat(agl_content::MAX_TEXT_PART_BYTES)).unwrap();
        let items = (0..9)
            .map(|_| SessionPresentationItem::UserMessage {
                message_id: agl_ids::MessageId::generate(),
                content: large_content.clone(),
            })
            .collect::<Vec<_>>();
        let root = write_presentation_transcript(&session_id, &items);
        let cursor_scope = presentation_page_cursor_scope(&session_id, &daemon_instance_id);
        let incomplete_context = empty_incomplete_context();
        let expected_keys = items
            .iter()
            .map(SessionPresentationItem::key)
            .collect::<Vec<_>>();
        let mut pages = Vec::new();
        let mut cursor = None;
        loop {
            let page = paginate_presentation_snapshot(
                snapshot.clone(),
                open_presentation_replay(&root, &session_id, cursor.as_deref(), &cursor_scope),
                &cursor_scope,
                &incomplete_context,
            )
            .unwrap();
            let wire = crate::surface::presentation_snapshot(
                page.snapshot.clone(),
                page.older_page_cursor.clone(),
            )
            .unwrap();
            assert!(wire.canonical_json_bytes().unwrap().len() <= MAX_PRESENTATION_CONTENT_BYTES);
            pages.push(
                page.snapshot
                    .items
                    .iter()
                    .map(SessionPresentationItem::key)
                    .collect::<Vec<_>>(),
            );
            cursor = page.older_page_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert!(pages.len() > 1);
        pages.reverse();
        assert_eq!(
            pages.into_iter().flatten().collect::<Vec<_>>(),
            expected_keys
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incomplete_item_replays_distinctly_and_claim_disables_continue() {
        let session_id = SessionId::generate();
        let run_id = RunId::generate();
        let turn_id = TurnId::generate();
        let message_id = MessageId::generate();
        let attempt_id = agl_ids::AttemptId::generate();
        let event = agl_session::ChatSessionEvent::Runtime {
            envelope: Box::new(agl_events::EventEnvelope {
                schema: agl_events::EVENT_SCHEMA.to_owned(),
                event_id: EventId::generate(),
                sequence: 1,
                occurred_at_unix_ms: 1,
                scope: agl_events::EventScope::builder(run_id.clone())
                    .session_id(session_id)
                    .turn_id(turn_id.clone())
                    .build()
                    .unwrap(),
                request_id: None,
                caused_by: None,
                payload: agl_events::RuntimeEvent::AssistantIncomplete {
                    message_id: message_id.clone(),
                    content: agl_content::Content::text("bounded partial").unwrap(),
                    source_attempt_id: attempt_id.clone(),
                    reason: agl_events::IncompleteOutputReasonEvent::ModelLength,
                    continuation_index: 2,
                    execution_context_revision: 7,
                    runtime_context_revision: 9,
                    policy_hash: "sha256:test-policy".to_owned(),
                },
            }),
        };
        let mut context = IncompleteProjectionContext {
            status: SessionPresentationStatus::Active,
            execution_context_revision: 7,
            runtime_context_revision: 9,
            current_policy_hash: "sha256:test-policy".to_owned(),
            claims: BTreeMap::new(),
            current_context_messages: BTreeSet::from([message_id.clone()]),
        };

        let available = presentation_item_from_transcript_event(event.clone(), &context).unwrap();
        assert!(matches!(
            available,
            SessionPresentationItem::IncompleteAssistant {
                item: IncompleteAssistantItemView {
                    message_id: actual_message_id,
                    source_run_id,
                    source_turn_id,
                    source_attempt_id,
                    continuation_index: 2,
                    continue_action: ContinueActionView::Available,
                    ..
                }
            } if actual_message_id == message_id
                && source_run_id == run_id
                && source_turn_id == turn_id
                && source_attempt_id == attempt_id
        ));

        context.current_policy_hash = "sha256:denied-policy".to_owned();
        let denied = presentation_item_from_transcript_event(event.clone(), &context).unwrap();
        assert!(matches!(
            denied,
            SessionPresentationItem::IncompleteAssistant {
                item: IncompleteAssistantItemView {
                    continue_action: ContinueActionView::Unavailable {
                        reason: ContinueUnavailableReason::PolicyDenied,
                    },
                    ..
                }
            }
        ));
        context.current_policy_hash = "sha256:test-policy".to_owned();

        let continuation_run_id = RunId::generate();
        context.claims.insert(
            message_id.clone(),
            IncompleteContinuationClaim {
                client_submission_id: "stable".to_owned(),
                continuation_run_id: continuation_run_id.clone(),
                continuation_turn_id: TurnId::generate(),
                continuation_request_id: RequestId::generate(),
            },
        );
        let claimed = presentation_item_from_transcript_event(event, &context).unwrap();
        assert!(matches!(
            claimed,
            SessionPresentationItem::IncompleteAssistant {
                item: IncompleteAssistantItemView {
                    continue_action: ContinueActionView::Claimed {
                        continuation_run_id: actual
                    },
                    ..
                }
            } if actual == continuation_run_id
        ));
    }

    fn write_presentation_transcript(
        session_id: &SessionId,
        items: &[SessionPresentationItem],
    ) -> PathBuf {
        use std::io::Write as _;

        let root = std::env::temp_dir().join(format!(
            "agl-daemon-presentation-paging-{}",
            EventId::generate()
        ));
        let session_dir = root.join(session_id.as_str());
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("session.json"), b"{}").unwrap();
        let mut transcript = std::fs::File::create(session_dir.join("transcript.jsonl")).unwrap();
        let run_id = RunId::generate();
        let turn_id = TurnId::generate();
        for (index, item) in items.iter().enumerate() {
            let payload = match item {
                SessionPresentationItem::UserMessage {
                    message_id,
                    content,
                } => agl_events::RuntimeEvent::UserMessage {
                    message_id: message_id.clone(),
                    content: content.clone(),
                },
                SessionPresentationItem::AssistantMessage {
                    message_id,
                    content,
                    ..
                } => agl_events::RuntimeEvent::AssistantMessage {
                    message_id: message_id.clone(),
                    content: content.clone(),
                },
                other => panic!("test transcript item is not a message: {other:?}"),
            };
            let event = agl_session::ChatSessionEvent::Runtime {
                envelope: Box::new(agl_events::EventEnvelope {
                    schema: agl_events::EVENT_SCHEMA.to_owned(),
                    event_id: EventId::generate(),
                    sequence: u64::try_from(index + 1).unwrap(),
                    occurred_at_unix_ms: u64::try_from(index + 1).unwrap(),
                    scope: agl_events::EventScope::builder(run_id.clone())
                        .session_id(session_id.clone())
                        .turn_id(turn_id.clone())
                        .build()
                        .unwrap(),
                    request_id: None,
                    caused_by: None,
                    payload,
                }),
            };
            serde_json::to_writer(&mut transcript, &event).unwrap();
            transcript.write_all(b"\n").unwrap();
        }
        transcript.flush().unwrap();
        root
    }

    fn open_presentation_replay(
        root: &Path,
        session_id: &SessionId,
        cursor: Option<&str>,
        cursor_scope: &str,
    ) -> ChatSessionReverseReader {
        let mut replay = ChatSessionStore::open_reverse_replay(
            root,
            session_id.clone(),
            MAX_PRESENTATION_TRANSCRIPT_RECORD_BYTES,
        )
        .unwrap();
        if let Some(cursor) = cursor {
            let end_offset =
                parse_presentation_page_cursor(cursor, cursor_scope, replay.transcript_len())
                    .unwrap();
            replay.set_end_offset(end_offset).unwrap();
        }
        replay
    }

    fn message_item(text: String) -> SessionPresentationItem {
        SessionPresentationItem::UserMessage {
            message_id: agl_ids::MessageId::generate(),
            content: agl_content::Content::text(text).unwrap(),
        }
    }

    fn empty_incomplete_context() -> IncompleteProjectionContext {
        IncompleteProjectionContext {
            status: SessionPresentationStatus::Active,
            execution_context_revision: 1,
            runtime_context_revision: 1,
            current_policy_hash: "sha256:test-policy".to_owned(),
            claims: BTreeMap::new(),
            current_context_messages: BTreeSet::new(),
        }
    }

    fn empty_presentation_snapshot(
        session_id: &SessionId,
        daemon_instance_id: &DaemonInstanceId,
    ) -> SessionPresentationSnapshot {
        SessionPresentationSnapshot {
            session_id: session_id.clone(),
            cursor: PresentationCursor {
                daemon_instance_id: daemon_instance_id.clone(),
                revision: 0,
            },
            header: SessionHeader {
                session_id: session_id.clone(),
                status: SessionPresentationStatus::Active,
                durable: true,
                resumed: false,
                title: None,
                function_name: "agentLIBRE".to_owned(),
                model_id: None,
                operation_mode: ChatToolMode::ReadOnly,
                selected_skills: Vec::new(),
                runtime_context_revision: 0,
                workspace_root: SanitizedDisplayPath::from_utf8("/workspace"),
                workspace_history_scope: workspace_history_scope(Path::new("/workspace")),
                cwd: SanitizedDisplayPath::from_utf8("/workspace"),
                execution_context_revision: 0,
                context_used_tokens: None,
                context_limit_tokens: None,
                active_run_count: 0,
                queued_prompt_count: 0,
                active_execution_count: 0,
            },
            items: Vec::new(),
            active_run: None,
            queued_prompts: Vec::new(),
            terminals: Vec::new(),
            executions: Vec::new(),
            activity: None,
            command_context: CommandContext {
                session_id: Some(session_id.clone()),
                session_active: true,
                ..CommandContext::default()
            },
        }
    }
}

fn exit_cancellation_rank(state: RunState) -> u8 {
    match state {
        RunState::Queued | RunState::Waiting => 0,
        RunState::Running => 1,
        RunState::Succeeded | RunState::Incomplete | RunState::Failed | RunState::Cancelled => 2,
    }
}

fn terminal_application_error(error: ProcessError) -> ApplicationError {
    let code = match error.code() {
        ProcessErrorCode::InvalidRequest
        | ProcessErrorCode::InvalidBytes
        | ProcessErrorCode::InputTooLarge
        | ProcessErrorCode::InvalidTerminalSize
        | ProcessErrorCode::IoModeMismatch
        | ProcessErrorCode::InputLeaseExpired
        | ProcessErrorCode::ExecutionNotLive => ApplicationErrorCode::InvalidArguments,
        ProcessErrorCode::ExecutionNotFound | ProcessErrorCode::OutputExpired => {
            ApplicationErrorCode::NotFound
        }
        ProcessErrorCode::ExecutionNotOwned => ApplicationErrorCode::TerminalOwnerMismatch,
        ProcessErrorCode::HostAuthorityRequired
        | ProcessErrorCode::LoginAuthorityRequired
        | ProcessErrorCode::GrantRevoked
        | ProcessErrorCode::GrantExpired => ApplicationErrorCode::AuthorizationRequired,
        ProcessErrorCode::PlatformUnsupported
        | ProcessErrorCode::LauncherUnavailable
        | ProcessErrorCode::SandboxUnavailable
        | ProcessErrorCode::SandboxExecutableUnavailable => {
            ApplicationErrorCode::CommandUnavailable
        }
        ProcessErrorCode::ActiveLimitReached | ProcessErrorCode::InputBackpressure => {
            ApplicationErrorCode::InputBackpressure
        }
        ProcessErrorCode::InputLeaseBusy => ApplicationErrorCode::WriterLeaseBusy,
        ProcessErrorCode::StateConflict if error.message().contains("outcome_unknown") => {
            ApplicationErrorCode::OutcomeUnknown
        }
        ProcessErrorCode::StateConflict => ApplicationErrorCode::InvalidArguments,
        ProcessErrorCode::Cancelled
        | ProcessErrorCode::TimedOut
        | ProcessErrorCode::LauncherProtocol
        | ProcessErrorCode::SpawnFailed
        | ProcessErrorCode::OutputLimitExceeded
        | ProcessErrorCode::SupervisorShutdown
        | ProcessErrorCode::StoreCorrupt
        | ProcessErrorCode::Internal => ApplicationErrorCode::Internal,
    };
    ApplicationError::new(code, error.message().to_owned())
}

fn protocol_application_error(error: ApplicationError) -> ProtocolError {
    crate::surface::protocol_error(error)
}

fn protocol_tool_mode_from_app(mode: ChatToolMode) -> ProtocolToolMode {
    match mode {
        ChatToolMode::ReadOnly => ProtocolToolMode::ReadOnly,
        ChatToolMode::Write => ProtocolToolMode::Write,
        ChatToolMode::Execute => ProtocolToolMode::Execute,
        ChatToolMode::Approve => ProtocolToolMode::Approve,
        ChatToolMode::Admin => ProtocolToolMode::Admin,
    }
}

fn application_protocol_error(error: ProtocolError) -> ApplicationError {
    let code = match error.code {
        ProtocolErrorCode::InvalidRequest | ProtocolErrorCode::InvalidArguments => {
            ApplicationErrorCode::InvalidArguments
        }
        ProtocolErrorCode::NotFound => ApplicationErrorCode::NotFound,
        ProtocolErrorCode::Unauthorized | ProtocolErrorCode::NotAuthorized => {
            ApplicationErrorCode::NotAuthorized
        }
        ProtocolErrorCode::Busy | ProtocolErrorCode::InputBackpressure => {
            ApplicationErrorCode::InputBackpressure
        }
        ProtocolErrorCode::Unsupported | ProtocolErrorCode::UnsupportedProtocolVersion => {
            ApplicationErrorCode::CommandUnavailable
        }
        ProtocolErrorCode::RuntimeIdentityMismatch => ApplicationErrorCode::CommandUnavailable,
        ProtocolErrorCode::RuntimeFailure | ProtocolErrorCode::Internal => {
            ApplicationErrorCode::Internal
        }
        ProtocolErrorCode::CommandUnavailable => ApplicationErrorCode::CommandUnavailable,
        ProtocolErrorCode::SessionBusy => ApplicationErrorCode::SessionBusy,
        ProtocolErrorCode::AuthorizationRequired => ApplicationErrorCode::AuthorizationRequired,
        ProtocolErrorCode::ConfirmationRequired => ApplicationErrorCode::ConfirmationRequired,
        ProtocolErrorCode::StaleContextRevision => ApplicationErrorCode::StaleContextRevision,
        ProtocolErrorCode::TerminalOwnerMismatch => ApplicationErrorCode::TerminalOwnerMismatch,
        ProtocolErrorCode::WriterLeaseBusy => ApplicationErrorCode::WriterLeaseBusy,
        ProtocolErrorCode::ModelNotInstalled => ApplicationErrorCode::ModelNotInstalled,
        ProtocolErrorCode::ModelContextTooSmall => ApplicationErrorCode::ModelContextTooSmall,
        ProtocolErrorCode::SkillNotAdmitted => ApplicationErrorCode::SkillNotAdmitted,
        ProtocolErrorCode::IncompleteOutputNotFound => {
            ApplicationErrorCode::IncompleteOutputNotFound
        }
        ProtocolErrorCode::ContinuationAlreadyClaimed => {
            ApplicationErrorCode::ContinuationAlreadyClaimed
        }
        ProtocolErrorCode::StaleContinuationContext => {
            ApplicationErrorCode::StaleContinuationContext
        }
        ProtocolErrorCode::ActivityCapacityExceeded => {
            ApplicationErrorCode::ActivityCapacityExceeded
        }
        ProtocolErrorCode::ResyncRequired => ApplicationErrorCode::ResyncRequired,
        ProtocolErrorCode::OutcomeUnknown => ApplicationErrorCode::OutcomeUnknown,
    };
    ApplicationError::new(code, error.message)
}

fn protocol_runtime_identity(identity: &CurrentRuntimeIdentity) -> RuntimeGenerationIdentity {
    RuntimeGenerationIdentity {
        kind: match identity.kind {
            RuntimeIdentityKind::Sealed => RuntimeGenerationKind::Sealed,
            RuntimeIdentityKind::Development => RuntimeGenerationKind::Development,
        },
        generation_id: identity.generation_id.clone(),
        builtin_catalog_digest: identity.builtin_catalog_digest.clone(),
        executable_digest: identity.executable_digest.clone(),
    }
}

fn application_runtime_error(error: impl std::fmt::Display) -> ApplicationError {
    ApplicationError::new(ApplicationErrorCode::Internal, error.to_string())
}

fn application_busy_error(error: anyhow::Error) -> ApplicationError {
    if error.to_string().contains("busy or not registered") {
        ApplicationError::new(ApplicationErrorCode::SessionBusy, "session is busy")
    } else {
        application_runtime_error(error)
    }
}

fn application_workspace_preflight_error(error: anyhow::Error) -> ApplicationError {
    if error.to_string().contains("busy or not registered") {
        ApplicationError::new(ApplicationErrorCode::SessionBusy, "session is busy")
    } else {
        ApplicationError::new(ApplicationErrorCode::InvalidArguments, error.to_string())
    }
}

fn application_model_error(error: anyhow::Error) -> ApplicationError {
    let message = format!("{error:#}");
    let code = if message.contains("not installed") || message.contains("installed file") {
        ApplicationErrorCode::ModelNotInstalled
    } else if message.contains("context limit") || message.contains("retained conversation") {
        ApplicationErrorCode::ModelContextTooSmall
    } else if message.contains("busy or not registered") {
        ApplicationErrorCode::SessionBusy
    } else {
        ApplicationErrorCode::Internal
    };
    ApplicationError::new(code, message)
}

fn application_mode_error(error: anyhow::Error) -> ApplicationError {
    let message = format!("{error:#}");
    let code = if message.contains("busy or not registered") {
        ApplicationErrorCode::SessionBusy
    } else if message.contains("not admitted") || message.contains("denied") {
        ApplicationErrorCode::NotAuthorized
    } else {
        ApplicationErrorCode::Internal
    };
    ApplicationError::new(code, message)
}

fn application_skill_error(error: anyhow::Error) -> ApplicationError {
    let message = format!("{error:#}");
    let code = if message.contains("busy or not registered") {
        ApplicationErrorCode::SessionBusy
    } else {
        ApplicationErrorCode::SkillNotAdmitted
    };
    ApplicationError::new(code, message)
}

fn ensure_application_call_live(context: &ApplicationCallContext) -> Result<(), ApplicationError> {
    if context.is_cancelled() {
        return Err(ApplicationError::new(
            ApplicationErrorCode::OutcomeUnknown,
            "application call was cancelled before a typed outcome was reached",
        ));
    }
    Ok(())
}

type DaemonStateOperation = Box<dyn FnOnce(&mut DaemonState) + Send + 'static>;

pub(crate) struct DaemonStateExecutor {
    operations: SyncSender<DaemonStateOperation>,
    pending_operations: Arc<AtomicUsize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DaemonStateCallError {
    Full,
    Cancelled,
    Closed,
}

impl DaemonStateExecutor {
    fn spawn(mut state: DaemonState) -> Result<Arc<Self>> {
        let (operations, receiver) = sync_channel(DAEMON_STATE_QUEUE_CAPACITY);
        let pending_operations = Arc::new(AtomicUsize::new(0));
        let worker_pending_operations = Arc::clone(&pending_operations);
        std::thread::Builder::new()
            .name("agl-daemon-state".to_owned())
            .spawn(move || {
                run_daemon_state_executor(&mut state, receiver, worker_pending_operations)
            })
            .context("failed to start bounded daemon state executor")?;
        Ok(Arc::new(Self {
            operations,
            pending_operations,
        }))
    }

    pub(crate) fn call<T: Send + 'static>(
        &self,
        context: ApplicationCallContext,
        operation: impl FnOnce(&mut DaemonState, &ApplicationCallContext) -> T + Send + 'static,
    ) -> Result<T, DaemonStateCallError> {
        if context.is_cancelled() {
            return Err(DaemonStateCallError::Cancelled);
        }
        let (reply, response) = sync_channel(1);
        let state_operation: DaemonStateOperation = Box::new(move |state| {
            if context.is_cancelled() {
                let _ = reply.send(Err(DaemonStateCallError::Cancelled));
                return;
            }
            let result = operation(state, &context);
            let _ = reply.send(Ok(result));
        });
        self.pending_operations.fetch_add(1, Ordering::AcqRel);
        match self.operations.try_send(state_operation) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.pending_operations.fetch_sub(1, Ordering::AcqRel);
                return Err(DaemonStateCallError::Full);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.pending_operations.fetch_sub(1, Ordering::AcqRel);
                return Err(DaemonStateCallError::Closed);
            }
        }
        response.recv().unwrap_or(Err(DaemonStateCallError::Closed))
    }

    #[cfg(test)]
    pub(crate) fn pending_operations(&self) -> usize {
        self.pending_operations.load(Ordering::Acquire)
    }

    pub(crate) fn invoke_application(
        &self,
        context: ApplicationCallContext,
        request: ApplicationActionRequest,
    ) -> Result<ApplicationToolResult, ApplicationError> {
        let ApplicationAction::SessionExit { confirm_active } = &request.action else {
            return self
                .call(context, move |state, context| {
                    state.application_invoke_with_context(request, context)
                })
                .map_err(daemon_state_application_error)?;
        };
        let confirm_active = *confirm_active;
        let session_id = request.session_id.ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "application action requires a current session",
            )
        })?;
        let plan = self
            .call(context.clone(), move |state, context| {
                state.begin_session_exit(
                    session_id,
                    agl_protocol::SessionFinishReason::ExitCommand,
                    confirm_active,
                    context,
                )
            })
            .map_err(daemon_state_application_error)??;
        let outcome = plan.wait(&context)?;
        let completed_session_id = plan.session_id.clone();
        self.call(context.clone(), move |state, context| {
            state.complete_session_exit(plan, context)
        })
        .map_err(daemon_state_application_error)??;
        Ok(ApplicationToolResult::SessionExited {
            session_id: completed_session_id,
            cancelled_runs: outcome.cancelled_runs,
            terminated_terminals: outcome.terminated.terminals,
            terminated_executions: outcome.terminated.executions,
        })
    }

    fn finish_session(
        &self,
        context: ApplicationCallContext,
        session_id: SessionId,
        reason: agl_protocol::SessionFinishReason,
    ) -> Result<DaemonEventKind, ApplicationError> {
        let plan = self
            .call(context.clone(), move |state, context| {
                state.begin_session_exit(session_id, reason, true, context)
            })
            .map_err(daemon_state_application_error)??;
        plan.wait(&context)?;
        self.call(context, move |state, context| {
            state.complete_session_exit(plan, context)
        })
        .map_err(daemon_state_application_error)?
    }

    fn cancel_active_session_work(
        &self,
        context: ApplicationCallContext,
        session_id: SessionId,
    ) -> Result<DaemonEventKind, ApplicationError> {
        let plan = self
            .call(context.clone(), move |state, context| {
                state.begin_active_work_cancel(session_id, context)
            })
            .map_err(daemon_state_application_error)??;
        let outcome = plan.wait(&context)?;
        self.call(context, move |state, context| {
            state.complete_active_work_cancel(plan, outcome, context)
        })
        .map_err(daemon_state_application_error)?
    }
}

fn run_daemon_state_executor(
    state: &mut DaemonState,
    operations: Receiver<DaemonStateOperation>,
    pending_operations: Arc<AtomicUsize>,
) {
    while let Ok(operation) = operations.recv() {
        pending_operations.fetch_sub(1, Ordering::AcqRel);
        operation(state);
    }
}

#[derive(Clone)]
pub struct SharedDaemonState {
    pub(crate) inner: Arc<DaemonStateExecutor>,
    application: agl_app::ApplicationService,
    blocking_bridge: Arc<tokio::sync::Semaphore>,
    inference_client: InferenceClientHandle,
    supervisor_handle: SupervisorHandle,
}

impl SharedDaemonState {
    pub fn new(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
        inference_status: EngineRuntimeStatusHandle,
    ) -> Self {
        Self::from_state(DaemonState::new(
            runtime,
            inference_defaults,
            inference_client,
            inference_status,
        ))
        .expect("test daemon state executor should initialize")
    }

    pub fn open(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
        inference_status: EngineRuntimeStatusHandle,
    ) -> Result<Self> {
        Self::from_state(DaemonState::open(
            runtime,
            inference_defaults,
            inference_client,
            inference_status,
        )?)
    }

    #[doc(hidden)]
    pub fn open_with_runtime_identity(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
        inference_status: EngineRuntimeStatusHandle,
        runtime_identity: CurrentRuntimeIdentity,
    ) -> Result<Self> {
        Self::from_state(DaemonState::open_with_runtime_identity(
            runtime,
            inference_defaults,
            inference_client,
            inference_status,
            runtime_identity,
        )?)
    }

    fn from_state(state: DaemonState) -> Result<Self> {
        let daemon_instance_id = state.daemon_instance_id.clone();
        let presentation_proxy = state.presentation_proxy.clone();
        let inference_client = state.inference_client.clone();
        let supervisor_handle = state.supervisor_handle();
        let inner = DaemonStateExecutor::spawn(state)?;
        let application =
            crate::surface::application_service(daemon_instance_id, Arc::downgrade(&inner));
        presentation_proxy
            .connect(application.clone())
            .expect("daemon turn presentation proxy must connect exactly once");
        Ok(Self {
            inner,
            application,
            blocking_bridge: Arc::new(tokio::sync::Semaphore::new(DAEMON_STATE_QUEUE_CAPACITY)),
            inference_client,
            supervisor_handle,
        })
    }

    pub(crate) fn application(&self) -> agl_app::ApplicationService {
        self.application.clone()
    }

    #[cfg(test)]
    pub(crate) fn available_blocking_permits(&self) -> usize {
        self.blocking_bridge.available_permits()
    }

    pub fn handle_request(&self, request: DaemonRequest) -> DaemonEvent {
        self.handle_request_with_context(request, ApplicationCallContext::new())
    }

    pub(crate) async fn handle_request_async(&self, request: DaemonRequest) -> DaemonEvent {
        let request_id = request.request_id.clone();
        let permit = match Arc::clone(&self.blocking_bridge).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return daemon_state_error_event(request_id, DaemonStateCallError::Full),
        };
        let context = ApplicationCallContext::new();
        let cancellation = DaemonCallCancelOnDrop {
            context: context.clone(),
            armed: true,
        };
        let state = self.clone();
        let task = tokio::task::spawn_blocking(move || {
            // Keep capacity charged to the blocking closure after its async
            // awaiter is aborted on connection close.
            let _permit = permit;
            state.handle_request_with_context(request, context)
        });
        let event = match task.await {
            Ok(event) => event,
            Err(_) => DaemonEvent::new(
                Some(request_id),
                DaemonEventKind::Error(ProtocolError::new(
                    ProtocolErrorCode::RuntimeFailure,
                    "daemon request task failed",
                    false,
                )),
            ),
        };
        cancellation.disarm();
        event
    }

    fn handle_request_with_context(
        &self,
        request: DaemonRequest,
        context: ApplicationCallContext,
    ) -> DaemonEvent {
        let request_id = request.request_id.clone();
        if let DaemonRequestKind::SessionFinish(finish) = &request.kind {
            let result =
                self.inner
                    .finish_session(context, finish.session_id.clone(), finish.reason);
            return DaemonEvent::new(
                Some(request_id),
                result.unwrap_or_else(|error| {
                    DaemonEventKind::Error(protocol_application_error(error))
                }),
            );
        }
        if let DaemonRequestKind::SessionCancelActive(cancel) = &request.kind {
            let result = self
                .inner
                .cancel_active_session_work(context, cancel.session_id.clone());
            return DaemonEvent::new(
                Some(request_id),
                result.unwrap_or_else(|error| {
                    DaemonEventKind::Error(protocol_application_error(error))
                }),
            );
        }
        self.inner
            .call(context, move |state, context| {
                state.handle_request_with_context(request, context)
            })
            .unwrap_or_else(|error| daemon_state_error_event(request_id, error))
    }

    pub fn subscribe_run(
        &self,
        run_id: RunId,
        after_sequence: u64,
    ) -> Result<RunSubscription, ProtocolError> {
        self.supervisor_handle
            .subscribe(run_id, after_sequence)
            .map_err(supervisor_error)
    }

    pub fn run_outcome(&self, run_id: RunId) -> Result<RunOutcome, ProtocolError> {
        self.supervisor_handle
            .outcome(run_id.clone())
            .map_err(supervisor_error)?
            .ok_or_else(|| not_found(run_id.as_str()))
    }

    pub fn model_manager_status(&self) -> Result<ModelManagerStatus> {
        self.inference_client.status()
    }

    pub fn supervisor_handle(&self) -> Result<SupervisorHandle> {
        Ok(self.supervisor_handle.clone())
    }

    pub(crate) async fn operator_ensure_human_host_terminal(
        &self,
        request: HumanTerminalEnsure,
        operator_uid: u32,
        confirm_host_authority: bool,
    ) -> Result<TerminalEnsured, ApplicationError> {
        let permit = Arc::clone(&self.blocking_bridge)
            .try_acquire_owned()
            .map_err(|_| daemon_state_application_error(DaemonStateCallError::Full))?;
        let context = ApplicationCallContext::new();
        let cancellation = DaemonCallCancelOnDrop {
            context: context.clone(),
            armed: true,
        };
        let state = Arc::clone(&self.inner);
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            state
                .call(context, move |state, _| {
                    state.operator_ensure_human_host_terminal(
                        request,
                        operator_uid,
                        confirm_host_authority,
                    )
                })
                .map_err(daemon_state_application_error)
                .and_then(|result| result)
        });
        let result = task.await.map_err(|_| {
            ApplicationError::new(
                ApplicationErrorCode::Internal,
                "local-operator terminal admission task failed",
            )
        })?;
        cancellation.disarm();
        result
    }

    pub fn submit_cron_job(
        &self,
        job: &CronJob,
        scheduled_for: &str,
    ) -> Result<RunAccepted, ProtocolError> {
        let job = job.clone();
        let scheduled_for = scheduled_for.to_owned();
        self.inner
            .call(ApplicationCallContext::new(), move |state, _| {
                state.submit_cron_job(&job, &scheduled_for)
            })
            .map_err(daemon_state_protocol_error)?
    }
}

struct DaemonCallCancelOnDrop {
    context: ApplicationCallContext,
    armed: bool,
}

impl DaemonCallCancelOnDrop {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for DaemonCallCancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.context.cancel();
        }
    }
}

fn daemon_state_error_message(error: DaemonStateCallError) -> &'static str {
    match error {
        DaemonStateCallError::Full => "bounded daemon application queue is full",
        DaemonStateCallError::Cancelled => "daemon application call was cancelled",
        DaemonStateCallError::Closed => "daemon application owner is shutting down",
    }
}

pub(crate) fn daemon_state_application_error(error: DaemonStateCallError) -> ApplicationError {
    let code = match error {
        DaemonStateCallError::Full => ApplicationErrorCode::InputBackpressure,
        DaemonStateCallError::Cancelled | DaemonStateCallError::Closed => {
            ApplicationErrorCode::OutcomeUnknown
        }
    };
    ApplicationError::new(code, daemon_state_error_message(error))
}

fn daemon_state_protocol_error(error: DaemonStateCallError) -> ProtocolError {
    let code = match error {
        DaemonStateCallError::Full => ProtocolErrorCode::InputBackpressure,
        DaemonStateCallError::Cancelled | DaemonStateCallError::Closed => {
            ProtocolErrorCode::OutcomeUnknown
        }
    };
    ProtocolError::new(
        code,
        daemon_state_error_message(error),
        error == DaemonStateCallError::Full,
    )
}

fn daemon_state_error_event(request_id: RequestId, error: DaemonStateCallError) -> DaemonEvent {
    DaemonEvent::new(
        Some(request_id),
        DaemonEventKind::Error(daemon_state_protocol_error(error)),
    )
}

pub(crate) fn run_status_event(outcome: RunOutcome) -> RunStatusEvent {
    let status = outcome.status;
    let expose_terminal_content = status.kind != RunKind::Subagent;
    RunStatusEvent {
        session_id: status.session_id,
        run_id: status.run_id,
        turn_id: status.turn_id,
        run_kind: protocol_run_kind(status.kind),
        state: protocol_run_state(status.state),
        concurrency_key: status
            .concurrency_key
            .as_ref()
            .map(|key| key.as_str().to_owned()),
        usage: RunUsageEvent {
            wall_time_ms: status.usage.wall_time_ms,
            model_input_tokens: status.usage.model_input_tokens,
            model_output_tokens: status.usage.model_output_tokens,
            model_attempts: status.usage.model_attempts,
            tool_calls: status.usage.tool_calls,
        },
        cancellation_requested: status.cancellation_requested,
        attempts: status.attempts,
        created_at_ms: status.created_at_ms,
        updated_at_ms: status.updated_at_ms,
        started_at_ms: status.started_at_ms,
        finished_at_ms: status.finished_at_ms,
        error_code: status.error_code,
        terminal_result: expose_terminal_content
            .then_some(outcome.terminal_result)
            .flatten(),
        error_message: expose_terminal_content
            .then_some(outcome.error_message)
            .flatten(),
        parent_run_id: status.parent_run_id,
        root_run_id: status.root_run_id,
        depth: status.depth,
        subagent_id: status.subagent_id,
        spawned_by_step_id: status.spawned_by_step_id,
        child_spec_digest: status.child_spec_digest,
        model_profile_digest: status.model_profile_digest,
        result_delivered: status.result_delivered,
    }
}

fn run_tree_node(status: SafeRunStatus) -> RunTreeNodeEvent {
    RunTreeNodeEvent {
        session_id: status.session_id,
        run_id: status.run_id,
        turn_id: status.turn_id,
        run_kind: protocol_run_kind(status.kind),
        state: protocol_run_state(status.state),
        concurrency_key: status
            .concurrency_key
            .as_ref()
            .map(|key| key.as_str().to_owned()),
        usage: RunUsageEvent {
            wall_time_ms: status.usage.wall_time_ms,
            model_input_tokens: status.usage.model_input_tokens,
            model_output_tokens: status.usage.model_output_tokens,
            model_attempts: status.usage.model_attempts,
            tool_calls: status.usage.tool_calls,
        },
        cancellation_requested: status.cancellation_requested,
        attempts: status.attempts,
        created_at_ms: status.created_at_ms,
        updated_at_ms: status.updated_at_ms,
        started_at_ms: status.started_at_ms,
        finished_at_ms: status.finished_at_ms,
        error_code: status.error_code,
        parent_run_id: status.parent_run_id,
        root_run_id: status.root_run_id,
        depth: status.depth,
        subagent_id: status.subagent_id,
        spawned_by_step_id: status.spawned_by_step_id,
        child_spec_digest: status.child_spec_digest,
        model_profile_digest: status.model_profile_digest,
        result_delivered: status.result_delivered,
    }
}

fn protocol_run_kind(kind: RunKind) -> ProtocolRunKind {
    match kind {
        RunKind::Turn => ProtocolRunKind::Turn,
        RunKind::Cron => ProtocolRunKind::Cron,
        RunKind::Subagent => ProtocolRunKind::Subagent,
    }
}

pub(crate) fn protocol_run_state(state: RunState) -> ProtocolRunState {
    match state {
        RunState::Queued => ProtocolRunState::Queued,
        RunState::Running => ProtocolRunState::Running,
        RunState::Waiting => ProtocolRunState::Waiting,
        RunState::Succeeded => ProtocolRunState::Succeeded,
        RunState::Incomplete => ProtocolRunState::Incomplete,
        RunState::Failed => ProtocolRunState::Failed,
        RunState::Cancelled => ProtocolRunState::Cancelled,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RootActivityReservation {
    active_nodes: usize,
    active_bytes: usize,
    deepest_path_nodes: usize,
}

fn root_activity_reservation(
    delegation_plan: Option<&RuntimeDelegationPlan>,
) -> Option<RootActivityReservation> {
    let (max_descendants, max_depth) = match delegation_plan {
        None => (0_usize, 0_usize),
        Some(plan) => (
            usize::try_from(plan.budget.max_descendants).ok()?,
            usize::try_from(plan.budget.max_depth).ok()?,
        ),
    };
    let reachable_depth = max_depth.min(max_descendants);
    let active_nodes = max_descendants
        .checked_mul(DESCENDANT_ACTIVE_ACTIVITY_NODES)?
        .checked_add(ROOT_ACTIVE_ACTIVITY_NODES)?;
    let deepest_path_nodes = reachable_depth
        .checked_mul(DESCENDANT_ACTIVITY_PATH_NODES)?
        .checked_add(ROOT_ACTIVITY_PATH_NODES)?;
    let active_bytes = active_nodes
        .checked_mul(MAX_ACTIVITY_NODE_BYTES)?
        .checked_add(
            deepest_path_nodes.checked_mul(MAX_RESERVED_ACTIVITY_NODE_ID_BYTES.checked_add(3)?)?,
        )?
        .checked_add(ACTIVE_ACTIVITY_ENCODING_OVERHEAD_BYTES)?;
    (active_nodes <= MAX_ACTIVE_ACTIVITY_NODES
        && active_bytes <= MAX_ACTIVE_ACTIVITY_BYTES
        && deepest_path_nodes <= MAX_ACTIVITY_PATH_NODES)
        .then_some(RootActivityReservation {
            active_nodes,
            active_bytes,
            deepest_path_nodes,
        })
}

fn validate_root_activity_capacity_protocol(
    delegation_plan: Option<&RuntimeDelegationPlan>,
) -> Result<(), ProtocolError> {
    root_activity_reservation(delegation_plan)
        .map(|_| ())
        .ok_or_else(|| {
            ProtocolError::new(
                ProtocolErrorCode::ActivityCapacityExceeded,
                "activity_capacity_exceeded: immutable run/delegation budget cannot fit the active activity projection",
                false,
            )
        })
}

fn validate_root_activity_capacity_application(
    delegation_plan: Option<&RuntimeDelegationPlan>,
) -> Result<(), ApplicationError> {
    root_activity_reservation(delegation_plan)
        .map(|_| ())
        .ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::ActivityCapacityExceeded,
                "immutable run/delegation budget cannot fit the active activity projection",
            )
        })
}

#[cfg(test)]
mod root_activity_capacity_tests {
    use super::*;

    fn delegation_plan(max_depth: u32, max_descendants: u32) -> RuntimeDelegationPlan {
        RuntimeDelegationPlan {
            budget: agl_function::FunctionDelegationBudget {
                max_depth,
                max_children_per_run: 64,
                max_descendants,
                max_total_output_tokens: 1,
                timeout_seconds: 1,
            },
            root_subagents: Vec::new(),
            subagent_specs: BTreeMap::new(),
        }
    }

    #[test]
    fn root_without_delegation_reserves_one_full_active_path() {
        let reservation = root_activity_reservation(None).unwrap();

        assert_eq!(reservation.active_nodes, ROOT_ACTIVE_ACTIVITY_NODES);
        assert_eq!(reservation.deepest_path_nodes, ROOT_ACTIVITY_PATH_NODES);
        assert!(reservation.active_bytes <= MAX_ACTIVE_ACTIVITY_BYTES);
    }

    #[test]
    fn reachable_depth_not_unreachable_manifest_depth_controls_the_path() {
        let plan = delegation_plan(16, 4);
        let reservation = root_activity_reservation(Some(&plan)).unwrap();

        assert_eq!(reservation.deepest_path_nodes, 16);
    }

    #[test]
    fn boundary_budget_fits_but_deeper_or_wider_topology_fails_closed() {
        let boundary = delegation_plan(9, 49);
        let reservation = root_activity_reservation(Some(&boundary)).unwrap();
        assert_eq!(reservation.active_nodes, 249);
        assert_eq!(reservation.deepest_path_nodes, 31);

        let too_deep = delegation_plan(10, 49);
        assert!(root_activity_reservation(Some(&too_deep)).is_none());

        let too_wide = delegation_plan(9, 50);
        assert!(root_activity_reservation(Some(&too_wide)).is_none());
        let error = validate_root_activity_capacity_protocol(Some(&too_wide)).unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::ActivityCapacityExceeded);
        assert!(!error.retryable);
    }
}

fn run_fingerprint(session_id: &SessionId, content: &agl_content::Content) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentlibre.daemon.run_submit.v2\0");
    hasher.update(session_id.as_str().as_bytes());
    hasher.update(b"\0");
    hasher
        .update(serde_json::to_vec(content).expect("validated content always serializes to JSON"));
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn cron_fingerprint(job: &CronJob, scheduled_for: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentlibre.daemon.cron_run.v1\0");
    hasher.update(job.id.as_bytes());
    hasher.update(b"\0");
    hasher.update(scheduled_for.as_bytes());
    hasher.update(b"\0");
    hasher.update(job.target_kind.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(job.target_ref.as_bytes());
    hasher.update(b"\0");
    hasher.update(job.prompt.as_deref().unwrap_or_default().as_bytes());
    hasher.update(b"\0");
    hasher.update(job.input.as_deref().unwrap_or_default().as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn chat_tool_mode(mode: ProtocolToolMode) -> ChatToolMode {
    match mode {
        ProtocolToolMode::ReadOnly => ChatToolMode::ReadOnly,
        ProtocolToolMode::Write => ChatToolMode::Write,
        ProtocolToolMode::Execute => ChatToolMode::Execute,
        ProtocolToolMode::Approve => ChatToolMode::Approve,
        ProtocolToolMode::Admin => ChatToolMode::Admin,
    }
}

fn protocol_tool_mode(value: &str) -> Result<ProtocolToolMode, ProtocolError> {
    match value {
        "read-only" => Ok(ProtocolToolMode::ReadOnly),
        "write" => Ok(ProtocolToolMode::Write),
        "execute" => Ok(ProtocolToolMode::Execute),
        "approve" => Ok(ProtocolToolMode::Approve),
        "admin" => Ok(ProtocolToolMode::Admin),
        _ => Err(invalid(format!(
            "invalid persisted operation mode `{value}`"
        ))),
    }
}

fn invalid(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}

fn not_found(resource: &str) -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::NotFound,
        format!("{resource} not found"),
        false,
    )
}

fn runtime_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::RuntimeFailure, error.to_string(), false)
}

fn agent_execution_filter(
    session_id: Option<&SessionId>,
    root_run_id: Option<&RunId>,
    include_finished: bool,
) -> ExecutionListFilter {
    let owner = session_id.map(|session_id| {
        ExecutionOwner::new(
            CallerOwner::new(
                CallerNamespace::new("agentlibre", 1).expect("static caller namespace is valid"),
                OpaqueOwnerId::new(session_id.as_str())
                    .expect("canonical session ID fits opaque owner contract"),
                CallerOwnerKind::Persistent,
                CallerRole::Human,
            ),
            OpaqueOwnerId::new(session_id.as_str())
                .expect("canonical session ID fits opaque authority contract"),
        )
    });
    let authority_scope = root_run_id.map(|run_id| {
        OpaqueOwnerId::new(run_id.as_str())
            .expect("canonical run ID fits opaque authority contract")
    });
    ExecutionListFilter {
        owner,
        authority_scope,
        include_finished,
    }
}

fn agent_caller_namespace() -> CallerNamespace {
    CallerNamespace::new("agentlibre", 1).expect("static caller namespace is valid")
}

fn opaque_agent_id(value: &str) -> OpaqueOwnerId {
    OpaqueOwnerId::new(value).expect("canonical agent ID fits opaque terminal contract")
}

fn agent_caller_owner(
    owner_id: &str,
    owner_kind: CallerOwnerKind,
    role: CallerRole,
) -> CallerOwner {
    CallerOwner::new(
        agent_caller_namespace(),
        opaque_agent_id(owner_id),
        owner_kind,
        role,
    )
}

fn terminal_topology_id(session_id: &SessionId) -> TerminalTopologyId {
    TerminalTopologyId::new(opaque_agent_id(session_id.as_str()))
}

fn session_id_from_topology(
    topology_id: &TerminalTopologyId,
) -> Result<SessionId, ApplicationError> {
    SessionId::parse(topology_id.as_str()).map_err(|_| {
        ApplicationError::new(
            ApplicationErrorCode::Internal,
            "terminal topology does not map to an agent session",
        )
    })
}

fn protocol_release_reason(reason: ManagerReleaseReason) -> ModelReleaseReason {
    match reason {
        ManagerReleaseReason::IdleContext => ModelReleaseReason::IdleContext,
        ManagerReleaseReason::IdleModel => ModelReleaseReason::IdleModel,
        ManagerReleaseReason::Manual => ModelReleaseReason::Manual,
        ManagerReleaseReason::Shutdown => ModelReleaseReason::Shutdown,
        ManagerReleaseReason::Capacity => ModelReleaseReason::Capacity,
    }
}

fn protocol_release_outcome(outcome: ManagerReleaseOutcome) -> ModelReleaseOutcome {
    match outcome {
        ManagerReleaseOutcome::Released => ModelReleaseOutcome::Released,
        ManagerReleaseOutcome::Failed => ModelReleaseOutcome::Failed,
        ManagerReleaseOutcome::BackendLost => ModelReleaseOutcome::BackendLost,
    }
}

fn supervisor_error(error: agl_supervisor::SupervisorError) -> ProtocolError {
    let (code, retryable) = match error {
        agl_supervisor::SupervisorError::CommandQueueFull => (ProtocolErrorCode::Busy, true),
        agl_supervisor::SupervisorError::Store(agl_store::StoreError::NotFound { .. }) => {
            (ProtocolErrorCode::NotFound, false)
        }
        agl_supervisor::SupervisorError::Store(agl_store::StoreError::IdempotencyConflict {
            ..
        }) => (ProtocolErrorCode::InvalidRequest, false),
        _ => (ProtocolErrorCode::RuntimeFailure, false),
    };
    ProtocolError::new(code, error.to_string(), retryable)
}

fn busy_or_runtime(error: anyhow::Error, busy_message: &str) -> ProtocolError {
    if error.to_string().contains("busy or not registered") {
        ProtocolError::new(ProtocolErrorCode::Busy, busy_message, true)
    } else {
        runtime_error(error)
    }
}
