//! What can go wrong in the daemon itself: its socket taken, a session or
//! a transcriber it does not hold, audio it cannot read (AGL-121, D66).

use std::path::PathBuf;

use agentlibre_common::i18n::{Lang, Msg};
use agentlibre_common::tr;

/// The daemon's own error, wrapped by the core's `Error::DaemonServer`
/// (AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// Another daemon answers on the socket.
    Running { path: PathBuf },
    /// No session of that id is open in the daemon.
    NoSession { id: String },
    /// `transcribe` for a directory with no session open in the daemon
    /// (AGL-076, D15).
    NoTranscriber { dir: PathBuf },
    /// `transcribe`'s audio is not base64.
    BadAudio,
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::Running { path } => tr!(lang, RUNNING, path = path.display()),
            Self::NoSession { id } => tr!(lang, NO_SESSION, id = id),
            Self::NoTranscriber { dir } => tr!(lang, NO_TRANSCRIBER, dir = dir.display()),
            Self::BadAudio => tr!(lang, BAD_AUDIO).to_string(),
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
