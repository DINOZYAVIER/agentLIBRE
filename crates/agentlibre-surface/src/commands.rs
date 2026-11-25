//! One table of the slash commands (AGL-121, D9), read by the surfaces and
//! by `/help`: the name, who prints it, whether it may ask the human, which
//! surfaces take it. The dispatch of each command stays with its surface;
//! the table is the one record of them. With it, the words of `/memory` at a
//! prompt (AGL-127, D7).

use agentlibre_proto as proto;

/// A surface that takes a slash command.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Surfaces(u8);

pub const REPL: Surfaces = Surfaces(1);
pub const STDIN: Surfaces = Surfaces(2);
pub const BOTH: Surfaces = Surfaces(3);

impl Surfaces {
    fn has(self, other: Surfaces) -> bool {
        self.0 & other.0 != 0
    }
}

/// One line of the table. The columns of a surface that does not need
/// one yet are data, not dead code (D9).
#[allow(dead_code)]
pub struct Slash {
    /// The command's name, with the slash.
    pub name: &'static str,
    /// Whether the surface prints the text itself (`/help`, `/status`)
    /// or the run says the command.
    pub prints: bool,
    /// Whether the command may ask the human: `/memory remove` asks
    /// `[y/N]` in the REPL and demands `--yes` in a session without a
    /// terminal (AGL-060, D7).
    pub asks: bool,
    /// Which surfaces take it; the others answer `unknown command`.
    pub surfaces: Surfaces,
}

/// The slash commands, in the order `/help` lists them (AGL-121, D9).
pub const SLASH: [Slash; 13] = [
    Slash { name: "/quit", prints: false, asks: false, surfaces: BOTH },
    Slash { name: "/exit", prints: false, asks: false, surfaces: BOTH },
    Slash { name: "/help", prints: true, asks: false, surfaces: BOTH },
    Slash { name: "/status", prints: true, asks: false, surfaces: BOTH },
    Slash { name: "/think", prints: false, asks: false, surfaces: REPL },
    Slash { name: "/attach", prints: false, asks: false, surfaces: BOTH },
    Slash { name: "/remember", prints: false, asks: false, surfaces: BOTH },
    Slash { name: "/memory", prints: false, asks: true, surfaces: BOTH },
    Slash { name: "/rename", prints: false, asks: false, surfaces: BOTH },
    Slash { name: "/new", prints: false, asks: false, surfaces: BOTH },
    Slash { name: "/compact", prints: false, asks: false, surfaces: BOTH },
    Slash { name: "/function", prints: false, asks: false, surfaces: REPL },
    Slash { name: "/plane", prints: false, asks: false, surfaces: REPL },
];

/// Whether the surface `surface` takes the command `name`.
pub fn takes(name: &str, surface: Surfaces) -> bool {
    SLASH.iter().any(|cmd| cmd.name == name && cmd.surfaces.has(surface))
}

/// What `/memory` asks for at a prompt (AGL-060, D2, D5; AGL-127, D7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Memory {
    List,
    Show { name: String },
    /// `yes`: no question (AGL-060, D3).
    Remove { name: String, yes: bool },
}

/// Why the words after `/memory` are no `Memory`: the word that is missing
/// or that is not taken, or given twice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Usage {
    /// What the line lacks: `memory`, `show` or `remove`.
    Missing(String),
    /// An action, an option or a word that is not taken.
    Unknown(String),
    /// An option given twice.
    Repeated(String),
}

/// Reads the words after `memory`, of the command line and of `/memory`.
pub fn memory(words: impl IntoIterator<Item = String>) -> Result<Memory, Usage> {
    let mut words = words.into_iter();
    let action = words.next().ok_or_else(|| Usage::Missing("memory".into()))?;
    let parsed = match action.as_str() {
        "list" => Memory::List,
        "show" => Memory::Show {
            name: words.next().ok_or_else(|| Usage::Missing("show".into()))?,
        },
        "remove" => {
            let mut name = None;
            let mut yes = false;
            for word in words.by_ref() {
                match word.as_str() {
                    "--yes" if yes => return Err(Usage::Repeated(word)),
                    "--yes" => yes = true,
                    _ if word.starts_with('-') || name.is_some() => {
                        return Err(Usage::Unknown(word));
                    }
                    _ => name = Some(word),
                }
            }
            Memory::Remove {
                name: name.ok_or_else(|| Usage::Missing("remove".into()))?,
                yes,
            }
        }
        _ => return Err(Usage::Unknown(action)),
    };
    match words.next() {
        Some(extra) => Err(Usage::Unknown(extra)),
        None => Ok(parsed),
    }
}

/// What a `/memory` that is a look asks of the session: `list` and `show`;
/// a removal is not a look, and asks for the list (AGL-130, D4).
pub fn look_of(memory: Memory) -> proto::Look {
    match memory {
        Memory::Show { name } => proto::Look::Show { name },
        _ => proto::Look::List,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn memory_reads_its_three_actions() {
        assert_eq!(memory(words("list")), Ok(Memory::List));
        assert_eq!(memory(words("show tea")), Ok(Memory::Show { name: "tea".into() }));
        assert_eq!(
            memory(words("remove tea")),
            Ok(Memory::Remove { name: "tea".into(), yes: false })
        );
        assert_eq!(
            memory(words("remove tea --yes")),
            Ok(Memory::Remove { name: "tea".into(), yes: true })
        );
        assert_eq!(
            memory(words("remove --yes tea")),
            Ok(Memory::Remove { name: "tea".into(), yes: true })
        );
    }

    #[test]
    fn memory_says_what_is_wrong_with_the_words() {
        assert_eq!(memory(words("")), Err(Usage::Missing("memory".into())));
        assert_eq!(memory(words("show")), Err(Usage::Missing("show".into())));
        assert_eq!(memory(words("remove")), Err(Usage::Missing("remove".into())));
        assert_eq!(memory(words("remove --yes")), Err(Usage::Missing("remove".into())));
        assert_eq!(memory(words("forget tea")), Err(Usage::Unknown("forget".into())));
        assert_eq!(memory(words("list now")), Err(Usage::Unknown("now".into())));
        assert_eq!(memory(words("remove tea two")), Err(Usage::Unknown("two".into())));
        assert_eq!(memory(words("remove tea -x")), Err(Usage::Unknown("-x".into())));
        assert_eq!(
            memory(words("remove tea --yes --yes")),
            Err(Usage::Repeated("--yes".into()))
        );
    }

    #[test]
    fn a_look_is_what_the_session_is_asked() {
        assert_eq!(look_of(Memory::List), proto::Look::List);
        assert_eq!(
            look_of(Memory::Show { name: "tea".into() }),
            proto::Look::Show { name: "tea".into() }
        );
    }

    #[test]
    fn a_session_without_a_terminal_takes_no_repl_command() {
        assert!(takes("/status", STDIN));
        assert!(!takes("/think", STDIN));
        assert!(!takes("/function", STDIN));
        assert!(!takes("/plane", STDIN));
        assert!(!takes("/nope", STDIN));
        assert!(takes("/think", REPL));
        assert!(takes("/plane", REPL));
    }
}
