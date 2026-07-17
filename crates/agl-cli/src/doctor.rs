use std::path::PathBuf;
use std::time::{Duration, Instant};

use agl_chat::{
    ChatOptions, ChatTurnStatus, InferenceClientHandle, InferenceOptions, SupervisedChat,
    ToolAccessMode,
};
use agl_functions::{
    FunctionStatusReport, function_status_with_model_bindings, load_function,
    resolve_function_reference,
};
use agl_inference::{LlamaCppModelRuntime, ModelManager, ModelManagerOptions};
use agl_models::RuntimePlan;
use agl_runtime::AgentLibreRuntimeConfig;
use agl_store::RunBudget;
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

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
    let model_manager = ModelManager::spawn(
        ModelManagerOptions::default().with_model_lease_root(runtime.paths.model_lease_root()),
        LlamaCppModelRuntime::new(),
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
