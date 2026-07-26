use std::env;
use std::path::PathBuf;
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
use agl_inference::{
    InferenceDeviceInfo, InferenceDeviceKind, ModelManager, ModelManagerOptions, WorkerModelRuntime,
};
use agl_protocol::{
    AssistantItemState, DaemonCapability, InferenceStatusRequest, ModelReleaseOutcome,
    ModelReleaseReason, ProtocolInferenceDeviceKind, ProtocolInferenceWorkerState,
    ProtocolRunState, ProtocolToolMode, RunBudgetRequest, RunSubmitRequest, RunSubscribeRequest,
    SessionFinishReason, SessionFinishRequest, SessionOpenRequest, SessionPresentationItem,
    SessionPresentationRequest,
};
use agl_repo::{
    ComponentStatus, RepoComponentInitOptions as AglRepoComponentInitOptions, init_repo_component,
    read_workspace_default_function,
};
use agl_runtime::{
    AgentLibreHistoryConfig, AgentLibreLoggingConfig, AgentLibrePaths, AgentLibreProcessMode,
    AgentLibreRuntimeConfig, AgentLibreWorkspaceConfig, init_tracing,
};
use agl_skill::{
    SkillFolderCreateSituation, SkillFolderSyncActionKind,
    SkillFolderSyncOptions as AglSkillFolderSyncOptions, SkillFolderSyncReport, SkillPermissions,
    SkillTrustOptions as AglSkillTrustOptions, SkillTrustUpdateReport, WorkspaceSkillDiagnostic,
    WorkspaceSkillDiagnosticScope, WorkspaceSkillDiagnosticSeverity, WorkspaceSkillReport,
    WorkspaceSkillStatus, builtin_registry, revoke_workspace_skill, sync_workspace_skill_folders,
    trust_workspace_skill, workspace_skill_report_with_trust,
};
use agl_store::{AglStore, IdempotencyOutcome, MatrixNotificationOutboxDraft};
use anyhow::{Context, Result, anyhow, bail};

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
#[path = "process.rs"]
mod process_command;
mod repo;
mod store;
mod tui;

use args::{
    CliCommand, CronAddOptions, CronCommand, CronDeleteOptions, CronDisableOptions,
    CronEnableOptions, CronHistoryOptions, CronListOptions, CronRunOptions, CronShowOptions,
    CronTargetArg, CronTargetKindArg, CronTickOptions, DaemonStatusOptions, InferenceCommand,
    ProcessCommand, RunOptions, ServeOptions, SkillCommand, SkillFolderSyncOptions,
    SkillFolderSyncSituationArg, SkillInitOptions, SkillInspectOptions, SkillListOptions,
    SkillListSourceArg, SkillRevokeOptions, SkillStatusOptions, SkillTrustOptions,
    SkillVerifyOptions, parse_cli, print_completion, print_usage,
};
use artifact::run_artifact;
use config::run_config;
use function::run_function;
use init::run_init;
use memory::run_memory;
use model::run_model;
use notes::run_notes;
use one_shot::OneShotSession;
use process_command::run_process;
use repo::run_repo;
use store::run_store;

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
    if env::var_os("AGL_INTERNAL_VERIFY_RUNTIME_BUNDLE").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        if let Err(err) = process_command::verify_runtime_bundle_identity() {
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
        CliCommand::Help { bin_name } => {
            if let Err(err) = print_usage(bin_name) {
                eprintln!("error: {err:#}");
                process::exit(1);
            }
            return;
        }
        CliCommand::HelpPrinted => return,
        CliCommand::Completion { shell } => {
            print_completion(*shell);
            return;
        }
        _ => {}
    }

    let runtime = match runtime_for_command(&command, invocation.home) {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("error: failed to resolve agentLIBRE runtime: {err:#}");
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
        eprintln!("error: {err:#}");
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
    InitInventory,
    FunctionSmoke,
    Interactive,
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
            | InferenceAuthoritySurface::InitInventory
            | InferenceAuthoritySurface::FunctionSmoke => InferenceAuthorityDecision::Standalone,
            InferenceAuthoritySurface::Interactive => InferenceAuthorityDecision::Reject,
        },
        DaemonConnectionClass::Incompatible => InferenceAuthorityDecision::Reject,
    }
}

fn cli_runtime_profile(command: &CliCommand) -> CliRuntimeProfile {
    match command {
        CliCommand::Interactive(_)
        | CliCommand::Run(_)
        | CliCommand::Process(ProcessCommand::Attach(_))
        | CliCommand::Inference(InferenceCommand::Run(_)) => CliRuntimeProfile::Interactive,
        CliCommand::Config(_)
        | CliCommand::Artifact(_)
        | CliCommand::Cron(_)
        | CliCommand::Function(_)
        | CliCommand::Init(_)
        | CliCommand::Model(_)
        | CliCommand::Store(_)
        | CliCommand::Repo(_)
        | CliCommand::Skill(_)
        | CliCommand::Memory(_)
        | CliCommand::Notes(_)
        | CliCommand::Process(_)
        | CliCommand::DaemonStatus(_) => CliRuntimeProfile::LightBatch,
        CliCommand::Serve(_)
        | CliCommand::Inference(InferenceCommand::Serve(_))
        | CliCommand::Help { .. }
        | CliCommand::HelpPrinted
        | CliCommand::Completion { .. } => CliRuntimeProfile::FullBatch,
    }
}

fn run(command: CliCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    match command {
        CliCommand::Interactive(options) => tui::run_interactive(options, runtime),
        CliCommand::Help { bin_name } => print_usage(bin_name),
        CliCommand::HelpPrinted => Ok(()),
        CliCommand::Completion { shell } => {
            print_completion(shell);
            Ok(())
        }
        CliCommand::Config(command) => run_config(command, runtime),
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
        CliCommand::Process(command) => run_process(command, runtime),
        CliCommand::Serve(options) => run_serve(options, runtime),
        CliCommand::Inference(command) => run_inference(command, runtime),
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
    let inference_config_present = runtime.paths.default_local_inference_config().exists();
    let report = serde_json::json!({
        "ok": true,
        "target_kind": job.target_kind.as_str(),
        "target_ref": job.target_ref,
        "prompt_ready": job.target_kind != CronTargetKind::Skill || prompt.is_some(),
        "prompt_preview": prompt.as_deref().map(prompt_preview),
        "inference_config_present": inference_config_present,
        "records_run": false,
    });
    if json {
        crate::print_json(&serde_json::json!({
            "job": job,
            "preflight": report,
        }))?;
    } else {
        println!("cron.preflight.ok=true");
        println!(
            "cron.preflight.target={}:{}",
            job.target_kind.as_str(),
            job.target_ref
        );
        println!("cron.preflight.records_run=false");
        println!("cron.preflight.inference_config_present={inference_config_present}");
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
        println!("cron.tick.at={unix_seconds}");
        println!("cron.tick.due_jobs={}", report.due_jobs);
        println!("cron.tick.recorded_runs={}", report.recorded_runs.len());
        println!("cron.tick.notifications={}", report.notifications);
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
        println!("cron.deleted=true");
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
    let workspace = workspace_skill_report_with_trust(
        std::env::current_dir().context("failed to resolve current directory")?,
        skill_trust_store_path(runtime),
    )?;
    let matches = workspace
        .skills
        .iter()
        .filter(|skill| skill.name.as_deref() == Some(name))
        .collect::<Vec<_>>();
    if matches.iter().any(|skill| skill.usable) {
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
        SkillCommand::Init(options) => run_skill_init(options),
        SkillCommand::List(options) => run_skill_list(options, runtime),
        SkillCommand::Inspect(options) => run_skill_inspect(options, runtime),
        SkillCommand::Status(options) => run_skill_status(options, runtime),
        SkillCommand::Verify(options) => run_skill_verify(options, runtime),
        SkillCommand::SyncFolders(options) => run_skill_sync_folders(options),
        SkillCommand::Trust(options) => run_skill_trust(options, runtime),
        SkillCommand::Revoke(options) => run_skill_revoke(options, runtime),
    }
}

fn run_skill_init(options: SkillInitOptions) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "skill init", "starting command");
    let report = init_repo_component(
        std::env::current_dir().context("failed to resolve current directory")?,
        &AglRepoComponentInitOptions {
            component: "skills".to_string(),
            dry_run: options.dry_run,
        },
    )?;
    crate::print_json_or(options.json, &report, || {
        repo::print_repo_component_init_report(&report)
    })?;
    if report.has_errors() {
        bail!("workspace skills initialization failed");
    }
    Ok(())
}

fn run_skill_list(options: SkillListOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "skill list", "starting command");
    const DEFAULT_SKILL_LIST_LIMIT: usize = 100;
    const MAX_SKILL_LIST_LIMIT: usize = 100;

    let registry = builtin_registry()?;
    let workspace = workspace_skill_report_with_trust(
        std::env::current_dir().context("failed to resolve current directory")?,
        skill_trust_store_path(runtime),
    )?;
    let limit = options
        .limit
        .unwrap_or(DEFAULT_SKILL_LIST_LIMIT)
        .min(MAX_SKILL_LIST_LIMIT);
    let workspace_overrides = workspace
        .skills
        .iter()
        .filter_map(|skill| {
            skill
                .overrides_builtin
                .then(|| skill.name.clone())
                .flatten()
        })
        .collect::<std::collections::BTreeSet<_>>();
    let include_builtin = matches!(
        options.source,
        SkillListSourceArg::All | SkillListSourceArg::Core
    );
    let include_workspace = true;
    let mut emitted = 0usize;

    if options.json {
        let mut builtins = Vec::new();
        if include_builtin {
            for skill in registry.skills() {
                if emitted >= limit {
                    break;
                }
                if options.trusted_only && !skill.permits_context_injection() {
                    continue;
                }
                emitted += 1;
                builtins.push(serde_json::json!({
                    "name": skill.harness.name,
                    "source": skill.harness.source.as_str(),
                    "pack": skill.harness.pack,
                    "description": skill.harness.description,
                    "trust": format!("{:?}", skill.trust),
                    "usable": skill.permits_context_injection(),
                    "overridden_by_workspace": workspace_overrides.contains(&skill.harness.name),
                    "permissions": skill.harness.permissions,
                }));
            }
        }
        let mut workspace_skills = Vec::new();
        if include_workspace {
            for skill in &workspace.skills {
                if emitted >= limit {
                    break;
                }
                if !skill_list_matches_workspace_source(options.source, skill) {
                    continue;
                }
                if options.trusted_only && !skill.usable {
                    continue;
                }
                emitted += 1;
                workspace_skills.push(skill);
            }
        }
        crate::print_json(&serde_json::json!({
            "source": skill_list_source_as_str(options.source),
            "trusted_only": options.trusted_only,
            "limit": limit,
            "builtins": builtins,
            "workspace": {
                "state": workspace.state,
                "workspace_root": workspace.workspace_root,
                "component": workspace.component,
                "lock_path": workspace.lock_path,
                "skills": workspace_skills,
                "warnings": if include_workspace { workspace.warnings } else { Vec::new() },
                "errors": if include_workspace { workspace.errors } else { Vec::new() },
                "next_steps": if include_workspace { workspace.next_steps } else { Vec::new() },
            },
        }))?;
    } else {
        if include_builtin {
            for skill in registry.skills() {
                if emitted >= limit {
                    break;
                }
                if options.trusted_only && !skill.permits_context_injection() {
                    continue;
                }
                emitted += 1;
                println!(
                    "skill name={} source={} pack={} trust={:?} usable={} overridden_by_workspace={}",
                    skill.harness.name,
                    skill.harness.source.as_str(),
                    skill.harness.pack,
                    skill.trust,
                    skill.permits_context_injection(),
                    workspace_overrides.contains(&skill.harness.name)
                );
                print_skill_permissions(
                    &format!("skill.{}", skill.harness.name),
                    &skill.harness.permissions,
                );
            }
        }
        if include_workspace {
            for skill in &workspace.skills {
                if emitted >= limit {
                    break;
                }
                if !skill_list_matches_workspace_source(options.source, skill) {
                    continue;
                }
                if options.trusted_only && !skill.usable {
                    continue;
                }
                emitted += 1;
                print_workspace_skill_status(skill);
            }
            for warning in &workspace.warnings {
                println!("warning={warning}");
            }
            for error in &workspace.errors {
                println!("error={error}");
            }
        }
    }

    Ok(())
}

fn skill_list_source_as_str(source: SkillListSourceArg) -> &'static str {
    match source {
        SkillListSourceArg::All => "all",
        SkillListSourceArg::Core => "core",
        SkillListSourceArg::Community => "community",
        SkillListSourceArg::Local => "local",
    }
}

fn skill_list_matches_workspace_source(
    source: SkillListSourceArg,
    skill: &WorkspaceSkillStatus,
) -> bool {
    match source {
        SkillListSourceArg::All => true,
        SkillListSourceArg::Core => skill.source.as_deref() == Some("core"),
        SkillListSourceArg::Community => skill.source.as_deref() == Some("community"),
        SkillListSourceArg::Local => skill.source.as_deref() == Some("local"),
    }
}

fn run_skill_inspect(
    options: SkillInspectOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "skill inspect", "starting command");
    let registry = builtin_registry()?;
    let workspace = workspace_skill_report_with_trust(
        std::env::current_dir().context("failed to resolve current directory")?,
        skill_trust_store_path(runtime),
    )?;

    let builtins = registry
        .skills()
        .iter()
        .filter(|skill| skill.harness.name == options.name)
        .collect::<Vec<_>>();
    let workspace_skills = workspace
        .skills
        .iter()
        .filter(|skill| skill.name.as_deref() == Some(options.name.as_str()))
        .collect::<Vec<_>>();

    if builtins.is_empty() && workspace_skills.is_empty() {
        bail!("skill not found: {}", options.name);
    }
    let runtime_usable = builtins
        .iter()
        .any(|skill| skill.permits_context_injection())
        || workspace_skills.iter().any(|skill| skill.usable);
    let workspace_overrides = workspace_skills
        .iter()
        .filter(|skill| skill.overrides_builtin)
        .filter_map(|skill| skill.name.clone())
        .collect::<std::collections::BTreeSet<_>>();

    if options.json {
        let builtins = builtins
            .into_iter()
            .map(|skill| {
                serde_json::json!({
                    "name": skill.harness.name,
                    "source": skill.harness.source.as_str(),
                    "pack": skill.harness.pack,
                    "description": skill.harness.description,
                    "version": skill.harness.version,
                    "trust": format!("{:?}", skill.trust),
                    "usable": skill.permits_context_injection(),
                    "overridden_by_workspace": workspace_overrides.contains(&skill.harness.name),
                    "permissions": skill.harness.permissions,
                })
            })
            .collect::<Vec<_>>();
        crate::print_json(&serde_json::json!({
            "name": options.name,
            "builtins": builtins,
            "workspace": workspace_skills,
        }))?;
    } else {
        for skill in builtins {
            println!(
                "skill name={} source={} pack={} version={} trust={:?} usable={} overridden_by_workspace={}",
                skill.harness.name,
                skill.harness.source.as_str(),
                skill.harness.pack,
                skill.harness.version,
                skill.trust,
                skill.permits_context_injection(),
                workspace_overrides.contains(&skill.harness.name)
            );
            println!("description={}", skill.harness.description);
            print_skill_permissions(
                &format!("skill.{}", skill.harness.name),
                &skill.harness.permissions,
            );
        }
        for skill in workspace_skills {
            print_workspace_skill_status(skill);
        }
    }

    if options.runtime && !runtime_usable {
        bail!("skill is not runtime usable: {}", options.name);
    }

    Ok(())
}

fn run_skill_status(options: SkillStatusOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "skill status", "starting command");
    let report = workspace_skill_report_with_trust(
        std::env::current_dir().context("failed to resolve current directory")?,
        skill_trust_store_path(runtime),
    )?;

    crate::print_json_or(options.json, &report, || {
        print_workspace_skill_report(&report)
    })?;

    if report.should_fail(options.strict) {
        bail!("workspace skill status is not healthy");
    }
    Ok(())
}

fn run_skill_verify(options: SkillVerifyOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "skill verify", "starting command");
    let report = workspace_skill_report_with_trust(
        std::env::current_dir().context("failed to resolve current directory")?,
        skill_trust_store_path(runtime),
    )?;

    crate::print_json_or(options.json, &report, || {
        print_workspace_skill_report(&report)
    })?;

    if report.should_fail(true) {
        bail!("workspace skill verification failed");
    }
    Ok(())
}

fn run_skill_sync_folders(options: SkillFolderSyncOptions) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "skill sync-folders", "starting command");
    let report = sync_workspace_skill_folders(
        std::env::current_dir().context("failed to resolve current directory")?,
        &AglSkillFolderSyncOptions {
            dry_run: options.dry_run,
            situation: skill_folder_sync_situation(options.when),
        },
    )?;

    crate::print_json_or(options.json, &report, || {
        print_skill_folder_sync_report(&report)
    })?;

    if report.has_errors() {
        bail!("workspace skill folder sync failed");
    }
    Ok(())
}

fn run_skill_trust(options: SkillTrustOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "skill trust", "starting command");
    let report = trust_workspace_skill(
        std::env::current_dir().context("failed to resolve current directory")?,
        skill_trust_store_path(runtime),
        &options.name,
        &AglSkillTrustOptions {
            approve: options.yes,
            agentlibre_version: env!("CARGO_PKG_VERSION").to_string(),
        },
    )?;

    crate::print_json_or(options.json, &report, || {
        print_skill_trust_update_report(&report)
    })?;

    if report.has_errors() {
        bail!("workspace skill trust failed");
    }
    Ok(())
}

fn run_skill_revoke(options: SkillRevokeOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "skill revoke", "starting command");
    let report = revoke_workspace_skill(
        std::env::current_dir().context("failed to resolve current directory")?,
        skill_trust_store_path(runtime),
        &options.name,
    )?;

    crate::print_json_or(options.json, &report, || {
        print_skill_trust_update_report(&report)
    })?;

    if report.has_errors() {
        bail!("workspace skill revoke failed");
    }
    Ok(())
}

fn inference_options_from_serve_options(
    options: &ServeOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<InferenceOptions> {
    let run_options = RunOptions {
        config: options.config.clone(),
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
        config: options.config.clone(),
        function_ref: options.function_ref.clone(),
        artifact_root: options.artifact_root.clone(),
        workspace_root: options.workspace_root.clone(),
        max_output_tokens,
        tool_mode: chat_tool_mode(tool_mode),
        skills: options.skills.clone(),
        memory,
        model_bindings_path: None,
        model_bindings_override: None,
        runtime_plan_override: None,
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

fn skill_trust_store_path(runtime: &AgentLibreRuntimeConfig) -> PathBuf {
    runtime.paths.state_dir.join("skill-trust.toml")
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
        config: options.config.clone(),
        function_ref: options.function_ref.clone(),
        artifact_root: options.artifact_root.clone(),
        workspace_root: options.workspace_root.clone(),
        max_output_tokens,
        tool_mode: chat_tool_mode(tool_mode),
        skills: options.skills.clone(),
        memory,
        model_bindings_path: None,
        model_bindings_override: None,
        runtime_plan_override: None,
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
            let require_profile = options.config.is_none()
                && std::env::var_os("AGL_LOCAL_INFERENCE_CONFIG").is_none();
            if require_profile {
                agl_function::resolve_runtime_function(
                    reference,
                    &workspace_root,
                    &runtime.paths.config_dir,
                )
            } else {
                agl_function::resolve_runtime_function_allow_missing_profile(
                    reference,
                    &workspace_root,
                    &runtime.paths.config_dir,
                )
            }
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

fn run_inference(command: InferenceCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    match command {
        InferenceCommand::Run(options) => run_one_shot_raw(options, runtime),
        InferenceCommand::Serve(options) => run_serve_raw(options, runtime),
    }
}

fn process_local_model_manager(runtime: &AgentLibreRuntimeConfig) -> Result<ModelManager> {
    let inference_runtime =
        WorkerModelRuntime::discover(runtime.paths.inference_worker_temp_root())
            .context("failed to prepare isolated process-local inference worker")?;
    ModelManager::spawn(
        ModelManagerOptions::default()
            .with_residency_durations(
                Duration::from_secs(runtime.inference.residency.context_idle_seconds),
                Duration::from_secs(runtime.inference.residency.model_idle_seconds),
            )
            .with_model_lease_root(runtime.paths.model_lease_root()),
        inference_runtime,
    )
    .context("failed to start process-local model manager")
}

enum CliCronInference {
    Daemon {
        runtime: tokio::runtime::Runtime,
        client: AgentLibreClient,
    },
    Standalone {
        _manager: ModelManager,
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
    let connection = async_runtime.block_on(AgentLibreClient::connect(&socket_path));
    let authority = inference_authority_decision(
        InferenceAuthoritySurface::Cron,
        classify_daemon_connection(&connection),
    );
    match (authority, connection) {
        (InferenceAuthorityDecision::Daemon, Ok(client)) => {
            let required = [
                DaemonCapability::SessionOpen,
                DaemonCapability::RunSubmit,
                DaemonCapability::RunSubscribe,
                DaemonCapability::SessionPresentation,
                DaemonCapability::SessionFinish,
            ];
            let hello = client.hello().context("failed to read daemon identity")?;
            if let Some(missing) = required
                .into_iter()
                .find(|capability| !hello.capabilities.contains(capability))
            {
                bail!(
                    "daemon at {} lacks required cron capability {missing:?}; refusing standalone inference while it is active",
                    socket_path.display()
                );
            }
            Ok(CliCronInference::Daemon {
                runtime: async_runtime,
                client,
            })
        }
        (InferenceAuthorityDecision::Standalone, Err(ClientError::DaemonUnavailable(_))) => {
            let manager = process_local_model_manager(runtime).context(
                "no daemon is running and the isolated standalone cron inference authority failed",
            )?;
            let client = InferenceClientHandle::from(manager.handle());
            Ok(CliCronInference::Standalone {
                _manager: manager,
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

pub(crate) fn daemon_first_inference_inventory(
    runtime: &AgentLibreRuntimeConfig,
) -> Result<Vec<InferenceDeviceInfo>> {
    let socket_path = default_socket_path(&runtime.paths);
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build daemon inference inventory runtime")?;
    let connection = async_runtime.block_on(AgentLibreClient::connect(&socket_path));
    let authority = inference_authority_decision(
        InferenceAuthoritySurface::InitInventory,
        classify_daemon_connection(&connection),
    );
    match (authority, connection) {
        (InferenceAuthorityDecision::Daemon, Ok(client)) => {
            let hello = client.hello().context("failed to read daemon identity")?;
            if !hello
                .capabilities
                .contains(&DaemonCapability::InferenceInventory)
            {
                bail!(
                    "daemon at {} does not provide the current inference inventory capability; refusing process-local inference while a daemon is active",
                    socket_path.display()
                );
            }
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
            let manager = process_local_model_manager(runtime).context(
                "no daemon is running and the isolated standalone inference authority failed",
            )?;
            let inventory = manager
                .handle()
                .device_inventory()
                .context("standalone inference inventory failed")?;
            drop(manager);
            Ok(inventory)
        }
        (InferenceAuthorityDecision::Reject, Err(error)) => bail!(
            "daemon at {} is active but incompatible or unhealthy ({error}); refusing standalone inference",
            socket_path.display()
        ),
        _ => unreachable!("daemon connection classification and authority decision diverged"),
    }
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
    let connection = async_runtime.block_on(AgentLibreClient::connect(&socket_path));
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
    if options.function_ref.is_none()
        || options.config.is_some()
        || options.artifact_root.is_some()
        || options.max_output_tokens.is_some()
        || options.memory
    {
        bail!(
            "the active daemon at {} cannot represent this direct-run override; stop it explicitly before using standalone --config/--artifact-root/--max-output-tokens/--memory or raw `agl inference run`",
            socket_path.display()
        );
    }
    let required = [
        DaemonCapability::SessionOpen,
        DaemonCapability::RunSubmit,
        DaemonCapability::RunSubscribe,
        DaemonCapability::SessionPresentation,
        DaemonCapability::SessionFinish,
    ];
    let hello = client.hello().context("failed to read daemon identity")?;
    if let Some(missing) = required
        .into_iter()
        .find(|capability| !hello.capabilities.contains(capability))
    {
        bail!(
            "daemon at {} lacks required capability {missing:?}; refusing standalone inference while it is active",
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
        let mut subscription = client
            .subscribe_run(RunSubscribeRequest {
                run_id: accepted.run_id.clone(),
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
                .or(finished.error_code)
                .unwrap_or_else(|| format!("{:?}", finished.state));
            bail!("turn failed: {detail}");
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
        match item {
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
        }
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
        (Ok(answer), Ok(_)) => {
            println!("{answer}");
            Ok(())
        }
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(turn), Err(finish)) => Err(anyhow!("{turn:#}; additionally {finish:#}")),
    }
}

fn run_one_shot_standalone(options: RunOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let prompt = options
        .prompt
        .clone()
        .context("run requires PROMPT or --prompt TEXT")?;
    let chat_options = one_shot_chat_options_from_run_options(&options, runtime)?;
    let tool_mode = chat_options.inference.tool_mode;
    let model_manager = process_local_model_manager(runtime)?;
    let inference_client = InferenceClientHandle::from(model_manager.handle());
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
    match output.status {
        ChatTurnStatus::Answered { answer } => println!("{answer}"),
        ChatTurnStatus::Incomplete { partial, reason } => {
            println!("{partial}");
            bail!("turn output is incomplete: {}", reason.as_str());
        }
        ChatTurnStatus::Stopped { reason } => {
            println!("stopped=true reason={}", reason.as_str());
        }
        ChatTurnStatus::Failed { message } => {
            if let Some(cpu) =
                cpu_fallback_for_failed_turn(summary.automatic_runtime_plan.as_ref(), &message)
            {
                bail!(
                    "GPU inference failed: {message}\nA benchmarked CPU fallback is available (profile {}, context {}, expected speed {}), but non-interactive `agl run` never switches devices automatically. Select the CPU runtime explicitly and retry.",
                    cpu.profile_id,
                    cpu.runtime.context_tokens,
                    cpu.expected_speed
                );
            }
            bail!("turn failed: {message}")
        }
        ChatTurnStatus::Cancelled => bail!("turn cancelled"),
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
    match async_runtime.block_on(AgentLibreClient::connect(&socket_path)) {
        Ok(client) => {
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
                    println!("worker_build_id={}", status.worker_build_id);
                    println!(
                        "worker_state={}",
                        inference_worker_state_label(status.worker_state)
                    );
                    println!("worker_pid={}", optional_status_value(status.worker_pid));
                    println!(
                        "worker_launch_generation={}",
                        optional_status_value(status.launch_generation)
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

fn inference_worker_state_label(state: ProtocolInferenceWorkerState) -> &'static str {
    match state {
        ProtocolInferenceWorkerState::Cold => "cold",
        ProtocolInferenceWorkerState::Starting => "starting",
        ProtocolInferenceWorkerState::Ready => "ready",
        ProtocolInferenceWorkerState::Busy => "busy",
        ProtocolInferenceWorkerState::CoolingDown => "cooling_down",
    }
}

fn optional_status_value<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| value.to_string())
}

fn print_workspace_skill_report(report: &WorkspaceSkillReport) {
    println!("state={}", skill_report_state(report.state));
    println!("workspace_root={}", report.workspace_root.display());
    println!("lock_path={}", report.lock_path.display());
    if let Some(component) = &report.component {
        print_component_status(component);
    }
    for skill in &report.skills {
        print_workspace_skill_status(skill);
    }
    for diagnostic in &report.diagnostics {
        print_workspace_skill_diagnostic(diagnostic);
    }
    for warning in &report.warnings {
        println!("warning={warning}");
    }
    for error in &report.errors {
        println!("error={error}");
    }
    for next_step in &report.next_steps {
        println!("next_step={next_step}");
    }
}

fn print_skill_folder_sync_report(report: &SkillFolderSyncReport) {
    println!(
        "state={}",
        if report.errors.is_empty() {
            "ok"
        } else {
            "error"
        }
    );
    println!("workspace_root={}", report.workspace_root.display());
    println!("dry_run={}", report.dry_run);
    println!(
        "situation={}",
        skill_folder_create_situation(report.situation)
    );
    for action in &report.actions {
        println!(
            "skill.folder_action skill={} folder={} path={} action={}",
            action.skill,
            action.folder_id,
            action.path.display(),
            skill_folder_sync_action(action.action)
        );
    }
    for warning in &report.warnings {
        println!("warning={warning}");
    }
    for error in &report.errors {
        println!("error={error}");
    }
}

fn skill_folder_sync_action(action: SkillFolderSyncActionKind) -> &'static str {
    match action {
        SkillFolderSyncActionKind::Exists => "exists",
        SkillFolderSyncActionKind::SkippedReadOnly => "skipped_read_only",
        SkillFolderSyncActionKind::SkippedSource => "skipped_source",
        SkillFolderSyncActionKind::SkippedNoCreateRule => "skipped_no_create_rule",
        SkillFolderSyncActionKind::SkippedSituationMismatch => "skipped_situation_mismatch",
        SkillFolderSyncActionKind::WouldCreateDir => "would_create_dir",
        SkillFolderSyncActionKind::CreatedDir => "created_dir",
    }
}

fn skill_folder_sync_situation(when: SkillFolderSyncSituationArg) -> SkillFolderCreateSituation {
    match when {
        SkillFolderSyncSituationArg::SkillSync => SkillFolderCreateSituation::SkillSync,
        SkillFolderSyncSituationArg::RuntimePrepare => SkillFolderCreateSituation::RuntimePrepare,
        SkillFolderSyncSituationArg::ArtifactWrite => SkillFolderCreateSituation::ArtifactWrite,
    }
}

fn skill_folder_create_situation(when: SkillFolderCreateSituation) -> &'static str {
    match when {
        SkillFolderCreateSituation::SkillSync => "skill_sync",
        SkillFolderCreateSituation::RuntimePrepare => "runtime_prepare",
        SkillFolderCreateSituation::ArtifactWrite => "artifact_write",
    }
}

fn skill_report_state(state: agl_skill::SkillReportState) -> &'static str {
    match state {
        agl_skill::SkillReportState::Ok => "ok",
        agl_skill::SkillReportState::Warning => "warning",
        agl_skill::SkillReportState::Invalid => "invalid",
    }
}

fn print_component_status(component: &ComponentStatus) {
    println!(
        "component name={} path={} kind={:?} state={:?} exists={}",
        component.name,
        component.path.display(),
        component.kind,
        component.state,
        component.exists
    );
    if let Some(expected_url) = &component.expected_url {
        println!("component.{}.expected_url={expected_url}", component.name);
    }
    if let Some(actual_url) = &component.actual_url {
        println!("component.{}.actual_url={actual_url}", component.name);
    }
    if let Some(expected_rev) = &component.expected_rev {
        println!("component.{}.expected_rev={expected_rev}", component.name);
    }
    if let Some(expected_commit) = &component.expected_commit {
        println!(
            "component.{}.expected_commit={expected_commit}",
            component.name
        );
    }
    if let Some(actual_commit) = &component.actual_commit {
        println!("component.{}.actual_commit={actual_commit}", component.name);
    }
    if let Some(expected_tree) = &component.expected_tree {
        println!("component.{}.expected_tree={expected_tree}", component.name);
    }
    if let Some(actual_tree) = &component.actual_tree {
        println!("component.{}.actual_tree={actual_tree}", component.name);
    }
    if let Some(registered) = component.submodule_registered {
        println!(
            "component.{}.submodule_registered={registered}",
            component.name
        );
    }
    if let Some(gitlink) = component.gitlink_present {
        println!("component.{}.gitlink_present={gitlink}", component.name);
    }
    if let Some(top) = &component.nested_git_top {
        println!(
            "component.{}.nested_git_top={}",
            component.name,
            top.display()
        );
    }
    if let Some(dirty) = component.tracked_dirty {
        println!("component.{}.tracked_dirty={dirty}", component.name);
    }
    if let Some(untracked) = component.untracked_suspicious {
        println!(
            "component.{}.untracked_suspicious={untracked}",
            component.name
        );
    }
    for warning in &component.warnings {
        println!("component.{}.warning={warning}", component.name);
    }
    for error in &component.errors {
        println!("component.{}.error={error}", component.name);
    }
}

fn print_workspace_skill_status(skill: &WorkspaceSkillStatus) {
    let name = workspace_skill_key(skill);
    println!(
        "skill name={} path={} valid={} usable={} shadowed_by_builtin={} overrides_builtin={} trust_state={:?}",
        name,
        skill.path.display(),
        skill.valid,
        skill.usable,
        skill.shadowed_by_builtin,
        skill.overrides_builtin,
        skill.trust_state
    );
    if let Some(source_path) = &skill.source_path {
        println!("skill.{name}.source_path={source_path}");
    }
    if let Some(source) = &skill.source {
        println!("skill.{name}.source={source}");
    }
    if let Some(pack) = &skill.pack {
        println!("skill.{name}.pack={pack}");
    }
    if let Some(version) = &skill.version {
        println!("skill.{name}.version={version}");
    }
    if let Some(description) = &skill.description {
        println!("skill.{name}.description={description}");
    }
    if !skill.memory_read_scopes.is_empty() {
        println!(
            "skill.{name}.permissions.memory.read={}",
            skill.memory_read_scopes.join(",")
        );
    }
    if skill.notes_read || skill.notes_write {
        println!("skill.{name}.permissions.notes.read={}", skill.notes_read);
        println!("skill.{name}.permissions.notes.write={}", skill.notes_write);
    }
    for folder in &skill.artifact_folders {
        println!(
            "skill.{name}.folder id={} path={} kind={:?} access={:?} exists={}",
            folder.id,
            folder.path.display(),
            folder.kind,
            folder.access,
            folder.exists
        );
        for value in &folder.provides {
            println!("skill.{name}.folder.{}.provides={value}", folder.id);
        }
        if let Some(schema) = &folder.schema {
            println!("skill.{name}.folder.{}.schema={schema}", folder.id);
        }
        for rule in &folder.create {
            println!(
                "skill.{name}.folder.{}.create.when={}",
                folder.id,
                skill_folder_create_situation(rule.when)
            );
        }
        for readiness in &folder.readiness {
            println!(
                "skill.{name}.folder.{}.ready.when={} action={}",
                folder.id,
                skill_folder_create_situation(readiness.situation),
                skill_folder_sync_action(readiness.action)
            );
        }
        for warning in &folder.warnings {
            println!("skill.{name}.folder.{}.warning={warning}", folder.id);
        }
        for error in &folder.errors {
            println!("skill.{name}.folder.{}.error={error}", folder.id);
        }
    }
    for warning in &skill.warnings {
        println!("skill.{name}.warning={warning}");
    }
    for error in &skill.errors {
        println!("skill.{name}.error={error}");
    }
}

fn workspace_skill_key(skill: &WorkspaceSkillStatus) -> String {
    skill
        .name
        .clone()
        .unwrap_or_else(|| format!("path:{}", skill.path.display()))
}

fn print_workspace_skill_diagnostic(diagnostic: &WorkspaceSkillDiagnostic) {
    print!(
        "diagnostic severity={} scope={} code={} message={}",
        workspace_skill_diagnostic_severity(diagnostic.severity),
        workspace_skill_diagnostic_scope(diagnostic.scope),
        diagnostic.code,
        diagnostic.message
    );
    if let Some(component) = &diagnostic.component {
        print!(" component={component}");
    }
    if let Some(skill) = &diagnostic.skill {
        print!(" skill={skill}");
    }
    if let Some(skill_path) = &diagnostic.skill_path {
        print!(" skill_path={}", skill_path.display());
    }
    if let Some(folder_id) = &diagnostic.folder_id {
        print!(" folder={folder_id}");
    }
    if let Some(path) = &diagnostic.path {
        print!(" path={}", path.display());
    }
    println!();
}

fn workspace_skill_diagnostic_severity(severity: WorkspaceSkillDiagnosticSeverity) -> &'static str {
    match severity {
        WorkspaceSkillDiagnosticSeverity::Warning => "warning",
        WorkspaceSkillDiagnosticSeverity::Error => "error",
    }
}

fn workspace_skill_diagnostic_scope(scope: WorkspaceSkillDiagnosticScope) -> &'static str {
    match scope {
        WorkspaceSkillDiagnosticScope::Workspace => "workspace",
        WorkspaceSkillDiagnosticScope::Component => "component",
        WorkspaceSkillDiagnosticScope::Lock => "lock",
        WorkspaceSkillDiagnosticScope::SkillManifest => "skill_manifest",
        WorkspaceSkillDiagnosticScope::SkillArtifactFolder => "skill_artifact_folder",
        WorkspaceSkillDiagnosticScope::SkillTrust => "skill_trust",
    }
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

fn print_skill_trust_update_report(report: &SkillTrustUpdateReport) {
    println!(
        "state={}",
        if report.has_errors() { "invalid" } else { "ok" }
    );
    println!("workspace_root={}", report.workspace_root.display());
    println!("trust_store_path={}", report.trust_store_path.display());
    println!("skill_name={}", report.skill_name);
    println!("action={:?}", report.action);
    println!("dry_run={}", report.dry_run);
    println!("wrote={}", report.wrote);
    if let Some(record) = &report.record {
        println!("trust.skill_name={}", record.skill_name);
        println!("trust.source={}", record.source);
        println!("trust.workspace_root={}", record.workspace_root.display());
        println!("trust.artifact_identity={}", record.artifact_identity);
        println!("trust.package_digest={}", record.package_digest);
        println!("trust.remote={}", record.remote);
        println!("trust.ref={}", record.ref_name);
        println!("trust.commit={}", record.commit);
        println!("trust.tree={}", record.tree);
        println!("trust.approved_at={}", record.approved_at);
        println!("trust.agentlibre_version={}", record.agentlibre_version);
        println!("trust.revoked={}", record.revoked);
        if let Some(revoked_at) = &record.revoked_at {
            println!("trust.revoked_at={revoked_at}");
        }
    }
    for warning in &report.warnings {
        println!("warning={warning}");
    }
    for error in &report.errors {
        println!("error={error}");
    }
}

fn cpu_fallback_for_failed_turn<'a>(
    plans: Option<&'a agl_model::RuntimePlanSet>,
    message: &str,
) -> Option<&'a agl_model::RuntimePlan> {
    let plans = plans?;
    (plans.selected.runtime.gpu_layers > 0 && is_gpu_load_failure_message(message))
        .then_some(plans.cpu_fallback.as_ref())
        .flatten()
}

fn is_gpu_load_failure_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("failed to load")
        || message.contains("explicit runtime plan is not a current")
        || ((message.contains("gpu") || message.contains("vulkan") || message.contains("cuda"))
            && (message.contains("memory") || message.contains("device")))
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
    use agl_config::ResolvedInferenceConfig;
    #[cfg(unix)]
    use agl_inference::{
        InferenceFinishReason, InferenceResponse, InferenceResponseMetadata, ModelManagerStatus,
        WorkerRuntimeStatusHandle,
    };

    use crate::args::ConfigCommand;

    use super::*;

    fn serve_options() -> ServeOptions {
        ServeOptions {
            socket_path: None,
            systemd_activation: false,
            config: None,
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
                },
            })
        }

        fn clear_context(
            &self,
            _config: &ResolvedInferenceConfig,
            _session_id: &agl_ids::SessionId,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn release_context(
            &self,
            _config: &ResolvedInferenceConfig,
            _session_id: &agl_ids::SessionId,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn status(&self) -> anyhow::Result<ModelManagerStatus> {
            Ok(ModelManagerStatus::default())
        }

        fn device_inventory(&self) -> anyhow::Result<Vec<InferenceDeviceInfo>> {
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
        let model_path = runtime.paths.data_dir.join("fake-model.gguf");
        std::fs::create_dir_all(model_path.parent().unwrap()).unwrap();
        std::fs::write(&model_path, b"fake model fixture").unwrap();
        let config_path = runtime.paths.default_local_inference_config();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            config_path,
            format!(
                r#"[backend]
kind = "llama_cpp"
model = "{}"

[runtime]
gpu_layers = 0
context_tokens = 4096
threads = 2
batch_size = 128
ubatch_size = 64

[model]
dialect = "gemma4"
tool_call_format = "gemma_function_call"
"#,
                model_path.display()
            ),
        )
        .unwrap();
        agl_config::write_model_bindings(
            agl_config::model_bindings_path(&runtime.paths.config_dir),
            &agl_config::ModelBindings {
                version: 1,
                models: std::collections::BTreeMap::from([(
                    agl_config::ModelId::new("fake-daemon-model").unwrap(),
                    agl_config::ModelBinding { path: model_path },
                )]),
            },
        )
        .unwrap();
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
            let async_runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            async_runtime.block_on(async move {
                let listener = tokio::net::UnixListener::bind(&socket_path)?;
                ready_tx.send(()).unwrap();
                let (stream, _) = listener.accept().await?;
                let state = agl_daemon::SharedDaemonState::new(
                    runtime,
                    InferenceOptions::default(),
                    InferenceClientHandle::new(DaemonTestInference { calls }),
                    WorkerRuntimeStatusHandle::default(),
                );
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
            InferenceAuthoritySurface::InitInventory,
            InferenceAuthoritySurface::FunctionSmoke,
            InferenceAuthoritySurface::Interactive,
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
            InferenceAuthoritySurface::InitInventory,
            InferenceAuthoritySurface::FunctionSmoke,
        ] {
            assert_eq!(
                inference_authority_decision(surface, DaemonConnectionClass::Unavailable),
                InferenceAuthorityDecision::Standalone
            );
        }
        assert_eq!(
            inference_authority_decision(
                InferenceAuthoritySurface::Interactive,
                DaemonConnectionClass::Unavailable
            ),
            InferenceAuthorityDecision::Reject
        );

        let available: std::result::Result<(), ClientError> = Ok(());
        let unavailable: std::result::Result<(), ClientError> =
            Err(ClientError::DaemonUnavailable("absent".to_owned()));
        let incompatible: std::result::Result<(), ClientError> = Err(ClientError::SchemaMismatch {
            expected: "agentlibre.event.v6alpha",
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

    #[test]
    fn cpu_fallback_classifier_accepts_load_device_failures_only() {
        assert!(is_gpu_load_failure_message(
            "model abc failed to load: Vulkan device is out of memory"
        ));
        assert!(is_gpu_load_failure_message(
            "CUDA device disappeared while loading weights"
        ));
        assert!(!is_gpu_load_failure_message(
            "inference generation failed: malformed tool output"
        ));
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
            assert!(matches!(
                request.kind,
                agl_protocol::DaemonRequestKind::Hello(_)
            ));
            serde_json::to_writer(
                &mut stream,
                &agl_protocol::DaemonEvent::new(
                    Some(request.request_id),
                    agl_protocol::DaemonEventKind::Hello(agl_protocol::HelloEvent {
                        protocol_version: agl_protocol::PROTOCOL_VERSION.to_owned(),
                        product_version: "test".to_owned(),
                        daemon_instance_id: agl_ids::DaemonInstanceId::generate(),
                        capabilities: Vec::new(),
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
        assert!(rendered.contains("lacks required cron capability SessionOpen"));
        assert!(rendered.contains("refusing standalone inference"));
        assert!(!runtime.paths.inference_worker_temp_root().exists());
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn direct_run_uses_a_compatible_daemon_for_inference() {
        let root = std::env::temp_dir().join(format!(
            "agl-cli-run-compatible-daemon-{}",
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
artifact:
  schema: agentlibre.artifact/v1
  type: function
  id: daemon-test
  version: 1.0.0
  payload_schema: agentlibre.function/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires: []
title: Daemon test
runtime:
  tool_mode: read-only
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
            "agl-cli-cron-compatible-daemon-{}",
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
        let inference_serve = CliCommand::Inference(InferenceCommand::Serve(serve_options()));
        assert_eq!(
            cli_runtime_profile(&inference_serve),
            CliRuntimeProfile::FullBatch
        );

        let run = CliCommand::Run(RunOptions::default());
        assert_eq!(cli_runtime_profile(&run), CliRuntimeProfile::Interactive);
        assert_eq!(
            process_mode_for_command(&run),
            AgentLibreProcessMode::Interactive
        );
        let inference_run = CliCommand::Inference(InferenceCommand::Run(RunOptions::default()));
        assert_eq!(
            cli_runtime_profile(&inference_run),
            CliRuntimeProfile::Interactive
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
            r#"version = 2
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

        assert_eq!(options.function_ref.as_deref(), Some("coding"));
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
            function_root.join("FUNCTION.md"),
            r#"---
artifact:
  schema: agentlibre.artifact/v1
  type: function
  id: coding
  version: 1.0.0
  payload_schema: agentlibre.function/v2
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
