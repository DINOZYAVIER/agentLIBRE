pub use agl_terminal::environment;
pub use agl_terminal::history;
pub mod registry;
pub mod repository;
pub mod shell;

pub use agl_terminal::CommandCardSanitizer;

pub use registry::{
    TerminalEnsureRequest, TerminalOwner, TerminalRecord, TerminalRegistry, TerminalState,
};
pub use repository::{
    InMemoryTerminalRepository, StoredTerminalRecord, TerminalRepository, TerminalReservation,
    terminal_slot_key, validate_terminal_replacement, validate_terminal_reservation,
};
