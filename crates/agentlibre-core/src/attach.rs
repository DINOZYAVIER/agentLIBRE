//! Attaching media to a message (AGL-014, D18, D21): the files the human
//! gives are read at once and get their placeholder; they are stored in
//! the session, and an audio the model does not hear is turned into text,
//! only when the message that names them is sent.

use crate::backend::Backend;
use crate::error::Error;
use agentlibre_common::media::{Found, Kind, Pending, Source, placeholders};
use crate::session::Session;

/// Turns an audio into text (D22).
pub trait Transcriber {
    fn transcribe(&self, source: &Source, say: &mut dyn FnMut(&str)) -> Result<String, Error>;
}

/// The media of each kind the session holds, in the order of [`Kind::ALL`].
pub fn media_counts(session: &Session) -> [u32; 3] {
    Kind::ALL.map(|kind| session.next_media_number(kind) - 1)
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
    let named: Vec<Found> = placeholders(text);
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use super::*;
    use crate::backend::{Generated, Prompt, Request};
    use agentlibre_common::testutil::TempDir;

    /// A backend that only says whether it hears audio.
    struct Ears(bool);

    impl Backend for Ears {
        fn render(&self, _: &Prompt) -> Result<String, Error> {
            unreachable!()
        }
        fn generate(
            &self,
            _: &Request,
            _: &mut dyn FnMut(&str),
            _: &mut dyn FnMut(&str),
        ) -> Result<Generated, Error> {
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
        fn health(&self) -> Option<u16> {
            None
        }
        fn url(&self) -> &str {
            "http://127.0.0.1:0"
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

    /// Adds `source` to `pending`, numbered after the session's media and
    /// those waiting (a surface's `add_counted`, by hand).
    fn add(session: &Session, pending: &mut Vec<Pending>, source: Source) {
        let number = session.next_media_number(source.kind)
            + pending.iter().filter(|p| p.source.kind == source.kind).count() as u32;
        pending.push(Pending { number, source });
    }

    fn session(dir: &TempDir) -> Session {
        Session::create(&dir.0.join("s"), "f", &dir.0).unwrap()
    }

    #[test]
    fn only_the_media_the_message_names_are_stored() {
        let dir = TempDir::new();
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
        let dir = TempDir::new();
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
                Err(crate::engine::error::Error::EngineNotFound {
                    engine: "whisper-server".into(),
                }
                .into())
            }
        }
        let dir = TempDir::new();
        let mut session = session(&dir);
        let mut pending = Vec::new();
        add(&session, &mut pending, source(Kind::Audio, "v.wav"));
        let result = store(&mut session, pending, "[Audio#1]", &Ears(false), &Fails, &mut |_| {});
        assert!(matches!(result, Err(crate::error::Error::Engine(crate::engine::error::Error::EngineNotFound { .. }))));
        assert!(session.media().is_empty());
    }
}
