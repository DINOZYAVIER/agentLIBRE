use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agl_ids::{AttemptId, RequestId, SessionId};
use agl_loop::{EffectOutcome, TurnEffect, TurnEffectResult};
use agl_store::{AglStore, DurableRunRecord, EffectDeliveryClass, RunState, RunUsage};
use agl_supervisor::{
    DriverEffectError, DriverSnapshot, DurableRunDriver, DurableRunDriverFactory,
    EffectExecutionContext, Result as SupervisorResult, RunCancellation, SupervisorEffect,
    SupervisorError, SupervisorTerminal,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{ChatService, ChatTurnExecution, ChatTurnStatus};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatRunInput {
    pub text: String,
    pub request_id: Option<RequestId>,
    pub options: crate::ChatOptions,
}

#[derive(Clone)]
pub struct ChatSupervisorFactory {
    store_root: PathBuf,
    services: Arc<Mutex<BTreeMap<SessionId, ChatService>>>,
    runtime: Option<agl_runtime::AgentLibreRuntimeConfig>,
    inference_client: Option<crate::InferenceClientHandle>,
}

impl ChatSupervisorFactory {
    pub fn new(store_root: impl AsRef<Path>) -> Self {
        Self {
            store_root: store_root.as_ref().to_path_buf(),
            services: Arc::new(Mutex::new(BTreeMap::new())),
            runtime: None,
            inference_client: None,
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
            runtime: Some(runtime),
            inference_client: Some(inference_client),
        }
    }

    pub fn register(&self, service: ChatService) -> Result<()> {
        let session_id = service.session_id().clone();
        let mut services = self.services.lock().map_err(|error| {
            anyhow::anyhow!("chat supervisor service pool is poisoned: {error}")
        })?;
        if services.contains_key(&session_id) {
            bail!("chat session {session_id} is already registered");
        }
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
        self.services
            .lock()
            .map_err(|error| anyhow::anyhow!("chat supervisor service pool is poisoned: {error}"))
            .map(|mut services| services.remove(session_id))
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
        operation(service)
    }
}

impl DurableRunDriverFactory for ChatSupervisorFactory {
    fn open(
        &self,
        run: &DurableRunRecord,
        cancellation: RunCancellation,
    ) -> SupervisorResult<Box<dyn DurableRunDriver>> {
        let session_id = run
            .session_id
            .clone()
            .ok_or_else(|| SupervisorError::Driver("chat runs require a session ID".to_string()))?;
        let turn_id = run
            .turn_id
            .clone()
            .ok_or_else(|| SupervisorError::Driver("chat runs require a turn ID".to_string()))?;
        let input: ChatRunInput = serde_json::from_value(run.input.clone())?;
        let mut service = self
            .services
            .lock()
            .map_err(|error| SupervisorError::Driver(format!("chat service pool poisoned: {error}")))?
            .remove(&session_id)
            .map(Ok)
            .unwrap_or_else(|| {
                let runtime = self.runtime.as_ref().ok_or_else(|| {
                    SupervisorError::Driver(format!(
                        "chat session {session_id} is not registered and no recovery runtime is configured"
                    ))
                })?;
                let inference_client = self.inference_client.clone().ok_or_else(|| {
                    SupervisorError::Driver("chat recovery inference client is missing".to_string())
                })?;
                let mut options = input.options.clone();
                options.session_id = Some(session_id.clone());
                options.new_session = false;
                ChatService::open(options, runtime, inference_client)
                    .map_err(|error| SupervisorError::Driver(format!("{error:#}")))
            })?;

        let execution = if let Some(checkpoint) = &run.checkpoint {
            let checkpoint = serde_json::from_value(checkpoint.clone())?;
            let store = AglStore::open_current_at(&self.store_root)?;
            let event_sequence = store.latest_run_event_sequence(&run.run_id)?;
            let attempt_ids = durable_attempt_ids(&store, &run.run_id)?;
            service
                .resume_user_turn_from_checkpoint(
                    run.run_id.clone(),
                    turn_id,
                    input.request_id,
                    checkpoint,
                    event_sequence,
                    attempt_ids,
                )
                .map_err(|error| SupervisorError::Driver(format!("{error:#}")))?
        } else {
            service
                .start_user_turn_with_ids(
                    run.run_id.clone(),
                    turn_id,
                    input.request_id,
                    &input.text,
                )
                .map_err(|error| SupervisorError::Driver(format!("{error:#}")))?
        };
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
        let pending_effect = self
            .execution
            .pending_effect()
            .map(|effect| -> SupervisorResult<SupervisorEffect> {
                Ok(SupervisorEffect {
                    sequence: effect.key().sequence,
                    kind: effect.kind().as_str().to_string(),
                    delivery_class: effect_delivery_class(
                        self.service
                            .as_ref()
                            .expect("chat driver retains its service"),
                        effect,
                    )?,
                    request: serde_json::to_value(effect)?,
                })
            })
            .transpose()?;
        let checkpoint = serde_json::to_value(self.execution.checkpoint())?;
        Ok(DriverSnapshot {
            checkpoint,
            pending_effect,
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
        let pending_kind = self.execution.pending_effect().map(TurnEffect::kind);
        let tokens_before = service.model_token_usage();
        if !self.cancellation.is_cancelled() {
            match pending_kind {
                Some(agl_loop::TurnEffectKind::ModelGeneration) => {
                    self.usage.model_attempts = self.usage.model_attempts.saturating_add(1);
                }
                Some(agl_loop::TurnEffectKind::CapabilityDispatch) => {
                    self.usage.capability_calls = self.usage.capability_calls.saturating_add(1);
                }
                Some(
                    agl_loop::TurnEffectKind::HookBatch
                    | agl_loop::TurnEffectKind::TranscriptAppend,
                )
                | None => {}
            }
        }
        let result = service
            .execute_user_turn_effect_with_step(&mut self.execution, Some(&context.step_id))
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
            .resume_user_turn_effect(&mut self.execution, result)
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
        let session_id = service.session_id().clone();
        if let Ok(mut pool) = self.pool.lock() {
            pool.insert(session_id, service);
        }
    }
}

fn effect_delivery_class(
    service: &ChatService,
    effect: &TurnEffect,
) -> SupervisorResult<EffectDeliveryClass> {
    match effect {
        TurnEffect::HookBatch { .. }
        | TurnEffect::ModelGeneration { .. }
        | TurnEffect::TranscriptAppend { .. } => Ok(EffectDeliveryClass::ReplaySafe),
        TurnEffect::CapabilityDispatch { request, .. } => service
            .capability_delivery_class(&request.capability_id)
            .map_err(|error| SupervisorError::Driver(format!("{error:#}"))),
    }
}

fn retryable_failure(result: &TurnEffectResult) -> Option<&agl_loop::EffectFailure> {
    match result {
        TurnEffectResult::HookBatch {
            outcome: EffectOutcome::Failed(failure),
            ..
        }
        | TurnEffectResult::ModelGeneration {
            outcome: EffectOutcome::Failed(failure),
            ..
        }
        | TurnEffectResult::CapabilityDispatch {
            outcome: EffectOutcome::Failed(failure),
            ..
        }
        | TurnEffectResult::TranscriptAppend {
            outcome: EffectOutcome::Failed(failure),
            ..
        } if failure.retryable => Some(failure),
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
