use std::env;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process;

use agl_chat::{
    ChatLoopHost, ChatOptions, ChatService, ChatTurnStatus, InferenceOptions, InferenceSession,
    ToolAccessMode as ChatToolAccessMode, assistant_text_for_terminal, build_turn_input,
    chat_workspace_root, default_run_id,
};
use agl_client::AgentLibreClient;
use agl_cron::{CronJob, CronJobDraft, CronRepository, CronRun, CronRunStatus, CronTargetKind};
use agl_daemon::{DaemonOptions, DaemonServer, default_socket_path};
use agl_loop::{TurnOutput, run_turn};
use agl_memory::{
    MemoryDraft, MemoryEntry, MemoryKind, MemoryRepository, MemoryScope, MemoryScopeKind,
    MemorySearchQuery, MemorySuggestion, MemorySuggestionDraft, MemorySuggestionQuery,
    MemorySuggestionStatus,
};
use agl_notes::{Note, NoteDraft, NoteLink, NoteRepository, NoteSearchQuery, NoteUpdate};
use agl_protocol::{HelloRequest, PROTOCOL_VERSION};
use agl_repo::{
    ComponentStatus, HookInstallReport, RepoHooksOptions as AglRepoHooksOptions, RepoInitAction,
    RepoInitOptions as AglRepoInitOptions, RepoInitReport,
    RepoStatusOptions as AglRepoStatusOptions, RepoStatusReport, init_repo_workspace,
    install_repo_hooks, status_repo_workspace,
};
use agl_runtime::{
    AgentLibreHistoryConfig, AgentLibreLoggingConfig, AgentLibrePaths, AgentLibreProcessMode,
    AgentLibreRuntimeConfig, AgentLibreWorkspaceConfig, init_tracing,
};
use agl_skills::{
    SkillLockOptions as AglSkillLockOptions, SkillLockReport, SkillPermissions,
    SkillTrustOptions as AglSkillTrustOptions, SkillTrustUpdateReport, WorkspaceSkillReport,
    WorkspaceSkillStatus, builtin_registry, lock_workspace_skills, revoke_workspace_skill,
    trust_workspace_skill, workspace_skill_report, workspace_skill_report_with_trust,
};
use agl_store::{AglStore, StoreDomain, StoreExportOptions as AglStoreExportOptions, StoreStatus};
use anyhow::{Context, Result, bail};

mod args;
mod chat;
mod config;

use args::{
    CliCommand, CronAddOptions, CronCommand, CronDeleteOptions, CronDisableOptions,
    CronEnableOptions, CronHistoryOptions, CronListOptions, CronRunOptions, CronShowOptions,
    CronTargetArg, CronTargetKindArg, DaemonStatusOptions, MemoryAddOptions, MemoryApproveOptions,
    MemoryCommand, MemoryDeleteOptions, MemoryKindArg, MemoryListOptions,
    MemoryListSuggestionsOptions, MemoryRejectOptions, MemoryScopeArg, MemorySearchOptions,
    MemoryShowOptions, MemorySuggestOptions, MemorySuggestionStatusArg, NotesAddOptions,
    NotesCommand, NotesDeleteOptions, NotesLinkOptions, NotesListOptions, NotesRememberOptions,
    NotesSearchOptions, NotesShowOptions, NotesUpdateOptions, RepoCommand, RepoHooksOptions,
    RepoInitOptions, RepoStatusOptions, RunOptions, ServeOptions, SkillCommand,
    SkillInspectOptions, SkillListOptions, SkillLockOptions, SkillRevokeOptions,
    SkillStatusOptions, SkillTrustOptions, SkillVerifyOptions, StoreCommand, StoreDomainArg,
    StoreExportCliOptions, StoreStatusOptions, parse_cli, print_completion, print_usage,
};
use chat::{CHAT_COMMANDS_HELP, ChatCommand, ParsedChatInput, parse_chat_input};
use config::run_config;

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
    let _tracing_guards = match init_tracing(
        &runtime.paths,
        &runtime.logging,
        process_mode_for_command(&command),
    ) {
        Ok(guards) => Some(guards),
        Err(err) => {
            eprintln!("warning: failed to initialize logging: {err:#}");
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
    if matches!(
        command,
        CliCommand::Config(_)
            | CliCommand::Cron(_)
            | CliCommand::Store(_)
            | CliCommand::Repo(_)
            | CliCommand::Skill(_)
            | CliCommand::Memory(_)
            | CliCommand::Notes(_)
            | CliCommand::DaemonStatus(_)
    ) {
        return Ok(AgentLibreRuntimeConfig {
            paths,
            logging: AgentLibreLoggingConfig::from_env(),
            history: AgentLibreHistoryConfig::default(),
            workspace: AgentLibreWorkspaceConfig::default(),
        });
    }

    AgentLibreRuntimeConfig::from_paths(paths)
}

fn process_mode_for_command(command: &CliCommand) -> AgentLibreProcessMode {
    match command {
        CliCommand::Infer(_) | CliCommand::Chat(_) => AgentLibreProcessMode::Interactive,
        CliCommand::Serve(_)
        | CliCommand::Repo(_)
        | CliCommand::Skill(_)
        | CliCommand::Cron(_)
        | CliCommand::Store(_)
        | CliCommand::Memory(_)
        | CliCommand::Notes(_)
        | CliCommand::DaemonStatus(_)
        | CliCommand::Help { .. }
        | CliCommand::HelpPrinted
        | CliCommand::Completion { .. }
        | CliCommand::Config(_) => AgentLibreProcessMode::Batch,
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
        CliCommand::Memory(command) => run_memory(command, runtime),
        CliCommand::Notes(command) => run_notes(command, runtime),
        CliCommand::Repo(command) => run_repo(command),
        CliCommand::Skill(command) => run_skill(command, runtime),
        CliCommand::Serve(options) => run_serve(options, runtime),
        CliCommand::DaemonStatus(options) => run_daemon_status(options, runtime),
        CliCommand::Infer(options) => run_infer(options, runtime),
        CliCommand::Chat(options) => run_chat(options, runtime),
    }
}

fn run_store(command: StoreCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "store", "starting command");
    let store = AglStore::open_default(&runtime.paths).context("failed to open store")?;

    match command {
        StoreCommand::Status(options) => run_store_status(options, &store),
        StoreCommand::Export(options) => run_store_export(options, &store),
    }
}

fn run_store_status(options: StoreStatusOptions, store: &AglStore) -> Result<()> {
    let status = store.status().context("failed to read store status")?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_store_status(&status);
    }
    Ok(())
}

fn run_store_export(options: StoreExportCliOptions, store: &AglStore) -> Result<()> {
    let domain = store_domain(options.domain);
    if let Some(parent) = options
        .out
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create store export directory {}",
                parent.display()
            )
        })?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .create_new(!options.force)
        .truncate(options.force)
        .open(&options.out)
        .with_context(|| {
            if options.force {
                format!("failed to open store export path {}", options.out.display())
            } else {
                format!(
                    "failed to create store export path {}; pass --force to overwrite",
                    options.out.display()
                )
            }
        })?;
    let records = store
        .export_domain_jsonl(
            &AglStoreExportOptions {
                domain,
                include_deleted: options.include_deleted,
            },
            file,
        )
        .context("failed to export store domain")?;

    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "domain": domain.as_str(),
                "path": options.out,
                "records": records,
                "include_deleted": options.include_deleted,
            }))?
        );
    } else {
        println!("store.exported=true");
        println!("store.export.domain={}", domain.as_str());
        println!("store.export.path={}", options.out.display());
        println!("store.export.records={records}");
        println!("store.export.include_deleted={}", options.include_deleted);
    }
    Ok(())
}

fn run_cron(command: CronCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "cron", "starting command");
    let store = AglStore::open_default(&runtime.paths).context("failed to open cron store")?;
    let cron = CronRepository::new(&store);

    match command {
        CronCommand::Add(options) => run_cron_add(options, &cron, runtime),
        CronCommand::List(options) => run_cron_list(options, &cron),
        CronCommand::Show(options) => run_cron_show(options, &cron),
        CronCommand::Enable(options) => run_cron_enable(options, &cron),
        CronCommand::Disable(options) => run_cron_disable(options, &cron),
        CronCommand::Run(options) => run_cron_run(options, &cron, &store, runtime),
        CronCommand::History(options) => run_cron_history(options, &cron),
        CronCommand::Delete(options) => run_cron_delete(options, &cron),
    }
}

fn run_notes(command: NotesCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "notes", "starting command");
    let store = AglStore::open_default(&runtime.paths).context("failed to open notes store")?;
    let notes = NoteRepository::new(&store);

    match command {
        NotesCommand::Add(options) => run_notes_add(options, &notes),
        NotesCommand::List(options) => run_notes_list(options, &notes),
        NotesCommand::Search(options) => run_notes_search(options, &notes),
        NotesCommand::Show(options) => run_notes_show(options, &notes),
        NotesCommand::Update(options) => run_notes_update(options, &notes),
        NotesCommand::Delete(options) => run_notes_delete(options, &notes),
        NotesCommand::Link(options) => run_notes_link(options, &notes),
        NotesCommand::Remember(options) => run_notes_remember(options, &notes),
    }
}

fn run_memory(command: MemoryCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "memory", "starting command");
    let store = AglStore::open_default(&runtime.paths).context("failed to open memory store")?;
    let memory = MemoryRepository::new(&store);

    match command {
        MemoryCommand::Add(options) => run_memory_add(options, &memory),
        MemoryCommand::List(options) => run_memory_list(options, &memory),
        MemoryCommand::Search(options) => run_memory_search(options, &memory),
        MemoryCommand::Show(options) => run_memory_show(options, &memory),
        MemoryCommand::Delete(options) => run_memory_delete(options, &memory),
        MemoryCommand::Suggest(options) => run_memory_suggest(options, &memory),
        MemoryCommand::ListSuggestions(options) => run_memory_list_suggestions(options, &memory),
        MemoryCommand::Approve(options) => run_memory_approve(options, &memory),
        MemoryCommand::Reject(options) => run_memory_reject(options, &memory),
    }
}

fn run_repo(command: RepoCommand) -> Result<()> {
    match command {
        RepoCommand::Init(options) => run_repo_init(options),
        RepoCommand::Status(options) => run_repo_status(options),
        RepoCommand::InstallHooks(options) => run_install_hooks(options),
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

    if options.json {
        println!("{}", serde_json::to_string_pretty(&job)?);
    } else {
        print_cron_job_summary(&job);
    }
    Ok(())
}

fn run_cron_list(options: CronListOptions, cron: &CronRepository<'_>) -> Result<()> {
    let jobs = cron
        .list_jobs(options.include_deleted)
        .context("failed to list cron jobs")?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&jobs)?);
    } else {
        print_cron_jobs(&jobs);
    }
    Ok(())
}

fn run_cron_show(options: CronShowOptions, cron: &CronRepository<'_>) -> Result<()> {
    let job = cron
        .job(&options.id)
        .context("failed to read cron job")?
        .ok_or_else(|| anyhow::anyhow!("cron job not found: {}", options.id))?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&job)?);
    } else {
        print_cron_job_detail(&job);
    }
    Ok(())
}

fn run_cron_enable(options: CronEnableOptions, cron: &CronRepository<'_>) -> Result<()> {
    let job = cron
        .set_enabled(&options.id, true)
        .context("failed to enable cron job")?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&job)?);
    } else {
        print_cron_job_summary(&job);
    }
    Ok(())
}

fn run_cron_disable(options: CronDisableOptions, cron: &CronRepository<'_>) -> Result<()> {
    let job = cron
        .set_enabled(&options.id, false)
        .context("failed to disable cron job")?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&job)?);
    } else {
        print_cron_job_summary(&job);
    }
    Ok(())
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
    validate_stored_cron_target(&job, runtime)?;
    let execution = run_cron_target(&job, store, runtime);
    let (status, result_ref, error) = match execution {
        Ok(result_ref) => (CronRunStatus::Succeeded, Some(result_ref), None),
        Err(err) => (CronRunStatus::Failed, None, Some(format!("{err:#}"))),
    };
    let (run, outcome) = cron
        .record_manual_run_result(&job.id, status, result_ref.as_deref(), error.as_deref())
        .context("failed to record cron run")?;

    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "job": job,
                "run": run,
                "idempotency": format!("{outcome:?}"),
            }))?
        );
    } else {
        print_cron_run(&run);
        println!("cron_run.{}.idempotency={outcome:?}", run.id);
    }
    Ok(())
}

fn run_cron_history(options: CronHistoryOptions, cron: &CronRepository<'_>) -> Result<()> {
    let runs = cron
        .history(&options.id)
        .context("failed to read cron run history")?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&runs)?);
    } else {
        print_cron_runs(&runs);
    }
    Ok(())
}

fn run_cron_delete(options: CronDeleteOptions, cron: &CronRepository<'_>) -> Result<()> {
    let job = cron
        .delete_job(&options.id)
        .context("failed to delete cron job")?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&job)?);
    } else {
        println!("cron.deleted=true");
        print_cron_job_summary(&job);
    }
    Ok(())
}

fn validate_cron_target(target: &CronTargetArg, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    match target.kind {
        CronTargetKindArg::Builtin => validate_builtin_cron_target(&target.target_ref),
        CronTargetKindArg::Skill => validate_trusted_cron_skill(&target.target_ref, runtime),
    }
}

fn validate_stored_cron_target(job: &CronJob, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    match job.target_kind {
        CronTargetKind::Builtin => validate_builtin_cron_target(&job.target_ref),
        CronTargetKind::Skill => validate_trusted_cron_skill(&job.target_ref, runtime),
    }
}

fn validate_builtin_cron_target(target_ref: &str) -> Result<()> {
    match target_ref {
        "store-status" => Ok(()),
        _ => bail!(
            "unknown builtin cron target: {target_ref}; supported builtin targets: store-status"
        ),
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

fn run_cron_target(
    job: &CronJob,
    store: &AglStore,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<String> {
    match job.target_kind {
        CronTargetKind::Builtin => run_builtin_cron_target(job, store),
        CronTargetKind::Skill => run_skill_cron_target(job, runtime),
    }
}

fn run_builtin_cron_target(job: &CronJob, store: &AglStore) -> Result<String> {
    match job.target_ref.as_str() {
        "store-status" => {
            let health = store.health().context("failed to check store health")?;
            Ok(format!(
                "builtin:store-status:schema:{}",
                health.migration_version
            ))
        }
        _ => bail!(
            "unknown builtin cron target: {}; supported builtin targets: store-status",
            job.target_ref
        ),
    }
}

fn run_skill_cron_target(job: &CronJob, runtime: &AgentLibreRuntimeConfig) -> Result<String> {
    let prompt = cron_skill_prompt(job)?;
    let inference = InferenceOptions {
        skills: vec![job.target_ref.clone()],
        tool_mode: ChatToolAccessMode::Write,
        ..InferenceOptions::default()
    };
    let mut service = ChatService::open(
        ChatOptions {
            inference,
            workspace_root: None,
            session_id: None,
            no_history: false,
            new_session: true,
        },
        runtime,
    )
    .context("failed to open cron skill chat session")?;
    let summary = service.summary();
    let output = service
        .run_user_turn(&prompt)
        .context("failed to run cron skill turn")?;
    service
        .finish_eof_if_needed()
        .context("failed to finish cron skill session")?;
    match output.status {
        ChatTurnStatus::Answered { .. } => Ok(format!(
            "skill:{}:session:{}:run:{}",
            job.target_ref, summary.session_id, summary.run_id
        )),
        ChatTurnStatus::Stopped { reason } => bail!("cron skill stopped before answer: {reason:?}"),
    }
}

fn cron_skill_prompt(job: &CronJob) -> Result<String> {
    let prompt = job
        .prompt
        .as_deref()
        .context("skill cron job missing prompt")?;
    if let Some(input) = job.input.as_deref() {
        Ok(format!("{prompt}\n\nCron input:\n{input}"))
    } else {
        Ok(prompt.to_string())
    }
}

fn cron_target_kind(kind: CronTargetKindArg) -> CronTargetKind {
    match kind {
        CronTargetKindArg::Skill => CronTargetKind::Skill,
        CronTargetKindArg::Builtin => CronTargetKind::Builtin,
    }
}

fn store_domain(domain: StoreDomainArg) -> StoreDomain {
    match domain {
        StoreDomainArg::Memory => StoreDomain::Memory,
        StoreDomainArg::Notes => StoreDomain::Notes,
        StoreDomainArg::Cron => StoreDomain::Cron,
    }
}

fn run_notes_add(options: NotesAddOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let note = notes
        .add(NoteDraft::new(options.title, options.body))
        .context("failed to add note")?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&note)?);
    } else {
        print_note_summary(&note);
    }
    Ok(())
}

fn run_notes_list(options: NotesListOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let query = NoteSearchQuery {
        include_deleted: options.include_deleted,
        limit: options.limit,
        ..NoteSearchQuery::default()
    };
    let entries = notes.list(&query).context("failed to list notes")?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        print_notes(&entries);
    }
    Ok(())
}

fn run_notes_search(options: NotesSearchOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let query = NoteSearchQuery {
        text: Some(options.query),
        include_deleted: options.include_deleted,
        limit: options.limit,
    };
    let entries = notes.search(&query).context("failed to search notes")?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        print_notes(&entries);
    }
    Ok(())
}

fn run_notes_show(options: NotesShowOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let note = notes
        .get(&options.id)
        .context("failed to read note")?
        .ok_or_else(|| anyhow::anyhow!("note not found: {}", options.id))?;
    let links = notes
        .links(&options.id)
        .context("failed to read note links")?;
    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "note": note,
                "links": links,
            }))?
        );
    } else {
        print_note_detail(&note, &links);
    }
    Ok(())
}

fn run_notes_update(options: NotesUpdateOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let note = notes
        .update(
            &options.id,
            NoteUpdate {
                title: options.title,
                body: options.body,
            },
        )
        .context("failed to update note")?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&note)?);
    } else {
        print_note_summary(&note);
    }
    Ok(())
}

fn run_notes_delete(options: NotesDeleteOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let note = notes.delete(&options.id).context("failed to delete note")?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&note)?);
    } else {
        println!("note.deleted=true");
        print_note_summary(&note);
    }
    Ok(())
}

fn run_notes_link(options: NotesLinkOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let link = notes
        .link(&options.id, &options.target_ref, options.label)
        .context("failed to link note")?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&link)?);
    } else {
        print_note_link(&link);
    }
    Ok(())
}

fn run_notes_remember(options: NotesRememberOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let scope = memory_scope(options.scope, options.scope_key)?;
    let promotion = notes
        .remember(&options.id, scope, memory_kind(options.kind))
        .context("failed to promote note into memory")?;

    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "note": promotion.note,
                "memory": promotion.memory,
                "link": promotion.link,
            }))?
        );
    } else {
        println!("note.remembered=true");
        print_note_summary(&promotion.note);
        print_memory_entry_summary(&promotion.memory);
        print_note_link(&promotion.link);
    }
    Ok(())
}

fn run_memory_add(options: MemoryAddOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let scope = memory_scope(options.scope, options.scope_key)?;
    let mut draft = MemoryDraft::new(
        scope,
        memory_kind(options.kind),
        options.title,
        options.body,
    );
    draft.source_ref = options.source_ref;
    draft.confidence = options.confidence;
    let entry = memory.add(draft).context("failed to add memory entry")?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&entry)?);
    } else {
        print_memory_entry_summary(&entry);
    }
    Ok(())
}

fn run_memory_list(options: MemoryListOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let scope = memory_scope(options.scope, options.scope_key)?;
    let mut query = MemorySearchQuery::scoped(scope);
    query.include_deleted = options.include_deleted;
    query.limit = options.limit;
    let entries = memory
        .list(&query)
        .context("failed to list memory entries")?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        print_memory_entries(&entries);
    }
    Ok(())
}

fn run_memory_search(options: MemorySearchOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let scope = memory_scope(options.scope, options.scope_key)?;
    let mut query = MemorySearchQuery::text(Some(scope), options.query);
    query.include_deleted = options.include_deleted;
    query.limit = options.limit;
    let entries = memory
        .search(&query)
        .context("failed to search memory entries")?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        print_memory_entries(&entries);
    }
    Ok(())
}

fn run_memory_show(options: MemoryShowOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let entry = memory
        .get(&options.id)
        .context("failed to read memory entry")?
        .ok_or_else(|| anyhow::anyhow!("memory entry not found: {}", options.id))?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&entry)?);
    } else {
        print_memory_entry_detail(&entry);
    }
    Ok(())
}

fn run_memory_delete(options: MemoryDeleteOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let entry = memory
        .delete(&options.id)
        .context("failed to delete memory entry")?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&entry)?);
    } else {
        println!("memory.deleted=true");
        print_memory_entry_summary(&entry);
    }
    Ok(())
}

fn run_memory_suggest(options: MemorySuggestOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let scope = memory_scope(options.scope, options.scope_key)?;
    let mut draft = MemorySuggestionDraft::new(
        scope,
        memory_kind(options.kind),
        options.title,
        options.body,
        options.source_ref,
    );
    draft.confidence = options.confidence;
    let suggestion = memory
        .suggest(draft)
        .context("failed to create memory suggestion")?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&suggestion)?);
    } else {
        print_memory_suggestion_summary(&suggestion);
    }
    Ok(())
}

fn run_memory_list_suggestions(
    options: MemoryListSuggestionsOptions,
    memory: &MemoryRepository<'_>,
) -> Result<()> {
    let scope = if options.all_scopes {
        None
    } else {
        Some(memory_scope(options.scope, options.scope_key)?)
    };
    let status = options
        .status
        .map(memory_suggestion_status)
        .or(Some(MemorySuggestionStatus::Pending));
    let suggestions = memory
        .list_suggestions(&MemorySuggestionQuery {
            scope,
            status,
            limit: options.limit,
        })
        .context("failed to list memory suggestions")?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&suggestions)?);
    } else {
        print_memory_suggestions(&suggestions);
    }
    Ok(())
}

fn run_memory_approve(options: MemoryApproveOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let (suggestion, entry) = memory
        .approve_suggestion(&options.id)
        .context("failed to approve memory suggestion")?;

    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "suggestion": suggestion,
                "memory": entry,
            }))?
        );
    } else {
        println!("memory_suggestion.approved=true");
        print_memory_suggestion_summary(&suggestion);
        print_memory_entry_summary(&entry);
    }
    Ok(())
}

fn run_memory_reject(options: MemoryRejectOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let suggestion = memory
        .reject_suggestion(&options.id, options.reason.as_deref())
        .context("failed to reject memory suggestion")?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&suggestion)?);
    } else {
        println!("memory_suggestion.rejected=true");
        print_memory_suggestion_summary(&suggestion);
    }
    Ok(())
}

fn memory_scope(kind: MemoryScopeArg, key: Option<String>) -> Result<MemoryScope> {
    let kind = match kind {
        MemoryScopeArg::User => MemoryScopeKind::User,
        MemoryScopeArg::Repo => MemoryScopeKind::Repo,
        MemoryScopeArg::MatrixRoom => MemoryScopeKind::MatrixRoom,
        MemoryScopeArg::MatrixUser => MemoryScopeKind::MatrixUser,
    };
    match (kind, key) {
        (MemoryScopeKind::User, None) => Ok(MemoryScope::user()),
        (kind, Some(key)) => MemoryScope::new(kind, key).map_err(anyhow::Error::from),
        (kind, None) => bail!("--scope-key is required for --scope {}", kind.as_str()),
    }
}

fn memory_suggestion_status(status: MemorySuggestionStatusArg) -> MemorySuggestionStatus {
    match status {
        MemorySuggestionStatusArg::Pending => MemorySuggestionStatus::Pending,
        MemorySuggestionStatusArg::Approved => MemorySuggestionStatus::Approved,
        MemorySuggestionStatusArg::Rejected => MemorySuggestionStatus::Rejected,
    }
}

fn memory_kind(kind: MemoryKindArg) -> MemoryKind {
    match kind {
        MemoryKindArg::Fact => MemoryKind::Fact,
        MemoryKindArg::Preference => MemoryKind::Preference,
        MemoryKindArg::Summary => MemoryKind::Summary,
        MemoryKindArg::Decision => MemoryKind::Decision,
        MemoryKindArg::WorkingNote => MemoryKind::WorkingNote,
    }
}

fn run_skill(command: SkillCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    match command {
        SkillCommand::List(options) => run_skill_list(options, runtime),
        SkillCommand::Inspect(options) => run_skill_inspect(options, runtime),
        SkillCommand::Status(options) => run_skill_status(options, runtime),
        SkillCommand::Verify(options) => run_skill_verify(options),
        SkillCommand::Lock(options) => run_skill_lock(options),
        SkillCommand::Trust(options) => run_skill_trust(options, runtime),
        SkillCommand::Revoke(options) => run_skill_revoke(options, runtime),
    }
}

fn run_repo_init(options: RepoInitOptions) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "init", "starting command");
    let report = init_repo_workspace(
        std::env::current_dir().context("failed to resolve current directory")?,
        &AglRepoInitOptions {
            profile: options.profile,
            profile_file: options.profile_file,
            dry_run: options.dry_run,
            force: options.force,
        },
    )?;
    print_repo_init_report(&report);
    Ok(())
}

fn run_repo_status(options: RepoStatusOptions) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "status", "starting command");
    let report = status_repo_workspace(
        std::env::current_dir().context("failed to resolve current directory")?,
        &AglRepoStatusOptions {
            component: options.component,
            strict: options.strict,
        },
    )?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_repo_status_report(&report);
    }

    if report.should_fail(options.strict) {
        bail!("repo workspace status is not healthy");
    }
    Ok(())
}

fn run_install_hooks(options: RepoHooksOptions) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "install-hooks", "starting command");
    let report = install_repo_hooks(
        std::env::current_dir().context("failed to resolve current directory")?,
        &AglRepoHooksOptions {
            dry_run: options.dry_run,
            force: options.force,
        },
    )?;
    print_hook_install_report(&report);
    if report.has_errors() {
        bail!("git hook installation has conflicts");
    }
    Ok(())
}

fn run_skill_list(options: SkillListOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "skill list", "starting command");
    let registry = builtin_registry()?;
    let workspace = workspace_skill_report_with_trust(
        std::env::current_dir().context("failed to resolve current directory")?,
        skill_trust_store_path(runtime),
    )?;

    if options.json {
        let builtins = registry
            .skills()
            .iter()
            .map(|skill| {
                serde_json::json!({
                    "name": skill.harness.name,
                    "source": skill.harness.source.as_str(),
                    "pack": skill.harness.pack,
                    "description": skill.harness.description,
                    "trust": format!("{:?}", skill.trust),
                    "usable": skill.permits_context_injection(),
                    "permissions": skill.harness.permissions,
                })
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "builtins": builtins,
                "workspace": workspace,
            }))?
        );
    } else {
        for skill in registry.skills() {
            println!(
                "skill name={} source={} pack={} trust={:?} usable={}",
                skill.harness.name,
                skill.harness.source.as_str(),
                skill.harness.pack,
                skill.trust,
                skill.permits_context_injection()
            );
            print_skill_permissions(
                &format!("skill.{}", skill.harness.name),
                &skill.harness.permissions,
            );
        }
        for skill in &workspace.skills {
            print_workspace_skill_status(skill);
        }
        for warning in &workspace.warnings {
            println!("warning={warning}");
        }
        for error in &workspace.errors {
            println!("error={error}");
        }
    }

    Ok(())
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
                    "permissions": skill.harness.permissions,
                })
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "name": options.name,
                "builtins": builtins,
                "workspace": workspace_skills,
            }))?
        );
    } else {
        for skill in builtins {
            println!(
                "skill name={} source={} pack={} version={} trust={:?} usable={}",
                skill.harness.name,
                skill.harness.source.as_str(),
                skill.harness.pack,
                skill.harness.version,
                skill.trust,
                skill.permits_context_injection()
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

    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_workspace_skill_report(&report);
    }

    if report.should_fail(options.strict) {
        bail!("workspace skill status is not healthy");
    }
    Ok(())
}

fn run_skill_verify(options: SkillVerifyOptions) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "skill verify", "starting command");
    let report = workspace_skill_report(
        std::env::current_dir().context("failed to resolve current directory")?,
    )?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_workspace_skill_report(&report);
    }

    if report.should_fail(true) {
        bail!("workspace skill verification failed");
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

    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_skill_lock_report(&report);
    }

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

    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_skill_trust_update_report(&report);
    }

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

    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_skill_trust_update_report(&report);
    }

    if report.has_errors() {
        bail!("workspace skill revoke failed");
    }
    Ok(())
}

fn inference_options_from_serve_options(options: &ServeOptions) -> InferenceOptions {
    InferenceOptions {
        config: options.config.clone(),
        artifact_root: options.artifact_root.clone(),
        run_id: options.run_id.clone(),
        workspace_root: options.workspace_root.clone(),
        max_output_tokens: options.max_output_tokens,
        tool_mode: chat_tool_mode(options.tool_mode),
        skills: options.skills.clone(),
        memory: options.memory,
    }
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

fn inference_options_from_run_options(options: &RunOptions) -> InferenceOptions {
    InferenceOptions {
        config: options.config.clone(),
        artifact_root: options.artifact_root.clone(),
        run_id: options.run_id.clone(),
        workspace_root: options.workspace_root.clone(),
        max_output_tokens: options.max_output_tokens,
        tool_mode: chat_tool_mode(options.tool_mode),
        skills: options.skills.clone(),
        memory: options.memory,
    }
}

fn chat_options_from_run_options(options: &RunOptions) -> ChatOptions {
    ChatOptions {
        inference: inference_options_from_run_options(options),
        workspace_root: options.workspace_root.clone(),
        session_id: options.session_id.clone(),
        no_history: options.no_history,
        new_session: options.new_session,
    }
}

fn chat_tool_mode(mode: args::ToolAccessMode) -> ChatToolAccessMode {
    match mode {
        args::ToolAccessMode::ReadOnly => ChatToolAccessMode::ReadOnly,
        args::ToolAccessMode::Write => ChatToolAccessMode::Write,
    }
}

fn run_infer(options: RunOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "run", "starting command");
    let prompt = options
        .prompt
        .clone()
        .context("run requires PROMPT or --prompt TEXT")?;
    let workspace_root = runtime.resolve_workspace_root(options.workspace_root.as_deref())?;
    let tool_mode = options.tool_mode;
    let inference_options = inference_options_from_run_options(&options);
    let session = InferenceSession::new(inference_options, runtime, None)?;
    let mut loop_host = ChatLoopHost::new(session, &workspace_root)?;
    tracing::info!(
        target: "agentlibre::app",
        run_id = %loop_host.session().run_id(),
        event_stream = %loop_host.event_sink_path().display(),
        workspace_root = %loop_host.workspace_root().display(),
        tool_mode = tool_mode.as_str(),
        "runtime loop host initialized"
    );
    let hook_batches = loop_host.session().turn_hook_batches().to_vec();
    let visible_tools = loop_host.session().turn_visible_tools().to_vec();
    let input = build_turn_input(
        loop_host.session().run_id().as_str(),
        1,
        &[],
        &hook_batches,
        &visible_tools,
        &prompt,
    );
    loop_host.reset_turn_counters();
    let output = run_turn(&mut loop_host, input)?;
    print_turn_output(&output);
    Ok(())
}

fn run_serve(options: ServeOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "serve", "starting command");
    let mut daemon_options = DaemonOptions::new(
        &runtime.paths,
        inference_options_from_serve_options(&options),
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

fn print_repo_init_report(report: &RepoInitReport) {
    println!("state=initialized");
    println!("workspace_root={}", report.workspace_root.display());
    println!("manifest_path={}", report.manifest_path.display());
    println!("dry_run={}", report.dry_run);
    for change in &report.changes {
        println!(
            "change path={} action={}",
            change.path.display(),
            repo_init_action(change.action)
        );
    }
    for next_step in &report.next_steps {
        println!("next_step={next_step}");
    }
}

fn print_repo_status_report(report: &RepoStatusReport) {
    println!("state={}", repo_status_state(report.state));
    println!("workspace_root={}", report.workspace_root.display());
    println!("manifest_path={}", report.manifest_path.display());
    for component in &report.components {
        print_component_status(component);
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
    let name = skill.name.as_deref().unwrap_or("<invalid>");
    println!(
        "skill name={} path={} valid={} usable={} shadowed_by_builtin={} trust_state={:?}",
        name,
        skill.path.display(),
        skill.valid,
        skill.usable,
        skill.shadowed_by_builtin,
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
    for warning in &skill.warnings {
        println!("skill.{name}.warning={warning}");
    }
    for error in &skill.errors {
        println!("skill.{name}.error={error}");
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

fn print_store_status(status: &StoreStatus) {
    println!("store.path={}", status.database_path.display());
    println!("store.schema_version={}", status.schema_version);
    for domain in &status.domains {
        println!(
            "store.domain.{}={} total_rows={} active_rows={}",
            domain.domain.as_str(),
            domain.status.as_str(),
            domain.total_rows,
            domain.active_rows
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

fn print_memory_entries(entries: &[MemoryEntry]) {
    for entry in entries {
        print_memory_entry_summary(entry);
    }
}

fn print_memory_entry_summary(entry: &MemoryEntry) {
    println!(
        "memory id={} scope={} scope_key={} kind={} title={} deleted={}",
        entry.id,
        entry.scope.kind.as_str(),
        entry.scope.key,
        entry.kind.as_str(),
        entry.title,
        entry.deleted_at.is_some()
    );
}

fn print_memory_entry_detail(entry: &MemoryEntry) {
    print_memory_entry_summary(entry);
    println!("memory.{}.confidence={}", entry.id, entry.confidence);
    println!("memory.{}.created_at={}", entry.id, entry.created_at);
    println!("memory.{}.updated_at={}", entry.id, entry.updated_at);
    if let Some(source_ref) = &entry.source_ref {
        println!("memory.{}.source_ref={source_ref}", entry.id);
    }
    if let Some(deleted_at) = &entry.deleted_at {
        println!("memory.{}.deleted_at={deleted_at}", entry.id);
    }
    println!("memory.{}.body={}", entry.id, entry.body);
}

fn print_memory_suggestions(suggestions: &[MemorySuggestion]) {
    for suggestion in suggestions {
        print_memory_suggestion_summary(suggestion);
    }
}

fn print_memory_suggestion_summary(suggestion: &MemorySuggestion) {
    println!(
        "memory_suggestion id={} scope={} scope_key={} kind={} status={} title={}",
        suggestion.id,
        suggestion.scope.kind.as_str(),
        suggestion.scope.key,
        suggestion.kind.as_str(),
        suggestion.status.as_str(),
        suggestion.title
    );
    println!(
        "memory_suggestion.{}.source_ref={}",
        suggestion.id, suggestion.source_ref
    );
    if let Some(resolution_ref) = &suggestion.resolution_ref {
        println!(
            "memory_suggestion.{}.resolution_ref={resolution_ref}",
            suggestion.id
        );
    }
    if let Some(resolution_note) = &suggestion.resolution_note {
        println!(
            "memory_suggestion.{}.resolution_note={resolution_note}",
            suggestion.id
        );
    }
}

fn print_notes(notes: &[Note]) {
    for note in notes {
        print_note_summary(note);
    }
}

fn print_note_summary(note: &Note) {
    println!(
        "note id={} title={} deleted={}",
        note.id,
        note.title,
        note.deleted_at.is_some()
    );
}

fn print_note_detail(note: &Note, links: &[NoteLink]) {
    print_note_summary(note);
    println!("note.{}.created_at={}", note.id, note.created_at);
    println!("note.{}.updated_at={}", note.id, note.updated_at);
    if let Some(deleted_at) = &note.deleted_at {
        println!("note.{}.deleted_at={deleted_at}", note.id);
    }
    println!("note.{}.body={}", note.id, note.body);
    for link in links {
        print_note_link(link);
    }
}

fn print_note_link(link: &NoteLink) {
    println!(
        "note_link id={} note_id={} target_ref={}",
        link.id, link.note_id, link.target_ref
    );
    if let Some(label) = &link.label {
        println!("note_link.{}.label={label}", link.id);
    }
}

fn print_hook_install_report(report: &HookInstallReport) {
    println!(
        "state={}",
        if report.has_errors() {
            "conflict"
        } else {
            "ok"
        }
    );
    println!("workspace_root={}", report.workspace_root.display());
    println!("dry_run={}", report.dry_run);
    for hook in &report.hooks {
        println!(
            "hook name={} path={} action={:?}",
            hook.hook,
            hook.path.display(),
            hook.action
        );
    }
    for error in &report.errors {
        println!("error={error}");
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

fn repo_init_action(action: RepoInitAction) -> &'static str {
    match action {
        RepoInitAction::WouldCreateDir => "would_create_dir",
        RepoInitAction::CreatedDir => "created_dir",
        RepoInitAction::Exists => "exists",
        RepoInitAction::WouldWriteFile => "would_write_file",
        RepoInitAction::WroteFile => "wrote_file",
        RepoInitAction::WouldOverwriteFile => "would_overwrite_file",
        RepoInitAction::OverwroteFile => "overwrote_file",
        RepoInitAction::DeclaredSubmodule => "declared_submodule",
        RepoInitAction::DeclaredGitComponent => "declared_git_component",
    }
}

fn repo_status_state(state: agl_repo::RepoStatusState) -> &'static str {
    match state {
        agl_repo::RepoStatusState::Ok => "ok",
        agl_repo::RepoStatusState::Warning => "warning",
        agl_repo::RepoStatusState::Invalid => "invalid",
    }
}

fn skill_report_state(state: agl_skills::SkillReportState) -> &'static str {
    match state {
        agl_skills::SkillReportState::Ok => "ok",
        agl_skills::SkillReportState::Warning => "warning",
        agl_skills::SkillReportState::Invalid => "invalid",
    }
}

fn run_chat(mut options: RunOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "chat", "starting command");
    let run_id = options.run_id.clone().unwrap_or_else(default_run_id);
    options.run_id = Some(run_id.clone());
    let mut chat_service = ChatService::open(chat_options_from_run_options(&options), runtime)?;
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

fn print_turn_output(output: &TurnOutput) {
    match output {
        TurnOutput::Answered { answer } => println!("{}", assistant_text_for_terminal(answer)),
        TurnOutput::Stopped { reason } => println!("stopped=true reason={}", reason.as_str()),
    }
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
