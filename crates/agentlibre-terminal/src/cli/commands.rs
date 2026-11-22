//! One table of the slash commands (AGL-121, D9), read by both the REPL
//! and `lines.rs`: the name, who prints it, whether it may ask the human,
//! which surfaces take it. The dispatch of each command stays with its
//! surface; the table is the one record of them.

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::COMMANDS;

    /// The table names what `/help` lists, in its order (AGL-121, D9).
    #[test]
    fn the_table_names_the_help_list() {
        let names: Vec<&str> = SLASH.iter().map(|cmd| cmd.name).collect();
        assert_eq!(names, COMMANDS.to_vec());
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
