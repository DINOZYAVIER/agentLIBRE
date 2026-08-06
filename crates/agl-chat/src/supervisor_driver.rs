use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agl_config::ResolvedInferenceConfig;
use agl_content::Content;
use agl_function::RuntimeDelegationPlan;
use agl_ids::{AttemptId, MessageId, RequestId, SessionId};
use agl_kernel::ToolId;
use agl_kernel::{TurnRequest, TurnRequestOutcome, TurnRequestResult};
use agl_store::{AglStore, DurableRunRecord, EffectDeliveryClass, RunKind, RunState, RunUsage};
use agl_supervisor::{
    DriverEffectError, DriverSnapshot, DurableRunDriver, DurableRunDriverFactory,
    EffectExecutionContext, Result as SupervisorResult, RunCancellation, SupervisorEffect,
    SupervisorError, SupervisorTerminal,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{ChatService, ChatTurnExecution, ChatTurnStatus, service::DurableTurnResume};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "run_kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChatRunInput {
    Root {
        content: Content,
        request_id: Option<RequestId>,
        options: crate::ChatOptions,
        delegation_plan: Option<RuntimeDelegationPlan>,
    },
    Continuation {
        source_message_id: MessageId,
        continuation_index: u16,
        request_id: Option<RequestId>,
        options: crate::ChatOptions,
        delegation_plan: Option<RuntimeDelegationPlan>,
    },
    Subagent {
        task: Content,
        execution_session_id: SessionId,
        execution_turn_id: agl_ids::TurnId,
        workspace_root: PathBuf,
        artifact_root: PathBuf,
        inference_config: ResolvedInferenceConfig,
        delegation_plan: RuntimeDelegationPlan,
        authority_ceiling: BTreeSet<ToolId>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatDriverCheckpoint {
    turn: agl_kernel::TurnCheckpoint,
    effective_policy_hash: String,
    delegation_authority_ceiling: BTreeSet<ToolId>,
}

#[derive(Clone)]
pub struct ChatSupervisorFactory {
    store_root: PathBuf,
    services: Arc<Mutex<BTreeMap<SessionId, ChatService>>>,
    policy_hashes: Arc<Mutex<BTreeMap<SessionId, String>>>,
    runtime: Option<agl_runtime::AgentLibreRuntimeConfig>,
    inference_client: Option<crate::InferenceClientHandle>,
    presentation_sink: Arc<dyn crate::TurnPresentationSink>,
}

impl ChatSupervisorFactory {
    pub fn new(store_root: impl AsRef<Path>) -> Self {
        Self {
            store_root: store_root.as_ref().to_path_buf(),
            services: Arc::new(Mutex::new(BTreeMap::new())),
            policy_hashes: Arc::new(Mutex::new(BTreeMap::new())),
            runtime: None,
            inference_client: None,
            presentation_sink: Arc::new(crate::NoopTurnPresentationSink),
        }
    }

    pub fn with_runtime(
        store_root: impl AsRef<Path>,
        runtime: agl_runtime::AgentLibreRuntimeConfig,
        inference_client: crate::InferenceClientHandle,
    ) -> Self {
        Self {
            store_root: store_root.as_ref().to_path_buf(),
            services: Arc::new(Mutex::new(BTreeMap::new())),
            policy_hashes: Arc::new(Mutex::new(BTreeMap::new())),
            runtime: Some(runtime),
            inference_client: Some(inference_client),
            presentation_sink: Arc::new(crate::NoopTurnPresentationSink),
        }
    }

    pub fn with_presentation_sink(mut self, sink: Arc<dyn crate::TurnPresentationSink>) -> Self {
        self.presentation_sink = sink;
        self
    }

    pub fn register(&self, service: ChatService) -> Result<()> {
        let session_id = service.session_id().clone();
        let mut services = self.services.lock().map_err(|error| {
            anyhow::anyhow!("chat supervisor service pool is poisoned: {error}")
        })?;
        if services.contains_key(&session_id) {
            bail!("chat session {session_id} is already registered");
        }
        self.policy_hashes
            .lock()
            .map_err(|error| anyhow::anyhow!("chat policy cache is poisoned: {error}"))?
            .insert(session_id.clone(), service.effective_policy_hash());
        services.insert(session_id, service);
        Ok(())
    }

    pub fn has_session(&self, session_id: &SessionId) -> bool {
        self.services
            .lock()
            .map(|services| services.contains_key(session_id))
            .unwrap_or(false)
    }

    pub fn unregister(&self, session_id: &SessionId) -> Result<Option<ChatService>> {
        let removed = self
            .services
            .lock()
            .map_err(|error| anyhow::anyhow!("chat supervisor service pool is poisoned: {error}"))
            .map(|mut services| services.remove(session_id))?;
        self.policy_hashes
            .lock()
            .map_err(|error| anyhow::anyhow!("chat policy cache is poisoned: {error}"))?
            .remove(session_id);
        Ok(removed)
    }

    pub fn with_session<T>(
        &self,
        session_id: &SessionId,
        operation: impl FnOnce(&mut ChatService) -> Result<T>,
    ) -> Result<T> {
        let mut services = self.services.lock().map_err(|error| {
            anyhow::anyhow!("chat supervisor service pool is poisoned: {error}")
        })?;
        let service = services
            .get_mut(session_id)
            .with_context(|| format!("chat session {session_id} is busy or not registered"))?;
        let result = operation(service);
        let policy_hash = service.effective_policy_hash();
        self.policy_hashes
            .lock()
            .map_err(|error| anyhow::anyhow!("chat policy cache is poisoned: {error}"))?
            .insert(session_id.clone(), policy_hash);
        result
    }

    pub fn current_policy_hash(&self, session_id: &SessionId) -> Result<Option<String>> {
        self.policy_hashes
            .lock()
            .map_err(|error| anyhow::anyhow!("chat policy cache is poisoned: {error}"))
            .map(|policies| policies.get(session_id).cloned())
    }
}

impl DurableRunDriverFactory for ChatSupervisorFactory {
    fn open(
        &self,
        run: &DurableRunRecord,
        cancellation: RunCancellation,
    ) -> SupervisorResult<Box<dyn DurableRunDriver>> {
        let input: ChatRunInput = serde_json::from_value(run.input.clone())?;
        let child_presentation_context = if run.kind == RunKind::Subagent {
            let parent_run_id = run.parent_run_id.clone().ok_or_else(|| {
                SupervisorError::Driver("subagent run has no parent run ID".to_string())
            })?;
            let spawned_by_step_id = run.spawned_by_step_id.clone().ok_or_else(|| {
                SupervisorError::Driver("subagent run has no spawning step ID".to_string())
            })?;
            let subagent_id = run.subagent_id.clone().ok_or_else(|| {
                SupervisorError::Driver("subagent run has no subagent ID".to_string())
            })?;
            let store = AglStore::open_current_at(&self.store_root)?;
            let root = store.run(&run.root_run_id)?.ok_or_else(|| {
                SupervisorError::Driver(format!(
                    "subagent root run {} does not exist",
                    run.root_run_id
                ))
            })?;
            let session_id = root.session_id.ok_or_else(|| {
                SupervisorError::Driver(format!(
                    "subagent root run {} has no Human session",
                    run.root_run_id
                ))
            })?;
            Some((
                session_id,
                crate::ChildRunPresentation {
                    parent_run_id,
                    spawned_by_step_id,
                    subagent_id,
                },
            ))
        } else {
            None
        };
        let open_root_service = |session_id: &SessionId,
                                 mut options: crate::ChatOptions,
                                 delegation_plan: Option<RuntimeDelegationPlan>|
         -> SupervisorResult<ChatService> {
            let mut service = self
                .services
                .lock()
                .map_err(|error| {
                    SupervisorError::Driver(format!("chat service pool poisoned: {error}"))
                })?
                .remove(session_id)
                .map(Ok)
                .unwrap_or_else(|| {
                    let runtime = self.runtime.as_ref().ok_or_else(|| {
                        SupervisorError::Driver(format!(
                            "chat session {session_id} is not registered and no recovery runtime is configured"
                        ))
                    })?;
                    let inference_client = self.inference_client.clone().ok_or_else(|| {
                        SupervisorError::Driver(
                            "chat recovery inference client is missing".to_string(),
                        )
                    })?;
                    options.session_id = Some(session_id.clone());
                    options.new_session = false;
                    ChatService::open(options, runtime, inference_client)
                        .map_err(|error| SupervisorError::Driver(format!("{error:#}")))
                })?;
            service.install_root_delegation_plan(delegation_plan);
            Ok(service)
        };
        let (
            turn_id,
            request_id,
            content,
            mut service,
            expected_policy_hash,
            continuation_index,
            internal_continuation,
            continuation_source_message_id,
        ) = match input {
            ChatRunInput::Root {
                content,
                request_id,
                options,
                delegation_plan,
            } => {
                if run.kind == RunKind::Subagent {
                    return Err(SupervisorError::Driver(
                        "subagent run cannot use root chat input".to_string(),
                    ));
                }
                let session_id = run.session_id.clone().ok_or_else(|| {
                    SupervisorError::Driver("root chat runs require a session ID".to_string())
                })?;
                let turn_id = run.turn_id.clone().ok_or_else(|| {
                    SupervisorError::Driver("root chat runs require a turn ID".to_string())
                })?;
                let service = open_root_service(&session_id, options, delegation_plan)?;
                (turn_id, request_id, content, service, None, 0, false, None)
            }
            ChatRunInput::Continuation {
                source_message_id,
                continuation_index,
                request_id,
                options,
                delegation_plan,
            } => {
                if run.kind == RunKind::Subagent {
                    return Err(SupervisorError::Driver(
                        "subagent run cannot use continuation chat input".to_string(),
                    ));
                }
                let session_id = run.session_id.clone().ok_or_else(|| {
                    SupervisorError::Driver("continuation runs require a session ID".to_string())
                })?;
                let turn_id = run.turn_id.clone().ok_or_else(|| {
                    SupervisorError::Driver("continuation runs require a turn ID".to_string())
                })?;
                let service = open_root_service(&session_id, options, delegation_plan)?;
                (
                    turn_id,
                    request_id,
                    Content::text("internal continuation")
                        .map_err(|error| SupervisorError::Driver(error.to_string()))?,
                    service,
                    run.effective_policy_hash.clone(),
                    continuation_index,
                    true,
                    Some(source_message_id),
                )
            }
            ChatRunInput::Subagent {
                task,
                execution_session_id,
                execution_turn_id,
                workspace_root,
                artifact_root,
                inference_config,
                delegation_plan,
                authority_ceiling,
            } => {
                if run.kind != RunKind::Subagent
                    || run.session_id.is_some()
                    || run.turn_id.is_some()
                {
                    return Err(SupervisorError::Driver(
                        "subagent chat input requires a sessionless child run".to_string(),
                    ));
                }
                let subagent_id = run.subagent_id.as_deref().ok_or_else(|| {
                    SupervisorError::Driver("child run has no subagent ID".to_string())
                })?;
                let spec = delegation_plan
                    .subagent_specs
                    .get(subagent_id)
                    .ok_or_else(|| {
                        SupervisorError::Driver(format!(
                            "persisted delegation plan has no subagent `{subagent_id}`"
                        ))
                    })?;
                if run.child_spec_digest.as_deref() != Some(spec.spec_digest.as_str()) {
                    return Err(SupervisorError::Driver(
                        "child specification digest differs from its admitted snapshot".to_string(),
                    ));
                }
                let config_digest =
                    crate::delegation::inference_config_digest(&inference_config)
                        .map_err(|error| SupervisorError::Driver(format!("{error:#}")))?;
                if run.model_profile_digest.as_deref() != Some(config_digest.as_str()) {
                    return Err(SupervisorError::Driver(
                        "child model profile digest differs from its admitted snapshot".to_string(),
                    ));
                }
                let spec = spec.clone();
                let service = self
                    .services
                    .lock()
                    .map_err(|error| {
                        SupervisorError::Driver(format!("chat service pool poisoned: {error}"))
                    })?
                    .remove(&execution_session_id)
                    .map(Ok)
                    .unwrap_or_else(|| {
                        let runtime = self.runtime.as_ref().ok_or_else(|| {
                            SupervisorError::Driver(
                                "subagent recovery runtime is not configured".to_string(),
                            )
                        })?;
                        let inference_client = self.inference_client.clone().ok_or_else(|| {
                            SupervisorError::Driver(
                                "subagent recovery inference client is missing".to_string(),
                            )
                        })?;
                        ChatService::open_subagent(
                            crate::session::SubagentSessionConfig {
                                inference_config,
                                spec,
                                delegation_plan,
                                authority_ceiling,
                                artifact_root,
                                workspace_root,
                                execution_session_id,
                            },
                            runtime,
                            inference_client,
                        )
                        .map_err(|error| SupervisorError::Driver(format!("{error:#}")))
                    })?;
                (
                    execution_turn_id,
                    None,
                    task,
                    service,
                    run.effective_policy_hash.clone(),
                    0,
                    false,
                    None,
                )
            }
        };

        service.set_presentation_sink(Arc::clone(&self.presentation_sink));
        if let Some((session_id, child_run)) = child_presentation_context {
            service.set_presentation_context(session_id, child_run);
        }

        if run.session_id.is_none() {
            service
                .install_run_execution_context(run.execution_context.clone())
                .map_err(|error| SupervisorError::Driver(format!("{error:#}")))?;
        }

        let (mut execution, checkpoint_policy_hash) = if let Some(checkpoint) = &run.checkpoint {
            let checkpoint: ChatDriverCheckpoint = serde_json::from_value(checkpoint.clone())?;
            let store = AglStore::open_current_at(&self.store_root)?;
            let event_sequence = store.latest_run_event_sequence(&run.run_id)?;
            let attempt_ids = durable_attempt_ids(&store, &run.run_id)?;
            let execution = service
                .resume_user_turn_from_checkpoint(DurableTurnResume {
                    run_id: run.run_id.clone(),
                    turn_id,
                    request_id,
                    checkpoint: checkpoint.turn,
                    event_sequence,
                    attempt_ids,
                    delegation_authority_ceiling: checkpoint.delegation_authority_ceiling,
                    continuation_index,
                    internal_continuation,
                    continuation_source_message_id: continuation_source_message_id.clone(),
                })
                .map_err(|error| SupervisorError::Driver(format!("{error:#}")))?;
            (execution, Some(checkpoint.effective_policy_hash))
        } else {
            let execution = if internal_continuation {
                continuation_source_message_id
                    .clone()
                    .context("internal continuation is missing its source message ID")
                    .and_then(|source_message_id| {
                        service.start_incomplete_continuation_with_ids(
                            run.run_id.clone(),
                            turn_id,
                            request_id,
                            source_message_id,
                            continuation_index,
                        )
                    })
            } else {
                service.start_user_turn_with_ids(run.run_id.clone(), turn_id, request_id, content)
            }
            .map_err(|error| SupervisorError::Driver(format!("{error:#}")))?;
            (execution, None)
        };
        let remaining_wall_time_ms = run
            .budget
            .wall_time_ms
            .saturating_sub(run.usage.wall_time_ms);
        execution
            .set_deadline(Instant::now() + Duration::from_millis(remaining_wall_time_ms.max(1)));
        for (expected_policy_hash, label) in [
            (expected_policy_hash.as_deref(), "admitted subagent"),
            (checkpoint_policy_hash.as_deref(), "durable checkpoint"),
        ] {
            if let Some(expected_policy_hash) = expected_policy_hash
                && service.effective_policy_hash() != expected_policy_hash
            {
                return Err(SupervisorError::Driver(format!(
                    "effective tool policy differs from the {label} snapshot"
                )));
            }
        }
        if run.session_id.is_some() {
            self.policy_hashes
                .lock()
                .map_err(|error| {
                    SupervisorError::Driver(format!("chat policy cache poisoned: {error}"))
                })?
                .insert(
                    service.session_id().clone(),
                    service.effective_policy_hash(),
                );
        }
        let inference_cancellation = execution.cancellation_handle();
        let bridge_finished = Arc::new(AtomicBool::new(false));
        let watcher_finished = bridge_finished.clone();
        let watcher_cancellation = cancellation.clone();
        std::thread::Builder::new()
            .name(format!("agl-chat-cancel-{}", run.run_id))
            .spawn(move || {
                while !watcher_finished.load(Ordering::Acquire) {
                    if watcher_cancellation.is_cancelled() {
                        inference_cancellation.cancel();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
            .map_err(|error| SupervisorError::Driver(error.to_string()))?;

        Ok(Box::new(ChatSupervisorDriver {
            pool: self.services.clone(),
            policy_hashes: self.policy_hashes.clone(),
            service: Some(service),
            execution,
            cancellation,
            bridge_finished,
            terminal: None,
            usage: run.usage.clone(),
        }))
    }
}

struct ChatSupervisorDriver {
    pool: Arc<Mutex<BTreeMap<SessionId, ChatService>>>,
    policy_hashes: Arc<Mutex<BTreeMap<SessionId, String>>>,
    service: Option<ChatService>,
    execution: ChatTurnExecution,
    cancellation: RunCancellation,
    bridge_finished: Arc<AtomicBool>,
    terminal: Option<SupervisorTerminal>,
    usage: RunUsage,
}

impl DurableRunDriver for ChatSupervisorDriver {
    fn snapshot(&mut self) -> SupervisorResult<DriverSnapshot> {
        if self.execution.is_terminal() && self.terminal.is_none() {
            let output = self.execution.take_output().ok_or_else(|| {
                SupervisorError::Driver("terminal chat execution has no output".to_string())
            })?;
            self.terminal = Some(match output.status {
                ChatTurnStatus::Answered { answer } => SupervisorTerminal {
                    state: RunState::Succeeded,
                    result: Some(serde_json::json!({
                        "status": "answered",
                        "answer": answer,
                        "attempt_ids": output.attempt_ids,
                    })),
                    error_code: None,
                    error_message: None,
                },
                ChatTurnStatus::Incomplete { partial, reason } => SupervisorTerminal {
                    state: RunState::Incomplete,
                    result: Some(serde_json::json!({
                        "status": "incomplete_output",
                        "partial": partial,
                        "reason": reason,
                        "attempt_ids": output.attempt_ids,
                    })),
                    error_code: None,
                    error_message: None,
                },
                ChatTurnStatus::Stopped { reason } => SupervisorTerminal {
                    state: RunState::Succeeded,
                    result: Some(serde_json::json!({
                        "status": "stopped",
                        "reason": reason,
                        "attempt_ids": output.attempt_ids,
                    })),
                    error_code: None,
                    error_message: None,
                },
                ChatTurnStatus::Failed { message } => SupervisorTerminal {
                    state: RunState::Failed,
                    result: None,
                    error_code: Some("chat_turn_failed".to_string()),
                    error_message: Some(message),
                },
                ChatTurnStatus::Cancelled => SupervisorTerminal {
                    state: RunState::Cancelled,
                    result: None,
                    error_code: None,
                    error_message: None,
                },
            });
        }
        let pending_request = self
            .execution
            .pending_request()
            .map(|request| -> SupervisorResult<SupervisorEffect> {
                Ok(SupervisorEffect {
                    sequence: request.key().sequence,
                    kind: request.kind().as_str().to_string(),
                    delivery_class: effect_delivery_class(
                        self.service
                            .as_ref()
                            .expect("chat driver retains its service"),
                        request,
                    )?,
                    request: serde_json::to_value(request)?,
                })
            })
            .transpose()?;
        let checkpoint = serde_json::to_value(ChatDriverCheckpoint {
            turn: self.execution.checkpoint(),
            effective_policy_hash: self
                .service
                .as_ref()
                .expect("chat driver retains its service")
                .effective_policy_hash(),
            delegation_authority_ceiling: self
                .service
                .as_ref()
                .expect("chat driver retains its service")
                .delegation_authority_ceiling()
                .clone(),
        })?;
        Ok(DriverSnapshot {
            checkpoint,
            pending_request,
            events: self.execution.take_events(),
            terminal: self.terminal.clone(),
            usage: self.usage.clone(),
        })
    }

    fn execute_pending_effect(
        &mut self,
        context: &EffectExecutionContext,
    ) -> std::result::Result<serde_json::Value, DriverEffectError> {
        if self.cancellation.is_cancelled() {
            self.execution
                .request_cancellation()
                .map_err(|error| DriverEffectError::new("turn.cancel", error.to_string(), false))?;
        }
        let service = self
            .service
            .as_mut()
            .expect("chat driver retains its service");
        let pending_kind = self.execution.pending_request().map(TurnRequest::kind);
        let pending_is_delegation = matches!(
            self.execution.pending_request(),
            Some(TurnRequest::ToolDispatch { request, .. })
                if request.tool_id.as_str()
                    == crate::delegation_contract::AGENT_DELEGATE_TOOL_ID
        );
        let tokens_before = service.model_token_usage();
        if !self.cancellation.is_cancelled() {
            match pending_kind {
                Some(agl_kernel::TurnRequestKind::ModelGeneration) => {
                    self.usage.model_attempts = self.usage.model_attempts.saturating_add(1);
                }
                Some(agl_kernel::TurnRequestKind::ToolDispatch) => {
                    if !pending_is_delegation || context.attempt == 1 {
                        self.usage.tool_calls = self.usage.tool_calls.saturating_add(1);
                    }
                }
                Some(
                    agl_kernel::TurnRequestKind::HookBatch
                    | agl_kernel::TurnRequestKind::TranscriptAppend,
                )
                | None => {}
            }
        }
        let result = service
            .execute_user_turn_request_with_step(&mut self.execution, Some(&context.step_id))
            .map_err(|error| {
                DriverEffectError::new("chat.effect_execute", format!("{error:#}"), true)
            })?;
        let tokens_after = service.model_token_usage();
        self.usage.model_input_tokens = self
            .usage
            .model_input_tokens
            .saturating_add(tokens_after.0.saturating_sub(tokens_before.0));
        self.usage.model_output_tokens = self
            .usage
            .model_output_tokens
            .saturating_add(tokens_after.1.saturating_sub(tokens_before.1));
        if pending_is_delegation && crate::delegation::result_is_waiting(&result) {
            return Err(DriverEffectError::durable_wait(
                "delegation.child_waiting",
                "delegated child run has not reached a terminal state",
            ));
        }
        if let Some(failure) = retryable_failure(&result) {
            return Err(DriverEffectError::new(
                failure.code.as_str(),
                failure.message.clone(),
                true,
            ));
        }
        let evidence = serde_json::to_value(&result).map_err(|error| {
            DriverEffectError::new("chat.effect_evidence", error.to_string(), false)
        })?;
        service
            .resume_user_turn_request(&mut self.execution, result)
            .map_err(|error| {
                DriverEffectError::new("chat.effect_resume", format!("{error:#}"), false)
            })?;
        Ok(evidence)
    }
}

impl Drop for ChatSupervisorDriver {
    fn drop(&mut self) {
        self.bridge_finished.store(true, Ordering::Release);
        let Some(service) = self.service.take() else {
            return;
        };
        let mut service = service;
        let terminal = self.execution.is_terminal();
        if !terminal {
            service.suspend_durable_turn();
        }
        if terminal && !service.is_session_scoped() {
            return;
        }
        let session_id = service.session_id().clone();
        if let Ok(mut policies) = self.policy_hashes.lock() {
            policies.insert(session_id.clone(), service.effective_policy_hash());
        }
        if let Ok(mut pool) = self.pool.lock() {
            pool.insert(session_id, service);
        }
    }
}

fn effect_delivery_class(
    service: &ChatService,
    request: &TurnRequest,
) -> SupervisorResult<EffectDeliveryClass> {
    match request {
        TurnRequest::HookBatch { .. }
        | TurnRequest::ModelGeneration { .. }
        | TurnRequest::TranscriptAppend { .. } => Ok(EffectDeliveryClass::ReplaySafe),
        TurnRequest::ToolDispatch { request, .. } => service
            .tool_delivery_class(&request.tool_id)
            .map_err(|error| SupervisorError::Driver(format!("{error:#}"))),
    }
}

fn retryable_failure(result: &TurnRequestResult) -> Option<&agl_kernel::TurnRequestFailure> {
    match result {
        TurnRequestResult::HookBatch {
            outcome: TurnRequestOutcome::Failed(failure),
            ..
        }
        | TurnRequestResult::ModelGeneration {
            outcome: TurnRequestOutcome::Failed(failure),
            ..
        }
        | TurnRequestResult::TranscriptAppend {
            outcome: TurnRequestOutcome::Failed(failure),
            ..
        } if failure.retryable => Some(failure),
        TurnRequestResult::ToolDispatch { outcome, .. } => match outcome.as_ref() {
            TurnRequestOutcome::Failed(failure) if failure.retryable => Some(failure),
            _ => None,
        },
        _ => None,
    }
}

fn durable_attempt_ids(
    store: &AglStore,
    run_id: &agl_ids::RunId,
) -> SupervisorResult<Vec<AttemptId>> {
    let mut after_sequence = 0;
    let mut attempt_ids = BTreeSet::new();
    loop {
        let events = store.run_events_after(run_id, after_sequence, 1_000)?;
        if events.is_empty() {
            break;
        }
        for event in &events {
            if let Some(attempt_id) = event.scope.attempt_id() {
                attempt_ids.insert(attempt_id.clone());
            }
        }
        after_sequence = events.last().expect("events are nonempty").sequence;
    }
    Ok(attempt_ids.into_iter().collect())
}
