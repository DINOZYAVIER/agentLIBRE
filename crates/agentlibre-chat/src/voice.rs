//! The voice of the chat, as the REPL's (AGL-016, AGL-122, AGL-123; AGL-128,
//! D2): Ctrl-R records the microphone and sends the text, Alt-R records and
//! asks, the text going into the field; Alt-R over a recording flips the
//! two, Ctrl-R ends it. The daemon transcribes the recording with the
//! session's whisper (AGL-127, D4; AGL-136), and, when it has a draft
//! server, the chunks as the human speaks: the draft grows in the field.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use agentlibre_common::i18n::Lang;
use agentlibre_common::paths::Paths;
use agentlibre_surface::dictation::{Dictation, DraftEngine, DraftWorker, RemoteDraft, RemoteVoice, merge_draft};
use agentlibre_surface::record::{self, Recording, Vad};

/// What the text of a dictation does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Ctrl-R: it is sent (AGL-123, D1).
    Send,
    /// Alt-R: it goes into the field, to be read first.
    Ask,
}

/// A recording under way.
struct Under {
    recording: Box<dyn Recording>,
    mode: Mode,
    /// The draft engine at work, when the daemon has a draft server.
    draft: Option<DraftWorker>,
    /// The draft so far.
    drafted: String,
}

/// The microphone of the window.
#[derive(Default, Clone)]
pub struct Voice {
    under: Rc<RefCell<Option<Under>>>,
}

/// What a press of Ctrl-R or Alt-R did.
pub enum Press {
    /// A recording began, in this mode.
    Started(Mode),
    /// Alt-R over a recording: its mode now.
    Flipped(Mode),
    /// Ctrl-R ended the recording: its WAV, and what its text does.
    Stopped(Vec<u8>, Mode),
    /// It failed: the error, said.
    Failed(String),
}

impl Voice {
    /// A press of Ctrl-R (`alt` false) or Alt-R. A new recording lasts at
    /// most `limit` seconds; with `draft` its chunks are drafted as it goes.
    pub fn press(&self, alt: bool, limit: u32, draft: Option<(String, Lang)>, lang: Lang) -> Press {
        let mut slot = self.under.borrow_mut();
        if let Some(under) = slot.as_mut() {
            if alt {
                under.mode = if under.mode == Mode::Ask { Mode::Send } else { Mode::Ask };
                return Press::Flipped(under.mode);
            }
            let under = slot.take().expect("a recording");
            return match under.recording.stop() {
                Ok(wav) => Press::Stopped(wav, under.mode),
                Err(error) => Press::Failed(error.message(lang)),
            };
        }
        let mode = if alt { Mode::Ask } else { Mode::Send };
        // With a draft server the recording chunks (AGL-122, D5, D6); one
        // that cannot chunk falls back to the plain recording.
        let (recording, draft) = match draft {
            Some((session, lang)) => match record::start_chunked(limit, Vad::default()) {
                Ok(recording) => {
                    let engine = Paths::from_env(|name| std::env::var_os(name))
                        .ok()
                        .map(|paths| Arc::new(RemoteDraft { paths, session, lang }) as Arc<dyn DraftEngine>);
                    (Ok(recording), engine.map(DraftWorker::start))
                }
                Err(_) => (record::start(limit), None),
            },
            None => (record::start(limit), None),
        };
        match recording {
            Ok(recording) => {
                *slot = Some(Under { recording, mode, draft, drafted: String::new() });
                Press::Started(mode)
            }
            Err(error) => Press::Failed(error.message(lang)),
        }
    }

    /// Whether a recording is under way.
    pub fn recording(&self) -> bool {
        self.under.borrow().is_some()
    }

    /// The draft, grown by the chunks finished since the last look; `None`
    /// when nothing new came.
    pub fn draft(&self) -> Option<String> {
        let mut slot = self.under.borrow_mut();
        let under = slot.as_mut()?;
        let worker = under.draft.as_ref()?;
        while let Some((wav, end)) = under.recording.poll_chunk() {
            let _ = worker.chunks.send((wav, end));
        }
        let mut grown = false;
        while let Ok((text, _)) = worker.texts.try_recv() {
            under.drafted = merge_draft(&under.drafted, &text);
            grown = true;
        }
        grown.then(|| under.drafted.clone())
    }
}

/// The text of `wav`, by the daemon's transcription of the session
/// `session`; it waits for the daemon, so it runs off the window's thread.
pub fn transcribe(wav: &[u8], session: String, lang: Lang) -> Result<String, String> {
    let paths = Paths::from_env(|name| std::env::var_os(name)).map_err(|error| error.message(lang))?;
    let voice = RemoteVoice { paths, session, lang };
    voice.transcribe(wav, None, &mut |_| {}).map_err(|error| error.message(lang))
}
