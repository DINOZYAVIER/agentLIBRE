use agl_turn::TurnMessage;

pub(crate) const CHAT_COMMANDS_HELP: &str = "\
Commands:
  /help
  /session
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
        "/clear" => ParsedChatInput::Command(ChatCommand::Clear),
        "/exit" | "/quit" => ParsedChatInput::Command(ChatCommand::Exit),
        unknown if unknown.starts_with('/') => ParsedChatInput::UnknownCommand(unknown),
        message => ParsedChatInput::Message(message),
    }
}

pub(crate) fn clear_chat_context(messages: &mut Vec<TurnMessage>) -> usize {
    let cleared_messages = messages.len();
    messages.clear();
    cleared_messages
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

    #[test]
    fn clear_chat_context_removes_messages() {
        let mut messages = vec![
            TurnMessage::User {
                content: "hello".to_string(),
            },
            TurnMessage::Assistant {
                content: "hi".to_string(),
            },
        ];

        let cleared = clear_chat_context(&mut messages);

        assert_eq!(cleared, 2);
        assert!(messages.is_empty());
    }
}
