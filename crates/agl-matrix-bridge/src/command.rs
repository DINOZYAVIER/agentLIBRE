#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BridgeCommand {
    Help,
    Status,
    Bind { session_id: String },
    Unbind,
    Message { body: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandParseError {
    EmptyPrefix,
    MissingArgument { command: &'static str },
    UnknownCommand { command: String },
}

impl BridgeCommand {
    pub fn parse(input: &str, prefix: &str) -> Result<Option<Self>, CommandParseError> {
        if prefix.is_empty() {
            return Err(CommandParseError::EmptyPrefix);
        }

        let Some(rest) = input.strip_prefix(prefix) else {
            return Ok(None);
        };
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return Ok(None);
        }

        let rest = rest.trim();
        if rest.is_empty() || rest == "help" {
            return Ok(Some(Self::Help));
        }

        if rest == "status" {
            return Ok(Some(Self::Status));
        }

        if let Some(session_id) = command_argument(rest, "bind") {
            if session_id.is_empty() {
                return Err(CommandParseError::MissingArgument { command: "bind" });
            }

            return Ok(Some(Self::Bind {
                session_id: session_id.to_owned(),
            }));
        }

        if rest == "unbind" {
            return Ok(Some(Self::Unbind));
        }

        if let Some(body) = command_argument(rest, "send") {
            if body.is_empty() {
                return Err(CommandParseError::MissingArgument { command: "send" });
            }

            return Ok(Some(Self::Message {
                body: body.to_owned(),
            }));
        }

        let command = rest.split_whitespace().next().unwrap_or(rest).to_owned();
        Err(CommandParseError::UnknownCommand { command })
    }
}

fn command_argument<'a>(input: &'a str, command: &str) -> Option<&'a str> {
    let rest = input.strip_prefix(command)?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_non_commands() {
        assert_eq!(BridgeCommand::parse("hello", "!agl"), Ok(None));
    }

    #[test]
    fn ignores_prefix_without_token_boundary() {
        assert_eq!(BridgeCommand::parse("!aglance status", "!agl"), Ok(None));
    }

    #[test]
    fn parses_bind_command() {
        assert_eq!(
            BridgeCommand::parse("!agl bind session-1", "!agl"),
            Ok(Some(BridgeCommand::Bind {
                session_id: "session-1".to_owned()
            }))
        );
    }

    #[test]
    fn parses_unbind_command() {
        assert_eq!(
            BridgeCommand::parse("!agl unbind", "!agl"),
            Ok(Some(BridgeCommand::Unbind))
        );
    }

    #[test]
    fn requires_send_body() {
        assert_eq!(
            BridgeCommand::parse("!agl send", "!agl"),
            Err(CommandParseError::MissingArgument { command: "send" })
        );
    }

    #[test]
    fn rejects_subcommands_without_token_boundary() {
        assert_eq!(
            BridgeCommand::parse("!agl binding session-1", "!agl"),
            Err(CommandParseError::UnknownCommand {
                command: "binding".to_owned()
            })
        );
    }
}
