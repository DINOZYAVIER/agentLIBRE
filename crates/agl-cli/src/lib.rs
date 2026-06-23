use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;

use agl_loop::{TurnInput, TurnOutput, run_turn};
use agl_runtime::{
    AgentLibreHistoryConfig, AgentLibreLoggingConfig, AgentLibrePaths, AgentLibreProcessMode,
    AgentLibreRuntimeConfig, AgentLibreWorkspaceConfig, init_tracing, logged_message_fields,
};
use agl_session::{
    AgentLibreMessageId, AgentLibreSessionId, ChatSessionEvent, ChatSessionReplay, ChatSessionStore,
};
use agl_turn::{StopReason, TurnHookBatch, TurnMessage, VisibleTool};
use anyhow::{Context, Result};

mod args;
mod chat;
mod config;
mod loop_host;
mod prompt;
mod session;
mod terminal;

use args::{CliCommand, RunOptions, parse_cli, print_completion, print_usage};
use chat::{
    CHAT_COMMANDS_HELP, ChatCommand, ParsedChatInput, clear_chat_context, parse_chat_input,
};
use config::run_config;
use loop_host::CliLoopHost;
use session::{InferenceSession, default_run_id};
use terminal::assistant_text_for_terminal;

const MAX_TOOL_CALLS_PER_TURN: usize = 8;

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

fn run_infer(options: RunOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "run", "starting command");
    let prompt = options
        .prompt
        .clone()
        .context("run requires PROMPT or --prompt TEXT")?;
    let workspace_root = runtime.resolve_workspace_root(options.workspace_root.as_deref())?;
    let tool_mode = options.tool_mode;
    let session = InferenceSession::new(options, runtime, None)?;
    let mut loop_host = CliLoopHost::new(session, &workspace_root)?;
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
    let workspace_root = runtime.resolve_workspace_root(options.workspace_root.as_deref())?;
    let tool_mode = options.tool_mode;
    let artifact_root_override = if history_enabled {
        explicit_artifact_root.or_else(|| {
            Some(
                runtime
                    .paths
                    .session_run_artifact_root(session_id.as_str(), &run_id),
            )
        })
    } else {
        None
    };
    let session = InferenceSession::new(options, runtime, artifact_root_override)?;
    let (mut chat_history, replay) = if history_enabled {
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
                    session.backend_name(),
                )?),
                None,
            )
        }
    } else {
        (None, None)
    };
    let mut loop_host = CliLoopHost::new(session, &workspace_root)?;
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
        run_id = %loop_host.session().run_id(),
        artifact_root = %loop_host.session().artifact_root().display(),
        event_stream = %loop_host.event_sink_path().display(),
        workspace_root = %loop_host.workspace_root().display(),
        tool_mode = tool_mode.as_str(),
        history_enabled,
        resumed = resumed_session,
        replayed_messages = messages.len(),
        "chat session started"
    );
    println!("session_id={session_id}");

    let mut session_finished = false;
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
                print_chat_session_summary(
                    &session_id,
                    loop_host.session(),
                    loop_host.workspace_root(),
                );
                continue;
            }
            ParsedChatInput::Workspace(path) => {
                if let Some(path) = path {
                    let root = chat_workspace_root(path, loop_host.workspace_root());
                    if let Err(err) = loop_host.set_workspace_root(&root) {
                        tracing::warn!(
                            target: "agentlibre::app",
                            session_id = %session_id,
                            run_id = %loop_host.session().run_id(),
                            requested_workspace_root = %root.display(),
                            error = %err,
                            "chat workspace root change failed"
                        );
                        println!("workspace_error={err:#}");
                    } else {
                        tracing::info!(
                            target: "agentlibre::app",
                            session_id = %session_id,
                            run_id = %loop_host.session().run_id(),
                            workspace_root = %loop_host.workspace_root().display(),
                            "chat workspace root changed"
                        );
                    }
                }
                println!("workspace_root={}", loop_host.workspace_root().display());
                continue;
            }
            ParsedChatInput::Command(ChatCommand::Clear) => {
                let cleared_messages = clear_chat_context(&mut messages);
                loop_host.session_mut().clear_context();
                if let Some(history) = &mut chat_history {
                    history.append_context_cleared()?;
                }
                tracing::info!(
                    target: "agentlibre::app",
                    session_id = %session_id,
                    run_id = %loop_host.session().run_id(),
                    cleared_messages,
                    "chat context cleared"
                );
                println!("context_cleared=true cleared_messages={cleared_messages}");
                continue;
            }
            ParsedChatInput::Command(ChatCommand::Exit) => {
                if let Some(history) = &mut chat_history {
                    history.request_exit()?;
                    session_finished = true;
                }
                break;
            }
        };

        let user_message_id = AgentLibreMessageId::indexed(message_index);
        message_index += 1;
        log_message_metadata("user", &session_id, &user_message_id, input, runtime);
        let attempt_id = format!("attempt-{request_index:04}");
        if let Some(history) = &mut chat_history {
            history.append_user_message(user_message_id.clone(), input.to_string())?;
            history.link_attempt(attempt_id.clone())?;
        }
        let turn_input = build_turn_input(
            loop_host.session().run_id().as_str(),
            request_index,
            &messages,
            loop_host.session().turn_hook_batches(),
            loop_host.session().turn_visible_tools(),
            input,
        );
        loop_host.reset_turn_counters();
        let output = match run_turn(&mut loop_host, turn_input) {
            Ok(output) => output,
            Err(err) => {
                if let Some(history) = &mut chat_history {
                    history.fail(format!("{err:#}"))?;
                }
                return Err(err);
            }
        };
        let generated_requests = loop_host.generated_requests();
        messages.push(TurnMessage::User {
            content: input.to_string(),
        });
        match output {
            TurnOutput::Answered { answer } => {
                let content = assistant_text_for_terminal(&answer);
                println!("assistant> {content}");
                let assistant_message_id = AgentLibreMessageId::indexed(message_index);
                message_index += 1;
                if let Some(history) = &mut chat_history {
                    history
                        .append_assistant_message(assistant_message_id.clone(), content.clone())?;
                }
                log_message_metadata(
                    "assistant",
                    &session_id,
                    &assistant_message_id,
                    &content,
                    runtime,
                );
                messages.push(TurnMessage::Assistant { content });
            }
            TurnOutput::Stopped { reason } => {
                println!("stopped=true reason={}", reason.as_str());
                let content = stopped_turn_context_message(reason).to_string();
                let assistant_message_id = AgentLibreMessageId::indexed(message_index);
                message_index += 1;
                if let Some(history) = &mut chat_history {
                    history.append_assistant_stop_marker(
                        assistant_message_id.clone(),
                        content.clone(),
                    )?;
                }
                log_message_metadata(
                    "assistant",
                    &session_id,
                    &assistant_message_id,
                    &content,
                    runtime,
                );
                messages.push(TurnMessage::Assistant { content });
            }
        }
        request_index += generated_requests;
    }

    if !session_finished && let Some(history) = &mut chat_history {
        history.finish_eof()?;
    }
    tracing::info!(
        target: "agentlibre::app",
        session_id = %session_id,
        run_id = %loop_host.session().run_id(),
        "chat session finished"
    );
    Ok(())
}

fn build_turn_input(
    run_id: &str,
    request_index: usize,
    context_messages: &[TurnMessage],
    hook_batches: &[TurnHookBatch],
    visible_tools: &[VisibleTool],
    user_input: &str,
) -> TurnInput {
    let mut input = TurnInput::user(user_input.to_string())
        .with_turn_id(run_id.to_string())
        .with_context_messages(context_messages.to_vec())
        .with_request_index_start(request_index);
    for hook_batch in hook_batches {
        input = input.with_hook_batch(hook_batch.clone());
    }
    for tool in visible_tools {
        input = input.with_visible_tool(tool.clone());
    }
    if !visible_tools.is_empty() {
        input = input.with_max_tool_calls(MAX_TOOL_CALLS_PER_TURN);
    }
    input
}

fn print_turn_output(output: &TurnOutput) {
    match output {
        TurnOutput::Answered { answer } => println!("{}", assistant_text_for_terminal(answer)),
        TurnOutput::Stopped { reason } => println!("stopped=true reason={}", reason.as_str()),
    }
}

fn stopped_turn_context_message(reason: StopReason) -> &'static str {
    match reason {
        StopReason::ToolJsonUnrepairable => {
            "The previous turn stopped because the model produced malformed tool JSON. No tool was executed."
        }
        StopReason::ToolLimitReached => {
            "The previous turn stopped because tool use is not available in this CLI session. No tool was executed."
        }
        StopReason::HiddenTool => {
            "The previous turn stopped because the requested tool is not available in this CLI session. No tool was executed."
        }
        StopReason::InvalidToolArguments => {
            "The previous turn stopped because the requested tool arguments were invalid. No tool was executed."
        }
    }
}

fn print_chat_session_summary(
    session_id: &AgentLibreSessionId,
    session: &InferenceSession,
    workspace_root: &Path,
) {
    println!("session_id={session_id}");
    println!("run_id={}", session.run_id());
    println!("artifact_root={}", session.artifact_root().display());
    println!("workspace_root={}", workspace_root.display());
}

fn chat_workspace_root(input: &str, current_root: &Path) -> PathBuf {
    let path = PathBuf::from(input);
    if path.is_absolute() {
        path
    } else {
        current_root.join(path)
    }
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
    use agl_session::{AgentLibreMessageId, AgentLibreSessionId};

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
    fn build_turn_input_preserves_context_and_request_index() {
        let context = vec![
            TurnMessage::User {
                content: "old".to_string(),
            },
            TurnMessage::Assistant {
                content: "previous".to_string(),
            },
        ];

        let hook_batches = vec![
            TurnHookBatch::new(agl_loop::HookEvent::ArtifactWrite)
                .with_required_hook(agl_loop::HookId::new("task_spec.validate").unwrap()),
        ];

        let visible_tools = vec![
            VisibleTool::new("fs.read")
                .describe("Read a repository file")
                .require_argument("path"),
        ];

        let input = build_turn_input("run-001", 7, &context, &hook_batches, &visible_tools, "new");

        assert_eq!(input.turn_id, "run-001");
        assert_eq!(input.user_input, "new");
        assert_eq!(input.context_messages, context);
        assert_eq!(input.hook_batches, hook_batches);
        assert_eq!(input.request_index_start, 7);
        assert_eq!(input.visible_tools, visible_tools);
        assert_eq!(input.max_tool_calls, MAX_TOOL_CALLS_PER_TURN);
    }

    #[test]
    fn build_turn_input_keeps_tools_disabled_without_visible_tools() {
        let input = build_turn_input("run-001", 1, &[], &[], &[], "new");

        assert!(input.visible_tools.is_empty());
        assert_eq!(input.max_tool_calls, 0);
    }

    #[test]
    fn chat_workspace_root_resolves_relative_to_current_root() {
        assert_eq!(
            chat_workspace_root("../next", std::path::Path::new("/tmp/root/current")),
            PathBuf::from("/tmp/root/current/../next")
        );
        assert_eq!(
            chat_workspace_root("/tmp/absolute", std::path::Path::new("/tmp/root")),
            PathBuf::from("/tmp/absolute")
        );
    }

    #[test]
    fn stop_reason_names_are_cli_stable() {
        assert_eq!(
            StopReason::ToolJsonUnrepairable.as_str(),
            "tool_json_unrepairable"
        );
        assert_eq!(StopReason::ToolLimitReached.as_str(), "tool_limit_reached");
        assert_eq!(StopReason::HiddenTool.as_str(), "hidden_tool");
        assert_eq!(
            StopReason::InvalidToolArguments.as_str(),
            "invalid_tool_arguments"
        );
    }

    #[test]
    fn stopped_turn_context_message_explains_no_tool_execution() {
        for reason in [
            StopReason::ToolJsonUnrepairable,
            StopReason::ToolLimitReached,
            StopReason::HiddenTool,
            StopReason::InvalidToolArguments,
        ] {
            let message = stopped_turn_context_message(reason);

            assert!(message.contains("previous turn stopped"));
            assert!(message.contains("No tool was executed."));
        }
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
