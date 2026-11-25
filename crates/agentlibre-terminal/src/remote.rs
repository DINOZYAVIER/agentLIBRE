//! The terminal as a client of the daemon (AGL-076, D5): its session
//! opened there, the questions of its plane asked of the human, its
//! Ctrl-C sent as `stop`, and its voice transcribed there (D15).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use agentlibre_core::client::{self, Remote};
use agentlibre_core::plane::wire;
use agentlibre_proto::{self as proto, Ask, Reply, Request};

use agentlibre_common::reasoning::ReasoningLevel;
use crate::error::CoreError as Error;
use crate::function::catalog::Entry;
use crate::repl::Functions;
use crate::session::{Event, TransitionLine};
use agentlibre_common::i18n::Lang;
use crate::interrupt;
use agentlibre_common::paths::Paths;
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
    let human = agentlibre_common::tty::is_interactive();
    let client = client::connect(paths, human, lang, true)?;
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

/// What the REPL asks of the stores (AGL-126, D2), each request on a
/// connection of its own: the input thread and the main thread ask both.
pub struct Stores {
    pub paths: Paths,
    pub lang: Lang,
    pub dir: String,
    pub plane: Option<String>,
    /// The Function the session runs.
    pub function: String,
}

impl Stores {
    fn ask(&self, request: &Request) -> Result<Reply, Error> {
        let client = client::connect(&self.paths, false, self.lang, true)?;
        Ok(client.ask(request, &mut |_| {})?)
    }

    fn function(&self, verb: proto::FunctionVerb) -> Result<proto::FunctionView, Error> {
        match self.ask(&Request::Function {
            dir: self.dir.clone(),
            plane: self.plane.clone(),
            verb,
        })? {
            Reply::Function(view) => Ok(view),
            reply => Err(unexpected(&reply)),
        }
    }

    /// The planes of the session's directory, or the plane `name` alone,
    /// refused when there is none.
    pub fn planes(&self, name: Option<&str>) -> Result<Vec<String>, Error> {
        match self.ask(&Request::Planes {
            dir: self.dir.clone(),
            name: name.map(str::to_owned),
        })? {
            Reply::Planes { planes } => Ok(planes),
            reply => Err(unexpected(&reply)),
        }
    }
}

fn unexpected(reply: &Reply) -> Error {
    Error::Daemon {
        detail: format!("an unexpected reply: {reply:?}"),
    }
}

impl Functions for Stores {
    fn current(&self) -> &str {
        &self.function
    }

    fn entries(&self) -> Result<Vec<Entry>, Error> {
        match self.ask(&Request::Functions {
            dir: self.dir.clone(),
            plane: self.plane.clone(),
        })? {
            Reply::Functions { functions } => Ok(functions
                .into_iter()
                .filter_map(wire::function_entry_of)
                .map(|entry| Entry {
                    current: entry.name == self.function,
                    ..entry
                })
                .collect()),
            reply => Err(unexpected(&reply)),
        }
    }

    fn show(&self, name: &str) -> Result<(String, PathBuf), Error> {
        let view = self.function(proto::FunctionVerb::Show { name: name.into() })?;
        Ok((view.text.unwrap_or_default(), view.path.into()))
    }

    fn check(&self, name: &str) -> Result<proto::FunctionLoad, Error> {
        let view = self.function(proto::FunctionVerb::Check { name: name.into() })?;
        Ok(view.load.unwrap_or_default())
    }

    fn create(&self, name: &str, from: &str) -> Result<PathBuf, Error> {
        let verb = proto::FunctionVerb::New {
            name: name.into(),
            from: from.into(),
        };
        Ok(self.function(verb)?.path.into())
    }

    fn delete(&self, name: &str) -> Result<(), Error> {
        self.function(proto::FunctionVerb::Delete { name: name.into() }).map(drop)
    }
}

/// A session read while its turn runs (AGL-126, D2): what the inspect mode
/// shows, and whether it grew, on a connection of its own.
pub struct Follower {
    client: client::Client,
    session: String,
}

/// A session's events and its phases' times.
pub type Past = (Vec<Event>, Vec<TransitionLine>);

impl Follower {
    pub fn of(paths: &Paths, lang: Lang, session: &str) -> Result<Follower, Error> {
        let client = client::connect(paths, false, lang, false)?;
        let subscribe = Request::Subscribe {
            session: Some(session.to_owned()),
        };
        client.ask(&subscribe, &mut |_| {})?;
        Ok(Follower {
            client,
            session: session.to_owned(),
        })
    }

    pub fn past(&self) -> Result<Past, Error> {
        let request = Request::History {
            session: self.session.clone(),
        };
        match self.client.ask(&request, &mut |_| {})? {
            Reply::History { events, transitions, .. } => Ok((
                events.into_iter().map(wire::event_of).collect(),
                transitions.into_iter().map(wire::phase_time_of).collect(),
            )),
            reply => Err(unexpected(&reply)),
        }
    }

    /// The past, when the session grew since it was last read.
    pub fn grown(&self) -> Option<Past> {
        let mut grew = false;
        while let Some(reply) = self.client.try_next() {
            grew |= matches!(
                reply,
                Reply::Done { .. }
                    | Reply::Sent {
                        sent: proto::Sent::Report(proto::Report::Event(_) | proto::Report::Transition(_)),
                        ..
                    }
            );
        }
        if grew { self.past().ok() } else { None }
    }
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
        let client = client::connect(&self.paths, false, self.lang, true)?;
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
