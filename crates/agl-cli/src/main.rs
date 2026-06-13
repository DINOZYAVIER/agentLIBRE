use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use agl_config::{ModelConfig, load_local_inference_config};
use agl_inference::evidence::{InferenceArtifactRoot, InferenceAttemptId, InferenceRunId};
use agl_inference::{InferenceBackend, InferenceRequest, InferenceResponse, LlamaCppCliBackend};
use agl_oven::render_model_request;
use agl_turn::{ModelRequest, TurnMessage};
use anyhow::{Context, Result, bail};

const CONFIG_ENV: &str = "AGL_LOCAL_INFERENCE_CONFIG";
const ARTIFACT_ROOT_ENV: &str = "AGL_INFERENCE_ARTIFACT_ROOT";
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 256;

fn main() {
    if let Err(err) = run(env::args()) {
        eprintln!("error: {err:#}");
        process::exit(1);
    }
}

fn run(args: impl IntoIterator<Item = String>) -> Result<()> {
    match parse_cli(args)? {
        CliCommand::Help => {
            print_usage();
            Ok(())
        }
        CliCommand::Infer(options) => run_infer(options),
        CliCommand::Chat(options) => run_chat(options),
    }
}

fn run_infer(options: RunOptions) -> Result<()> {
    let prompt = options
        .prompt
        .clone()
        .context("infer requires --prompt TEXT")?;
    let mut session = InferenceSession::new(options)?;
    let messages = vec![TurnMessage::User { content: prompt }];
    let response = session.generate(&messages, 1)?;
    println!("{}", assistant_text_for_terminal(&response.content));
    Ok(())
}

fn run_chat(options: RunOptions) -> Result<()> {
    let mut session = InferenceSession::new(options)?;
    let mut messages = Vec::new();
    let stdin = io::stdin();
    let mut request_index = 1;

    loop {
        print!("agentLIBRE> ");
        io::stdout().flush().context("failed to flush prompt")?;

        let mut input = String::new();
        let bytes_read = stdin
            .read_line(&mut input)
            .context("failed to read chat input")?;
        if bytes_read == 0 {
            break;
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if matches!(input, "/exit" | "/quit") {
            break;
        }

        messages.push(TurnMessage::User {
            content: input.to_string(),
        });
        let response = session.generate(&messages, request_index)?;
        let content = assistant_text_for_terminal(&response.content);
        println!("assistant> {content}");
        messages.push(TurnMessage::Assistant { content });
        request_index += 1;
    }

    Ok(())
}

struct InferenceSession {
    backend: LlamaCppCliBackend,
    model_config: ModelConfig,
    run_id: InferenceRunId,
}

impl InferenceSession {
    fn new(options: RunOptions) -> Result<Self> {
        let config_path = options
            .config
            .or_else(|| env::var_os(CONFIG_ENV).map(PathBuf::from))
            .context("missing --config PATH or AGL_LOCAL_INFERENCE_CONFIG")?;
        let artifact_root = options
            .artifact_root
            .or_else(|| env::var_os(ARTIFACT_ROOT_ENV).map(PathBuf::from))
            .context("missing --artifact-root DIR or AGL_INFERENCE_ARTIFACT_ROOT")?;

        let config = load_local_inference_config(&config_path).with_context(|| {
            format!(
                "failed to load local inference config {}",
                config_path.display()
            )
        })?;
        let model_config = config.model.clone();
        let backend = LlamaCppCliBackend::new(config, InferenceArtifactRoot::new(artifact_root))?
            .with_max_output_tokens(options.max_output_tokens);
        let run_id = InferenceRunId::new(options.run_id.unwrap_or_else(default_run_id))?;

        Ok(Self {
            backend,
            model_config,
            run_id,
        })
    }

    fn generate(
        &mut self,
        messages: &[TurnMessage],
        request_index: usize,
    ) -> Result<InferenceResponse> {
        let request = build_inference_request(
            self.run_id.clone(),
            request_index,
            messages.to_vec(),
            &self.model_config,
        )?;
        self.backend.generate(request)
    }
}

fn build_inference_request(
    run_id: InferenceRunId,
    request_index: usize,
    messages: Vec<TurnMessage>,
    model_config: &ModelConfig,
) -> Result<InferenceRequest> {
    let model_request = ModelRequest {
        turn_id: run_id.to_string(),
        request_index,
        messages,
        visible_tools: Vec::new(),
    };
    let rendered = render_model_request(&model_request, model_config)?;
    Ok(InferenceRequest {
        run_id,
        attempt_id: InferenceAttemptId::new(format!("attempt-{request_index:04}"))?,
        rendered,
    })
}

fn default_run_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("manual-{millis}")
}

fn assistant_text_for_terminal(content: &str) -> String {
    let mut content = content.trim();
    if let Some(stripped) = content.strip_prefix("Assistant:") {
        content = stripped.trim_start();
    }

    let marker_offset = ["\nUser:", "\nAssistant:", "\nTool:"]
        .iter()
        .filter_map(|marker| content.find(marker))
        .min()
        .unwrap_or(content.len());

    content[..marker_offset].trim().to_string()
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CliCommand {
    Help,
    Infer(RunOptions),
    Chat(RunOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RunOptions {
    config: Option<PathBuf>,
    artifact_root: Option<PathBuf>,
    run_id: Option<String>,
    max_output_tokens: u32,
    prompt: Option<String>,
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

fn parse_cli(args: impl IntoIterator<Item = String>) -> Result<CliCommand> {
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

fn print_usage() {
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
    use agl_config::{ModelDialect, ToolCallFormat};

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

    #[test]
    fn build_request_uses_agentlibre_boundaries() {
        let run_id = InferenceRunId::new("manual-test").unwrap();
        let config = ModelConfig {
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
        };

        let request = build_inference_request(
            run_id.clone(),
            7,
            vec![TurnMessage::User {
                content: "hello".to_string(),
            }],
            &config,
        )
        .unwrap();

        assert_eq!(request.run_id, run_id);
        assert_eq!(request.attempt_id.as_str(), "attempt-0007");
        assert_eq!(request.rendered.turn_id, "manual-test");
        assert_eq!(request.rendered.request_index, 7);
        assert_eq!(request.rendered.messages.len(), 1);
        assert_eq!(request.rendered.dialect, ModelDialect::Qwen3);
        assert_eq!(
            request.rendered.tool_call_format,
            ToolCallFormat::HermesJson
        );
    }

    #[test]
    fn terminal_text_cuts_generated_next_turn() {
        let content = "agentLIBRE ok\n\nUser:\nnew prompt\n\nAssistant:\nnext";

        assert_eq!(assistant_text_for_terminal(content), "agentLIBRE ok");
    }

    #[test]
    fn terminal_text_strips_leading_assistant_label() {
        assert_eq!(
            assistant_text_for_terminal("Assistant:\nhello\n"),
            "hello".to_string()
        );
    }
}
