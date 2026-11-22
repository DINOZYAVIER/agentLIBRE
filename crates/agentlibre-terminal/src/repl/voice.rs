//! The voice input of the interactive session (AGL-016): Ctrl-R records
//! the microphone, the second press stops, the model when it hears
//! (AGL-051), or else whisper, turns the recording into text and the text
//! goes into the input line (D2, D3). It works while a turn runs (D7).
//! AGL-123 adds the send mode: Alt-R starts a dictation that asks instead
//! of sending, and upgrades the live one; Ctrl-R auto-sends the transcript.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender as MpscSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::dock::{Dock, SendMode, VoiceLabel};
use super::editor::Outcome;
use super::term::Key;
use super::transcript::{Screen, notice_lines};
use crate::whisper::{Dictation, Draft};
use crate::error::Error;
use crate::i18n::{Lang, m};
use crate::record::Recording;
use crate::style::Role;
use crate::theme::Theme;
use crate::tr;

/// How often a recording is looked at: its time is shown and its end at
/// the limit is seen.
const WATCH: Duration = Duration::from_millis(100);

/// Starts a recording of at most the given number of seconds.
pub type Recorder = Box<dyn Fn(u32) -> Result<Box<dyn Recording>, Error> + Send + Sync>;

/// Queues an auto-sent transcript (AGL-123, D1).
pub type Sender = Arc<dyn Fn(String) + Send + Sync>;

/// What turns a chunk into draft text; `whisper::Draft` is the engine,
/// the tests fake it (AGL-122).
pub trait DraftEngine: Send + Sync {
    fn transcribe(&self, wav: &[u8]) -> Result<String, Error>;
}

impl DraftEngine for Draft {
    fn transcribe(&self, wav: &[u8]) -> Result<String, Error> {
        Draft::transcribe(self, wav)
    }
}

/// The draft engine at work (AGL-122): one thread turns chunks into text
/// one by one, so the lines keep their order; when the last sender of
/// the recording drops, it stops.
struct DraftWorker {
    chunks: MpscSender<Vec<u8>>,
    texts: Receiver<String>,
    _thread: JoinHandle<()>,
}

impl DraftWorker {
    fn start(draft: Arc<dyn DraftEngine>) -> DraftWorker {
        let (ctx, crx) = mpsc::channel::<Vec<u8>>();
        let (tx, rx) = mpsc::channel::<String>();
        let thread = thread::spawn(move || {
            for chunk in crx.iter() {
                // A chunk that failed or came out empty shows nothing:
                // the final pass has the last word (D7).
                if let Ok(text) = draft.transcribe(&chunk)
                    && !text.is_empty()
                    && tx.send(text).is_err()
                {
                    break;
                }
            }
        });
        DraftWorker { chunks: ctx, texts: rx, _thread: thread }
    }
}

struct Active {
    recording: Box<dyn Recording>,
    since: Instant,
    /// The server being brought up while the human speaks (D6).
    warm: Option<JoinHandle<Result<(), Error>>>,
    /// Tells a recording from the next one, for the thread that watches.
    id: u64,
    /// The live draft of this recording, when a draft engine is set
    /// (AGL-122, D5).
    worker: Option<DraftWorker>,
    /// The draft text spoken so far, shown by the dock in place.
    text: String,
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
    /// The chunking recorder of the draft mode (AGL-122, D6); used when
    /// a draft engine is set.
    recorder_chunked: Recorder,
    /// The live draft engine, `None` without one (AGL-122, D5).
    draft: Option<Arc<dyn DraftEngine>>,
    /// The longest recording, in seconds (D5).
    limit: u32,
    lang: Lang,
    theme: Theme,
    /// Whether the live dictation asks before sending (AGL-123, D1, D2).
    ask: AtomicBool,
    /// The callback that queues an auto-send message (AGL-123, D1).
    send: Mutex<Option<Sender>>,
    state: Mutex<State>,
    next_id: Mutex<u64>,
}

impl VoiceInput {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dock: Arc<Dock>,
        dictation: Arc<dyn Dictation>,
        recorder: Recorder,
        recorder_chunked: Recorder,
        draft: Option<Arc<dyn DraftEngine>>,
        limit: u32,
        lang: Lang,
        theme: Theme,
    ) -> VoiceInput {
        VoiceInput {
            dock,
            dictation,
            recorder,
            recorder_chunked,
            draft,
            limit,
            lang,
            theme,
            ask: AtomicBool::new(false),
            send: Mutex::new(None),
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

    /// The dictation key of `mode` (AGL-123, D1, D2, D11): from idle Ctrl-R
    /// starts auto, Alt-R starts ask; Ctrl-R always stops the recording;
    /// Alt-R flips the live dictation's mode, both ways, recording or
    /// transcribing.
    pub fn press(self: &Arc<Self>, mode: SendMode) {
        let recording = matches!(&*self.lock(), State::Recording(_));
        if recording {
            return match mode {
                SendMode::Auto => self.finish(),
                SendMode::Ask => self.flip_mode(),
            };
        }
        if matches!(&*self.lock(), State::Transcribing) {
            // The text is still being made: only the flip reaches it (D11).
            if matches!(mode, SendMode::Ask) {
                self.flip_mode();
            }
            return;
        }
        self.start(mode);
    }

    /// Alt-R over a live dictation: its mode flips, ask ⇄ auto
    /// (AGL-123, D11).
    fn flip_mode(self: &Arc<Self>) {
        let ask = !self.ask.load(Ordering::Relaxed);
        self.ask.store(ask, Ordering::Relaxed);
        let mode = if ask { SendMode::Ask } else { SendMode::Auto };
        let label = match &*self.lock() {
            State::Recording(active) => {
                Some(VoiceLabel::Recording(active.since.elapsed().as_secs(), mode))
            }
            State::Transcribing => Some(VoiceLabel::Transcribing(mode)),
            State::Idle => None,
        };
        self.dock.set_voice(label);
    }

    fn start(self: &Arc<Self>, mode: SendMode) {
        self.ask.store(matches!(mode, SendMode::Ask), Ordering::Relaxed);
        // What takes the recording must be set up: told before the human
        // speaks.
        if let Err(error) = self.dictation.ready() {
            self.notice(&error.message(self.lang), Role::StatusFailure);
            return;
        }
        // With a draft engine the recording chunks (D5, D6); without it
        // today's single pass records.
        let chunking = self.draft.is_some();
        let recording = if chunking {
            (self.recorder_chunked)(self.limit)
        } else {
            (self.recorder)(self.limit)
        };
        let recording = match recording {
            Ok(recording) => recording,
            Err(error) => {
                // A chunking recorder that cannot start falls back to
                // the plain one: the dictation matters more than the
                // draft.
                if !chunking {
                    self.notice(&error.message(self.lang), Role::StatusFailure);
                    return;
                }
                match (self.recorder)(self.limit) {
                    Ok(recording) => {
                        self.notice(&error.message(self.lang), Role::Muted);
                        recording
                    }
                    Err(error) => {
                        self.notice(&error.message(self.lang), Role::StatusFailure);
                        return;
                    }
                }
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
        let worker = self.draft.clone().map(DraftWorker::start);
        self.dock.set_draft(None);
        *self.lock() = State::Recording(Active {
            recording,
            since: Instant::now(),
            warm: Some(warm),
            id,
            worker,
            text: String::new(),
        });
        self.dock.set_voice(Some(VoiceLabel::Recording(0, mode)));
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
                            let mode = self.mode();
                            self.dock.set_voice(Some(VoiceLabel::Recording(seconds, mode)));
                        }
                        // The live draft (AGL-122): finished chunks go
                        // to the engine, its finished lines to the
                        // transcript; the final pass replaces them (D7).
                        if let Some(worker) = active.worker.as_ref() {
                            while let Some(chunk) = active.recording.poll_chunk() {
                                if worker.chunks.send(chunk).is_err() {
                                    break;
                                }
                            }
                            // The draft grows in place, not line after
                            // line (AGL-122): the whole text is set
                            // again and the dock shows its last rows.
                            let mut grew = false;
                            while let Ok(text) = worker.texts.try_recv() {
                                if !active.text.is_empty() {
                                    active.text.push(' ');
                                }
                                active.text.push_str(&text);
                                grew = true;
                            }
                            if grew {
                                let text = active.text.clone();
                                self.dock.set_draft(Some(text));
                            }
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
        // D7: the final pass replaces the draft in place — the block
        // goes, the label says what is happening.
        self.dock.set_draft(None);
        self.dock.set_voice(Some(VoiceLabel::Transcribing(self.mode())));
        let me = self.clone();
        thread::spawn(move || {
            let result = me.text_of(active);
            *me.lock() = State::Idle;
            me.dock.set_voice(None);
            // The mode is read last: an Alt-R during transcription still
            // asks (AGL-123, D2).
            let ask = matches!(me.mode(), SendMode::Ask);
            match result {
                Ok(text) if text.is_empty() => {
                    // Silence never sends (AGL-123, D8).
                    me.notice(tr!(me.lang, m::VOICE_NOTHING_HEARD), Role::StatusPending);
                }
                // Ask: the text goes in at the cursor, where the human was
                // writing (D3). Auto: it goes whole and the message sends
                // itself (AGL-123, D1).
                Ok(text) if ask => {
                    me.dock.key(Key::Paste(format!("{text} ")));
                }
                Ok(text) => {
                    me.dock.key(Key::Paste(text));
                    if let Outcome::Submit(message) = me.dock.key(Key::Enter) {
                        let send = me.send.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        if let Some(send) = send {
                            send(message);
                        }
                    }
                }
                Err(error) => me.notice(&error.message(me.lang), Role::StatusFailure),
            }
        });
    }

    /// The send mode of the live dictation (AGL-123).
    fn mode(&self) -> SendMode {
        if self.ask.load(Ordering::Relaxed) {
            SendMode::Ask
        } else {
            SendMode::Auto
        }
    }

    /// The callback that sends the auto mode's message, wired by the input
    /// loop which owns what queuing takes (AGL-123, D1).
    pub fn on_send(&self, send: Sender) {
        *self.send.lock().unwrap_or_else(|e| e.into_inner()) = Some(send);
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
            self.dock.set_draft(None);
            self.dock.set_voice(None);
        }
    }
}
