//! What can go wrong running a Function's machine (AGL-121, D66).

use agentlibre_common::i18n::{Lang, Msg};
use agentlibre_common::tr;

/// The machine's own error, wrapped by the core's `Error::Machine`
/// (AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// The session continues a turn in a state the Function no longer has.
    SessionState { state: String, function: String },
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::SessionState { state, function } => {
                tr!(lang, SESSION_STATE, state = state, function = function)
            }
        }
    }
}

/// `Error::SessionState`.
const SESSION_STATE: Msg = Msg {
    en: "the session continues in state `{state}`, which Function `{function}` no longer has",
    uk: "сесія продовжується в стані `{state}`, якого Function `{function}` вже не має",
};
