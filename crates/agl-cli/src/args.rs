use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CliCommand {
    Help,
    Infer(RunOptions),
    Chat(RunOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunOptions {
    pub(crate) config: Option<PathBuf>,
    pub(crate) artifact_root: Option<PathBuf>,
    pub(crate) run_id: Option<String>,
    pub(crate) max_output_tokens: u32,
    pub(crate) prompt: Option<String>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            config: None,
            artifact_root: None,
            run_id: None,
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
        "infer" => parse_run_options(args, true).map(CliCommand::Infer),
        "chat" => parse_run_options(args, false).map(CliCommand::Chat),
        other => bail!("unknown command {other:?}"),
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

    Ok(options)
}

fn next_value(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .with_context(|| format!("{name} requires a value"))
}

pub(crate) fn print_usage() {
    println!(
        "Usage:
  agentLIBRE infer --config PATH --artifact-root DIR --prompt TEXT [--run-id ID] [--max-output-tokens N]
  agentLIBRE chat --config PATH --artifact-root DIR [--run-id ID] [--max-output-tokens N]

Environment defaults:
  AGL_LOCAL_INFERENCE_CONFIG
  AGL_INFERENCE_ARTIFACT_ROOT

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
                max_output_tokens: 32,
                prompt: Some("hello".to_string()),
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
}
