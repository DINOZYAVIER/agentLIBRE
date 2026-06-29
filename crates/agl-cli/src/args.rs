use std::path::PathBuf;

use agl_tools::SkillId;
use anyhow::{Context, Result, bail};
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};

pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CliInvocation {
    pub(crate) command: CliCommand,
    pub(crate) home: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CliCommand {
    Help { bin_name: &'static str },
    HelpPrinted,
    Completion { shell: Shell },
    Config(ConfigCommand),
    Memory(MemoryCommand),
    Notes(NotesCommand),
    Repo(RepoCommand),
    Skill(SkillCommand),
    DaemonStatus(DaemonStatusOptions),
    Serve(ServeOptions),
    Infer(RunOptions),
    Chat(RunOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConfigCommand {
    Paths,
    Init { force: bool },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RepoCommand {
    Init(RepoInitOptions),
    Status(RepoStatusOptions),
    InstallHooks(RepoHooksOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MemoryCommand {
    Add(MemoryAddOptions),
    List(MemoryListOptions),
    Search(MemorySearchOptions),
    Show(MemoryShowOptions),
    Delete(MemoryDeleteOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NotesCommand {
    Add(NotesAddOptions),
    List(NotesListOptions),
    Search(NotesSearchOptions),
    Show(NotesShowOptions),
    Update(NotesUpdateOptions),
    Delete(NotesDeleteOptions),
    Link(NotesLinkOptions),
    Remember(NotesRememberOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SkillCommand {
    List(SkillListOptions),
    Inspect(SkillInspectOptions),
    Status(SkillStatusOptions),
    Verify(SkillVerifyOptions),
    Lock(SkillLockOptions),
    Trust(SkillTrustOptions),
    Revoke(SkillRevokeOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepoInitOptions {
    pub(crate) profile: String,
    pub(crate) dry_run: bool,
    pub(crate) force: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepoStatusOptions {
    pub(crate) json: bool,
    pub(crate) component: Option<String>,
    pub(crate) strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepoHooksOptions {
    pub(crate) dry_run: bool,
    pub(crate) force: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillListOptions {
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryAddOptions {
    pub(crate) scope: MemoryScopeArg,
    pub(crate) scope_key: Option<String>,
    pub(crate) kind: MemoryKindArg,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) source_ref: Option<String>,
    pub(crate) confidence: u8,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryListOptions {
    pub(crate) scope: MemoryScopeArg,
    pub(crate) scope_key: Option<String>,
    pub(crate) include_deleted: bool,
    pub(crate) limit: usize,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemorySearchOptions {
    pub(crate) query: String,
    pub(crate) scope: MemoryScopeArg,
    pub(crate) scope_key: Option<String>,
    pub(crate) include_deleted: bool,
    pub(crate) limit: usize,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryShowOptions {
    pub(crate) id: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryDeleteOptions {
    pub(crate) id: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesAddOptions {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesListOptions {
    pub(crate) include_deleted: bool,
    pub(crate) limit: usize,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesSearchOptions {
    pub(crate) query: String,
    pub(crate) include_deleted: bool,
    pub(crate) limit: usize,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesShowOptions {
    pub(crate) id: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesUpdateOptions {
    pub(crate) id: String,
    pub(crate) title: Option<String>,
    pub(crate) body: Option<String>,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesDeleteOptions {
    pub(crate) id: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesLinkOptions {
    pub(crate) id: String,
    pub(crate) target_ref: String,
    pub(crate) label: Option<String>,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesRememberOptions {
    pub(crate) id: String,
    pub(crate) scope: MemoryScopeArg,
    pub(crate) scope_key: Option<String>,
    pub(crate) kind: MemoryKindArg,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillInspectOptions {
    pub(crate) name: String,
    pub(crate) json: bool,
    pub(crate) runtime: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillStatusOptions {
    pub(crate) json: bool,
    pub(crate) strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillVerifyOptions {
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillLockOptions {
    pub(crate) json: bool,
    pub(crate) dry_run: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillTrustOptions {
    pub(crate) name: String,
    pub(crate) json: bool,
    pub(crate) yes: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillRevokeOptions {
    pub(crate) name: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunOptions {
    pub(crate) config: Option<PathBuf>,
    pub(crate) artifact_root: Option<PathBuf>,
    pub(crate) run_id: Option<String>,
    pub(crate) workspace_root: Option<PathBuf>,
    pub(crate) session_id: Option<String>,
    pub(crate) no_history: bool,
    pub(crate) new_session: bool,
    pub(crate) max_output_tokens: u32,
    pub(crate) tool_mode: ToolAccessMode,
    pub(crate) skills: Vec<String>,
    pub(crate) memory: bool,
    pub(crate) prompt: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServeOptions {
    pub(crate) socket_path: Option<PathBuf>,
    pub(crate) config: Option<PathBuf>,
    pub(crate) artifact_root: Option<PathBuf>,
    pub(crate) run_id: Option<String>,
    pub(crate) workspace_root: Option<PathBuf>,
    pub(crate) max_output_tokens: u32,
    pub(crate) tool_mode: ToolAccessMode,
    pub(crate) skills: Vec<String>,
    pub(crate) memory: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DaemonStatusOptions {
    pub(crate) socket_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub(crate) enum ToolAccessMode {
    ReadOnly,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub(crate) enum MemoryScopeArg {
    User,
    Repo,
    MatrixRoom,
    MatrixUser,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub(crate) enum MemoryKindArg {
    Fact,
    Preference,
    Summary,
    Decision,
    WorkingNote,
}

impl ToolAccessMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Write => "write",
        }
    }
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            config: None,
            artifact_root: None,
            run_id: None,
            workspace_root: None,
            session_id: None,
            no_history: false,
            new_session: false,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            tool_mode: ToolAccessMode::ReadOnly,
            skills: Vec::new(),
            memory: false,
            prompt: None,
        }
    }
}

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
enum RepoCommands {
    /// Initialize the repo-local AgentLIBRE workspace.
    Init(RepoInitArgs),
    /// Report repo-local AgentLIBRE workspace status.
    Status(RepoStatusArgs),
    /// Install AgentLIBRE git hooks for this repository.
    InstallHooks(RepoHooksArgs),
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

    /// Print planned changes without writing files.
    #[arg(long)]
    dry_run: bool,

    /// Repair or replace AgentLIBRE-managed files.
    #[arg(long)]
    force: bool,
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
struct RepoHooksArgs {
    /// Print planned hook changes without writing files.
    #[arg(long)]
    dry_run: bool,

    /// Replace AgentLIBRE-managed hooks or overwrite conflicts.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct SkillListArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemoryScopeArgs {
    /// Memory scope.
    #[arg(long, value_enum, default_value_t = MemoryScopeArg::User)]
    scope: MemoryScopeArg,

    /// Scope key. Required for repo, matrix-room, and matrix-user scopes.
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
    let display_name = cli_display_name(args.first().map(String::as_str));
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
                RepoCommands::Status(args) => RepoCommand::Status(repo_status_options(args)),
                RepoCommands::InstallHooks(args) => {
                    RepoCommand::InstallHooks(repo_hooks_options(args))
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
        dry_run: args.dry_run,
        force: args.force,
    }
}

fn repo_status_options(args: RepoStatusArgs) -> RepoStatusOptions {
    RepoStatusOptions {
        json: args.json,
        component: args.component,
        strict: args.strict,
    }
}

fn repo_hooks_options(args: RepoHooksArgs) -> RepoHooksOptions {
    RepoHooksOptions {
        dry_run: args.dry_run,
        force: args.force,
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
        SkillCommands::List(args) => SkillCommand::List(SkillListOptions { json: args.json }),
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

    let options = RunOptions {
        config: args.common.config,
        artifact_root: args.common.artifact_root,
        run_id: args.common.run_id,
        workspace_root: args.common.workspace_root,
        session_id: None,
        no_history: false,
        new_session: false,
        max_output_tokens: validate_max_output_tokens(args.common.max_output_tokens)?,
        tool_mode: args.common.tool_mode,
        skills: validate_skill_ids(args.common.skills)?,
        memory: args.common.memory,
        prompt,
    };
    Ok(options)
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
        config: args.common.config,
        artifact_root: args.common.artifact_root,
        run_id: args.common.run_id,
        workspace_root: args.common.workspace_root,
        session_id: args.session_id,
        no_history: args.no_history,
        new_session: args.new_session,
        max_output_tokens: validate_max_output_tokens(args.common.max_output_tokens)?,
        tool_mode: args.common.tool_mode,
        skills: validate_skill_ids(args.common.skills)?,
        memory: args.common.memory,
        prompt: None,
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

fn cli_display_name(program: Option<&str>) -> &'static str {
    let _ = program;
    "agl"
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
mod tests {
    use super::*;

    fn parse_command(args: impl IntoIterator<Item = &'static str>) -> CliCommand {
        parse_cli(args.into_iter().map(str::to_string))
            .unwrap()
            .command
    }

    #[test]
    fn parse_run_command_with_options() {
        let command = parse_command([
            "agl",
            "run",
            "--config",
            "local.toml",
            "--artifact-root",
            "artifacts",
            "--prompt",
            "hello",
            "--run-id",
            "manual-test",
            "--workspace-root",
            "/tmp/workspace",
            "--max-output-tokens",
            "32",
            "--skill",
            "task-spec",
            "--tool-mode",
            "write",
        ]);

        assert_eq!(
            command,
            CliCommand::Infer(RunOptions {
                config: Some(PathBuf::from("local.toml")),
                artifact_root: Some(PathBuf::from("artifacts")),
                run_id: Some("manual-test".to_string()),
                workspace_root: Some(PathBuf::from("/tmp/workspace")),
                session_id: None,
                no_history: false,
                new_session: false,
                max_output_tokens: 32,
                tool_mode: ToolAccessMode::Write,
                skills: vec!["task-spec".to_string()],
                memory: false,
                prompt: Some("hello".to_string()),
            })
        );
    }

    #[test]
    fn parse_run_rejects_invalid_skill_id() {
        let error = parse_cli([
            "agl".to_string(),
            "run".to_string(),
            "--skill".to_string(),
            "Bad Skill".to_string(),
            "--prompt".to_string(),
            "hello".to_string(),
        ])
        .unwrap_err();

        assert!(error.to_string().contains("--skill is invalid"));
    }

    #[test]
    fn parse_retired_infer_command_rejects_with_run_guidance() {
        let error = parse_cli([
            "agl".to_string(),
            "infer".to_string(),
            "--config".to_string(),
            "local.toml".to_string(),
            "--prompt".to_string(),
            "hello".to_string(),
        ])
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("agl infer"));
        assert!(message.contains("Use `agl run --config PATH PROMPT`"));
    }

    #[test]
    fn parse_run_prompt_argument() {
        let command = parse_command(["agl", "run", "hello", "world"]);

        assert_eq!(
            command,
            CliCommand::Infer(RunOptions {
                prompt: Some("hello world".to_string()),
                ..RunOptions::default()
            })
        );
    }

    #[test]
    fn parse_generate_alias() {
        let command = parse_command(["agl", "generate", "--prompt", "hello"]);

        assert_eq!(
            command,
            CliCommand::Infer(RunOptions {
                prompt: Some("hello".to_string()),
                ..RunOptions::default()
            })
        );
    }

    #[test]
    fn parse_run_command_with_memory_context() {
        let command = parse_command(["agl", "run", "--memory", "--prompt", "hello"]);

        assert_eq!(
            command,
            CliCommand::Infer(RunOptions {
                memory: true,
                prompt: Some("hello".to_string()),
                ..RunOptions::default()
            })
        );
    }

    #[test]
    fn parse_serve_command_with_daemon_options() {
        let command = parse_command([
            "agl",
            "serve",
            "--socket",
            "/tmp/agl.sock",
            "--config",
            "local.toml",
            "--artifact-root",
            "artifacts",
            "--workspace-root",
            "/tmp/workspace",
            "--max-output-tokens",
            "33",
            "--tool-mode",
            "write",
            "--skill",
            "tool-smoke",
        ]);

        assert_eq!(
            command,
            CliCommand::Serve(ServeOptions {
                socket_path: Some(PathBuf::from("/tmp/agl.sock")),
                config: Some(PathBuf::from("local.toml")),
                artifact_root: Some(PathBuf::from("artifacts")),
                run_id: None,
                workspace_root: Some(PathBuf::from("/tmp/workspace")),
                max_output_tokens: 33,
                tool_mode: ToolAccessMode::Write,
                skills: vec!["tool-smoke".to_string()],
                memory: false,
            })
        );
    }

    #[test]
    fn parse_init_command() {
        let command = parse_command(["agl", "init", "--dry-run"]);

        assert_eq!(
            command,
            CliCommand::Repo(RepoCommand::Init(RepoInitOptions {
                profile: "repo-workflow".to_string(),
                dry_run: true,
                force: false,
            }))
        );
    }

    #[test]
    fn parse_repo_init_hidden_alias() {
        let command = parse_command(["agl", "repo", "init", "--force"]);

        assert_eq!(
            command,
            CliCommand::Repo(RepoCommand::Init(RepoInitOptions {
                profile: "repo-workflow".to_string(),
                dry_run: false,
                force: true,
            }))
        );
    }

    #[test]
    fn parse_status_command_with_repo_options() {
        let command = parse_command([
            "agl",
            "status",
            "--json",
            "--component",
            "skills",
            "--strict",
        ]);

        assert_eq!(
            command,
            CliCommand::Repo(RepoCommand::Status(RepoStatusOptions {
                json: true,
                component: Some("skills".to_string()),
                strict: true,
            }))
        );
    }

    #[test]
    fn parse_repo_status_hidden_alias() {
        let command = parse_command(["agl", "repo", "status", "--json"]);

        assert_eq!(
            command,
            CliCommand::Repo(RepoCommand::Status(RepoStatusOptions {
                json: true,
                component: None,
                strict: false,
            }))
        );
    }

    #[test]
    fn parse_install_hooks_command() {
        let command = parse_command(["agl", "install-hooks", "--dry-run"]);

        assert_eq!(
            command,
            CliCommand::Repo(RepoCommand::InstallHooks(RepoHooksOptions {
                dry_run: true,
                force: false,
            }))
        );
    }

    #[test]
    fn parse_skill_commands() {
        assert_eq!(
            parse_command(["agl", "skill", "list", "--json"]),
            CliCommand::Skill(SkillCommand::List(SkillListOptions { json: true }))
        );
        assert_eq!(
            parse_command(["agl", "skill", "inspect", "repo-change", "--json"]),
            CliCommand::Skill(SkillCommand::Inspect(SkillInspectOptions {
                name: "repo-change".to_string(),
                json: true,
                runtime: false,
            }))
        );
        assert_eq!(
            parse_command(["agl", "skill", "inspect", "repo-change", "--runtime"]),
            CliCommand::Skill(SkillCommand::Inspect(SkillInspectOptions {
                name: "repo-change".to_string(),
                json: false,
                runtime: true,
            }))
        );
        assert_eq!(
            parse_command(["agl", "skill", "status", "--strict"]),
            CliCommand::Skill(SkillCommand::Status(SkillStatusOptions {
                json: false,
                strict: true,
            }))
        );
        assert_eq!(
            parse_command(["agl", "skill", "verify", "--json"]),
            CliCommand::Skill(SkillCommand::Verify(SkillVerifyOptions { json: true }))
        );
        assert_eq!(
            parse_command(["agl", "skill", "lock", "--dry-run"]),
            CliCommand::Skill(SkillCommand::Lock(SkillLockOptions {
                json: false,
                dry_run: true,
            }))
        );
        assert_eq!(
            parse_command(["agl", "skill", "trust", "repo-change", "--yes"]),
            CliCommand::Skill(SkillCommand::Trust(SkillTrustOptions {
                name: "repo-change".to_string(),
                json: false,
                yes: true,
            }))
        );
        assert_eq!(
            parse_command(["agl", "skill", "revoke", "repo-change", "--json"]),
            CliCommand::Skill(SkillCommand::Revoke(SkillRevokeOptions {
                name: "repo-change".to_string(),
                json: true,
            }))
        );
    }

    #[test]
    fn parse_memory_commands() {
        assert_eq!(
            parse_command([
                "agl",
                "memory",
                "add",
                "--scope",
                "repo",
                "--scope-key",
                "/tmp/repo",
                "--kind",
                "decision",
                "--title",
                "Trust",
                "--body",
                "Use local approval.",
                "--source-ref",
                "manual",
                "--confidence",
                "90",
                "--json",
            ]),
            CliCommand::Memory(MemoryCommand::Add(MemoryAddOptions {
                scope: MemoryScopeArg::Repo,
                scope_key: Some("/tmp/repo".to_string()),
                kind: MemoryKindArg::Decision,
                title: "Trust".to_string(),
                body: "Use local approval.".to_string(),
                source_ref: Some("manual".to_string()),
                confidence: 90,
                json: true,
            }))
        );
        assert_eq!(
            parse_command([
                "agl", "memory", "search", "--scope", "user", "--limit", "10", "approval",
            ]),
            CliCommand::Memory(MemoryCommand::Search(MemorySearchOptions {
                query: "approval".to_string(),
                scope: MemoryScopeArg::User,
                scope_key: None,
                include_deleted: false,
                limit: 10,
                json: false,
            }))
        );
        assert_eq!(
            parse_command(["agl", "memory", "delete", "mem_1"]),
            CliCommand::Memory(MemoryCommand::Delete(MemoryDeleteOptions {
                id: "mem_1".to_string(),
                json: false,
            }))
        );
    }

    #[test]
    fn parse_notes_commands() {
        assert_eq!(
            parse_command([
                "agl",
                "notes",
                "add",
                "--title",
                "Workflow",
                "--body",
                "Use pinned skills.",
                "--json",
            ]),
            CliCommand::Notes(NotesCommand::Add(NotesAddOptions {
                title: "Workflow".to_string(),
                body: "Use pinned skills.".to_string(),
                json: true,
            }))
        );
        assert_eq!(
            parse_command([
                "agl",
                "notes",
                "remember",
                "note_1",
                "--scope",
                "repo",
                "--scope-key",
                "/tmp/repo",
                "--kind",
                "decision",
            ]),
            CliCommand::Notes(NotesCommand::Remember(NotesRememberOptions {
                id: "note_1".to_string(),
                scope: MemoryScopeArg::Repo,
                scope_key: Some("/tmp/repo".to_string()),
                kind: MemoryKindArg::Decision,
                json: false,
            }))
        );
        assert_eq!(
            parse_command([
                "agl",
                "notes",
                "link",
                "note_1",
                "--to",
                "task:AGL-084",
                "--label",
                "spec",
            ]),
            CliCommand::Notes(NotesCommand::Link(NotesLinkOptions {
                id: "note_1".to_string(),
                target_ref: "task:AGL-084".to_string(),
                label: Some("spec".to_string()),
                json: false,
            }))
        );
    }

    #[test]
    fn parse_memory_rejects_zero_limit() {
        let error = parse_cli([
            "agl".to_string(),
            "memory".to_string(),
            "list".to_string(),
            "--limit".to_string(),
            "0".to_string(),
        ])
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("--limit must be greater than zero")
        );
    }

    #[test]
    fn parse_daemon_status_command_with_socket_override() {
        let command = parse_command(["agl", "daemon", "status", "--socket", "/tmp/agl.sock"]);

        assert_eq!(
            command,
            CliCommand::DaemonStatus(DaemonStatusOptions {
                socket_path: Some(PathBuf::from("/tmp/agl.sock")),
            })
        );
    }

    #[test]
    fn parse_bare_prompt_as_run() {
        let command = parse_command(["agl", "hello"]);

        assert_eq!(
            command,
            CliCommand::Infer(RunOptions {
                prompt: Some("hello".to_string()),
                ..RunOptions::default()
            })
        );
    }

    #[test]
    fn parse_rejects_blank_bare_prompt() {
        let error = parse_cli(["agl".to_string(), "   ".to_string()]).unwrap_err();

        assert!(error.to_string().contains("prompt cannot be empty"));
    }

    #[test]
    fn parse_home_override() {
        let invocation = parse_cli([
            "agl".to_string(),
            "--home".to_string(),
            "/tmp/agl-home".to_string(),
            "config".to_string(),
            "paths".to_string(),
        ])
        .unwrap();

        assert_eq!(invocation.home, Some(PathBuf::from("/tmp/agl-home")));
        assert_eq!(invocation.command, CliCommand::Config(ConfigCommand::Paths));
    }

    #[test]
    fn parse_chat_session_options() {
        let command = parse_command([
            "agl",
            "chat",
            "--session-id",
            "session-001",
            "--no-history",
            "--workspace-root",
            "/tmp/workspace",
        ]);

        assert_eq!(
            command,
            CliCommand::Chat(RunOptions {
                session_id: Some("session-001".to_string()),
                no_history: true,
                workspace_root: Some(PathBuf::from("/tmp/workspace")),
                ..RunOptions::default()
            })
        );
    }

    #[test]
    fn parse_chat_rejects_new_session_with_session_id() {
        let error = parse_cli([
            "agl".to_string(),
            "chat".to_string(),
            "--new-session".to_string(),
            "--session-id".to_string(),
            "session-001".to_string(),
        ])
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("--new-session cannot be used with --session-id")
        );
    }

    #[test]
    fn parse_chat_rejects_prompt() {
        let error = parse_cli([
            "agl".to_string(),
            "chat".to_string(),
            "--prompt".to_string(),
            "hello".to_string(),
        ])
        .unwrap_err();

        assert!(error.to_string().contains("unexpected argument"));
    }

    #[test]
    fn parse_config_paths_command() {
        let command = parse_command(["agl", "config", "paths"]);

        assert_eq!(command, CliCommand::Config(ConfigCommand::Paths));
    }

    #[test]
    fn parse_config_init_command() {
        let command = parse_command(["agl", "config", "init"]);

        assert_eq!(
            command,
            CliCommand::Config(ConfigCommand::Init { force: false })
        );
    }

    #[test]
    fn parse_config_init_force_command() {
        let command = parse_command(["agl", "config", "init", "--force"]);

        assert_eq!(
            command,
            CliCommand::Config(ConfigCommand::Init { force: true })
        );
    }

    #[test]
    fn parse_config_paths_rejects_force() {
        let error = parse_cli([
            "agl".to_string(),
            "config".to_string(),
            "paths".to_string(),
            "--force".to_string(),
        ])
        .unwrap_err();

        assert!(error.to_string().contains("unexpected argument"));
    }

    #[test]
    fn parse_completion_command() {
        let command = parse_command(["agl", "completion", "bash"]);

        assert_eq!(command, CliCommand::Completion { shell: Shell::Bash });
    }

    #[test]
    fn parse_reserved_setup_rejects_before_bare_prompt() {
        let error = parse_cli(["agl".to_string(), "setup".to_string()]).unwrap_err();

        assert!(error.to_string().contains("planned but not implemented"));
    }

    #[test]
    fn parse_reserved_doctor_rejects_before_bare_prompt() {
        let error = parse_cli(["agl".to_string(), "doctor".to_string()]).unwrap_err();

        assert!(error.to_string().contains("planned but not implemented"));
    }

    #[test]
    fn parse_reserved_model_rejects_subcommand_before_bare_prompt() {
        let error = parse_cli([
            "agl".to_string(),
            "model".to_string(),
            "pull".to_string(),
            "owner/repo/model.gguf".to_string(),
            "--set-default".to_string(),
        ])
        .unwrap_err();

        assert!(error.to_string().contains("agl model pull"));
        assert!(error.to_string().contains("planned but not implemented"));
    }

    #[test]
    fn display_name_prefers_agl_alias() {
        assert_eq!(cli_display_name(Some("agl")), "agl");
        assert_eq!(cli_display_name(Some("/usr/local/bin/agl")), "agl");
        assert_eq!(cli_display_name(Some("agentLIBRE")), "agl");
        assert_eq!(cli_display_name(Some("/usr/local/bin/agentLIBRE")), "agl");
        assert_eq!(cli_display_name(None), "agl");
    }
}
