use std::path::{Path, PathBuf};
use std::sync::Arc;

use agl_ids::{AttemptId, RunId, SessionId, TurnId};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_store::{DurableRunDraft, RunBudget, RunKind, RunState};
use agl_supervisor::{RunSpec, Supervisor, SupervisorHandle, SupervisorOptions};
use anyhow::{Context, Result, bail};

use crate::{
    ChatOptions, ChatRunInput, ChatService, ChatSessionSummary, ChatSupervisorFactory,
    ChatTurnOutput, ChatTurnStatus, InferenceClientHandle,
};

pub struct SupervisedChat {
    supervisor: Supervisor,
    handle: SupervisorHandle,
    factory: ChatSupervisorFactory,
    session_id: SessionId,
    options: ChatOptions,
}

impl SupervisedChat {
    pub fn open(
        options: ChatOptions,
        runtime: &AgentLibreRuntimeConfig,
        inference_client: InferenceClientHandle,
    ) -> Result<Self> {
        let service = ChatService::open(options.clone(), runtime, inference_client.clone())?;
        let session_id = service.session_id().clone();
        let persisted_options = ChatOptions {
            session_id: Some(session_id.clone()),
            new_session: false,
            ..options
        };
        let store_root = runtime.paths.store_root();
        let factory =
            ChatSupervisorFactory::with_runtime(&store_root, runtime.clone(), inference_client);
        factory.register(service)?;
        let supervisor = Supervisor::spawn(
            &store_root,
            Arc::new(factory.clone()),
            SupervisorOptions::default(),
        )?;
        let handle = supervisor.handle();
        Ok(Self {
            supervisor,
            handle,
            factory,
            session_id,
            options: persisted_options,
        })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn summary(&self) -> Result<ChatSessionSummary> {
        self.factory
            .with_session(&self.session_id, |service| Ok(service.summary()))
    }

    pub fn workspace_root(&self) -> Result<PathBuf> {
        self.factory.with_session(&self.session_id, |service| {
            Ok(service.workspace_root().to_path_buf())
        })
    }

    pub fn artifact_root(&self) -> Result<PathBuf> {
        self.factory.with_session(&self.session_id, |service| {
            Ok(service.artifact_root().to_path_buf())
        })
    }

    pub fn reload_runtime_context(&self) -> Result<usize> {
        self.factory
            .with_session(&self.session_id, |service| service.reload_runtime_context())
    }

    pub fn set_workspace_root(&mut self, workspace_root: impl AsRef<Path>) -> Result<()> {
        let workspace_root = workspace_root.as_ref().to_path_buf();
        self.factory.with_session(&self.session_id, |service| {
            service.set_workspace_root(&workspace_root)
        })?;
        self.options.workspace_root = Some(workspace_root.clone());
        self.options.inference.workspace_root = Some(workspace_root);
        Ok(())
    }

    pub fn clear_context(&self) -> Result<usize> {
        self.factory
            .with_session(&self.session_id, ChatService::clear_context)
    }

    pub fn request_exit(&self) -> Result<()> {
        self.factory
            .with_session(&self.session_id, ChatService::request_exit)
    }

    pub fn finish_eof_if_needed(&self) -> Result<()> {
        self.factory
            .with_session(&self.session_id, ChatService::finish_eof_if_needed)
    }

    pub fn run_user_turn(&self, input: &str) -> Result<ChatTurnOutput> {
        let run_id = RunId::generate();
        let turn_id = TurnId::generate();
        self.handle.submit(RunSpec {
            run: DurableRunDraft {
                run_id: run_id.clone(),
                session_id: Some(self.session_id.clone()),
                turn_id: Some(turn_id.clone()),
                kind: RunKind::Turn,
                priority: 0,
                input: serde_json::to_value(ChatRunInput {
                    text: input.to_string(),
                    request_id: None,
                    options: self.options.clone(),
                })?,
                checkpoint: None,
                effective_policy_hash: None,
                budget: RunBudget::default(),
                not_before_ms: None,
            },
            idempotency: None,
        })?;
        let mut subscription = self.handle.subscribe(run_id.clone(), 0)?;
        let mut runtime_events = std::mem::take(&mut subscription.backlog);
        while let Some(event) = subscription.recv()? {
            runtime_events.push(event);
        }
        let outcome = self
            .handle
            .outcome(run_id.clone())?
            .context("admitted process-local run disappeared")?;
        let generated_requests =
            usize::try_from(outcome.status.usage.model_attempts).unwrap_or(usize::MAX);
        let result = outcome.terminal_result.unwrap_or(serde_json::Value::Null);
        let attempt_ids = result
            .get("attempt_ids")
            .cloned()
            .map(serde_json::from_value::<Vec<AttemptId>>)
            .transpose()?
            .unwrap_or_default();
        let status = match outcome.status.state {
            RunState::Succeeded => match result.get("status").and_then(serde_json::Value::as_str) {
                Some("answered") => ChatTurnStatus::Answered {
                    answer: result
                        .get("answer")
                        .and_then(serde_json::Value::as_str)
                        .context("answered run has no answer")?
                        .to_string(),
                },
                Some("stopped") => ChatTurnStatus::Stopped {
                    reason: serde_json::from_value(
                        result
                            .get("reason")
                            .cloned()
                            .context("stopped run has no reason")?,
                    )?,
                },
                other => bail!("unknown successful run result status {other:?}"),
            },
            RunState::Failed => ChatTurnStatus::Failed {
                message: outcome
                    .error_message
                    .or(outcome.status.error_code)
                    .unwrap_or_else(|| "durable turn failed".to_string()),
            },
            RunState::Cancelled => ChatTurnStatus::Cancelled,
            RunState::Queued | RunState::Running | RunState::Waiting => {
                bail!("run subscription ended before terminal state")
            }
        };
        Ok(ChatTurnOutput {
            run_id,
            turn_id,
            attempt_ids,
            runtime_events,
            status,
            generated_requests,
        })
    }

    pub fn shutdown(self) -> Result<()> {
        self.supervisor.shutdown()?;
        Ok(())
    }
}
