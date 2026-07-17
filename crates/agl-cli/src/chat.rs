pub(crate) const CHAT_COMMANDS_HELP: &str = "\
Commands:
  /help
  /session
  /workspace [PATH]
  /pwd
  /cd PATH
  /cd --host PATH
  /processes
  /attach EXECUTION_ID [--read-only]
  /kill EXECUTION_ID [--immediate]
  /reload
  /clear
  /exit
  /quit
";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChatCommand {
    Help,
    Session,
    Reload,
    Clear,
    Exit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ParsedChatInput<'a> {
    Empty,
    Message(&'a str),
    Command(ChatCommand),
    Workspace(Option<&'a str>),
    Pwd,
    Cd {
        path: &'a str,
        host: bool,
    },
    Processes,
    Attach {
        execution_id: &'a str,
        read_only: bool,
    },
    Kill {
        execution_id: &'a str,
        immediate: bool,
    },
    UnknownCommand(&'a str),
}

pub(crate) fn parse_chat_input(input: &str) -> ParsedChatInput<'_> {
    let input = input.trim();
    if input.is_empty() {
        return ParsedChatInput::Empty;
    }

    match input {
        "/help" => ParsedChatInput::Command(ChatCommand::Help),
        "/session" => ParsedChatInput::Command(ChatCommand::Session),
        "/reload" => ParsedChatInput::Command(ChatCommand::Reload),
        "/workspace" => ParsedChatInput::Workspace(None),
        command if command.starts_with("/workspace ") => {
            let path = command["/workspace ".len()..].trim();
            if path.is_empty() {
                ParsedChatInput::Workspace(None)
            } else {
                ParsedChatInput::Workspace(Some(path))
            }
        }
        "/pwd" => ParsedChatInput::Pwd,
        "/processes" => ParsedChatInput::Processes,
        command if command.starts_with("/cd ") => {
            let arguments = command["/cd ".len()..].trim();
            let (host, path) = arguments
                .strip_prefix("--host ")
                .map_or((false, arguments), |path| (true, path.trim()));
            if path.is_empty() {
                ParsedChatInput::UnknownCommand(command)
            } else {
                ParsedChatInput::Cd { path, host }
            }
        }
        command if command.starts_with("/attach ") => {
            let mut arguments = command["/attach ".len()..].split_whitespace();
            match (arguments.next(), arguments.next(), arguments.next()) {
                (Some(execution_id), None, None) => ParsedChatInput::Attach {
                    execution_id,
                    read_only: false,
                },
                (Some(execution_id), Some("--read-only"), None) => ParsedChatInput::Attach {
                    execution_id,
                    read_only: true,
                },
                _ => ParsedChatInput::UnknownCommand(command),
            }
        }
        command if command.starts_with("/kill ") => {
            let mut arguments = command["/kill ".len()..].split_whitespace();
            match (arguments.next(), arguments.next(), arguments.next()) {
                (Some(execution_id), None, None) => ParsedChatInput::Kill {
                    execution_id,
                    immediate: false,
                },
                (Some(execution_id), Some("--immediate"), None) => ParsedChatInput::Kill {
                    execution_id,
                    immediate: true,
                },
                _ => ParsedChatInput::UnknownCommand(command),
            }
        }
        "/clear" => ParsedChatInput::Command(ChatCommand::Clear),
        "/exit" | "/quit" => ParsedChatInput::Command(ChatCommand::Exit),
        unknown if unknown.starts_with('/') => ParsedChatInput::UnknownCommand(unknown),
        message => ParsedChatInput::Message(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chat_commands() {
        assert_eq!(
            parse_chat_input("/help"),
            ParsedChatInput::Command(ChatCommand::Help)
        );
        assert_eq!(
            parse_chat_input("/session"),
            ParsedChatInput::Command(ChatCommand::Session)
        );
        assert_eq!(
            parse_chat_input("/reload"),
            ParsedChatInput::Command(ChatCommand::Reload)
        );
        assert_eq!(
            parse_chat_input("/workspace"),
            ParsedChatInput::Workspace(None)
        );
        assert_eq!(
            parse_chat_input("/workspace ../repo"),
            ParsedChatInput::Workspace(Some("../repo"))
        );
        assert_eq!(
            parse_chat_input("/clear"),
            ParsedChatInput::Command(ChatCommand::Clear)
        );
        assert_eq!(
            parse_chat_input("/quit"),
            ParsedChatInput::Command(ChatCommand::Exit)
        );
        assert_eq!(parse_chat_input("/pwd"), ParsedChatInput::Pwd);
        assert_eq!(
            parse_chat_input("/cd child dir"),
            ParsedChatInput::Cd {
                path: "child dir",
                host: false
            }
        );
        assert_eq!(
            parse_chat_input("/cd --host /tmp"),
            ParsedChatInput::Cd {
                path: "/tmp",
                host: true
            }
        );
        assert_eq!(parse_chat_input("/processes"), ParsedChatInput::Processes);
        assert_eq!(
            parse_chat_input("/attach exec_example --read-only"),
            ParsedChatInput::Attach {
                execution_id: "exec_example",
                read_only: true
            }
        );
        assert_eq!(
            parse_chat_input("/kill exec_example --immediate"),
            ParsedChatInput::Kill {
                execution_id: "exec_example",
                immediate: true
            }
        );
    }

    #[test]
    fn parses_chat_messages_and_unknown_commands() {
        assert_eq!(
            parse_chat_input("  hello  "),
            ParsedChatInput::Message("hello")
        );
        assert_eq!(
            parse_chat_input("/unknown"),
            ParsedChatInput::UnknownCommand("/unknown")
        );
        assert_eq!(parse_chat_input(""), ParsedChatInput::Empty);
    }
}
