//! What a client asks of the daemon beside its own session (AGL-126, D2;
//! AGL-127, D2, D6): a connection, the stores of the REPL's `/function` and
//! `/plane`, and a session followed while its turn runs. Each request is on
//! a connection of its own: the input thread and the main thread ask both.

use std::path::PathBuf;

use agentlibre_client::{Client, Start};
use agentlibre_common::i18n::Lang;
use agentlibre_common::paths::Paths;
use agentlibre_common::settings::ServerKind;
use agentlibre_proto::{self as proto, Reply, Request};

use crate::Error;

/// Connects to the daemon of `paths`, which is started when none answers
/// and `start` says so (AGL-076, D6); `human` says whether a human answers
/// the planes' questions (D11). The clients' crate knows no `Paths`
/// (AGL-121, D29; AGL-130, D6).
pub fn connect(paths: &Paths, human: bool, lang: Lang, start: bool) -> Result<Client, Error> {
    let runtime = paths.runtime.as_deref().ok_or(agentlibre_client::Error::NoRuntime)?;
    let start = if start { Start::ThisExe } else { Start::No };
    Ok(Client::connect(
        &agentlibre_client::socket_path(runtime),
        human,
        lang.code(),
        start,
    )?)
}

/// The Functions as the REPL asks for them (AGL-126, D2): by the daemon's
/// requests ([`Stores`]), the tests by a catalog of their own.
pub trait Functions: Send + Sync {
    /// The Function the session runs.
    fn current(&self) -> &str;
    /// Every Function by name, the session's marked.
    fn entries(&self) -> Result<Vec<proto::FunctionEntry>, Error>;
    /// The source of `name` and the file a run loads it from.
    fn show(&self, name: &str) -> Result<(String, PathBuf), Error>;
    /// What `name` runs, when it loads and its model fits the settings.
    fn check(&self, name: &str) -> Result<proto::FunctionLoad, Error>;
    /// A copy of `from` in the workspace's `functions/`: its file.
    fn create(&self, name: &str, from: &str) -> Result<PathBuf, Error>;
    /// The file of `name` deleted.
    fn delete(&self, name: &str) -> Result<(), Error>;
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
        let client = connect(&self.paths, false, self.lang, true)?;
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
    agentlibre_client::Error::Daemon {
        detail: format!("an unexpected reply: {reply:?}"),
    }
    .into()
}

impl Functions for Stores {
    fn current(&self) -> &str {
        &self.function
    }

    fn entries(&self) -> Result<Vec<proto::FunctionEntry>, Error> {
        match self.ask(&Request::Functions {
            dir: self.dir.clone(),
            plane: self.plane.clone(),
        })? {
            // The current one is the session's Function, not the entry's own
            // mark; a kind this agl does not know is not listed.
            Reply::Functions { functions } => Ok(functions
                .into_iter()
                .filter(|entry| ServerKind::parse(&entry.kind).is_some())
                .map(|entry| proto::FunctionEntry {
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
    client: Client,
    session: String,
}

/// A session's events and its phases' times.
pub type Past = (Vec<proto::Event>, Vec<proto::PhaseTime>);

impl Follower {
    pub fn of(paths: &Paths, lang: Lang, session: &str) -> Result<Follower, Error> {
        let client = connect(paths, false, lang, false)?;
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
            Reply::History { events, transitions, .. } => Ok((events, transitions)),
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
