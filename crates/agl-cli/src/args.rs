use std::path::PathBuf;

use agl_tools::SkillId;
use anyhow::{Context, Result, bail};
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use clap_complete::{Shell, generate};

mod model;

pub(crate) use model::*;

const CLI_DISPLAY_NAME: &str = "agl";

#[derive(Debug, Parser)]
#[command(
    name = "agl",
    bin_name = "agl",
    version,
    about = "agentLIBRE CLI - local-first agentic inference"
)]
struct Cli {
    /// Override AGL_HOME for this invocation.
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,

    /// Prompt text for a one-shot run.
    #[arg(value_name = "PROMPT", num_args = 1.., trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Write shell completion scripts to stdout.
    Completion {
        /// Shell to generate completions for.
        #[arg(value_enum, default_value_t = Shell::Bash)]
        shell: Shell,
    },
    /// Runtime configuration commands.
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Inspect and export the local AgentLIBRE store.
    Store {
        #[command(subcommand)]
        command: StoreCommands,
    },
    /// Manage local scheduled AgentLIBRE jobs.
    Cron {
        #[command(subcommand)]
        command: CronCommands,
    },
    /// Manage local AgentLIBRE memory.
    Memory {
        #[command(subcommand)]
        command: MemoryCommands,
    },
    /// Manage local AgentLIBRE notes.
    Notes {
        #[command(subcommand)]
        command: NotesCommands,
    },
    /// Initialize the repo-local AgentLIBRE workspace.
    Init(RepoInitArgs),
    /// Retired internal command name.
    #[command(hide = true, disable_help_flag = true)]
    Infer(ReservedCommandArgs),
    /// Run one prompt and print the final answer.
    Run(RunArgs),
    /// Alias for `run`.
    Generate(RunArgs),
    /// Start an interactive chat session.
    Chat(ChatArgs),
    /// Run the local agent runtime daemon in the foreground.
    Serve(ServeArgs),
    /// Report repo-local AgentLIBRE workspace status.
    Status(RepoStatusArgs),
    /// Inspect and verify AgentLIBRE skills.
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },
    /// Install AgentLIBRE git hooks for this repository.
    InstallHooks(RepoHooksArgs),
    /// Advanced repo workspace commands.
    #[command(hide = true)]
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
    },
    /// Advanced daemon commands.
    #[command(hide = true)]
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },
    /// Planned public setup command.
    #[command(hide = true)]
    Setup(ReservedCommandArgs),
    /// Planned public diagnostics command.
    #[command(hide = true)]
    Doctor(ReservedCommandArgs),
    /// Planned public model lifecycle commands.
    #[command(hide = true)]
    Model(ReservedCommandArgs),
}

#[derive(Debug, Subcommand)]
enum ConfigCommands {
    /// Print resolved config, data, state, cache, log, and session paths.
    Paths,
    /// Write a default runtime config.
    Init {
        /// Overwrite an existing runtime config.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum StoreCommands {
    /// Report local store health.
    Status(StoreStatusArgs),
    /// Run local store migrations.
    Migrate(StoreMigrateArgs),
    /// Export one store domain as JSONL records.
    Export(StoreExportArgs),
}

#[derive(Debug, Subcommand)]
enum CronCommands {
    /// Add a scheduled job.
    Add(CronAddArgs),
    /// List scheduled jobs.
    List(CronListArgs),
    /// Show one scheduled job.
    Show(CronShowArgs),
    /// Enable a scheduled job.
    Enable(CronEnableArgs),
    /// Disable a scheduled job.
    Disable(CronDisableArgs),
    /// Run a scheduled job once.
    Run(CronRunArgs),
    /// Run one scheduler tick.
    #[command(hide = true)]
    Tick(CronTickArgs),
    /// Show run history for one scheduled job.
    History(CronHistoryArgs),
    /// Tombstone a scheduled job.
    Delete(CronDeleteArgs),
}

#[derive(Debug, Subcommand)]
enum RepoCommands {
    /// Initialize the repo-local AgentLIBRE workspace.
    Init(RepoInitArgs),
    /// Initialize a declared submodule component.
    InitComponent(RepoComponentInitArgs),
    /// Apply an explicit workspace profile file.
    #[command(hide = true)]
    ImportProfile(RepoImportProfileArgs),
    /// Report repo-local AgentLIBRE workspace status.
    Status(RepoStatusArgs),
    /// Verify task spec files in the tasks component.
    VerifyTasks(TaskSpecVerifyArgs),
    /// Install AgentLIBRE git hooks for this repository.
    InstallHooks(RepoHooksArgs),
    /// Export a portable workspace profile manifest.
    ExportProfile(RepoExportProfileArgs),
}

#[derive(Debug, Subcommand)]
enum MemoryCommands {
    /// Add an explicit memory entry.
    Add(MemoryAddArgs),
    /// List memory entries in one scope.
    List(MemoryListArgs),
    /// Search memory entries in one scope.
    Search(MemorySearchArgs),
    /// Show one memory entry.
    Show(MemoryShowArgs),
    /// Tombstone one memory entry.
    Delete(MemoryDeleteArgs),
    /// Create a pending memory suggestion.
    Suggest(MemorySuggestArgs),
    /// List memory suggestions.
    ListSuggestions(MemoryListSuggestionsArgs),
    /// Approve a pending memory suggestion.
    Approve(MemoryApproveArgs),
    /// Reject a pending memory suggestion.
    Reject(MemoryRejectArgs),
}

#[derive(Debug, Subcommand)]
enum NotesCommands {
    /// Add a note.
    Add(NotesAddArgs),
    /// List notes.
    List(NotesListArgs),
    /// Search notes.
    Search(NotesSearchArgs),
    /// Show one note.
    Show(NotesShowArgs),
    /// Update a note.
    Update(NotesUpdateArgs),
    /// Tombstone one note.
    Delete(NotesDeleteArgs),
    /// Link a note to another local reference.
    Link(NotesLinkArgs),
    /// Promote a note into memory.
    Remember(NotesRememberArgs),
}

#[derive(Debug, Subcommand)]
enum SkillCommands {
    /// Initialize the workspace skills submodule declared in .agl/workspace.toml.
    Init(SkillInitArgs),
    /// List builtin and workspace skills.
    List(SkillListArgs),
    /// Inspect one skill by name.
    Inspect(SkillInspectArgs),
    /// Report workspace skill component and lock status.
    Status(SkillStatusArgs),
    /// Verify workspace skills and lock state.
    Verify(SkillVerifyArgs),
    /// Write or preview .agl/skills.lock.
    Lock(SkillLockArgs),
    /// Locally approve a locked workspace skill identity.
    Trust(SkillTrustArgs),
    /// Revoke local approval for a workspace skill identity.
    Revoke(SkillRevokeArgs),
}

#[derive(Debug, Subcommand)]
enum DaemonCommands {
    /// Report local agent runtime daemon status.
    Status(StatusArgs),
}

#[derive(Debug, Args)]
struct RepoInitArgs {
    /// Repo workflow profile to initialize.
    #[arg(long, default_value = "repo-workflow")]
    profile: String,

    /// Local workspace profile manifest to apply.
    #[arg(long, value_name = "PATH")]
    profile_file: Option<PathBuf>,

    /// Skills repository URL for the .agl/skills submodule.
    #[arg(long, value_name = "URL")]
    skills_url: Option<String>,

    /// Skills repository revision to pin in the workspace manifest.
    #[arg(long, value_name = "REV")]
    skills_rev: Option<String>,

    /// Task/spec repository URL for the .agl/tasks submodule.
    #[arg(long, value_name = "URL")]
    tasks_url: Option<String>,

    /// Task/spec repository revision to pin in the workspace manifest.
    #[arg(long, value_name = "REV", requires = "tasks_url")]
    tasks_rev: Option<String>,

    /// Print planned changes without writing files.
    #[arg(long)]
    dry_run: bool,

    /// Repair or replace AgentLIBRE-managed files.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct RepoImportProfileArgs {
    /// Local workspace profile manifest to apply.
    #[arg(long, value_name = "PATH")]
    profile_file: PathBuf,

    /// Print planned changes without writing files.
    #[arg(long)]
    dry_run: bool,

    /// Repair or replace AgentLIBRE-managed files.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct RepoComponentInitArgs {
    /// Workspace component name to initialize.
    #[arg(value_name = "NAME")]
    component: String,

    /// Print planned git operations without writing.
    #[arg(long)]
    dry_run: bool,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RepoStatusArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Limit status to one component.
    #[arg(long, value_name = "NAME")]
    component: Option<String>,

    /// Treat warnings as failures.
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Args)]
struct TaskSpecVerifyArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Treat warnings as failures.
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Args)]
struct RepoHooksArgs {
    /// Print planned hook changes without writing files.
    #[arg(long)]
    dry_run: bool,

    /// Replace AgentLIBRE-managed hooks or overwrite conflicts.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct RepoExportProfileArgs {
    /// Destination workspace profile TOML path.
    #[arg(long, value_name = "PATH")]
    out: PathBuf,

    /// Overwrite an existing output file.
    #[arg(long)]
    force: bool,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct StoreStatusArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct StoreMigrateArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct StoreExportArgs {
    /// Domain to export. Domains may include multiple record_type values.
    #[arg(long, value_enum)]
    domain: StoreDomainArg,

    /// Destination JSONL path.
    #[arg(long, value_name = "PATH")]
    out: PathBuf,

    /// Include tombstoned records.
    #[arg(long)]
    include_deleted: bool,

    /// Overwrite an existing output file.
    #[arg(long)]
    force: bool,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronAddArgs {
    /// Job name.
    #[arg(long, value_name = "TEXT")]
    name: String,

    /// Schedule, such as hourly, daily HH:MM, weekly mon HH:MM, or a 5-field cron expression.
    #[arg(long, value_name = "EXPR")]
    schedule: String,

    /// Trusted skill id to run.
    #[arg(long, value_name = "ID", conflicts_with = "builtin")]
    skill: Option<String>,

    /// Builtin cron target id to run.
    #[arg(long, value_name = "ID", conflicts_with = "skill")]
    builtin: Option<String>,

    /// Create the job disabled.
    #[arg(long)]
    disabled: bool,

    /// Timezone label for human schedules.
    #[arg(long, value_name = "TZ")]
    timezone: Option<String>,

    /// Optional notification reference, such as matrix-room:<room_id>.
    #[arg(long, value_name = "REF")]
    notify: Option<String>,

    /// Stored prompt used when this cron job executes a skill.
    #[arg(long, value_name = "TEXT")]
    prompt: Option<String>,

    /// Optional stored input appended to the cron prompt.
    #[arg(long, value_name = "TEXT")]
    input: Option<String>,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronListArgs {
    /// Include tombstoned jobs.
    #[arg(long)]
    include_deleted: bool,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronShowArgs {
    /// Cron job id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronEnableArgs {
    /// Cron job id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronDisableArgs {
    /// Cron job id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronRunArgs {
    /// Cron job id.
    #[arg(value_name = "ID")]
    id: String,

    /// Run the job immediately.
    #[arg(long)]
    now: bool,

    /// Validate the job without recording or executing it.
    #[arg(long, conflicts_with = "now")]
    preflight: bool,

    /// Use deterministic mock execution for skill targets.
    #[arg(long, hide = true)]
    mock_skill_execution: bool,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronTickArgs {
    /// Unix timestamp used for due-job calculation. Defaults to current time.
    #[arg(long, value_name = "SECONDS")]
    at: Option<u64>,

    /// Use deterministic mock execution for skill targets.
    #[arg(long, hide = true)]
    mock_skill_execution: bool,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronHistoryArgs {
    /// Cron job id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronDeleteArgs {
    /// Cron job id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SkillListArgs {
    /// Skills to list.
    #[arg(long, value_enum, default_value_t = SkillListSourceArg::All)]
    source: SkillListSourceArg,

    /// Only list skills currently usable by the runtime trust policy.
    #[arg(long)]
    trusted_only: bool,

    /// Maximum number of skills to print.
    #[arg(long, value_name = "N")]
    limit: Option<usize>,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SkillInitArgs {
    /// Print planned git operations without writing.
    #[arg(long)]
    dry_run: bool,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemoryScopeArgs {
    /// Memory scope.
    #[arg(long, value_enum, default_value_t = MemoryScopeArg::User)]
    scope: MemoryScopeArg,

    /// Scope key. Defaults to `default` for user scope; required for repo, matrix-room, and matrix-user scopes.
    #[arg(long, value_name = "KEY")]
    scope_key: Option<String>,
}

#[derive(Debug, Args)]
struct MemoryAddArgs {
    #[command(flatten)]
    scope: MemoryScopeArgs,

    /// Memory kind.
    #[arg(long, value_enum, default_value_t = MemoryKindArg::Fact)]
    kind: MemoryKindArg,

    /// Memory title.
    #[arg(long, value_name = "TEXT")]
    title: String,

    /// Memory body.
    #[arg(long, value_name = "TEXT")]
    body: String,

    /// Optional source reference.
    #[arg(long, value_name = "REF")]
    source_ref: Option<String>,

    /// Confidence from 0 to 100.
    #[arg(long, value_name = "N", default_value_t = 100)]
    confidence: u8,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemoryListArgs {
    #[command(flatten)]
    scope: MemoryScopeArgs,

    /// Include tombstoned entries.
    #[arg(long)]
    include_deleted: bool,

    /// Maximum entries to print.
    #[arg(long, value_name = "N", default_value_t = 50)]
    limit: usize,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemorySearchArgs {
    #[command(flatten)]
    scope: MemoryScopeArgs,

    /// Include tombstoned entries.
    #[arg(long)]
    include_deleted: bool,

    /// Maximum entries to print.
    #[arg(long, value_name = "N", default_value_t = 50)]
    limit: usize,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Query text.
    #[arg(value_name = "QUERY")]
    query: String,
}

#[derive(Debug, Args)]
struct MemoryShowArgs {
    /// Memory entry id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemoryDeleteArgs {
    /// Memory entry id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemorySuggestArgs {
    #[command(flatten)]
    scope: MemoryScopeArgs,

    /// Suggested memory kind.
    #[arg(long, value_enum, default_value_t = MemoryKindArg::Fact)]
    kind: MemoryKindArg,

    /// Suggested memory title.
    #[arg(long, value_name = "TEXT")]
    title: String,

    /// Suggested memory body.
    #[arg(long, value_name = "TEXT")]
    body: String,

    /// Required source reference.
    #[arg(long, value_name = "REF")]
    source_ref: String,

    /// Confidence from 0 to 100.
    #[arg(long, value_name = "N", default_value_t = 100)]
    confidence: u8,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemoryListSuggestionsArgs {
    #[command(flatten)]
    scope: MemoryScopeArgs,

    /// Suggestion status to list. Defaults to pending.
    #[arg(long, value_enum)]
    status: Option<MemorySuggestionStatusArg>,

    /// List suggestions across every scope.
    #[arg(long)]
    all_scopes: bool,

    /// Maximum suggestions to print.
    #[arg(long, value_name = "N", default_value_t = 50)]
    limit: usize,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemoryApproveArgs {
    /// Memory suggestion id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemoryRejectArgs {
    /// Memory suggestion id.
    #[arg(value_name = "ID")]
    id: String,

    /// Optional rejection reason.
    #[arg(long, value_name = "TEXT")]
    reason: Option<String>,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct NotesAddArgs {
    /// Note title.
    #[arg(long, value_name = "TEXT")]
    title: String,

    /// Note body.
    #[arg(long, value_name = "TEXT")]
    body: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct NotesListArgs {
    /// Include tombstoned notes.
    #[arg(long)]
    include_deleted: bool,

    /// Maximum notes to print.
    #[arg(long, value_name = "N", default_value_t = 50)]
    limit: usize,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct NotesSearchArgs {
    /// Include tombstoned notes.
    #[arg(long)]
    include_deleted: bool,

    /// Maximum notes to print.
    #[arg(long, value_name = "N", default_value_t = 50)]
    limit: usize,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Query text.
    #[arg(value_name = "QUERY")]
    query: String,
}

#[derive(Debug, Args)]
struct NotesShowArgs {
    /// Note id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct NotesUpdateArgs {
    /// Note id.
    #[arg(value_name = "ID")]
    id: String,

    /// New note title.
    #[arg(long, value_name = "TEXT")]
    title: Option<String>,

    /// New note body.
    #[arg(long, value_name = "TEXT")]
    body: Option<String>,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct NotesDeleteArgs {
    /// Note id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct NotesLinkArgs {
    /// Note id.
    #[arg(value_name = "ID")]
    id: String,

    /// Target reference, such as memory:<id> or task:<id>.
    #[arg(long = "to", value_name = "REF")]
    target_ref: String,

    /// Link label.
    #[arg(long, value_name = "TEXT")]
    label: Option<String>,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct NotesRememberArgs {
    #[command(flatten)]
    scope: MemoryScopeArgs,

    /// Memory kind for the promoted note.
    #[arg(long, value_enum, default_value_t = MemoryKindArg::WorkingNote)]
    kind: MemoryKindArg,

    /// Note id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SkillInspectArgs {
    /// Skill name to inspect.
    #[arg(value_name = "NAME")]
    name: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Require the skill to be injectable by the runtime now.
    #[arg(long)]
    runtime: bool,
}

#[derive(Debug, Args)]
struct SkillStatusArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Treat warnings as failures.
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Args)]
struct SkillVerifyArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SkillLockArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Print planned lock changes without writing files.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct SkillTrustArgs {
    /// Workspace skill name to trust.
    #[arg(value_name = "NAME")]
    name: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Approve after reviewing the printed git identity.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct SkillRevokeArgs {
    /// Workspace skill name to revoke.
    #[arg(value_name = "NAME")]
    name: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CommonRunArgs {
    /// Local inference config TOML path.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Inference artifact root directory.
    #[arg(long, value_name = "DIR")]
    artifact_root: Option<PathBuf>,

    /// Stable run id for artifacts.
    #[arg(long, value_name = "ID")]
    run_id: Option<String>,

    /// Workspace root for filesystem tools.
    #[arg(long, value_name = "DIR")]
    workspace_root: Option<PathBuf>,

    /// Maximum response tokens.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_MAX_OUTPUT_TOKENS)]
    max_output_tokens: u32,

    /// Filesystem tool access mode.
    #[arg(long, value_enum, default_value_t = ToolAccessMode::ReadOnly)]
    tool_mode: ToolAccessMode,

    /// Builtin or trusted workspace skill id to inject for this turn/session.
    #[arg(long = "skill", value_name = "ID")]
    skills: Vec<String>,

    /// Inject explicit user memory into the model context.
    #[arg(long)]
    memory: bool,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[command(flatten)]
    common: CommonRunArgs,

    /// Prompt text.
    #[arg(long = "prompt", value_name = "TEXT", conflicts_with = "prompt")]
    prompt_option: Option<String>,

    /// Prompt text.
    #[arg(value_name = "PROMPT", num_args = 1.., trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[derive(Debug, Args)]
struct ChatArgs {
    #[command(flatten)]
    common: CommonRunArgs,

    /// Resume or write a specific chat session id.
    #[arg(long, value_name = "ID")]
    session_id: Option<String>,

    /// Start a new chat session even when a session id is configured.
    #[arg(long)]
    new_session: bool,

    /// Disable persisted chat history for this process.
    #[arg(long)]
    no_history: bool,
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[command(flatten)]
    common: CommonRunArgs,

    /// Unix socket path for the daemon.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct StatusArgs {
    /// Unix socket path for the daemon.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ReservedCommandArgs {
    #[arg(value_name = "ARGS", num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

pub(crate) fn parse_cli(args: impl IntoIterator<Item = String>) -> Result<CliInvocation> {
    let args = args.into_iter().collect::<Vec<_>>();
    let display_name = cli_display_name();
    let command = Cli::command().name(display_name).bin_name(display_name);

    match command.try_get_matches_from(args) {
        Ok(matches) => Cli::from_arg_matches(&matches)
            .map_err(anyhow::Error::from)
            .and_then(|cli| cli.into_invocation(display_name)),
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            err.print().context("failed to print CLI help")?;
            Ok(CliInvocation {
                command: CliCommand::HelpPrinted,
                home: None,
            })
        }
        Err(err) => Err(err.into()),
    }
}

impl Cli {
    fn into_invocation(self, display_name: &'static str) -> Result<CliInvocation> {
        let command = match self.command {
            Some(Commands::Completion { shell }) => CliCommand::Completion { shell },
            Some(Commands::Config { command }) => CliCommand::Config(match command {
                ConfigCommands::Paths => ConfigCommand::Paths,
                ConfigCommands::Init { force } => ConfigCommand::Init { force },
            }),
            Some(Commands::Store { command }) => CliCommand::Store(store_command(command)),
            Some(Commands::Cron { command }) => CliCommand::Cron(cron_command(command)?),
            Some(Commands::Memory { command }) => CliCommand::Memory(memory_command(command)?),
            Some(Commands::Notes { command }) => CliCommand::Notes(notes_command(command)?),
            Some(Commands::Init(args)) => {
                CliCommand::Repo(RepoCommand::Init(repo_init_options(args)))
            }
            Some(Commands::Infer(args)) => retired_infer_command(args.args)?,
            Some(Commands::Run(args) | Commands::Generate(args)) => {
                CliCommand::Infer(run_options_from_args(args)?)
            }
            Some(Commands::Chat(args)) => CliCommand::Chat(chat_options_from_args(args)?),
            Some(Commands::Serve(args)) => CliCommand::Serve(serve_options_from_args(args)?),
            Some(Commands::Status(args)) => {
                CliCommand::Repo(RepoCommand::Status(repo_status_options(args)))
            }
            Some(Commands::Skill { command }) => CliCommand::Skill(skill_command(command)),
            Some(Commands::InstallHooks(args)) => {
                CliCommand::Repo(RepoCommand::InstallHooks(repo_hooks_options(args)))
            }
            Some(Commands::Repo { command }) => CliCommand::Repo(match command {
                RepoCommands::Init(args) => RepoCommand::Init(repo_init_options(args)),
                RepoCommands::InitComponent(args) => {
                    RepoCommand::InitComponent(repo_component_init_options(args))
                }
                RepoCommands::ImportProfile(args) => {
                    RepoCommand::ImportProfile(repo_import_profile_options(args))
                }
                RepoCommands::Status(args) => RepoCommand::Status(repo_status_options(args)),
                RepoCommands::VerifyTasks(args) => {
                    RepoCommand::VerifyTasks(task_spec_verify_options(args))
                }
                RepoCommands::InstallHooks(args) => {
                    RepoCommand::InstallHooks(repo_hooks_options(args))
                }
                RepoCommands::ExportProfile(args) => {
                    RepoCommand::ExportProfile(repo_export_profile_options(args))
                }
            }),
            Some(Commands::Daemon { command }) => match command {
                DaemonCommands::Status(args) => CliCommand::DaemonStatus(DaemonStatusOptions {
                    socket_path: args.socket,
                }),
            },
            Some(Commands::Setup(args)) => unavailable_command("setup", args.args)?,
            Some(Commands::Doctor(args)) => unavailable_command("doctor", args.args)?,
            Some(Commands::Model(args)) => unavailable_command("model", args.args)?,
            None if self.prompt.is_empty() => CliCommand::Help {
                bin_name: display_name,
            },
            None => CliCommand::Infer(run_options_from_prompt(join_prompt(self.prompt))?),
        };

        Ok(CliInvocation {
            command,
            home: self.home,
        })
    }
}

fn repo_init_options(args: RepoInitArgs) -> RepoInitOptions {
    RepoInitOptions {
        profile: args.profile,
        profile_file: args.profile_file,
        skills_url: args.skills_url,
        skills_rev: args.skills_rev,
        tasks_url: args.tasks_url,
        tasks_rev: args.tasks_rev,
        dry_run: args.dry_run,
        force: args.force,
    }
}

fn repo_component_init_options(args: RepoComponentInitArgs) -> RepoComponentInitOptions {
    RepoComponentInitOptions {
        component: args.component,
        dry_run: args.dry_run,
        json: args.json,
    }
}

fn repo_status_options(args: RepoStatusArgs) -> RepoStatusOptions {
    RepoStatusOptions {
        json: args.json,
        component: args.component,
        strict: args.strict,
    }
}

fn task_spec_verify_options(args: TaskSpecVerifyArgs) -> TaskSpecVerifyOptions {
    TaskSpecVerifyOptions {
        json: args.json,
        strict: args.strict,
    }
}

fn repo_hooks_options(args: RepoHooksArgs) -> RepoHooksOptions {
    RepoHooksOptions {
        dry_run: args.dry_run,
        force: args.force,
    }
}

fn repo_export_profile_options(args: RepoExportProfileArgs) -> RepoExportProfileOptions {
    RepoExportProfileOptions {
        out: args.out,
        force: args.force,
        json: args.json,
    }
}

fn repo_import_profile_options(args: RepoImportProfileArgs) -> RepoImportProfileOptions {
    RepoImportProfileOptions {
        profile_file: args.profile_file,
        dry_run: args.dry_run,
        force: args.force,
    }
}

fn store_command(command: StoreCommands) -> StoreCommand {
    match command {
        StoreCommands::Status(args) => StoreCommand::Status(StoreStatusOptions { json: args.json }),
        StoreCommands::Migrate(args) => {
            StoreCommand::Migrate(StoreMigrateOptions { json: args.json })
        }
        StoreCommands::Export(args) => StoreCommand::Export(StoreExportCliOptions {
            domain: args.domain,
            out: args.out,
            include_deleted: args.include_deleted,
            force: args.force,
            json: args.json,
        }),
    }
}

fn cron_command(command: CronCommands) -> Result<CronCommand> {
    Ok(match command {
        CronCommands::Add(args) => {
            if let Some(prompt) = &args.prompt {
                validate_prompt(prompt)?;
            }
            if let Some(input) = &args.input {
                validate_prompt(input)?;
            }
            let target = cron_target(args.skill, args.builtin)?;
            if target.kind == CronTargetKindArg::Skill && args.prompt.is_none() {
                bail!("--prompt is required when --skill is used");
            }
            CronCommand::Add(CronAddOptions {
                name: args.name,
                schedule: args.schedule,
                target,
                enabled: !args.disabled,
                timezone: args.timezone,
                notify_ref: args.notify,
                prompt: args.prompt,
                input: args.input,
                json: args.json,
            })
        }
        CronCommands::List(args) => CronCommand::List(CronListOptions {
            include_deleted: args.include_deleted,
            json: args.json,
        }),
        CronCommands::Show(args) => {
            validate_prompt(&args.id)?;
            CronCommand::Show(CronShowOptions {
                id: args.id,
                json: args.json,
            })
        }
        CronCommands::Enable(args) => {
            validate_prompt(&args.id)?;
            CronCommand::Enable(CronEnableOptions {
                id: args.id,
                json: args.json,
            })
        }
        CronCommands::Disable(args) => {
            validate_prompt(&args.id)?;
            CronCommand::Disable(CronDisableOptions {
                id: args.id,
                json: args.json,
            })
        }
        CronCommands::Run(args) => {
            validate_prompt(&args.id)?;
            if !args.now && !args.preflight {
                bail!(
                    "agl cron run requires --now or --preflight until daemon scheduling is enabled"
                );
            }
            CronCommand::Run(CronRunOptions {
                id: args.id,
                now: args.now,
                preflight: args.preflight,
                mock_skill_execution: args.mock_skill_execution,
                json: args.json,
            })
        }
        CronCommands::Tick(args) => CronCommand::Tick(CronTickOptions {
            at: args.at,
            mock_skill_execution: args.mock_skill_execution,
            json: args.json,
        }),
        CronCommands::History(args) => {
            validate_prompt(&args.id)?;
            CronCommand::History(CronHistoryOptions {
                id: args.id,
                json: args.json,
            })
        }
        CronCommands::Delete(args) => {
            validate_prompt(&args.id)?;
            CronCommand::Delete(CronDeleteOptions {
                id: args.id,
                json: args.json,
            })
        }
    })
}

fn cron_target(skill: Option<String>, builtin: Option<String>) -> Result<CronTargetArg> {
    match (skill, builtin) {
        (Some(skill), None) => {
            if let Err(err) = SkillId::new(skill.clone()) {
                bail!("--skill is invalid: {err}");
            }
            Ok(CronTargetArg {
                kind: CronTargetKindArg::Skill,
                target_ref: skill,
            })
        }
        (None, Some(builtin)) => {
            validate_prompt(&builtin)?;
            Ok(CronTargetArg {
                kind: CronTargetKindArg::Builtin,
                target_ref: builtin,
            })
        }
        (None, None) => bail!("exactly one of --skill or --builtin is required"),
        (Some(_), Some(_)) => bail!("--skill and --builtin cannot be used together"),
    }
}

fn memory_command(command: MemoryCommands) -> Result<MemoryCommand> {
    Ok(match command {
        MemoryCommands::Add(args) => MemoryCommand::Add(MemoryAddOptions {
            scope: args.scope.scope,
            scope_key: args.scope.scope_key,
            kind: args.kind,
            title: args.title,
            body: args.body,
            source_ref: args.source_ref,
            confidence: validate_confidence(args.confidence)?,
            json: args.json,
        }),
        MemoryCommands::List(args) => MemoryCommand::List(MemoryListOptions {
            scope: args.scope.scope,
            scope_key: args.scope.scope_key,
            include_deleted: args.include_deleted,
            limit: validate_limit(args.limit, "--limit")?,
            json: args.json,
        }),
        MemoryCommands::Search(args) => {
            validate_prompt(&args.query)?;
            MemoryCommand::Search(MemorySearchOptions {
                query: args.query,
                scope: args.scope.scope,
                scope_key: args.scope.scope_key,
                include_deleted: args.include_deleted,
                limit: validate_limit(args.limit, "--limit")?,
                json: args.json,
            })
        }
        MemoryCommands::Show(args) => {
            validate_prompt(&args.id)?;
            MemoryCommand::Show(MemoryShowOptions {
                id: args.id,
                json: args.json,
            })
        }
        MemoryCommands::Delete(args) => {
            validate_prompt(&args.id)?;
            MemoryCommand::Delete(MemoryDeleteOptions {
                id: args.id,
                json: args.json,
            })
        }
        MemoryCommands::Suggest(args) => {
            validate_prompt(&args.source_ref)?;
            MemoryCommand::Suggest(MemorySuggestOptions {
                scope: args.scope.scope,
                scope_key: args.scope.scope_key,
                kind: args.kind,
                title: args.title,
                body: args.body,
                source_ref: args.source_ref,
                confidence: validate_confidence(args.confidence)?,
                json: args.json,
            })
        }
        MemoryCommands::ListSuggestions(args) => {
            MemoryCommand::ListSuggestions(MemoryListSuggestionsOptions {
                scope: args.scope.scope,
                scope_key: args.scope.scope_key,
                status: args.status,
                all_scopes: args.all_scopes,
                limit: validate_limit(args.limit, "--limit")?,
                json: args.json,
            })
        }
        MemoryCommands::Approve(args) => {
            validate_prompt(&args.id)?;
            MemoryCommand::Approve(MemoryApproveOptions {
                id: args.id,
                json: args.json,
            })
        }
        MemoryCommands::Reject(args) => {
            validate_prompt(&args.id)?;
            if let Some(reason) = &args.reason {
                validate_prompt(reason)?;
            }
            MemoryCommand::Reject(MemoryRejectOptions {
                id: args.id,
                reason: args.reason,
                json: args.json,
            })
        }
    })
}

fn notes_command(command: NotesCommands) -> Result<NotesCommand> {
    Ok(match command {
        NotesCommands::Add(args) => NotesCommand::Add(NotesAddOptions {
            title: args.title,
            body: args.body,
            json: args.json,
        }),
        NotesCommands::List(args) => NotesCommand::List(NotesListOptions {
            include_deleted: args.include_deleted,
            limit: validate_limit(args.limit, "--limit")?,
            json: args.json,
        }),
        NotesCommands::Search(args) => {
            validate_prompt(&args.query)?;
            NotesCommand::Search(NotesSearchOptions {
                query: args.query,
                include_deleted: args.include_deleted,
                limit: validate_limit(args.limit, "--limit")?,
                json: args.json,
            })
        }
        NotesCommands::Show(args) => {
            validate_prompt(&args.id)?;
            NotesCommand::Show(NotesShowOptions {
                id: args.id,
                json: args.json,
            })
        }
        NotesCommands::Update(args) => {
            validate_prompt(&args.id)?;
            NotesCommand::Update(NotesUpdateOptions {
                id: args.id,
                title: args.title,
                body: args.body,
                json: args.json,
            })
        }
        NotesCommands::Delete(args) => {
            validate_prompt(&args.id)?;
            NotesCommand::Delete(NotesDeleteOptions {
                id: args.id,
                json: args.json,
            })
        }
        NotesCommands::Link(args) => {
            validate_prompt(&args.id)?;
            validate_prompt(&args.target_ref)?;
            NotesCommand::Link(NotesLinkOptions {
                id: args.id,
                target_ref: args.target_ref,
                label: args.label,
                json: args.json,
            })
        }
        NotesCommands::Remember(args) => {
            validate_prompt(&args.id)?;
            NotesCommand::Remember(NotesRememberOptions {
                id: args.id,
                scope: args.scope.scope,
                scope_key: args.scope.scope_key,
                kind: args.kind,
                json: args.json,
            })
        }
    })
}

fn skill_command(command: SkillCommands) -> SkillCommand {
    match command {
        SkillCommands::Init(args) => SkillCommand::Init(SkillInitOptions {
            dry_run: args.dry_run,
            json: args.json,
        }),
        SkillCommands::List(args) => SkillCommand::List(SkillListOptions {
            json: args.json,
            source: args.source,
            trusted_only: args.trusted_only,
            limit: args.limit,
        }),
        SkillCommands::Inspect(args) => SkillCommand::Inspect(SkillInspectOptions {
            name: args.name,
            json: args.json,
            runtime: args.runtime,
        }),
        SkillCommands::Status(args) => SkillCommand::Status(SkillStatusOptions {
            json: args.json,
            strict: args.strict,
        }),
        SkillCommands::Verify(args) => SkillCommand::Verify(SkillVerifyOptions { json: args.json }),
        SkillCommands::Lock(args) => SkillCommand::Lock(SkillLockOptions {
            json: args.json,
            dry_run: args.dry_run,
        }),
        SkillCommands::Trust(args) => SkillCommand::Trust(SkillTrustOptions {
            name: args.name,
            json: args.json,
            yes: args.yes,
        }),
        SkillCommands::Revoke(args) => SkillCommand::Revoke(SkillRevokeOptions {
            name: args.name,
            json: args.json,
        }),
    }
}

fn run_options_from_args(args: RunArgs) -> Result<RunOptions> {
    let prompt = args.prompt_option.or_else(|| {
        if args.prompt.is_empty() {
            None
        } else {
            Some(join_prompt(args.prompt))
        }
    });
    if let Some(prompt) = &prompt {
        validate_prompt(prompt)?;
    }

    Ok(RunOptions {
        prompt,
        ..run_options_from_common(args.common)?
    })
}

fn run_options_from_prompt(prompt: String) -> Result<RunOptions> {
    validate_prompt(&prompt)?;
    Ok(RunOptions {
        prompt: Some(prompt),
        ..RunOptions::default()
    })
}

fn chat_options_from_args(args: ChatArgs) -> Result<RunOptions> {
    if args.new_session && args.session_id.is_some() {
        bail!("--new-session cannot be used with --session-id");
    }

    Ok(RunOptions {
        session_id: args.session_id,
        no_history: args.no_history,
        new_session: args.new_session,
        ..run_options_from_common(args.common)?
    })
}

fn serve_options_from_args(args: ServeArgs) -> Result<ServeOptions> {
    Ok(ServeOptions {
        socket_path: args.socket,
        config: args.common.config,
        artifact_root: args.common.artifact_root,
        run_id: args.common.run_id,
        workspace_root: args.common.workspace_root,
        max_output_tokens: validate_max_output_tokens(args.common.max_output_tokens)?,
        tool_mode: args.common.tool_mode,
        skills: validate_skill_ids(args.common.skills)?,
        memory: args.common.memory,
    })
}

fn run_options_from_common(common: CommonRunArgs) -> Result<RunOptions> {
    Ok(RunOptions {
        config: common.config,
        artifact_root: common.artifact_root,
        run_id: common.run_id,
        workspace_root: common.workspace_root,
        session_id: None,
        no_history: false,
        new_session: false,
        max_output_tokens: validate_max_output_tokens(common.max_output_tokens)?,
        tool_mode: common.tool_mode,
        skills: validate_skill_ids(common.skills)?,
        memory: common.memory,
        prompt: None,
    })
}

fn validate_prompt(prompt: &str) -> Result<()> {
    if prompt.trim().is_empty() {
        bail!("prompt cannot be empty");
    }
    Ok(())
}

fn validate_max_output_tokens(value: u32) -> Result<u32> {
    if value == 0 {
        bail!("--max-output-tokens must be greater than zero");
    }
    Ok(value)
}

fn validate_limit(value: usize, flag: &str) -> Result<usize> {
    if value == 0 {
        bail!("{flag} must be greater than zero");
    }
    Ok(value)
}

fn validate_confidence(value: u8) -> Result<u8> {
    if value > 100 {
        bail!("--confidence must be between 0 and 100");
    }
    Ok(value)
}

fn validate_skill_ids(values: Vec<String>) -> Result<Vec<String>> {
    let mut seen = std::collections::BTreeSet::new();
    for value in &values {
        if let Err(err) = SkillId::new(value.clone()) {
            bail!("--skill is invalid: {err}");
        }
        if !seen.insert(value) {
            bail!("--skill is duplicated: {value}");
        }
    }
    Ok(values)
}

fn retired_infer_command(args: Vec<String>) -> Result<CliCommand> {
    let attempted = if args.is_empty() {
        "infer".to_string()
    } else {
        format!("infer {}", args.join(" "))
    };
    bail!(
        "agl {attempted} is not part of the public CLI in this alpha. Use `agl run --config PATH PROMPT` instead."
    );
}

fn unavailable_command(name: &str, args: Vec<String>) -> Result<CliCommand> {
    let attempted = if args.is_empty() {
        name.to_string()
    } else {
        format!("{name} {}", args.join(" "))
    };
    bail!(
        "agl {attempted} is planned but not implemented in this alpha. Use `agl config paths` and `agl run --config PATH PROMPT` with a local GGUF config for now."
    );
}

fn join_prompt(parts: Vec<String>) -> String {
    parts.join(" ")
}

pub(crate) fn print_usage(bin_name: &'static str) -> Result<()> {
    let mut command = Cli::command().name(bin_name).bin_name(bin_name);
    command.print_help().context("failed to print CLI help")?;
    println!();
    Ok(())
}

pub(crate) fn print_completion(shell: Shell) {
    let mut command = PublicCompletionCli::command().name("agl").bin_name("agl");
    generate(shell, &mut command, "agl", &mut std::io::stdout());
}

fn cli_display_name() -> &'static str {
    CLI_DISPLAY_NAME
}

#[derive(Debug, Parser)]
#[command(
    name = "agl",
    bin_name = "agl",
    version,
    about = "agentLIBRE CLI - local-first agentic inference"
)]
struct PublicCompletionCli {
    /// Override AGL_HOME for this invocation.
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<PublicCompletionCommands>,

    /// Prompt text for a one-shot run.
    #[arg(value_name = "PROMPT", num_args = 1.., trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum PublicCompletionCommands {
    /// Write shell completion scripts to stdout.
    Completion {
        /// Shell to generate completions for.
        #[arg(value_enum, default_value_t = Shell::Bash)]
        shell: Shell,
    },
    /// Runtime configuration commands.
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Inspect and export the local AgentLIBRE store.
    Store {
        #[command(subcommand)]
        command: StoreCommands,
    },
    /// Manage local scheduled AgentLIBRE jobs.
    Cron {
        #[command(subcommand)]
        command: CronCommands,
    },
    /// Manage local AgentLIBRE memory.
    Memory {
        #[command(subcommand)]
        command: MemoryCommands,
    },
    /// Manage local AgentLIBRE notes.
    Notes {
        #[command(subcommand)]
        command: NotesCommands,
    },
    /// Initialize the repo-local AgentLIBRE workspace.
    Init(RepoInitArgs),
    /// Run one prompt and print the final answer.
    Run(RunArgs),
    /// Alias for `run`.
    Generate(RunArgs),
    /// Start an interactive chat session.
    Chat(ChatArgs),
    /// Run the local agent runtime daemon in the foreground.
    Serve(ServeArgs),
    /// Report repo-local AgentLIBRE workspace status.
    Status(RepoStatusArgs),
    /// Inspect and verify AgentLIBRE skills.
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },
    /// Install AgentLIBRE git hooks for this repository.
    InstallHooks(RepoHooksArgs),
}

#[cfg(test)]
mod tests;
