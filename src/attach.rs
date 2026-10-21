//! Attaching media to a message (AGL-014, D18, D21): the files the human
//! gives are read at once and get their placeholder; they are stored in
//! the session, and an audio the model does not hear is turned into text,
//! only when the message that names them is sent.

use std::path::Path;

use crate::backend::Backend;
use crate::error::Error;
use crate::media::{self, Kind, Source};
use crate::session::Session;

/// A medium the human attached that no message has sent yet.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub number: u32,
    pub source: Source,
}

impl Pending {
    pub fn placeholder(&self) -> String {
        media::placeholder(self.source.kind, self.number)
    }
}

/// Turns an audio into text (D22).
pub trait Transcriber {
    fn transcribe(&self, source: &Source, say: &mut dyn FnMut(&str)) -> Result<String, Error>;
}

/// The number the next medium of `kind` gets: after the media the session
/// holds and those waiting to be sent (D21).
pub fn next_number(session: &Session, pending: &[Pending], kind: Kind) -> u32 {
    session.next_media_number(kind)
        + pending.iter().filter(|p| p.source.kind == kind).count() as u32
}

/// Adds `source` to `pending` and returns its placeholder.
pub fn add(session: &Session, pending: &mut Vec<Pending>, source: Source) -> String {
    let number = next_number(session, pending, source.kind);
    let item = Pending { number, source };
    let placeholder = item.placeholder();
    pending.push(item);
    placeholder
}

/// Reads the files at `paths` and adds them; returns their placeholders.
pub fn add_files(
    session: &Session,
    pending: &mut Vec<Pending>,
    paths: &[impl AsRef<Path>],
) -> Result<Vec<String>, Error> {
    // All are read before one is added: a bad file adds none.
    let sources = paths
        .iter()
        .map(|path| Source::from_file(path.as_ref()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(sources
        .into_iter()
        .map(|source| add(session, pending, source))
        .collect())
}

/// Stores the media of `pending` that `text` names; the others are
/// dropped (a placeholder the human deleted sends nothing, D21). An audio
/// goes to the model as it is when it hears (D19), else through
/// `whisper` (D22). `say` takes the progress lines.
pub fn store(
    session: &mut Session,
    pending: Vec<Pending>,
    text: &str,
    backend: &dyn Backend,
    whisper: &dyn Transcriber,
    say: &mut dyn FnMut(&str),
) -> Result<(), Error> {
    let named: Vec<media::Found> = media::placeholders(text);
    let mut hears: Option<bool> = None;
    for item in pending {
        if !named
            .iter()
            .any(|f| f.kind == item.source.kind && f.number == item.number)
        {
            continue;
        }
        let transcript = if item.source.kind == Kind::Audio {
            if hears.is_none() {
                hears = Some(backend.hears_audio()?);
            }
            if hears == Some(true) {
                None
            } else {
                Some(whisper.transcribe(&item.source, say)?)
            }
        } else {
            None
        };
        session.store_media(&item.source, item.number, transcript)?;
    }
    Ok(())
}

/// The placeholders and the message: `[Image#1] [Audio#1] message` (D18).
pub fn with_placeholders(placeholders: &[String], message: &str) -> String {
    if placeholders.is_empty() {
        message.to_owned()
    } else {
        format!("{} {message}", placeholders.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::backend::{Generated, Prompt, Request};

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let dir = std::env::temp_dir().join(format!("agentLIBRE-attach-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Dir(dir)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A backend that only says whether it hears audio.
    struct Ears(bool);

    impl Backend for Ears {
        fn render(&self, _: &Prompt) -> Result<String, Error> {
            unreachable!()
        }
        fn generate(&self, _: &Request, _: &mut dyn FnMut(&str)) -> Result<Generated, Error> {
            unreachable!()
        }
        fn count_tokens(&self, _: &str) -> Result<usize, Error> {
            unreachable!()
        }
        fn context_size(&self) -> Result<usize, Error> {
            unreachable!()
        }
        fn hears_audio(&self) -> Result<bool, Error> {
            Ok(self.0)
        }
    }

    #[derive(Default)]
    struct Words(RefCell<Vec<String>>);

    impl Transcriber for Words {
        fn transcribe(&self, source: &Source, say: &mut dyn FnMut(&str)) -> Result<String, Error> {
            say(&format!("heard {}", source.name));
            self.0.borrow_mut().push(source.name.clone());
            Ok(format!("words of {}", source.name))
        }
    }

    fn source(kind: Kind, name: &str) -> Source {
        Source {
            kind,
            bytes: name.as_bytes().to_vec(),
            ext: match kind {
                Kind::Image => "png",
                Kind::Video => "mp4",
                Kind::Audio => "wav",
            }
            .into(),
            name: name.into(),
        }
    }

    fn session(dir: &Dir) -> Session {
        Session::create(&dir.0.join("s"), "f", &dir.0).unwrap()
    }

    #[test]
    fn the_numbers_follow_the_session_and_the_pending_media_by_kind() {
        let dir = Dir::new("numbers");
        let mut session = session(&dir);
        let mut pending = Vec::new();
        assert_eq!(add(&session, &mut pending, source(Kind::Image, "a.png")), "[Image#1]");
        assert_eq!(add(&session, &mut pending, source(Kind::Image, "b.png")), "[Image#2]");
        assert_eq!(add(&session, &mut pending, source(Kind::Audio, "v.wav")), "[Audio#1]");
        // The session already holds one image: the next is the second.
        session.store_media(&source(Kind::Image, "old.png"), 1, None).unwrap();
        let mut more = Vec::new();
        assert_eq!(add(&session, &mut more, source(Kind::Image, "c.png")), "[Image#2]");
        assert_eq!(with_placeholders(&["[Image#1]".into(), "[Audio#1]".into()], "hi"), "[Image#1] [Audio#1] hi");
        assert_eq!(with_placeholders(&[], "hi"), "hi");
    }

    #[test]
    fn files_are_read_together_and_a_bad_one_adds_none() {
        let dir = Dir::new("files");
        let session = session(&dir);
        fs::write(dir.0.join("a.png"), b"a").unwrap();
        fs::write(dir.0.join("notes.txt"), b"t").unwrap();
        let mut pending = Vec::new();
        let result = add_files(&session, &mut pending, &[dir.0.join("a.png"), dir.0.join("notes.txt")]);
        assert!(matches!(result, Err(Error::MediaKind { .. })));
        assert!(pending.is_empty());
        let placeholders = add_files(&session, &mut pending, &[dir.0.join("a.png")]).unwrap();
        assert_eq!(placeholders, ["[Image#1]"]);
        assert_eq!(pending[0].source.name, "a.png");
    }

    #[test]
    fn only_the_media_the_message_names_are_stored() {
        let dir = Dir::new("named");
        let mut session = session(&dir);
        let mut pending = Vec::new();
        add(&session, &mut pending, source(Kind::Image, "kept.png"));
        add(&session, &mut pending, source(Kind::Image, "removed.png"));
        let words = Words::default();
        // The human deleted [Image#2] from the line.
        store(&mut session, pending, "look at [Image#1]", &Ears(false), &words, &mut |_| {}).unwrap();
        let kept = session.media();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "kept.png");
        assert!(words.0.borrow().is_empty());
    }

    #[test]
    fn an_audio_is_transcribed_unless_the_model_hears() {
        let dir = Dir::new("audio");
        let mut session = session(&dir);
        let mut pending = Vec::new();
        add(&session, &mut pending, source(Kind::Audio, "v.wav"));
        let words = Words::default();
        let mut said = Vec::new();
        store(&mut session, pending.clone(), "[Audio#1]", &Ears(false), &words, &mut |l| said.push(l.to_owned())).unwrap();
        assert_eq!(session.media()[0].transcript.as_deref(), Some("words of v.wav"));
        assert_eq!(said, ["heard v.wav"]);
        // The model hears: no transcript, whisper is not asked (D19).
        let mut other = Session::create(&dir.0.join("s2"), "f", &dir.0).unwrap();
        let words = Words::default();
        store(&mut other, pending, "[Audio#1]", &Ears(true), &words, &mut |_| {}).unwrap();
        assert_eq!(other.media()[0].transcript, None);
        assert!(words.0.borrow().is_empty());
    }

    #[test]
    fn a_failed_transcription_stores_nothing_for_that_audio() {
        struct Fails;
        impl Transcriber for Fails {
            fn transcribe(&self, _: &Source, _: &mut dyn FnMut(&str)) -> Result<String, Error> {
                Err(Error::WhisperServerNotFound)
            }
        }
        let dir = Dir::new("fails");
        let mut session = session(&dir);
        let mut pending = Vec::new();
        add(&session, &mut pending, source(Kind::Audio, "v.wav"));
        let result = store(&mut session, pending, "[Audio#1]", &Ears(false), &Fails, &mut |_| {});
        assert!(matches!(result, Err(Error::WhisperServerNotFound)));
        assert!(session.media().is_empty());
    }
}
