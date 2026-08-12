use std::path::PathBuf;
use std::time::{Duration, Instant};

use agl_chat::{
    ChatOptions, ChatTurnStatus, InferenceClientHandle, InferenceOptions, SupervisedChat,
    ToolAccessMode,
};
use agl_client::{AgentLibreClient, ClientError, RunSubscriptionEvent};
use agl_function::{FunctionStatusReport, function_status_from_loaded};
use agl_inference::{InferenceHost, InferenceHostConfig};
use agl_protocol::{
    AssistantItemState, DaemonTool, ProtocolRunState, ProtocolToolMode, RunBudgetRequest,
    RunSubmitRequest, RunSubscribeRequest, SessionFinishReason, SessionFinishRequest,
    SessionOpenRequest, SessionPresentationItem, SessionPresentationRequest,
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
    let loaded = crate::function::resolve_loaded_function(
        runtime,
        &request.workspace_root,
        &request.reference,
    )?;
    let static_status = function_status_from_loaded(
        &request.reference,
        loaded.clone(),
        &request.workspace_root,
        &runtime.paths.config_dir,
        None,
    );
    let runtime_profile_id = agl_runtime::resolve_composed_runtime_function(
        &runtime.paths,
        &request.workspace_root,
        &request.reference,
        true,
    )?
    .model_profile;
    ensure!(
        static_status.errors.is_empty(),
        "function static validation failed: {}",
        static_status.errors.join("; ")
    );
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
    let connection = async_runtime.block_on(crate::runtime::connect_daemon(&socket_path));
    let authority = inference_authority_decision(
        InferenceAuthoritySurface::FunctionSmoke,
        classify_daemon_connection(&connection),
    );
    match (authority, connection) {
        (InferenceAuthorityDecision::Daemon, Ok(client)) => {
            let timeout_ms = u64::try_from(request.timeout.as_millis()).unwrap_or(u64::MAX);
            let started = Instant::now();
            let answer = async_runtime.block_on(run_daemon_function_smoke(
                client,
                &request.reference,
                &request.workspace_root,
                &prompt,
                timeout_ms,
                request.max_output_tokens,
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
                runtime_profile_id,
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
    let inference_host = InferenceHost::start_with_journal_root(
        InferenceHostConfig::development_default(
            runtime.paths.inference_state_root().join("authority"),
            runtime.paths.default_artifact_root(),
            std::time::Duration::from_secs(runtime.inference.residency.context_idle_seconds),
            std::time::Duration::from_secs(runtime.inference.residency.model_idle_seconds),
        )?,
        runtime.paths.inference_state_root().join("attempts"),
    )
    .context("failed to start inference host for function smoke")?;
    let inference_client = InferenceClientHandle::from(inference_host);
    let chat = SupervisedChat::open(options, runtime, inference_client)
        .context("failed to open normal chat path for function smoke")?;
    let timeout_ms = u64::try_from(request.timeout.as_millis()).unwrap_or(u64::MAX);
    let budget = RunBudget {
        wall_time_ms: timeout_ms,
        model_input_tokens: 32_768,
        model_output_tokens: u64::from(request.max_output_tokens),
        model_attempts: 2,
        tool_calls: 0,
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
        runtime_profile_id,
    })
}

async fn run_daemon_function_smoke(
    client: AgentLibreClient,
    reference: &str,
    workspace_root: &std::path::Path,
    prompt: &str,
    timeout_ms: u64,
    max_output_tokens: u32,
) -> Result<String> {
    let mut required = vec![
        DaemonTool::RunSubmit,
        DaemonTool::RunSubscribe,
        DaemonTool::SessionPresentation,
        DaemonTool::SessionFinish,
    ];
    required.push(DaemonTool::SessionOpen);
    let hello = client.hello().context("failed to read daemon identity")?;
    if let Some(missing) = required
        .into_iter()
        .find(|tool| !hello.tools.contains(tool))
    {
        bail!("daemon lacks required function-smoke tool {missing:?}");
    }
    let opened = client
        .open_session(SessionOpenRequest {
            session_id: None,
            new_session: true,
            workspace_root: Some(workspace_root.to_string_lossy().into_owned()),
            function_ref: Some(reference.to_owned()),
            skills: Vec::new(),
            tool_mode: ProtocolToolMode::ReadOnly,
        })
        .await
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
                    tool_calls: 0,
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
