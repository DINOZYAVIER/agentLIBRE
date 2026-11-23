//! Errors reported to the user. [`crate::i18n`] renders their messages.

use std::io;
use std::path::PathBuf;


/// An error that stops `agl`.
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
    /// `--function` with `--resume`: the session names its Function (D106).
    FunctionWithResume,
    /// `--plane` with `--resume`: the session keeps its plane (AGL-077).
    PlaneWithResume,
    /// `--think` names a level outside the four (AGL-013).
    BadReasoningLevel { value: String },
    /// agl's directories cannot be found. Its own type, wrapped (AGL-121,
    /// D75).
    Paths(crate::paths::Error),
    /// The current directory cannot be read.
    CurrentDir(io::Error),
    /// A settings file cannot be read.
    Read { path: PathBuf, source: io::Error },
    /// A settings file has a key outside `allowed` (D60).
    UnknownKey {
        path: PathBuf,
        key: String,
        allowed: Vec<&'static str>,
    },
    /// A required key is missing.
    MissingKey { path: PathBuf, key: String },
    /// A settings value has the wrong type.
    WrongType {
        path: PathBuf,
        key: String,
        expected: Expected,
    },
    /// `agl search login` without a complete `search.ca` in the user file
    /// (AGL-043 F20); `key` is the first one missing.
    SearchCaNotSet { user_path: PathBuf, key: String },
    /// A step of `agl search login` failed (AGL-043 F20).
    SearchLogin { detail: String },
    /// A value of the settings out of what agl takes: a style, a limit, a
    /// port, a language, a url, a level, a path. Its own type, wrapped
    /// (AGL-121, D66).
    Settings(crate::settings::error::Error),
    /// The forge: its git, its markers and stores, its artifacts and their
    /// states (AGL-005 and after). Its own type, wrapped (AGL-121, D66).
    Forge(crate::forge::error::Error),
    /// The engine and its server: found, launched, its unit, its port, the
    /// human's answers about it (D147, D149; AGL-024 and after). Its own type,
    /// wrapped (AGL-121, D66).
    Engine(crate::engine::error::Error),
    /// The model's server as the backend reaches it: unreachable, its status,
    /// its reply, its certificate, no projector (D52; AGL-014; AGL-058). Its
    /// own type, wrapped (AGL-121, D66).
    Backend(crate::backend::error::Error),
    /// A credential file of `search` the policy refuses (AGL-019, D4). Its
    /// own type, wrapped (AGL-121, D66).
    Search(crate::search::Error),
    /// A format or Function name that cannot name a file.
    InvalidName(String),
    /// A template or string of a format is unusable (D81). Its own type,
    /// wrapped (AGL-121, D66).
    Format(crate::format::Error),
    /// A Function not found, built in, existing already, or unusable at a
    /// key (D21; AGL-102). Its own type, wrapped (AGL-121, D66).
    Function(crate::function::error::Error),
    /// A session not found, its name, its file's lines or its writing
    /// (D107; AGL-062). Its own type, wrapped (AGL-121, D66).
    Session(crate::session::error::Error),
    /// The machine cannot continue a session's turn. Its own type, wrapped
    /// (AGL-121, D66).
    Machine(crate::machine::error::Error),
    /// The terminal cannot be switched to raw mode.
    Terminal { source: io::Error },
    /// Ctrl-C stopped a generation (D111).
    Interrupted,
    /// The server generated text the grammar does not allow.
    OutsideGrammar(String),
    /// `--attach` without `-p` (AGL-014, D21).
    AttachWithoutPrompt,
    /// The Windows engine in WSL could not start or run (AGL-052, D9, D10;
    /// AGL-056, D8). Its own type, wrapped (AGL-121, D66).
    Wsl(crate::wsl::Error),
    /// `agl notify` lost or could not reach the session bus (AGL-074, D2).
    Notify { detail: String },
    /// A plane's name, store, files or directories (AGL-077). Its own
    /// type, wrapped (AGL-121, D66).
    Plane(crate::plane::error::Error),
    /// What the daemon refused, in its words (AGL-076).
    Said { text: String },
    /// `agl daemon` has no runtime directory for its socket (AGL-076, D7).
    DaemonNoRuntime,
    /// The daemon's socket or a connection failed.
    Daemon { detail: String },
    /// The daemon's own: its socket taken, a session or a transcriber it
    /// does not hold, bad audio. Its own type, wrapped (AGL-121, D66).
    DaemonServer(crate::daemon::error::Error),
    /// The thread of a run ended before it answered: a fault of agl's
    /// (AGL-075, D7).
    PlaneGone,
    /// The Strata engine's own error (AGL-089, D20, D9, D13). Its own type,
    /// wrapped (AGL-121, D66).
    Strata(crate::strata::Error),
    /// `agl bridge` and `bridge` lacks `address` or `port` (AGL-083, D20).
    BridgeNotSet { path: PathBuf },
    /// No `agl-bridge` beside `agl` (AGL-083, D18).
    BridgeProgramMissing { path: PathBuf },
    /// The bridge's unit did not come to listen.
    BridgeNotListening { unit: String },
    /// A device's name the bridge does not take (AGL-083, D10).
    BridgeDeviceName { name: String },
    /// `agl bridge pair` of a device paired already.
    BridgeDeviceExists { name: String },
    /// `agl bridge revoke` of a device not paired.
    BridgeDeviceUnknown { name: String },
    /// The bridge's directory could not be read or written.
    Bridge { detail: String },
    /// The whisper server was not set or could not transcribe (AGL-014,
    /// D22). Its own type, wrapped (AGL-121, D66).
    Whisper(crate::whisper::Error),
    /// A medium: changed, of no kind agl takes, or ffmpeg failing on it
    /// (AGL-014). Its own type, wrapped (AGL-121, D66).
    Media(crate::media::Error),
    /// A weights file was missing (AGL-039, D24c; AGL-050, D11). Its own
    /// type, wrapped (AGL-121, D66).
    Weights(crate::weights::Error),
    /// A weights file failed to download (AGL-050, D17). Its own type,
    /// wrapped (AGL-121, D66).
    Download(crate::download::Error),
    /// Nix evaluation: the directory, the evaluator, a failed evaluation, a
    /// file that is not a function (AGL-039, D6-D13, D38). Its own type,
    /// wrapped (AGL-121, D66).
    Nix(crate::nix::Error),
    /// `AGENTLIBRE_TOOLS` is not set to an absolute directory: agl runs
    /// outside its package's wrapper and the dev shell (AGL-040).
    ToolsDirMissing,
    /// A helper program is not in the `AGENTLIBRE_TOOLS` directory.
    ToolMissing { name: String, dir: PathBuf },
    /// A forge file or directory cannot be written.
    Write { path: PathBuf, source: io::Error },
    /// A memory entry not found or not removed (AGL-060). Its own type,
    /// wrapped (AGL-121, D66).
    Memory(crate::memory::Error),
    /// `agl notify` cannot start the `agl-notify` beside it (AGL-121, D23).
    NotifySpawn { source: io::Error },
}

impl From<agentlibre_client::Error> for Error {
    /// A client's error as agl's: the daemon, its words, its end
    /// (AGL-121, D29). The clients' crate has no i18n; agl's words are here.
    fn from(source: agentlibre_client::Error) -> Error {
        match source {
            agentlibre_client::Error::NoRuntime => Error::DaemonNoRuntime,
            agentlibre_client::Error::Daemon { detail } => Error::Daemon { detail },
            agentlibre_client::Error::Said { text } => Error::Said { text },
            agentlibre_client::Error::Gone => Error::PlaneGone,
        }
    }
}

impl From<crate::plane::error::Error> for Error {
    /// A plane's own error as agl's (AGL-121, D66).
    fn from(source: crate::plane::error::Error) -> Error {
        Error::Plane(source)
    }
}

impl From<crate::media::Error> for Error {
    /// A medium's own error as agl's (AGL-121, D66).
    fn from(source: crate::media::Error) -> Error {
        Error::Media(source)
    }
}

impl From<crate::function::error::Error> for Error {
    /// A Function's own error as agl's (AGL-121, D66).
    fn from(source: crate::function::error::Error) -> Error {
        Error::Function(source)
    }
}

impl From<crate::session::error::Error> for Error {
    /// A session's own error as agl's (AGL-121, D66).
    fn from(source: crate::session::error::Error) -> Error {
        Error::Session(source)
    }
}

impl From<crate::machine::error::Error> for Error {
    /// The machine's own error as agl's (AGL-121, D66).
    fn from(source: crate::machine::error::Error) -> Error {
        Error::Machine(source)
    }
}

impl From<crate::daemon::error::Error> for Error {
    /// The daemon's own error as agl's (AGL-121, D66).
    fn from(source: crate::daemon::error::Error) -> Error {
        Error::DaemonServer(source)
    }
}

impl From<crate::settings::error::Error> for Error {
    /// The settings' own error as agl's (AGL-121, D66).
    fn from(source: crate::settings::error::Error) -> Error {
        Error::Settings(source)
    }
}

impl From<crate::memory::Error> for Error {
    /// The memory's own error as agl's (AGL-121, D66).
    fn from(source: crate::memory::Error) -> Error {
        Error::Memory(source)
    }
}

impl From<crate::forge::error::Error> for Error {
    /// The forge's own error as agl's (AGL-121, D66).
    fn from(source: crate::forge::error::Error) -> Error {
        Error::Forge(source)
    }
}

impl From<crate::engine::error::Error> for Error {
    /// The engine's own error as agl's (AGL-121, D66).
    fn from(source: crate::engine::error::Error) -> Error {
        Error::Engine(source)
    }
}

impl From<crate::backend::error::Error> for Error {
    /// The backend's own error as agl's (AGL-121, D66).
    fn from(source: crate::backend::error::Error) -> Error {
        Error::Backend(source)
    }
}

impl From<crate::paths::Error> for Error {
    /// The paths' own error as agl's (AGL-121, D75).
    fn from(source: crate::paths::Error) -> Error {
        Error::Paths(source)
    }
}

impl From<crate::wsl::Error> for Error {
    /// The Windows engine's own error as agl's (AGL-121, D66).
    fn from(source: crate::wsl::Error) -> Error {
        Error::Wsl(source)
    }
}

impl From<crate::download::Error> for Error {
    /// A download's own error as agl's (AGL-121, D66).
    fn from(source: crate::download::Error) -> Error {
        Error::Download(source)
    }
}

impl From<crate::weights::Error> for Error {
    /// A weights lookup's own error as agl's (AGL-121, D66).
    fn from(source: crate::weights::Error) -> Error {
        Error::Weights(source)
    }
}

impl From<crate::whisper::Error> for Error {
    /// The whisper server's own error as agl's (AGL-121, D66).
    fn from(source: crate::whisper::Error) -> Error {
        Error::Whisper(source)
    }
}

impl From<crate::search::Error> for Error {
    /// The search credential policy's own error as agl's (AGL-121, D66).
    fn from(source: crate::search::Error) -> Error {
        Error::Search(source)
    }
}

impl From<crate::format::Error> for Error {
    /// A format template's own error as agl's (AGL-121, D66).
    fn from(source: crate::format::Error) -> Error {
        Error::Format(source)
    }
}

impl From<crate::strata::Error> for Error {
    /// A Strata engine's own error as agl's (AGL-121, D66).
    fn from(source: crate::strata::Error) -> Error {
        Error::Strata(source)
    }
}

impl From<crate::nix::Error> for Error {
    /// A Nix evaluation's own error as agl's (AGL-121, D66).
    fn from(source: crate::nix::Error) -> Error {
        Error::Nix(source)
    }
}

/// TOML type a settings key requires.
#[derive(Debug)]
pub enum Expected {
    String,
    Table,
    Bool,
    Integer,
    Number,
    Strings,
    /// A list of sets (AGL-089).
    Tables,
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
                | Error::FunctionWithResume
                | Error::PlaneWithResume
                | Error::BadReasoningLevel { .. }
                | Error::AttachWithoutPrompt
        )
    }

    /// Process exit status for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Interrupted => crate::stop::EXIT_CODE,
            _ if self.is_usage() => 2,
            _ => 1,
        }
    }
}

/// The English message, for places that keep an error as text (`forge` list
/// marks the files it cannot parse with it): the one text of `crate::i18n`
/// (AGL-058, D22).
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message(crate::i18n::Lang::En))
    }
}
