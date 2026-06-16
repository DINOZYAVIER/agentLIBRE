use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CliCommand {
    Help,
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
    pub(crate) session_id: Option<String>,
    pub(crate) no_history: bool,
    pub(crate) new_session: bool,
    pub(crate) max_output_tokens: u32,
    pub(crate) prompt: Option<String>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            config: None,
            artifact_root: None,
            run_id: None,
            session_id: None,
            no_history: false,
            new_session: false,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            prompt: None,
        }
    }
}

pub(crate) fn parse_cli(args: impl IntoIterator<Item = String>) -> Result<CliCommand> {
    let mut args = args.into_iter();
    let _program = args.next();
    let Some(command) = args.next() else {
        return Ok(CliCommand::Help);
    };

    match command.as_str() {
        "-h" | "--help" | "help" => Ok(CliCommand::Help),
        "config" => parse_config_command(args).map(CliCommand::Config),
        "infer" => parse_run_options(args, true).map(CliCommand::Infer),
        "chat" => parse_run_options(args, false).map(CliCommand::Chat),
        other => bail!("unknown command {other:?}"),
    }
}

fn parse_config_command(mut args: impl Iterator<Item = String>) -> Result<ConfigCommand> {
    let Some(command) = args.next() else {
        bail!("config requires a subcommand");
    };

    match command.as_str() {
        "paths" => {
            if let Some(arg) = args.next() {
                bail!("config paths does not accept {arg:?}");
            }
            Ok(ConfigCommand::Paths)
        }
        "init" => {
            let mut force = false;
            for arg in args {
                match arg.as_str() {
                    "--force" => force = true,
                    other => bail!("config init does not accept {other:?}"),
                }
            }
            Ok(ConfigCommand::Init { force })
        }
        other => bail!("unknown config subcommand {other:?}"),
    }
}

fn parse_run_options(
    mut args: impl Iterator<Item = String>,
    allow_prompt: bool,
) -> Result<RunOptions> {
    let mut options = RunOptions::default();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => bail!("command help is available with agentLIBRE --help"),
            "--config" => options.config = Some(PathBuf::from(next_value(&mut args, "--config")?)),
            "--artifact-root" => {
                options.artifact_root =
                    Some(PathBuf::from(next_value(&mut args, "--artifact-root")?));
            }
            "--run-id" => options.run_id = Some(next_value(&mut args, "--run-id")?),
            "--session-id" => options.session_id = Some(next_value(&mut args, "--session-id")?),
            "--new-session" => options.new_session = true,
            "--no-history" => options.no_history = true,
            "--max-output-tokens" => {
                let value = next_value(&mut args, "--max-output-tokens")?;
                options.max_output_tokens = value
                    .parse()
                    .with_context(|| format!("invalid --max-output-tokens value {value:?}"))?;
                if options.max_output_tokens == 0 {
                    bail!("--max-output-tokens must be greater than zero");
                }
            }
            "--prompt" if allow_prompt => options.prompt = Some(next_value(&mut args, "--prompt")?),
            "--prompt" => bail!("chat does not accept --prompt"),
            other => bail!("unknown option {other:?}"),
        }
    }

    if options.new_session && options.session_id.is_some() {
        bail!("--new-session cannot be used with --session-id");
    }
    if allow_prompt && (options.session_id.is_some() || options.no_history || options.new_session) {
        bail!("infer does not accept chat session options");
    }

    Ok(options)
}

fn next_value(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .with_context(|| format!("{name} requires a value"))
}

pub(crate) fn print_usage() {
    println!(
        "Usage:
  agentLIBRE config paths
  agentLIBRE config init [--force]
  agentLIBRE infer [--config PATH] [--artifact-root DIR] --prompt TEXT [--run-id ID] [--max-output-tokens N]
  agentLIBRE chat [--config PATH] [--artifact-root DIR] [--run-id ID] [--session-id ID] [--no-history] [--max-output-tokens N]

Environment defaults:
  AGL_LOCAL_INFERENCE_CONFIG
  AGL_INFERENCE_ARTIFACT_ROOT
  AGL_HOME

Chat commands:
  /exit
  /quit"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_infer_command() {
        let command = parse_cli([
            "agentLIBRE".to_string(),
            "infer".to_string(),
            "--config".to_string(),
            "local.toml".to_string(),
            "--artifact-root".to_string(),
            "artifacts".to_string(),
            "--prompt".to_string(),
            "hello".to_string(),
            "--run-id".to_string(),
            "manual-test".to_string(),
            "--max-output-tokens".to_string(),
            "32".to_string(),
        ])
        .unwrap();

        assert_eq!(
            command,
            CliCommand::Infer(RunOptions {
                config: Some(PathBuf::from("local.toml")),
                artifact_root: Some(PathBuf::from("artifacts")),
                run_id: Some("manual-test".to_string()),
                session_id: None,
                no_history: false,
                new_session: false,
                max_output_tokens: 32,
                prompt: Some("hello".to_string()),
            })
        );
    }

    #[test]
    fn parse_chat_session_options() {
        let command = parse_cli([
            "agentLIBRE".to_string(),
            "chat".to_string(),
            "--session-id".to_string(),
            "session-001".to_string(),
            "--no-history".to_string(),
        ])
        .unwrap();

        assert_eq!(
            command,
            CliCommand::Chat(RunOptions {
                session_id: Some("session-001".to_string()),
                no_history: true,
                ..RunOptions::default()
            })
        );
    }

    #[test]
    fn parse_chat_rejects_prompt() {
        let error = parse_cli([
            "agentLIBRE".to_string(),
            "chat".to_string(),
            "--prompt".to_string(),
            "hello".to_string(),
        ])
        .unwrap_err();

        assert!(error.to_string().contains("chat does not accept --prompt"));
    }

    #[test]
    fn parse_config_paths_command() {
        let command = parse_cli([
            "agentLIBRE".to_string(),
            "config".to_string(),
            "paths".to_string(),
        ])
        .unwrap();

        assert_eq!(command, CliCommand::Config(ConfigCommand::Paths));
    }

    #[test]
    fn parse_config_init_command() {
        let command = parse_cli([
            "agentLIBRE".to_string(),
            "config".to_string(),
            "init".to_string(),
        ])
        .unwrap();

        assert_eq!(
            command,
            CliCommand::Config(ConfigCommand::Init { force: false })
        );
    }

    #[test]
    fn parse_config_init_force_command() {
        let command = parse_cli([
            "agentLIBRE".to_string(),
            "config".to_string(),
            "init".to_string(),
            "--force".to_string(),
        ])
        .unwrap();

        assert_eq!(
            command,
            CliCommand::Config(ConfigCommand::Init { force: true })
        );
    }

    #[test]
    fn parse_config_paths_rejects_force() {
        let error = parse_cli([
            "agentLIBRE".to_string(),
            "config".to_string(),
            "paths".to_string(),
            "--force".to_string(),
        ])
        .unwrap_err();

        assert!(error.to_string().contains("config paths does not accept"));
    }
}
