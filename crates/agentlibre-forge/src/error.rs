//! What can go wrong in the stores: the forge's own errors and the
//! shared ones of common (AGL-121, D66, D77, D78).

use agentlibre_common::i18n::Lang;

/// An error of agentlibre-forge. The core wraps it whole, keeping the
/// forge's own in `Error::Forge` and common's in `Error::Common`
/// (AGL-121, D77).
#[derive(Debug)]
pub enum Error {
    /// The forge: git, markers, stores, an artifact's file and states.
    Forge(crate::forge::error::Error),
    /// A file that cannot be read or written, Nix, a name (AGL-121, D78).
    Common(agentlibre_common::error::Error),
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Error::Forge(error) => error.message(lang),
            Error::Common(error) => error.message(lang),
        }
    }
}

impl From<crate::forge::error::Error> for Error {
    fn from(source: crate::forge::error::Error) -> Error {
        Error::Forge(source)
    }
}

impl From<agentlibre_common::error::Error> for Error {
    fn from(source: agentlibre_common::error::Error) -> Error {
        Error::Common(source)
    }
}

impl From<agentlibre_common::nix::Error> for Error {
    /// Nix as agl's, through common's (AGL-121, D71, D78).
    fn from(source: agentlibre_common::nix::Error) -> Error {
        Error::Common(source.into())
    }
}

impl From<agentlibre_common::settings::error::Error> for Error {
    /// A settings value as agl's, through common's (AGL-121, D78).
    fn from(source: agentlibre_common::settings::error::Error) -> Error {
        Error::Common(source.into())
    }
}

impl From<agentlibre_common::paths::Error> for Error {
    /// agl's directories as agl's, through common's (AGL-121, D75, D78).
    fn from(source: agentlibre_common::paths::Error) -> Error {
        Error::Common(source.into())
    }
}

impl From<agentlibre_common::programs::Error> for Error {
    /// A helper program as agl's, through common's (AGL-121, D75, D78).
    fn from(source: agentlibre_common::programs::Error) -> Error {
        Error::Common(source.into())
    }
}

/// The English message, for places that keep an error as text (`forge`
/// list marks the files it cannot parse with it): the one text of
/// `agentlibre_common::i18n` (AGL-058, D22).
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message(agentlibre_common::i18n::Lang::En))
    }
}
