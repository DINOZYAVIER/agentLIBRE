//! Media of a message (AGL-014): an image, a video or an audio the human
//! attaches, kept by a session as a copy named by its sha256 (D7).
//!
//! The text of a message holds a placeholder (`[Image#1]`, `[Video#1]`,
//! `[Audio#1]`, numbered by kind, D21); the session keeps the table that
//! says what each one is. When a step is rendered the placeholders become
//! the server's media marker and the bytes go with the request (D6).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::backend::error::Error as BackendError;
use crate::backend::{Backend, Message};
use crate::error;
use agentlibre_forge::forge::sha256;
use agentlibre_common::i18n::{Lang, Msg};
use agentlibre_common::tr;
use agentlibre_common::error::Error as CommonError;
use agentlibre_common::media::{Error as MediaError, Kind, placeholder, placeholders};

/// What goes wrong with a medium of a session: its copy changed, or what
/// the client's side of media says (AGL-014; AGL-121, D66; AGL-127, D6).
#[derive(Debug)]
pub enum Error {
    /// A medium the session keeps is not what its name says (AGL-014, D7).
    Changed { path: PathBuf },
    /// A file that is no medium, or `ffmpeg` failing on one.
    Common(MediaError),
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::Changed { path } => tr!(lang, CHANGED, path = path.display()),
            Self::Common(error) => error.message(lang),
        }
    }
}

impl From<MediaError> for Error {
    fn from(source: MediaError) -> Error {
        Error::Common(source)
    }
}

/// `Error::Changed` (AGL-014, D7).
const CHANGED: Msg = Msg {
    en: "{path} is not what its name says (the copy of a medium of the session was changed)",
    uk: "{path} не те, що в його назві (копію медіа сесії змінено)",
};

/// The most frames taken from a video (D16).
pub const MAX_FRAMES: usize = 32;
/// The frames are scaled so that their long edge is at most this (D16).
pub const FRAME_EDGE: u32 = 640;

/// A frame of a video: its time in seconds and its picture (a copy named
/// by its sha256, with the extension `jpg`).
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub at: f64,
    pub sha256: String,
}

/// A medium of a session. Its bytes are `<media dir>/<sha256>.<ext>`.
#[derive(Clone, Debug, PartialEq)]
pub struct Media {
    pub kind: Kind,
    /// The number in the placeholder, from 1 for each kind (D21).
    pub number: u32,
    pub sha256: String,
    pub ext: String,
    /// The name the file had.
    pub name: String,
    /// What whisper heard, for an audio the model does not hear itself
    /// (D22).
    pub transcript: Option<String>,
    /// The frames of a video (D8).
    pub frames: Vec<Frame>,
}

impl Media {
    pub fn placeholder(&self) -> String {
        placeholder(self.kind, self.number)
    }

    /// The file of the medium in `dir`.
    pub fn path(&self, dir: &Path) -> PathBuf {
        file_in(dir, &self.sha256, &self.ext)
    }
}

/// `m:ss`, for the time of a frame.
pub fn clock(seconds: f64) -> String {
    let whole = seconds.max(0.0).round() as u64;
    format!("{}:{:02}", whole / 60, whole % 60)
}

/// The lowercase hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    ::hex::encode(sha256(bytes))
}

/// `bytes` in standard base64 with padding, by the `base64` crate already in
/// the build (AGL-121, D34).
pub fn base64(bytes: &[u8]) -> String {
    use ::base64::Engine as _;
    ::base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// What the placeholders of the messages of a step became.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Expanded {
    /// The bytes of the media, base64, in the order of the markers.
    pub uploads: Vec<String>,
    /// How many of the markers stand for a picture (an image or a frame
    /// of a video); their tokens are bounded (D10).
    pub pictures: usize,
    /// The index, in `messages`, of the last user message whose medium stood
    /// as the text of a server with no projector; `None` when there is none
    /// (AGL-137, D1).
    pub unseen: Option<usize>,
}

/// What a medium stands as when the server has no projector (AGL-137, D1),
/// as the `view` tool says it.
const NO_PROJECTOR: &str = "the server has no multimodal projector, so it cannot take images";

/// The server's media marker, asked once (AGL-137, D2): `Some` with a
/// projector, `None` when it has none and a medium then stands as a text
/// instead of failing (D1). Any other failure of `media_marker` fails.
fn marker_of<'a>(
    asked: &'a mut Option<Option<String>>,
    backend: &dyn Backend,
) -> Result<Option<&'a str>, error::Error> {
    if asked.is_none() {
        *asked = Some(match backend.media_marker() {
            Ok(marker) => Some(marker),
            Err(error) => match error {
                error::Error::Backend(BackendError::NoMultimodal) => None,
                error => return Err(error),
            },
        });
    }
    Ok(asked.as_ref().unwrap().as_deref())
}

/// Replaces the placeholders in the user messages and the tool results
/// of `messages` with what the model reads (AGL-014): an image the
/// server's marker; a video its frames, each after its time; an audio
/// its transcript, or the marker when the model hears it (D19, D22). A
/// placeholder the table does not know stays text. A medium that needs
/// the marker stands as a text when the server has no projector (AGL-137,
/// D1), rather than failing; only the newest user message of a step fails.
pub fn expand(
    messages: &mut [Message],
    table: &[&Media],
    dir: &Path,
    backend: &dyn Backend,
) -> Result<Expanded, error::Error> {
    let mut expanded = Expanded::default();
    let mut marker: Option<Option<String>> = None;
    let mut hears: Option<bool> = None;
    for (at, message) in messages.iter_mut().enumerate() {
        let user = matches!(message, Message::User(_));
        let text = match message {
            Message::User(text) => text,
            Message::Tool { content, .. } => content,
            _ => continue,
        };
        let found = placeholders(text);
        if found.is_empty() {
            continue;
        }
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for item in found {
            let Some(medium) = table
                .iter()
                .find(|m| m.kind == item.kind && m.number == item.number)
            else {
                continue;
            };
            out.push_str(&text[last..item.start]);
            last = item.end;
            match medium.kind {
                Kind::Image => match marker_of(&mut marker, backend)? {
                    Some(marker) => {
                        let bytes = read(dir, &medium.sha256, &medium.ext)?;
                        expanded.uploads.push(base64(&bytes));
                        expanded.pictures += 1;
                        out.push_str(marker);
                    }
                    None => {
                        out.push_str(&format!("[Image#{}: {NO_PROJECTOR}]", medium.number));
                        if user {
                            expanded.unseen = Some(at);
                        }
                    }
                },
                Kind::Video => match marker_of(&mut marker, backend)? {
                    Some(marker) => {
                        out.push_str(&format!("[Video#{}: frames", medium.number));
                        for frame in &medium.frames {
                            let bytes = read(dir, &frame.sha256, "jpg")?;
                            expanded.uploads.push(base64(&bytes));
                            expanded.pictures += 1;
                            out.push_str(&format!(" {} {marker}", clock(frame.at)));
                        }
                        out.push(']');
                    }
                    None => {
                        out.push_str(&format!("[Video#{}: {NO_PROJECTOR}]", medium.number));
                        if user {
                            expanded.unseen = Some(at);
                        }
                    }
                },
                Kind::Audio => match &medium.transcript {
                    Some(transcript) => {
                        out.push_str(&format!("[Audio#{}: \"{transcript}\"]", medium.number));
                    }
                    None => {
                        if hears.is_none() {
                            hears = Some(backend.hears_audio()?);
                        }
                        if hears != Some(true) {
                            out.push_str(&format!("[Audio#{}: not transcribed]", medium.number));
                        } else if let Some(marker) = marker_of(&mut marker, backend)? {
                            let bytes = read(dir, &medium.sha256, &medium.ext)?;
                            expanded.uploads.push(base64(&bytes));
                            out.push_str(marker);
                        } else {
                            out.push_str(&format!("[Audio#{}: {NO_PROJECTOR}]", medium.number));
                            if user {
                                expanded.unseen = Some(at);
                            }
                        }
                    }
                },
            }
        }
        out.push_str(&text[last..]);
        *text = out;
    }
    Ok(expanded)
}

fn file_in(dir: &Path, sha256: &str, ext: &str) -> PathBuf {
    dir.join(format!("{sha256}.{ext}"))
}

/// Writes `bytes` to `dir` as `<sha256>.<ext>` unless it is there; returns
/// the sha256 (D7).
pub fn put(dir: &Path, bytes: &[u8], ext: &str) -> Result<String, error::Error> {
    let sha = sha256_hex(bytes);
    let path = file_in(dir, &sha, ext);
    if path.is_file() {
        return Ok(sha);
    }
    fs::create_dir_all(dir).map_err(|source| error::Error::from(CommonError::Write {
        path: dir.into(),
        source,
    }))?;
    let temp = dir.join(format!(".{sha}.{}.tmp", std::process::id()));
    fs::write(&temp, bytes).map_err(|source| error::Error::from(CommonError::Write {
        path: temp.clone(),
        source,
    }))?;
    fs::rename(&temp, &path).map_err(|source| error::Error::from(CommonError::Write { path, source }))?;
    Ok(sha)
}

/// Reads `<sha256>.<ext>` from `dir` and checks it against its name.
pub fn read(dir: &Path, sha256: &str, ext: &str) -> Result<Vec<u8>, error::Error> {
    let path = file_in(dir, sha256, ext);
    let bytes = fs::read(&path).map_err(|source| error::Error::from(CommonError::Read {
        path: path.clone(),
        source,
    }))?;
    if sha256_hex(&bytes) != sha256 {
        return Err(Error::Changed { path }.into());
    }
    Ok(bytes)
}

/// Times, in seconds, of the frames taken from a video of `duration`
/// seconds: one per second, at most [`MAX_FRAMES`], evenly over a longer
/// video (D8, D16).
pub fn frame_times(duration: f64) -> Vec<f64> {
    if !duration.is_finite() || duration <= 0.0 {
        return vec![0.0];
    }
    if duration <= MAX_FRAMES as f64 {
        let count = (duration.ceil() as usize).max(1);
        return (0..count).map(|i| i as f64).collect();
    }
    let step = duration / MAX_FRAMES as f64;
    (0..MAX_FRAMES).map(|i| i as f64 * step).collect()
}

/// The length of a video, from `ffprobe`.
fn video_duration(path: &Path) -> Result<f64, error::Error> {
    let output = Command::new(agentlibre_common::programs::program("ffprobe")?)
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| MediaError::Ffmpeg {
            detail: source.to_string(),
        })?;
    if !output.status.success() {
        return Err(MediaError::Ffmpeg {
            detail: String::from_utf8_lossy(&output.stderr).trim().into(),
        }
        .into());
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|_| {
            MediaError::Ffmpeg {
                detail: format!("no duration in {}", path.display()),
            }
            .into()
        })
}

/// The frames of the video at `path` as JPEG pictures with their times
/// (D8, D16).
pub fn video_frames(path: &Path) -> Result<Vec<(f64, Vec<u8>)>, error::Error> {
    let times = frame_times(video_duration(path)?);
    let ffmpeg = agentlibre_common::programs::program("ffmpeg")?;
    let scale = format!(
        "scale=w='if(gt(iw,ih),min({FRAME_EDGE},iw),-2)':h='if(gt(iw,ih),-2,min({FRAME_EDGE},ih))'"
    );
    let mut frames = Vec::new();
    for at in times {
        let output = Command::new(&ffmpeg)
            .args(["-v", "error", "-ss", &format!("{at:.3}"), "-i"])
            .arg(path)
            .args(["-frames:v", "1", "-vf", &scale, "-q:v", "4", "-f", "image2pipe", "-c:v", "mjpeg", "-"])
            .stdin(Stdio::null())
            .output()
            .map_err(|source| MediaError::Ffmpeg {
                detail: source.to_string(),
            })?;
        if !output.status.success() {
            return Err(MediaError::Ffmpeg {
                detail: String::from_utf8_lossy(&output.stderr).trim().into(),
            }
            .into());
        }
        // A time past the last frame gives nothing: that frame is skipped.
        if !output.stdout.is_empty() {
            frames.push((at, output.stdout));
        }
    }
    if frames.is_empty() {
        return Err(MediaError::Ffmpeg {
            detail: format!("no frames in {}", path.display()),
        }
        .into());
    }
    Ok(frames)
}

/// The loudest sample of a recording with no sound in it, of 32768: under 50 dB
/// below the full scale, as with a muted microphone (AGL-051).
const SILENT_PEAK: i32 = 100;

/// Whether a 16-bit WAV, as the recording makes it, holds no sound. One that
/// cannot be read as such is not called silent.
pub fn wav_is_silent(wav: &[u8]) -> bool {
    let Some(at) = wav.windows(4).position(|tag| tag == b"data") else {
        return false;
    };
    let samples = wav.get(at + 8..).unwrap_or_default();
    let (samples, _) = samples.as_chunks::<2>();
    samples
        .iter()
        .all(|sample| i32::from(i16::from_le_bytes(*sample)).abs() < SILENT_PEAK)
}

/// An audio as 16 kHz mono WAV, what whisper reads.
pub fn audio_to_wav(bytes: &[u8]) -> Result<Vec<u8>, error::Error> {
    let mut child = Command::new(agentlibre_common::programs::program("ffmpeg")?)
        .args(["-v", "error", "-i", "pipe:0", "-ar", "16000", "-ac", "1", "-f", "wav", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| MediaError::Ffmpeg {
            detail: source.to_string(),
        })?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = bytes.to_vec();
    // ffmpeg may stop reading early; the writer runs apart so that a full
    // pipe does not block the reader.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let output = child.wait_with_output().map_err(|source| MediaError::Ffmpeg {
        detail: source.to_string(),
    })?;
    let _ = writer.join();
    if !output.status.success() || output.stdout.is_empty() {
        return Err(MediaError::Ffmpeg {
            detail: String::from_utf8_lossy(&output.stderr).trim().into(),
        }
        .into());
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_recording_with_no_sound_is_silent() {
        let wav = |samples: &[i16]| {
            let mut bytes = b"RIFF....WAVEfmt ....data....".to_vec();
            bytes.extend(samples.iter().flat_map(|sample| sample.to_le_bytes()));
            bytes
        };
        assert!(super::wav_is_silent(&wav(&[0, 3, -40, 99])));
        assert!(!super::wav_is_silent(&wav(&[0, 3, -4000, 9])));
        assert!(!super::wav_is_silent(b"not a wav"));
    }
    use super::*;
    use agentlibre_common::testutil::TempDir;

    #[test]
    fn base64_matches_the_standard() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected);
        }
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn sha256_of_the_empty_input() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_stored_copy_is_idempotent_and_checked_at_read() {
        let dir = TempDir::new();
        let media = dir.0.join("media");
        let sha = put(&media, b"bytes", "png").unwrap();
        assert_eq!(sha, sha256_hex(b"bytes"));
        assert_eq!(put(&media, b"bytes", "png").unwrap(), sha);
        assert_eq!(read(&media, &sha, "png").unwrap(), b"bytes");
        // A changed file fails the check; a missing one is a read error.
        fs::write(file_in(&media, &sha, "png"), b"other").unwrap();
        assert!(matches!(read(&media, &sha, "png"), Err(error::Error::Media(Error::Changed { .. }))));
        assert!(matches!(read(&media, &"0".repeat(64), "png"), Err(error::Error::Common(CommonError::Read { .. }))));
        // No temporary file is left behind.
        let left: Vec<_> = fs::read_dir(&media).unwrap().flatten().collect();
        assert_eq!(left.len(), 1);
    }

    #[test]
    fn frames_are_one_per_second_and_at_most_32() {
        assert_eq!(frame_times(0.0), [0.0]);
        assert_eq!(frame_times(0.4), [0.0]);
        assert_eq!(frame_times(1.0), [0.0]);
        assert_eq!(frame_times(5.0), [0.0, 1.0, 2.0, 3.0, 4.0]);
        assert_eq!(frame_times(5.2).len(), 6);
        assert_eq!(frame_times(32.0).len(), 32);
        // A longer video: 32 frames, evenly over its length.
        let long = frame_times(600.0);
        assert_eq!(long.len(), 32);
        assert_eq!(long[0], 0.0);
        assert!((long[31] - 31.0 * 600.0 / 32.0).abs() < 1e-9);
        assert!(long.windows(2).all(|w| (w[1] - w[0] - 18.75).abs() < 1e-9));
        assert_eq!(frame_times(f64::NAN), [0.0]);
    }

    #[test]
    fn the_clock_is_minutes_and_seconds() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(5.0), "0:05");
        assert_eq!(clock(61.4), "1:01");
        assert_eq!(clock(3600.0), "60:00");
    }

    /// `ffmpeg` makes a video and a sound; the frames and the WAV come out.
    #[test]
    fn ffmpeg_cuts_frames_and_converts_audio() {
        // The tools' ffmpeg, as the code under test runs it (AGL-040).
        let Ok(ffmpeg) = agentlibre_common::programs::program("ffmpeg") else {
            return;
        };
        let dir = TempDir::new();
        let video = dir.0.join("t.mp4");
        let made = Command::new(&ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=1280x720:rate=10", "-t", "3", "-pix_fmt", "yuv420p"])
            .arg(&video)
            .status()
            .unwrap();
        assert!(made.success());
        let frames = video_frames(&video).unwrap();
        assert_eq!(frames.iter().map(|(at, _)| *at).collect::<Vec<_>>(), [0.0, 1.0, 2.0]);
        // JPEG, scaled to 640 on the long edge.
        assert!(frames.iter().all(|(_, jpeg)| jpeg.starts_with(&[0xff, 0xd8])));
        let sound = dir.0.join("t.mp3");
        let made = Command::new(&ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
            .arg(&sound)
            .status()
            .unwrap();
        assert!(made.success());
        let wav = audio_to_wav(&fs::read(&sound).unwrap()).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
    }
}
