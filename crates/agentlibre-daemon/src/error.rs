//! What can go wrong in the daemon: the core's errors, its socket, one
//! taken, a session or a transcriber it does not hold, audio it cannot read
//! (AGL-121, D66; AGL-131, D2).

use std::path::PathBuf;

use agentlibre_common::i18n::{Lang, Msg, m};
use agentlibre_common::tr;
use agentlibre_core::error::Error as CoreError;

/// The daemon's error: the core's, wrapped, and its own.
#[derive(Debug)]
pub enum Error {
    /// An error of the core, of a run or of the stores the daemon reads.
    Core(CoreError),
    /// The socket cannot be made, taken over or reached.
    Socket { detail: String },
    /// There is no runtime directory, so no place for the socket (D7).
    NoRuntime,
    /// Another daemon answers on the socket.
    Running { path: PathBuf },
    /// No session of that id is open in the daemon.
    NoSession { id: String },
    /// `transcribe` for a directory with no session open in the daemon
    /// (AGL-076, D15).
    NoTranscriber { dir: PathBuf },
    /// `transcribe`'s audio is not base64.
    BadAudio,
    /// `transcribe` of a draft for a session whose whisper has no draft
    /// server set (AGL-127, D4).
    NoDraft,
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::Core(error) => error.message(lang),
            Self::Socket { detail } => tr!(lang, m::DAEMON_FAILED, detail = detail),
            Self::NoRuntime => tr!(lang, m::DAEMON_NO_RUNTIME).to_string(),
            Self::Running { path } => tr!(lang, RUNNING, path = path.display()),
            Self::NoSession { id } => tr!(lang, NO_SESSION, id = id),
            Self::NoTranscriber { dir } => tr!(lang, NO_TRANSCRIBER, dir = dir.display()),
            Self::BadAudio => tr!(lang, BAD_AUDIO).to_string(),
            Self::NoDraft => tr!(lang, NO_DRAFT).to_string(),
        }
    }
}

/// `Error::Running`.
const RUNNING: Msg = Msg {
    en: "another agl daemon answers on {path}",
    uk: "на {path} уже відповідає інший agl daemon",
};

/// `Error::NoSession`.
const NO_SESSION: Msg = Msg {
    en: "no session {id} is open in the daemon",
    uk: "сесія {id} не відкрита в демоні",
};

/// `Error::NoTranscriber`.
const NO_TRANSCRIBER: Msg = Msg {
    en: "no session is open in the daemon for {dir}, to transcribe with its settings",
    uk: "у демоні не відкрито жодної сесії для {dir}, щоб розпізнати з її налаштуваннями",
};

/// `Error::BadAudio`.
const BAD_AUDIO: Msg = Msg {
    en: "the audio is not base64",
    uk: "аудіо не в base64",
};

/// `Error::NoDraft`.
const NO_DRAFT: Msg = Msg {
    en: "no draft server is set for this session's whisper",
    uk: "для whisper цієї сесії не задано сервера чернетки",
};

impl From<CoreError> for Error {
    fn from(source: CoreError) -> Error {
        Error::Core(source)
    }
}

/// A core module's own error is the core's, and so the daemon's.
macro_rules! from_core {
    ($($path:path),* $(,)?) => {
        $(impl From<$path> for Error {
            fn from(source: $path) -> Error {
                Error::Core(source.into())
            }
        })*
    };
}

from_core!(
    agentlibre_core::plane::error::Error,
    agentlibre_core::session::error::Error,
    agentlibre_core::engine::error::Error,
    agentlibre_core::function::error::Error,
    agentlibre_core::backend::error::Error,
    agentlibre_core::machine::error::Error,
    agentlibre_core::whisper::Error,
    agentlibre_core::media::Error,
    agentlibre_forge::forge::error::Error,
    agentlibre_forge::memory::Error,
    agentlibre_forge::Error,
    agentlibre_common::error::Error,
    agentlibre_common::programs::Error,
);
