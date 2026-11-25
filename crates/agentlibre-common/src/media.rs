//! The files a message names (AGL-014; AGL-127, D2, D6): the kinds of media,
//! a file read and not yet stored, its placeholder in the text, and what
//! waits to be sent. What the core and a surface both need, below both.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::Error as CommonError;
use crate::i18n::{Lang, Msg};
use crate::tr;

/// What goes wrong with a medium, on a computer of the client: a file of no
/// kind agl takes, or `ffmpeg` and `ffprobe` failing on it (AGL-014;
/// AGL-121, D66; AGL-127, D6).
#[derive(Debug)]
pub enum Error {
    /// A file to attach that is not an image, a video or an audio.
    Kind { path: PathBuf },
    /// `ffmpeg` or `ffprobe` cannot be run or failed.
    Ffmpeg { detail: String },
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::Kind { path } => tr!(lang, KIND, path = path.display()),
            Self::Ffmpeg { detail } => tr!(lang, FFMPEG, detail = detail),
        }
    }
}

/// `Error::Kind`.
const KIND: Msg = Msg {
    en: "{path} is not an image, a video or an audio",
    uk: "{path} не є зображенням, відео чи аудіо",
};

/// `Error::Ffmpeg`.
const FFMPEG: Msg = Msg {
    en: "ffmpeg failed: {detail}",
    uk: "помилка ffmpeg: {detail}",
};

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
    pub fn from_file(path: &Path) -> Result<Source, CommonError> {
        let kind = Kind::of_path(path).ok_or_else(|| Error::Kind { path: path.into() })?;
        let bytes = fs::read(path).map_err(|source| CommonError::Read {
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

/// A medium the human attached that no message has sent yet.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub number: u32,
    pub source: Source,
}

impl Pending {
    pub fn placeholder(&self) -> String {
        placeholder(self.source.kind, self.number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

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
        assert!(matches!(
            Source::from_file(&text),
            Err(CommonError::Media(Error::Kind { .. }))
        ));
        assert!(matches!(
            Source::from_file(&dir.0.join("missing.png")),
            Err(CommonError::Read { .. })
        ));
    }

    #[test]
    fn a_pending_medium_has_its_placeholder() {
        let pending = Pending {
            number: 2,
            source: Source {
                kind: Kind::Audio,
                bytes: Vec::new(),
                ext: "wav".into(),
                name: "a.wav".into(),
            },
        };
        assert_eq!(pending.placeholder(), "[Audio#2]");
    }

    #[test]
    fn the_messages_are_said_in_both_languages() {
        let kind = Error::Kind { path: "/a.txt".into() };
        assert_eq!(kind.message(Lang::En), "/a.txt is not an image, a video or an audio");
        assert_eq!(kind.message(Lang::Uk), "/a.txt не є зображенням, відео чи аудіо");
        let ffmpeg = Error::Ffmpeg { detail: "boom".into() };
        assert_eq!(ffmpeg.message(Lang::En), "ffmpeg failed: boom");
        assert_eq!(ffmpeg.message(Lang::Uk), "помилка ffmpeg: boom");
    }
}
