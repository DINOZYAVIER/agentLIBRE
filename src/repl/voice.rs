//! The voice input of the interactive session (AGL-016): Ctrl-R records
//! the microphone, the second press stops, the model when it hears
//! (AGL-051), or else whisper, turns the recording into text and the text
//! goes into the input line (D2, D3). It works while a turn runs (D7).

use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::dock::{Dock, VoiceLabel};
use super::term::Key;
use super::transcript::{Screen, notice_lines};
use crate::cli::whisper::Dictation;
use crate::error::Error;
use crate::i18n::{Lang, m};
use crate::record::Recording;
use crate::theme::{Role, Theme};
use crate::tr;

/// How often a recording is looked at: its time is shown and its end at
/// the limit is seen.
const WATCH: Duration = Duration::from_millis(100);

/// Starts a recording of at most the given number of seconds.
pub type Recorder = Box<dyn Fn(u32) -> Result<Box<dyn Recording>, Error> + Send + Sync>;

struct Active {
    recording: Box<dyn Recording>,
    since: Instant,
    /// The server being brought up while the human speaks (D6).
    warm: Option<JoinHandle<Result<(), Error>>>,
    /// Tells a recording from the next one, for the thread that watches.
    id: u64,
}

enum State {
    Idle,
    Recording(Active),
    Transcribing,
}

pub struct VoiceInput {
    dock: Arc<Dock>,
    dictation: Arc<dyn Dictation>,
    recorder: Recorder,
    /// The longest recording, in seconds (D5).
    limit: u32,
    lang: Lang,
    theme: Theme,
    state: Mutex<State>,
    next_id: Mutex<u64>,
}

impl VoiceInput {
    pub fn new(
        dock: Arc<Dock>,
        dictation: Arc<dyn Dictation>,
        recorder: Recorder,
        limit: u32,
        lang: Lang,
        theme: Theme,
    ) -> VoiceInput {
        VoiceInput {
            dock,
            dictation,
            recorder,
            limit,
            lang,
            theme,
            state: Mutex::new(State::Idle),
            next_id: Mutex::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notice(&self, text: &str, role: Role) {
        let columns = self.dock.columns();
        self.dock
            .print(&notice_lines(&self.theme, text, columns, role));
    }

    /// Whether a recording is going.
    pub fn recording(&self) -> bool {
        matches!(*self.lock(), State::Recording(_))
    }

    /// Ctrl-R: starts a recording, or stops the one that goes (D2).
    pub fn toggle(self: &Arc<Self>) {
        let recording = match &*self.lock() {
            State::Idle => false,
            State::Recording(_) => true,
            // The text of the last one is still being made.
            State::Transcribing => return,
        };
        if recording {
            self.finish();
        } else {
            self.start();
        }
    }

    fn start(self: &Arc<Self>) {
        // What takes the recording must be set up: told before the human
        // speaks.
        if let Err(error) = self.dictation.ready() {
            self.notice(&error.message(self.lang), Role::StatusFailure);
            return;
        }
        let recording = match (self.recorder)(self.limit) {
            Ok(recording) => recording,
            Err(error) => {
                self.notice(&error.message(self.lang), Role::StatusFailure);
                return;
            }
        };
        let id = {
            let mut next = self.next_id.lock().unwrap_or_else(|e| e.into_inner());
            *next += 1;
            *next
        };
        // The unit starts while the human speaks (D6).
        let warm = {
            let me = self.clone();
            thread::spawn(move || {
                me.dictation
                    .warm_up(&mut |line| me.notice(line, Role::Muted))
            })
        };
        *self.lock() = State::Recording(Active {
            recording,
            since: Instant::now(),
            warm: Some(warm),
            id,
        });
        self.dock.set_voice(Some(VoiceLabel::Recording(0)));
        let me = self.clone();
        thread::spawn(move || me.watch(id));
    }

    /// Shows the time and sees the end at the limit (D5).
    fn watch(self: Arc<Self>, id: u64) {
        let mut shown = 0;
        loop {
            thread::sleep(WATCH);
            let ended = {
                let mut state = self.lock();
                match &mut *state {
                    State::Recording(active) if active.id == id => {
                        let seconds = active.since.elapsed().as_secs();
                        if seconds != shown {
                            shown = seconds;
                            self.dock.set_voice(Some(VoiceLabel::Recording(seconds)));
                        }
                        active.recording.finished()
                    }
                    _ => return,
                }
            };
            if ended {
                self.finish();
                return;
            }
        }
    }

    /// Stops the recording and turns it into text on a thread (D3).
    fn finish(self: &Arc<Self>) {
        let active = {
            let mut state = self.lock();
            match std::mem::replace(&mut *state, State::Transcribing) {
                State::Recording(active) => active,
                other => {
                    *state = other;
                    return;
                }
            }
        };
        self.dock.set_voice(Some(VoiceLabel::Transcribing));
        let me = self.clone();
        thread::spawn(move || {
            let result = me.text_of(active);
            *me.lock() = State::Idle;
            me.dock.set_voice(None);
            match result {
                Ok(text) if text.is_empty() => {
                    me.notice(tr!(me.lang, m::VOICE_NOTHING_HEARD), Role::StatusPending);
                }
                // The text goes in at the cursor, where the human was
                // writing (D3).
                Ok(text) => {
                    me.dock.key(Key::Paste(format!("{text} ")));
                }
                Err(error) => me.notice(&error.message(me.lang), Role::StatusFailure),
            }
        });
    }

    fn text_of(&self, active: Active) -> Result<String, Error> {
        let Active {
            recording, warm, ..
        } = active;
        let wav = recording.stop()?;
        if let Some(warm) = warm {
            // The server that was starting must answer first; if it could
            // not start, that is the error.
            match warm.join() {
                Ok(result) => result?,
                Err(_) => return Err(Error::Ffmpeg { detail: "the thread of the server failed".into() }),
            }
        }
        let text = self
            .dictation
            .transcribe(&wav, &mut |line| self.notice(line, Role::Muted))?;
        Ok(text.trim().to_owned())
    }

    /// Escape or Ctrl-C while recording, or the end of the session: keeps
    /// nothing (D2).
    pub fn cancel(&self) {
        let active = {
            let mut state = self.lock();
            match std::mem::replace(&mut *state, State::Idle) {
                State::Recording(active) => Some(active),
                State::Transcribing => {
                    *state = State::Transcribing;
                    None
                }
                State::Idle => None,
            }
        };
        if let Some(active) = active {
            active.recording.cancel();
            self.dock.set_voice(None);
        }
    }
}
