//! The core's side of a client of the daemon (AGL-076, D5, D7): the
//! connection itself is the clients' crate's (AGL-121, D29), and what is
//! here is only the part that knows agl's paths, its language and its
//! plane: `connect` with a `Paths`, and `Remote`, a session's plane held
//! over the daemon's protocol.

use std::sync::Mutex;

use agentlibre_client::Client;
use agentlibre_proto::{self as proto, Reply, Request};

use crate::error::Error;
use crate::i18n::Lang;
use crate::paths::Paths;

/// Connects to the daemon of `paths`, which is started when none answers
/// and `start` says so (D6); `human` says whether a human answers the
/// planes' questions (D11).
pub fn connect(paths: &Paths, human: bool, lang: Lang, start: bool) -> Result<Client, Error> {
    let runtime = paths.runtime.as_deref().ok_or(Error::DaemonNoRuntime)?;
    let start = if start {
        agentlibre_client::Start::ThisExe
    } else {
        agentlibre_client::Start::No
    };
    Ok(Client::connect(
        &crate::daemon::socket_path(runtime),
        human,
        lang.code(),
        start,
    )?)
}

/// Who answers a plane's question: its kind and text in, the answer out.
pub type Answer = Box<dyn Fn(proto::Ask, &str) -> Option<String> + Send + Sync>;

/// A session's plane in the daemon, taken as a plane's handle is (AGL-075,
/// D7): a surface sends it the plane's commands and draws what it sends.
pub struct Remote {
    pub client: Client,
    session: Mutex<String>,
    /// Answers a plane's question during a command (D11); none, no answer.
    pub answer: Answer,
}

impl Remote {
    pub fn new(client: Client, session: String) -> Remote {
        Remote {
            client,
            session: Mutex::new(session),
            answer: Box::new(|_, _| None),
        }
    }

    /// The session's id, a new one after `/new`.
    pub fn session(&self) -> String {
        self.session.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The running turn stopped (no reply).
    pub fn stop(&self) {
        let _ = self.client.send(&Request::Stop {
            session: self.session(),
        });
    }

    /// The client no longer holds the session.
    pub fn close(&self) {
        let _ = self.client.ask(
            &Request::Close {
                session: self.session(),
            },
            &mut |_| {},
        );
    }

    /// Sends `command` and waits for its end; what the plane sends before it
    /// goes to `each`. `None` when the daemon is gone.
    pub fn ask(
        &self,
        command: crate::plane::Command,
        each: &mut dyn FnMut(crate::plane::Sent),
    ) -> Option<crate::plane::Done> {
        use crate::plane::{Command, wire};
        let session = self.session();
        let turn = matches!(command, Command::Say { .. });
        let request = match command {
            Command::Say {
                message,
                attached,
                reasoning,
                observer: _,
            } => Request::Say {
                session: session.clone(),
                message,
                attached: attached.iter().map(wire::attached).collect(),
                reasoning: reasoning.map(|level| level.name().to_owned()),
            },
            // A wake is the daemon's own hand on a run it holds; no
            // client sends one over the socket (AGL-121, D44).
            Command::Wake { .. } => unreachable!("a wake is the daemon's"),
            Command::Remember { name, text, workspace } => Request::Remember {
                session: session.clone(),
                name,
                text,
                workspace,
            },
            Command::Remove { name } => Request::Remove {
                session: session.clone(),
                name,
            },
            Command::LookAt(action) => Request::Look {
                session: session.clone(),
                look: match action {
                    crate::memory::MemoryAction::Show { name } => proto::Look::Show { name },
                    _ => proto::Look::List,
                },
            },
            Command::Rename(name) => Request::Rename {
                session: session.clone(),
                name,
            },
            Command::New => Request::New {
                session: session.clone(),
            },
            Command::Compact { reasoning } => Request::Compact {
                session: session.clone(),
                reasoning: reasoning.map(|level| level.name().to_owned()),
            },
            Command::Append(events) => Request::Record {
                session: session.clone(),
                lines: events
                    .into_iter()
                    .filter_map(|event| match event {
                        crate::session::Event::Command(run) => Some(wire::command(run)),
                        _ => None,
                    })
                    .collect(),
            },
            // A flow's verb works on the plane's store, not on its session
            // (AGL-118, D2): `agl plane flow` does it itself, and a daemon
            // that serves it is a later piece of this same layer.
            Command::Start | Command::History(_) => return None,
        };
        let failed = |error: Error| {
            let (turn, said) = if turn { (Some(Err(error)), None) } else { (None, Some(Err(error))) };
            crate::plane::Done {
                turn,
                why: None,
                said,
                session: crate::plane::SessionView {
                    id: session.clone(),
                    ..Default::default()
                },
            }
        };
        // What a turn another client started sent while this one waited is
        // not drawn now as this command's: only a question is answered.
        while let Some(reply) = self.client.try_next() {
            if let Reply::Question { .. } = reply {
                self.unasked(&session, reply, each);
            }
        }
        // The command's own reply first; what the plane sends follows.
        let mut each_unasked = |reply: Reply| self.unasked(&session, reply, each);
        if let Err(error) = self.client.ask(&request, &mut each_unasked).map_err(Error::from) {
            return Some(failed(error));
        }
        loop {
            match self.client.next().ok()? {
                Reply::Done { session: of, done, yours: true } if of == session => {
                    let done = done_of(done);
                    *self.session.lock().unwrap_or_else(|e| e.into_inner()) = done.session.id.clone();
                    return Some(done);
                }
                reply => self.unasked(&session, reply, each),
            }
        }
    }

    /// Waits for the end of a command the daemon runs of itself (the
    /// server's start of `open`); what the plane sends before it goes to
    /// `each`. `None` when the daemon is gone.
    pub fn wait(&self, each: &mut dyn FnMut(crate::plane::Sent)) -> Option<crate::plane::Done> {
        let session = self.session();
        loop {
            match self.client.next().ok()? {
                Reply::Done { session: of, done, yours: true } if of == session => return Some(done_of(done)),
                reply => self.unasked(&session, reply, each),
            }
        }
    }

    /// What the daemon sent unasked: the session's reports, a question.
    fn unasked(&self, session: &str, reply: Reply, each: &mut dyn FnMut(crate::plane::Sent)) {
        match reply {
            Reply::Sent { session: of, sent } if of == session => {
                each(crate::plane::wire::sent_of(sent));
            }
            Reply::Question { question, ask, text, .. } => {
                let answer = (self.answer)(ask, &text);
                let _ = self.client.send(&Request::Answer { question, answer });
            }
            _ => {}
        }
    }
}

/// A command's end as the plane's, its errors as the daemon said them.
pub fn done_of(done: proto::Done) -> crate::plane::Done {
    use crate::plane::{Text, wire};
    crate::plane::Done {
        turn: done
            .turn
            .map(|turn| turn.map(wire::turn_end_of).map_err(|text| Error::Said { text })),
        why: done.why,
        said: done.said.map(|said| {
            said.map(|said| Text {
                lines: said.lines.into_iter().map(wire::told_of).collect(),
                memory: said.memory.map(wire::write_of),
                content: said.content,
                failed: said.failed,
                interrupted: said.interrupted,
                record_error: said.record_error.map(|text| Error::Said { text }),
            })
            .map_err(|text| Error::Said { text })
        }),
        session: wire::view_of(done.session),
    }
}
