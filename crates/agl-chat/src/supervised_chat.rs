use std::path::{Path, PathBuf};
use std::sync::Arc;

use agl_content::Content;
use agl_function::RuntimeDelegationPlan;
use agl_ids::{AttemptId, RunId, SessionId, TurnId};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_store::{DurableRunDraft, RunBudget, RunKind, RunState};
use agl_supervisor::{RunSpec, Supervisor, SupervisorHandle, SupervisorOptions};
use anyhow::{Context, Result, bail, ensure};

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
    delegation_plan: Option<RuntimeDelegationPlan>,
    execution_context: agl_exec::ExecutionContextSnapshot,
}

impl SupervisedChat {
    pub fn open(
        options: ChatOptions,
        runtime: &AgentLibreRuntimeConfig,
        inference_client: InferenceClientHandle,
    ) -> Result<Self> {
        Self::open_with_presentation_sink(
            options,
            runtime,
            inference_client,
            Arc::new(crate::NoopTurnPresentationSink),
        )
    }

    fn open_with_presentation_sink(
        options: ChatOptions,
        runtime: &AgentLibreRuntimeConfig,
        inference_client: InferenceClientHandle,
        presentation_sink: Arc<dyn crate::TurnPresentationSink>,
    ) -> Result<Self> {
        let service = ChatService::open(options.clone(), runtime, inference_client.clone())?;
        let session_id = service.session_id().clone();
        let execution_context = service.execution_context().clone();
        let delegation_plan = service.delegation_plan();
        let persisted_options = ChatOptions {
            session_id: Some(session_id.clone()),
            new_session: false,
            ..options
        };
        let store_root = runtime.paths.store_root();
        let factory =
            ChatSupervisorFactory::with_runtime(&store_root, runtime.clone(), inference_client)
                .with_presentation_sink(presentation_sink);
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
            delegation_plan,
            execution_context,
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

    pub fn execution_context(&self) -> Result<agl_exec::ExecutionContextSnapshot> {
        self.factory.with_session(&self.session_id, |service| {
            Ok(service.execution_context().clone())
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
        let execution_context = self.factory.with_session(&self.session_id, |service| {
            service.set_workspace_root(&workspace_root)?;
            Ok(service.execution_context().clone())
        })?;
        self.options.workspace_root = Some(workspace_root.clone());
        self.options.inference.workspace_root = Some(workspace_root);
        self.execution_context = execution_context;
        Ok(())
    }

    pub fn set_working_directory(
        &mut self,
        working_directory: impl AsRef<Path>,
        host: bool,
    ) -> Result<agl_exec::ExecutionContextSnapshot> {
        let snapshot = self.factory.with_session(&self.session_id, |service| {
            service.set_working_directory(working_directory, host)?;
            Ok(service.execution_context().clone())
        })?;
        self.execution_context = snapshot.clone();
        Ok(snapshot)
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
        self.run_user_turn_with_budget(input, RunBudget::default())
    }

    pub fn run_user_turn_with_budget(
        &self,
        input: &str,
        budget: RunBudget,
    ) -> Result<ChatTurnOutput> {
        let run_id = RunId::generate();
        let turn_id = TurnId::generate();
        let execution_context = self.execution_context()?;
        self.handle.submit(RunSpec {
            run: DurableRunDraft {
                run_id: run_id.clone(),
                session_id: Some(self.session_id.clone()),
                turn_id: Some(turn_id.clone()),
                kind: RunKind::Turn,
                priority: 0,
                concurrency_key: Some(agl_store::RunConcurrencyKey::session(&self.session_id)?),
                input: serde_json::to_value(ChatRunInput::Root {
                    content: Content::text(input)?,
                    request_id: None,
                    options: self.options.clone(),
                    delegation_plan: self.delegation_plan.clone(),
                })?,
                checkpoint: None,
                effective_policy_hash: None,
                execution_context,
                budget,
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
            RunState::Incomplete => {
                ensure!(
                    result.get("status").and_then(serde_json::Value::as_str)
                        == Some("incomplete_output"),
                    "incomplete run result has a non-incomplete status"
                );
                ChatTurnStatus::Incomplete {
                    partial: result
                        .get("partial")
                        .and_then(serde_json::Value::as_str)
                        .context("incomplete run has no partial output")?
                        .to_string(),
                    reason: serde_json::from_value(
                        result
                            .get("reason")
                            .cloned()
                            .context("incomplete run has no reason")?,
                    )?,
                }
            }
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use agl_config::ResolvedInferenceConfig;
    use agl_inference::{
        InferenceFinishReason, InferenceResponse, InferenceResponseMetadata, ModelManagerStatus,
    };

    use super::*;
    use crate::{ChatInferenceJob, InferenceClient, ToolAccessMode};

    #[derive(Clone, Default)]
    struct ScriptState {
        responses: Arc<Mutex<VecDeque<String>>>,
        jobs: Arc<Mutex<Vec<JobEvidence>>>,
        released_contexts: Arc<Mutex<Vec<SessionId>>>,
        grant_store_root: Option<PathBuf>,
        late_grant_id: Arc<Mutex<Option<String>>>,
    }

    #[derive(Clone, Debug)]
    struct JobEvidence {
        session_id: SessionId,
        model_path: PathBuf,
        rendered: String,
        tools: Vec<String>,
    }

    struct DelegationInferenceClient {
        state: ScriptState,
    }

    #[derive(Default)]
    struct RecordingPresentationSink {
        events: Mutex<Vec<crate::TurnPresentationEvent>>,
    }

    impl crate::TurnPresentationSink for RecordingPresentationSink {
        fn try_publish(&self, event: crate::TurnPresentationEvent) -> crate::PresentationDelivery {
            self.events.lock().unwrap().push(event);
            crate::PresentationDelivery::Delivered
        }
    }

    impl InferenceClient for DelegationInferenceClient {
        fn generate(&self, job: ChatInferenceJob) -> Result<InferenceResponse> {
            let job_count = {
                let mut jobs = self.state.jobs.lock().unwrap();
                jobs.push(JobEvidence {
                    session_id: job.session_id.clone(),
                    model_path: job.config.backend.model.clone(),
                    rendered: serde_json::to_string(&job.request.rendered)?,
                    tools: job
                        .request
                        .rendered
                        .tools
                        .iter()
                        .map(|tool| tool.name.clone())
                        .collect(),
                });
                jobs.len()
            };
            if let Some(store_root) = &self.state.grant_store_root {
                if job_count == 1 {
                    *self.state.late_grant_id.lock().unwrap() = Some(grant_cron_add(store_root));
                } else if job_count == 2
                    && let Some(grant_id) = self.state.late_grant_id.lock().unwrap().as_deref()
                {
                    agl_store::AglStore::open_current_at(store_root)
                        .unwrap()
                        .revoke_permission_grant(grant_id, Some("delegation-e2e-cleanup"))
                        .unwrap();
                }
            }
            let content = self
                .state
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .context("delegation test response queue is empty")?;
            Ok(InferenceResponse {
                attempt_id: job.request.attempt_id,
                content,
                finish_reason: InferenceFinishReason::Stop,
                metadata: InferenceResponseMetadata {
                    model_state: Some("scripted-delegation".to_string()),
                    selected_device: None,
                    duration_ms: 1,
                    input_tokens: 5,
                    output_tokens: 3,
                    resource_admission: None,
                },
            })
        }

        fn clear_context(
            &self,
            _config: &ResolvedInferenceConfig,
            _session_id: &SessionId,
        ) -> Result<()> {
            Ok(())
        }

        fn release_context(
            &self,
            _config: &ResolvedInferenceConfig,
            session_id: &SessionId,
        ) -> Result<()> {
            self.state
                .released_contexts
                .lock()
                .unwrap()
                .push(session_id.clone());
            Ok(())
        }

        fn status(&self) -> Result<ModelManagerStatus> {
            Ok(ModelManagerStatus::default())
        }
    }

    fn grant_cron_add(store_root: &Path) -> String {
        agl_store::AglStore::open_current_at(store_root)
            .unwrap()
            .create_permission_grant(agl_store::PermissionGrantDraft {
                request_id: None,
                tool_id: "cron.add".to_string(),
                max_operation_kind: "write".to_string(),
                state_effects: vec!["store_cron".to_string()],
                sensitive_inputs: Vec::new(),
                scope: serde_json::json!({}),
                duration: "one_turn".to_string(),
                granted_by_ref: "delegation-e2e".to_string(),
            })
            .unwrap()
            .id
    }

    #[test]
    fn supervised_delegation_isolated_child_resumes_parent_once() {
        let root =
            std::env::temp_dir().join(format!("agl-chat-delegation-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let function_root = root.join(".agl/functions/coordinator");
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
description: Delegates one review.
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
        let reviewer_function_root = root.join(".agl/functions/reviewer");
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
description: Reviews one bounded task.
---
"#,
        )
        .unwrap();
        std::fs::write(
            reviewer_function_root.join("SYSTEM.md"),
            "Review one bounded task.\n",
        )
        .unwrap();
        std::fs::write(
            function_root.join("SYSTEM.md"),
            "Delegate review work and use the returned verdict.\n",
        )
        .unwrap();
        std::fs::write(
            function_root.join("subagents/reviewer.md"),
            r#"---
schema: agentlibre/subagent/v1
id: reviewer
title: Reviewer
description: Reviews a bounded patch task.
model:
  profile: reviewer
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

Only return the child verdict.
"#,
        )
        .unwrap();
        let config_path = root.join("inference.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"[backend]
kind = "llama_cpp"
model = "{}"

[runtime]
gpu_layers = 0
context_tokens = 256
threads = 1
batch_size = 32
ubatch_size = 32

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#,
                root.join("missing.gguf").display()
            ),
        )
        .unwrap();
        let child_profile = root.join(".agl/inference/profiles/reviewer.toml");
        std::fs::create_dir_all(child_profile.parent().unwrap()).unwrap();
        std::fs::write(
            &child_profile,
            format!(
                r#"[backend]
kind = "llama_cpp"
model = "{}"

[runtime]
gpu_layers = 0
context_tokens = 256
threads = 1
batch_size = 32
ubatch_size = 32

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#,
                root.join("missing-child.gguf").display()
            ),
        )
        .unwrap();
        let runtime = AgentLibreRuntimeConfig {
            paths: agl_runtime::AgentLibrePaths::from_agl_home(root.join("home")),
            logging: agl_runtime::AgentLibreLoggingConfig::default(),
            history: agl_runtime::AgentLibreHistoryConfig::default(),
            workspace: agl_runtime::AgentLibreWorkspaceConfig::default(),
            inference: agl_runtime::AgentLibreInferenceConfig::default(),
            execution: agl_runtime::AgentLibreExecutionConfig::default(),
        };
        let state = ScriptState {
            responses: Arc::new(Mutex::new(VecDeque::from([
                r#"<tool_call>{"name":"agent.delegate","arguments":{"subagent_id":"reviewer","task":"Review patch CHILD_TASK_PRIVATE_SENTINEL"}}</tool_call>"#.to_string(),
                "Child verdict CHILD_VERDICT_PRIVATE_SENTINEL".to_string(),
                "Final parent answer".to_string(),
            ]))),
            jobs: Arc::new(Mutex::new(Vec::new())),
            released_contexts: Arc::new(Mutex::new(Vec::new())),
            grant_store_root: Some(runtime.paths.store_root()),
            late_grant_id: Arc::new(Mutex::new(None)),
        };
        let options = ChatOptions {
            inference: crate::InferenceOptions {
                config: Some(config_path),
                function_ref: Some("coordinator".to_string()),
                artifact_root: Some(root.join("artifacts")),
                workspace_root: Some(root.clone()),
                max_output_tokens: 128,
                tool_mode: ToolAccessMode::Execute,
                skills: Vec::new(),
                memory: false,
                model_bindings_path: None,
                model_bindings_override: None,
                runtime_plan_override: None,
            },
            workspace_root: Some(root.clone()),
            session_id: None,
            no_history: true,
            new_session: true,
        };
        let presentation = Arc::new(RecordingPresentationSink::default());
        let chat = SupervisedChat::open_with_presentation_sink(
            options,
            &runtime,
            InferenceClientHandle::new(DelegationInferenceClient {
                state: state.clone(),
            }),
            presentation.clone(),
        )
        .unwrap();

        let output = chat
            .run_user_turn("Parent secret phrase; obtain a review.")
            .unwrap();
        assert_eq!(
            output.status,
            ChatTurnStatus::Answered {
                answer: "Final parent answer".to_string()
            }
        );
        let store = agl_store::AglStore::open_current_at(runtime.paths.store_root()).unwrap();
        let tree = store.run_tree(&output.run_id).unwrap();
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[1].parent_run_id.as_ref(), Some(&output.run_id));
        assert_eq!(tree[1].root_run_id, output.run_id);
        assert_eq!(tree[1].depth, 1);
        assert_eq!(tree[1].state, RunState::Succeeded);
        assert!(tree[1].session_id.is_none());
        assert!(tree[1].result_delivered);
        let root_record = store.run(&output.run_id).unwrap().unwrap();
        let frozen_authority =
            root_record.checkpoint.as_ref().unwrap()["delegation_authority_ceiling"]
                .as_array()
                .unwrap();
        assert!(!frozen_authority.iter().any(|id| id == "cron.add"));
        let child_record = store.run(&tree[1].run_id).unwrap().unwrap();
        let presentation_events = presentation.events.lock().unwrap().clone();
        let child_started = presentation_events
            .iter()
            .find_map(|event| match event {
                crate::TurnPresentationEvent::ModelAttemptStarted {
                    session_id,
                    run_id,
                    child_run: Some(child),
                    ..
                } if run_id == &child_record.run_id => Some((session_id, child)),
                _ => None,
            })
            .expect("child model attempt must publish into the root activity stream");
        assert_eq!(child_started.0, chat.session_id());
        assert_eq!(
            child_started.1.parent_run_id,
            child_record.parent_run_id.clone().unwrap()
        );
        assert_eq!(
            child_started.1.spawned_by_step_id,
            child_record.spawned_by_step_id.clone().unwrap()
        );
        assert_eq!(
            child_started.1.subagent_id,
            child_record.subagent_id.clone().unwrap()
        );
        assert!(presentation_events.iter().any(|event| matches!(
            event,
            crate::TurnPresentationEvent::ToolActionFinished {
                capability_id,
                outcome: crate::ToolActionOutcome::Waiting,
                ..
            } if capability_id.as_str() == agl_extension::AGENT_DELEGATE_TOOL_ID
        )));
        assert!(presentation_events.iter().any(|event| matches!(
            event,
            crate::TurnPresentationEvent::TurnFinished {
                run_id,
                child_run: Some(child),
                ..
            } if run_id == &child_record.run_id
                && child.spawned_by_step_id == child_record.spawned_by_step_id.clone().unwrap()
        )));
        assert!(!presentation_events.iter().any(|event| matches!(
            event,
            crate::TurnPresentationEvent::AssistantTextDelta { run_id, .. }
                | crate::TurnPresentationEvent::AssistantMessageFinal { run_id, .. }
                | crate::TurnPresentationEvent::AssistantMessageIncomplete { run_id, .. }
                    if run_id == &child_record.run_id
        )));
        let safe_presentation = format!("{presentation_events:?}");
        for sentinel in [
            "Parent secret phrase",
            "CHILD_TASK_PRIVATE_SENTINEL",
            "CHILD_VERDICT_PRIVATE_SENTINEL",
        ] {
            assert!(!safe_presentation.contains(sentinel));
        }
        let child_input: ChatRunInput = serde_json::from_value(child_record.input).unwrap();
        let ChatRunInput::Subagent {
            authority_ceiling, ..
        } = child_input
        else {
            panic!("durable child has root input");
        };
        assert!(!authority_ceiling.contains(&agl_extension::ToolId::new("cron.add").unwrap()));

        let jobs = state.jobs.lock().unwrap().clone();
        assert_eq!(jobs.len(), 3);
        assert_eq!(jobs[0].session_id, jobs[2].session_id);
        assert_ne!(jobs[0].session_id, jobs[1].session_id);
        assert_eq!(jobs[0].model_path, root.join("missing.gguf"));
        assert_eq!(jobs[1].model_path, root.join("missing-child.gguf"));
        assert_eq!(jobs[2].model_path, root.join("missing.gguf"));
        assert!(jobs[0].tools.iter().any(|tool| tool == "agent.delegate"));
        assert!(!jobs[0].tools.iter().any(|tool| tool == "cron.add"));
        assert!(jobs[1].tools.is_empty());
        assert!(!jobs[0].rendered.contains("Only return the child verdict"));
        assert!(jobs[1].rendered.contains("Only return the child verdict"));
        assert!(
            jobs[1]
                .rendered
                .contains("Review patch CHILD_TASK_PRIVATE_SENTINEL")
        );
        assert!(!jobs[1].rendered.contains("Parent secret phrase"));
        assert!(
            jobs[2]
                .rendered
                .contains("Child verdict CHILD_VERDICT_PRIVATE_SENTINEL")
        );
        assert_eq!(
            state.released_contexts.lock().unwrap().as_slice(),
            &[jobs[1].session_id.clone()]
        );
        let late_grant_id = state.late_grant_id.lock().unwrap().clone().unwrap();
        assert_eq!(
            store
                .permission_grant(&late_grant_id)
                .unwrap()
                .unwrap()
                .status,
            agl_store::PermissionGrantStatus::Revoked
        );

        chat.shutdown().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
