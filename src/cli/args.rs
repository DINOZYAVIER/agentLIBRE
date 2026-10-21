//! Command-line arguments (D51, D53, D146).

use std::ffi::OsString;

use crate::backend::ReasoningLevel;
use crate::error::Error;

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Print the help text.
    Help,
    /// Run the agent.
    Run(Args),
    /// `agl server start|stop|status` (D146); the flag is `--yes` of
    /// `start` (AGL-015, D16).
    Server(ServerAction, bool),
    /// `agl init` (AGL-010): register the current directory as
    /// a project.
    Init,
    /// `agl forge …` (AGL-007); the arguments after the word
    /// `forge` are parsed in the forge surface.
    Forge(Vec<OsString>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerAction {
    Start,
    Stop,
    Status,
}

/// Options of a run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// `-p <message>`: answer one message and exit; REPL otherwise.
    pub prompt: Option<String>,
    /// `--resume <id>`.
    pub resume: Option<String>,
    /// `--function <name>`.
    pub function: Option<String>,
    /// `--think <level>`: the model's reasoning level (AGL-013).
    pub reasoning: Option<ReasoningLevel>,
    /// `--yes`: start the server of the project even when another
    /// project's server runs, without asking (AGL-015, D13, D16).
    pub yes: bool,
}

/// Parses the arguments after the program name. Each option takes the
/// next argument as its value, even one that starts with `-`.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, Error> {
    let mut parsed = Args::default();
    let mut args = args.into_iter().peekable();
    let text = |arg: OsString| arg.into_string().map_err(|_| Error::InvalidUnicode);
    // The forge surface parses its own arguments (AGL-007).
    if args.peek().is_some_and(|arg| arg == "forge") {
        args.next();
        return Ok(Command::Forge(args.collect()));
    }
    if args.peek().is_some_and(|arg| arg == "init") {
        args.next();
        return match args.next().map(text).transpose()?.as_deref() {
            None => Ok(Command::Init),
            Some("-h" | "--help") => Ok(Command::Help),
            Some(other) => Err(Error::UnknownOption(other.into())),
        };
    }
    if args.peek().is_some_and(|arg| arg == "server") {
        args.next();
        let action = match args.next().map(text).transpose()?.as_deref() {
            Some("start") => ServerAction::Start,
            Some("stop") => ServerAction::Stop,
            Some("status") => ServerAction::Status,
            Some("-h" | "--help") => return Ok(Command::Help),
            Some(other) => return Err(Error::UnknownOption(other.into())),
            None => return Err(Error::MissingValue("server".into())),
        };
        let mut yes = false;
        for extra in args {
            match text(extra)?.as_str() {
                "--yes" if action == ServerAction::Start && !yes => yes = true,
                "--yes" if action == ServerAction::Start => {
                    return Err(Error::RepeatedOption("--yes".into()));
                }
                other => return Err(Error::UnknownOption(other.into())),
            }
        }
        return Ok(Command::Server(action, yes));
    }
    // `--think` is parsed to its level after the loop (AGL-013).
    let mut think: Option<String> = None;
    while let Some(arg) = args.next() {
        let arg = text(arg)?;
        let slot = match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--yes" => {
                if parsed.yes {
                    return Err(Error::RepeatedOption(arg));
                }
                parsed.yes = true;
                continue;
            }
            "-p" => &mut parsed.prompt,
            "--resume" => &mut parsed.resume,
            "--function" => &mut parsed.function,
            "--think" => &mut think,
            _ => return Err(Error::UnknownOption(arg)),
        };
        if slot.is_some() {
            return Err(Error::RepeatedOption(arg));
        }
        let value = args
            .next()
            .ok_or_else(|| Error::MissingValue(arg.clone()))?;
        *slot = Some(text(value)?);
    }
    if let Some(value) = think {
        parsed.reasoning =
            Some(ReasoningLevel::parse(&value).ok_or(Error::BadReasoningLevel { value })?);
    }
    Ok(Command::Run(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Command, Error> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn no_arguments_start_the_repl() {
        assert_eq!(parse_strs(&[]).unwrap(), Command::Run(Args::default()));
    }

    #[test]
    fn all_options() {
        let command = parse_strs(&["--function", "coder", "-p", "hi there", "--resume", "id"]);
        assert_eq!(
            command.unwrap(),
            Command::Run(Args {
                prompt: Some("hi there".into()),
                resume: Some("id".into()),
                function: Some("coder".into()),
                reasoning: None,
                yes: false,
            })
        );
    }

    #[test]
    fn yes_is_a_flag_without_a_value() {
        assert_eq!(
            parse_strs(&["-p", "hi", "--yes"]).unwrap(),
            Command::Run(Args {
                prompt: Some("hi".into()),
                yes: true,
                ..Args::default()
            })
        );
        // Before the message too, and in the stdin session (no `-p`).
        assert_eq!(
            parse_strs(&["--yes"]).unwrap(),
            Command::Run(Args {
                yes: true,
                ..Args::default()
            })
        );
        assert!(matches!(
            parse_strs(&["--yes", "--yes"]),
            Err(Error::RepeatedOption(o)) if o == "--yes"
        ));
        // A message may be "--yes" as the value of `-p`.
        assert_eq!(
            parse_strs(&["-p", "--yes"]).unwrap(),
            Command::Run(Args {
                prompt: Some("--yes".into()),
                ..Args::default()
            })
        );
    }

    #[test]
    fn think_takes_the_four_levels() {
        for value in ["off", "low", "medium", "xhigh"] {
            let command = parse_strs(&["--think", value]).unwrap();
            let Command::Run(args) = command else {
                panic!("expected a run");
            };
            assert_eq!(args.reasoning, ReasoningLevel::parse(value));
        }
        // A value outside the four, a missing value and a repetition are
        // usage errors (AGL-013).
        assert!(matches!(
            parse_strs(&["--think", "high"]),
            Err(Error::BadReasoningLevel { value }) if value == "high"
        ));
        assert!(matches!(
            parse_strs(&["--think"]),
            Err(Error::MissingValue(o)) if o == "--think"
        ));
        assert!(matches!(
            parse_strs(&["--think", "off", "--think", "low"]),
            Err(Error::RepeatedOption(o)) if o == "--think"
        ));
    }

    #[test]
    fn values_may_start_with_a_dash() {
        let command = parse_strs(&["-p", "--help"]).unwrap();
        let Command::Run(args) = command else {
            panic!("expected a run");
        };
        assert_eq!(args.prompt.as_deref(), Some("--help"));
    }

    #[test]
    fn help() {
        assert_eq!(parse_strs(&["-h"]).unwrap(), Command::Help);
        assert_eq!(parse_strs(&["-p", "x", "--help"]).unwrap(), Command::Help);
    }

    #[test]
    fn errors() {
        assert!(matches!(
            parse_strs(&["--url", "x"]),
            Err(Error::UnknownOption(o)) if o == "--url"
        ));
        assert!(matches!(
            parse_strs(&["hello"]),
            Err(Error::UnknownOption(o)) if o == "hello"
        ));
        assert!(matches!(
            parse_strs(&["-p"]),
            Err(Error::MissingValue(o)) if o == "-p"
        ));
        assert!(matches!(
            parse_strs(&["-p", "a", "-p", "b"]),
            Err(Error::RepeatedOption(o)) if o == "-p"
        ));
    }

    #[test]
    fn server_commands() {
        for (word, action) in [
            ("start", ServerAction::Start),
            ("stop", ServerAction::Stop),
            ("status", ServerAction::Status),
        ] {
            assert_eq!(
                parse_strs(&["server", word]).unwrap(),
                Command::Server(action, false)
            );
        }
        // `--yes` belongs to `start` only (AGL-015, D16).
        assert_eq!(
            parse_strs(&["server", "start", "--yes"]).unwrap(),
            Command::Server(ServerAction::Start, true)
        );
        for word in ["stop", "status"] {
            assert!(matches!(
                parse_strs(&["server", word, "--yes"]),
                Err(Error::UnknownOption(o)) if o == "--yes"
            ));
        }
        assert!(matches!(
            parse_strs(&["server", "start", "--yes", "--yes"]),
            Err(Error::RepeatedOption(o)) if o == "--yes"
        ));
        assert_eq!(parse_strs(&["server", "--help"]).unwrap(), Command::Help);
        assert!(matches!(
            parse_strs(&["server"]),
            Err(Error::MissingValue(o)) if o == "server"
        ));
        assert!(matches!(
            parse_strs(&["server", "restart"]),
            Err(Error::UnknownOption(o)) if o == "restart"
        ));
        assert!(matches!(
            parse_strs(&["server", "start", "-p"]),
            Err(Error::UnknownOption(o)) if o == "-p"
        ));
        // Only the first word is a command; a message may say "server".
        assert_eq!(
            parse_strs(&["-p", "server"]).unwrap(),
            Command::Run(Args {
                prompt: Some("server".into()),
                ..Args::default()
            })
        );
    }

    #[test]
    fn forge_keeps_the_rest_of_the_arguments() {
        assert_eq!(parse_strs(&["forge"]).unwrap(), Command::Forge(Vec::new()));
        assert_eq!(
            parse_strs(&["forge", "list", "-h", "x"]).unwrap(),
            Command::Forge(vec!["list".into(), "-h".into(), "x".into()])
        );
        // Only the first word is a command; a message may say "forge".
        assert_eq!(
            parse_strs(&["-p", "forge"]).unwrap(),
            Command::Run(Args {
                prompt: Some("forge".into()),
                ..Args::default()
            })
        );
    }

    #[test]
    fn init_takes_no_arguments() {
        assert_eq!(parse_strs(&["init"]).unwrap(), Command::Init);
        assert_eq!(parse_strs(&["init", "--help"]).unwrap(), Command::Help);
        assert!(matches!(
            parse_strs(&["init", "x"]),
            Err(Error::UnknownOption(o)) if o == "x"
        ));
        // Only the first word is a command; a message may say "init".
        assert_eq!(
            parse_strs(&["-p", "init"]).unwrap(),
            Command::Run(Args {
                prompt: Some("init".into()),
                ..Args::default()
            })
        );
    }

    #[test]
    fn non_utf8_is_an_error() {
        use std::os::unix::ffi::OsStringExt;
        let arg = OsString::from_vec(vec![0xff]);
        assert!(matches!(
            parse([OsString::from("-p"), arg]),
            Err(Error::InvalidUnicode)
        ));
    }
}
