use std::path::PathBuf;

use agl_extension::SkillId;
use anyhow::{Context, Result, bail};
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
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
    Infer(RunOptions),
    Chat(RunOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConfigCommand {
    Paths,
    Init { force: bool },
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
    pub(crate) skills: Vec<String>,
    pub(crate) prompt: Option<String>,
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
            skills: Vec::new(),
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
    /// Retired internal command name.
    #[command(hide = true, disable_help_flag = true)]
    Infer(ReservedCommandArgs),
    /// Run one prompt and print the final answer.
    Run(RunArgs),
    /// Alias for `run`.
    Generate(RunArgs),
    /// Start an interactive chat session.
    Chat(ChatArgs),
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

    /// Builtin skill id to inject for this turn/session.
    #[arg(long = "skill", value_name = "ID")]
    skills: Vec<String>,
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
            Some(Commands::Infer(args)) => retired_infer_command(args.args)?,
            Some(Commands::Run(args) | Commands::Generate(args)) => {
                CliCommand::Infer(run_options_from_args(args)?)
            }
            Some(Commands::Chat(args)) => CliCommand::Chat(chat_options_from_args(args)?),
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
        skills: validate_skill_ids(args.common.skills)?,
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
        skills: validate_skill_ids(args.common.skills)?,
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
    /// Run one prompt and print the final answer.
    Run(RunArgs),
    /// Alias for `run`.
    Generate(RunArgs),
    /// Start an interactive chat session.
    Chat(ChatArgs),
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
            "core:task-spec",
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
                skills: vec!["core:task-spec".to_string()],
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
