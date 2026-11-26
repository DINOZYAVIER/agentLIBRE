//! The voice of the chat, as the REPL's (AGL-016, AGL-123; AGL-128, D2):
//! Ctrl-R or Alt-R records the microphone, the second press stops it, and
//! the daemon transcribes it with the session's whisper (AGL-127, D4;
//! AGL-136). A dictation begun or touched with Alt-R asks: its text goes into
//! the field; one with Ctrl-R alone is sent.

use std::cell::RefCell;
use std::rc::Rc;

use agentlibre_common::i18n::Lang;
use agentlibre_common::paths::Paths;
use agentlibre_surface::dictation::{Dictation, RemoteVoice};
use agentlibre_surface::record::{self, Recording};

/// What the text of a dictation does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Ctrl-R: it is sent (AGL-123, D1).
    Send,
    /// Alt-R: it goes into the field, to be read first.
    Ask,
}

/// A recording under way, and what its text does.
type Under = Option<(Box<dyn Recording>, Mode)>;

/// The microphone of the window.
#[derive(Default, Clone)]
pub struct Voice {
    recording: Rc<RefCell<Under>>,
}

/// What a press of Ctrl-R or Alt-R did.
pub enum Press {
    /// A recording began.
    Started,
    /// The recording ended: its WAV, and what its text does.
    Stopped(Vec<u8>, Mode),
    /// It failed: the error, said.
    Failed(String),
}

impl Voice {
    /// A press of Ctrl-R (`alt` false) or Alt-R: the first starts a
    /// recording of at most `limit` seconds, the second stops it.
    pub fn press(&self, alt: bool, limit: u32, lang: Lang) -> Press {
        let mode = if alt { Mode::Ask } else { Mode::Send };
        let mut slot = self.recording.borrow_mut();
        match slot.take() {
            None => match record::start(limit) {
                Ok(recording) => {
                    *slot = Some((recording, mode));
                    Press::Started
                }
                Err(error) => Press::Failed(error.message(lang)),
            },
            Some((recording, began)) => {
                // Alt-R at either end asks (AGL-123).
                let mode = if began == Mode::Ask { Mode::Ask } else { mode };
                match recording.stop() {
                    Ok(wav) => Press::Stopped(wav, mode),
                    Err(error) => Press::Failed(error.message(lang)),
                }
            }
        }
    }

}

/// The text of `wav`, by the daemon's transcription of the session
/// `session`; it waits for the daemon, so it runs off the window's thread.
pub fn transcribe(wav: &[u8], session: String, lang: Lang) -> Result<String, String> {
    let paths = Paths::from_env(|name| std::env::var_os(name)).map_err(|error| error.message(lang))?;
    let voice = RemoteVoice { paths, session, lang };
    voice.transcribe(wav, None, &mut |_| {}).map_err(|error| error.message(lang))
}
