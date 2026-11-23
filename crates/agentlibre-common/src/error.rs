//! The errors shared by the crates above: a file that cannot be read or
//! written, a name that cannot name a file, a stopped generation, and the
//! common modules' own: programs, paths, Nix, settings (AGL-121, D78).

use std::io;
use std::path::PathBuf;

use crate::i18n::{Lang, m};
use crate::tr;

/// An error of agentlibre-common, wrapped by the core's `Error::Common`
/// (AGL-121, D78).
#[derive(Debug)]
pub enum Error {
    /// A settings file cannot be read.
    Read { path: PathBuf, source: io::Error },
    /// A file or directory cannot be written.
    Write { path: PathBuf, source: io::Error },
    /// A format or Function name that cannot name a file.
    InvalidName(String),
    /// Ctrl-C stopped a generation (D111).
    Interrupted,
    /// A helper program of agl's package cannot be found (AGL-040).
    Programs(crate::programs::Error),
    /// agl's directories cannot be found (AGL-121, D75).
    Paths(crate::paths::Error),
    /// Nix evaluation and the shape of a Nix file (AGL-039; AGL-121, D66,
    /// D71).
    Nix(crate::nix::Error),
    /// A value of the settings out of what agl takes (AGL-121, D66).
    Settings(crate::settings::error::Error),
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Error::Read { path, source } => {
                tr!(lang, m::READ, path = path.display(), source = source)
            }
            Error::Write { path, source } => {
                tr!(lang, m::FORGE_WRITE, path = path.display(), source = source)
            }
            Error::InvalidName(name) => tr!(lang, m::INVALID_NAME, name = name),
            Error::Interrupted => tr!(lang, m::INTERRUPTED).to_string(),
            Error::Programs(error) => error.message(lang),
            Error::Paths(error) => error.message(lang),
            Error::Nix(error) => error.message(lang),
            Error::Settings(error) => error.message(lang),
        }
    }
}

impl From<crate::programs::Error> for Error {
    fn from(source: crate::programs::Error) -> Error {
        Error::Programs(source)
    }
}

impl From<crate::paths::Error> for Error {
    fn from(source: crate::paths::Error) -> Error {
        Error::Paths(source)
    }
}

impl From<crate::nix::Error> for Error {
    fn from(source: crate::nix::Error) -> Error {
        Error::Nix(source)
    }
}

impl From<crate::settings::error::Error> for Error {
    fn from(source: crate::settings::error::Error) -> Error {
        Error::Settings(source)
    }
}
