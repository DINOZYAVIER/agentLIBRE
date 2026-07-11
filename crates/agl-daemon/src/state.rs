use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agl_chat::{
    ChatOptions, ChatRunInput, ChatService, ChatSupervisorFactory, InferenceClientHandle,
    InferenceOptions, ToolAccessMode as ChatToolMode,
};
use agl_cron::{CronJob, CronTargetKind, STORE_STATUS_BUILTIN_CRON_TARGET};
use agl_ids::{RequestId, RunId, SessionId, TurnId};
use agl_inference::ModelManagerStatus;
use agl_protocol::{
    DaemonCapability, DaemonEvent, DaemonEventKind, DaemonRequest, DaemonRequestKind, HelloEvent,
    PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ProtocolRunState, ProtocolToolMode,
    RunAcceptedEvent, RunEventsEvent, RunStatusEvent, RunUsageEvent, SessionFinishedEvent,
    SessionListEvent, SessionOpenedEvent, SessionStatus, SessionStatusEvent, SessionSummary,
    SessionTranscriptEvent,
};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_session::ChatSessionStore;
use agl_store::{RunBudget, RunState};
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

pub struct DaemonState {
    runtime: AgentLibreRuntimeConfig,
    inference_defaults: InferenceOptions,
    inference_client: InferenceClientHandle,
    sessions: BTreeMap<SessionId, SessionRuntime>,
    chat_factory: ChatSupervisorFactory,
    _supervisor: Supervisor,
    supervisor_handle: SupervisorHandle,
}

struct SessionRuntime {
    status: SessionStatus,
    options: ChatOptions,
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
            runtime,
            inference_defaults,
            inference_client,
            sessions: BTreeMap::new(),
            chat_factory,
            _supervisor: supervisor,
            supervisor_handle,
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
            DaemonRequestKind::RunCancel(request) => self.cancel_run(request.run_id),
            DaemonRequestKind::RunEvents(request) => {
                self.run_events(request.run_id, request.after_sequence, request.limit)
            }
            DaemonRequestKind::RunSubscribe(_) => Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "run_subscribe must be handled by the streaming transport",
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
        let (session_id, turn_id, input, registered_session) = match job.target_kind {
            CronTargetKind::Builtin => {
                if job.target_ref != STORE_STATUS_BUILTIN_CRON_TARGET {
                    return Err(invalid(format!(
                        "unsupported builtin cron target {}",
                        job.target_ref
                    )));
                }
                (
                    None,
                    None,
                    serde_json::to_value(BuiltinCronRunInput {
                        builtin: job.target_ref.clone(),
                    })
                    .map_err(runtime_error)?,
                    None,
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
                    serde_json::to_value(ChatRunInput {
                        text: prompt,
                        request_id: None,
                        options: persisted_options,
                    })
                    .map_err(runtime_error)?,
                    Some(session_id),
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
                DaemonCapability::RunCancel,
                DaemonCapability::RunReplay,
                DaemonCapability::RunSubscribe,
            ],
        }
    }

    fn open_session(
        &mut self,
        request: agl_protocol::SessionOpenRequest,
    ) -> Result<DaemonEventKind, ProtocolError> {
        if request.new_session && request.session_id.is_some() {
            return Err(invalid("new session cannot include session_id"));
        }
        let options = ChatOptions {
            inference: InferenceOptions {
                skills: request.skills,
                tool_mode: chat_tool_mode(request.tool_mode),
                workspace_root: request.workspace_root.as_ref().map(PathBuf::from),
                ..self.inference_defaults.clone()
            },
            workspace_root: request.workspace_root.map(PathBuf::from),
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
        let session_id = summary.session_id.clone();
        self.chat_factory.register(service).map_err(runtime_error)?;
        self.sessions.insert(
            session_id.clone(),
            SessionRuntime {
                status: SessionStatus::Open,
                options: ChatOptions {
                    session_id: Some(session_id.clone()),
                    new_session: false,
                    ..options
                },
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
        Ok(DaemonEventKind::SessionList(SessionListEvent {
            sessions: self
                .sessions
                .iter()
                .map(|(session_id, session)| SessionSummary {
                    session_id: session_id.clone(),
                    title: None,
                    status: session.status,
                })
                .collect(),
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
        if request.text.trim().is_empty() {
            return Err(invalid("run text cannot be blank"));
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
        let input = serde_json::to_value(ChatRunInput {
            text: request.text.clone(),
            request_id: Some(request_id),
            options: session.options.clone(),
        })
        .map_err(runtime_error)?;
        let idempotency = request.idempotency_key.map(|key| IdempotentRunSpec {
            namespace: RUN_SUBMIT_IDEMPOTENCY_NAMESPACE.to_string(),
            fingerprint: run_fingerprint(&request.session_id, &request.text),
            key,
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
        Ok(DaemonEventKind::RunStatus(run_status_event(outcome)))
    }

    fn cancel_run(&self, run_id: RunId) -> Result<DaemonEventKind, ProtocolError> {
        self.supervisor_handle
            .cancel(run_id.clone())
            .map_err(supervisor_error)?;
        self.run_status(run_id)
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
}

#[derive(Clone)]
pub struct SharedDaemonState {
    inner: Arc<Mutex<DaemonState>>,
}

impl SharedDaemonState {
    pub fn new(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DaemonState::new(
                runtime,
                inference_defaults,
                inference_client,
            ))),
        }
    }

    pub fn open(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
    ) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Mutex::new(DaemonState::open(
                runtime,
                inference_defaults,
                inference_client,
            )?)),
        })
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
    RunStatusEvent {
        session_id: status.session_id,
        run_id: status.run_id,
        turn_id: status.turn_id,
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
        terminal_result: outcome.terminal_result,
        error_message: outcome.error_message,
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

fn run_fingerprint(session_id: &SessionId, text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentlibre.daemon.run_submit.v1\0");
    hasher.update(session_id.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(text.as_bytes());
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
