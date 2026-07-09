use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use agl_chat::{
    ChatOptions, ChatService, ChatTurnStatus, DEFAULT_MAX_OUTPUT_TOKENS, InferenceOptions,
    ToolAccessMode as ChatToolAccessMode, chat_workspace_root, default_run_id,
};
use agl_client::AgentLibreClient;
use agl_cron::{
    CronJob, CronJobDraft, CronRepository, CronRun, CronTargetKind,
    STORE_STATUS_BUILTIN_CRON_TARGET, unsupported_builtin_cron_target_message,
    validate_builtin_cron_target,
};
use agl_daemon::{
    CronExecution, CronNotification, CronNotifier, CronTargetExecutor, DaemonOptions, DaemonServer,
    default_socket_path, render_cron_notification_body, render_cron_skill_prompt,
    run_cron_skill_chat_turn, run_cron_tick,
};
use agl_protocol::{HelloRequest, PROTOCOL_VERSION};
use agl_repo::{
    ComponentStatus, RepoComponentInitOptions as AglRepoComponentInitOptions, init_repo_component,
    read_workspace_default_function,
};
use agl_runtime::{
    AgentLibreHistoryConfig, AgentLibreLoggingConfig, AgentLibrePaths, AgentLibreProcessMode,
    AgentLibreRuntimeConfig, AgentLibreWorkspaceConfig, init_tracing,
};
use agl_skills::{
    SkillFolderCreateSituation, SkillFolderSyncActionKind,
    SkillFolderSyncOptions as AglSkillFolderSyncOptions, SkillFolderSyncReport,
    SkillLockOptions as AglSkillLockOptions, SkillLockReport, SkillPermissions,
    SkillTrustOptions as AglSkillTrustOptions, SkillTrustUpdateReport, WorkspaceSkillDiagnostic,
    WorkspaceSkillDiagnosticScope, WorkspaceSkillDiagnosticSeverity, WorkspaceSkillReport,
    WorkspaceSkillStatus, builtin_registry, lock_workspace_skills, revoke_workspace_skill,
    sync_workspace_skill_folders, trust_workspace_skill, workspace_skill_report_with_trust,
};
use agl_store::{AglStore, IdempotencyOutcome, MatrixNotificationOutboxDraft};
use anyhow::{Context, Result, bail};

mod args;
mod chat;
mod config;
mod function;
mod init;
mod memory;
mod notes;
mod repo;
mod store;

use args::{
    CliCommand, CronAddOptions, CronCommand, CronDeleteOptions, CronDisableOptions,
    CronEnableOptions, CronHistoryOptions, CronListOptions, CronRunOptions, CronShowOptions,
    CronTargetArg, CronTargetKindArg, CronTickOptions, DaemonStatusOptions, InferenceCommand,
    RunOptions, ServeOptions, SkillCommand, SkillFolderSyncOptions, SkillFolderSyncSituationArg,
    SkillInitOptions, SkillInspectOptions, SkillListOptions, SkillListSourceArg, SkillLockOptions,
    SkillRevokeOptions, SkillStatusOptions, SkillTrustOptions, SkillVerifyOptions, parse_cli,
    print_completion, print_usage,
};
use chat::{CHAT_COMMANDS_HELP, ChatCommand, ParsedChatInput, parse_chat_input};
use config::run_config;
use function::run_function;
use init::run_init;
use memory::run_memory;
use notes::run_notes;
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

fn cli_runtime_profile(command: &CliCommand) -> CliRuntimeProfile {
    match command {
        CliCommand::Run(_)
        | CliCommand::Chat(_)
        | CliCommand::Inference(InferenceCommand::Run(_) | InferenceCommand::Chat(_)) => {
            CliRuntimeProfile::Interactive
        }
        CliCommand::Config(_)
        | CliCommand::Cron(_)
        | CliCommand::Function(_)
        | CliCommand::Init(_)
        | CliCommand::Store(_)
        | CliCommand::Repo(_)
        | CliCommand::Skill(_)
        | CliCommand::Memory(_)
        | CliCommand::Notes(_)
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
        CliCommand::Help { bin_name } => print_usage(bin_name),
        CliCommand::HelpPrinted => Ok(()),
        CliCommand::Completion { shell } => {
            print_completion(shell);
            Ok(())
        }
        CliCommand::Config(command) => run_config(command, runtime),
        CliCommand::Cron(command) => run_cron(command, runtime),
        CliCommand::Store(command) => run_store(command, runtime),
        CliCommand::Function(command) => run_function(command, runtime),
        CliCommand::Init(options) => run_init(options, runtime),
        CliCommand::Memory(command) => run_memory(command, runtime),
        CliCommand::Notes(command) => run_notes(command, runtime),
        CliCommand::Repo(command) => run_repo(command),
        CliCommand::Skill(command) => run_skill(command, runtime),
        CliCommand::Serve(options) => run_serve(options, runtime),
        CliCommand::Inference(command) => run_inference(command, runtime),
        CliCommand::DaemonStatus(options) => run_daemon_status(options, runtime),
        CliCommand::Run(options) => run_one_shot(options, runtime),
        CliCommand::Chat(options) => run_chat(options, runtime),
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
    let execution = execute_cron_target(&job, store, runtime, options.mock_skill_execution);
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
    let mut executor = CliCronExecutor {
        store,
        runtime,
        mock_skill_execution: options.mock_skill_execution,
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
) -> CronExecution {
    match run_cron_target(job, store, runtime, mock_skill_execution) {
        Ok(result_ref) => CronExecution::succeeded(result_ref),
        Err(err) => CronExecution::failed(format!("{err:#}")),
    }
}

fn run_cron_target(
    job: &CronJob,
    store: &AglStore,
    runtime: &AgentLibreRuntimeConfig,
    mock_skill_execution: bool,
) -> Result<String> {
    match job.target_kind {
        CronTargetKind::Builtin => run_builtin_cron_target(job, store),
        CronTargetKind::Skill if mock_skill_execution => run_mock_skill_cron_target(job),
        CronTargetKind::Skill => run_skill_cron_target(job, runtime),
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

fn run_skill_cron_target(job: &CronJob, runtime: &AgentLibreRuntimeConfig) -> Result<String> {
    run_cron_skill_chat_turn(job, runtime, InferenceOptions::default(), None)
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
}

impl CronTargetExecutor for CliCronExecutor<'_> {
    fn execute(&mut self, job: &CronJob) -> CronExecution {
        execute_cron_target(job, self.store, self.runtime, self.mock_skill_execution)
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
        SkillCommand::Lock(options) => run_skill_lock(options),
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

fn run_skill_lock(options: SkillLockOptions) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "skill lock", "starting command");
    let report = lock_workspace_skills(
        std::env::current_dir().context("failed to resolve current directory")?,
        &AglSkillLockOptions {
            dry_run: options.dry_run,
        },
    )?;

    crate::print_json_or(options.json, &report, || print_skill_lock_report(&report))?;

    if report.has_errors() {
        bail!("workspace skill lock failed");
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
        run_id: options.run_id.clone(),
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
        run_id: options.run_id.clone(),
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
) -> Result<Option<agl_functions::RuntimeFunction>> {
    options
        .function_ref
        .as_deref()
        .map(|reference| {
            let workspace_root =
                runtime.resolve_workspace_root(options.workspace_root.as_deref())?;
            let require_profile = options.config.is_none()
                && std::env::var_os("AGL_LOCAL_INFERENCE_CONFIG").is_none();
            if require_profile {
                agl_functions::resolve_runtime_function(
                    reference,
                    &workspace_root,
                    &runtime.paths.config_dir,
                )
            } else {
                agl_functions::resolve_runtime_function_allow_missing_profile(
                    reference,
                    &workspace_root,
                    &runtime.paths.config_dir,
                )
            }
            .with_context(|| format!("failed to resolve function `{reference}`"))
        })
        .transpose()
}

fn args_tool_mode_from_function(mode: agl_functions::FunctionToolMode) -> args::ToolAccessMode {
    match mode {
        agl_functions::FunctionToolMode::ReadOnly => args::ToolAccessMode::ReadOnly,
        agl_functions::FunctionToolMode::Write => args::ToolAccessMode::Write,
        agl_functions::FunctionToolMode::Execute => args::ToolAccessMode::Execute,
        agl_functions::FunctionToolMode::Approve => args::ToolAccessMode::Approve,
        agl_functions::FunctionToolMode::Admin => args::ToolAccessMode::Admin,
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
        InferenceCommand::Chat(options) => run_chat_raw(options, runtime),
        InferenceCommand::Serve(options) => run_serve_raw(options, runtime),
    }
}

fn run_one_shot(mut options: RunOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    apply_workspace_default_function_to_run(&mut options, runtime)?;
    run_one_shot_raw(options, runtime)
}

fn run_one_shot_raw(options: RunOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "run", "starting command");
    let prompt = options
        .prompt
        .clone()
        .context("run requires PROMPT or --prompt TEXT")?;
    let chat_options = one_shot_chat_options_from_run_options(&options, runtime)?;
    let tool_mode = chat_options.inference.tool_mode;
    let mut chat_service = ChatService::open(chat_options, runtime)?;
    let summary = chat_service.summary();
    tracing::info!(
        target: "agentlibre::app",
        run_id = %summary.run_id,
        event_stream = %summary.event_stream.display(),
        workspace_root = %summary.workspace_root.display(),
        tool_mode = tool_mode.as_str(),
        "runtime loop host initialized"
    );
    match chat_service.run_user_turn(&prompt)?.status {
        ChatTurnStatus::Answered { answer } => println!("{answer}"),
        ChatTurnStatus::Stopped { reason } => {
            println!("stopped=true reason={}", reason.as_str());
        }
    }
    Ok(())
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
    if let Some(socket_path) = options.socket_path {
        daemon_options.socket_path = socket_path;
    }
    println!("socket_path={}", daemon_options.socket_path.display());
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
    match AgentLibreClient::connect(&socket_path) {
        Ok(mut client) => match client.hello(HelloRequest {
            client_name: Some("agl-status".to_string()),
            accepted_protocol_versions: vec![PROTOCOL_VERSION.to_string()],
        }) {
            Ok(hello) => {
                println!("state=running");
                println!("socket_path={}", socket_path.display());
                println!("protocol_version={}", hello.protocol_version);
                println!("product_version={}", hello.product_version);
                Ok(())
            }
            Err(err) => {
                println!("state=unhealthy");
                println!("socket_path={}", socket_path.display());
                println!("error={err:#}");
                Ok(())
            }
        },
        Err(err) => {
            println!("state=not_running");
            println!("socket_path={}", socket_path.display());
            println!("next_step=agl serve");
            tracing::debug!(
                target: "agentlibre::app",
                socket_path = %socket_path.display(),
                error = %err,
                "daemon status connection failed"
            );
            Ok(())
        }
    }
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

fn skill_report_state(state: agl_skills::SkillReportState) -> &'static str {
    match state {
        agl_skills::SkillReportState::Ok => "ok",
        agl_skills::SkillReportState::Warning => "warning",
        agl_skills::SkillReportState::Invalid => "invalid",
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
    if let Some(version) = skill.version {
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
}

fn print_skill_lock_report(report: &SkillLockReport) {
    println!(
        "state={}",
        if report.has_errors() { "invalid" } else { "ok" }
    );
    println!("workspace_root={}", report.workspace_root.display());
    println!("lock_path={}", report.lock_path.display());
    println!("dry_run={}", report.dry_run);
    println!("wrote={}", report.wrote);
    if let Some(lock) = &report.lock {
        println!("lock.version={}", lock.version);
        println!("lock.locked_at={}", lock.locked_at);
        if let Some(component) = lock.components.get("skills") {
            println!("lock.component.skills.path={}", component.path.display());
            println!("lock.component.skills.kind={:?}", component.kind);
            println!("lock.component.skills.remote={}", component.remote);
            println!("lock.component.skills.ref={}", component.ref_name);
            println!("lock.component.skills.commit={}", component.commit);
            println!("lock.component.skills.tree={}", component.tree);
        }
        for skill in &lock.skills {
            println!(
                "lock.skill name={} path={} source={} component={} locked_at={}",
                skill.name,
                skill.path.display(),
                skill.source,
                skill.component,
                skill.locked_at
            );
        }
    }
    for warning in &report.warnings {
        println!("warning={warning}");
    }
    for error in &report.errors {
        println!("error={error}");
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

fn run_chat(mut options: RunOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    apply_workspace_default_function_to_run(&mut options, runtime)?;
    run_chat_raw(options, runtime)
}

fn run_chat_raw(mut options: RunOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "chat", "starting command");
    let run_id = options.run_id.clone().unwrap_or_else(default_run_id);
    options.run_id = Some(run_id.clone());
    let mut chat_service =
        ChatService::open(chat_options_from_run_options(&options, runtime)?, runtime)?;
    let summary = chat_service.summary();
    let stdin = io::stdin();

    tracing::info!(
        target: "agentlibre::app",
        session_id = %summary.session_id,
        run_id = %summary.run_id,
        artifact_root = %summary.artifact_root.display(),
        event_stream = %summary.event_stream.display(),
        workspace_root = %summary.workspace_root.display(),
        tool_mode = summary.tool_mode,
        history_enabled = summary.history_enabled,
        resumed = summary.resumed,
        replayed_messages = summary.replayed_messages,
        "chat session started"
    );
    println!("session_id={}", chat_service.session_id());

    loop {
        print!("agl> ");
        io::stdout().flush().context("failed to flush prompt")?;

        let mut input = String::new();
        let bytes_read = stdin
            .read_line(&mut input)
            .context("failed to read chat input")?;
        if bytes_read == 0 {
            break;
        }

        let input = match parse_chat_input(&input) {
            ParsedChatInput::Empty => {
                continue;
            }
            ParsedChatInput::Message(input) => input,
            ParsedChatInput::UnknownCommand(command) => {
                println!("unknown_command={command}");
                continue;
            }
            ParsedChatInput::Command(ChatCommand::Help) => {
                print!("{CHAT_COMMANDS_HELP}");
                continue;
            }
            ParsedChatInput::Command(ChatCommand::Session) => {
                print_chat_session_summary(&chat_service);
                continue;
            }
            ParsedChatInput::Command(ChatCommand::Reload) => {
                match chat_service.reload_runtime_context() {
                    Ok(visible_tools) => {
                        tracing::info!(
                            target: "agentlibre::app",
                            session_id = %chat_service.session_id(),
                            run_id = %chat_service.run_id(),
                            workspace_root = %chat_service.workspace_root().display(),
                            visible_tools,
                            "chat runtime context reloaded"
                        );
                        println!("context_reloaded=true visible_tools={visible_tools}");
                        println!("workspace_root={}", chat_service.workspace_root().display());
                        println!("profile_reloaded=false");
                        println!("profile_reload_next_step=start a new chat or run command");
                    }
                    Err(err) => {
                        tracing::warn!(
                            target: "agentlibre::app",
                            session_id = %chat_service.session_id(),
                            run_id = %chat_service.run_id(),
                            error = %err,
                            "chat runtime context reload failed"
                        );
                        println!("reload_error={err:#}");
                    }
                }
                continue;
            }
            ParsedChatInput::Workspace(path) => {
                if let Some(path) = path {
                    let root = chat_workspace_root(path, chat_service.workspace_root());
                    if let Err(err) = chat_service.set_workspace_root(&root) {
                        tracing::warn!(
                            target: "agentlibre::app",
                            session_id = %chat_service.session_id(),
                            run_id = %chat_service.run_id(),
                            requested_workspace_root = %root.display(),
                            error = %err,
                            "chat workspace root change failed"
                        );
                        println!("workspace_error={err:#}");
                    } else {
                        tracing::info!(
                            target: "agentlibre::app",
                            session_id = %chat_service.session_id(),
                            run_id = %chat_service.run_id(),
                            workspace_root = %chat_service.workspace_root().display(),
                            "chat workspace root changed"
                        );
                    }
                }
                println!("workspace_root={}", chat_service.workspace_root().display());
                continue;
            }
            ParsedChatInput::Command(ChatCommand::Clear) => {
                let cleared_messages = chat_service.clear_context()?;
                tracing::info!(
                    target: "agentlibre::app",
                    session_id = %chat_service.session_id(),
                    run_id = %chat_service.run_id(),
                    cleared_messages,
                    "chat context cleared"
                );
                println!("context_cleared=true cleared_messages={cleared_messages}");
                continue;
            }
            ParsedChatInput::Command(ChatCommand::Exit) => {
                chat_service.request_exit()?;
                break;
            }
        };

        match chat_service.run_user_turn(input)?.status {
            ChatTurnStatus::Answered { answer } => {
                println!("assistant> {answer}");
            }
            ChatTurnStatus::Stopped { reason } => {
                println!("stopped=true reason={}", reason.as_str());
            }
        }
    }

    chat_service.finish_eof_if_needed()?;
    tracing::info!(
        target: "agentlibre::app",
        session_id = %chat_service.session_id(),
        run_id = %chat_service.run_id(),
        "chat session finished"
    );
    Ok(())
}

fn print_chat_session_summary(chat_service: &ChatService) {
    println!("session_id={}", chat_service.session_id());
    println!("run_id={}", chat_service.run_id());
    println!("artifact_root={}", chat_service.artifact_root().display());
    println!("workspace_root={}", chat_service.workspace_root().display());
}

#[cfg(test)]
mod tests {
    use crate::args::ConfigCommand;

    use super::*;

    fn serve_options() -> ServeOptions {
        ServeOptions {
            socket_path: None,
            config: None,
            function_ref: None,
            artifact_root: None,
            run_id: None,
            workspace_root: None,
            max_output_tokens: None,
            tool_mode: None,
            skills: Vec::new(),
            memory: false,
        }
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
            r#"
version = 1
profile = "repo-workflow"

[functions]
default = "coding"

[components.state]
path = ".agl/state"
kind = "ignored"
"#,
        )
        .unwrap();
        let runtime = AgentLibreRuntimeConfig {
            paths: AgentLibrePaths::from_agl_home(root.join("home")),
            logging: AgentLibreLoggingConfig::default(),
            history: AgentLibreHistoryConfig::default(),
            workspace: AgentLibreWorkspaceConfig::default(),
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
schema: agentfunction/v1
id: coding
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
            session_id: Some("ignored-session".to_string()),
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
