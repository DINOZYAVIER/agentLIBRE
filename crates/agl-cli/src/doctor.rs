use std::path::PathBuf;
use std::time::{Duration, Instant};

use agl_chat::{
    ChatOptions, ChatTurnStatus, InferenceClientHandle, InferenceOptions, SupervisedChat,
    ToolAccessMode,
};
use agl_client::{AgentLibreClient, ClientError, RunSubscriptionEvent};
use agl_functions::{
    FunctionStatusReport, function_status_with_model_bindings, load_function,
    resolve_function_reference,
};
use agl_inference::{ModelManager, ModelManagerOptions, WorkerModelRuntime};
use agl_models::RuntimePlan;
use agl_protocol::{
    AssistantItemState, DaemonCapability, ProtocolRunState, ProtocolToolMode, RunBudgetRequest,
    RunSubmitRequest, RunSubscribeRequest, SessionFinishReason, SessionFinishRequest,
    SessionOpenRequest, SessionPresentationItem, SessionPresentationRequest, SetupSmokeRuntimePlan,
    SetupSmokeSessionOpenRequest,
};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_store::RunBudget;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Serialize;

use crate::{
    InferenceAuthorityDecision, InferenceAuthoritySurface, classify_daemon_connection,
    inference_authority_decision,
};

#[derive(Clone, Debug)]
pub(crate) struct FunctionSmokeRequest {
    pub(crate) reference: String,
    pub(crate) workspace_root: PathBuf,
    pub(crate) bindings_path: Option<PathBuf>,
    pub(crate) runtime_plan_override: Option<RuntimePlan>,
    pub(crate) timeout: Duration,
    pub(crate) max_output_tokens: u32,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FunctionSmokeReport {
    pub(crate) version: u32,
    pub(crate) reference: String,
    pub(crate) static_status: FunctionStatusReport,
    pub(crate) prompt: String,
    pub(crate) answer: String,
    pub(crate) elapsed_ms: u64,
    pub(crate) timeout_ms: u64,
    pub(crate) runtime_profile_id: Option<String>,
}

pub(crate) fn run_function_smoke(
    runtime: &AgentLibreRuntimeConfig,
    request: FunctionSmokeRequest,
) -> Result<FunctionSmokeReport> {
    let static_status = function_status_with_model_bindings(
        &request.reference,
        &request.workspace_root,
        &runtime.paths.config_dir,
        request.bindings_path.as_deref(),
    );
    ensure!(
        static_status.errors.is_empty(),
        "function static validation failed: {}",
        static_status.errors.join("; ")
    );
    let loaded = load_function(resolve_function_reference(
        &request.reference,
        &request.workspace_root,
        &runtime.paths.config_dir,
    )?)?;
    let prompt = loaded
        .front_matter
        .doctor
        .and_then(|doctor| doctor.smoke_prompt)
        .context("function has no doctor.smoke_prompt")?;
    ensure!(
        request.max_output_tokens > 0,
        "smoke output limit must be positive"
    );
    ensure!(!request.timeout.is_zero(), "smoke timeout must be positive");

    let socket_path = agl_daemon::default_socket_path(&runtime.paths);
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build daemon-first function smoke runtime")?;
    let connection = async_runtime.block_on(AgentLibreClient::connect(&socket_path));
    let authority = inference_authority_decision(
        InferenceAuthoritySurface::FunctionSmoke,
        classify_daemon_connection(&connection),
    );
    match (authority, connection) {
        (InferenceAuthorityDecision::Daemon, Ok(client)) => {
            let setup_request = match (
                request.bindings_path.as_deref(),
                request.runtime_plan_override.as_ref(),
            ) {
                (Some(bindings_path), Some(runtime_plan)) => {
                    let staged_bindings = agl_config::load_model_bindings(bindings_path)
                        .context("failed to load staged model bindings for daemon setup smoke")?;
                    Some(SetupSmokeSessionOpenRequest {
                        workspace_root: request.workspace_root.to_string_lossy().into_owned(),
                        function_ref: request.reference.clone(),
                        staged_bindings,
                        runtime_plan: SetupSmokeRuntimePlan {
                            profile_id: runtime_plan.profile_id.clone(),
                            selected_device: runtime_plan.selected_device.clone(),
                            runtime: runtime_plan.runtime.clone(),
                            smoke_timeout_seconds: runtime_plan.smoke_timeout_seconds,
                            expected_speed: runtime_plan.expected_speed.clone(),
                        },
                        max_output_tokens: request.max_output_tokens,
                    })
                }
                (None, None) => None,
                _ => {
                    bail!("staged model bindings and setup runtime plan must be supplied together")
                }
            };
            let timeout_ms = u64::try_from(request.timeout.as_millis()).unwrap_or(u64::MAX);
            let started = Instant::now();
            let answer = async_runtime.block_on(run_daemon_function_smoke(
                client,
                &request.reference,
                &request.workspace_root,
                &prompt,
                timeout_ms,
                request.max_output_tokens,
                setup_request,
            ))?;
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            ensure!(
                elapsed_ms <= timeout_ms.saturating_add(1_000),
                "function smoke exceeded its {timeout_ms} ms timeout"
            );
            return Ok(FunctionSmokeReport {
                version: 1,
                reference: request.reference,
                static_status,
                prompt,
                answer,
                elapsed_ms,
                timeout_ms,
                runtime_profile_id: request
                    .runtime_plan_override
                    .as_ref()
                    .map(|plan| plan.profile_id.clone()),
            });
        }
        (InferenceAuthorityDecision::Standalone, Err(ClientError::DaemonUnavailable(_))) => {}
        (InferenceAuthorityDecision::Reject, Err(error)) => bail!(
            "daemon at {} is active but incompatible or unhealthy ({error}); refusing standalone function smoke",
            socket_path.display()
        ),
        _ => unreachable!("daemon connection classification and authority decision diverged"),
    }

    let mut inference = InferenceOptions {
        function_ref: Some(request.reference.clone()),
        workspace_root: Some(request.workspace_root.clone()),
        artifact_root: Some(runtime.paths.setup_state_root().join("smoke-artifacts")),
        max_output_tokens: request.max_output_tokens,
        tool_mode: ToolAccessMode::ReadOnly,
        model_bindings_path: request.bindings_path,
        runtime_plan_override: request.runtime_plan_override.clone(),
        ..InferenceOptions::default()
    };
    // Keep these internal overrides explicit even if defaults change later.
    inference.skills.clear();
    inference.memory = false;
    let options = ChatOptions {
        inference,
        workspace_root: Some(request.workspace_root.clone()),
        session_id: None,
        no_history: true,
        new_session: true,
    };
    let inference_runtime =
        WorkerModelRuntime::discover(runtime.paths.inference_worker_temp_root())
            .context("failed to prepare isolated inference worker for function smoke")?;
    let model_manager = ModelManager::spawn(
        ModelManagerOptions::default().with_model_lease_root(runtime.paths.model_lease_root()),
        inference_runtime,
    )
    .context("failed to start model manager for function smoke")?;
    let inference_client = InferenceClientHandle::from(model_manager.handle());
    let chat = SupervisedChat::open(options, runtime, inference_client)
        .context("failed to open normal chat path for function smoke")?;
    let timeout_ms = u64::try_from(request.timeout.as_millis()).unwrap_or(u64::MAX);
    let budget = RunBudget {
        wall_time_ms: timeout_ms,
        model_input_tokens: 32_768,
        model_output_tokens: u64::from(request.max_output_tokens),
        model_attempts: 2,
        capability_calls: 0,
    };
    let started = Instant::now();
    let output = chat
        .run_user_turn_with_budget(&prompt, budget)
        .context("function smoke turn failed")?;
    chat.finish_eof_if_needed()?;
    let answer = match output.status {
        ChatTurnStatus::Answered { answer } => answer,
        ChatTurnStatus::Incomplete { reason, .. } => {
            bail!(
                "function smoke returned incomplete output: {}",
                reason.as_str()
            )
        }
        ChatTurnStatus::Stopped { reason } => {
            bail!(
                "function smoke stopped before an answer: {}",
                reason.as_str()
            )
        }
        ChatTurnStatus::Failed { message } => bail!("function smoke failed: {message}"),
        ChatTurnStatus::Cancelled => bail!("function smoke was cancelled or timed out"),
    };
    ensure!(
        !answer.trim().is_empty(),
        "function smoke returned an empty assistant answer"
    );
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    ensure!(
        elapsed_ms <= timeout_ms.saturating_add(1_000),
        "function smoke exceeded its {} ms timeout",
        timeout_ms
    );
    Ok(FunctionSmokeReport {
        version: 1,
        reference: request.reference,
        static_status,
        prompt,
        answer,
        elapsed_ms,
        timeout_ms,
        runtime_profile_id: request.runtime_plan_override.map(|plan| plan.profile_id),
    })
}

async fn run_daemon_function_smoke(
    client: AgentLibreClient,
    reference: &str,
    workspace_root: &std::path::Path,
    prompt: &str,
    timeout_ms: u64,
    max_output_tokens: u32,
    setup_request: Option<SetupSmokeSessionOpenRequest>,
) -> Result<String> {
    let mut required = vec![
        DaemonCapability::RunSubmit,
        DaemonCapability::RunSubscribe,
        DaemonCapability::SessionPresentation,
        DaemonCapability::SessionFinish,
    ];
    required.push(if setup_request.is_some() {
        DaemonCapability::SetupSmokeSessionOpen
    } else {
        DaemonCapability::SessionOpen
    });
    let hello = client.hello().context("failed to read daemon identity")?;
    if let Some(missing) = required
        .into_iter()
        .find(|capability| !hello.capabilities.contains(capability))
    {
        bail!("daemon lacks required function-smoke capability {missing:?}");
    }
    let opened = match setup_request {
        Some(request) => client.open_setup_smoke_session(request).await,
        None => {
            client
                .open_session(SessionOpenRequest {
                    session_id: None,
                    new_session: true,
                    workspace_root: Some(workspace_root.to_string_lossy().into_owned()),
                    function_ref: Some(reference.to_owned()),
                    skills: Vec::new(),
                    tool_mode: ProtocolToolMode::ReadOnly,
                })
                .await
        }
    }
    .context("daemon rejected the function smoke session")?;
    let session_id = opened.session_id;
    let turn = async {
        let accepted = client
            .submit_run(RunSubmitRequest {
                session_id: session_id.clone(),
                content: agl_content::Content::text(prompt.to_owned())?,
                client_submission_id: format!("cli-doctor-{}", agl_ids::RequestId::generate()),
                budget: RunBudgetRequest {
                    wall_time_ms: timeout_ms,
                    model_input_tokens: 32_768,
                    model_output_tokens: u64::from(max_output_tokens),
                    model_attempts: 2,
                    capability_calls: 0,
                },
            })
            .await
            .context("daemon rejected the function smoke run")?;
        let mut subscription = client
            .subscribe_run(RunSubscribeRequest {
                run_id: accepted.run_id,
                after_sequence: 0,
            })
            .await
            .context("failed to subscribe to the function smoke run")?;
        let finished = loop {
            match subscription.next().await? {
                Some(RunSubscriptionEvent::Event(_)) => {}
                Some(RunSubscriptionEvent::Finished(finished)) => break finished,
                None => bail!("function smoke subscription ended without a terminal event"),
            }
        };
        ensure!(
            finished.state == ProtocolRunState::Succeeded,
            "function smoke failed: {}",
            finished
                .error_message
                .or(finished.error_code)
                .unwrap_or_else(|| format!("{:?}", finished.state))
        );
        let snapshot = client
            .session_presentation(SessionPresentationRequest {
                session_id: session_id.clone(),
                page_cursor: None,
            })
            .await
            .context("failed to read function smoke presentation")?;
        let item = snapshot
            .items
            .iter()
            .rev()
            .find(|item| {
                matches!(
                    item,
                    SessionPresentationItem::AssistantMessage { .. }
                        | SessionPresentationItem::IncompleteAssistant { .. }
                )
            })
            .context("function smoke completed without an assistant result")?;
        match item {
            SessionPresentationItem::AssistantMessage { content, state, .. }
                if *state == AssistantItemState::Final =>
            {
                content
                    .text_only()
                    .context("function smoke assistant result is not text-only")
            }
            SessionPresentationItem::IncompleteAssistant { item } => bail!(
                "function smoke returned incomplete output: {:?}",
                item.reason
            ),
            SessionPresentationItem::AssistantMessage { state, .. } => {
                bail!("function smoke assistant result is not final: {state:?}")
            }
            _ => unreachable!("assistant filter admits only assistant items"),
        }
    }
    .await;
    let finish = client
        .finish_session(SessionFinishRequest {
            session_id,
            reason: SessionFinishReason::Eof,
        })
        .await
        .context("failed to finish daemon function smoke session");
    match (turn, finish) {
        (Ok(answer), Ok(_)) => Ok(answer),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(turn), Err(finish)) => Err(anyhow!("{turn:#}; additionally {finish:#}")),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agl_chat::{ChatInferenceJob, InferenceClient};
    use agl_config::ResolvedInferenceConfig;
    use agl_ids::SessionId;
    use agl_inference::{
        InferenceFinishReason, InferenceResponse, InferenceResponseMetadata, ModelManagerStatus,
        WorkerRuntimeStatusHandle,
    };
    use agl_runtime::{
        AgentLibreExecutionConfig, AgentLibreHistoryConfig, AgentLibreLoggingConfig,
        AgentLibrePaths, AgentLibreRuntimeConfig, AgentLibreWorkspaceConfig,
    };

    use super::*;

    #[derive(Clone)]
    struct SmokeInference {
        calls: Arc<AtomicUsize>,
    }

    impl InferenceClient for SmokeInference {
        fn generate(&self, job: ChatInferenceJob) -> anyhow::Result<InferenceResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(InferenceResponse {
                attempt_id: job.request.attempt_id,
                content: "daemon setup smoke answer".to_owned(),
                finish_reason: InferenceFinishReason::Stop,
                metadata: InferenceResponseMetadata {
                    model_state: Some("fake-daemon".to_owned()),
                    selected_device: None,
                    duration_ms: 1,
                    input_tokens: 4,
                    output_tokens: 4,
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

    fn runtime(root: &std::path::Path) -> AgentLibreRuntimeConfig {
        AgentLibreRuntimeConfig {
            paths: AgentLibrePaths::from_agl_home(root.join("home")),
            logging: AgentLibreLoggingConfig::default(),
            history: AgentLibreHistoryConfig::default(),
            workspace: AgentLibreWorkspaceConfig::default(),
            execution: AgentLibreExecutionConfig::default(),
        }
    }

    fn runtime_plan() -> RuntimePlan {
        RuntimePlan {
            profile_id: "setup-cpu".to_owned(),
            selected_device: None,
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
                mtp: agl_config::MtpRuntimeConfig::default(),
            },
            smoke_timeout_seconds: 30,
            expected_speed: "test".to_owned(),
        }
    }

    #[test]
    fn compatible_daemon_executes_staged_setup_smoke_without_local_worker() {
        let root = std::env::temp_dir().join(format!(
            "agl-cli-daemon-setup-smoke-{}",
            agl_ids::RequestId::generate()
        ));
        let runtime = runtime(&root);
        let workspace = root.join("workspace");
        let function_root = workspace.join(".agl/functions/setup-smoke");
        std::fs::create_dir_all(&function_root).unwrap();
        std::fs::write(
            function_root.join("FUNCTION.md"),
            r#"---
schema: agentfunction/v1
id: setup-smoke
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
        let model = root.join("staged-model.gguf");
        std::fs::write(&model, b"test model fixture").unwrap();
        let staged_bindings_path = root.join("staged-models.toml");
        agl_config::write_model_bindings(
            &staged_bindings_path,
            &agl_config::ModelBindings {
                version: 1,
                models: std::collections::BTreeMap::from([(
                    agl_config::ModelId::new("setup-smoke-model").unwrap(),
                    agl_config::ModelBinding { path: model },
                )]),
            },
        )
        .unwrap();
        let published_bindings_path = agl_config::model_bindings_path(&runtime.paths.config_dir);
        agl_config::write_model_bindings(
            &published_bindings_path,
            &agl_config::ModelBindings::empty(),
        )
        .unwrap();

        let socket_path = agl_daemon::default_socket_path(&runtime.paths);
        std::fs::create_dir_all(socket_path.parent().unwrap()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = Arc::clone(&calls);
        let server_runtime = runtime.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let server = std::thread::spawn(move || -> anyhow::Result<()> {
            let async_runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            async_runtime.block_on(async move {
                let listener = tokio::net::UnixListener::bind(&socket_path)?;
                ready_tx.send(()).unwrap();
                let (stream, _) = listener.accept().await?;
                let state = agl_daemon::SharedDaemonState::new(
                    server_runtime,
                    InferenceOptions::default(),
                    InferenceClientHandle::new(SmokeInference {
                        calls: server_calls,
                    }),
                    WorkerRuntimeStatusHandle::default(),
                );
                agl_daemon::serve_connection(stream, &state).await
            })
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let report = run_function_smoke(
            &runtime,
            FunctionSmokeRequest {
                reference: "setup-smoke".to_owned(),
                workspace_root: workspace,
                bindings_path: Some(staged_bindings_path),
                runtime_plan_override: Some(runtime_plan()),
                timeout: Duration::from_secs(5),
                max_output_tokens: 32,
            },
        )
        .unwrap();

        assert_eq!(report.answer, "daemon setup smoke answer");
        assert_eq!(report.runtime_profile_id.as_deref(), Some("setup-cpu"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!runtime.paths.inference_worker_temp_root().exists());
        assert!(
            agl_config::load_model_bindings(&published_bindings_path)
                .unwrap()
                .models
                .is_empty()
        );
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
