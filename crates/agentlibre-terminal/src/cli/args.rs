//! Command-line arguments (D51, D53, D146).

use std::ffi::OsString;

use crate::backend::ReasoningLevel;
use crate::cli::bridge::BridgeAction;
use crate::cli::forge::ForgeAction;
use crate::error::Error;

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Print the help of the program or of one command (AGL-061, D3).
    Help(HelpTopic),
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
    /// `agl forge …` (AGL-007), parsed by the forge surface before any
    /// settings load (AGL-061, D1).
    Forge(ForgeAction),
    /// `agl function list|use` (AGL-036, D3).
    Function(FunctionAction),
    /// `agl search login` (AGL-043 F20).
    Search(SearchAction),
    /// `agl memory list|show|remove` (AGL-060).
    Memory(MemoryAction),
    /// `agl plane new|list|show|rename|add|remove|delete` (AGL-077, D11).
    Plane(PlaneAction),
    /// `agl session show <id|name|last>` (AGL-063, D3).
    Session(SessionShow),
    /// `agl forge|memory|function|session … --plane <name>`: the command on
    /// the plane's store (AGL-077, D9, D11).
    InPlane { plane: String, command: Box<Command> },
    /// `agl daemon`: the planes served on the socket (AGL-076, D6).
    Daemon,
    /// `agl notify`: the running sessions told on the desktop (AGL-074,
    /// D2).
    Notify,
    /// `agl bridge start|stop|status|pair|revoke|list` (AGL-083, D19).
    Bridge(BridgeAction),
}

/// What `--help` shows (AGL-061, D3): the help of the program, or of one
/// command alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelpTopic {
    Program,
    Init,
    Server,
    Function,
    Search,
    Memory,
    Plane,
    Session,
    Daemon,
    Notify,
    Bridge,
    Forge,
}

/// `agl session show <id|name|last>` (AGL-063, D3): the transcript of a
/// session file, with no server and no model; `verbose` prints the lines
/// the transcript skips (the observer's and the non-events).
#[derive(Debug, PartialEq, Eq)]
pub struct SessionShow {
    pub key: String,
    pub verbose: bool,
}

pub use crate::memory::MemoryAction;

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

/// `agl plane …` (AGL-077, D11): a directory is the current one when it
/// is not named.
#[derive(Debug, PartialEq, Eq)]
pub enum PlaneAction {
    /// `new <name> [--empty]`: with the current directory, or none.
    New { name: String, empty: bool },
    List,
    /// `show [<name>]`: none, the planes of the current directory.
    Show { name: Option<String> },
    Rename { old: String, new: String },
    Add { name: String, dir: Option<String> },
    Remove { name: String, dir: Option<String> },
    /// `delete <name> [--yes]` (D14).
    Delete { name: String, yes: bool },
    /// `flow list|status|start|stop|reset [<flow>] | message <flow> <event>`
    /// (AGL-118, D2, D7, D21): the
    /// plane of the current directory, the verb of the flow's own machine.
    Flow(crate::plane::flow::Verb),
}

/// Reads the words after `plane flow` (AGL-118, D2, D7): the verb, and the
/// flow's name where the verb needs one.
fn flow_action(mut words: impl Iterator<Item = String>) -> Result<PlaneAction, Error> {
    let verb = words.next().ok_or_else(|| Error::MissingValue("flow".into()))?;
    let name = words.next();
    use crate::plane::flow::Verb;
    // `message <flow> <event>`: the one delivery verb of the nudges (D21).
    if verb == "message" {
        let event = words.next().ok_or_else(|| Error::MissingValue("event".into()))?;
        if let Some(extra) = words.next() {
            return Err(Error::UnknownOption(extra));
        }
        return Ok(PlaneAction::Flow(Verb::Message {
            name: name.ok_or_else(|| Error::MissingValue("message".into()))?,
            event,
        }));
    }
    if let Some(extra) = words.next() {
        return Err(Error::UnknownOption(extra));
    }
    let need = || name.clone().ok_or_else(|| Error::MissingValue(verb.clone()));
    Ok(PlaneAction::Flow(match verb.as_str() {
        "list" => Verb::List,
        "start" => Verb::Start { name: need()? },
        "stop" => Verb::Stop { name: need()? },
        "status" => Verb::Status { name: need()? },
        "reset" => Verb::Reset { name: need()? },
        _ => return Err(Error::UnknownOption(verb)),
    }))
}

/// Reads the words after `plane`.
fn plane_args(args: impl Iterator<Item = OsString>) -> Result<Command, Error> {
    let words = args.map(text).collect::<Result<Vec<_>, _>>()?;
    let mut words = words.into_iter();
    let action = words
        .next()
        .ok_or_else(|| Error::MissingValue("plane".into()))?;
    // `agl plane flow <verb> [<flow>]` (AGL-118, D2, D7, D21): the plane is the
    // current directory's, and the verb is the flow's own.
    if action == "flow" {
        return flow_action(words).map(Command::Plane);
    }
    // The one flag an action has, when it has one.
    let flag_name = match action.as_str() {
        "new" => Some("--empty"),
        "delete" => Some("--yes"),
        _ => None,
    };
    let mut values = Vec::new();
    let mut flag = false;
    for word in words {
        match flag_name {
            Some(name) if word == name && flag => return Err(Error::RepeatedOption(word)),
            Some(name) if word == name => flag = true,
            _ if word.starts_with('-') => return Err(Error::UnknownOption(word)),
            _ => values.push(word),
        }
    }
    let (least, most) = match action.as_str() {
        "list" => (0, 0),
        "show" => (0, 1),
        "new" | "delete" => (1, 1),
        "rename" => (2, 2),
        "add" | "remove" => (1, 2),
        _ => return Err(Error::UnknownOption(action)),
    };
    if values.len() < least {
        return Err(Error::MissingValue(action));
    }
    if let Some(extra) = values.get(most) {
        return Err(Error::UnknownOption(extra.clone()));
    }
    let mut values = values.into_iter();
    let mut next = || values.next();
    let parsed = match action.as_str() {
        "new" => PlaneAction::New {
            name: next().unwrap_or_default(),
            empty: flag,
        },
        "list" => PlaneAction::List,
        "show" => PlaneAction::Show { name: next() },
        "rename" => PlaneAction::Rename {
            old: next().unwrap_or_default(),
            new: next().unwrap_or_default(),
        },
        "add" => PlaneAction::Add {
            name: next().unwrap_or_default(),
            dir: next(),
        },
        "remove" => PlaneAction::Remove {
            name: next().unwrap_or_default(),
            dir: next(),
        },
        _ => PlaneAction::Delete {
            name: next().unwrap_or_default(),
            yes: flag,
        },
    };
    Ok(Command::Plane(parsed))
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
    /// `agl function show <name>` (AGL-102, D2).
    Show { name: String },
    /// `agl function new <name> [from]` (AGL-102, D3).
    New { name: String, from: Option<String> },
    /// `agl function check <name|file>` (AGL-109, D5): loads what the
    /// name resolves to, or the given file alone, and says whether it
    /// loads; nvim may ask it on every save.
    Check(String),
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
    /// `--plane <name>`: the plane of the run (AGL-077, D6).
    pub plane: Option<String>,
    /// `--think <level>`: the model's reasoning level (AGL-013).
    pub reasoning: Option<ReasoningLevel>,
    /// `--yes`: start the server of the project even when another
    /// project's server runs, without asking (AGL-015, D13, D16).
    pub yes: bool,
    /// `--attach <path>`, repeatable: files that go with the `-p` message
    /// (AGL-014, D21).
    pub attach: Vec<String>,
}

/// An argument as text.
fn text(arg: OsString) -> Result<String, Error> {
    arg.into_string().map_err(|_| Error::InvalidUnicode)
}

/// Parses the arguments after the program name. Each option takes the
/// next argument as its value, even one that starts with `-`.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, Error> {
    let mut args = args.into_iter().peekable();
    let topic = match args.peek().and_then(|arg| arg.to_str()) {
        Some("init") => HelpTopic::Init,
        Some("server") => HelpTopic::Server,
        Some("function") => HelpTopic::Function,
        Some("search") => HelpTopic::Search,
        Some("memory") => HelpTopic::Memory,
        Some("plane") => HelpTopic::Plane,
        Some("session") => HelpTopic::Session,
        Some("daemon") => HelpTopic::Daemon,
        Some("notify") => HelpTopic::Notify,
        Some("bridge") => HelpTopic::Bridge,
        Some("forge") => HelpTopic::Forge,
        // Only the first word is a command; a message may say "forge".
        _ => return run_args(args),
    };
    let word = args.next().map(text).transpose()?.unwrap_or_default();
    // `--plane <name>` of the commands that act on a store (AGL-077, D11).
    let mut rest: Vec<OsString> = args.collect();
    let plane = match topic {
        HelpTopic::Forge | HelpTopic::Memory | HelpTopic::Function | HelpTopic::Session => {
            take_plane(&mut rest)?
        }
        _ => None,
    };
    let args = rest.into_iter();
    let parsed = match topic {
        HelpTopic::Init => init_args(args),
        HelpTopic::Server => server_args(args),
        HelpTopic::Function => function_args(args),
        HelpTopic::Search => search_args(args),
        HelpTopic::Memory => {
            let words = args.map(text).collect::<Result<Vec<_>, _>>()?;
            memory_action(words).map(Command::Memory)
        }
        HelpTopic::Plane => plane_args(args),
        HelpTopic::Session => session_args(args),
        HelpTopic::Daemon => daemon_args(args),
        HelpTopic::Notify => notify_args(args),
        HelpTopic::Bridge => bridge_args(args),
        // The forge surface parses its own arguments (AGL-007), here, so
        // that its help needs no settings (AGL-061, D1).
        HelpTopic::Forge => crate::cli::forge::parse(&args.collect::<Vec<_>>()).map(Command::Forge),
        HelpTopic::Program => unreachable!("the program is no command"),
    };
    // No action, or `-h`/`--help` where the command takes no value, is
    // the help of the command (AGL-061, D2, D4, D6): a value comes first,
    // so `forge show --help` still names an artifact `--help`.
    match parsed {
        Err(Error::MissingValue(missing)) if missing == word => Ok(Command::Help(topic)),
        Err(Error::UnknownOption(option)) if option == "-h" || option == "--help" => {
            Ok(Command::Help(topic))
        }
        Ok(Command::Help(topic)) => Ok(Command::Help(topic)),
        Ok(command) => Ok(match plane {
            Some(plane) => Command::InPlane {
                plane,
                command: Box::new(command),
            },
            None => command,
        }),
        parsed => parsed,
    }
}

/// Takes `--plane <name>` out of `args`, wherever it stands.
fn take_plane(args: &mut Vec<OsString>) -> Result<Option<String>, Error> {
    let Some(at) = args.iter().position(|arg| arg == "--plane") else {
        return Ok(None);
    };
    if at + 1 >= args.len() {
        return Err(Error::MissingValue("--plane".into()));
    }
    let name = text(args.remove(at + 1))?;
    args.remove(at);
    if args.iter().any(|arg| arg == "--plane") {
        return Err(Error::RepeatedOption("--plane".into()));
    }
    Ok(Some(name))
}

/// `agl daemon`: no arguments (AGL-076, D6).
fn daemon_args(mut args: impl Iterator<Item = OsString>) -> Result<Command, Error> {
    match args.next() {
        None => Ok(Command::Daemon),
        Some(other) => Err(Error::UnknownOption(text(other)?)),
    }
}

/// `agl notify`: no arguments (AGL-074, D2).
fn notify_args(mut args: impl Iterator<Item = OsString>) -> Result<Command, Error> {
    match args.next() {
        None => Ok(Command::Notify),
        Some(other) => Err(Error::UnknownOption(text(other)?)),
    }
}

/// `agl bridge start|stop|status|list`, `pair <name>`, `revoke <name>`
/// (AGL-083, D19).
fn bridge_args(mut args: impl Iterator<Item = OsString>) -> Result<Command, Error> {
    let word = args
        .next()
        .map(text)
        .transpose()?
        .ok_or_else(|| Error::MissingValue("bridge".into()))?;
    let mut name = || {
        args.next()
            .ok_or_else(|| Error::MissingValue(word.clone()))
            .and_then(text)
    };
    let action = match word.as_str() {
        "start" => BridgeAction::Start,
        "stop" => BridgeAction::Stop,
        "status" => BridgeAction::Status,
        "list" => BridgeAction::List,
        "pair" => BridgeAction::Pair { name: name()? },
        "revoke" => BridgeAction::Revoke { name: name()? },
        _ => return Err(Error::UnknownOption(word)),
    };
    match args.next() {
        Some(extra) => Err(Error::UnknownOption(text(extra)?)),
        None => Ok(Command::Bridge(action)),
    }
}

/// `agl init`: no arguments.
fn init_args(mut args: impl Iterator<Item = OsString>) -> Result<Command, Error> {
    match args.next() {
        None => Ok(Command::Init),
        Some(other) => Err(Error::UnknownOption(text(other)?)),
    }
}

/// `agl function list` or `agl function use <name> [--user]`,
/// `agl function show <name>`, `agl function new <name> [from]`,
/// `agl function check <name|file>` (AGL-102, AGL-109).
fn function_args(mut args: impl Iterator<Item = OsString>) -> Result<Command, Error> {
    let mut action = match args.next().map(text).transpose()?.as_deref() {
        Some("list") => FunctionAction::List,
        Some("show") => FunctionAction::Show {
            name: args
                .next()
                .ok_or_else(|| Error::MissingValue("show".into()))
                .and_then(text)?,
        },
        Some("new") => {
            let name = args
                .next()
                .ok_or_else(|| Error::MissingValue("new".into()))
                .and_then(text)?;
            let from = args.next().map(text).transpose()?;
            FunctionAction::New { name, from }
        }
        Some("check") => FunctionAction::Check(
            args.next()
                .ok_or_else(|| Error::MissingValue("check".into()))
                .and_then(text)?,
        ),
        Some("use") => {
            let name = args
                .next()
                .ok_or_else(|| Error::MissingValue("use".into()))
                .and_then(text)?;
            FunctionAction::Use { name, user: false }
        }
        Some(other) => return Err(Error::UnknownOption(other.into())),
        None => return Err(Error::MissingValue("function".into())),
    };
    for extra in args {
        match (text(extra)?.as_str(), &mut action) {
            ("--user", FunctionAction::Use { user, .. }) if !*user => *user = true,
            ("--user", FunctionAction::Use { .. }) => {
                return Err(Error::RepeatedOption("--user".into()));
            }
            (other, _) => return Err(Error::UnknownOption(other.into())),
        }
    }
    Ok(Command::Function(action))
}

/// `agl search login`.
fn search_args(mut args: impl Iterator<Item = OsString>) -> Result<Command, Error> {
    let action = match args.next().map(text).transpose()?.as_deref() {
        Some("login") => SearchAction::Login,
        Some(other) => return Err(Error::UnknownOption(other.into())),
        None => return Err(Error::MissingValue("search".into())),
    };
    if let Some(extra) = args.next() {
        return Err(Error::UnknownOption(text(extra)?));
    }
    Ok(Command::Search(action))
}

/// `agl session show <id|name|last> [--verbose]` (AGL-063, D3).
fn session_args(mut args: impl Iterator<Item = OsString>) -> Result<Command, Error> {
    match args.next().map(text).transpose()?.as_deref() {
        Some("show") => {}
        Some(other) => return Err(Error::UnknownOption(other.into())),
        None => return Err(Error::MissingValue("session".into())),
    }
    let key = args
        .next()
        .ok_or_else(|| Error::MissingValue("show".into()))
        .and_then(text)?;
    let mut verbose = false;
    for extra in args {
        match text(extra)?.as_str() {
            "--verbose" if !verbose => verbose = true,
            "--verbose" => return Err(Error::RepeatedOption("--verbose".into())),
            other => return Err(Error::UnknownOption(other.into())),
        }
    }
    Ok(Command::Session(SessionShow { key, verbose }))
}

/// `agl server start|stop|status|command` and their options.
fn server_args(mut args: impl Iterator<Item = OsString>) -> Result<Command, Error> {
    let action = match args.next().map(text).transpose()?.as_deref() {
        Some("start") => ServerAction::Start,
        Some("stop") => ServerAction::Stop,
        Some("status") => ServerAction::Status,
        Some("command") => ServerAction::Command,
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
    Ok(Command::Server(action, yes, function))
}

/// The options of a run: `-p`, `--resume`, `--function`, `--think`,
/// `--attach`, `--yes`.
fn run_args(args: impl Iterator<Item = OsString>) -> Result<Command, Error> {
    let mut parsed = Args::default();
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
            "-h" | "--help" => return Ok(Command::Help(HelpTopic::Program)),
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
            "--plane" => &mut parsed.plane,
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
    fn the_flow_verbs_of_a_plane() {
        // AGL-118, D2, D7: `agl plane flow <verb> [<flow>]`, the plane of the
        // current directory.
        use crate::plane::flow::Verb;
        assert_eq!(
            parse_strs(&["plane", "flow", "status", "build"]).unwrap(),
            Command::Plane(PlaneAction::Flow(Verb::Status { name: "build".into() }))
        );
        assert_eq!(
            parse_strs(&["plane", "flow", "list"]).unwrap(),
            Command::Plane(PlaneAction::Flow(Verb::List))
        );
        assert!(matches!(
            parse_strs(&["plane", "flow", "start"]),
            Err(Error::MissingValue(name)) if name == "start"
        ));
        assert!(matches!(
            parse_strs(&["plane", "flow", "walk"]),
            Err(Error::UnknownOption(word)) if word == "walk"
        ));
    }

    #[test]
    fn no_arguments_start_the_repl() {
        assert_eq!(parse_strs(&[]).unwrap(), Command::Run(Args::default()));
    }

    #[test]
    fn the_commands_on_a_store_take_a_plane() {
        // AGL-077, D11: `--plane <name>` wherever it stands.
        assert_eq!(
            parse_strs(&["forge", "list", "--plane", "work"]).unwrap(),
            Command::InPlane {
                plane: "work".into(),
                command: Box::new(Command::Forge(ForgeAction::List)),
            }
        );
        assert_eq!(
            parse_strs(&["memory", "--plane", "work", "show", "tea"]).unwrap(),
            Command::InPlane {
                plane: "work".into(),
                command: Box::new(Command::Memory(MemoryAction::Show { name: "tea".into() })),
            }
        );
        assert!(matches!(
            parse_strs(&["session", "show", "last", "--plane"]),
            Err(Error::MissingValue(_))
        ));
        assert!(matches!(
            parse_strs(&["function", "list", "--plane", "a", "--plane", "b"]),
            Err(Error::RepeatedOption(_))
        ));
        // Not a command on a store: an ordinary run's option.
        assert!(matches!(parse_strs(&["--plane", "work"]), Ok(Command::Run(_))));
    }

    #[test]
    fn all_options() {
        let command = parse_strs(&["--function", "coder", "-p", "hi there", "--resume", "id", "--plane", "work"]);
        assert_eq!(
            command.unwrap(),
            Command::Run(Args {
                prompt: Some("hi there".into()),
                resume: Some("id".into()),
                list_sessions: false,
                function: Some("coder".into()),
                plane: Some("work".into()),
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
        assert_eq!(parse_strs(&["-h"]).unwrap(), Command::Help(HelpTopic::Program));
        assert_eq!(parse_strs(&["-p", "x", "--help"]).unwrap(), Command::Help(HelpTopic::Program));
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
        assert_eq!(parse_strs(&["server", "--help"]).unwrap(), Command::Help(HelpTopic::Server));
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
            (&["memory", "show"][..], "show"),
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
        assert_eq!(parse_strs(&["memory", "--help"]).unwrap(), Command::Help(HelpTopic::Memory));
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
    fn session_commands() {
        // AGL-063, D3.
        assert_eq!(
            parse_strs(&["session", "show", "last"]).unwrap(),
            Command::Session(SessionShow {
                key: "last".into(),
                verbose: false
            })
        );
        assert_eq!(
            parse_strs(&["session", "show", "01a1", "--verbose"]).unwrap(),
            Command::Session(SessionShow {
                key: "01a1".into(),
                verbose: true
            })
        );
        assert!(matches!(
            parse_strs(&["session", "show"]),
            Err(Error::MissingValue(o)) if o == "show"
        ));
        // No action is the help of the command (AGL-061, D2, D4, D6).
        assert_eq!(
            parse_strs(&["session"]).unwrap(),
            Command::Help(HelpTopic::Session)
        );
        assert!(matches!(
            parse_strs(&["session", "log", "last"]),
            Err(Error::UnknownOption(o)) if o == "log"
        ));
        assert!(matches!(
            parse_strs(&["session", "show", "last", "more"]),
            Err(Error::UnknownOption(o)) if o == "more"
        ));
        assert!(matches!(
            parse_strs(&["session", "show", "last", "--verbose", "--verbose"]),
            Err(Error::RepeatedOption(o)) if o == "--verbose"
        ));
        assert_eq!(
            parse_strs(&["session", "--help"]).unwrap(),
            Command::Help(HelpTopic::Session)
        );
        // Only the first word is a command; a message may say "session".
        assert_eq!(
            parse_strs(&["-p", "session"]).unwrap(),
            Command::Run(Args {
                prompt: Some("session".into()),
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
        assert_eq!(
            parse_strs(&["function", "show", "p"]).unwrap(),
            Command::Function(FunctionAction::Show { name: "p".into() })
        );
        assert!(matches!(
            parse_strs(&["function", "show"]),
            Err(Error::MissingValue(o)) if o == "show"
        ));
        assert_eq!(
            parse_strs(&["function", "new", "p"]).unwrap(),
            Command::Function(FunctionAction::New { name: "p".into(), from: None })
        );
        assert_eq!(
            parse_strs(&["function", "new", "p", "q"]).unwrap(),
            Command::Function(FunctionAction::New { name: "p".into(), from: Some("q".into()) })
        );
        assert_eq!(
            parse_strs(&["function", "check", "/tmp/p.nix"]).unwrap(),
            Command::Function(FunctionAction::Check("/tmp/p.nix".into()))
        );
        assert!(matches!(
            parse_strs(&["function", "check"]),
            Err(Error::MissingValue(o)) if o == "check"
        ));
        assert_eq!(parse_strs(&["function", "--help"]).unwrap(), Command::Help(HelpTopic::Function));
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
    fn forge_commands() {
        assert_eq!(
            parse_strs(&["forge", "list"]).unwrap(),
            Command::Forge(ForgeAction::List)
        );
        assert_eq!(
            parse_strs(&["forge", "new", "task-spec", "--title", "t"]).unwrap(),
            Command::Forge(ForgeAction::New { title: "t".into(), name: None })
        );
        // The help needs no settings (AGL-061, D1).
        for words in [&["forge"][..], &["forge", "-h"], &["forge", "--help"]] {
            assert_eq!(parse_strs(words).unwrap(), Command::Help(HelpTopic::Forge), "{words:?}");
        }
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
        assert_eq!(parse_strs(&["init", "--help"]).unwrap(), Command::Help(HelpTopic::Init));
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
    fn bridge_commands() {
        // AGL-083, D19.
        for (word, action) in [
            ("start", BridgeAction::Start),
            ("stop", BridgeAction::Stop),
            ("status", BridgeAction::Status),
            ("list", BridgeAction::List),
        ] {
            assert_eq!(parse_strs(&["bridge", word]).unwrap(), Command::Bridge(action));
        }
        assert_eq!(
            parse_strs(&["bridge", "pair", "iphone"]).unwrap(),
            Command::Bridge(BridgeAction::Pair { name: "iphone".into() })
        );
        assert_eq!(
            parse_strs(&["bridge", "revoke", "iphone"]).unwrap(),
            Command::Bridge(BridgeAction::Revoke { name: "iphone".into() })
        );
        assert!(matches!(
            parse_strs(&["bridge", "pair"]),
            Err(Error::MissingValue(word)) if word == "pair"
        ));
        assert!(matches!(
            parse_strs(&["bridge", "start", "now"]),
            Err(Error::UnknownOption(word)) if word == "now"
        ));
        assert!(matches!(
            parse_strs(&["bridge", "open"]),
            Err(Error::UnknownOption(word)) if word == "open"
        ));
        for words in [&["bridge"][..], &["bridge", "-h"], &["bridge", "start", "--help"]] {
            assert_eq!(parse_strs(words).unwrap(), Command::Help(HelpTopic::Bridge), "{words:?}");
        }
    }

    #[test]
    fn the_help_of_a_command() {
        // AGL-061: no action (D2, D4), `-h` or `--help` in the place of the
        // action or of an option (D6), after the type of `forge new` (D5).
        let cases: [(&[&str], HelpTopic); 30] = [
            (&["init", "--help"], HelpTopic::Init),
            (&["init", "-h"], HelpTopic::Init),
            (&["server"], HelpTopic::Server),
            (&["server", "-h"], HelpTopic::Server),
            (&["server", "start", "--help"], HelpTopic::Server),
            (&["server", "stop", "--help"], HelpTopic::Server),
            (&["server", "status", "--function", "f", "--help"], HelpTopic::Server),
            (&["function"], HelpTopic::Function),
            (&["function", "--help"], HelpTopic::Function),
            (&["function", "list", "--help"], HelpTopic::Function),
            (&["function", "use", "p", "--help"], HelpTopic::Function),
            (&["search"], HelpTopic::Search),
            (&["search", "-h"], HelpTopic::Search),
            (&["search", "login", "--help"], HelpTopic::Search),
            (&["memory"], HelpTopic::Memory),
            (&["memory", "-h"], HelpTopic::Memory),
            (&["memory", "list", "--help"], HelpTopic::Memory),
            (&["memory", "show", "tea", "--help"], HelpTopic::Memory),
            (&["memory", "remove", "--help"], HelpTopic::Memory),
            (&["memory", "remove", "tea", "--help"], HelpTopic::Memory),
            (&["forge"], HelpTopic::Forge),
            (&["forge", "--help"], HelpTopic::Forge),
            (&["forge", "list", "--help"], HelpTopic::Forge),
            (&["forge", "show", "x", "-h"], HelpTopic::Forge),
            (&["forge", "range", "x", "--help"], HelpTopic::Forge),
            (&["forge", "new", "--help"], HelpTopic::Forge),
            (&["forge", "new", "-h"], HelpTopic::Forge),
            (&["forge", "new", "task-spec", "--help"], HelpTopic::Forge),
            (&["forge", "new", "task-spec", "--title", "t", "--help"], HelpTopic::Forge),
            (&["--help"], HelpTopic::Program),
        ];
        for (words, topic) in cases {
            assert_eq!(parse_strs(words).unwrap(), Command::Help(topic), "{words:?}");
        }
    }

    #[test]
    fn a_help_in_the_place_of_a_value_is_the_value() {
        // AGL-061, D6, D7.
        assert_eq!(
            parse_strs(&["memory", "show", "--help"]).unwrap(),
            Command::Memory(MemoryAction::Show { name: "--help".into() })
        );
        assert_eq!(
            parse_strs(&["function", "use", "--help"]).unwrap(),
            Command::Function(FunctionAction::Use { name: "--help".into(), user: false })
        );
        assert_eq!(
            parse_strs(&["server", "start", "--function", "--help"]).unwrap(),
            Command::Server(ServerAction::Start, false, Some("--help".into()))
        );
        assert_eq!(
            parse_strs(&["forge", "show", "--help"]).unwrap(),
            Command::Forge(ForgeAction::Show { name: "--help".into() })
        );
        assert_eq!(
            parse_strs(&["forge", "new", "task-spec", "--title", "--help"]).unwrap(),
            Command::Forge(ForgeAction::New { title: "--help".into(), name: None })
        );
        assert_eq!(
            parse_strs(&["forge", "diff", "x", "--help"]).unwrap(),
            Command::Forge(ForgeAction::Diff { name: "x".into(), commit: Some("--help".into()) })
        );
        // The options of the program keep the rule too (D7).
        assert_eq!(
            parse_strs(&["-p", "--help"]).unwrap(),
            Command::Run(Args { prompt: Some("--help".into()), ..Args::default() })
        );
        // A problem before the `--help` is still the error.
        assert!(matches!(
            parse_strs(&["memory", "list", "x", "--help"]),
            Err(Error::UnknownOption(o)) if o == "x"
        ));
        assert!(matches!(
            parse_strs(&["forge", "new", "doc", "--help"]),
            Err(Error::Core(crate::error::CoreError::Forge(crate::forge::error::Error::NewType { type_name }))) if type_name == "doc"
        ));
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
