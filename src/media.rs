//! Media of a message (AGL-014): an image, a video or an audio the human
//! attaches, kept by a session as a copy named by its sha256 (D7).
//!
//! The text of a message holds a placeholder (`[Image#1]`, `[Video#1]`,
//! `[Audio#1]`, numbered by kind, D21); the session keeps the table that
//! says what each one is. When a step is rendered the placeholders become
//! the server's media marker and the bytes go with the request (D6).

use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::backend::{Backend, Message};
use crate::error::Error;
use crate::forge::sha256;

/// The most frames taken from a video (D16).
pub const MAX_FRAMES: usize = 32;
/// The frames are scaled so that their long edge is at most this (D16).
pub const FRAME_EDGE: u32 = 640;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    Video,
    Audio,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Image, Kind::Video, Kind::Audio];

    /// The word of the placeholder and of the session file.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Image => "Image",
            Kind::Video => "Video",
            Kind::Audio => "Audio",
        }
    }

    pub fn from_label(label: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.label() == label)
    }

    /// The kind of a file by its extension.
    pub fn of_path(path: &Path) -> Option<Kind> {
        let ext = path.extension()?.to_str()?.to_lowercase();
        match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" => Some(Kind::Image),
            "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" => Some(Kind::Video),
            "wav" | "mp3" | "flac" | "ogg" | "opus" | "m4a" | "aac" => Some(Kind::Audio),
            _ => None,
        }
    }
}

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

/// `[Image#3]`.
pub fn placeholder(kind: Kind, number: u32) -> String {
    format!("[{}#{number}]", kind.label())
}

/// A placeholder found in a text: its byte range, kind and number.
#[derive(Debug, PartialEq, Eq)]
pub struct Found {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
    pub number: u32,
}

/// The placeholders of `text`, in order.
pub fn placeholders(text: &str) -> Vec<Found> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(open) = text[from..].find('[') {
        let start = from + open;
        from = start + 1;
        let Some(close) = text[start..].find(']') else {
            break;
        };
        let end = start + close + 1;
        let inner = &text[start + 1..end - 1];
        let Some((label, digits)) = inner.split_once('#') else {
            continue;
        };
        let Some(kind) = Kind::from_label(label) else {
            continue;
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(number) = digits.parse() else { continue };
        found.push(Found {
            start,
            end,
            kind,
            number,
        });
        from = end;
    }
    found
}

/// `m:ss`, for the time of a frame.
pub fn clock(seconds: f64) -> String {
    let whole = seconds.max(0.0).round() as u64;
    format!("{}:{:02}", whole / 60, whole % 60)
}

/// The lowercase hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    sha256(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// `bytes` in standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// What the placeholders of the messages of a step became.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Expanded {
    /// The bytes of the media, base64, in the order of the markers.
    pub uploads: Vec<String>,
    /// How many of the markers stand for a picture (an image or a frame
    /// of a video); their tokens are bounded (D10).
    pub pictures: usize,
}

/// Replaces the placeholders in the user messages and the tool results
/// of `messages` with what the model reads (AGL-014): an image the
/// server's marker; a video its frames, each after its time; an audio
/// its transcript, or the marker when the model hears it (D19, D22). A
/// placeholder the table does not know stays text.
pub fn expand(
    messages: &mut [Message],
    table: &[&Media],
    dir: &Path,
    backend: &dyn Backend,
) -> Result<Expanded, Error> {
    let mut expanded = Expanded::default();
    let mut marker: Option<String> = None;
    let mut hears: Option<bool> = None;
    for message in messages {
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
            let mut stand = |expanded: &mut Expanded, uploads: Vec<String>| -> Result<String, Error> {
                if marker.is_none() {
                    marker = Some(backend.media_marker()?);
                }
                expanded.uploads.extend(uploads);
                Ok(marker.clone().unwrap_or_default())
            };
            match medium.kind {
                Kind::Image => {
                    let bytes = read(dir, &medium.sha256, &medium.ext)?;
                    out.push_str(&stand(&mut expanded, vec![base64(&bytes)])?);
                    expanded.pictures += 1;
                }
                Kind::Video => {
                    out.push_str(&format!("[Video#{}: frames", medium.number));
                    for frame in &medium.frames {
                        let bytes = read(dir, &frame.sha256, "jpg")?;
                        let marker = stand(&mut expanded, vec![base64(&bytes)])?;
                        out.push_str(&format!(" {} {marker}", clock(frame.at)));
                        expanded.pictures += 1;
                    }
                    out.push(']');
                }
                Kind::Audio => match &medium.transcript {
                    Some(transcript) => {
                        out.push_str(&format!("[Audio#{}: \"{transcript}\"]", medium.number));
                    }
                    None => {
                        if hears.is_none() {
                            hears = Some(backend.hears_audio()?);
                        }
                        if hears == Some(true) {
                            let bytes = read(dir, &medium.sha256, &medium.ext)?;
                            out.push_str(&stand(&mut expanded, vec![base64(&bytes)])?);
                        } else {
                            out.push_str(&format!("[Audio#{}: not transcribed]", medium.number));
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
pub fn put(dir: &Path, bytes: &[u8], ext: &str) -> Result<String, Error> {
    let sha = sha256_hex(bytes);
    let path = file_in(dir, &sha, ext);
    if path.is_file() {
        return Ok(sha);
    }
    fs::create_dir_all(dir).map_err(|source| Error::Write {
        path: dir.into(),
        source,
    })?;
    let temp = dir.join(format!(".{sha}.{}.tmp", std::process::id()));
    fs::write(&temp, bytes).map_err(|source| Error::Write {
        path: temp.clone(),
        source,
    })?;
    fs::rename(&temp, &path).map_err(|source| Error::Write { path, source })?;
    Ok(sha)
}

/// Reads `<sha256>.<ext>` from `dir` and checks it against its name.
pub fn read(dir: &Path, sha256: &str, ext: &str) -> Result<Vec<u8>, Error> {
    let path = file_in(dir, sha256, ext);
    let bytes = fs::read(&path).map_err(|source| Error::Read {
        path: path.clone(),
        source,
    })?;
    if sha256_hex(&bytes) != sha256 {
        return Err(Error::MediaChanged { path });
    }
    Ok(bytes)
}

/// A file the human gives, read and not yet stored: what `/attach`, the
/// option of `-p` and Ctrl-V make (D18, D21).
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub kind: Kind,
    pub bytes: Vec<u8>,
    pub ext: String,
    pub name: String,
}

impl Source {
    /// Reads the file at `path`; its kind comes from the extension.
    pub fn from_file(path: &Path) -> Result<Source, Error> {
        let kind = Kind::of_path(path).ok_or_else(|| Error::MediaKind { path: path.into() })?;
        let bytes = fs::read(path).map_err(|source| Error::Read {
            path: path.into(),
            source,
        })?;
        Ok(Source {
            kind,
            bytes,
            ext: path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or_default()
                .to_lowercase(),
            name: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        })
    }
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
fn video_duration(path: &Path) -> Result<f64, Error> {
    let output = Command::new(crate::programs::program("ffprobe")?)
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| Error::Ffmpeg {
            detail: source.to_string(),
        })?;
    if !output.status.success() {
        return Err(Error::Ffmpeg {
            detail: String::from_utf8_lossy(&output.stderr).trim().into(),
        });
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|_| Error::Ffmpeg {
            detail: format!("no duration in {}", path.display()),
        })
}

/// The frames of the video at `path` as JPEG pictures with their times
/// (D8, D16).
pub fn video_frames(path: &Path) -> Result<Vec<(f64, Vec<u8>)>, Error> {
    let times = frame_times(video_duration(path)?);
    let ffmpeg = crate::programs::program("ffmpeg")?;
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
            .map_err(|source| Error::Ffmpeg {
                detail: source.to_string(),
            })?;
        if !output.status.success() {
            return Err(Error::Ffmpeg {
                detail: String::from_utf8_lossy(&output.stderr).trim().into(),
            });
        }
        // A time past the last frame gives nothing: that frame is skipped.
        if !output.stdout.is_empty() {
            frames.push((at, output.stdout));
        }
    }
    if frames.is_empty() {
        return Err(Error::Ffmpeg {
            detail: format!("no frames in {}", path.display()),
        });
    }
    Ok(frames)
}

/// An audio as 16 kHz mono WAV, what whisper reads.
pub fn audio_to_wav(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let mut child = Command::new(crate::programs::program("ffmpeg")?)
        .args(["-v", "error", "-i", "pipe:0", "-ar", "16000", "-ac", "1", "-f", "wav", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::Ffmpeg {
            detail: source.to_string(),
        })?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = bytes.to_vec();
    // ffmpeg may stop reading early; the writer runs apart so that a full
    // pipe does not block the reader.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let output = child.wait_with_output().map_err(|source| Error::Ffmpeg {
        detail: source.to_string(),
    })?;
    let _ = writer.join();
    if !output.status.success() || output.stdout.is_empty() {
        return Err(Error::Ffmpeg {
            detail: String::from_utf8_lossy(&output.stderr).trim().into(),
        });
    }
    Ok(output.stdout)
}

/// The image in the clipboard, as PNG (`wl-paste`); `None` when there is
/// none or no `wl-paste` (D21).
pub fn clipboard_image() -> Option<Vec<u8>> {
    let types = wl_paste(&["--list-types"])?;
    let types = String::from_utf8_lossy(&types);
    let ty = types
        .lines()
        .find(|line| line.trim() == "image/png")
        .or_else(|| types.lines().find(|line| line.trim().starts_with("image/")))?
        .trim()
        .to_owned();
    wl_paste(&["--type", &ty]).filter(|bytes| !bytes.is_empty())
}

/// The files copied in a file manager, from the clipboard's
/// `text/uri-list` (D21).
pub fn clipboard_files() -> Vec<PathBuf> {
    let Some(types) = wl_paste(&["--list-types"]) else {
        return Vec::new();
    };
    if !String::from_utf8_lossy(&types)
        .lines()
        .any(|line| line.trim() == "text/uri-list")
    {
        return Vec::new();
    }
    wl_paste(&["--type", "text/uri-list"])
        .map(|bytes| uri_list_paths(&String::from_utf8_lossy(&bytes)))
        .unwrap_or_default()
}

/// What Ctrl-V finds in the clipboard (AGL-014, D21).
#[derive(Clone, Debug, PartialEq)]
pub enum Clip {
    /// Files copied in a file manager.
    Files(Vec<PathBuf>),
    /// A picture, as PNG.
    Image(Vec<u8>),
    Text(String),
    Empty,
}

/// The clipboard: files first, then an image, then text.
pub fn clipboard() -> Clip {
    let files = clipboard_files();
    if !files.is_empty() {
        return Clip::Files(files);
    }
    if let Some(image) = clipboard_image() {
        return Clip::Image(image);
    }
    let Some(types) = wl_paste(&["--list-types"]) else {
        return Clip::Empty;
    };
    let has_text = String::from_utf8_lossy(&types)
        .lines()
        .any(|line| line.trim().starts_with("text/plain") || line.trim() == "UTF8_STRING");
    if !has_text {
        return Clip::Empty;
    }
    match wl_paste(&["--no-newline", "--type", "text/plain"]) {
        Some(bytes) if !bytes.is_empty() => Clip::Text(String::from_utf8_lossy(&bytes).into_owned()),
        _ => Clip::Empty,
    }
}

fn wl_paste(args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new(crate::programs::program("wl-paste").ok()?)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// The local paths of a `text/uri-list`: the `file://` lines, decoded.
pub fn uri_list_paths(list: &str) -> Vec<PathBuf> {
    list.lines()
        .filter_map(|line| line.trim().strip_prefix("file://"))
        .map(|rest| {
            let rest = rest.strip_prefix("localhost").unwrap_or(rest);
            PathBuf::from(std::ffi::OsStr::from_bytes(&percent_decode(rest)))
        })
        .collect()
}

fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

fn hex(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|d| d as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("agentLIBRE-media-{}-{n}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_kind_comes_from_the_extension() {
        for (name, kind) in [
            ("a.png", Kind::Image),
            ("A.JPG", Kind::Image),
            ("clip.mp4", Kind::Video),
            ("v.webm", Kind::Video),
            ("voice.wav", Kind::Audio),
            ("song.MP3", Kind::Audio),
        ] {
            assert_eq!(Kind::of_path(Path::new(name)), Some(kind), "{name}");
        }
        assert_eq!(Kind::of_path(Path::new("notes.txt")), None);
        assert_eq!(Kind::of_path(Path::new("noext")), None);
    }

    #[test]
    fn placeholders_are_found_in_order_and_only_the_exact_form() {
        let text = "see [Image#1] and [Audio#12]: x [Image#] [image#2] [Video#3 [Video#4]";
        assert_eq!(
            placeholders(text),
            [
                Found { start: 4, end: 13, kind: Kind::Image, number: 1 },
                Found { start: 18, end: 28, kind: Kind::Audio, number: 12 },
                Found { start: 60, end: 69, kind: Kind::Video, number: 4 },
            ]
        );
        assert_eq!(&text[4..13], "[Image#1]");
        assert_eq!(&text[60..69], "[Video#4]");
        assert!(placeholders("no brackets").is_empty());
        assert!(placeholders("[[Image#2]]").len() == 1);
        assert_eq!(placeholder(Kind::Video, 7), "[Video#7]");
    }

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
        assert!(matches!(read(&media, &sha, "png"), Err(Error::MediaChanged { .. })));
        assert!(matches!(read(&media, &"0".repeat(64), "png"), Err(Error::Read { .. })));
        // No temporary file is left behind.
        let left: Vec<_> = fs::read_dir(&media).unwrap().flatten().collect();
        assert_eq!(left.len(), 1);
    }

    #[test]
    fn a_source_reads_a_file_and_refuses_other_kinds() {
        let dir = TempDir::new();
        let path = dir.0.join("Shot.PNG");
        fs::write(&path, b"png").unwrap();
        let source = Source::from_file(&path).unwrap();
        assert_eq!(source.kind, Kind::Image);
        assert_eq!(source.ext, "png");
        assert_eq!(source.name, "Shot.PNG");
        assert_eq!(source.bytes, b"png");
        let text = dir.0.join("a.txt");
        fs::write(&text, b"x").unwrap();
        assert!(matches!(Source::from_file(&text), Err(Error::MediaKind { .. })));
        assert!(matches!(
            Source::from_file(&dir.0.join("missing.png")),
            Err(Error::Read { .. })
        ));
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

    #[test]
    fn a_uri_list_gives_local_paths() {
        let list = "# comment\nfile:///home/u/My%20Pics/a%C3%A9.png\r\nhttp://x/y\nfile://localhost/tmp/b.mp4\n";
        assert_eq!(
            uri_list_paths(list),
            [PathBuf::from("/home/u/My Pics/aé.png"), PathBuf::from("/tmp/b.mp4")]
        );
        assert!(uri_list_paths("").is_empty());
    }

    /// `ffmpeg` makes a video and a sound; the frames and the WAV come out.
    #[test]
    fn ffmpeg_cuts_frames_and_converts_audio() {
        // The tools' ffmpeg, as the code under test runs it (AGL-040).
        let Ok(ffmpeg) = crate::programs::program("ffmpeg") else {
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
