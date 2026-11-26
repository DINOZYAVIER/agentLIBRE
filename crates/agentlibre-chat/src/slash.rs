//! The REPL's `/` commands in the chat (AGL-128, D5): each asks the daemon
//! what the REPL asks it, or says what the REPL says, in the words of the
//! shared catalog; the menu over the field offers them (KS-050).

use std::path::PathBuf;

use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::reasoning::ReasoningLevel;
use agentlibre_common::tr;
use agentlibre_proto::{self as proto, CommandLine, Request};
use agentlibre_surface::commands::{self, Memory, REPL};
use agentlibre_surface::words::{self, Remember};

/// What a line of the field that starts with `/` does.
#[derive(Debug, PartialEq)]
pub enum Slash {
    /// A text the chat shows, and the line the session's record keeps.
    Say { text: String, failed: bool, line: String },
    /// A request of the session; the daemon answers with its `done`.
    Ask(Request),
    /// The reasoning level of the next messages (`/think <level>`).
    Think(ReasoningLevel),
    /// A file to attach to the next message (`/attach <path>`).
    Attach(PathBuf),
    /// `/memory remove <name>` without `--yes`: asked first, as the REPL asks.
    Confirm { name: String },
    /// `/function` and `/plane` with no name: the list the daemon gives.
    List(Request),
    /// `/function <name>`, `/plane <name>`: checked, then a new session.
    Switch(Switch),
    /// `/new`: a new session.
    New,
    /// `/quit`, `/exit`: the window closes.
    Quit,
}

/// A switch to another Function or plane (AGL-036, D4; AGL-077, D12).
#[derive(Clone, Debug, PartialEq)]
pub enum Switch {
    Function(String),
    Plane(String),
}

/// What the command needs of the chat.
pub struct Here<'a> {
    pub lang: Lang,
    pub session: &'a str,
    pub dir: &'a str,
    pub plane: Option<&'a str>,
    pub busy: bool,
    pub reasoning: Option<ReasoningLevel>,
    /// What `/status` says (D134).
    pub facts: Option<&'a words::Facts>,
    pub info: &'a words::Info,
}

/// The commands the menu offers, in `/help`'s order, with what each does.
pub fn menu(lang: Lang) -> Vec<(&'static str, &'static str)> {
    commands::SLASH
        .iter()
        .filter(|cmd| commands::takes(cmd.name, REPL))
        .map(|cmd| (cmd.name, words::command(cmd.name, lang)))
        .collect()
}

/// What `line` does.
pub fn slash(line: &str, here: &Here) -> Slash {
    let lang = here.lang;
    let line = line.trim();
    let name = line.split_whitespace().next().unwrap_or(line);
    let rest = line[name.len()..].trim();
    let say = |text: String, failed: bool| Slash::Say { text, failed, line: line.to_owned() };
    // The commands that change the session wait for its turn's end.
    let busy = || say(tr!(lang, m::BUSY, name = name), true);
    if !commands::takes(name, REPL) {
        return say(tr!(lang, m::UNKNOWN_COMMAND, name = name), true);
    }
    match name {
        "/help" => say(words::help_text(lang, false), false),
        "/status" => match here.facts {
            Some(facts) => say(words::status_text(facts, here.info, here.reasoning, lang), false),
            None => say(tr!(lang, m::IDLE).to_string(), false),
        },
        "/think" => match rest.split_whitespace().next() {
            None => say(
                tr!(lang, m::THINK_LEVEL, level = words::reasoning_level(here.reasoning, lang)),
                false,
            ),
            Some(value) => match ReasoningLevel::parse(value) {
                Some(level) => Slash::Think(level),
                None => say(tr!(lang, m::REASONING_BAD, value = value), true),
            },
        },
        "/attach" => {
            let path = rest
                .strip_prefix('"')
                .and_then(|p| p.strip_suffix('"'))
                .or_else(|| rest.strip_prefix('\'').and_then(|p| p.strip_suffix('\'')))
                .unwrap_or(rest);
            if path.is_empty() {
                say(tr!(lang, m::ATTACH_USAGE).to_string(), true)
            } else {
                Slash::Attach(PathBuf::from(path))
            }
        }
        _ if here.busy => busy(),
        "/remember" => match words::parse_remember(rest) {
            Remember::Usage => say(tr!(lang, m::REMEMBER_USAGE).to_string(), true),
            Remember::Entry { name, text, workspace } => Slash::Ask(Request::Remember {
                session: here.session.to_owned(),
                name,
                text,
                workspace,
            }),
            // An ordinary message: the model names the entry (M37).
            Remember::Turn(message) => Slash::Ask(Request::Say {
                session: here.session.to_owned(),
                message,
                attached: Vec::new(),
                reasoning: here.reasoning.map(|level| level.name().to_owned()),
            }),
        },
        "/memory" => match commands::memory(rest.split_whitespace().map(str::to_owned)) {
            Err(_) => say(tr!(lang, m::MEMORY_USAGE).to_string(), true),
            Ok(Memory::Remove { name, yes: false }) => Slash::Confirm { name },
            Ok(Memory::Remove { name, yes: true }) => Slash::Ask(Request::Remove {
                session: here.session.to_owned(),
                name,
            }),
            Ok(memory) => Slash::Ask(Request::Look {
                session: here.session.to_owned(),
                look: commands::look_of(memory),
            }),
        },
        "/rename" if rest.is_empty() => say(tr!(lang, m::RENAME_USAGE).to_string(), true),
        "/rename" => Slash::Ask(Request::Rename {
            session: here.session.to_owned(),
            name: rest.to_owned(),
        }),
        "/new" => Slash::New,
        "/compact" => Slash::Ask(Request::Compact {
            session: here.session.to_owned(),
            reasoning: here.reasoning.map(|level| level.name().to_owned()),
        }),
        "/function" => match rest.split_whitespace().next() {
            None => Slash::List(Request::Functions {
                dir: here.dir.to_owned(),
                plane: here.plane.map(str::to_owned),
            }),
            Some(function) => Slash::Switch(Switch::Function(function.to_owned())),
        },
        "/plane" => match rest.split_whitespace().next() {
            None => Slash::List(Request::Planes { dir: here.dir.to_owned(), name: None }),
            Some(plane) => Slash::Switch(Switch::Plane(plane.to_owned())),
        },
        // `/quit` and `/exit`.
        _ => Slash::Quit,
    }
}

/// The request that checks a switch before the session is left: the
/// Function loads and fits, the plane is one of the directory's.
pub fn check(switch: &Switch, dir: &str, plane: Option<&str>) -> Request {
    match switch {
        Switch::Function(name) => Request::Function {
            dir: dir.to_owned(),
            plane: plane.map(str::to_owned),
            verb: proto::FunctionVerb::Check { name: name.clone() },
        },
        Switch::Plane(name) => Request::Planes { dir: dir.to_owned(), name: Some(name.clone()) },
    }
}

/// A command's line in the session's record (AGL-047, D5).
pub fn record(session: &str, line: &str, failed: bool, output: Option<String>) -> Request {
    Request::Record {
        session: session.to_owned(),
        lines: vec![CommandLine { line: line.to_owned(), failed, output }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn here(busy: bool) -> Here<'static> {
        static INFO: std::sync::OnceLock<words::Info> = std::sync::OnceLock::new();
        Here {
            lang: Lang::En,
            session: "s",
            dir: "/d",
            plane: None,
            busy,
            reasoning: None,
            facts: None,
            info: INFO.get_or_init(words::Info::default),
        }
    }

    #[test]
    fn the_commands_ask_what_the_repl_asks() {
        assert_eq!(slash("/rename tea", &here(false)), Slash::Ask(Request::Rename { session: "s".into(), name: "tea".into() }));
        assert_eq!(slash("/new", &here(false)), Slash::New);
        assert_eq!(slash("/memory remove tea", &here(false)), Slash::Confirm { name: "tea".into() });
        assert_eq!(
            slash("/memory remove tea --yes", &here(false)),
            Slash::Ask(Request::Remove { session: "s".into(), name: "tea".into() })
        );
        assert_eq!(slash("/think low", &here(false)), Slash::Think(ReasoningLevel::Low));
        assert_eq!(slash("/attach \"a b.png\"", &here(false)), Slash::Attach(PathBuf::from("a b.png")));
        assert_eq!(slash("/function f", &here(false)), Slash::Switch(Switch::Function("f".into())));
        assert!(matches!(slash("/plane", &here(false)), Slash::List(Request::Planes { name: None, .. })));
        assert_eq!(slash("/quit", &here(false)), Slash::Quit);
    }

    #[test]
    fn a_turn_running_or_a_wrong_line_is_said() {
        assert!(matches!(slash("/new", &here(true)), Slash::Say { failed: true, .. }));
        assert!(matches!(slash("/help", &here(true)), Slash::Say { failed: false, .. }));
        assert!(matches!(slash("/nope", &here(false)), Slash::Say { failed: true, .. }));
        assert!(matches!(slash("/rename", &here(false)), Slash::Say { failed: true, .. }));
        assert!(matches!(slash("/think much", &here(false)), Slash::Say { failed: true, .. }));
    }

    #[test]
    fn the_menu_is_the_repl_s_commands() {
        let names: Vec<&str> = menu(Lang::En).into_iter().map(|(name, _)| name).collect();
        assert!(names.contains(&"/function") && names.contains(&"/plane") && names.contains(&"/think"));
        assert_eq!(names[0], "/quit");
    }
}
