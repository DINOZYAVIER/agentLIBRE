//! A client of the daemon (AGL-076, D5): its connection, the daemon started
//! when none answers (D6), the requests and the replies, and what the
//! daemon sends unasked — what a session does and a plane's questions.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use agentlibre_proto::{self as proto, Lines, Reply, Request};

use crate::error::Error;
use crate::i18n::Lang;
use crate::paths::Paths;

/// How long a daemon that was started may take to answer.
const START_WAIT: Duration = Duration::from_secs(10);

pub struct Client {
    out: Mutex<UnixStream>,
    from: Mutex<Receiver<Reply>>,
    /// The daemon's pid, from its `hello`.
    pub pid: u32,
}

impl Client {
    /// Connects to the daemon of `paths`, which is started when none
    /// answers and `start` says so (D6); `human` says whether a human
    /// answers the planes' questions (D11).
    pub fn connect(paths: &Paths, human: bool, lang: Lang, start: bool) -> Result<Client, Error> {
        let runtime = paths.runtime.as_deref().ok_or(Error::DaemonNoRuntime)?;
        let socket = crate::daemon::socket_path(runtime);
        let stream = match UnixStream::connect(&socket) {
            Ok(stream) => stream,
            Err(_) if start => start_daemon(&socket)?,
            Err(source) => {
                return Err(Error::Daemon {
                    detail: format!("{}: {source}", socket.display()),
                });
            }
        };
        Client::of(stream, human, lang)
    }

    /// A client on `stream`, its `hello` said.
    pub fn of(stream: UnixStream, human: bool, lang: Lang) -> Result<Client, Error> {
        let fail = |source: std::io::Error| Error::Daemon {
            detail: source.to_string(),
        };
        let reader = stream.try_clone().map_err(fail)?;
        let (to, from) = mpsc::channel();
        thread::spawn(move || {
            let mut lines = Lines::new(BufReader::new(reader));
            while let Ok(Some(reply)) = lines.read::<Reply>() {
                if to.send(reply).is_err() {
                    break;
                }
            }
        });
        let client = Client {
            out: Mutex::new(stream),
            from: Mutex::new(from),
            pid: 0,
        };
        let code = match lang {
            Lang::En => "en",
            Lang::Uk => "uk",
        };
        match client.ask(
            &Request::Hello {
                version: proto::VERSION,
                human,
                lang: code.into(),
                pid: std::process::id(),
            },
            &mut |_| {},
        )? {
            Reply::Hello { pid, .. } => Ok(Client { pid, ..client }),
            other => Err(unexpected(other)),
        }
    }

    /// Sends `request` and waits for its reply; what the daemon sends
    /// meanwhile unasked goes to `each`. A refusal is an error.
    pub fn ask(&self, request: &Request, each: &mut dyn FnMut(Reply)) -> Result<Reply, Error> {
        self.send(request)?;
        loop {
            match self.next()? {
                Reply::Refused { error } => return Err(Error::Said { text: error }),
                reply if is_unasked(&reply) => each(reply),
                reply => return Ok(reply),
            }
        }
    }

    /// Sends `request`; its reply comes as the others.
    pub fn send(&self, request: &Request) -> Result<(), Error> {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        proto::send(&mut *out, request).map_err(|source| Error::Daemon {
            detail: source.to_string(),
        })
    }

    /// What the daemon sends next.
    pub fn next(&self) -> Result<Reply, Error> {
        let from = self.from.lock().unwrap_or_else(|e| e.into_inner());
        from.recv().map_err(|_| Error::PlaneGone)
    }
}

/// What the daemon sends without a request: what a session does, a plane's
/// question.
pub fn is_unasked(reply: &Reply) -> bool {
    matches!(reply, Reply::Sent { .. } | Reply::Done { .. } | Reply::Question { .. })
}

fn unexpected(reply: Reply) -> Error {
    Error::Daemon {
        detail: format!("an unexpected reply: {reply:?}"),
    }
}

/// Starts `agl daemon` apart from this process and waits until its socket
/// answers (D6).
fn start_daemon(socket: &Path) -> Result<UnixStream, Error> {
    let fail = |detail: String| Error::Daemon { detail };
    let program = std::env::current_exe().map_err(|error| fail(error.to_string()))?;
    let mut command = Command::new(program);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Its own session: the terminal's Ctrl-C and its end do not reach it.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn().map_err(|error| fail(error.to_string()))?;
    let since = Instant::now();
    loop {
        if let Ok(stream) = UnixStream::connect(socket) {
            return Ok(stream);
        }
        if since.elapsed() > START_WAIT {
            return Err(fail(format!("{} does not answer", socket.display())));
        }
        thread::sleep(Duration::from_millis(20));
    }
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
            Command::Start => return None,
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
        // The command's own reply first; what the plane sends follows.
        let mut each_unasked = |reply: Reply| self.unasked(&session, reply, each);
        if let Err(error) = self.client.ask(&request, &mut each_unasked) {
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
    use crate::plane::{Said, wire};
    crate::plane::Done {
        turn: done
            .turn
            .map(|turn| turn.map(wire::turn_end_of).map_err(|text| Error::Said { text })),
        why: done.why,
        said: done.said.map(|said| {
            said.map(|said| Said {
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
