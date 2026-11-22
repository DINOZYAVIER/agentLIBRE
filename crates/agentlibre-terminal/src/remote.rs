//! The terminal as a client of the daemon (AGL-076, D5): its session
//! opened there, the questions of its plane asked of the human, its
//! Ctrl-C sent as `stop`, and its voice transcribed there (D15).

use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use agentlibre_core::client::{Client, Remote};
use agentlibre_proto::{self as proto, Ask, Reply, Request};

use crate::backend::ReasoningLevel;
use crate::error::Error;
use crate::i18n::Lang;
use crate::interrupt;
use crate::paths::Paths;
use crate::whisper::Dictation;

/// What the terminal opens.
pub struct Opening<'a> {
    pub cwd: &'a Path,
    pub plane: Option<&'a str>,
    pub resume: Option<&'a str>,
    pub function: Option<&'a str>,
    pub reasoning: Option<ReasoningLevel>,
    pub yes: bool,
    pub one_request: bool,
    pub start_server: bool,
}

/// Opens the session in the daemon, started when none answers (D6); the
/// lines of the opening go to stderr and its questions to the human on the
/// plain terminal when there is one (D11).
pub fn open(paths: &Paths, opening: Opening, lang: Lang) -> Result<(Arc<Remote>, proto::Opened), Error> {
    let human = crate::tty::is_interactive();
    let client = Client::connect(paths, human, lang, true)?;
    let request = Request::Open(proto::Open {
        dir: opening.cwd.to_string_lossy().into_owned(),
        plane: opening.plane.map(str::to_owned),
        resume: opening.resume.map(str::to_owned),
        function: opening.function.map(str::to_owned),
        reasoning: opening.reasoning.map(|level| level.name().to_owned()),
        yes: opening.yes,
        one_request: opening.one_request,
        start_server: opening.start_server,
    });
    let reply = client.ask(&request, &mut |reply| match reply {
        Reply::Sent {
            sent: proto::Sent::Line(line),
            ..
        } => eprintln!("{line}"),
        Reply::Question { question, ask, text, .. } => {
            let answer = if human { asked(ask, &text, lang) } else { None };
            let _ = client.send(&Request::Answer { question, answer });
        }
        _ => {}
    })?;
    let Reply::Opened(opened) = reply else {
        return Err(Error::Daemon {
            detail: format!("an unexpected reply: {reply:?}"),
        });
    };
    let mut remote = Remote::new(client, opened.session.id.clone());
    // A question of a turn of `-p` is asked on the plain terminal, as the
    // opening's; the REPL's terminal is its own, and the stdin session has
    // none: theirs are not answered (D11).
    if human && opening.one_request {
        remote.answer = Box::new(move |ask, text| asked(ask, text, lang));
    }
    Ok((Arc::new(remote), opened))
}

/// A question asked of the human on the plain terminal.
pub fn asked(ask: Ask, text: &str, lang: Lang) -> Option<String> {
    match ask {
        Ask::Yes => Some(if crate::ask::ask_terminal(text, lang) { "y" } else { "n" }.into()),
        Ask::Line => crate::ask::ask_line(text),
    }
}

/// The terminal's Ctrl-C, its stop's flag, is sent as `stop` (D5).
pub fn forward_stop(remote: &Arc<Remote>) {
    let remote = Arc::clone(remote);
    thread::spawn(move || {
        loop {
            if interrupt::stop().requested() {
                remote.stop();
                // The next Ctrl-C asks again; a second during a turn exits
                // as before (D111 to D113).
                while interrupt::stop().requested() {
                    thread::sleep(Duration::from_millis(50));
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
}

/// The REPL's voice by the daemon's transcription (D15), on a connection
/// of its own.
pub struct RemoteVoice {
    pub paths: Paths,
    pub dir: String,
    pub plane: Option<String>,
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
        let client = Client::connect(&self.paths, false, self.lang, true)?;
        let request = Request::Transcribe {
            dir: self.dir.clone(),
            plane: self.plane.clone(),
            audio: proto::Attached {
                number: 1,
                kind: proto::MediaKind::Audio,
                name: "voice.wav".into(),
                ext: "wav".into(),
                base64: proto::base64::encode(wav),
            },
        };
        match client.ask(&request, &mut |_| {})? {
            Reply::Transcribed { text } => Ok(text),
            reply => Err(Error::Daemon {
                detail: format!("an unexpected reply: {reply:?}"),
            }),
        }
    }
}
