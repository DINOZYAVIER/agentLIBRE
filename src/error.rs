//! Errors reported to the user. [`crate::i18n`] renders their messages.

use std::io;
use std::path::PathBuf;

/// An error that stops `agl-lite`.
#[derive(Debug)]
pub enum Error {
    /// An argument is not valid UTF-8.
    InvalidUnicode,
    /// An argument is not a known option.
    UnknownOption(String),
    /// An option is the last argument.
    MissingValue(String),
    /// An option is given twice.
    RepeatedOption(String),
    /// Neither `var` nor `HOME` is an absolute path.
    NoHome { var: &'static str },
    /// The current directory cannot be read.
    CurrentDir(io::Error),
    /// A settings file cannot be read.
    Read { path: PathBuf, source: io::Error },
    /// A settings file is not valid TOML; `detail` comes from the parser.
    Parse { path: PathBuf, detail: String },
    /// A settings file has a key outside `allowed` (D60).
    UnknownKey {
        path: PathBuf,
        key: String,
        allowed: &'static [&'static str],
    },
    /// A settings value has the wrong TOML type.
    WrongType {
        path: PathBuf,
        key: String,
        expected: Expected,
    },
    /// `language` names a language without translations.
    UnsupportedLanguage { path: PathBuf, value: String },
    /// `[server] url` is not an `http://` URL (D52).
    UnsupportedUrl { path: PathBuf, url: String },
    /// A project file sets `[server]` (D57).
    ServerInProject { path: PathBuf, user_path: PathBuf },
    /// No `[server] url` in the user file (D59).
    MissingServerUrl { user_path: PathBuf },
    /// Everything after loading the settings arrives in later tasks (D62).
    NotImplemented,
}

/// TOML type a settings key requires.
#[derive(Debug)]
pub enum Expected {
    String,
    Table,
}

impl Error {
    /// Whether the error is in the command line rather than the setup.
    pub fn is_usage(&self) -> bool {
        matches!(
            self,
            Error::InvalidUnicode
                | Error::UnknownOption(_)
                | Error::MissingValue(_)
                | Error::RepeatedOption(_)
        )
    }

    /// Process exit status for this error.
    pub fn exit_code(&self) -> u8 {
        if self.is_usage() { 2 } else { 1 }
    }
}
