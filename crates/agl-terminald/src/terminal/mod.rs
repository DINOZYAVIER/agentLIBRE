pub use agl_terminal::environment;
pub use agl_terminal::history;
pub mod registry;
pub mod shell;

pub use agl_terminal::{
    CommandCardSanitizer, InMemoryTerminalRepository, StoredTerminalRecord, TerminalOwner,
    TerminalRecord, TerminalRepository, TerminalReservation, TerminalState, TerminalTopologyId,
    terminal_slot_key, validate_terminal_replacement, validate_terminal_reservation,
};

pub use registry::{TerminalEnsureRequest, TerminalRegistry};
