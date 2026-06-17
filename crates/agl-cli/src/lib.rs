use std::env;
use std::io::{self, Write};
use std::process;

use agl_runtime::{
    AgentLibreHistoryConfig, AgentLibreLoggingConfig, AgentLibreMessageId, AgentLibrePaths,
    AgentLibreProcessMode, AgentLibreRuntimeConfig, AgentLibreSessionId, ChatSessionEvent,
    ChatSessionReplay, ChatSessionStore, init_tracing, logged_message_fields,
};
use agl_turn::TurnMessage;
use anyhow::{Context, Result};

mod args;
mod chat;
mod config;
mod session;
mod terminal;

use args::{CliCommand, RunOptions, parse_cli, print_completion, print_usage};
use chat::{
    CHAT_COMMANDS_HELP, ChatCommand, ParsedChatInput, clear_chat_context, parse_chat_input,
};
use config::run_config;
use session::{InferenceSession, default_run_id};
use terminal::assistant_text_for_terminal;

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

fn run_infer(options: RunOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "run", "starting command");
    let prompt = options
        .prompt
        .clone()
        .context("run requires PROMPT or --prompt TEXT")?;
    let mut session = InferenceSession::new(options, runtime, None)?;
    let messages = vec![TurnMessage::User { content: prompt }];
    let response = session.generate(&messages, 1)?;
    println!("{}", assistant_text_for_terminal(&response.content));
    Ok(())
}

fn run_chat(mut options: RunOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "chat", "starting command");
    let history_enabled = runtime.history.enabled && !options.no_history;
    let run_id = options.run_id.clone().unwrap_or_else(default_run_id);
    options.run_id = Some(run_id.clone());
    let session_id = if options.new_session {
        AgentLibreSessionId::generate()
    } else if let Some(session_id) = &options.session_id {
        AgentLibreSessionId::new(session_id.clone())?
    } else {
        AgentLibreSessionId::generate()
    };
    let resumed_session = history_enabled
        && !options.new_session
        && options.session_id.is_some()
        && ChatSessionStore::exists(runtime.paths.sessions_root(), &session_id);
    let explicit_artifact_root = InferenceSession::resolve_artifact_root(&options);
    let artifact_root_override = if history_enabled {
        explicit_artifact_root.or_else(|| {
            Some(
                runtime
                    .paths
                    .session_run_artifact_root(&session_id, &run_id),
            )
        })
    } else {
        None
    };
    let mut session = InferenceSession::new(options, runtime, artifact_root_override)?;
    let (chat_history, replay) = if history_enabled {
        if resumed_session {
            let history = ChatSessionStore::open(
                runtime.paths.sessions_root(),
                session_id.clone(),
                session.run_id().as_str().to_string(),
            )?;
            let replay = history.read_replay()?;
            (Some(history), Some(replay))
        } else {
            (
                Some(ChatSessionStore::start(
                    runtime.paths.sessions_root(),
                    session_id.clone(),
                    session.run_id().as_str().to_string(),
                    session.config_path().to_path_buf(),
                    "llama_cpp",
                )?),
                None,
            )
        }
    } else {
        (None, None)
    };
    let mut messages = replay
        .as_ref()
        .map(replay_turn_messages)
        .unwrap_or_default();
    let stdin = io::stdin();
    let mut request_index = replay
        .as_ref()
        .map(|replay| replay.next_attempt_index)
        .unwrap_or(1);
    let mut message_index = replay
        .as_ref()
        .map(|replay| replay.next_message_index)
        .unwrap_or(1);

    tracing::info!(
        target: "agentlibre::app",
        session_id = %session_id,
        run_id = %session.run_id(),
        artifact_root = %session.artifact_root().display(),
        history_enabled,
        resumed = resumed_session,
        replayed_messages = messages.len(),
        "chat session started"
    );
    println!("session_id={session_id}");

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
            ParsedChatInput::Empty => continue,
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
                print_chat_session_summary(&session_id, &session);
                continue;
            }
            ParsedChatInput::Command(ChatCommand::Clear) => {
                let cleared_messages = clear_chat_context(&mut messages);
                session.clear_context();
                if let Some(history) = &chat_history {
                    history.append_context_cleared()?;
                }
                tracing::info!(
                    target: "agentlibre::app",
                    session_id = %session_id,
                    run_id = %session.run_id(),
                    cleared_messages,
                    "chat context cleared"
                );
                println!("context_cleared=true cleared_messages={cleared_messages}");
                continue;
            }
            ParsedChatInput::Command(ChatCommand::Exit) => break,
        };

        let user_message_id = AgentLibreMessageId::indexed(message_index);
        message_index += 1;
        if let Some(history) = &chat_history {
            history.append_user_message(user_message_id.clone(), input.to_string())?;
        }
        log_message_metadata("user", &session_id, &user_message_id, input, runtime);
        messages.push(TurnMessage::User {
            content: input.to_string(),
        });
        if let Some(history) = &chat_history {
            history.link_attempt(format!("attempt-{request_index:04}"))?;
        }
        let response = session.generate(&messages, request_index)?;
        let content = assistant_text_for_terminal(&response.content);
        println!("assistant> {content}");
        let assistant_message_id = AgentLibreMessageId::indexed(message_index);
        message_index += 1;
        if let Some(history) = &chat_history {
            history.append_assistant_message(assistant_message_id.clone(), content.clone())?;
        }
        log_message_metadata(
            "assistant",
            &session_id,
            &assistant_message_id,
            &content,
            runtime,
        );
        messages.push(TurnMessage::Assistant { content });
        request_index += 1;
    }

    if let Some(history) = &chat_history {
        history.finish()?;
    }
    tracing::info!(
        target: "agentlibre::app",
        session_id = %session_id,
        run_id = %session.run_id(),
        "chat session finished"
    );
    Ok(())
}

fn print_chat_session_summary(session_id: &AgentLibreSessionId, session: &InferenceSession) {
    println!("session_id={session_id}");
    println!("run_id={}", session.run_id());
    println!("artifact_root={}", session.artifact_root().display());
}

fn replay_turn_messages(replay: &ChatSessionReplay) -> Vec<TurnMessage> {
    let mut messages = Vec::new();
    for event in &replay.events {
        match event {
            ChatSessionEvent::UserMessage { content, .. } => messages.push(TurnMessage::User {
                content: content.clone(),
            }),
            ChatSessionEvent::AssistantMessage { content, .. } => {
                messages.push(TurnMessage::Assistant {
                    content: content.clone(),
                });
            }
            ChatSessionEvent::ToolMessage { name, content, .. } => {
                messages.push(TurnMessage::ToolObservation {
                    name: name.clone(),
                    content: content.clone(),
                });
            }
            ChatSessionEvent::ContextCleared { .. } => messages.clear(),
            _ => {}
        }
    }
    messages
}

fn log_message_metadata(
    role: &str,
    session_id: &AgentLibreSessionId,
    message_id: &AgentLibreMessageId,
    content: &str,
    runtime: &AgentLibreRuntimeConfig,
) {
    let fields = logged_message_fields(role, content, runtime.logging.include_message_text);
    tracing::info!(
        target: "agentlibre::app",
        session_id = %session_id,
        message_id = %message_id,
        role = %fields.role,
        content_bytes = fields.content_bytes,
        content = ?fields.content,
        "chat message recorded"
    );
}

#[cfg(test)]
mod tests {
    use agl_runtime::{AgentLibreMessageId, AgentLibreSessionId};

    use crate::args::ConfigCommand;

    use super::*;

    #[test]
    fn replay_turn_messages_keeps_transcript_order() {
        let session_id = AgentLibreSessionId::new("session-001").unwrap();
        let replay = ChatSessionReplay {
            events: vec![
                ChatSessionEvent::SessionStarted {
                    session_id: session_id.clone(),
                    run_id: "run-001".to_string(),
                },
                ChatSessionEvent::UserMessage {
                    session_id: session_id.clone(),
                    message_id: AgentLibreMessageId::indexed(1),
                    content: "hello".to_string(),
                },
                ChatSessionEvent::AssistantMessage {
                    session_id: session_id.clone(),
                    message_id: AgentLibreMessageId::indexed(2),
                    content: "hi".to_string(),
                },
                ChatSessionEvent::ToolMessage {
                    session_id,
                    message_id: AgentLibreMessageId::indexed(3),
                    name: "read_file".to_string(),
                    content: "content".to_string(),
                },
            ],
            next_message_index: 4,
            next_attempt_index: 1,
        };

        assert_eq!(
            replay_turn_messages(&replay),
            vec![
                TurnMessage::User {
                    content: "hello".to_string()
                },
                TurnMessage::Assistant {
                    content: "hi".to_string()
                },
                TurnMessage::ToolObservation {
                    name: "read_file".to_string(),
                    content: "content".to_string()
                }
            ]
        );
    }

    #[test]
    fn replay_turn_messages_honors_context_clear() {
        let session_id = AgentLibreSessionId::new("session-001").unwrap();
        let replay = ChatSessionReplay {
            events: vec![
                ChatSessionEvent::UserMessage {
                    session_id: session_id.clone(),
                    message_id: AgentLibreMessageId::indexed(1),
                    content: "old".to_string(),
                },
                ChatSessionEvent::ContextCleared {
                    session_id: session_id.clone(),
                },
                ChatSessionEvent::UserMessage {
                    session_id,
                    message_id: AgentLibreMessageId::indexed(2),
                    content: "new".to_string(),
                },
            ],
            next_message_index: 3,
            next_attempt_index: 1,
        };

        assert_eq!(
            replay_turn_messages(&replay),
            vec![TurnMessage::User {
                content: "new".to_string()
            }]
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
}
