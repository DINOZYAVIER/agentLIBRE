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
use super::card::notice_lines;
use super::transcript::Screen;
use super::Notice;
use agentlibre_surface::Error;
use agentlibre_surface::dictation::{Dictation, DraftEngine, DraftWorker, compose, merge_draft};
use agentlibre_common::i18n::{Lang, m};
use agentlibre_surface::record::Recording;
use crate::theme::Theme;
use agentlibre_common::tr;

/// How often a recording is looked at: its time is shown and its end at
/// the limit is seen.
const WATCH: Duration = Duration::from_millis(100);

/// Starts a recording of at most the given number of seconds.
pub type Recorder = Box<dyn Fn(u32) -> Result<Box<dyn Recording>, agentlibre_surface::Error> + Send + Sync>;

/// Queues an auto-sent transcript (AGL-123, D1).
pub type Sender = Arc<dyn Fn(String) + Send + Sync>;

/// A piece of the recording for the final pass (AGL-122, D12): the
/// audio, the confirmed text before it as prompt, and where it ends.
struct FinalSeg {
    wav: Vec<u8>,
    prompt: String,
    end: u64,
}

/// The final engine at work while the human still speaks (AGL-122,
/// D12): one thread turns pieces into confirmed text strictly in the
/// order of the recording, each with the tail of the text before it as
/// prompt; the answers carry the end position back.
struct FinalWorker {
    req: MpscSender<FinalSeg>,
    res: Receiver<(u64, Result<String, Error>)>,
    _thread: JoinHandle<()>,
}

impl FinalWorker {
    fn start(dictation: Arc<dyn Dictation>, warm: Option<JoinHandle<Result<(), Error>>>) -> FinalWorker {
        let (ctx, crx) = mpsc::channel::<FinalSeg>();
        let (tx, rx) = mpsc::channel::<(u64, Result<String, Error>)>();
        let thread = thread::spawn(move || {
            // The server must be up before the first piece asks (D6).
            if let Some(warm) = warm {
                let _ = warm.join();
            }
            for seg in crx.iter() {
                let prompt = if seg.prompt.is_empty() { None } else { Some(seg.prompt.as_str()) };
                let answer =
                    dictation.transcribe(&seg.wav, prompt, &mut |_| {});
                if tx.send((seg.end, answer)).is_err() {
                    break;
                }
            }
        });
        FinalWorker { req: ctx, res: rx, _thread: thread }
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
    /// The draft windows answered so far, each ending at its position
    /// in the recording; only those beyond the confirmed pieces show (D12).
    drafts: Vec<(u64, String)>,
    /// The final engine following the recording piece by piece (D12).
    final_worker: Option<FinalWorker>,
    /// The confirmed text of the pieces answered so far.
    final_text: String,
    /// Where the confirmed text reaches, in recording milliseconds.
    final_upto: u64,
    /// Pieces sent to the final engine, answers not all back yet.
    final_sent: usize,
    /// A piece failed: the confirmed text cannot answer for the whole.
    final_failed: bool,
}

enum State {
    Idle,
    Recording(Box<Active>),
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

    fn notice(&self, text: &str, kind: Notice) {
        let columns = self.dock.columns();
        self.dock
            .print(&notice_lines(&self.theme, text, columns, kind.role()));
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
            self.notice(&error.message(self.lang), Notice::Failure);
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
                    self.notice(&error.message(self.lang), Notice::Failure);
                    return;
                }
                match (self.recorder)(self.limit) {
                    Ok(recording) => {
                        self.notice(&error.message(self.lang), Notice::Info);
                        recording
                    }
                    Err(error) => {
                        self.notice(&error.message(self.lang), Notice::Failure);
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
                    .warm_up(&mut |line| me.notice(line, Notice::Info))
            })
        };
        let worker = self.draft.clone().map(DraftWorker::start);
        // The final pass follows the recording piece by piece (D12);
        // the server-warming thread reports to it before the first ask.
        let (warm, final_worker) = if chunking {
            (None, Some(FinalWorker::start(self.dictation.clone(), Some(warm))))
        } else {
            (Some(warm), None)
        };
        self.dock.set_draft(None);
        *self.lock() = State::Recording(Box::new(Active {
            recording,
            since: Instant::now(),
            warm,
            id,
            worker,
            drafts: Vec::new(),
            final_worker,
            final_text: String::new(),
            final_upto: 0,
            final_sent: 0,
            final_failed: false,
        }));
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
                        // The live draft (AGL-122): finished windows go
                        // to the engine; its finished text, with the
                        // confirmed pieces under it, fills the block (D12).
                        let mut grew = false;
                        if let Some(worker) = active.worker.as_ref() {
                            while let Some((chunk, end)) = active.recording.poll_chunk() {
                                if worker.chunks.send((chunk, end)).is_err() {
                                    break;
                                }
                            }
                            while let Ok((text, end)) = worker.texts.try_recv() {
                                active.drafts.push((end, text));
                                grew = true;
                            }
                        }
                        if let Some(finalw) = active.final_worker.as_ref() {
                            // A piece of the recording leaves for the
                            // final engine when its pause comes (D12).
                            while let Some((piece, end)) = active.recording.take_final() {
                                let seg = FinalSeg {
                                    wav: piece,
                                    prompt: active.final_text.clone(),
                                    end,
                                };
                                if finalw.req.send(seg).is_err() {
                                    break;
                                }
                                active.final_sent += 1;
                            }
                            // A confirmed piece replaces the draft words
                            // it covers; the block grows exact (D12).
                            while let Ok((end, answer)) = finalw.res.try_recv() {
                                match answer {
                                    Ok(text) => {
                                        active.final_text = merge_draft(&active.final_text, &text);
                                        active.final_upto = active.final_upto.max(end);
                                    }
                                    Err(_) => active.final_failed = true,
                                }
                                // One answer less stands between the
                                // pieces sent and the text gathered.
                                active.final_sent -= 1;
                                grew = true;
                            }
                            if active.final_upto > 0 {
                                // Draft windows the confirmed text has
                                // passed no longer show.
                                let first = active
                                    .drafts
                                    .iter()
                                    .position(|(end, _)| *end > active.final_upto)
                                    .unwrap_or(active.drafts.len());
                                active.drafts.drain(..first.saturating_sub(1));
                            }
                        }
                        if grew {
                            let text =
                                compose(&active.final_text, &active.drafts, active.final_upto);
                            self.dock.set_draft(Some(text));
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
                    me.notice(tr!(me.lang, m::VOICE_NOTHING_HEARD), Notice::Pending);
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
                Err(error) => me.notice(&error.message(me.lang), Notice::Failure),
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

    fn text_of(&self, active: Box<Active>) -> Result<String, Error> {
        let Active {
            mut recording, warm, mut final_worker, mut final_text, final_sent, final_failed, ..
        } = *active;
        // The last piece of the recording joins the rest (D12); the
        // whole WAV is kept for the answer of the fallback pass.
        let tail = recording.take_final_tail();
        let wav = recording.stop()?;
        if final_worker.is_some() && (final_sent > 0 || tail.is_some()) && !final_failed {
            let fw = final_worker.take().unwrap();
            let mut pending = final_sent;
            if let Some((piece, end)) = tail {
                let seg = FinalSeg { wav: piece, prompt: final_text.clone(), end };
                if fw.req.send(seg).is_ok() {
                    pending += 1;
                }
            }
            drop(fw.req);
            let mut exact = true;
            for _ in 0..pending {
                match fw.res.recv() {
                    Ok((_, Ok(text))) => final_text = merge_draft(&final_text, &text),
                    _ => {
                        exact = false;
                        break;
                    }
                }
            }
            if exact {
                return Ok(final_text.trim().to_owned());
            }
            // A piece that failed: the whole recording answers instead.
        }
        if let Some(warm) = warm {
            // The server that was starting must answer first; if it could
            // not start, that is the error.
            match warm.join() {
                Ok(result) => result?,
                Err(_) => return Err(agentlibre_common::media::Error::Ffmpeg { detail: "the thread of the server failed".into() }.into()),
            }
        }
        let text = self
            .dictation
            .transcribe(&wav, None, &mut |line| self.notice(line, Notice::Info))?;
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
