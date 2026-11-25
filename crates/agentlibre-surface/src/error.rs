//! The crate's error (AGL-127, D6): the daemon, not the session behind it;
//! a file or a path; a medium.

use std::fmt;

use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::tr;

/// What goes wrong on a client's side of the computer.
#[derive(Debug)]
pub enum Error {
    /// The daemon cannot be reached, read or written, or it refused.
    Client(agentlibre_client::Error),
    /// A file that cannot be read or written, and the common modules' own.
    Common(agentlibre_common::error::Error),
    /// A file that is no medium, or `ffmpeg` failing on one.
    Media(agentlibre_common::media::Error),
}

/// The message of a client's error in `lang`: the words agl says them in, a
/// refusal as the daemon said it (AGL-127, D6; AGL-131, D5).
pub fn client_message(error: &agentlibre_client::Error, lang: Lang) -> String {
    match error {
        agentlibre_client::Error::NoRuntime => tr!(lang, m::DAEMON_NO_RUNTIME).to_string(),
        agentlibre_client::Error::Daemon { detail } => tr!(lang, m::DAEMON_FAILED, detail = detail),
        agentlibre_client::Error::Said { text } => text.clone(),
        agentlibre_client::Error::Gone => tr!(lang, m::PLANE_GONE).to_string(),
    }
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Error::Client(error) => client_message(error, lang),
            Error::Common(error) => error.message(lang),
            Error::Media(error) => error.message(lang),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}", self.message(Lang::En))
    }
}

impl std::error::Error for Error {}

impl From<agentlibre_client::Error> for Error {
    fn from(source: agentlibre_client::Error) -> Error {
        Error::Client(source)
    }
}

impl From<agentlibre_common::error::Error> for Error {
    fn from(source: agentlibre_common::error::Error) -> Error {
        Error::Common(source)
    }
}

impl From<agentlibre_common::media::Error> for Error {
    fn from(source: agentlibre_common::media::Error) -> Error {
        Error::Media(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The message in English and in Ukrainian.
    fn both(error: Error) -> (String, String) {
        (error.message(Lang::En), error.message(Lang::Uk))
    }

    #[test]
    fn the_daemon_is_said_in_both_languages() {
        let (en, uk) = both(agentlibre_client::Error::NoRuntime.into());
        assert!(en.contains("XDG_RUNTIME_DIR is not set"), "{en}");
        assert!(uk.contains("XDG_RUNTIME_DIR не задано"), "{uk}");
        let (en, uk) = both(agentlibre_client::Error::Daemon { detail: "no socket".into() }.into());
        assert_eq!(en, "the daemon: no socket");
        assert_eq!(uk, "демон: no socket");
        let (en, uk) = both(agentlibre_client::Error::Gone.into());
        assert!(en.starts_with("the run of the session ended"), "{en}");
        assert!(uk.starts_with("виконання сесії скінчилося"), "{uk}");
    }

    #[test]
    fn a_refusal_is_the_daemons_own_words() {
        let (en, uk) = both(agentlibre_client::Error::Said { text: "no such session".into() }.into());
        assert_eq!((en.as_str(), uk.as_str()), ("no such session", "no such session"));
    }

    #[test]
    fn a_file_is_said_in_both_languages() {
        let read = agentlibre_common::error::Error::Read {
            path: "/a/b".into(),
            source: std::io::Error::other("denied"),
        };
        let (en, uk) = both(read.into());
        assert!(en.contains("/a/b") && en.contains("denied"), "{en}");
        assert!(uk.contains("/a/b") && uk.contains("denied"), "{uk}");
        assert_ne!(en, uk);
    }

    #[test]
    fn a_medium_is_said_in_both_languages() {
        let (en, uk) = both(agentlibre_common::media::Error::Kind { path: "/a.txt".into() }.into());
        assert_eq!(en, "/a.txt is not an image, a video or an audio");
        assert_eq!(uk, "/a.txt не є зображенням, відео чи аудіо");
        let (en, uk) = both(agentlibre_common::media::Error::Ffmpeg { detail: "busy".into() }.into());
        assert_eq!(en, "ffmpeg failed: busy");
        assert_eq!(uk, "помилка ffmpeg: busy");
    }
}
