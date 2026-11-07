//! Command-line arguments (D51, D53, D146).

use std::ffi::OsString;

use crate::backend::ReasoningLevel;
use crate::error::Error;

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Print the help text.
    Help,
    /// Print the version (AGL-048).
    Version,
    /// Run the agent.
    Run(Args),
    /// `agl server start|stop|status` (D146); the flag is `--yes` of
    /// `start` (AGL-015, D16), and the option `--function <name>` of
    /// `start` and `status` names the Function whose `[server]` table is
    /// the model side (AGL-035, D3).
    Server(ServerAction, bool, Option<String>),
    /// `agl init` (AGL-010): register the current directory as
    /// a project.
    Init,
    /// `agl forge …` (AGL-007); the arguments after the word
    /// `forge` are parsed in the forge surface.
    Forge(Vec<OsString>),
    /// `agl function list|use` (AGL-036, D3).
    Function(FunctionAction),
    /// `agl search login` (AGL-043 F20).
    Search(SearchAction),
    /// `agl memory list|show|remove` (AGL-060).
    Memory(MemoryAction),
}

/// What `agl memory` and `/memory` ask for (AGL-060, D2, D5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryAction {
    List,
    Show { name: String },
    /// `yes`: no question (D3).
    Remove { name: String, yes: bool },
}

/// Reads the words after `memory`, of the command line and of `/memory`.
pub fn memory_action(words: impl IntoIterator<Item = String>) -> Result<MemoryAction, Error> {
    let mut words = words.into_iter();
    let action = words
        .next()
        .ok_or_else(|| Error::MissingValue("memory".into()))?;
    let parsed = match action.as_str() {
        "list" => MemoryAction::List,
        "show" => MemoryAction::Show {
            name: words
                .next()
                .ok_or_else(|| Error::MissingValue("show".into()))?,
        },
        "remove" => {
            let mut name = None;
            let mut yes = false;
            for word in words.by_ref() {
                match word.as_str() {
                    "--yes" if yes => return Err(Error::RepeatedOption(word)),
                    "--yes" => yes = true,
                    _ if word.starts_with('-') || name.is_some() => {
                        return Err(Error::UnknownOption(word));
                    }
                    _ => name = Some(word),
                }
            }
            MemoryAction::Remove {
                name: name.ok_or_else(|| Error::MissingValue("remove".into()))?,
                yes,
            }
        }
        _ => return Err(Error::UnknownOption(action)),
    };
    match words.next() {
        Some(extra) => Err(Error::UnknownOption(extra)),
        None => Ok(parsed),
    }
}

/// `agl search login` (AGL-043 F20).
#[derive(Debug, PartialEq, Eq)]
pub enum SearchAction {
    Login,
}

/// `agl function list` or `agl function use <name> [--user]` (AGL-036,
/// D3): `user` writes the user's file instead of the project's.
#[derive(Debug, PartialEq, Eq)]
pub enum FunctionAction {
    List,
    Use { name: String, user: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerAction {
    Start,
    Stop,
    Status,
    /// The line that starts the server of a Function (AGL-052, D12).
    Command,
}

/// Options of a run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// `-p <message>`: answer one message and exit; REPL otherwise.
    pub prompt: Option<String>,
    /// `--resume <id>`, or `last`.
    pub resume: Option<String>,
    /// `--resume` with no id: the sessions are listed (AGL-058, D20).
    pub list_sessions: bool,
    /// `--function <name>`.
    pub function: Option<String>,
    /// `--think <level>`: the model's reasoning level (AGL-013).
    pub reasoning: Option<ReasoningLevel>,
    /// `--yes`: start the server of the project even when another
    /// project's server runs, without asking (AGL-015, D13, D16).
    pub yes: bool,
    /// `--attach <path>`, repeatable: files that go with the `-p` message
    /// (AGL-014, D21).
    pub attach: Vec<String>,
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
    if args.peek().is_some_and(|arg| arg == "function") {
        args.next();
        let action = match args.next().map(text).transpose()?.as_deref() {
            Some("list") => FunctionAction::List,
            Some("use") => {
                let name = args
                    .next()
                    .ok_or_else(|| Error::MissingValue("use".into()))
                    .and_then(text)?;
                FunctionAction::Use { name, user: false }
            }
            Some("-h" | "--help") => return Ok(Command::Help),
            Some(other) => return Err(Error::UnknownOption(other.into())),
            None => return Err(Error::MissingValue("function".into())),
        };
        let mut action = action;
        for extra in args {
            match (text(extra)?.as_str(), &mut action) {
                ("--user", FunctionAction::Use { user, .. }) if !*user => *user = true,
                ("--user", FunctionAction::Use { .. }) => {
                    return Err(Error::RepeatedOption("--user".into()));
                }
                (other, _) => return Err(Error::UnknownOption(other.into())),
            }
        }
        return Ok(Command::Function(action));
    }
    if args.peek().is_some_and(|arg| arg == "search") {
        args.next();
        let action = match args.next().map(text).transpose()?.as_deref() {
            Some("login") => SearchAction::Login,
            Some("-h" | "--help") => return Ok(Command::Help),
            Some(other) => return Err(Error::UnknownOption(other.into())),
            None => return Err(Error::MissingValue("search".into())),
        };
        if let Some(extra) = args.next() {
            return Err(Error::UnknownOption(text(extra)?));
        }
        return Ok(Command::Search(action));
    }
    if args.peek().is_some_and(|arg| arg == "memory") {
        args.next();
        let words = args.map(text).collect::<Result<Vec<_>, _>>()?;
        if matches!(words.first().map(String::as_str), Some("-h" | "--help")) {
            return Ok(Command::Help);
        }
        return memory_action(words).map(Command::Memory);
    }
    if args.peek().is_some_and(|arg| arg == "server") {
        args.next();
        let action = match args.next().map(text).transpose()?.as_deref() {
            Some("start") => ServerAction::Start,
            Some("stop") => ServerAction::Stop,
            Some("status") => ServerAction::Status,
            Some("command") => ServerAction::Command,
            Some("-h" | "--help") => return Ok(Command::Help),
            Some(other) => return Err(Error::UnknownOption(other.into())),
            None => return Err(Error::MissingValue("server".into())),
        };
        let mut yes = false;
        let mut function = None;
        while let Some(extra) = args.next() {
            match text(extra)?.as_str() {
                "--yes" if action == ServerAction::Start && !yes => yes = true,
                "--yes" if action == ServerAction::Start => {
                    return Err(Error::RepeatedOption("--yes".into()));
                }
                "--function" if action != ServerAction::Stop && function.is_none() => {
                    let value = args
                        .next()
                        .ok_or_else(|| Error::MissingValue("--function".into()))?;
                    function = Some(text(value)?);
                }
                "--function" if action != ServerAction::Stop => {
                    return Err(Error::RepeatedOption("--function".into()));
                }
                other => return Err(Error::UnknownOption(other.into())),
            }
        }
        return Ok(Command::Server(action, yes, function));
    }
    // `--think` is parsed to its level after the loop (AGL-013).
    let mut think: Option<String> = None;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let arg = text(arg)?;
        // `--resume` alone, or before another option (AGL-058, D20).
        if arg == "--resume"
            && parsed.resume.is_none()
            && args.peek().is_none_or(|next| next.as_encoded_bytes().starts_with(b"-"))
        {
            parsed.list_sessions = true;
            continue;
        }
        let slot = match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--version" => return Ok(Command::Version),
            "--yes" => {
                if parsed.yes {
                    return Err(Error::RepeatedOption(arg));
                }
                parsed.yes = true;
                continue;
            }
            "--attach" => {
                let value = args
                    .next()
                    .ok_or_else(|| Error::MissingValue(arg.clone()))?;
                parsed.attach.push(text(value)?);
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
    if !parsed.attach.is_empty() && parsed.prompt.is_none() {
        return Err(Error::AttachWithoutPrompt);
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
                list_sessions: false,
                function: Some("coder".into()),
                reasoning: None,
                yes: false,
                attach: Vec::new(),
            })
        );
    }

    #[test]
    fn attach_is_repeatable_and_needs_a_message() {
        assert_eq!(
            parse_strs(&["-p", "look", "--attach", "a.png", "--attach", "b.wav"]).unwrap(),
            Command::Run(Args {
                prompt: Some("look".into()),
                attach: vec!["a.png".into(), "b.wav".into()],
                ..Args::default()
            })
        );
        assert!(matches!(
            parse_strs(&["--attach", "a.png"]),
            Err(Error::AttachWithoutPrompt)
        ));
        assert!(matches!(
            parse_strs(&["-p", "x", "--attach"]),
            Err(Error::MissingValue(o)) if o == "--attach"
        ));
        // A value that starts with `-` is still the value.
        assert_eq!(
            parse_strs(&["-p", "x", "--attach", "-odd.png"]).unwrap(),
            Command::Run(Args {
                prompt: Some("x".into()),
                attach: vec!["-odd.png".into()],
                ..Args::default()
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
    fn version() {
        assert_eq!(parse_strs(&["--version"]).unwrap(), Command::Version);
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
    fn resume_with_no_id_lists_the_sessions() {
        // AGL-058, D20.
        let Command::Run(args) = parse_strs(&["--resume"]).unwrap() else {
            panic!("not a run");
        };
        assert!(args.list_sessions);
        assert_eq!(args.resume, None);
        let Command::Run(args) = parse_strs(&["--resume", "last"]).unwrap() else {
            panic!("not a run");
        };
        assert!(!args.list_sessions);
        assert_eq!(args.resume.as_deref(), Some("last"));
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
                Command::Server(action, false, None)
            );
        }
        assert_eq!(
            parse_strs(&["server", "command", "--function", "f"]).unwrap(),
            Command::Server(ServerAction::Command, false, Some("f".into()))
        );
        // `--yes` belongs to `start` only (AGL-015, D16).
        assert_eq!(
            parse_strs(&["server", "start", "--yes"]).unwrap(),
            Command::Server(ServerAction::Start, true, None)
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
        // `--function <name>` of `start` and `status` (AGL-035, D3).
        for action in [ServerAction::Start, ServerAction::Status] {
            let word = if action == ServerAction::Start { "start" } else { "status" };
            assert_eq!(
                parse_strs(&["server", word, "--function", "p"]).unwrap(),
                Command::Server(action, false, Some("p".into()))
            );
            assert!(matches!(
                parse_strs(&["server", word, "--function"]),
                Err(Error::MissingValue(o)) if o == "--function"
            ));
            assert!(matches!(
                parse_strs(&["server", word, "--function", "p", "--function", "q"]),
                Err(Error::RepeatedOption(o)) if o == "--function"
            ));
        }
        assert_eq!(
            parse_strs(&["server", "start", "--yes", "--function", "p"]).unwrap(),
            Command::Server(ServerAction::Start, true, Some("p".into()))
        );
        assert!(matches!(
            parse_strs(&["server", "stop", "--function", "p"]),
            Err(Error::UnknownOption(o)) if o == "--function"
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
    fn search_commands() {
        // AGL-043 F20.
        assert_eq!(
            parse_strs(&["search", "login"]).unwrap(),
            Command::Search(SearchAction::Login)
        );
        assert!(matches!(
            parse_strs(&["search"]),
            Err(Error::MissingValue(word)) if word == "search"
        ));
        assert!(matches!(
            parse_strs(&["search", "logout"]),
            Err(Error::UnknownOption(word)) if word == "logout"
        ));
        assert!(matches!(
            parse_strs(&["search", "login", "x"]),
            Err(Error::UnknownOption(word)) if word == "x"
        ));
    }

    #[test]
    fn memory_commands() {
        // AGL-060, D2, D3.
        assert_eq!(
            parse_strs(&["memory", "list"]).unwrap(),
            Command::Memory(MemoryAction::List)
        );
        assert_eq!(
            parse_strs(&["memory", "show", "tea"]).unwrap(),
            Command::Memory(MemoryAction::Show { name: "tea".into() })
        );
        for words in [&["memory", "remove", "tea"][..], &["memory", "remove", "tea", "--yes"], &["memory", "remove", "--yes", "tea"]] {
            assert_eq!(
                parse_strs(words).unwrap(),
                Command::Memory(MemoryAction::Remove {
                    name: "tea".into(),
                    yes: words.len() == 4,
                })
            );
        }
        for (words, word) in [
            (&["memory"][..], "memory"),
            (&["memory", "show"], "show"),
            (&["memory", "remove"], "remove"),
            (&["memory", "remove", "--yes"], "remove"),
        ] {
            assert!(
                matches!(parse_strs(words), Err(Error::MissingValue(o)) if o == word),
                "{words:?}"
            );
        }
        for (words, word) in [
            (&["memory", "add"][..], "add"),
            (&["memory", "list", "x"], "x"),
            (&["memory", "list", "--yes"], "--yes"),
            (&["memory", "show", "a", "b"], "b"),
            (&["memory", "show", "a", "--yes"], "--yes"),
            (&["memory", "remove", "a", "b"], "b"),
            (&["memory", "remove", "a", "--force"], "--force"),
        ] {
            assert!(
                matches!(parse_strs(words), Err(Error::UnknownOption(o)) if o == word),
                "{words:?}"
            );
        }
        assert!(matches!(
            parse_strs(&["memory", "remove", "a", "--yes", "--yes"]),
            Err(Error::RepeatedOption(o)) if o == "--yes"
        ));
        assert_eq!(parse_strs(&["memory", "--help"]).unwrap(), Command::Help);
        // Only the first word is a command; a message may say "memory".
        assert_eq!(
            parse_strs(&["-p", "memory"]).unwrap(),
            Command::Run(Args {
                prompt: Some("memory".into()),
                ..Args::default()
            })
        );
    }

    #[test]
    fn function_commands() {
        // AGL-036, D3.
        assert_eq!(
            parse_strs(&["function", "list"]).unwrap(),
            Command::Function(FunctionAction::List)
        );
        assert_eq!(
            parse_strs(&["function", "use", "p"]).unwrap(),
            Command::Function(FunctionAction::Use { name: "p".into(), user: false })
        );
        assert_eq!(
            parse_strs(&["function", "use", "p", "--user"]).unwrap(),
            Command::Function(FunctionAction::Use { name: "p".into(), user: true })
        );
        assert!(matches!(
            parse_strs(&["function", "use", "p", "--user", "--user"]),
            Err(Error::RepeatedOption(o)) if o == "--user"
        ));
        assert!(matches!(
            parse_strs(&["function", "use"]),
            Err(Error::MissingValue(o)) if o == "use"
        ));
        assert!(matches!(
            parse_strs(&["function", "list", "--user"]),
            Err(Error::UnknownOption(o)) if o == "--user"
        ));
        assert!(matches!(
            parse_strs(&["function"]),
            Err(Error::MissingValue(o)) if o == "function"
        ));
        assert!(matches!(
            parse_strs(&["function", "show"]),
            Err(Error::UnknownOption(o)) if o == "show"
        ));
        assert_eq!(parse_strs(&["function", "--help"]).unwrap(), Command::Help);
        // Only the first word is a command; a message may say "function".
        assert_eq!(
            parse_strs(&["-p", "function"]).unwrap(),
            Command::Run(Args {
                prompt: Some("function".into()),
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
