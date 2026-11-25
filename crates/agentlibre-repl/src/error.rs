//! The REPL's errors: what the surfaces share, and what only the raw
//! terminal makes (AGL-131, D5).

use std::io;

use agentlibre_common::i18n::{Lang, Msg};
use agentlibre_common::tr;

/// The core's error, for the tests' fixtures of what the daemon says
/// (AGL-126, D2).
#[cfg(test)]
pub(crate) use agentlibre_core::error::Error as CoreError;

/// An error of the REPL: one the surfaces share, or raw mode's own.
#[derive(Debug)]
pub enum Error {
    /// What the daemon refused, a file, the clipboard (AGL-127, D6).
    Surface(agentlibre_surface::Error),
    /// The terminal cannot be switched to raw mode.
    Terminal { source: io::Error },
}

impl From<agentlibre_surface::Error> for Error {
    fn from(source: agentlibre_surface::Error) -> Error {
        Error::Surface(source)
    }
}

impl From<agentlibre_client::Error> for Error {
    fn from(source: agentlibre_client::Error) -> Error {
        Error::Surface(source.into())
    }
}

impl From<agentlibre_common::error::Error> for Error {
    fn from(source: agentlibre_common::error::Error) -> Error {
        Error::Surface(source.into())
    }
}

impl From<agentlibre_common::media::Error> for Error {
    fn from(source: agentlibre_common::media::Error) -> Error {
        Error::Surface(source.into())
    }
}

impl Error {
    /// Process exit status for this error: an interrupt's 130, else 1
    /// (AGL-131, D5).
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Surface(agentlibre_surface::Error::Common(
                agentlibre_common::error::Error::Interrupted,
            )) => agentlibre_common::tty::INTERRUPTED_EXIT,
            _ => 1,
        }
    }

    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Error::Surface(error) => error.message(lang),
            Error::Terminal { source } => tr!(lang, TERMINAL, source = source),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message(Lang::En))
    }
}

/// `Error::Terminal`.
const TERMINAL: Msg = Msg {
    en: "cannot switch the terminal to raw mode: {source}",
    uk: "не вдалося перемкнути термінал у raw-режим: {source}",
};
