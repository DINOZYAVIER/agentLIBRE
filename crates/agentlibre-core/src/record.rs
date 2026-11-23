//! Recording the human's voice from the microphone (AGL-016): `ffmpeg`
//! reads the system's default source through the Pulse layer of PipeWire
//! and writes 16 kHz mono WAV, which whisper reads as it is (D4). AGL-122
//! adds a chunking mode: the same stream is cut at speech ends by an
//! energy VAD (D6), so a draft engine can transcribe while the human
//! speaks; `stop` still returns the whole recording for the final pass.

use std::collections::VecDeque;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::Error;

/// How long a recording that is stopped is waited for to finish its file.
const FINISH_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a new recording is watched for an immediate failure (no
/// source, no server of sound).
const START_WATCH: Duration = Duration::from_millis(250);

/// A recording in progress.
pub trait Recording: Send {
    /// Whether it ended by itself: the limit was reached, or `ffmpeg`
    /// stopped.
    fn finished(&mut self) -> bool;
    /// A finished speech chunk as a 16 kHz mono WAV and the millisecond
    /// of the recording it ends at, without waiting (AGL-122, D6);
    /// `None` — none yet, or this mode does not chunk.
    fn poll_chunk(&mut self) -> Option<(Vec<u8>, u64)> {
        None
    }
    /// A finished piece of the whole recording for the final pass
    /// (AGL-122, D12): the audio since the last piece, ending at the
    /// first pause six or more seconds in, forced at twelve; `None` —
    /// none ready, or this mode keeps no whole.
    fn take_final(&mut self) -> Option<(Vec<u8>, u64)> {
        None
    }
    /// The last piece of the whole recording, up to the sample reached
    /// so far (AGL-122, D12); `None` when nothing meaningful is left.
    fn take_final_tail(&mut self) -> Option<(Vec<u8>, u64)> {
        None
    }
    /// Ends it and returns the WAV.
    fn stop(self: Box<Self>) -> Result<Vec<u8>, Error>;
    /// Ends it and keeps nothing.
    fn cancel(self: Box<Self>);
}

/// Starts recording the default source for at most `limit` seconds (D4,
/// D5).
pub fn start(limit: u32) -> Result<Box<dyn Recording>, Error> {
    let ffmpeg = crate::programs::program("ffmpeg")?;
    Ffmpeg::start(ffmpeg.as_os_str(), limit).map(|recording| Box::new(recording) as Box<dyn Recording>)
}

/// Starts a chunking recording of at most `limit` seconds (AGL-122, D6):
/// `poll_chunk` gives the speech chunks as they end, `stop` — the whole
/// recording.
pub fn start_chunked(limit: u32, vad: Vad) -> Result<Box<dyn Recording>, Error> {
    let ffmpeg = crate::programs::program("ffmpeg")?;
    Ffmpeg::start_chunked(ffmpeg.as_os_str(), limit, vad)
        .map(|recording| Box::new(recording) as Box<dyn Recording>)
}

/// The thresholds of the energy VAD that cuts a recording into draft
/// chunks (AGL-122, D6).
#[derive(Clone, Copy, Debug)]
pub struct Vad {
    /// RMS at or above this (of 32767) is speech; −41 dB ≈ 300 suits a
    /// quiet studio mic: the room's hum sits near 200, speech peaks over
    /// 700 — measured on the human's own recordings, not on ffmpeg's
    /// book-example −30 dB, which their levels never reach.
    pub rms_on: i64,
    /// A window leaves the chunker this often, pause or no pause
    /// (AGL-122, D10): the draft answers on the clock, not on phrases.
    pub hop_ms: u32,
    /// How much of the previous window's tail a new window repeats; the
    /// repeated words fall out at the text, where the worker matches
    /// them, not at the audio.
    pub overlap_ms: u32,
}

impl Default for Vad {
    fn default() -> Vad {
        Vad { rms_on: 300, hop_ms: 1200, overlap_ms: 800 }
    }
}

/// A frame: 20 ms of 16 kHz mono audio.
const FRAME_SAMPLES: usize = 320;

/// Cuts a 16 kHz mono PCM stream into rolling draft windows
/// (AGL-122, D10): every `hop_ms` the audio since the last window goes
/// out — the next one starting `overlap_ms` before the previous cut —
/// and a window with no loud frame is dropped whole, silence being
/// cheap to keep but dear to transcribe.
#[derive(Debug)]
pub struct Chunker {
    vad: Vad,
    /// The kept audio, from `base` on; never longer than a window plus
    /// the overlap, so the positions below are `base + kept.len()`.
    kept: VecDeque<i16>,
    base: u64,
    /// Samples fed since the start.
    written: u64,
    /// The pending window's start, `None` while the silence holds.
    start: Option<u64>,
    /// A loud frame came since the last window.
    voiced: bool,
    /// The sample position the next window is due at.
    due: u64,
    hop_samples: u64,
    tail_samples: u64,
}

impl Chunker {
    pub fn new(vad: Vad) -> Chunker {
        Chunker {
            vad,
            kept: VecDeque::new(),
            base: 0,
            written: 0,
            start: None,
            voiced: false,
            due: (vad.hop_ms as u64) * 16,
            hop_samples: (vad.hop_ms as u64) * 16,
            tail_samples: (vad.overlap_ms as u64) * 16,
        }
    }

    /// The RMS of the frame, or `None` when it is not whole.
    fn rms(frame: &[i16]) -> Option<i64> {
        if frame.len() < FRAME_SAMPLES {
            return None;
        }
        let sum: i64 = frame.iter().map(|s| (*s as i64) * (*s as i64)).sum();
        Some(((sum / FRAME_SAMPLES as i64) as f64).sqrt() as i64)
    }

    /// Feeds samples; every finished window comes out as it ends, with
    /// the sample position it ends at.
    pub fn push(&mut self, samples: &[i16]) -> Vec<(u64, Vec<i16>)> {
        let mut out = Vec::new();
        for frame in samples.chunks(FRAME_SAMPLES) {
            self.kept.extend(frame.iter().copied());
            self.written += frame.len() as u64;
            let rms = match Chunker::rms(frame) {
                Some(rms) => rms,
                // The last, short frame of a stream: kept, never judged.
                None => continue,
            };
            if rms >= self.vad.rms_on {
                let back = (self.written - frame.len() as u64)
                    .saturating_sub(self.tail_samples)
                    .max(self.base);
                self.start = Some(self.start.map_or(back, |s| s.min(back)));
                self.voiced = true;
            }
            if self.written >= self.due {
                self.due = self.written + self.hop_samples;
                if self.voiced {
                    out.push(self.cut());
                } else {
                    self.drop_silence();
                }
            }
        }
        out
    }

    /// The pending window at the end of the recording: whatever speech
    /// has no window yet goes out whole, with its end position.
    pub fn flush(&mut self) -> Option<(u64, Vec<i16>)> {
        let start = self.start?;
        self.start = None;
        self.voiced = false;
        if start >= self.written {
            return None;
        }
        Some((self.written, self.take(start, self.written)))
    }

    /// Closes the window, from its start to now, and opens the next one
    /// `overlap_ms` before the cut, so no word is lost on the seam.
    fn cut(&mut self) -> (u64, Vec<i16>) {
        let start = self.start.take().unwrap_or(self.base).max(self.base);
        self.start = Some((self.written - self.tail_samples).max(self.base));
        self.voiced = false;
        (self.written, self.take(start, self.written))
    }

    /// A window of pure silence: nothing to hear, nothing to keep.
    fn drop_silence(&mut self) {
        self.start = None;
        self.voiced = false;
        self.take(self.written, self.written);
    }

    /// The kept samples from `from` to `to`, dropping what lies before
    /// the next chunk can need.
    fn take(&mut self, from: u64, to: u64) -> Vec<i16> {
        debug_assert!(from >= self.base && to <= self.written && to >= from);
        let head = (from - self.base) as usize;
        let chunk: Vec<i16> = self.kept.iter().skip(head).take((to - from) as usize).copied().collect();
        let keep_from = self.start.unwrap_or(self.written);
        if keep_from > self.base {
            self.kept.drain(..(keep_from - self.base) as usize);
            self.base = keep_from;
        }
        chunk
    }
}

/// The final-pass segmenter of the whole recording (AGL-122, D12): the
/// first pause of a quarter second at least six seconds into the piece,
/// a forced cut at twelve, and the next piece starting a second and a
/// half back so no word stands on the seam alone. `scan` remembers how
/// far the pauses were already sought; positions are samples of `full`.
const FINAL_MIN: usize = 6 * 16000;
const FINAL_MAX: usize = 12 * 16000;
const FINAL_PAUSE_FRAMES: usize = 13;
const FINAL_QUIET: i64 = 150;
const FINAL_OVERLAP: usize = 1500 * 16;

fn segment_cut(full: &[i16], next: usize, scan: &mut usize) -> Option<usize> {
    let min = next + FINAL_MIN;
    if full.len() < min {
        return None;
    }
    *scan = (*scan).max(min);
    let limit = (next + FINAL_MAX).min(full.len());
    let mut quiet = 0;
    while *scan + FRAME_SAMPLES <= limit {
        let frame = Chunker::rms(&full[*scan..*scan + FRAME_SAMPLES]).unwrap_or(0);
        if frame < FINAL_QUIET {
            quiet += 1;
        } else {
            quiet = 0;
        }
        *scan += FRAME_SAMPLES;
        if quiet >= FINAL_PAUSE_FRAMES {
            return Some(*scan - quiet * FRAME_SAMPLES + FRAME_SAMPLES);
        }
    }
    (full.len() >= next + FINAL_MAX).then(|| next + FINAL_MAX)
}

/// A 16 kHz mono WAV of `samples`.
fn wav16k(samples: &[i16]) -> Vec<u8> {
    let data = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&16000u32.to_le_bytes());
    out.extend_from_slice(&32000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data as u32).to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// The arguments of `ffmpeg` for a chunking recording: raw PCM to
/// stdout, at most `limit` seconds.
fn ffmpeg_raw_args(limit: u32) -> Vec<std::ffi::OsString> {
    ["-v", "error", "-f", "pulse", "-i", "default", "-ar", "16000", "-ac", "1", "-f", "s16le", "-t"]
        .iter()
        .map(Into::into)
        .chain(std::iter::once(limit.to_string().into()))
        .chain(std::iter::once("-".into()))
        .collect()
}

#[cfg(test)]
mod chunker_tests {
    use super::*;

    fn tone(n: usize) -> Vec<i16> {
        let mut v = Vec::with_capacity(n);
        for i in 0..n {
            v.push(if i / 32 % 2 == 0 { 6000 } else { -6000 });
        }
        v
    }

    fn silence(n: usize) -> Vec<i16> {
        vec![0; n]
    }

    #[test]
    fn a_window_leaves_on_the_clock() {
        // The draft answers on the clock, not on phrases (D10): speech
        // gets its window at the hop even though it never paused.
        let mut c = Chunker::new(Vad::default()); // 1.2 s hop
        let mut chunks = Vec::new();
        chunks.extend(c.push(&tone(20 * FRAME_SAMPLES)));
        assert!(chunks.is_empty(), "the hop has not come yet");
        chunks.extend(c.push(&tone(40 * FRAME_SAMPLES)));
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn windows_come_out_in_order_on_one_breath() {
        let mut c = Chunker::new(Vad::default());
        let mut chunks = Vec::new();
        chunks.extend(c.push(&tone(30 * FRAME_SAMPLES)));
        chunks.extend(c.push(&tone(40 * FRAME_SAMPLES)));
        chunks.extend(c.push(&tone(60 * FRAME_SAMPLES)));
        chunks.extend(c.push(&tone(30 * FRAME_SAMPLES)));
        assert!(chunks.len() >= 2, "{:?}", chunks.len());
    }

    #[test]
    fn a_silent_window_opens_nothing() {
        let mut c = Chunker::new(Vad::default());
        let chunks = c.push(&silence(200 * FRAME_SAMPLES));
        assert!(chunks.is_empty(), "silence never reaches the engine");
        assert!(c.flush().is_none());
        // Speech after a silence starts a fresh window, not a re-heard
        // quarter-hour of dropped quiet.
        let mut chunks = c.push(&tone(30 * FRAME_SAMPLES));
        chunks.extend(c.push(&tone(40 * FRAME_SAMPLES)));
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].1.len() <= 80 * FRAME_SAMPLES, "{:?}", chunks[0].1.len());
    }

    #[test]
    fn the_next_window_repeats_the_tail_of_the_previous() {
        // The seam of consecutive windows carries `overlap_ms` of the
        // same audio; the worker folds the doubled words at the text.
        let vad = Vad { hop_ms: 400, overlap_ms: 300, ..Vad::default() };
        let mut c = Chunker::new(vad);
        let chunks = c.push(&tone(60 * FRAME_SAMPLES));
        assert!(chunks.len() >= 2, "{:?}", chunks.len());
        assert_eq!(&chunks[0].1[chunks[0].1.len() - 4800..], &chunks[1].1[..4800]);
    }

    #[test]
    fn flush_keeps_the_unfinished_tail() {
        let mut c = Chunker::new(Vad::default());
        assert!(c.push(&tone(2 * 20 * FRAME_SAMPLES)).is_empty());
        let tail = c.flush().unwrap();
        assert!(tail.1.len() >= 2 * 20 * FRAME_SAMPLES);
        assert!(c.flush().is_none());
    }

    #[test]
    fn flush_of_silence_only_gives_nothing() {
        let mut c = Chunker::new(Vad::default());
        c.push(&silence(20 * FRAME_SAMPLES));
        assert!(c.flush().is_none());
    }

    #[test]
    fn the_default_thresholds_hear_a_quiet_mic() {
        // The human's levels: speech around 700 RMS, room hum around
        // 190 — the chunker must cut on those, not on book volumes.
        let mut c = Chunker::new(Vad::default());
        let quiet: Vec<i16> = (0..200 * FRAME_SAMPLES)
            .map(|i| if i / 32 % 2 == 0 { 700 } else { -700 })
            .collect();
        let hum: Vec<i16> = (0..40 * FRAME_SAMPLES).map(|i| if i / 8 % 2 == 0 { 190 } else { -190 }).collect();
        let chunks = c.push(&quiet);
        assert!(!chunks.is_empty(), "the human's speech level cuts windows");
        let mut hum_only = Chunker::new(Vad::default());
        let chunks = hum_only.push(&hum);
        assert!(chunks.is_empty(), "the room hum alone opens no window");
    }

    #[test]
    fn a_final_piece_ends_at_a_pause_after_the_floor() {
        // Eight seconds of speech, then a pause: the piece ends there,
        // the pause being past the six-second floor (D12).
        let mut full = tone(8 * 16000);
        full.extend(silence(20 * FRAME_SAMPLES));
        let mut scan = 0;
        let cut = segment_cut(&full, 0, &mut scan).unwrap();
        let pause = 8 * 16000;
        assert!(pause as i64 - cut as i64 <= FRAME_SAMPLES as i64, "{cut}");
    }

    #[test]
    fn a_pause_before_the_floor_is_not_the_end() {
        // A pause at three seconds is too early for a piece's end; with
        // no later pause the force at twelve cuts.
        let mut full = tone(3 * 16000);
        full.extend(silence(16000));
        full.extend(tone(9 * 16000));
        let mut scan = 0;
        assert_eq!(segment_cut(&full, 0, &mut scan), Some(FINAL_MAX));
    }

    #[test]
    fn no_piece_before_the_floor() {
        let full = tone(5 * 16000);
        let mut scan = 0;
        assert_eq!(segment_cut(&full, 0, &mut scan), None);
    }

    #[test]
    fn wav16k_carries_the_samples() {        let bytes = wav16k(&[1, -2]);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(&bytes[44..], &[1, 0, 0xfe, 0xff]);
    }
}

/// The arguments of `ffmpeg` for a recording of `limit` seconds into
/// `path`.
fn ffmpeg_args(limit: u32, path: &Path) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = [
        "-v", "error", "-y", "-f", "pulse", "-i", "default", "-ar", "16000", "-ac", "1", "-t",
    ]
    .iter()
    .map(Into::into)
    .collect();
    args.push(limit.to_string().into());
    args.push(path.into());
    args
}

struct Ffmpeg {
    child: Option<Child>,
    /// The file of a plain recording; a chunking one pipes stdout and
    /// keeps the whole in `full` instead (AGL-122, D6).
    path: PathBuf,
    rx: Option<Receiver<Vec<i16>>>,
    chunker: Option<Chunker>,
    full: Vec<i16>,
    /// Windows a single read cut but this poll has not given up yet,
    /// each with the sample it ends at.
    pending: std::collections::VecDeque<(Vec<i16>, u64)>,
    /// Where the next final piece starts in `full` (AGL-122, D12).
    final_next: usize,
    /// How far the pauses for it were already sought.
    final_scan: usize,
}

impl Ffmpeg {
    fn start(program: &std::ffi::OsStr, limit: u32) -> Result<Ffmpeg, Error> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "agentLIBRE-voice-{}-{n}.wav",
            std::process::id()
        ));
        let mut command = Command::new(program);
        command
            .args(ffmpeg_args(limit, &path))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // The thread that starts it may block SIGINT (the REPL's input
        // thread does, D131), or the process may ignore it (`cargo test`
        // without a terminal does), and a child keeps the mask and the
        // disposition of the parent: ffmpeg must get the SIGINT that
        // makes it finish its file.
        unsafe {
            command.pre_exec(|| {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = libc::SIG_DFL;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
                Ok(())
            });
        }
        let child = command.spawn().map_err(|source| crate::media::Error::Ffmpeg {
            detail: source.to_string(),
        })?;
        let mut recording = Ffmpeg {
            child: Some(child),
            path,
            rx: None,
            chunker: None,
            full: Vec::new(),
            pending: std::collections::VecDeque::new(),
            final_next: 0,
            final_scan: 0,
        };
        // No source or no sound server: ffmpeg fails at once, and the
        // human is told before speaking.
        let watched = Instant::now();
        while watched.elapsed() < START_WATCH {
            if recording.exited().is_some() {
                return Err(crate::media::Error::Ffmpeg {
                    detail: recording.failure(),
                }
                .into());
            }
            thread::sleep(Duration::from_millis(25));
        }
        Ok(recording)
    }

    /// A chunking recording: raw PCM from stdout, cut by the energy VAD
    /// (AGL-122, D6). The same SIGINT care as in `start`.
    fn start_chunked(program: &std::ffi::OsStr, limit: u32, vad: Vad) -> Result<Ffmpeg, Error> {
        let mut command = Command::new(program);
        command
            .args(ffmpeg_raw_args(limit))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = libc::SIG_DFL;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|source| crate::media::Error::Ffmpeg {
            detail: source.to_string(),
        })?;
        let stdout = child.stdout.take().ok_or(crate::media::Error::Ffmpeg {
            detail: "no stdout of the chunking recorder".into(),
        })?;
        let (tx, rx) = std::sync::mpsc::channel::<Vec<i16>>();
        thread::spawn(move || {
            use std::io::Read;
            let mut reader = stdout;
            let mut bytes = Vec::with_capacity(64 * 1024);
            let mut buf = [0u8; 8 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => bytes.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
                let whole = bytes.len() / 2 * 2;
                if whole >= 2 {
                    let raw: Vec<u8> = bytes.drain(..whole).collect();
                    let samples: Vec<i16> =
                        raw.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
                    let _ = tx.send(samples);
                }
            }
        });
        let recording = Ffmpeg {
            child: Some(child),
            path: PathBuf::new(),
            rx: Some(rx),
            chunker: Some(Chunker::new(vad)),
            full: Vec::new(),
            pending: std::collections::VecDeque::new(),
            final_next: 0,
            final_scan: 0,
        };
        // As in `start`: no source or no sound server fails at once.
        let watched = Instant::now();
        let mut recording = recording;
        while watched.elapsed() < START_WATCH {
            if recording.exited().is_some() {
                return Err(crate::media::Error::Ffmpeg {
                    detail: recording.failure(),
                }
                .into());
            }
            thread::sleep(Duration::from_millis(25));
        }
        Ok(recording)
    }

    /// The exit status when the process has ended.
    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.as_mut()?.try_wait().ok().flatten()
    }

    /// What `ffmpeg` said, after it failed.
    fn failure(&mut self) -> String {
        use std::io::Read;
        let mut text = String::new();
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut text);
            }
        }
        let _ = fs::remove_file(&self.path);
        let text = text.trim();
        if text.is_empty() {
            "ffmpeg stopped without a recording".into()
        } else {
            text.into()
        }
    }
}

impl Recording for Ffmpeg {
    fn finished(&mut self) -> bool {
        self.child.is_none() || self.exited().is_some()
    }

    fn poll_chunk(&mut self) -> Option<(Vec<u8>, u64)> {
        if let Some((chunk, end)) = self.pending.pop_front() {
            return Some((wav16k(&chunk), end / 16));
        }
        let rx = self.rx.as_mut()?;
        loop {
            match rx.try_recv() {
                Ok(samples) => {
                    self.full.extend(samples.iter().copied());
                    let chunks = self.chunker.as_mut()?.push(&samples);
                    let mut queue = std::collections::VecDeque::<(Vec<i16>, u64)>::from(
                        chunks.into_iter().map(|(end, chunk)| (chunk, end)).collect::<Vec<_>>(),
                    );
                    if let Some((chunk, end)) = queue.pop_front() {
                        self.pending = queue;
                        return Some((wav16k(&chunk), end / 16));
                    }
                }
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => {
                    self.rx = None;
                    let tail = self
                        .chunker
                        .as_mut()
                        .and_then(|c| c.flush())
                        .map(|(end, chunk)| (wav16k(&chunk), end / 16));
                    return tail;
                }
            }
        }
    }

    fn take_final(&mut self) -> Option<(Vec<u8>, u64)> {
        self.chunker.as_ref()?;
        let cut = segment_cut(&self.full, self.final_next, &mut self.final_scan)?;
        let piece = wav16k(&self.full[self.final_next..cut]);
        self.final_next = cut.saturating_sub(FINAL_OVERLAP);
        Some((piece, (cut / 16) as u64))
    }

    fn take_final_tail(&mut self) -> Option<(Vec<u8>, u64)> {
        self.chunker.as_ref()?;
        let len = self.full.len();
        if len < self.final_next + FINAL_OVERLAP / 5 {
            return None;
        }
        let piece = wav16k(&self.full[self.final_next..len]);
        self.final_next = len;
        Some((piece, (len / 16) as u64))
    }

    fn stop(mut self: Box<Self>) -> Result<Vec<u8>, Error> {
        let Some(mut child) = self.child.take() else {
            return Err(crate::media::Error::Ffmpeg {
                detail: "ffmpeg stopped without a recording".into(),
            }
            .into());
        };
        // SIGINT lets ffmpeg finish the file; one that ended by itself
        // at the limit is done already.
        if child.try_wait().ok().flatten().is_none() {
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
        }
        let waited = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if waited.elapsed() < FINISH_TIMEOUT => {
                    thread::sleep(Duration::from_millis(20));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        // The chunking mode keeps the whole in the pipe; the tail of an
        // unfinished speech segment belongs to it too (AGL-122, D6).
        if self.rx.take().is_some() {
            let drained = Instant::now();
            while let Some(rx) = self.rx.as_mut() {
                match rx.try_recv() {
                    Ok(samples) => {
                        self.full.extend(samples);
                        if drained.elapsed() >= FINISH_TIMEOUT {
                            break;
                        }
                    }
                    Err(TryRecvError::Empty) => {
                        thread::sleep(Duration::from_millis(10));
                        if drained.elapsed() >= FINISH_TIMEOUT {
                            break;
                        }
                    }
                    Err(TryRecvError::Disconnected) => {
                        self.rx = None;
                    }
                }
            }
            // The whole already holds every sample, the unfinished
            // window too: the chunker's copy must not be added again
            // (the final pass would hear those seconds twice).
            let bytes = wav16k(&self.full);
            if bytes.len() > 44 {
                return Ok(bytes);
            }
            return Err(crate::media::Error::Ffmpeg {
                detail: "ffmpeg stopped without a recording".into(),
            }
            .into());
        }
        let bytes = fs::read(&self.path);
        let _ = fs::remove_file(&self.path);
        match (status, bytes) {
            (_, Ok(bytes)) if bytes.len() > 44 => Ok(bytes),
            _ => Err(crate::media::Error::Ffmpeg {
                detail: "ffmpeg stopped without a recording".into(),
            }
            .into()),
        }
    }

    fn cancel(mut self: Box<Self>) {
        self.kill();
    }
}

impl Ffmpeg {
    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for Ffmpeg {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A directory with a script that stands for `ffmpeg`.
    struct Fake(PathBuf);

    impl Fake {
        fn new(name: &str, body: &str) -> Fake {
            let dir = std::env::temp_dir().join(format!("agentLIBRE-record-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let script = dir.join("ffmpeg");
            fs::write(&script, format!("#!/bin/sh\nfor last; do :; done\n{body}\n")).unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            Fake(script)
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    /// Starts the fake. A script that was just written may be busy for a
    /// moment, when another test forks while its file is open; it is tried
    /// again.
    fn start_fake(fake: &Fake, limit: u32) -> Result<Ffmpeg, Error> {
        let mut tries = 0;
        loop {
            match Ffmpeg::start(fake.0.as_os_str(), limit) {
                Err(Error::Media(crate::media::Error::Ffmpeg { detail })) if detail.contains("busy") && tries < 50 => {
                    tries += 1;
                    thread::sleep(Duration::from_millis(20));
                }
                other => return other,
            }
        }
    }

    /// A WAV long enough for `stop` to take it for a recording.
    const WRITE: &str = "head -c 100 /dev/zero > \"$last\"";

    #[test]
    fn the_command_records_the_default_source_as_16_khz_mono() {
        let args = ffmpeg_args(60, Path::new("/t/v.wav"));
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            ["-v", "error", "-y", "-f", "pulse", "-i", "default", "-ar", "16000", "-ac", "1", "-t", "60", "/t/v.wav"]
        );
    }

    #[test]
    fn stop_finishes_the_file_and_returns_it() {
        // Writes on SIGINT, as ffmpeg finishes its file.
        let fake = Fake::new("stop", &format!("trap '{WRITE}; exit 0' INT\nwhile true; do sleep 0.02; done"));
        let mut recording = start_fake(&fake, 60).unwrap();
        assert!(!recording.finished());
        let path = recording.path.clone();
        let bytes = Box::new(recording).stop().unwrap();
        assert_eq!(bytes.len(), 100);
        assert!(!path.exists());
    }

    #[test]
    fn stop_works_when_the_thread_that_starts_it_blocks_sigint() {
        // As the input thread of the REPL: SIGINT is blocked in it, and a
        // child would keep the mask (the live bug of AGL-016).
        let fake = Fake::new("mask", &format!("trap '{WRITE}; exit 0' INT\nwhile true; do sleep 0.02; done"));
        let program = fake.0.clone();
        let handle = thread::spawn(move || {
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGINT);
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            }
            let fake = Fake(program);
            let recording = start_fake(&fake, 60).unwrap();
            std::mem::forget(fake);
            let started = Instant::now();
            let bytes = Box::new(recording).stop().unwrap();
            (bytes.len(), started.elapsed())
        });
        let (len, elapsed) = handle.join().unwrap();
        assert_eq!(len, 100);
        // Finished by the signal, not by the timeout that kills it.
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    }

    #[test]
    fn cancel_keeps_nothing() {
        let fake = Fake::new("cancel", &format!("{WRITE}\nwhile true; do sleep 0.02; done"));
        let recording = start_fake(&fake, 60).unwrap();
        let path = recording.path.clone();
        assert!(path.exists());
        Box::new(recording).cancel();
        assert!(!path.exists());
    }

    #[test]
    fn a_recording_that_ends_at_the_limit_is_finished_and_stops_with_its_file() {
        let fake = Fake::new("limit", &format!("sleep 0.4\n{WRITE}"));
        let mut recording = start_fake(&fake, 1).unwrap();
        let waited = Instant::now();
        while !recording.finished() {
            assert!(waited.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(Box::new(recording).stop().unwrap().len(), 100);
    }

    #[test]
    fn an_ffmpeg_that_fails_at_once_says_why_and_a_missing_one_is_an_error() {
        let fake = Fake::new("fail", "echo 'default: Connection refused' >&2\nexit 1");
        let error = start_fake(&fake, 60).map(|_| ()).unwrap_err();
        assert!(matches!(&error, Error::Media(crate::media::Error::Ffmpeg { detail }) if detail.contains("Connection refused")));
        let missing = Ffmpeg::start("/no/such/ffmpeg".as_ref(), 60).map(|_| ()).unwrap_err();
        assert!(matches!(missing, Error::Media(crate::media::Error::Ffmpeg { .. })));
    }

    #[test]
    fn dropping_a_recording_stops_the_process_and_removes_the_file() {
        let fake = Fake::new("drop", &format!("{WRITE}\nwhile true; do sleep 0.02; done"));
        let recording = start_fake(&fake, 60).unwrap();
        let path = recording.path.clone();
        drop(recording);
        assert!(!path.exists());
    }

    #[test]
    fn a_chunking_recording_gives_chunks_while_it_runs_and_the_whole_at_stop() {
        // Stands for ffmpeg: three speech segments, raw s16le to stdout;
        // a loud frame is 0x10 bytes (an RMS far over the threshold),
        // silence comes from /dev/zero.
        let dir = std::env::temp_dir().join(format!("agentLIBRE-record-pcm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let loud = dir.join("loud");
        {
            use std::io::Write;
            let mut f = fs::File::create(&loud).unwrap();
            f.write_all(&vec![0x10u8; 20 * FRAME_SAMPLES * 2]).unwrap();
        }
        let script = dir.join("ffmpeg");
        let loud_text = loud.to_string_lossy().into_owned();
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ntrap 'exit 0' INT\nfor i in 1 2 3; do cat {loud}; cat /dev/zero | head -c 32000; done\ncat {loud} | head -c 6400\ncat /dev/zero | head -c 32000\nwhile true; do sleep 0.05; done\n",
                loud = loud_text
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let mut recording = Ffmpeg::start_chunked(script.as_os_str(), 60, Vad::default()).unwrap();
        let got = Instant::now();
        let mut chunks = Vec::new();
        while chunks.len() < 3 && got.elapsed() < Duration::from_secs(10) {
            while let Some((chunk, _end)) = recording.poll_chunk() {
                assert!(chunk.len() > 44);
                assert_eq!(&chunk[0..4], b"RIFF");
                chunks.push(chunk);
            }
            thread::sleep(Duration::from_millis(30));
        }
        assert!(chunks.len() >= 2, "chunks: {}", chunks.len());
        let whole = Box::new(recording).stop().unwrap();
        // The whole carries every segment: far more than any single
        // chunk, plus the 0.2 s tail of the unfinished one.
        assert!(whole.len() > chunks[0].len() + 44 * 100, "{}", whole.len());
        assert_eq!(&whole[0..4], b"RIFF");
        let _ = fs::remove_dir_all(&dir);
    }
}
