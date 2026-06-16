use std::env;
use std::io::{self, Write};
use std::process;

use agl_runtime::{
    AgentLibreMessageId, AgentLibreRuntimeConfig, AgentLibreSessionId, ChatSessionEvent,
    ChatSessionReplay, ChatSessionStore, init_tracing, logged_message_fields,
};
use agl_turn::TurnMessage;
use anyhow::{Context, Result};

mod args;
mod session;
mod terminal;

use args::{CliCommand, RunOptions, parse_cli, print_usage};
use session::{InferenceSession, default_run_id};
use terminal::assistant_text_for_terminal;

fn main() {
    let runtime = match AgentLibreRuntimeConfig::from_env() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("error: failed to resolve agentLIBRE runtime: {err:#}");
            process::exit(1);
        }
    };
    let _tracing_guards = match init_tracing(&runtime.paths, &runtime.logging) {
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

    if let Err(err) = run(env::args(), &runtime) {
        tracing::error!(target: "agentlibre::app", error = %err, "agentLIBRE command failed");
        eprintln!("error: {err:#}");
        process::exit(1);
    }
}

fn run(args: impl IntoIterator<Item = String>, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    match parse_cli(args)? {
        CliCommand::Help => {
            print_usage();
            Ok(())
        }
        CliCommand::Infer(options) => run_infer(options, runtime),
        CliCommand::Chat(options) => run_chat(options, runtime),
    }
}

fn run_infer(options: RunOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "infer", "starting command");
    let prompt = options
        .prompt
        .clone()
        .context("infer requires --prompt TEXT")?;
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

fn replay_turn_messages(replay: &ChatSessionReplay) -> Vec<TurnMessage> {
    replay
        .events
        .iter()
        .filter_map(|event| match event {
            ChatSessionEvent::UserMessage { content, .. } => Some(TurnMessage::User {
                content: content.clone(),
            }),
            ChatSessionEvent::AssistantMessage { content, .. } => Some(TurnMessage::Assistant {
                content: content.clone(),
            }),
            ChatSessionEvent::ToolMessage { name, content, .. } => {
                Some(TurnMessage::ToolObservation {
                    name: name.clone(),
                    content: content.clone(),
                })
            }
            _ => None,
        })
        .collect()
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
}
