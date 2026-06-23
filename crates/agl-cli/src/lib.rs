use std::env;
use std::io::{self, Write};
use std::process;

use agl_chat::{
    ChatLoopHost, ChatOptions, ChatService, ChatTurnStatus, InferenceOptions, InferenceSession,
    ToolAccessMode as ChatToolAccessMode, assistant_text_for_terminal, build_turn_input,
    chat_workspace_root, default_run_id,
};
use agl_loop::{TurnOutput, run_turn};
use agl_runtime::{
    AgentLibreHistoryConfig, AgentLibreLoggingConfig, AgentLibrePaths, AgentLibreProcessMode,
    AgentLibreRuntimeConfig, AgentLibreWorkspaceConfig, init_tracing,
};
use anyhow::{Context, Result};

mod args;
mod chat;
mod config;

use args::{CliCommand, RunOptions, parse_cli, print_completion, print_usage};
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
    if matches!(command, CliCommand::Config(_)) {
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
        CliCommand::Help { .. }
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
        CliCommand::Infer(options) => run_infer(options, runtime),
        CliCommand::Chat(options) => run_chat(options, runtime),
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

fn inference_options_from_run_options(options: &RunOptions) -> InferenceOptions {
    InferenceOptions {
        config: options.config.clone(),
        artifact_root: options.artifact_root.clone(),
        run_id: options.run_id.clone(),
        max_output_tokens: options.max_output_tokens,
        tool_mode: chat_tool_mode(options.tool_mode),
        skills: options.skills.clone(),
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
}
