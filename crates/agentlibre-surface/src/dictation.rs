//! The dictation of a surface (AGL-016, AGL-122; AGL-127, D2, D4): what
//! turns a recording into text and a chunk of it into a live draft, both by
//! the daemon, and the folding of the draft's windows into one text.

use std::sync::mpsc::{self, Receiver, Sender as MpscSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use agentlibre_common::i18n::Lang;
use agentlibre_common::paths::Paths;
use agentlibre_proto::{self as proto, Reply, Request};

use crate::Error;
use crate::daemon::connect;

/// What the REPL's voice input needs of whisper (AGL-016; AGL-127, D2): the
/// daemon's, over the socket.
pub trait Dictation: Send + Sync {
    /// Whether whisper is set up for this project; the error says what is
    /// missing, before the human speaks.
    fn ready(&self) -> Result<(), Error>;
    /// Brings the server up; returns when it answers (D6).
    fn warm_up(&self, say: &mut dyn FnMut(&str)) -> Result<(), Error>;
    /// The text of a 16 kHz mono WAV, `prompt` the tail of the text
    /// that came before it when the WAV is a piece of a longer speech
    /// (AGL-122, D12).
    fn transcribe(
        &self,
        wav: &[u8],
        prompt: Option<&str>,
        say: &mut dyn FnMut(&str),
    ) -> Result<String, Error>;
}

/// What turns a chunk into draft text; `RemoteDraft` asks the daemon's draft
/// server, the tests fake it (AGL-122; AGL-127, D4).
pub trait DraftEngine: Send + Sync {
    fn transcribe(&self, wav: &[u8]) -> Result<String, Error>;
}

/// The draft engine at work (AGL-122): one thread turns chunks into text
/// one by one, so the lines keep their order; when the last sender of
/// the recording drops, it stops.
pub struct DraftWorker {
    pub chunks: MpscSender<(Vec<u8>, u64)>,
    pub texts: Receiver<(String, u64)>,
    _thread: JoinHandle<()>,
}

impl DraftWorker {
    pub fn start(draft: Arc<dyn DraftEngine>) -> DraftWorker {
        let (ctx, crx) = mpsc::channel::<(Vec<u8>, u64)>();
        let (tx, rx) = mpsc::channel::<(String, u64)>();
        let thread = thread::spawn(move || {
            while let Ok(mut chunk) = crx.recv() {
                // The latest window when the engine falls behind the
                // clock (D10): a stale window would lag the human's
                // mouth, and the next one re-hears its tail anyway.
                while let Ok(fresher) = crx.try_recv() {
                    chunk = fresher;
                }
                // A chunk that failed or came out empty shows nothing:
                // the final pass has the last word (D7).
                if let Ok(text) = draft.transcribe(&chunk.0)
                    && !text.is_empty()
                    && tx.send((text, chunk.1)).is_err()
                {
                    break;
                }
            }
        });
        DraftWorker { chunks: ctx, texts: rx, _thread: thread }
    }
}

/// The draft after one more window's text (AGL-122, D10): the windows
/// overlap in audio, so their texts repeat the words on the seam — the
/// longest run of words that ends the draft and begins the new text is
/// spoken once. Words compare lowercase and bare of punctuation, which
/// two hearings of the same syllable rarely agree on otherwise.
pub fn merge_draft(text: &str, fresh: &str) -> String {
    fn norm(word: &str) -> String {
        word.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect()
    }
    let old: Vec<&str> = text.split_whitespace().collect();
    let new: Vec<&str> = fresh.split_whitespace().collect();
    let cap = old.len().min(new.len()).min(8);
    let mut shared = 0;
    for n in (1..=cap).rev() {
        if old[old.len() - n..].iter().map(|w| norm(w)).eq(new[..n].iter().map(|w| norm(w))) {
            shared = n;
            break;
        }
    }
    let mut out = text.to_owned();
    for word in &new[shared..] {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// What the live block says (AGL-122, D12): the confirmed pieces first,
/// then the draft beyond the last of them; where confirmed and draft
/// meet, the seam folds as between two windows.
pub fn compose(confirmed: &str, drafts: &[(u64, String)], upto: u64) -> String {
    let first = drafts.iter().position(|(end, _)| *end > upto).unwrap_or(drafts.len());
    let mut tail = String::new();
    for (_, text) in &drafts[first..] {
        tail = merge_draft(&tail, text);
    }
    if confirmed.is_empty() {
        tail
    } else {
        merge_draft(confirmed, &tail)
    }
}


/// The REPL's voice by the daemon's transcription (D15), on a connection
/// of its own.
pub struct RemoteVoice {
    pub paths: Paths,
    pub session: String,
    pub lang: Lang,
}

impl Dictation for RemoteVoice {
    fn ready(&self) -> Result<(), Error> {
        Ok(())
    }

    fn warm_up(&self, _say: &mut dyn FnMut(&str)) -> Result<(), Error> {
        Ok(())
    }

    fn transcribe(
        &self,
        wav: &[u8],
        _prompt: Option<&str>,
        _say: &mut dyn FnMut(&str),
    ) -> Result<String, Error> {
        let client = connect(&self.paths, false, self.lang, true)?;
        let request = Request::Transcribe {
            session: self.session.clone(),
            audio: proto::Attached {
                number: 1,
                kind: proto::MediaKind::Audio,
                name: "voice.wav".into(),
                ext: "wav".into(),
                base64: proto::base64::encode(wav),
            },
            draft: false,
        };
        match client.ask(&request, &mut |_| {})? {
            Reply::Transcribed { text } => Ok(text),
            reply => Err(agentlibre_client::Error::Daemon {
                detail: format!("an unexpected reply: {reply:?}"),
            }
            .into()),
        }
    }
}

/// The draft of the live dictation by the daemon's draft server (AGL-127,
/// D4): each chunk goes on a connection of its own, as `RemoteVoice`'s
/// does, and the worker's one thread keeps their order.
pub struct RemoteDraft {
    pub paths: Paths,
    pub session: String,
    pub lang: Lang,
}

impl DraftEngine for RemoteDraft {
    fn transcribe(&self, wav: &[u8]) -> Result<String, Error> {
        let client = connect(&self.paths, false, self.lang, true)?;
        let request = Request::Transcribe {
            session: self.session.clone(),
            audio: proto::Attached {
                number: 1,
                kind: proto::MediaKind::Audio,
                name: "chunk.wav".into(),
                ext: "wav".into(),
                base64: proto::base64::encode(wav),
            },
            draft: true,
        };
        match client.ask(&request, &mut |_| {})? {
            Reply::Transcribed { text } => Ok(text),
            reply => Err(agentlibre_client::Error::Daemon {
                detail: format!("an unexpected reply: {reply:?}"),
            }
            .into()),
        }
    }
}

#[cfg(test)]
mod seam_tests {
    use super::{compose, merge_draft};

    #[test]
    fn the_block_is_confirmed_text_then_the_draft_beyond() {
        let drafts = vec![(1200, "пере проверка".to_string()), (2400, "проверка связи".to_string())];
        // Nothing confirmed yet: the draft whole, seam folded.
        assert_eq!(compose("", &drafts, 0), "пере проверка связи");
        // A piece confirmed to 1200 ms: the draft words it covers leave,
        // the later draft stays, the meeting seam folds once (D12).
        assert_eq!(compose("Проверка", &drafts, 1200), "Проверка связи");
        // All confirmed, nothing owed: no draft beyond.
        assert_eq!(compose("проверка связи", &[], 2400), "проверка связи");
    }

    #[test]
    fn the_words_on_the_seam_are_spoken_once() {
        // The windows overlap in audio, so the engine hears "still wa"
        // twice: the draft must not write it twice (D10).
        assert_eq!(merge_draft("he said still waiting", "still waiting for you"), "he said still waiting for you");
    }

    #[test]
    fn the_seam_survives_case_and_punctuation_disputes() {
        // Two hearings of the same syllable disagree on commas and
        // caps; the fold does not care.
        assert_eq!(merge_draft("пришел, и", "И ушел"), "пришел, и ушел");
    }

    #[test]
    fn no_seam_means_plain_append() {
        assert_eq!(merge_draft("one two", "three four"), "one two three four");
        assert_eq!(merge_draft("", "first"), "first");
        assert_eq!(merge_draft("kept", ""), "kept");
    }

    #[test]
    fn a_repeated_word_is_not_eaten_when_it_is_not_the_seam() {
        // The fold looks only at the seam: a word that repeats in the
        // middle of the new text is nobody's echo.
        assert_eq!(merge_draft("the word", "a word here more"), "the word a word here more");
    }
}

#[cfg(test)]
mod worker_tests {
    use std::time::Duration;

    use super::*;

    /// An engine that hears each chunk as its first byte, and fails on 0.
    struct Fake;

    impl DraftEngine for Fake {
        fn transcribe(&self, wav: &[u8]) -> Result<String, Error> {
            match wav.first() {
                Some(0) | None => Err(agentlibre_client::Error::Gone.into()),
                Some(2) => Ok(String::new()),
                Some(byte) => Ok(format!("w{byte}")),
            }
        }
    }

    #[test]
    fn a_failed_or_empty_chunk_shows_nothing_and_the_next_still_comes() {
        // AGL-127, traps: a draft's failure is silent; the final pass has
        // the last word (AGL-122, D7).
        let worker = DraftWorker::start(Arc::new(Fake));
        for (n, chunk) in [vec![0u8], vec![2], vec![7]].into_iter().enumerate() {
            worker.chunks.send((chunk, n as u64)).unwrap();
        }
        let (text, end) = worker.texts.recv_timeout(Duration::from_secs(5)).unwrap();
        // The worker keeps the latest window when it falls behind, so the
        // one that comes out is the last that was told, or an earlier one
        // that was heard before it came.
        assert_eq!(text, "w7");
        assert_eq!(end, 2);
        assert!(worker.texts.try_recv().is_err());
    }
}
