use std::env;
use std::fmt;
use std::path::PathBuf;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use agl_chat::{
    ChatOptions, ChatTurnStatus, DEFAULT_MAX_OUTPUT_TOKENS, InferenceClientHandle,
    InferenceOptions, ToolAccessMode as ChatToolAccessMode,
};
use agl_client::{AgentLibreClient, ClientError, RunSubscriptionEvent};
use agl_cron::{
    CronJob, CronJobDraft, CronRepository, CronRun, CronTargetKind,
    STORE_STATUS_BUILTIN_CRON_TARGET, unsupported_builtin_cron_target_message,
    validate_builtin_cron_target,
};
use agl_daemon::{
    CronExecution, CronNotification, CronNotifier, CronTargetExecutor, DaemonOptions, DaemonServer,
    default_socket_path, render_cron_notification_body, render_cron_skill_prompt, run_cron_tick,
};
use agl_ids::{RunId, SessionId};
use agl_inference::{InferenceDeviceInfo, InferenceDeviceKind, InferenceHost, InferenceHostConfig};
use agl_protocol::{
    AssistantItemState, DaemonTool, InferenceStatusRequest, ModelReleaseOutcome,
    ModelReleaseReason, ProtocolInferenceDeviceKind, ProtocolInferenceEngineState,
    ProtocolRunState, ProtocolToolMode, RunBudgetRequest, RunSubmitRequest, RunSubscribeRequest,
    RuntimeGenerationKind, SessionFinishReason, SessionFinishRequest, SessionListRequest,
    SessionOpenRequest, SessionPresentationItem, SessionPresentationRequest,
};
use agl_repo::read_workspace_default_function;
use agl_runtime::{
    AgentLibreHistoryConfig, AgentLibreLoggingConfig, AgentLibrePaths, AgentLibreProcessMode,
    AgentLibreRuntimeConfig, AgentLibreWorkspaceConfig, init_tracing,
};
use agl_skill::{SkillPermissions, SkillSource, SkillTrustStore, builtin_registry};
use agl_store::{AglStore, IdempotencyOutcome, MatrixNotificationOutboxDraft};
use anyhow::{Context, Result, anyhow, bail, ensure};

mod args;
mod artifact;
mod config;
mod doctor;
mod function;
mod init;
mod memory;
mod model;
mod notes;
mod one_shot;
mod package;
mod repo;
mod runtime;
mod session;
mod store;
mod trace;

use args::{
    CliCommand, CronAddOptions, CronCommand, CronDeleteOptions, CronDisableOptions,
    CronEnableOptions, CronHistoryOptions, CronListOptions, CronRunOptions, CronShowOptions,
    CronTargetArg, CronTargetKindArg, CronTickOptions, DaemonStatusOptions, RunOptions,
    ServeOptions, SkillCommand, SkillInspectOptions, SkillListOptions, SkillListSourceArg,
    SkillRevokeOptions, SkillStatusOptions, SkillTrustOptions, SkillVerifyOptions, parse_cli,
    print_completion,
};
use artifact::run_artifact;
use config::run_config;
use function::run_function;
use init::run_init;
use memory::run_memory;
use model::run_model;
use notes::run_notes;
use one_shot::OneShotSession;
use package::run_package;
use repo::run_repo;
use session::run_session;
use store::run_store;
use trace::run_trace;

pub(crate) fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub(crate) fn print_json_or(
    json: bool,
    value: &impl serde::Serialize,
    print_text: impl FnOnce(),
) -> Result<()> {
    if json {
        print_json(value)
    } else {
        print_text();
        Ok(())
    }
}

pub fn run_cli() {
    if let Some(result) = runtime::internal_runtime_action() {
        if let Err(err) = result {
            eprintln!("error: {err:#}");
            process::exit(1);
        }
        return;
    }
    if env::var_os("AGL_INTERNAL_VERIFY_RUNTIME_BUNDLE").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        if let Err(err) = runtime::verify_runtime_bundle_identity() {
            eprintln!("error: {err:#}");
            process::exit(1);
        }
        println!("runtime bundle identity verified");
        return;
    }
    let invocation = match parse_cli(env::args()) {
        Ok(invocation) => invocation,
        Err(err) => {
            print_cli_error(&err);
            process::exit(1);
        }
    };
    let command = invocation.command;
    match &command {
        CliCommand::HelpPrinted => return,
        CliCommand::Completion { shell } => {
            print_completion(*shell);
            return;
        }
        _ => {}
    }

    let package_json = match &command {
        CliCommand::Package(command) => package::json_requested(command),
        _ => false,
    };
    let run_json = matches!(&command, CliCommand::Run(options) if options.json);
    let runtime = match runtime_for_command(&command, invocation.home) {
        Ok(runtime) => runtime,
        Err(err) => {
            let err = err.context("failed to resolve agentLIBRE runtime");
            if package_json {
                package::print_error_json(&err);
            } else if run_json {
                print_run_error_json(&err);
            } else {
                eprintln!("error: {err:#}");
            }
            process::exit(1);
        }
    };
    let process_mode = process_mode_for_command(&command);
    let _tracing_guards = match init_tracing(&runtime.paths, &runtime.logging, process_mode) {
        Ok(guards) => Some(guards),
        Err(err) => {
            if matches!(process_mode, AgentLibreProcessMode::Interactive) {
                eprintln!("warning: failed to initialize logging: {err:#}");
            }
            None
        }
    };

    tracing::info!(
        target: "agentlibre::app",
        config_dir = %runtime.paths.config_dir.display(),
        data_dir = %runtime.paths.data_dir.display(),
        state_dir = %runtime.paths.state_dir.display(),
        cache_dir = %runtime.paths.cache_dir.display(),
        "agentLIBRE runtime paths resolved"
    );

    if let Err(err) = run(command, &runtime) {
        tracing::error!(target: "agentlibre::app", error = %err, "agentLIBRE command failed");
        if package_json {
            package::print_error_json(&err);
        } else if run_json {
            print_run_error_json(&err);
        } else {
            eprintln!("error: {err:#}");
        }
        process::exit(1);
    }
}

fn runtime_for_command(
    command: &CliCommand,
    home: Option<std::path::PathBuf>,
) -> Result<AgentLibreRuntimeConfig> {
    let paths = if let Some(home) = home {
        AgentLibrePaths::from_agl_home(home)
    } else {
        AgentLibrePaths::from_env()?
    };
    runtime_for_command_paths(command, paths)
}

fn runtime_for_command_paths(
    command: &CliCommand,
    paths: AgentLibrePaths,
) -> Result<AgentLibreRuntimeConfig> {
    match cli_runtime_profile(command) {
        CliRuntimeProfile::LightBatch => Ok(AgentLibreRuntimeConfig {
            paths,
            logging: AgentLibreLoggingConfig::from_env(),
            history: AgentLibreHistoryConfig::default(),
            workspace: AgentLibreWorkspaceConfig::default(),
            inference: agl_runtime::AgentLibreInferenceConfig::default(),
            execution: agl_runtime::AgentLibreExecutionConfig::default(),
        }),
        CliRuntimeProfile::FullBatch | CliRuntimeProfile::Interactive => {
            AgentLibreRuntimeConfig::from_paths(paths)
        }
    }
}

fn process_mode_for_command(command: &CliCommand) -> AgentLibreProcessMode {
    match cli_runtime_profile(command) {
        CliRuntimeProfile::Interactive => AgentLibreProcessMode::Interactive,
        CliRuntimeProfile::FullBatch | CliRuntimeProfile::LightBatch => {
            AgentLibreProcessMode::Batch
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CliRuntimeProfile {
    Interactive,
    FullBatch,
    LightBatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InferenceAuthoritySurface {
    DirectRun,
    Cron,
    FunctionSmoke,
    InitInventory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DaemonConnectionClass {
    Compatible,
    Unavailable,
    Incompatible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InferenceAuthorityDecision {
    Daemon,
    Standalone,
    Reject,
}

pub(crate) fn classify_daemon_connection<T>(
    connection: &std::result::Result<T, ClientError>,
) -> DaemonConnectionClass {
    match connection {
        Ok(_) => DaemonConnectionClass::Compatible,
        Err(ClientError::DaemonUnavailable(_)) => DaemonConnectionClass::Unavailable,
        Err(_) => DaemonConnectionClass::Incompatible,
    }
}

pub(crate) const fn inference_authority_decision(
    surface: InferenceAuthoritySurface,
    connection: DaemonConnectionClass,
) -> InferenceAuthorityDecision {
    match connection {
        DaemonConnectionClass::Compatible => InferenceAuthorityDecision::Daemon,
        DaemonConnectionClass::Unavailable => match surface {
            InferenceAuthoritySurface::DirectRun
            | InferenceAuthoritySurface::Cron
            | InferenceAuthoritySurface::FunctionSmoke
            | InferenceAuthoritySurface::InitInventory => InferenceAuthorityDecision::Standalone,
        },
        DaemonConnectionClass::Incompatible => InferenceAuthorityDecision::Reject,
    }
}

fn cli_runtime_profile(command: &CliCommand) -> CliRuntimeProfile {
    match command {
        CliCommand::Run(_) => CliRuntimeProfile::Interactive,
        CliCommand::Config(_)
        | CliCommand::Package(_)
        | CliCommand::Artifact(_)
        | CliCommand::Cron(_)
        | CliCommand::Function(_)
        | CliCommand::Init(_)
        | CliCommand::Model(_)
        | CliCommand::Store(_)
        | CliCommand::Repo(_)
        | CliCommand::Skill(_)
        | CliCommand::Session(_)
        | CliCommand::Memory(_)
        | CliCommand::Notes(_)
        | CliCommand::Trace(_)
        | CliCommand::RuntimeIdentity
        | CliCommand::DaemonStatus(_) => CliRuntimeProfile::LightBatch,
        CliCommand::Serve(_) | CliCommand::HelpPrinted | CliCommand::Completion { .. } => {
            CliRuntimeProfile::FullBatch
        }
    }
}

fn run(command: CliCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    match command {
        CliCommand::HelpPrinted => Ok(()),
        CliCommand::Completion { shell } => {
            print_completion(shell);
            Ok(())
        }
        CliCommand::Config(command) => run_config(command, runtime),
        CliCommand::Package(command) => run_package(command, runtime),
        CliCommand::Artifact(command) => run_artifact(command, runtime),
        CliCommand::Cron(command) => run_cron(command, runtime),
        CliCommand::Store(command) => run_store(command, runtime),
        CliCommand::Function(command) => run_function(command, runtime),
        CliCommand::Init(options) => run_init(options, runtime),
        CliCommand::Model(command) => run_model(command, runtime),
        CliCommand::Memory(command) => run_memory(command, runtime),
        CliCommand::Notes(command) => run_notes(command, runtime),
        CliCommand::Repo(command) => run_repo(command),
        CliCommand::Skill(command) => run_skill(command, runtime),
        CliCommand::Session(options) => run_session(options, runtime),
        CliCommand::Trace(command) => run_trace(command),
        CliCommand::RuntimeIdentity => runtime::print_runtime_identity(),
        CliCommand::Serve(options) => run_serve(options, runtime),
        CliCommand::DaemonStatus(options) => run_daemon_status(options, runtime),
        CliCommand::Run(options) => run_one_shot(options, runtime),
    }
}

fn run_cron(command: CronCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "cron", "starting command");
    let store =
        AglStore::open_at(runtime.paths.store_root()).context("failed to open cron store")?;
    let cron = CronRepository::new(&store);

    match command {
        CronCommand::Add(options) => run_cron_add(options, &cron, runtime),
        CronCommand::List(options) => run_cron_list(options, &cron),
        CronCommand::Show(options) => run_cron_show(options, &cron),
        CronCommand::Enable(options) => run_cron_enable(options, &cron),
        CronCommand::Disable(options) => run_cron_disable(options, &cron),
        CronCommand::Run(options) => run_cron_run(options, &cron, &store, runtime),
        CronCommand::Tick(options) => run_cron_tick_command(options, &store, runtime),
        CronCommand::History(options) => run_cron_history(options, &cron),
        CronCommand::Delete(options) => run_cron_delete(options, &cron),
    }
}

fn run_cron_add(
    options: CronAddOptions,
    cron: &CronRepository<'_>,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    validate_cron_target(&options.target, runtime)?;
    let mut draft = CronJobDraft::new(
        options.name,
        cron_target_kind(options.target.kind),
        options.target.target_ref,
        options.schedule,
    );
    draft.enabled = options.enabled;
    if let Some(timezone) = options.timezone {
        draft.timezone = timezone;
    }
    draft.notify_ref = options.notify_ref;
    draft.prompt = options.prompt;
    draft.input = options.input;
    let job = cron.add_job(draft).context("failed to add cron job")?;

    crate::print_json_or(options.json, &job, || print_cron_job_summary(&job))
}

fn run_cron_list(options: CronListOptions, cron: &CronRepository<'_>) -> Result<()> {
    let jobs = cron
        .list_jobs(options.include_deleted)
        .context("failed to list cron jobs")?;
    crate::print_json_or(options.json, &jobs, || print_cron_jobs(&jobs))
}

fn run_cron_show(options: CronShowOptions, cron: &CronRepository<'_>) -> Result<()> {
    let job = cron
        .job(&options.id)
        .context("failed to read cron job")?
        .ok_or_else(|| anyhow::anyhow!("cron job not found: {}", options.id))?;
    crate::print_json_or(options.json, &job, || print_cron_job_detail(&job))
}

fn run_cron_enable(options: CronEnableOptions, cron: &CronRepository<'_>) -> Result<()> {
    let job = cron
        .set_enabled(&options.id, true)
        .context("failed to enable cron job")?;
    crate::print_json_or(options.json, &job, || print_cron_job_summary(&job))
}

fn run_cron_disable(options: CronDisableOptions, cron: &CronRepository<'_>) -> Result<()> {
    let job = cron
        .set_enabled(&options.id, false)
        .context("failed to disable cron job")?;
    crate::print_json_or(options.json, &job, || print_cron_job_summary(&job))
}

fn run_cron_run(
    options: CronRunOptions,
    cron: &CronRepository<'_>,
    store: &AglStore,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let _ = options.now;
    let job = cron
        .job(&options.id)
        .context("failed to read cron job")?
        .ok_or_else(|| anyhow::anyhow!("cron job not found: {}", options.id))?;
    if options.preflight {
        return run_cron_preflight(&job, runtime, options.json);
    }
    validate_stored_cron_target(&job, runtime)?;
    let mut inference = if job.target_kind == CronTargetKind::Skill && !options.mock_skill_execution
    {
        Some(daemon_first_cron_inference(runtime)?)
    } else {
        None
    };
    let execution = execute_cron_target(
        &job,
        store,
        runtime,
        options.mock_skill_execution,
        inference.as_mut(),
    );
    let (run, outcome) = cron
        .record_manual_run_result(
            &job.id,
            execution.status,
            execution.result_ref.as_deref(),
            execution.error.as_deref(),
        )
        .context("failed to record cron run")?;
    let idempotency = idempotency_report(store, &outcome)?;

    if options.json {
        crate::print_json(&serde_json::json!({
            "job": job,
            "run": run,
            "idempotency": idempotency,
        }))?;
    } else {
        print_cron_run(&run);
        println!(
            "cron_run.{}.idempotency.admission={}",
            run.id,
            idempotency["admission"].as_str().unwrap_or("unknown")
        );
        println!(
            "cron_run.{}.idempotency.final_status={}",
            run.id,
            idempotency["final_status"].as_str().unwrap_or("unknown")
        );
    }
    Ok(())
}

fn run_cron_preflight(job: &CronJob, runtime: &AgentLibreRuntimeConfig, json: bool) -> Result<()> {
    validate_stored_cron_target(job, runtime)?;
    let prompt = if job.target_kind == CronTargetKind::Skill {
        Some(render_cron_skill_prompt(job)?)
    } else {
        None
    };
    let report = serde_json::json!({
        "ok": true,
        "target_kind": job.target_kind.as_str(),
        "target_ref": job.target_ref,
        "prompt_ready": job.target_kind != CronTargetKind::Skill || prompt.is_some(),
        "prompt_preview": prompt.as_deref().map(prompt_preview),
        "records_run": false,
    });
    if json {
        crate::print_json(&serde_json::json!({
            "job": job,
            "preflight": report,
        }))?;
    } else {
        println!("core.cron:preflight.ok=true");
        println!(
            "core.cron:preflight.target={}:{}",
            job.target_kind.as_str(),
            job.target_ref
        );
        println!("core.cron:preflight.records_run=false");
    }
    Ok(())
}

fn run_cron_tick_command(
    options: CronTickOptions,
    store: &AglStore,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let unix_seconds = options.at.unwrap_or_else(unix_now);
    let needs_inference = !options.mock_skill_execution
        && CronRepository::new(store)
            .due_jobs(unix_seconds)
            .context("failed to inspect due cron jobs for inference authority")?
            .iter()
            .any(|due| due.job.target_kind == CronTargetKind::Skill);
    let inference = needs_inference
        .then(|| daemon_first_cron_inference(runtime))
        .transpose()?;
    let mut executor = CliCronExecutor {
        store,
        runtime,
        mock_skill_execution: options.mock_skill_execution,
        inference,
    };
    let mut notifier = CliStoreCronNotifier { store };
    let report = run_cron_tick(store, unix_seconds, &mut executor, &mut notifier)
        .context("failed to run cron scheduler tick")?;
    if options.json {
        crate::print_json(&serde_json::json!({
            "at": unix_seconds,
            "due_jobs": report.due_jobs,
            "recorded_runs": report.recorded_runs,
            "notifications": report.notifications,
        }))?;
    } else {
        println!("core.cron:tick.at={unix_seconds}");
        println!("core.cron:tick.due_jobs={}", report.due_jobs);
        println!(
            "core.cron:tick.recorded_runs={}",
            report.recorded_runs.len()
        );
        println!("core.cron:tick.notifications={}", report.notifications);
        print_cron_runs(&report.recorded_runs);
    }
    Ok(())
}

fn run_cron_history(options: CronHistoryOptions, cron: &CronRepository<'_>) -> Result<()> {
    let runs = cron
        .history(&options.id)
        .context("failed to read cron run history")?;
    crate::print_json_or(options.json, &runs, || print_cron_runs(&runs))
}

fn run_cron_delete(options: CronDeleteOptions, cron: &CronRepository<'_>) -> Result<()> {
    let job = cron
        .delete_job(&options.id)
        .context("failed to delete cron job")?;
    crate::print_json_or(options.json, &job, || {
        println!("core.cron:deleted=true");
        print_cron_job_summary(&job);
    })
}

fn validate_cron_target(target: &CronTargetArg, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    match target.kind {
        CronTargetKindArg::Builtin => {
            validate_builtin_cron_target(&target.target_ref).map_err(anyhow::Error::msg)
        }
        CronTargetKindArg::Skill => validate_trusted_cron_skill(&target.target_ref, runtime),
    }
}

fn validate_stored_cron_target(job: &CronJob, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    match job.target_kind {
        CronTargetKind::Builtin => {
            validate_builtin_cron_target(&job.target_ref).map_err(anyhow::Error::msg)
        }
        CronTargetKind::Skill => validate_trusted_cron_skill(&job.target_ref, runtime),
    }
}

fn validate_trusted_cron_skill(name: &str, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let _ = runtime;
    let registry = builtin_registry()?;
    let matches = registry
        .skills()
        .iter()
        .filter(|skill| skill.harness.name == name || skill.harness.id.as_str() == name)
        .collect::<Vec<_>>();
    if matches
        .iter()
        .any(|skill| skill.permits_context_injection())
    {
        return Ok(());
    }
    if matches.is_empty() {
        bail!("cron skill target not found: {name}");
    }
    bail!("cron skill target is not runtime usable: {name}");
}

fn execute_cron_target(
    job: &CronJob,
    store: &AglStore,
    runtime: &AgentLibreRuntimeConfig,
    mock_skill_execution: bool,
    inference: Option<&mut CliCronInference>,
) -> CronExecution {
    match run_cron_target(job, store, runtime, mock_skill_execution, inference) {
        Ok(result_ref) => CronExecution::succeeded(result_ref),
        Err(err) => CronExecution::failed(format!("{err:#}")),
    }
}

fn run_cron_target(
    job: &CronJob,
    store: &AglStore,
    runtime: &AgentLibreRuntimeConfig,
    mock_skill_execution: bool,
    inference: Option<&mut CliCronInference>,
) -> Result<String> {
    match job.target_kind {
        CronTargetKind::Builtin => run_builtin_cron_target(job, store),
        CronTargetKind::Skill if mock_skill_execution => run_mock_skill_cron_target(job),
        CronTargetKind::Skill => inference
            .context("cron skill inference authority is not initialized")?
            .run_skill(job, runtime),
    }
}

fn run_builtin_cron_target(job: &CronJob, store: &AglStore) -> Result<String> {
    match job.target_ref.as_str() {
        STORE_STATUS_BUILTIN_CRON_TARGET => {
            let health = store.health().context("failed to check store health")?;
            Ok(format!(
                "builtin:store-status:schema:{}",
                health.migration_version
            ))
        }
        _ => bail!(
            "{}",
            unsupported_builtin_cron_target_message(&job.target_ref)
        ),
    }
}

fn run_skill_cron_target(
    job: &CronJob,
    runtime: &AgentLibreRuntimeConfig,
    inference_client: &InferenceClientHandle,
) -> Result<String> {
    let prompt = render_cron_skill_prompt(job)?;
    let mut inference = InferenceOptions::default();
    inference.skills.push(job.target_ref.clone());
    inference.tool_mode = ChatToolAccessMode::Write;
    let chat = OneShotSession::open(
        ChatOptions {
            inference,
            workspace_root: None,
            session_id: None,
            no_history: false,
            new_session: true,
        },
        runtime,
        inference_client.clone(),
    )?;
    let session_id = chat.session_id().clone();
    let output = chat.run_user_turn(&prompt)?;
    chat.finish_eof_if_needed()?;
    match output.status {
        ChatTurnStatus::Answered { .. } => Ok(format!(
            "skill:{}:session:{session_id}:run:{}",
            job.target_ref, output.run_id
        )),
        ChatTurnStatus::Incomplete { reason, .. } => {
            bail!("cron skill returned incomplete output: {}", reason.as_str())
        }
        ChatTurnStatus::Stopped { reason } => bail!("cron skill stopped before answer: {reason:?}"),
        ChatTurnStatus::Failed { message } => bail!("cron skill turn failed: {message}"),
        ChatTurnStatus::Cancelled => bail!("cron skill turn was cancelled"),
    }
}

async fn run_skill_cron_target_via_daemon(
    client: &AgentLibreClient,
    job: &CronJob,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<String> {
    let prompt = render_cron_skill_prompt(job)?;
    let workspace_root = runtime.resolve_workspace_root(None)?;
    let opened = client
        .open_session(SessionOpenRequest {
            session_id: None,
            new_session: true,
            workspace_root: Some(workspace_root.to_string_lossy().into_owned()),
            function_ref: None,
            skills: vec![job.target_ref.clone()],
            tool_mode: ProtocolToolMode::Write,
        })
        .await
        .context("daemon rejected the cron skill session")?;
    let session_id = opened.session_id;

    let execution = async {
        let accepted = client
            .submit_run(RunSubmitRequest {
                session_id: session_id.clone(),
                content: agl_content::Content::text(prompt)
                    .context("failed to encode cron skill prompt")?,
                client_submission_id: format!("cli-cron-{}", agl_ids::RequestId::generate()),
                budget: RunBudgetRequest::default(),
            })
            .await
            .context("daemon rejected the cron skill run")?;
        let run_id = accepted.run_id;
        let mut subscription = client
            .subscribe_run(RunSubscribeRequest {
                run_id: run_id.clone(),
                after_sequence: 0,
            })
            .await
            .context("failed to subscribe to the cron skill run")?;
        let finished = loop {
            match subscription.next().await? {
                Some(RunSubscriptionEvent::Event(_)) => {}
                Some(RunSubscriptionEvent::Finished(finished)) => break finished,
                None => bail!("daemon cron run subscription ended without a terminal event"),
            }
        };
        if finished.state != ProtocolRunState::Succeeded {
            let detail = finished
                .error_message
                .or(finished.error_code)
                .unwrap_or_else(|| format!("{:?}", finished.state));
            bail!("cron skill turn failed: {detail}");
        }
        let snapshot = client
            .session_presentation(SessionPresentationRequest {
                session_id: session_id.clone(),
                page_cursor: None,
            })
            .await
            .context("failed to read the completed cron skill presentation")?;
        match snapshot.items.iter().rev().find(|item| {
            matches!(
                item,
                SessionPresentationItem::AssistantMessage { .. }
                    | SessionPresentationItem::IncompleteAssistant { .. }
            )
        }) {
            Some(SessionPresentationItem::AssistantMessage { state, .. })
                if *state == AssistantItemState::Final => {}
            Some(SessionPresentationItem::IncompleteAssistant { item }) => {
                bail!("cron skill returned incomplete output: {:?}", item.reason)
            }
            Some(SessionPresentationItem::AssistantMessage { state, .. }) => {
                bail!("cron skill assistant result is not final: {state:?}")
            }
            Some(_) => unreachable!("cron assistant filter excludes non-assistant items"),
            None => bail!("daemon completed the cron skill run without an assistant result"),
        }
        Ok::<_, anyhow::Error>(run_id)
    }
    .await;

    let finish = client
        .finish_session(SessionFinishRequest {
            session_id: session_id.clone(),
            reason: SessionFinishReason::Eof,
        })
        .await
        .context("failed to finish the daemon cron skill session");
    match (execution, finish) {
        (Ok(run_id), Ok(_)) => Ok(format!(
            "skill:{}:session:{session_id}:run:{run_id}",
            job.target_ref
        )),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(execution), Err(finish)) => Err(anyhow!(
            "{execution:#}; additionally failed to finish cron session: {finish:#}"
        )),
    }
}

fn run_mock_skill_cron_target(job: &CronJob) -> Result<String> {
    let _prompt = render_cron_skill_prompt(job)?;
    Ok(format!("skill:{}:mock", job.target_ref))
}

fn prompt_preview(prompt: &str) -> String {
    const LIMIT: usize = 160;
    if prompt.chars().count() <= LIMIT {
        return prompt.to_string();
    }
    prompt.chars().take(LIMIT).collect()
}

fn idempotency_report(store: &AglStore, outcome: &IdempotencyOutcome) -> Result<serde_json::Value> {
    let (admission, initial) = match outcome {
        IdempotencyOutcome::Inserted(record) => ("inserted", record),
        IdempotencyOutcome::Replayed(record) => ("replayed", record),
    };
    let final_record = store
        .idempotency_record(&initial.namespace, &initial.key)
        .context("failed to read final idempotency record")?
        .unwrap_or_else(|| initial.clone());
    Ok(serde_json::json!({
        "admission": admission,
        "namespace": initial.namespace,
        "key": initial.key,
        "fingerprint": initial.fingerprint,
        "initial_status": initial.status.as_str(),
        "final_status": final_record.status.as_str(),
        "result_ref": final_record.result_ref,
        "created_at": initial.created_at,
        "updated_at": final_record.updated_at,
    }))
}

struct CliCronExecutor<'a> {
    store: &'a AglStore,
    runtime: &'a AgentLibreRuntimeConfig,
    mock_skill_execution: bool,
    inference: Option<CliCronInference>,
}

impl CronTargetExecutor for CliCronExecutor<'_> {
    fn execute(&mut self, job: &CronJob, _scheduled_for: &str) -> CronExecution {
        execute_cron_target(
            job,
            self.store,
            self.runtime,
            self.mock_skill_execution,
            self.inference.as_mut(),
        )
    }
}

struct CliStoreCronNotifier<'a> {
    store: &'a AglStore,
}

impl CronNotifier for CliStoreCronNotifier<'_> {
    fn notify(&mut self, notification: CronNotification) -> Result<()> {
        if !notification.notify_ref.starts_with("matrix-room:") {
            return Ok(());
        }
        let body = render_cron_notification_body(&notification);
        let dedupe_key = format!("cron:{}:{}", notification.run_id, notification.notify_ref);
        self.store
            .enqueue_matrix_notification(MatrixNotificationOutboxDraft::new(
                notification.notify_ref,
                "cron",
                notification.run_id,
                dedupe_key,
                body,
            ))
            .context("failed to enqueue Matrix notification")?;
        Ok(())
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn cron_target_kind(kind: CronTargetKindArg) -> CronTargetKind {
    match kind {
        CronTargetKindArg::Skill => CronTargetKind::Skill,
        CronTargetKindArg::Builtin => CronTargetKind::Builtin,
    }
}

fn run_skill(command: SkillCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    match command {
        SkillCommand::List(options) => run_skill_list(options, runtime),
        SkillCommand::Inspect(options) => run_skill_inspect(options, runtime),
        SkillCommand::Status(options) => run_skill_status(options, runtime),
        SkillCommand::Verify(options) => run_skill_verify(options, runtime),
        SkillCommand::Trust(options) => run_skill_trust(options, runtime),
        SkillCommand::Revoke(options) => run_skill_revoke(options, runtime),
    }
}

fn run_skill_list(options: SkillListOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let resolved = cli_workspace_skills(runtime)?;
    let registry = &resolved.registry;
    let limit = options.limit.unwrap_or(100).min(100);
    let skills = registry
        .skills()
        .iter()
        .filter(|skill| match options.source {
            SkillListSourceArg::All => true,
            SkillListSourceArg::Core => skill.harness.source == SkillSource::Core,
            SkillListSourceArg::Community => skill.harness.source == SkillSource::Community,
            SkillListSourceArg::Local => skill.harness.source == SkillSource::Local,
        })
        .filter(|skill| !options.trusted_only || skill.permits_context_injection())
        .take(limit)
        .collect::<Vec<_>>();
    if options.json {
        crate::print_json(
            &skills
                .iter()
                .map(|skill| {
                    serde_json::json!({
                        "id": skill.harness.id,
                        "name": skill.harness.name,
                        "package": skill.harness.package,
                        "trust": format!("{:?}", skill.trust),
                        "usable": skill.permits_context_injection(),
                    })
                })
                .collect::<Vec<_>>(),
        )
    } else {
        for skill in skills {
            println!(
                "skill id={} name={} source={} trust={:?} usable={}",
                skill.harness.id,
                skill.harness.name,
                skill.harness.source.as_str(),
                skill.trust,
                skill.permits_context_injection()
            );
        }
        Ok(())
    }
}

fn run_skill_inspect(
    options: SkillInspectOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let resolved = cli_workspace_skills(runtime)?;
    let registry = &resolved.registry;
    let skill = registry
        .skills()
        .iter()
        .find(|skill| {
            skill.harness.name == options.name || skill.harness.id.as_str() == options.name
        })
        .with_context(|| format!("skill not found: {}", options.name))?;
    if options.runtime {
        ensure!(
            skill.permits_context_injection(),
            "skill is not runtime usable"
        );
    }
    if options.json {
        crate::print_json(&serde_json::json!({
            "id": skill.harness.id,
            "name": skill.harness.name,
            "description": skill.harness.description,
            "package": skill.harness.package,
            "trust": format!("{:?}", skill.trust),
            "usable": skill.permits_context_injection(),
            "permissions": skill.harness.permissions,
        }))
    } else {
        println!("skill id={} name={}", skill.harness.id, skill.harness.name);
        println!("description={}", skill.harness.description);
        println!("usable={}", skill.permits_context_injection());
        print_skill_permissions("skill", &skill.harness.permissions);
        Ok(())
    }
}

fn run_skill_status(options: SkillStatusOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let resolved = cli_workspace_skills(runtime)?;
    let registry = &resolved.registry;
    let invalid = registry
        .skills()
        .iter()
        .filter(|skill| !skill.permits_context_injection())
        .count();
    let lock_valid = resolved.external_package_count == 0 || resolved.package_lock_present;
    let report = serde_json::json!({
        "package_count": registry.skills().len(),
        "external_package_count": resolved.external_package_count,
        "package_lock_present": resolved.package_lock_present,
        "invalid_or_untrusted": invalid,
    });
    crate::print_json_or(options.json, &report, || {
        println!("package_count={}", registry.skills().len());
        println!("package_lock_present={}", resolved.package_lock_present);
        println!("invalid_or_untrusted={invalid}");
    })?;
    if options.strict && (invalid != 0 || !lock_valid) {
        bail!("Skill package status is not healthy");
    }
    Ok(())
}

fn run_skill_verify(options: SkillVerifyOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    run_skill_status(
        SkillStatusOptions {
            json: options.json,
            strict: true,
        },
        runtime,
    )
}

fn run_skill_trust(options: SkillTrustOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    ensure!(options.yes, "skill trust requires --yes");
    update_cli_skill_trust(&options.name, runtime, true)
}

fn run_skill_revoke(options: SkillRevokeOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    update_cli_skill_trust(&options.name, runtime, false)
}

fn cli_workspace_skills(
    runtime: &AgentLibreRuntimeConfig,
) -> Result<agl_runtime::WorkspaceSkillRegistry> {
    agl_runtime::resolve_workspace_skills(
        &runtime.paths,
        agl_repo::package_composition_input(std::env::current_dir()?)?,
        runtime.paths.state_dir.join("skill-trust.toml"),
    )
}

fn update_cli_skill_trust(
    name: &str,
    runtime: &AgentLibreRuntimeConfig,
    approve: bool,
) -> Result<()> {
    let resolved = cli_workspace_skills(runtime)?;
    ensure!(
        resolved.package_lock_present,
        "workspace Skill trust requires .agl/package-lock.toml"
    );
    let skill = resolved
        .registry
        .skills()
        .iter()
        .find(|skill| skill.harness.id.as_str() == name || skill.harness.name == name)
        .with_context(|| format!("skill not found: {name}"))?;
    ensure!(
        skill.harness.source != SkillSource::Core,
        "core Skill trust is binary-owned"
    );
    let path = runtime.paths.state_dir.join("skill-trust.toml");
    let mut store = SkillTrustStore::load(&path)?;
    if approve {
        store.trust(&skill.harness);
    } else {
        store.revoke(&skill.harness);
    }
    store.write_atomic(&path)?;
    println!(
        "skill={} identity={} trust={}",
        name,
        agl_skill::skill_identity(&skill.harness),
        if approve { "trusted" } else { "revoked" }
    );
    Ok(())
}

fn inference_options_from_serve_options(
    options: &ServeOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<InferenceOptions> {
    let run_options = RunOptions {
        function_ref: options.function_ref.clone(),
        workspace_root: options.workspace_root.clone(),
        ..RunOptions::default()
    };
    let function = resolve_run_function_defaults(&run_options, runtime)?;
    let max_output_tokens = options
        .max_output_tokens
        .or_else(|| {
            function
                .as_ref()
                .and_then(|function| function.max_output_tokens)
        })
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    let tool_mode = options
        .tool_mode
        .or_else(|| {
            function
                .as_ref()
                .and_then(|function| function.tool_mode)
                .map(args_tool_mode_from_function)
        })
        .unwrap_or(args::ToolAccessMode::ReadOnly);
    let memory = options.memory
        || function
            .as_ref()
            .map(|function| function.memory_enabled)
            .unwrap_or(false);

    Ok(InferenceOptions {
        function_ref: options.function_ref.clone(),
        artifact_root: options.artifact_root.clone(),
        workspace_root: options.workspace_root.clone(),
        max_output_tokens,
        tool_mode: chat_tool_mode(tool_mode),
        skills: options.skills.clone(),
        memory,
    })
}

fn print_cli_error(err: &anyhow::Error) {
    let message = format!("{err:#}");
    if message.starts_with("error: ") {
        eprint!("{message}");
        if !message.ends_with('\n') {
            eprintln!();
        }
    } else {
        eprintln!("error: {message}");
    }
}

fn inference_options_from_run_options(
    options: &RunOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<InferenceOptions> {
    let function = resolve_run_function_defaults(options, runtime)?;
    let max_output_tokens = options
        .max_output_tokens
        .or_else(|| {
            function
                .as_ref()
                .and_then(|function| function.max_output_tokens)
        })
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    let tool_mode = options
        .tool_mode
        .or_else(|| {
            function
                .as_ref()
                .and_then(|function| function.tool_mode)
                .map(args_tool_mode_from_function)
        })
        .unwrap_or(args::ToolAccessMode::ReadOnly);
    let memory = options.memory
        || function
            .as_ref()
            .map(|function| function.memory_enabled)
            .unwrap_or(false);

    Ok(InferenceOptions {
        function_ref: options.function_ref.clone(),
        artifact_root: options.artifact_root.clone(),
        workspace_root: options.workspace_root.clone(),
        max_output_tokens,
        tool_mode: chat_tool_mode(tool_mode),
        skills: options.skills.clone(),
        memory,
    })
}

fn chat_options_from_run_options(
    options: &RunOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<ChatOptions> {
    Ok(ChatOptions {
        inference: inference_options_from_run_options(options, runtime)?,
        workspace_root: options.workspace_root.clone(),
        session_id: options.session_id.clone(),
        no_history: options.no_history,
        new_session: options.new_session,
    })
}

fn one_shot_chat_options_from_run_options(
    options: &RunOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<ChatOptions> {
    let mut chat_options = chat_options_from_run_options(options, runtime)?;
    chat_options.session_id = None;
    chat_options.no_history = true;
    chat_options.new_session = true;
    Ok(chat_options)
}

fn resolve_run_function_defaults(
    options: &RunOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<Option<agl_function::RuntimeFunction>> {
    options
        .function_ref
        .as_deref()
        .map(|reference| {
            let workspace_root =
                runtime.resolve_workspace_root(options.workspace_root.as_deref())?;
            agl_runtime::resolve_composed_runtime_function(
                &runtime.paths,
                agl_repo::package_composition_input(&workspace_root)?,
                reference,
                true,
            )
            .with_context(|| format!("failed to resolve function `{reference}`"))
        })
        .transpose()
}

fn args_tool_mode_from_function(mode: agl_function::FunctionToolMode) -> args::ToolAccessMode {
    match mode {
        agl_function::FunctionToolMode::ReadOnly => args::ToolAccessMode::ReadOnly,
        agl_function::FunctionToolMode::Write => args::ToolAccessMode::Write,
        agl_function::FunctionToolMode::Execute => args::ToolAccessMode::Execute,
        agl_function::FunctionToolMode::Approve => args::ToolAccessMode::Approve,
        agl_function::FunctionToolMode::Admin => args::ToolAccessMode::Admin,
    }
}

fn chat_tool_mode(mode: args::ToolAccessMode) -> ChatToolAccessMode {
    match mode {
        args::ToolAccessMode::ReadOnly => ChatToolAccessMode::ReadOnly,
        args::ToolAccessMode::Write => ChatToolAccessMode::Write,
        args::ToolAccessMode::Execute => ChatToolAccessMode::Execute,
        args::ToolAccessMode::Approve => ChatToolAccessMode::Approve,
        args::ToolAccessMode::Admin => ChatToolAccessMode::Admin,
    }
}

fn apply_workspace_default_function_to_run(
    options: &mut RunOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    if options.function_ref.is_some() {
        return Ok(());
    }
    let workspace_root = runtime.resolve_workspace_root(options.workspace_root.as_deref())?;
    let function = read_workspace_default_function(&workspace_root)?
        .unwrap_or_else(|| agl_repo::DEFAULT_FUNCTION.to_string());
    options.function_ref = Some(function);
    Ok(())
}

fn apply_workspace_default_function_to_serve(
    options: &mut ServeOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    if options.function_ref.is_some() {
        return Ok(());
    }
    let workspace_root = runtime.resolve_workspace_root(options.workspace_root.as_deref())?;
    let function = read_workspace_default_function(&workspace_root)?
        .unwrap_or_else(|| agl_repo::DEFAULT_FUNCTION.to_string());
    options.function_ref = Some(function);
    Ok(())
}

fn process_local_inference_host(runtime: &AgentLibreRuntimeConfig) -> Result<InferenceHost> {
    InferenceHost::start_with_journal_root(
        InferenceHostConfig::development_default(
            runtime.paths.inference_state_root().join("authority"),
            runtime.paths.default_artifact_root(),
            std::time::Duration::from_secs(runtime.inference.residency.context_idle_seconds),
            std::time::Duration::from_secs(runtime.inference.residency.model_idle_seconds),
        )?,
        runtime.paths.inference_state_root().join("attempts"),
    )
    .context("failed to start process-local inference host")
}

pub(crate) fn daemon_first_inference_inventory(
    runtime: &AgentLibreRuntimeConfig,
) -> Result<Vec<InferenceDeviceInfo>> {
    let socket_path = default_socket_path(&runtime.paths);
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build daemon inference inventory runtime")?;
    let connection = async_runtime.block_on(runtime::connect_daemon(&socket_path));
    let authority = inference_authority_decision(
        InferenceAuthoritySurface::InitInventory,
        classify_daemon_connection(&connection),
    );
    match (authority, connection) {
        (InferenceAuthorityDecision::Daemon, Ok(client)) => {
            let hello = client.hello().context("failed to read daemon identity")?;
            ensure!(
                hello.tools.contains(&DaemonTool::InferenceInventory),
                "daemon at {} does not provide canonical inference inventory",
                socket_path.display()
            );
            let inventory = async_runtime
                .block_on(client.inference_inventory())
                .context("daemon inference inventory failed")?;
            Ok(inventory
                .devices
                .into_iter()
                .map(|device| InferenceDeviceInfo {
                    physical_device_id: device.physical_device_id,
                    pci_device_id: device.pci_device_id,
                    pci_subsystem_id: device.pci_subsystem_id,
                    driver_build_id: device.driver_build_id,
                    backend_name: device.backend_name,
                    description: device.description,
                    kind: match device.kind {
                        ProtocolInferenceDeviceKind::Cpu => InferenceDeviceKind::Cpu,
                        ProtocolInferenceDeviceKind::DiscreteGpu => {
                            InferenceDeviceKind::DiscreteGpu
                        }
                        ProtocolInferenceDeviceKind::IntegratedGpu => {
                            InferenceDeviceKind::IntegratedGpu
                        }
                        ProtocolInferenceDeviceKind::Accelerator => {
                            InferenceDeviceKind::Accelerator
                        }
                        ProtocolInferenceDeviceKind::Metadata => InferenceDeviceKind::Metadata,
                        ProtocolInferenceDeviceKind::Unknown => InferenceDeviceKind::Unknown,
                    },
                    free_memory_bytes: device.free_memory_bytes,
                    total_memory_bytes: device.total_memory_bytes,
                    usable: device.usable,
                    supports_gpu_offload: device.supports_gpu_offload,
                })
                .collect())
        }
        (InferenceAuthorityDecision::Standalone, Err(ClientError::DaemonUnavailable(_))) => {
            let host = process_local_inference_host(runtime)
                .context("no daemon is running and standalone inference inventory failed")?;
            let inventory = InferenceClientHandle::from(host.clone())
                .device_inventory()
                .context("standalone inference inventory failed")?;
            host.shutdown();
            Ok(inventory)
        }
        (InferenceAuthorityDecision::Reject, Err(error)) => bail!(
            "daemon at {} is active but incompatible or unhealthy ({error}); refusing standalone inference",
            socket_path.display()
        ),
        _ => unreachable!("inference authority decision diverged from connection state"),
    }
}

enum CliCronInference {
    Daemon {
        runtime: tokio::runtime::Runtime,
        client: AgentLibreClient,
    },
    Standalone {
        _host: Box<InferenceHost>,
        client: InferenceClientHandle,
    },
}

impl CliCronInference {
    fn run_skill(&self, job: &CronJob, runtime: &AgentLibreRuntimeConfig) -> Result<String> {
        match self {
            Self::Daemon {
                runtime: async_runtime,
                client,
            } => async_runtime.block_on(run_skill_cron_target_via_daemon(client, job, runtime)),
            Self::Standalone { client, .. } => run_skill_cron_target(job, runtime, client),
        }
    }
}

fn daemon_first_cron_inference(runtime: &AgentLibreRuntimeConfig) -> Result<CliCronInference> {
    let socket_path = default_socket_path(&runtime.paths);
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build daemon-first cron runtime")?;
    let connection = async_runtime.block_on(runtime::connect_daemon(&socket_path));
    let authority = inference_authority_decision(
        InferenceAuthoritySurface::Cron,
        classify_daemon_connection(&connection),
    );
    match (authority, connection) {
        (InferenceAuthorityDecision::Daemon, Ok(client)) => {
            let required = [
                DaemonTool::SessionOpen,
                DaemonTool::RunSubmit,
                DaemonTool::RunSubscribe,
                DaemonTool::SessionPresentation,
                DaemonTool::SessionFinish,
            ];
            let hello = client.hello().context("failed to read daemon identity")?;
            if let Some(missing) = required
                .into_iter()
                .find(|tool| !hello.tools.contains(tool))
            {
                bail!(
                    "daemon at {} lacks required cron tool {missing:?}; refusing standalone inference while it is active",
                    socket_path.display()
                );
            }
            Ok(CliCronInference::Daemon {
                runtime: async_runtime,
                client,
            })
        }
        (InferenceAuthorityDecision::Standalone, Err(ClientError::DaemonUnavailable(_))) => {
            let host = process_local_inference_host(runtime).context(
                "no daemon is running and the isolated standalone cron inference authority failed",
            )?;
            let client = InferenceClientHandle::from(host.clone());
            Ok(CliCronInference::Standalone {
                _host: Box::new(host),
                client,
            })
        }
        (InferenceAuthorityDecision::Reject, Err(error)) => bail!(
            "daemon at {} is active but incompatible or unhealthy ({error}); refusing standalone cron inference",
            socket_path.display()
        ),
        _ => unreachable!("daemon connection classification and authority decision diverged"),
    }
}

#[derive(Debug, serde::Serialize)]
struct RunFailureDiagnostic {
    code: String,
    message: String,
    context: RunFailureContext,
}

#[derive(Debug, serde::Serialize)]
struct RunFailureContext {
    session_id: String,
    run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt_id: Option<String>,
    evidence_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    cause_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime_resolution: Option<serde_json::Value>,
}

#[derive(Debug)]
struct RunCommandFailure {
    diagnostic: RunFailureDiagnostic,
}

impl fmt::Display for RunCommandFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let context = &self.diagnostic.context;
        write!(
            formatter,
            "turn failed ({}): {}\nsession_id={}\nrun_id={}",
            self.diagnostic.code, self.diagnostic.message, context.session_id, context.run_id
        )?;
        if let Some(attempt_id) = &context.attempt_id {
            write!(formatter, "\nattempt_id={attempt_id}")?;
        }
        write!(
            formatter,
            "\nruntime_resolution={}",
            context.evidence_path.display()
        )?;
        if let Some(evidence_error) = &context.evidence_error {
            write!(formatter, "\nevidence_error={evidence_error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for RunCommandFailure {}

fn print_run_error_json(error: &anyhow::Error) {
    let envelope = if let Some(failure) = error.downcast_ref::<RunCommandFailure>() {
        serde_json::json!({ "error": failure.diagnostic })
    } else {
        serde_json::json!({
            "error": {
                "code": "run_error",
                "message": format!("{error:#}"),
                "context": {}
            }
        })
    };
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&envelope).expect("run error envelope is serializable")
    );
}

fn daemon_runtime_resolution_path(
    runtime: &AgentLibreRuntimeConfig,
    session_id: &SessionId,
    run_id: &RunId,
) -> PathBuf {
    runtime
        .paths
        .sessions_root()
        .join(session_id.as_str())
        .join("runs")
        .join(run_id.as_str())
        .join("runtime-resolution.json")
}

fn daemon_run_failure(
    runtime: &AgentLibreRuntimeConfig,
    session_id: &SessionId,
    run_id: &RunId,
    cause_code: Option<String>,
    message: String,
) -> anyhow::Error {
    let evidence_path = daemon_runtime_resolution_path(runtime, session_id, run_id);
    run_failure_from_evidence_path(session_id, run_id, cause_code, message, evidence_path)
}

fn run_failure_from_evidence_path(
    session_id: &SessionId,
    run_id: &RunId,
    cause_code: Option<String>,
    message: String,
    evidence_path: PathBuf,
) -> anyhow::Error {
    let (runtime_resolution, evidence_error) = match std::fs::read(&evidence_path) {
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(resolution) => (Some(resolution), None),
            Err(error) => (
                None,
                Some(format!(
                    "canonical runtime resolution is invalid JSON: {error}"
                )),
            ),
        },
        Err(error) => (
            None,
            Some(format!(
                "canonical runtime resolution is unavailable: {error}"
            )),
        ),
    };
    let attempt_id = runtime_resolution
        .as_ref()
        .and_then(|resolution| resolution.get("attempt_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let admission_code = runtime_resolution
        .as_ref()
        .and_then(|resolution| resolution.pointer("/admission/error/code"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let code = if evidence_error.is_some() {
        "runtime_resolution_unavailable".to_owned()
    } else {
        admission_code
            .or_else(|| cause_code.clone())
            .unwrap_or_else(|| "chat_turn_failed".to_owned())
    };
    RunCommandFailure {
        diagnostic: RunFailureDiagnostic {
            code,
            message,
            context: RunFailureContext {
                session_id: session_id.as_str().to_owned(),
                run_id: run_id.as_str().to_owned(),
                attempt_id,
                evidence_path,
                cause_code,
                evidence_error,
                runtime_resolution,
            },
        },
    }
    .into()
}

#[derive(Debug, serde::Serialize)]
struct RunSuccess {
    status: &'static str,
    session_id: String,
    run_id: String,
    answer: String,
    runtime_resolution: PathBuf,
}

fn run_one_shot(mut options: RunOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    apply_workspace_default_function_to_run(&mut options, runtime)?;
    run_one_shot_raw(options, runtime)
}

fn run_one_shot_raw(options: RunOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "run", "starting command");
    let socket_path = default_socket_path(&runtime.paths);
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build daemon-first run runtime")?;
    let connection = async_runtime.block_on(runtime::connect_daemon(&socket_path));
    let authority = inference_authority_decision(
        InferenceAuthoritySurface::DirectRun,
        classify_daemon_connection(&connection),
    );
    match (authority, connection) {
        (InferenceAuthorityDecision::Daemon, Ok(client)) => async_runtime.block_on(
            run_one_shot_via_daemon(client, options, runtime, &socket_path),
        ),
        (InferenceAuthorityDecision::Standalone, Err(ClientError::DaemonUnavailable(_))) => {
            run_one_shot_standalone(options, runtime)
        }
        (InferenceAuthorityDecision::Reject, Err(error)) => bail!(
            "daemon at {} is active but incompatible or unhealthy ({error}); refusing standalone inference",
            socket_path.display()
        ),
        _ => unreachable!("daemon connection classification and authority decision diverged"),
    }
}

async fn run_one_shot_via_daemon(
    client: AgentLibreClient,
    options: RunOptions,
    runtime: &AgentLibreRuntimeConfig,
    socket_path: &std::path::Path,
) -> Result<()> {
    let json = options.json;
    if options.function_ref.is_none()
        || options.artifact_root.is_some()
        || options.max_output_tokens.is_some()
        || options.memory
    {
        bail!(
            "the active daemon at {} cannot represent this direct-run override; stop it explicitly before using standalone --artifact-root/--max-output-tokens/--memory",
            socket_path.display()
        );
    }
    let required = [
        DaemonTool::SessionOpen,
        DaemonTool::RunSubmit,
        DaemonTool::RunSubscribe,
        DaemonTool::SessionPresentation,
        DaemonTool::SessionFinish,
    ];
    let hello = client.hello().context("failed to read daemon identity")?;
    if let Some(missing) = required
        .into_iter()
        .find(|tool| !hello.tools.contains(tool))
    {
        bail!(
            "daemon at {} lacks required tool {missing:?}; refusing standalone inference while it is active",
            socket_path.display()
        );
    }

    let prompt = options
        .prompt
        .clone()
        .context("run requires PROMPT or --prompt TEXT")?;
    let resolved = inference_options_from_run_options(&options, runtime)?;
    let workspace_root = runtime.resolve_workspace_root(options.workspace_root.as_deref())?;
    let opened = client
        .open_session(SessionOpenRequest {
            session_id: None,
            new_session: true,
            workspace_root: Some(workspace_root.to_string_lossy().into_owned()),
            function_ref: options.function_ref.clone(),
            skills: resolved.skills.clone(),
            tool_mode: protocol_tool_mode_from_chat(resolved.tool_mode),
        })
        .await
        .context("daemon rejected the one-shot session")?;
    let session_id = opened.session_id;

    let turn = async {
        let accepted = client
            .submit_run(RunSubmitRequest {
                session_id: session_id.clone(),
                content: agl_content::Content::text(prompt)
                    .context("failed to encode one-shot prompt")?,
                client_submission_id: format!("cli-run-{}", agl_ids::RequestId::generate()),
                budget: RunBudgetRequest::default(),
            })
            .await
            .context("daemon rejected the one-shot run")?;
        let run_id = accepted.run_id;
        let mut subscription = client
            .subscribe_run(RunSubscribeRequest {
                run_id: run_id.clone(),
                after_sequence: 0,
            })
            .await
            .context("failed to subscribe to the one-shot run")?;
        let finished = loop {
            match subscription.next().await? {
                Some(RunSubscriptionEvent::Event(_)) => {}
                Some(RunSubscriptionEvent::Finished(finished)) => break finished,
                None => bail!("daemon run subscription ended without a terminal event"),
            }
        };
        if finished.state != ProtocolRunState::Succeeded {
            let detail = finished
                .error_message
                .clone()
                .or_else(|| finished.error_code.clone())
                .unwrap_or_else(|| format!("{:?}", finished.state));
            return Err(daemon_run_failure(
                runtime,
                &session_id,
                &run_id,
                finished.error_code,
                detail,
            ));
        }
        let snapshot = client
            .session_presentation(SessionPresentationRequest {
                session_id: session_id.clone(),
                page_cursor: None,
            })
            .await
            .context("failed to read the completed one-shot presentation")?;
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
            .context("daemon completed the run without an assistant result")?;
        let answer = match item {
            SessionPresentationItem::AssistantMessage { content, state, .. }
                if *state == AssistantItemState::Final =>
            {
                content
                    .text_only()
                    .context("one-shot assistant result is not text-only")
            }
            SessionPresentationItem::IncompleteAssistant { item } => {
                let partial = item
                    .content
                    .text_only()
                    .context("incomplete assistant result is not text-only")?;
                println!("{partial}");
                bail!("turn output is incomplete: {:?}", item.reason)
            }
            SessionPresentationItem::AssistantMessage { state, .. } => {
                bail!("assistant result is not final: {state:?}")
            }
            _ => unreachable!("assistant result filter only admits assistant items"),
        }?;
        Ok(RunSuccess {
            status: "succeeded",
            session_id: session_id.as_str().to_owned(),
            run_id: run_id.as_str().to_owned(),
            answer,
            runtime_resolution: daemon_runtime_resolution_path(runtime, &session_id, &run_id),
        })
    }
    .await;

    let finish = client
        .finish_session(SessionFinishRequest {
            session_id,
            reason: SessionFinishReason::Eof,
        })
        .await
        .context("failed to finish the daemon one-shot session");
    match (turn, finish) {
        (Ok(outcome), Ok(_)) => {
            if json {
                print_json(&outcome)?;
            } else {
                println!("{}", outcome.answer);
            }
            Ok(())
        }
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(turn), Err(finish)) => Err(anyhow!("{turn:#}; additionally {finish:#}")),
    }
}

fn run_one_shot_standalone(options: RunOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let json = options.json;
    let prompt = options
        .prompt
        .clone()
        .context("run requires PROMPT or --prompt TEXT")?;
    let chat_options = one_shot_chat_options_from_run_options(&options, runtime)?;
    let tool_mode = chat_options.inference.tool_mode;
    let inference_host = process_local_inference_host(runtime)?;
    let inference_client = InferenceClientHandle::from(inference_host);
    let chat = OneShotSession::open(chat_options, runtime, inference_client)?;
    let summary = chat.summary()?;
    tracing::info!(
        target: "agentlibre::app",
        session_id = %summary.session_id,
        artifact_root = %summary.artifact_root.display(),
        workspace_root = %summary.workspace_root.display(),
        tool_mode = tool_mode.as_str(),
        "runtime loop host initialized"
    );
    let output = chat.run_user_turn(&prompt);
    let finish = chat.finish_eof_if_needed();
    let output = output?;
    finish?;
    tracing::info!(
        target: "agentlibre::app",
        session_id = %chat.session_id(),
        run_id = %output.run_id,
        turn_id = %output.turn_id,
        generated_requests = output.generated_requests,
        "runtime turn finished"
    );
    let run_id = output.run_id.clone();
    let session_id = chat.session_id().clone();
    let evidence_path = summary
        .artifact_root
        .join("runs")
        .join(run_id.as_str())
        .join("runtime-resolution.json");
    match output.status {
        ChatTurnStatus::Answered { answer } => {
            if json {
                print_json(&RunSuccess {
                    status: "succeeded",
                    session_id: session_id.as_str().to_owned(),
                    run_id: run_id.as_str().to_owned(),
                    answer,
                    runtime_resolution: evidence_path,
                })?;
            } else {
                println!("{answer}");
            }
        }
        ChatTurnStatus::Incomplete { partial, reason } => {
            if !json {
                println!("{partial}");
            }
            return Err(run_failure_from_evidence_path(
                &session_id,
                &run_id,
                Some("incomplete_output".to_owned()),
                format!("turn output is incomplete: {}", reason.as_str()),
                evidence_path,
            ));
        }
        ChatTurnStatus::Stopped { reason } => {
            if json {
                print_json(&serde_json::json!({
                    "status": "stopped",
                    "session_id": session_id,
                    "run_id": run_id,
                    "reason": reason.as_str(),
                    "runtime_resolution": evidence_path,
                }))?;
            } else {
                println!("stopped=true reason={}", reason.as_str());
            }
        }
        ChatTurnStatus::Failed { message } => {
            return Err(run_failure_from_evidence_path(
                &session_id,
                &run_id,
                Some("chat_turn_failed".to_owned()),
                message,
                evidence_path,
            ));
        }
        ChatTurnStatus::Cancelled => {
            return Err(run_failure_from_evidence_path(
                &session_id,
                &run_id,
                Some("turn_cancelled".to_owned()),
                "turn cancelled".to_owned(),
                evidence_path,
            ));
        }
    }
    Ok(())
}

fn protocol_tool_mode_from_chat(mode: ChatToolAccessMode) -> ProtocolToolMode {
    match mode {
        ChatToolAccessMode::ReadOnly => ProtocolToolMode::ReadOnly,
        ChatToolAccessMode::Write => ProtocolToolMode::Write,
        ChatToolAccessMode::Execute => ProtocolToolMode::Execute,
        ChatToolAccessMode::Approve => ProtocolToolMode::Approve,
        ChatToolAccessMode::Admin => ProtocolToolMode::Admin,
    }
}

fn run_serve(mut options: ServeOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    apply_workspace_default_function_to_serve(&mut options, runtime)?;
    run_serve_raw(options, runtime)
}

fn run_serve_raw(options: ServeOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "serve", "starting command");
    let mut daemon_options = DaemonOptions::new(
        &runtime.paths,
        inference_options_from_serve_options(&options, runtime)?,
    );
    daemon_options.listener_source = if options.systemd_activation {
        agl_daemon::ListenerSource::Systemd
    } else if let Some(socket_path) = options.socket_path {
        agl_daemon::ListenerSource::Bind(socket_path)
    } else {
        daemon_options.listener_source
    };
    println!("listener={}", daemon_options.listener_source);
    DaemonServer::new(runtime.clone(), daemon_options).run_foreground()
}

fn run_daemon_status(
    options: DaemonStatusOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "daemon status", "starting command");
    let socket_path = options
        .socket_path
        .unwrap_or_else(|| default_socket_path(&runtime.paths));
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build daemon status runtime")?;
    if options.overview {
        println!("agentLIBRE command client");
        println!("sessions=agl session --help");
        println!("interactive_ui=agl-terminal");
    }
    match async_runtime.block_on(runtime::connect_daemon(&socket_path)) {
        Ok(client) => {
            if options.overview {
                let hello = client.hello()?;
                let sessions =
                    async_runtime.block_on(client.list_sessions(SessionListRequest::default()))?;
                println!("daemon=running");
                println!("daemon_instance_id={}", hello.daemon_instance_id);
                println!("session_count={}", sessions.sessions.len());
                for session in sessions.sessions {
                    println!("session={} status={:?}", session.session_id, session.status);
                }
                return Ok(());
            }
            let inspection = client.hello().and_then(|hello| {
                async_runtime
                    .block_on(client.inference_status(InferenceStatusRequest {
                        detail: options.detail,
                    }))
                    .map(|status| (hello, status))
            });
            match inspection {
                Ok((hello, status)) => {
                    println!("state=running");
                    println!("socket_path={}", socket_path.display());
                    println!("daemon_instance_id={}", hello.daemon_instance_id);
                    println!("protocol_version={}", hello.protocol_version);
                    println!("product_version={}", hello.product_version);
                    println!(
                        "runtime_kind={}",
                        runtime_generation_kind_label(hello.daemon_runtime.kind)
                    );
                    println!(
                        "runtime_generation_id={}",
                        hello.daemon_runtime.generation_id
                    );
                    println!(
                        "runtime_builtin_catalog_digest={}",
                        hello.daemon_runtime.builtin_catalog_digest
                    );
                    println!(
                        "runtime_executable_digest={}",
                        hello.daemon_runtime.executable_digest
                    );
                    println!("engine_protocol_id={}", hello.engine_protocol_id);
                    println!("inference_engine_protocol_id={}", status.engine_protocol_id);
                    println!(
                        "engine_state={}",
                        inference_engine_state_label(status.engine_state)
                    );
                    println!("engine_pid={}", optional_status_value(status.engine_pid));
                    println!(
                        "engine_generation={}",
                        optional_status_value(status.engine_generation)
                    );
                    println!(
                        "accelerator_physical_device_id={}",
                        status.physical_device_id.as_deref().unwrap_or("none")
                    );
                    println!("accelerator_reserved_bytes={}", status.reserved_bytes);
                    println!(
                        "accelerator_cooldown_not_before_unix_ms={}",
                        optional_status_value(status.cooldown_not_before_unix_ms)
                    );
                    println!("resident_models={}", status.resident_models);
                    println!("resident_contexts={}", status.resident_contexts);
                    println!(
                        "next_residency_deadline_after_ms={}",
                        optional_status_value(status.next_residency_deadline_after_ms)
                    );
                    println!(
                        "last_release_reason={}",
                        status
                            .last_release_reason
                            .map(model_release_reason_label)
                            .unwrap_or("none")
                    );
                    println!(
                        "last_release_outcome={}",
                        status
                            .last_release_outcome
                            .map(model_release_outcome_label)
                            .unwrap_or("none")
                    );
                    println!(
                        "automatic_context_unloads={}",
                        status.automatic_context_unloads
                    );
                    println!("automatic_model_unloads={}", status.automatic_model_unloads);
                    println!("manual_unloads={}", status.manual_unloads);
                    println!("unload_failures={}", status.unload_failures);
                    if let Some(digests) = status.resident_model_digests {
                        for (index, digest) in digests.iter().enumerate() {
                            println!("resident_model_digest.{index}={digest}");
                        }
                        println!(
                            "resident_model_digests_truncated={}",
                            status.resident_model_digests_truncated.unwrap_or(false)
                        );
                    }
                    Ok(())
                }
                Err(err) => {
                    println!("state=unhealthy");
                    println!("socket_path={}", socket_path.display());
                    println!("error={err:#}");
                    Ok(())
                }
            }
        }
        Err(ClientError::DaemonUnavailable(err)) => {
            println!("state=not_running");
            println!("socket_path={}", socket_path.display());
            println!("error={err}");
            println!("next_step=agl serve");
            tracing::debug!(
                target: "agentlibre::app",
                socket_path = %socket_path.display(),
                error = %err,
                "daemon status connection failed"
            );
            Ok(())
        }
        Err(err) => {
            println!("state=unhealthy");
            println!("socket_path={}", socket_path.display());
            println!("error={err}");
            println!("next_step=restart the daemon with the current `agl serve` binary");
            tracing::warn!(
                target: "agentlibre::app",
                socket_path = %socket_path.display(),
                error = %err,
                "daemon socket accepted a connection but the current protocol handshake failed"
            );
            Ok(())
        }
    }
}

fn model_release_reason_label(reason: ModelReleaseReason) -> &'static str {
    match reason {
        ModelReleaseReason::IdleContext => "idle_context",
        ModelReleaseReason::IdleModel => "idle_model",
        ModelReleaseReason::Manual => "manual",
        ModelReleaseReason::Shutdown => "shutdown",
        ModelReleaseReason::Capacity => "capacity",
    }
}

fn model_release_outcome_label(outcome: ModelReleaseOutcome) -> &'static str {
    match outcome {
        ModelReleaseOutcome::Released => "released",
        ModelReleaseOutcome::Failed => "failed",
        ModelReleaseOutcome::BackendLost => "backend_lost",
    }
}

fn inference_engine_state_label(state: ProtocolInferenceEngineState) -> &'static str {
    match state {
        ProtocolInferenceEngineState::Cold => "cold",
        ProtocolInferenceEngineState::Starting => "starting",
        ProtocolInferenceEngineState::Ready => "ready",
        ProtocolInferenceEngineState::Busy => "busy",
        ProtocolInferenceEngineState::CoolingDown => "cooling_down",
    }
}

fn runtime_generation_kind_label(kind: RuntimeGenerationKind) -> &'static str {
    match kind {
        RuntimeGenerationKind::Sealed => "sealed",
        RuntimeGenerationKind::Development => "development",
    }
}

fn optional_status_value<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| value.to_string())
}

fn print_skill_permissions(prefix: &str, permissions: &SkillPermissions) {
    let memory_scopes = permissions
        .memory
        .read
        .iter()
        .map(|scope| scope.as_str())
        .collect::<Vec<_>>();
    if !memory_scopes.is_empty() {
        println!(
            "{prefix}.permissions.memory.read={}",
            memory_scopes.join(",")
        );
    }
    if permissions.notes.read || permissions.notes.write {
        println!("{prefix}.permissions.notes.read={}", permissions.notes.read);
        println!(
            "{prefix}.permissions.notes.write={}",
            permissions.notes.write
        );
    }
}

fn print_cron_jobs(jobs: &[CronJob]) {
    for job in jobs {
        print_cron_job_summary(job);
    }
}

fn print_cron_job_summary(job: &CronJob) {
    println!(
        "cron id={} name={} enabled={} target={}:{} schedule={} timezone={} deleted={}",
        job.id,
        job.name,
        job.enabled,
        job.target_kind.as_str(),
        job.target_ref,
        job.schedule_expr,
        job.timezone,
        job.deleted_at.is_some()
    );
}

fn print_cron_job_detail(job: &CronJob) {
    print_cron_job_summary(job);
    println!("cron.{}.created_at={}", job.id, job.created_at);
    println!("cron.{}.updated_at={}", job.id, job.updated_at);
    if let Some(notify_ref) = &job.notify_ref {
        println!("cron.{}.notify_ref={notify_ref}", job.id);
    }
    if let Some(prompt) = &job.prompt {
        println!("cron.{}.prompt={prompt}", job.id);
    }
    if let Some(input) = &job.input {
        println!("cron.{}.input={input}", job.id);
    }
    if let Some(deleted_at) = &job.deleted_at {
        println!("cron.{}.deleted_at={deleted_at}", job.id);
    }
}

fn print_cron_runs(runs: &[CronRun]) {
    for run in runs {
        print_cron_run(run);
    }
}

fn print_cron_run(run: &CronRun) {
    println!(
        "cron_run id={} job_id={} status={} scheduled_for={}",
        run.id,
        run.job_id,
        run.status.as_str(),
        run.scheduled_for
    );
    if let Some(started_at) = &run.started_at {
        println!("cron_run.{}.started_at={started_at}", run.id);
    }
    if let Some(finished_at) = &run.finished_at {
        println!("cron_run.{}.finished_at={finished_at}", run.id);
    }
    if let Some(result_ref) = &run.result_ref {
        println!("cron_run.{}.result_ref={result_ref}", run.id);
    }
    if let Some(error) = &run.error {
        println!("cron_run.{}.error={error}", run.id);
    }
    if let Some(supervisor_run_id) = &run.supervisor_run_id {
        println!("cron_run.{}.supervisor_run_id={supervisor_run_id}", run.id);
    }
}

#[cfg(all(test, unix))]
struct TestTerminalService {
    identity: agl_terminal_protocol::ServiceIdentity,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
}

#[cfg(all(test, unix))]
impl TestTerminalService {
    fn start(runtime: &AgentLibreRuntimeConfig) -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        use std::os::unix::net::UnixListener;

        let runtime_root = runtime.paths.terminal_runtime_root();
        std::fs::create_dir_all(&runtime_root).unwrap();
        let socket_path = runtime_root.join("terminal.sock");
        let identity = agl_terminal_protocol::ServiceIdentity::new(
            agl_terminal_protocol::TerminalGenerationIdentity::new(
                agl_exec::AuthorityFingerprint::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
                "b".repeat(40),
                agl_exec::AuthorityFingerprint::new(format!("sha256:{}", "c".repeat(64))).unwrap(),
                agl_terminal_protocol::TERMINAL_PROTOCOL_VERSION,
            )
            .unwrap(),
            agl_exec::ServiceGenerationId::generate(),
        )
        .unwrap();
        let identity_path = runtime_root.join("service-identity.json");
        std::fs::write(&identity_path, serde_json::to_vec(&identity).unwrap()).unwrap();
        std::fs::set_permissions(&identity_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let listener = UnixListener::bind(socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_stop = std::sync::Arc::clone(&stop);
        let thread_identity = identity.clone();
        let thread = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            use std::sync::atomic::Ordering;
            use std::time::Duration;

            while !thread_stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
                let mut length = [0_u8; 4];
                stream.read_exact(&mut length)?;
                let mut frame = vec![0_u8; u32::from_be_bytes(length) as usize];
                stream.read_exact(&mut frame)?;
                let request: agl_terminal_protocol::TerminalRequest =
                    serde_json::from_slice(&frame)?;
                let response = match request.request {
                    agl_terminal_protocol::TerminalRequestKind::Hello => {
                        agl_terminal_protocol::TerminalResponseKind::Hello
                    }
                    agl_terminal_protocol::TerminalRequestKind::ListExecutions { .. } => {
                        agl_terminal_protocol::TerminalResponseKind::ExecutionList {
                            statuses: Vec::new(),
                        }
                    }
                    agl_terminal_protocol::TerminalRequestKind::ListTopology { .. } => {
                        agl_terminal_protocol::TerminalResponseKind::TerminalList {
                            records: Vec::new(),
                        }
                    }
                    other => anyhow::bail!("unexpected terminal fixture request: {other:?}"),
                };
                let response = agl_terminal_protocol::TerminalResponse {
                    schema: agl_terminal_protocol::TERMINAL_RESPONSE_SCHEMA.to_owned(),
                    request_id: request.request_id,
                    service: thread_identity.clone(),
                    response,
                };
                let encoded = serde_json::to_vec(&response)?;
                stream.write_all(&(encoded.len() as u32).to_be_bytes())?;
                stream.write_all(&encoded)?;
            }
            Ok(())
        });
        Self {
            identity,
            stop,
            thread: Some(thread),
        }
    }
}

#[cfg(all(test, unix))]
impl Drop for TestTerminalService {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;

        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .expect("terminal fixture thread panicked")
                .expect("terminal fixture failed");
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::sync::Arc;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(unix)]
    use std::time::Duration;

    #[cfg(unix)]
    use agl_chat::{ChatInferenceJob, InferenceClient};
    #[cfg(unix)]
    #[cfg(unix)]
    use agl_inference::{
        EngineRuntimeStatusHandle, InferenceFinishReason, InferenceResponse,
        InferenceResponseMetadata, ModelManagerStatus,
    };

    use crate::args::ConfigCommand;

    use super::*;

    fn serve_options() -> ServeOptions {
        ServeOptions {
            socket_path: None,
            systemd_activation: false,
            function_ref: None,
            artifact_root: None,
            workspace_root: None,
            max_output_tokens: None,
            tool_mode: None,
            skills: Vec::new(),
            memory: false,
        }
    }

    #[cfg(unix)]
    #[derive(Clone)]
    struct DaemonTestInference {
        calls: Arc<AtomicUsize>,
    }

    #[cfg(unix)]
    impl InferenceClient for DaemonTestInference {
        fn static_capabilities(&self) -> anyhow::Result<agl_model::HostCapabilities> {
            Ok(agl_model::HostCapabilities {
                physical_host_bytes: 64_000_000_000,
                physical_cpu_cores: 8,
                logical_cpu_cores: 8,
                devices: vec![
                    agl_model::HostCapabilityDevice {
                        identity: "CPU".to_owned(),
                        kind: agl_model::HostCapabilityDeviceKind::Cpu,
                        pci_device_id: None,
                        pci_subsystem_id: None,
                        physical_pool_bytes: 64_000_000_000,
                        usable: true,
                        supports_gpu_offload: false,
                    },
                    agl_model::HostCapabilityDevice {
                        identity: "test-rx7900xtx".to_owned(),
                        kind: agl_model::HostCapabilityDeviceKind::DiscreteGpu,
                        pci_device_id: Some("1002:744c".to_owned()),
                        pci_subsystem_id: Some("1da2:471e".to_owned()),
                        physical_pool_bytes: 24_000_000_000,
                        usable: true,
                        supports_gpu_offload: true,
                    },
                ],
            })
        }

        fn generate(&self, job: ChatInferenceJob) -> anyhow::Result<InferenceResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(InferenceResponse {
                attempt_id: job.request.attempt_id,
                content: "fake daemon answer".to_owned(),
                finish_reason: InferenceFinishReason::Stop,
                metadata: InferenceResponseMetadata {
                    model_state: Some("fake-daemon".to_owned()),
                    selected_device: None,
                    duration_ms: 1,
                    input_tokens: 4,
                    output_tokens: 4,
                    configured_batch_size: 0,
                    prefill_chunks: 0,
                    resource_admission: None,
                },
            })
        }

        fn clear_context(&self, _context: &agl_model::ModelContextKey) -> anyhow::Result<()> {
            Ok(())
        }

        fn release_context(&self, _context: &agl_model::ModelContextKey) -> anyhow::Result<()> {
            Ok(())
        }

        fn status(&self) -> anyhow::Result<ModelManagerStatus> {
            Ok(ModelManagerStatus::default())
        }

        fn device_inventory(&self) -> anyhow::Result<Vec<agl_inference::InferenceDeviceInfo>> {
            Ok(Vec::new())
        }
    }

    #[cfg(unix)]
    fn daemon_test_runtime(root: &std::path::Path) -> AgentLibreRuntimeConfig {
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        AgentLibreRuntimeConfig {
            paths: AgentLibrePaths::from_agl_home(root.join("home")),
            logging: AgentLibreLoggingConfig::default(),
            history: AgentLibreHistoryConfig::default(),
            workspace: AgentLibreWorkspaceConfig {
                root: Some(workspace),
            },
            inference: agl_runtime::AgentLibreInferenceConfig::default(),
            execution: agl_runtime::AgentLibreExecutionConfig::default(),
        }
    }

    #[cfg(unix)]
    fn install_daemon_test_model(runtime: &AgentLibreRuntimeConfig) {
        let model_root = runtime.paths.data_dir.join("test-models");
        std::fs::create_dir_all(&model_root).unwrap();
        let store = agl_model::ModelInstallStore::new(runtime.paths.model_install_root());
        std::fs::create_dir_all(store.root()).unwrap();
        for (id, role, filename, byte_size, sha256) in [
            (
                "gemma4-e4b",
                agl_model::ModelArtifactRole::Main,
                "gemma-4-E4B-it-qat-UD-Q4_K_XL.gguf",
                4_215_693_760,
                "b3052f962d6449b4eb2075733c068bdec1c51eadb7b237e6c3157bfbb7b1dae0",
            ),
            (
                "gemma4-e4b-mmproj",
                agl_model::ModelArtifactRole::Projector,
                "mmproj-F16.gguf",
                990_372_672,
                "6a255159ee4b01b304f633a57f017dd7d5a69d30fff52abb2614bf0813cef034",
            ),
        ] {
            let path = model_root.join(filename);
            std::fs::write(&path, b"GGUF daemon fixture").unwrap();
            let model_id = agl_config::ModelId::new(id).unwrap();
            let record = agl_model::ModelInstallRecord {
                version: 1,
                model_id: model_id.clone(),
                package_id: Some(agl_model::ModelPackageId::new("gemma4-e4b").unwrap()),
                role,
                source: agl_model::InstallSource::HuggingFace {
                    repository: "unsloth/gemma-4-E4B-it-qat-GGUF".to_owned(),
                    revision: "e4a9ed86f935b06e87808789db3c56bba24cbd49".to_owned(),
                    filename: filename.to_owned(),
                },
                path,
                byte_size,
                sha256: sha256.to_owned(),
                additional_files: Vec::new(),
                installed_at_unix_ms: 1,
                state: agl_model::InstallRecordState::Active,
            };
            std::fs::write(
                store.record_path(&model_id),
                serde_json::to_vec_pretty(&record).unwrap(),
            )
            .unwrap();
        }
    }

    #[cfg(unix)]
    fn spawn_test_daemon(
        runtime: AgentLibreRuntimeConfig,
        calls: Arc<AtomicUsize>,
    ) -> (
        std::thread::JoinHandle<anyhow::Result<()>>,
        std::sync::mpsc::Receiver<()>,
    ) {
        let socket_path = default_socket_path(&runtime.paths);
        std::fs::create_dir_all(socket_path.parent().unwrap()).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let server = std::thread::spawn(move || -> anyhow::Result<()> {
            let terminal_service = TestTerminalService::start(&runtime);
            let async_runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            async_runtime.block_on(async move {
                let listener = tokio::net::UnixListener::bind(&socket_path)?;
                ready_tx.send(()).unwrap();
                let (stream, _) = listener.accept().await?;
                let mut runtime_identity = agl_runtime::current_runtime_identity()?;
                runtime_identity.terminal_generation =
                    Some(terminal_service.identity.installed_generation().clone());
                let state = agl_daemon::SharedDaemonState::open_with_runtime_identity(
                    runtime,
                    InferenceOptions::default(),
                    InferenceClientHandle::new(DaemonTestInference { calls }),
                    EngineRuntimeStatusHandle::default(),
                    runtime_identity,
                )?;
                agl_daemon::serve_connection(stream, &state).await
            })
        });
        (server, ready_rx)
    }

    #[cfg(unix)]
    fn spawn_incompatible_daemon(
        runtime: &AgentLibreRuntimeConfig,
    ) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Receiver<()>) {
        use std::io::{BufRead as _, BufReader, Write as _};
        use std::os::unix::net::UnixListener;

        let socket_path = default_socket_path(&runtime.paths);
        std::fs::create_dir_all(socket_path.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: agl_protocol::DaemonRequest = serde_json::from_str(&line).unwrap();
            assert!(matches!(
                request.kind,
                agl_protocol::DaemonRequestKind::Hello(_)
            ));
            stream.write_all(b"not-json\n").unwrap();
            stream.flush().unwrap();
        });
        (server, ready_rx)
    }

    #[test]
    fn inference_authority_matrix_is_daemon_first_with_an_exact_standalone_allowlist() {
        let surfaces = [
            InferenceAuthoritySurface::DirectRun,
            InferenceAuthoritySurface::Cron,
            InferenceAuthoritySurface::FunctionSmoke,
        ];
        for surface in surfaces {
            assert_eq!(
                inference_authority_decision(surface, DaemonConnectionClass::Compatible),
                InferenceAuthorityDecision::Daemon
            );
            assert_eq!(
                inference_authority_decision(surface, DaemonConnectionClass::Incompatible),
                InferenceAuthorityDecision::Reject
            );
        }

        for surface in [
            InferenceAuthoritySurface::DirectRun,
            InferenceAuthoritySurface::Cron,
            InferenceAuthoritySurface::FunctionSmoke,
        ] {
            assert_eq!(
                inference_authority_decision(surface, DaemonConnectionClass::Unavailable),
                InferenceAuthorityDecision::Standalone
            );
        }
        let available: std::result::Result<(), ClientError> = Ok(());
        let unavailable: std::result::Result<(), ClientError> =
            Err(ClientError::DaemonUnavailable("absent".to_owned()));
        let incompatible: std::result::Result<(), ClientError> = Err(ClientError::SchemaMismatch {
            expected: "agentlibre.event.v8alpha",
        });
        assert_eq!(
            classify_daemon_connection(&available),
            DaemonConnectionClass::Compatible
        );
        assert_eq!(
            classify_daemon_connection(&unavailable),
            DaemonConnectionClass::Unavailable
        );
        assert_eq!(
            classify_daemon_connection(&incompatible),
            DaemonConnectionClass::Incompatible
        );
    }

    #[cfg(unix)]
    #[test]
    fn incompatible_daemon_blocks_direct_run_before_standalone_worker_creation() {
        let root = std::env::temp_dir().join(format!(
            "agl-cli-run-incompatible-daemon-{}",
            agl_ids::RequestId::generate()
        ));
        let runtime = daemon_test_runtime(&root);
        let (server, ready) = spawn_incompatible_daemon(&runtime);
        ready.recv_timeout(Duration::from_secs(5)).unwrap();

        let error = run_one_shot_raw(
            RunOptions {
                prompt: Some("must not run".to_owned()),
                ..RunOptions::default()
            },
            &runtime,
        )
        .expect_err("incompatible daemon must fail closed");

        assert!(format!("{error:#}").contains("refusing standalone inference"));
        assert!(!runtime.paths.inference_worker_temp_root().exists());
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cron_inference_never_falls_back_from_an_active_incomplete_daemon() {
        use std::io::{BufRead as _, BufReader, Write as _};
        use std::os::unix::net::UnixListener;

        let root = std::env::temp_dir().join(format!(
            "agl-cli-cron-daemon-first-{}",
            agl_ids::RequestId::generate()
        ));
        let paths = AgentLibrePaths::from_agl_home(root.join("home"));
        let socket_path = default_socket_path(&paths);
        std::fs::create_dir_all(socket_path.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket_path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: agl_protocol::DaemonRequest = serde_json::from_str(&line).unwrap();
            let client_runtime = match &request.kind {
                agl_protocol::DaemonRequestKind::Hello(hello) => {
                    hello.client_runtime.clone().unwrap()
                }
                other => panic!("expected Hello, got {other:?}"),
            };
            serde_json::to_writer(
                &mut stream,
                &agl_protocol::DaemonEvent::new(
                    Some(request.request_id),
                    agl_protocol::DaemonEventKind::Hello(agl_protocol::HelloEvent {
                        protocol_version: agl_protocol::PROTOCOL_VERSION.to_owned(),
                        product_version: "test".to_owned(),
                        daemon_instance_id: agl_ids::DaemonInstanceId::generate(),
                        daemon_runtime: client_runtime,
                        engine_protocol_id: format!("sha256:{}", "d".repeat(64)),
                        tools: Vec::new(),
                    }),
                ),
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
        });
        let runtime = AgentLibreRuntimeConfig {
            paths,
            logging: AgentLibreLoggingConfig::default(),
            history: AgentLibreHistoryConfig::default(),
            workspace: AgentLibreWorkspaceConfig::default(),
            inference: agl_runtime::AgentLibreInferenceConfig::default(),
            execution: agl_runtime::AgentLibreExecutionConfig::default(),
        };

        let error = match daemon_first_cron_inference(&runtime) {
            Ok(_) => panic!("incomplete daemon unexpectedly admitted cron inference"),
            Err(error) => error,
        };
        let rendered = format!("{error:#}");
        assert!(rendered.contains("lacks required cron tool SessionOpen"));
        assert!(rendered.contains("refusing standalone inference"));
        assert!(!runtime.paths.inference_worker_temp_root().exists());
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn direct_run_uses_a_compatible_daemon_for_inference() {
        let root = std::env::temp_dir().join(format!(
            "agl-cli-run-daemon-{}",
            agl_ids::RequestId::generate()
        ));
        let runtime = daemon_test_runtime(&root);
        install_daemon_test_model(&runtime);
        let workspace = runtime.workspace.root.clone().unwrap();
        let function_root = workspace.join(".agl/functions/daemon-test");
        std::fs::create_dir_all(&function_root).unwrap();
        std::fs::write(
            function_root.join("FUNCTION.md"),
            r#"---
package:
  schema: agentlibre.package/v1
  type: function
  id: daemon-test
  version: 1.0.0
  payload_schema: agentlibre.function/v3
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires:
    - model:gemma4-e4b@^1.0
title: Daemon test
model:
  profile: cpu-8gb-32768
runtime:
  tool_mode: read-only
  max_output_tokens: 64
  stop_rules: []
  structured_generation: lazy_tool
  repair_malformed_tool_calls: true
skills:
  use: []
subagents:
  use: []
---
"#,
        )
        .unwrap();
        std::fs::write(function_root.join("SYSTEM.md"), "Answer the test prompt.\n").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let function_ref = function_root
            .join("FUNCTION.md")
            .to_string_lossy()
            .into_owned();
        let (server, ready) = spawn_test_daemon(runtime.clone(), Arc::clone(&calls));
        ready.recv_timeout(Duration::from_secs(5)).unwrap();

        run_one_shot_raw(
            RunOptions {
                function_ref: Some(function_ref),
                workspace_root: Some(workspace),
                prompt: Some("hello from direct run".to_owned()),
                ..RunOptions::default()
            },
            &runtime,
        )
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!runtime.paths.inference_worker_temp_root().exists());
        server.join().unwrap().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cron_skill_uses_a_compatible_daemon_for_inference() {
        let root = std::env::temp_dir().join(format!(
            "agl-cli-cron-daemon-{}",
            agl_ids::RequestId::generate()
        ));
        let runtime = daemon_test_runtime(&root);
        install_daemon_test_model(&runtime);
        let store = AglStore::open_at(runtime.paths.store_root()).unwrap();
        let mut draft = CronJobDraft::new(
            "Daemon cron test",
            CronTargetKind::Skill,
            "process",
            "hourly",
        );
        draft.prompt = Some("Report repository status.".to_owned());
        let job = CronRepository::new(&store).add_job(draft).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let (server, ready) = spawn_test_daemon(runtime.clone(), Arc::clone(&calls));
        ready.recv_timeout(Duration::from_secs(5)).unwrap();

        let inference = daemon_first_cron_inference(&runtime).unwrap();
        let result = inference.run_skill(&job, &runtime).unwrap();

        assert!(result.starts_with("skill:process:session:"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!runtime.paths.inference_worker_temp_root().exists());
        drop(inference);
        server.join().unwrap().unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cli_runtime_profile_drives_config_loading_and_process_mode() {
        let config = CliCommand::Config(ConfigCommand::Paths);
        assert_eq!(cli_runtime_profile(&config), CliRuntimeProfile::LightBatch);
        assert_eq!(
            process_mode_for_command(&config),
            AgentLibreProcessMode::Batch
        );

        let serve = CliCommand::Serve(serve_options());
        assert_eq!(cli_runtime_profile(&serve), CliRuntimeProfile::FullBatch);
        assert_eq!(
            process_mode_for_command(&serve),
            AgentLibreProcessMode::Batch
        );
        let run = CliCommand::Run(RunOptions::default());
        assert_eq!(cli_runtime_profile(&run), CliRuntimeProfile::Interactive);
        assert_eq!(
            process_mode_for_command(&run),
            AgentLibreProcessMode::Interactive
        );
    }

    #[test]
    fn top_level_run_uses_workspace_default_function() {
        let root =
            std::env::temp_dir().join(format!("agl-cli-default-function-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".agl")).unwrap();
        std::fs::write(
            root.join(".agl/workspace.toml"),
            r#"version = 3
default_function = "function:coding@^1.0"
"#,
        )
        .unwrap();
        let runtime = AgentLibreRuntimeConfig {
            paths: AgentLibrePaths::from_agl_home(root.join("home")),
            logging: AgentLibreLoggingConfig::default(),
            history: AgentLibreHistoryConfig::default(),
            workspace: AgentLibreWorkspaceConfig::default(),
            inference: agl_runtime::AgentLibreInferenceConfig::default(),
            execution: agl_runtime::AgentLibreExecutionConfig::default(),
        };
        let mut options = RunOptions {
            workspace_root: Some(root.clone()),
            ..RunOptions::default()
        };

        apply_workspace_default_function_to_run(&mut options, &runtime).unwrap();

        assert_eq!(
            options.function_ref.as_deref(),
            Some("function:coding@^1.0")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn top_level_run_uses_builtin_default_without_workspace_manifest() {
        let root = std::env::temp_dir().join(format!(
            "agl-cli-default-function-no-manifest-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let runtime = AgentLibreRuntimeConfig {
            paths: AgentLibrePaths::from_agl_home(root.join("home")),
            logging: AgentLibreLoggingConfig::default(),
            history: AgentLibreHistoryConfig::default(),
            workspace: AgentLibreWorkspaceConfig::default(),
            inference: agl_runtime::AgentLibreInferenceConfig::default(),
            execution: agl_runtime::AgentLibreExecutionConfig::default(),
        };
        let mut options = RunOptions {
            workspace_root: Some(root.clone()),
            ..RunOptions::default()
        };

        apply_workspace_default_function_to_run(&mut options, &runtime).unwrap();

        assert_eq!(
            options.function_ref.as_deref(),
            Some(agl_repo::DEFAULT_FUNCTION)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn serve_inherits_function_runtime_defaults() {
        let root = std::env::temp_dir().join(format!(
            "agl-cli-serve-function-defaults-{}",
            std::process::id()
        ));
        let function_root = root.join(".agl/functions/coding");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&function_root).unwrap();
        std::fs::write(
            root.join(".agl/workspace.toml"),
            "version = 3\ndefault_function = \"function:coding@^1\"\n\n[[sources]]\nid = \"workspace\"\ntier = \"workspace\"\nkind = \"directory\"\npath = \".agl\"\n\n[policy]\n[config]\n",
        )
        .unwrap();
        std::fs::write(
            function_root.join("FUNCTION.md"),
            r#"---
package:
  schema: agentlibre.package/v1
  type: function
  id: coding
  version: 1.0.0
  payload_schema: agentlibre.function/v3
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires: []
title: Coding
runtime:
  tool_mode: write
  max_output_tokens: 17
memory:
  read:
    - user
---
"#,
        )
        .unwrap();
        std::fs::write(function_root.join("SYSTEM.md"), "Code.\n").unwrap();
        let runtime = AgentLibreRuntimeConfig {
            paths: AgentLibrePaths::from_agl_home(root.join("home")),
            logging: AgentLibreLoggingConfig::default(),
            history: AgentLibreHistoryConfig::default(),
            workspace: AgentLibreWorkspaceConfig::default(),
            inference: agl_runtime::AgentLibreInferenceConfig::default(),
            execution: agl_runtime::AgentLibreExecutionConfig::default(),
        };
        let options = ServeOptions {
            function_ref: Some("coding".to_string()),
            workspace_root: Some(root.clone()),
            ..serve_options()
        };

        let inference = inference_options_from_serve_options(&options, &runtime).unwrap();

        assert_eq!(inference.max_output_tokens, 17);
        assert_eq!(inference.tool_mode, ChatToolAccessMode::Write);
        assert!(inference.memory);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn one_shot_run_uses_chat_service_without_history() {
        let options = RunOptions {
            workspace_root: Some(PathBuf::from("/tmp/workspace")),
            session_id: Some(agl_ids::SessionId::generate()),
            no_history: false,
            new_session: false,
            prompt: Some("hello".to_string()),
            ..RunOptions::default()
        };

        let runtime = AgentLibreRuntimeConfig {
            paths: AgentLibrePaths::from_agl_home("/tmp/agl-home"),
            logging: AgentLibreLoggingConfig::default(),
            history: AgentLibreHistoryConfig::default(),
            workspace: AgentLibreWorkspaceConfig::default(),
            inference: agl_runtime::AgentLibreInferenceConfig::default(),
            execution: agl_runtime::AgentLibreExecutionConfig::default(),
        };

        let chat_options = one_shot_chat_options_from_run_options(&options, &runtime).unwrap();

        assert!(chat_options.no_history);
        assert!(chat_options.new_session);
        assert_eq!(chat_options.session_id, None);
        assert_eq!(
            chat_options.workspace_root,
            Some(PathBuf::from("/tmp/workspace"))
        );
        assert_eq!(
            chat_options.inference.workspace_root,
            Some(PathBuf::from("/tmp/workspace"))
        );
    }

    #[test]
    fn daemon_run_failure_exposes_canonical_admission_and_durable_ids() {
        let root = std::env::temp_dir().join(format!("agl-cli-run-failure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let runtime = AgentLibreRuntimeConfig {
            paths: AgentLibrePaths::from_agl_home(&root),
            logging: AgentLibreLoggingConfig::default(),
            history: AgentLibreHistoryConfig::default(),
            workspace: AgentLibreWorkspaceConfig::default(),
            inference: agl_runtime::AgentLibreInferenceConfig::default(),
            execution: agl_runtime::AgentLibreExecutionConfig::default(),
        };
        let run_id = RunId::generate();
        let session_id = SessionId::generate();
        let path = daemon_runtime_resolution_path(&runtime, &session_id, &run_id);
        assert_eq!(
            path,
            runtime
                .paths
                .sessions_root()
                .join(session_id.as_str())
                .join("runs")
                .join(run_id.as_str())
                .join("runtime-resolution.json")
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "attempt_id": "attempt_fixture",
                "artifacts": { "root": "function:gemma4-31b-64k@1.0.0" },
                "admission": {
                    "status": "rejected",
                    "error": {
                        "code": "accelerator_capacity_exceeded",
                        "details": {
                            "selected_profile_id": "gemma4-31b-64k-reviewed",
                            "required_bytes": 23488102400_u64
                        }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let error = daemon_run_failure(
            &runtime,
            &session_id,
            &run_id,
            Some("chat_turn_failed".to_owned()),
            "model request failed".to_owned(),
        );
        let failure = error.downcast_ref::<RunCommandFailure>().unwrap();
        assert_eq!(failure.diagnostic.code, "accelerator_capacity_exceeded");
        assert_eq!(
            failure.diagnostic.context.attempt_id.as_deref(),
            Some("attempt_fixture")
        );
        assert_eq!(failure.diagnostic.context.evidence_path, path);
        assert_eq!(
            failure
                .diagnostic
                .context
                .runtime_resolution
                .as_ref()
                .unwrap()["admission"]["error"]["details"]["selected_profile_id"],
            "gemma4-31b-64k-reviewed"
        );
        let rendered = failure.to_string();
        assert!(rendered.contains(run_id.as_str()));
        assert!(rendered.contains(session_id.as_str()));
        assert!(rendered.contains("attempt_fixture"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn daemon_run_failure_types_missing_canonical_evidence() {
        let root = std::env::temp_dir().join(format!(
            "agl-cli-run-missing-evidence-{}",
            std::process::id()
        ));
        let runtime = AgentLibreRuntimeConfig {
            paths: AgentLibrePaths::from_agl_home(&root),
            logging: AgentLibreLoggingConfig::default(),
            history: AgentLibreHistoryConfig::default(),
            workspace: AgentLibreWorkspaceConfig::default(),
            inference: agl_runtime::AgentLibreInferenceConfig::default(),
            execution: agl_runtime::AgentLibreExecutionConfig::default(),
        };
        let error = daemon_run_failure(
            &runtime,
            &SessionId::generate(),
            &RunId::generate(),
            Some("chat_turn_failed".to_owned()),
            "model request failed".to_owned(),
        );
        let failure = error.downcast_ref::<RunCommandFailure>().unwrap();
        assert_eq!(failure.diagnostic.code, "runtime_resolution_unavailable");
        assert!(failure.diagnostic.context.evidence_error.is_some());
    }

    #[test]
    fn config_command_runtime_does_not_parse_existing_config() {
        let root = std::env::temp_dir().join(format!(
            "agl-cli-invalid-runtime-config-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let paths = AgentLibrePaths::from_agl_home(&root);
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.runtime_config_path(), "not toml").unwrap();

        let runtime = runtime_for_command_paths(
            &CliCommand::Config(ConfigCommand::Init { force: true }),
            paths,
        )
        .unwrap();

        assert_eq!(runtime.logging, AgentLibreLoggingConfig::from_env());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cli_manifest_does_not_depend_on_matrix_sdk() {
        let manifest = include_str!("../Cargo.toml");

        assert!(
            !manifest.lines().any(|line| {
                let line = line.trim_start();
                line.starts_with("matrix-sdk.") || line.starts_with("matrix-sdk =")
            }),
            "agl-cli must not depend on matrix-sdk; Matrix SDK stays in agl-matrix-bridge"
        );
    }
}
