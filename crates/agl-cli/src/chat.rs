pub(crate) const CHAT_COMMANDS_HELP: &str = "\
Commands:
  /help
  /session
  /workspace [PATH]
  /clear
  /exit
  /quit
";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChatCommand {
    Help,
    Session,
    Clear,
    Exit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ParsedChatInput<'a> {
    Empty,
    Message(&'a str),
    Command(ChatCommand),
    Workspace(Option<&'a str>),
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
        "/workspace" => ParsedChatInput::Workspace(None),
        command if command.starts_with("/workspace ") => {
            let path = command["/workspace ".len()..].trim();
            if path.is_empty() {
                ParsedChatInput::Workspace(None)
            } else {
                ParsedChatInput::Workspace(Some(path))
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
