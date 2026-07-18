use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use agl_app::{
    ActiveRunView, ApplicationAction, ApplicationActionRequest, ApplicationActionResult,
    ApplicationError, ApplicationErrorCode, CommandContext, PresentationCursor, PromptAdmission,
    PromptSubmission, QueuedPromptView, SessionHeader, SessionOpen, SessionOpened,
    SessionPresentationItem, SessionPresentationSnapshot, SessionPresentationStatus,
    SuggestionPage, SuggestionRequest, UserExecutionView, UserShellAdmission, UserShellSubmission,
};
use agl_chat::{
    ChatOptions, ChatRunInput, ChatService, ChatSupervisorFactory, InferenceClientHandle,
    InferenceOptions, ToolAccessMode as ChatToolMode, shared_process_handle,
};
use agl_cron::{CronJob, CronTargetKind, STORE_STATUS_BUILTIN_CRON_TARGET};
use agl_functions::RuntimeDelegationPlan;
use agl_ids::{DaemonInstanceId, EventId, RequestId, RunId, SessionId, StepId, TurnId};
use agl_inference::ModelManagerStatus;
use agl_process::{
    ExecutionAuthorization, ExecutionCursor, ExecutionIo, ExecutionKind, ExecutionLimits,
    ExecutionListFilter, ExecutionOwner, ExecutionProfile, ExecutionRequest, ProcessError,
    ProcessErrorCode,
};
use agl_protocol::{
    DaemonCapability, DaemonEvent, DaemonEventKind, DaemonRequest, DaemonRequestKind,
    ExecutionKillAcceptedEvent, ExecutionListEvent, ExecutionReadEvent, ExecutionStatusEvent,
    HelloEvent, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ProtocolRunKind,
    ProtocolRunState, ProtocolToolMode, RunAcceptedEvent, RunEventsEvent, RunStatusEvent,
    RunTreeEvent, RunTreeNodeEvent, RunUsageEvent, SessionFinishedEvent, SessionListEvent,
    SessionOpenedEvent, SessionStatus, SessionStatusEvent, SessionSummary, SessionTranscriptEvent,
};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_session::{ChatSessionStore, SessionCatalogStatus};
use agl_store::{AglStore, RunBudget, RunKind, RunState, SafeRunStatus};
use agl_supervisor::{
    IdempotentRunSpec, RunAccepted, RunOutcome, RunSpec, RunSubscription, Supervisor,
    SupervisorHandle, SupervisorOptions,
};
use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};

use crate::run_factory::{BuiltinCronRunInput, DaemonRunFactory};
use crate::transcript::transcript_event;

const RUN_SUBMIT_IDEMPOTENCY_NAMESPACE: &str = "daemon.run_submit";
const CRON_RUN_IDEMPOTENCY_NAMESPACE: &str = "daemon.cron_run";
const PRIVATE_COMMAND_DISPLAY_MAX_BYTES: usize = 4096;

pub struct DaemonState {
    daemon_instance_id: DaemonInstanceId,
    runtime: AgentLibreRuntimeConfig,
    inference_defaults: InferenceOptions,
    inference_client: InferenceClientHandle,
    sessions: BTreeMap<SessionId, SessionRuntime>,
    chat_factory: ChatSupervisorFactory,
    process_handle: agl_process::ProcessHandle,
    _supervisor: Supervisor,
    supervisor_handle: SupervisorHandle,
    user_shell_admissions: BTreeMap<(SessionId, String), UserShellAdmission>,
}

#[derive(Clone)]
struct SessionRuntime {
    status: SessionStatus,
    resumed: bool,
    options: ChatOptions,
    delegation_plan: Option<RuntimeDelegationPlan>,
    execution_context: agl_process::ExecutionContextSnapshot,
}

impl DaemonState {
    pub fn new(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
    ) -> Self {
        Self::open(runtime, inference_defaults, inference_client)
            .expect("test daemon state should initialize")
    }

    pub fn open(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
    ) -> Result<Self> {
        let store_root = runtime.paths.store_root();
        let process_handle =
            shared_process_handle(&runtime).context("failed to start daemon process supervisor")?;
        let chat_factory = ChatSupervisorFactory::with_runtime(
            &store_root,
            runtime.clone(),
            inference_client.clone(),
        );
        let supervisor = Supervisor::spawn(
            &store_root,
            Arc::new(DaemonRunFactory::new(chat_factory.clone(), &store_root)),
            SupervisorOptions::default(),
        )
        .context("failed to start durable run supervisor")?;
        let supervisor_handle = supervisor.handle();
        Ok(Self {
            daemon_instance_id: DaemonInstanceId::generate(),
            runtime,
            inference_defaults,
            inference_client,
            sessions: BTreeMap::new(),
            chat_factory,
            process_handle,
            _supervisor: supervisor,
            supervisor_handle,
            user_shell_admissions: BTreeMap::new(),
        })
    }

    pub fn handle_request(&mut self, request: DaemonRequest) -> DaemonEvent {
        let request_id = request.request_id;
        let result = match request.kind {
            DaemonRequestKind::Hello(_) => Ok(DaemonEventKind::Hello(self.hello())),
            DaemonRequestKind::SessionOpen(request) => self.open_session(request),
            DaemonRequestKind::SessionClear(request) => self.clear_session(request.session_id),
            DaemonRequestKind::SessionFinish(request) => {
                self.finish_session(request.session_id, request.reason)
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
            DaemonRequestKind::RunSubscribe(_) => Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "run_subscribe must be handled by the streaming transport",
                false,
            )),
            DaemonRequestKind::ExecutionList(request) => self.execution_list(request),
            DaemonRequestKind::ExecutionStatus(request) => {
                self.execution_status(request.execution_id, request.include_private_command)
            }
            DaemonRequestKind::ExecutionRead(request) => self.execution_read(
                request.execution_id,
                request.after_sequence,
                request.max_bytes,
            ),
            DaemonRequestKind::ExecutionKill(request) => {
                self.execution_kill(request.execution_id, request.mode)
            }
            DaemonRequestKind::ExecutionAttach(_)
            | DaemonRequestKind::ExecutionLeaseRenew(_)
            | DaemonRequestKind::ExecutionInput(_)
            | DaemonRequestKind::ExecutionResize(_)
            | DaemonRequestKind::ExecutionDetach(_) => Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "execution attachment operations must be handled by the streaming transport",
                false,
            )),
            DaemonRequestKind::CommandCatalog(_)
            | DaemonRequestKind::CommandSuggestions(_)
            | DaemonRequestKind::ApplicationAction(_)
            | DaemonRequestKind::SessionPresentation(_)
            | DaemonRequestKind::SessionPresentationSubscribe(_)
            | DaemonRequestKind::SubscriptionCancel(_)
            | DaemonRequestKind::UserShellStart(_) => Err(ProtocolError::new(
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

    pub fn process_handle(&self) -> agl_process::ProcessHandle {
        self.process_handle.clone()
    }

    pub fn process_read_limit(&self) -> usize {
        self.runtime.execution.max_result_bytes
    }

    pub fn process_input_limit(&self) -> usize {
        self.runtime.execution.max_input_bytes
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

    fn hello(&self) -> HelloEvent {
        HelloEvent {
            protocol_version: PROTOCOL_VERSION.to_string(),
            product_version: env!("CARGO_PKG_VERSION").to_string(),
            daemon_instance_id: self.daemon_instance_id.clone(),
            capabilities: vec![
                DaemonCapability::SessionOpen,
                DaemonCapability::SessionClear,
                DaemonCapability::SessionFinish,
                DaemonCapability::SessionStatus,
                DaemonCapability::SessionList,
                DaemonCapability::SessionTranscript,
                DaemonCapability::FinalAssistantMessage,
                DaemonCapability::RuntimeEvents,
                DaemonCapability::RunSubmit,
                DaemonCapability::RunStatus,
                DaemonCapability::RunTree,
                DaemonCapability::RunCancel,
                DaemonCapability::RunReplay,
                DaemonCapability::RunSubscribe,
                DaemonCapability::ExecutionList,
                DaemonCapability::ExecutionControl,
                DaemonCapability::ExecutionAttach,
                DaemonCapability::CommandCatalog,
                DaemonCapability::CommandSuggestions,
                DaemonCapability::ApplicationActions,
                DaemonCapability::SessionPresentation,
                DaemonCapability::UserShell,
                DaemonCapability::AssistantDeltas,
            ],
        }
    }

    fn execution_list(
        &self,
        request: agl_protocol::ExecutionListRequest,
    ) -> Result<DaemonEventKind, ProtocolError> {
        let executions = self
            .process_handle
            .operator_list(ExecutionListFilter {
                session_id: request.session_id,
                root_run_id: request.root_run_id,
                include_finished: request.include_finished,
            })
            .map_err(process_error)?;
        Ok(DaemonEventKind::ExecutionList(ExecutionListEvent {
            executions,
        }))
    }

    fn execution_status(
        &self,
        execution_id: agl_ids::ExecutionId,
        include_private_command: bool,
    ) -> Result<DaemonEventKind, ProtocolError> {
        let status = self
            .process_handle
            .operator_status(&execution_id)
            .map_err(process_error)?;
        let private_command = include_private_command
            .then(|| {
                self.process_handle
                    .operator_private_command(&execution_id, PRIVATE_COMMAND_DISPLAY_MAX_BYTES)
                    .map_err(process_error)
            })
            .transpose()?;
        Ok(DaemonEventKind::ExecutionStatus(ExecutionStatusEvent {
            status,
            private_command,
        }))
    }

    fn execution_read(
        &self,
        execution_id: agl_ids::ExecutionId,
        after_sequence: u64,
        max_bytes: usize,
    ) -> Result<DaemonEventKind, ProtocolError> {
        if max_bytes == 0 || max_bytes > self.runtime.execution.max_result_bytes {
            return Err(invalid(format!(
                "execution max_bytes must be between 1 and {}",
                self.runtime.execution.max_result_bytes
            )));
        }
        let output = self
            .process_handle
            .operator_read(&execution_id, ExecutionCursor { after_sequence }, max_bytes)
            .map_err(process_error)?;
        Ok(DaemonEventKind::ExecutionRead(ExecutionReadEvent {
            output,
        }))
    }

    fn execution_kill(
        &self,
        execution_id: agl_ids::ExecutionId,
        mode: agl_process::KillMode,
    ) -> Result<DaemonEventKind, ProtocolError> {
        self.process_handle
            .operator_kill(&execution_id, mode)
            .map_err(process_error)?;
        Ok(DaemonEventKind::ExecutionKillAccepted(
            ExecutionKillAcceptedEvent { execution_id, mode },
        ))
    }

    fn open_session(
        &mut self,
        request: agl_protocol::SessionOpenRequest,
    ) -> Result<DaemonEventKind, ProtocolError> {
        if request.new_session && request.session_id.is_some() {
            return Err(invalid("new session cannot include session_id"));
        }
        let resumed_workspace = if !request.new_session
            && request.workspace_root.is_none()
            && let Some(session_id) = request.session_id.as_ref()
            && ChatSessionStore::exists(self.runtime.paths.sessions_root(), session_id)
        {
            Some(
                ChatSessionStore::open(self.runtime.paths.sessions_root(), session_id.clone())
                    .map_err(runtime_error)?
                    .execution_context()
                    .workspace_root
                    .clone(),
            )
        } else {
            None
        };
        let workspace_root = request
            .workspace_root
            .map(PathBuf::from)
            .or(resumed_workspace);
        let options = ChatOptions {
            inference: InferenceOptions {
                skills: request.skills,
                tool_mode: chat_tool_mode(request.tool_mode),
                workspace_root: workspace_root.clone(),
                function_ref: request
                    .function_ref
                    .or_else(|| self.inference_defaults.function_ref.clone()),
                ..self.inference_defaults.clone()
            },
            workspace_root,
            session_id: request.session_id,
            no_history: false,
            new_session: request.new_session,
        };
        let service = ChatService::open(
            options.clone(),
            &self.runtime,
            self.inference_client.clone(),
        )
        .map_err(runtime_error)?;
        let summary = service.summary();
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
            },
        );
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
    ) -> Result<DaemonEventKind, ProtocolError> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| not_found(session_id.as_str()))?;
        if session.status == SessionStatus::Finished {
            return Ok(DaemonEventKind::SessionFinished(SessionFinishedEvent {
                session_id,
                reason,
            }));
        }
        self.chat_factory
            .with_session(&session_id, |service| service.request_exit())
            .map_err(|error| busy_or_runtime(error, "session has an active durable run"))?;
        session.status = SessionStatus::Finished;
        Ok(DaemonEventKind::SessionFinished(SessionFinishedEvent {
            session_id,
            reason,
        }))
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
                    updated_at_unix_ms: entry.metadata.updated_at_unix_ms,
                },
            );
        }
        let now = u128::MAX;
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
        if !request.content.has_artifacts()
            && request
                .content
                .text_only()
                .is_some_and(|text| text.trim().is_empty())
        {
            return Err(invalid("run content cannot be blank"));
        }
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
            fingerprint: run_fingerprint(&request.session_id, &request.content),
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
                    input,
                    checkpoint: None,
                    effective_policy_hash: None,
                    execution_context: session.execution_context.clone(),
                    budget: RunBudget {
                        wall_time_ms: request.budget.wall_time_ms,
                        model_input_tokens: request.budget.model_input_tokens,
                        model_output_tokens: request.budget.model_output_tokens,
                        model_attempts: request.budget.model_attempts,
                        capability_calls: request.budget.capability_calls,
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
        if launch.model_id.is_some() {
            return Err(ApplicationError::new(
                ApplicationErrorCode::CommandUnavailable,
                "explicit model selection is not available for this daemon profile",
            ));
        }
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
        let snapshot = self.application_snapshot(&opened.session_id)?;
        Ok(SessionOpened {
            session_id: opened.session_id,
            resumed: opened.resumed,
            snapshot,
        })
    }

    pub(crate) fn application_snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionPresentationSnapshot, ApplicationError> {
        let session = self.sessions.get(session_id).cloned().ok_or_else(|| {
            ApplicationError::new(ApplicationErrorCode::NotFound, "session not found")
        })?;
        let store = ChatSessionStore::open(self.runtime.paths.sessions_root(), session_id.clone())
            .map_err(application_runtime_error)?;
        let replay = store.read_replay().map_err(application_runtime_error)?;
        let mut items = Vec::new();
        let mut shell_items = BTreeMap::new();
        for event in replay.events {
            match event {
                agl_session::ChatSessionEvent::Runtime { envelope } => match envelope.payload {
                    agl_events::RuntimeEvent::UserMessage {
                        message_id,
                        content,
                    } => items.push(SessionPresentationItem::UserMessage {
                        message_id,
                        content,
                    }),
                    agl_events::RuntimeEvent::AssistantMessage {
                        message_id,
                        content,
                    } => items.push(SessionPresentationItem::AssistantMessage {
                        message_id,
                        content,
                        state: agl_app::AssistantItemState::Final,
                    }),
                    _ => {}
                },
                agl_session::ChatSessionEvent::ContextCleared { .. } => {
                    items.push(SessionPresentationItem::ContextBoundary {
                        event_id: EventId::generate(),
                        reason: "context_cleared".to_owned(),
                    });
                }
                agl_session::ChatSessionEvent::UserShellStarted {
                    execution_id,
                    command,
                    profile,
                    cwd,
                    ..
                } => {
                    shell_items.insert(
                        execution_id.clone(),
                        SessionPresentationItem::UserExecution {
                            execution_id,
                            command,
                            profile,
                            cwd: cwd.to_string_lossy().into_owned(),
                            state: agl_process::ExecutionState::OutcomeUnknown,
                            exit: None,
                            output: Vec::new(),
                            output_truncated: false,
                        },
                    );
                }
                agl_session::ChatSessionEvent::UserShellFinished {
                    execution_id,
                    state,
                    exit,
                    output_truncated,
                    ..
                } => {
                    if let Some(SessionPresentationItem::UserExecution {
                        state: item_state,
                        exit: item_exit,
                        output_truncated: item_truncated,
                        ..
                    }) = shell_items.get_mut(&execution_id)
                    {
                        *item_state = state;
                        *item_exit = exit;
                        *item_truncated = output_truncated;
                    }
                }
                agl_session::ChatSessionEvent::SessionStarted { .. }
                | agl_session::ChatSessionEvent::SessionFinished { .. }
                | agl_session::ChatSessionEvent::SessionFailed { .. } => {}
            }
        }

        let statuses = self
            .process_handle
            .operator_list(ExecutionListFilter {
                session_id: Some(session_id.clone()),
                root_run_id: None,
                include_finished: true,
            })
            .map_err(application_process_error)?;
        let mut executions = Vec::new();
        for status in statuses {
            if !matches!(status.owner, ExecutionOwner::Session { .. }) {
                continue;
            }
            if let Some(SessionPresentationItem::UserExecution {
                state,
                exit,
                output,
                output_truncated,
                ..
            }) = shell_items.get_mut(&status.execution_id)
            {
                *state = status.state;
                *exit = status.exit.clone();
                *output_truncated = status.output_truncated;
                if let Ok(read) = self.process_handle.operator_read(
                    &status.execution_id,
                    ExecutionCursor::default(),
                    self.runtime.execution.max_result_bytes,
                ) {
                    *output = read.chunks;
                    *output_truncated |= read.output_truncated || read.output_expired;
                }
            }
            executions.push(UserExecutionView {
                execution_id: status.execution_id,
                state: status.state,
                profile: status.profile,
                last_sequence: status.last_sequence,
                output_truncated: status.output_truncated || status.output_expired,
            });
        }
        items.extend(shell_items.into_values());
        let active_execution_count = executions
            .iter()
            .filter(|execution| execution.state.is_live())
            .count() as u32;
        let active_run_count = u32::from(session.status == SessionStatus::Busy);
        let status = match session.status {
            SessionStatus::Open | SessionStatus::Busy => SessionPresentationStatus::Active,
            SessionStatus::Finished => SessionPresentationStatus::Finished,
            SessionStatus::Failed => SessionPresentationStatus::Failed,
        };
        let operation_mode = session.options.inference.tool_mode;
        let command_context = CommandContext {
            session_id: Some(session_id.clone()),
            session_active: status == SessionPresentationStatus::Active,
            active_or_queued_turns: active_run_count,
            active_executions: active_execution_count,
            host_shell_available: true,
            operation_mode,
        };
        Ok(SessionPresentationSnapshot {
            session_id: session_id.clone(),
            cursor: PresentationCursor {
                daemon_instance_id: self.daemon_instance_id.clone(),
                revision: 0,
            },
            header: SessionHeader {
                session_id: session_id.clone(),
                status,
                durable: true,
                resumed: session.resumed,
                title: None,
                function_name: session
                    .options
                    .inference
                    .function_ref
                    .clone()
                    .unwrap_or_else(|| "agentLIBRE".to_owned()),
                model_id: None,
                operation_mode,
                selected_skills: session.options.inference.skills.clone(),
                runtime_context_revision: 1,
                workspace_root: session
                    .execution_context
                    .workspace_root
                    .to_string_lossy()
                    .into_owned(),
                cwd: session
                    .execution_context
                    .working_directory
                    .to_string_lossy()
                    .into_owned(),
                execution_context_revision: session.execution_context.revision,
                context_used_tokens: None,
                context_limit_tokens: None,
                active_run_count,
                queued_prompt_count: 0,
                active_execution_count,
            },
            items,
            active_run: None::<ActiveRunView>,
            queued_prompts: Vec::<QueuedPromptView>::new(),
            executions,
            command_context,
        })
    }

    pub(crate) fn application_submit_prompt(
        &self,
        request: PromptSubmission,
    ) -> Result<PromptAdmission, ApplicationError> {
        let response = self
            .submit_run(
                RequestId::generate(),
                agl_protocol::RunSubmitRequest {
                    session_id: request.session_id.clone(),
                    content: request.content,
                    client_submission_id: request.client_submission_id,
                    budget: agl_protocol::RunBudgetRequest::default(),
                },
            )
            .map_err(application_protocol_error)?;
        let DaemonEventKind::RunAccepted(accepted) = response else {
            unreachable!("run submit has one response family")
        };
        Ok(PromptAdmission {
            session_id: accepted.session_id,
            run_id: accepted.run_id,
            ordinal: 1,
            queued: accepted.state == ProtocolRunState::Queued,
            replayed: accepted.replayed,
        })
    }

    pub(crate) fn application_suggestions(
        &self,
        request: SuggestionRequest,
    ) -> Result<SuggestionPage, ApplicationError> {
        let query = request.query.to_ascii_lowercase();
        let entries = match request.argument_id.as_str() {
            "mode" => ["read_only", "write", "execute", "approve", "admin"]
                .into_iter()
                .filter(|value| value.contains(&query))
                .map(|value| agl_app::Suggestion {
                    value: value.to_owned(),
                    label: value.replace('_', " "),
                    detail: None,
                })
                .collect(),
            _ => Vec::new(),
        };
        Ok(SuggestionPage {
            entries,
            next_cursor: None,
        })
    }

    pub(crate) fn application_invoke(
        &mut self,
        request: ApplicationActionRequest,
    ) -> Result<ApplicationActionResult, ApplicationError> {
        if request.client_submission_id.is_empty() || request.client_submission_id.len() > 256 {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "client submission ID must be nonempty and bounded",
            ));
        }
        match request.action {
            ApplicationAction::SessionNew { launch } => self
                .application_open_session(SessionOpen { launch })
                .map(|opened| ApplicationActionResult::SessionOpened {
                    opened: Box::new(opened),
                }),
            ApplicationAction::SessionResume { .. } => Err(ApplicationError::new(
                ApplicationErrorCode::CommandUnavailable,
                "session resume requires an explicit session ID through the open-session API",
            )),
            action => {
                let session_id = request.session_id.ok_or_else(|| {
                    ApplicationError::new(
                        ApplicationErrorCode::InvalidArguments,
                        "application action requires a current session",
                    )
                })?;
                match action {
                    ApplicationAction::SessionStatus
                    | ApplicationAction::WorkspaceGet
                    | ApplicationAction::WorkingDirectoryGet => self
                        .application_snapshot(&session_id)
                        .map(|snapshot| ApplicationActionResult::Status {
                            header: snapshot.header,
                        }),
                    ApplicationAction::WorkspaceSet { path } => {
                        self.chat_factory
                            .with_session(&session_id, |service| service.set_workspace_root(&path))
                            .map_err(application_busy_error)?;
                        self.refresh_session_execution_context(&session_id)?;
                        self.application_snapshot(&session_id).map(|snapshot| {
                            ApplicationActionResult::WorkspaceChanged {
                                header: snapshot.header,
                            }
                        })
                    }
                    ApplicationAction::WorkingDirectorySet { path, profile } => {
                        self.chat_factory
                            .with_session(&session_id, |service| {
                                service
                                    .set_working_directory(&path, profile == ExecutionProfile::Host)
                                    .map(|_| ())
                            })
                            .map_err(application_busy_error)?;
                        self.refresh_session_execution_context(&session_id)?;
                        self.application_snapshot(&session_id).map(|snapshot| {
                            ApplicationActionResult::WorkingDirectoryChanged {
                                header: snapshot.header,
                            }
                        })
                    }
                    ApplicationAction::ExecutionList { include_finished } => self
                        .process_handle
                        .operator_list(ExecutionListFilter {
                            session_id: Some(session_id),
                            root_run_id: None,
                            include_finished,
                        })
                        .map_err(application_process_error)
                        .map(|executions| ApplicationActionResult::Executions { executions }),
                    ApplicationAction::ExecutionAttach {
                        execution_id,
                        read_only,
                    } => Ok(ApplicationActionResult::AttachAccepted {
                        execution_id,
                        read_only,
                    }),
                    ApplicationAction::ExecutionKill { execution_id, mode } => {
                        self.process_handle
                            .operator_kill(&execution_id, mode)
                            .map_err(application_process_error)?;
                        Ok(ApplicationActionResult::KillAccepted { execution_id, mode })
                    }
                    ApplicationAction::RuntimeContextReload => {
                        let visible_tools = self
                            .chat_factory
                            .with_session(&session_id, |service| service.reload_runtime_context())
                            .map_err(application_busy_error)?;
                        Ok(ApplicationActionResult::Reloaded {
                            visible_tools: (0..visible_tools)
                                .map(|index| format!("visible-tool-{index}"))
                                .collect(),
                            context_revision: 1,
                        })
                    }
                    ApplicationAction::SessionClear => {
                        let removed_messages = self
                            .chat_factory
                            .with_session(&session_id, |service| service.clear_context())
                            .map_err(application_busy_error)?;
                        Ok(ApplicationActionResult::Cleared {
                            removed_messages: removed_messages as u64,
                            cursor: PresentationCursor {
                                daemon_instance_id: self.daemon_instance_id.clone(),
                                revision: 0,
                            },
                        })
                    }
                    ApplicationAction::SessionExit { confirm_active } => {
                        let active = self
                            .process_handle
                            .operator_list(ExecutionListFilter {
                                session_id: Some(session_id.clone()),
                                root_run_id: None,
                                include_finished: false,
                            })
                            .map_err(application_process_error)?;
                        if !active.is_empty() && !confirm_active {
                            return Err(ApplicationError::new(
                                ApplicationErrorCode::SessionBusy,
                                "session has active work; confirmation is required",
                            ));
                        }
                        let terminated_executions = self
                            .process_handle
                            .terminate_owner(&ExecutionOwner::Session {
                                session_id: session_id.clone(),
                                root_run_id: RunId::generate(),
                            })
                            .map_err(application_process_error)?
                            as u32;
                        self.finish_session(
                            session_id.clone(),
                            agl_protocol::SessionFinishReason::ExitCommand,
                        )
                        .map_err(application_protocol_error)?;
                        Ok(ApplicationActionResult::SessionExited {
                            session_id,
                            cancelled_runs: 0,
                            terminated_executions,
                        })
                    }
                    ApplicationAction::ModelSelect { .. }
                    | ApplicationAction::OperationModeSelect { .. }
                    | ApplicationAction::SkillsSelect { .. } => Err(ApplicationError::new(
                        ApplicationErrorCode::CommandUnavailable,
                        "runtime selection mutation is not available for this session",
                    )),
                    ApplicationAction::SessionNew { .. }
                    | ApplicationAction::SessionResume { .. } => unreachable!(),
                }
            }
        }
    }

    pub(crate) fn application_start_user_shell(
        &mut self,
        submission: UserShellSubmission,
    ) -> Result<UserShellAdmission, ApplicationError> {
        submission.validate()?;
        if let Some(admission) = self.user_shell_admissions.get(&(
            submission.session_id.clone(),
            submission.client_submission_id.clone(),
        )) {
            let mut replayed = admission.clone();
            replayed.replayed = true;
            return Ok(replayed);
        }
        let session = self
            .sessions
            .get(&submission.session_id)
            .cloned()
            .ok_or_else(|| {
                ApplicationError::new(ApplicationErrorCode::NotFound, "session not found")
            })?;
        if session.execution_context.revision != submission.execution_context_revision {
            return Err(ApplicationError::new(
                ApplicationErrorCode::StaleContextRevision,
                format!(
                    "execution context changed to revision {}",
                    session.execution_context.revision
                ),
            ));
        }
        session
            .execution_context
            .shell
            .verify_executable()
            .map_err(application_process_error)?;
        let run_id = RunId::generate();
        let step_id = StepId::generate();
        let fingerprint = user_shell_fingerprint(&submission);
        let (authorization, grant_lease) = if submission.profile == ExecutionProfile::Host {
            let store = AglStore::open_current_at(self.runtime.paths.store_root())
                .map_err(application_runtime_error)?;
            let grant = store
                .create_permission_grant(agl_store::PermissionGrantDraft {
                    request_id: None,
                    tool_id: "operator.user_shell".to_owned(),
                    max_operation_kind: "execute".to_owned(),
                    state_effects: vec!["host_process_execution".to_owned()],
                    sensitive_inputs: vec!["shell_command".to_owned()],
                    scope: serde_json::json!({
                        "session_id": submission.session_id.as_str(),
                        "fingerprint": fingerprint,
                    }),
                    duration: "one_turn".to_owned(),
                    granted_by_ref: format!("local-uid:{}", submission.operator.uid),
                })
                .map_err(application_runtime_error)?;
            (
                ExecutionAuthorization {
                    host_process_execution: true,
                    shell_login_startup: false,
                },
                Some(agl_process::ExecutionGrantLease {
                    grant_id: grant.id,
                    duration: grant.duration,
                    scope_digest: fingerprint.clone(),
                }),
            )
        } else {
            (ExecutionAuthorization::default(), None)
        };
        let mut args = session.execution_context.shell.command_args.clone();
        args.push(submission.command.clone());
        let request = ExecutionRequest {
            owner: ExecutionOwner::Session {
                session_id: submission.session_id.clone(),
                root_run_id: run_id.clone(),
            },
            creating_run_id: run_id.clone(),
            creating_step_id: step_id.clone(),
            kind: ExecutionKind::Shell,
            program: session.execution_context.shell.program.clone(),
            program_digest: Some(session.execution_context.shell.executable_digest.clone()),
            args,
            workspace_root: session.execution_context.workspace_root.clone(),
            cwd: session.execution_context.working_directory.clone(),
            read_only_roots: self.runtime.execution.runtime_read_only_roots.clone(),
            environment: self
                .runtime
                .execution
                .admitted_environment()
                .map_err(application_runtime_error)?,
            stdin: None,
            close_stdin_after_initial: false,
            io: ExecutionIo::Pty,
            terminal_size: Some(submission.terminal_size),
            profile: submission.profile,
            authorization,
            grant_lease: grant_lease.clone(),
            limits: ExecutionLimits {
                timeout_ms: (!submission.background)
                    .then_some(self.runtime.execution.maximum_foreground_timeout_ms),
                max_input_bytes: self.runtime.execution.max_input_bytes as u64,
                max_output_bytes: self.runtime.execution.max_spool_bytes,
            },
        };
        let status = self
            .process_handle
            .start(request)
            .map_err(application_process_error)?;
        if let Some(lease) = &grant_lease {
            let store = AglStore::open_current_at(self.runtime.paths.store_root())
                .map_err(application_runtime_error)?;
            store
                .admit_permission_grant(&lease.grant_id, run_id.as_str())
                .map_err(application_runtime_error)?;
        }
        self.chat_factory
            .with_session(&submission.session_id, |service| {
                service.record_user_shell_started(
                    run_id.clone(),
                    step_id.clone(),
                    status.execution_id.clone(),
                    submission.command.clone(),
                    submission.profile,
                    status.cwd.clone(),
                    submission.background,
                )
            })
            .map_err(application_busy_error)?;
        let admission = UserShellAdmission {
            session_id: submission.session_id.clone(),
            run_id,
            step_id,
            execution_id: status.execution_id.clone(),
            resolved_cwd: status.cwd.to_string_lossy().into_owned(),
            profile: submission.profile,
            status: status.clone(),
            background: submission.background,
            replayed: false,
        };
        self.user_shell_admissions.insert(
            (
                submission.session_id.clone(),
                submission.client_submission_id,
            ),
            admission.clone(),
        );
        monitor_user_shell(
            self.chat_factory.clone(),
            self.process_handle.clone(),
            submission.session_id,
            status.execution_id,
        );
        Ok(admission)
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
}

fn monitor_user_shell(
    chat_factory: ChatSupervisorFactory,
    process: agl_process::ProcessHandle,
    session_id: SessionId,
    execution_id: agl_ids::ExecutionId,
) {
    let _ = thread::Builder::new()
        .name(format!("agl-user-shell-{execution_id}"))
        .spawn(move || {
            let status = loop {
                match process.operator_status(&execution_id) {
                    Ok(status) if status.state.is_terminal() => break status,
                    Ok(_) => thread::sleep(Duration::from_millis(25)),
                    Err(_) => return,
                }
            };
            let _ = chat_factory.with_session(&session_id, |service| {
                service.record_user_shell_finished(
                    execution_id,
                    status.state,
                    status.exit,
                    status.last_sequence,
                    status.output_truncated || status.output_expired,
                )
            });
        });
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

fn user_shell_fingerprint(submission: &UserShellSubmission) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agl-app.user_shell.v1\0");
    hasher.update(submission.session_id.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(submission.command.as_bytes());
    hasher.update(b"\0");
    hasher.update(submission.execution_context_revision.to_le_bytes());
    hasher.update([submission.profile as u8]);
    hasher.update(submission.terminal_size.columns.to_le_bytes());
    hasher.update(submission.terminal_size.rows.to_le_bytes());
    hasher.update([submission.background as u8]);
    let digest = hasher.finalize();
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}

fn application_protocol_error(error: ProtocolError) -> ApplicationError {
    let code = match error.code {
        ProtocolErrorCode::InvalidRequest => ApplicationErrorCode::InvalidArguments,
        ProtocolErrorCode::NotFound => ApplicationErrorCode::NotFound,
        ProtocolErrorCode::Unauthorized => ApplicationErrorCode::NotAuthorized,
        ProtocolErrorCode::Busy => ApplicationErrorCode::InputBackpressure,
        ProtocolErrorCode::Unsupported | ProtocolErrorCode::UnsupportedProtocolVersion => {
            ApplicationErrorCode::CommandUnavailable
        }
        ProtocolErrorCode::RuntimeFailure => ApplicationErrorCode::Internal,
    };
    ApplicationError::new(code, error.message)
}

fn application_process_error(error: ProcessError) -> ApplicationError {
    let protocol = process_error(error);
    application_protocol_error(protocol)
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

#[derive(Clone)]
pub struct SharedDaemonState {
    pub(crate) inner: Arc<Mutex<DaemonState>>,
    application: agl_app::ApplicationService,
}

impl SharedDaemonState {
    pub fn new(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
    ) -> Self {
        Self::from_state(DaemonState::new(
            runtime,
            inference_defaults,
            inference_client,
        ))
    }

    pub fn open(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
    ) -> Result<Self> {
        Ok(Self::from_state(DaemonState::open(
            runtime,
            inference_defaults,
            inference_client,
        )?))
    }

    fn from_state(state: DaemonState) -> Self {
        let daemon_instance_id = state.daemon_instance_id.clone();
        let inner = Arc::new(Mutex::new(state));
        let application =
            crate::surface::application_service(daemon_instance_id, Arc::downgrade(&inner));
        Self { inner, application }
    }

    pub(crate) fn application(&self) -> agl_app::ApplicationService {
        self.application.clone()
    }

    pub fn handle_request(&self, request: DaemonRequest) -> DaemonEvent {
        match self.inner.lock() {
            Ok(mut state) => state.handle_request(request),
            Err(error) => DaemonEvent::new(
                Some(request.request_id),
                DaemonEventKind::Error(ProtocolError::new(
                    ProtocolErrorCode::RuntimeFailure,
                    format!("daemon state lock is poisoned: {error}"),
                    false,
                )),
            ),
        }
    }

    pub fn subscribe_run(
        &self,
        run_id: RunId,
        after_sequence: u64,
    ) -> Result<RunSubscription, ProtocolError> {
        self.inner
            .lock()
            .map_err(|error| runtime_error(anyhow!("daemon state lock is poisoned: {error}")))?
            .subscribe_run(run_id, after_sequence)
    }

    pub fn run_outcome(&self, run_id: RunId) -> Result<RunOutcome, ProtocolError> {
        self.inner
            .lock()
            .map_err(|error| runtime_error(anyhow!("daemon state lock is poisoned: {error}")))?
            .run_outcome(run_id)
    }

    pub fn model_manager_status(&self) -> Result<ModelManagerStatus> {
        self.inner
            .lock()
            .map_err(|error| anyhow!("daemon state lock is poisoned: {error}"))?
            .model_manager_status()
    }

    pub fn supervisor_handle(&self) -> Result<SupervisorHandle> {
        Ok(self
            .inner
            .lock()
            .map_err(|error| anyhow!("daemon state lock is poisoned: {error}"))?
            .supervisor_handle())
    }

    pub fn process_handle(&self) -> Result<agl_process::ProcessHandle> {
        Ok(self
            .inner
            .lock()
            .map_err(|error| anyhow!("daemon state lock is poisoned: {error}"))?
            .process_handle())
    }

    pub fn process_read_limit(&self) -> Result<usize> {
        Ok(self
            .inner
            .lock()
            .map_err(|error| anyhow!("daemon state lock is poisoned: {error}"))?
            .process_read_limit())
    }

    pub fn process_input_limit(&self) -> Result<usize> {
        Ok(self
            .inner
            .lock()
            .map_err(|error| anyhow!("daemon state lock is poisoned: {error}"))?
            .process_input_limit())
    }

    pub fn submit_cron_job(
        &self,
        job: &CronJob,
        scheduled_for: &str,
    ) -> Result<RunAccepted, ProtocolError> {
        self.inner
            .lock()
            .map_err(|error| runtime_error(anyhow!("daemon state lock is poisoned: {error}")))?
            .submit_cron_job(job, scheduled_for)
    }
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
        usage: RunUsageEvent {
            wall_time_ms: status.usage.wall_time_ms,
            model_input_tokens: status.usage.model_input_tokens,
            model_output_tokens: status.usage.model_output_tokens,
            model_attempts: status.usage.model_attempts,
            capability_calls: status.usage.capability_calls,
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
        usage: RunUsageEvent {
            wall_time_ms: status.usage.wall_time_ms,
            model_input_tokens: status.usage.model_input_tokens,
            model_output_tokens: status.usage.model_output_tokens,
            model_attempts: status.usage.model_attempts,
            capability_calls: status.usage.capability_calls,
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
        RunState::Failed => ProtocolRunState::Failed,
        RunState::Cancelled => ProtocolRunState::Cancelled,
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

pub(crate) fn process_error(error: ProcessError) -> ProtocolError {
    let (code, retryable) = match error.code() {
        ProcessErrorCode::InvalidRequest
        | ProcessErrorCode::InvalidBytes
        | ProcessErrorCode::InputTooLarge
        | ProcessErrorCode::InvalidTerminalSize
        | ProcessErrorCode::IoModeMismatch
        | ProcessErrorCode::InputLeaseExpired
        | ProcessErrorCode::ExecutionNotLive => (ProtocolErrorCode::InvalidRequest, false),
        ProcessErrorCode::ExecutionNotFound | ProcessErrorCode::OutputExpired => {
            (ProtocolErrorCode::NotFound, false)
        }
        ProcessErrorCode::ExecutionNotOwned
        | ProcessErrorCode::HostAuthorityRequired
        | ProcessErrorCode::LoginAuthorityRequired
        | ProcessErrorCode::GrantRevoked
        | ProcessErrorCode::GrantExpired => (ProtocolErrorCode::Unauthorized, false),
        ProcessErrorCode::PlatformUnsupported
        | ProcessErrorCode::LauncherUnavailable
        | ProcessErrorCode::SandboxUnavailable
        | ProcessErrorCode::SandboxExecutableUnavailable => (ProtocolErrorCode::Unsupported, false),
        ProcessErrorCode::ActiveLimitReached
        | ProcessErrorCode::InputBackpressure
        | ProcessErrorCode::InputLeaseBusy => (ProtocolErrorCode::Busy, true),
        ProcessErrorCode::LauncherProtocol
        | ProcessErrorCode::SpawnFailed
        | ProcessErrorCode::Cancelled
        | ProcessErrorCode::TimedOut
        | ProcessErrorCode::OutputLimitExceeded
        | ProcessErrorCode::SupervisorShutdown
        | ProcessErrorCode::StateConflict
        | ProcessErrorCode::StoreCorrupt
        | ProcessErrorCode::Internal => (ProtocolErrorCode::RuntimeFailure, false),
    };
    ProtocolError::new(code, error.to_string(), retryable)
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
