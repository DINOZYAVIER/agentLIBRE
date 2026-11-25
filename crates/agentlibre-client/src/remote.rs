//! A session held over the daemon's protocol (AGL-076, D5, D7; AGL-130, D4,
//! D5): a surface sends it commands and draws what it sends, on the
//! protocol's types alone. What knows agl's paths and its language
//! (`connect`) is the terminal's (D6).

use std::collections::VecDeque;
use std::sync::Mutex;

use agentlibre_proto::{self as proto, Reply, Request};

use crate::{Client, Error};

/// What a surface asks of its session, over the socket (AGL-130, D4). A
/// flow's wake and the start of the server are the run's own, and are not
/// here.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// A message and the media it names, at a reasoning level.
    Say {
        message: String,
        attached: Vec<proto::Attached>,
        reasoning: Option<String>,
    },
    /// `/remember [--workspace] name | text`.
    Remember { name: String, text: String, workspace: bool },
    /// `/memory remove <name>` once agreed.
    Remove { name: String },
    /// `/memory list`, `/memory show`.
    Look(proto::Look),
    /// `/rename <name>`.
    Rename(String),
    /// `/new`: a new session on the plane.
    New,
    /// `/compact`, at a reasoning level.
    Compact { reasoning: Option<String> },
    /// The lines of a surface's own commands, for the session's record.
    Record(Vec<proto::CommandLine>),
}

/// Who answers a plane's question: its kind and text in, the answer out.
pub type Answer = Box<dyn Fn(proto::Ask, &str) -> Option<String> + Send + Sync>;

/// A reply that came while the client waited for no command (AGL-078).
#[derive(Debug, PartialEq)]
pub enum AtPrompt {
    /// What a session of this client did.
    Sent(proto::Sent),
    /// A command of another client on it has ended.
    OtherDone,
}

/// A session's plane in the daemon, taken as a plane's handle is (AGL-075,
/// D7): a surface sends it the plane's commands and draws what it sends.
pub struct Remote {
    pub client: Client,
    session: Mutex<String>,
    /// What came before the past it was read with, and is not in it.
    waiting: Mutex<VecDeque<Reply>>,
    /// Answers a plane's question during a command (D11); none, no answer.
    pub answer: Answer,
}

impl Remote {
    pub fn new(client: Client, session: String) -> Remote {
        Remote {
            client,
            session: Mutex::new(session),
            waiting: Mutex::new(VecDeque::new()),
            answer: Box::new(|_, _| None),
        }
    }

    /// The session's past (AGL-126, D10): its events, its phases' times and
    /// what its turn is writing. What the session did before it is in it and
    /// is not given again; what the session does next follows it.
    pub fn past(&self) -> Result<(Vec<proto::Event>, Vec<proto::PhaseTime>, proto::Writing), Error> {
        let mut before = VecDeque::new();
        let reply = self.client.ask(&Request::History { session: self.session() }, &mut |reply| {
            let told = matches!(
                &reply,
                Reply::Sent {
                    sent: proto::Sent::Report(
                        proto::Report::Event(_)
                            | proto::Report::Thinking(_)
                            | proto::Report::ActionStarted
                            | proto::Report::ActionText(_)
                    ),
                    ..
                }
            );
            if !told {
                before.push_back(reply);
            }
        })?;
        self.waiting.lock().unwrap_or_else(|e| e.into_inner()).extend(before);
        match reply {
            Reply::History {
                events,
                transitions,
                writing,
                ..
            } => Ok((events, transitions, writing)),
            reply => Err(Error::Daemon {
                detail: format!("an unexpected reply: {reply:?}"),
            }),
        }
    }

    fn waited(&self) -> Option<Reply> {
        self.waiting.lock().unwrap_or_else(|e| e.into_inner()).pop_front()
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

    /// Sends `command` and waits for its end; what the session sends before
    /// it goes to `each`. A refusal of the daemon is an end whose `turn` (of
    /// `Say`) or `said` (of the others) is the refusal's text (D5); the
    /// daemon gone is `Error::Gone`; any other error is returned.
    pub fn ask(
        &self,
        command: Command,
        each: &mut dyn FnMut(proto::Sent),
    ) -> Result<proto::Done, Error> {
        let session = self.session();
        let turn = matches!(command, Command::Say { .. });
        let request = match command {
            Command::Say {
                message,
                attached,
                reasoning,
            } => Request::Say {
                session: session.clone(),
                message,
                attached,
                reasoning,
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
            Command::Look(look) => Request::Look {
                session: session.clone(),
                look,
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
                reasoning,
            },
            Command::Record(lines) => Request::Record {
                session: session.clone(),
                lines,
            },
        };
        // What a turn another client started sent while this one waited is
        // not drawn now as this command's: only a question is answered.
        while let Some(reply) = self.waited().or_else(|| self.client.try_next()) {
            if let Reply::Question { .. } = reply {
                self.unasked(&session, reply, each);
            }
        }
        // The command's own reply first; what the session sends follows.
        let mut each_unasked = |reply: Reply| self.unasked(&session, reply, each);
        match self.client.ask(&request, &mut each_unasked) {
            Ok(_) => {}
            Err(Error::Said { text }) => return Ok(refused(&session, turn, text)),
            Err(error) => return Err(error),
        }
        loop {
            match self.client.next()? {
                // A `Done` counts only when it is this client's, of this
                // session; it names the session after the command (`/new`).
                Reply::Done { session: of, done, yours: true } if of == session => {
                    *self.session.lock().unwrap_or_else(|e| e.into_inner()) = done.session.id.clone();
                    return Ok(done);
                }
                reply => self.unasked(&session, reply, each),
            }
        }
    }

    /// One reply that came while this client waited for no command (AGL-078):
    /// what its session did, or the end of another client's command on it.
    /// A plane's question is answered as it would be during a command, a
    /// reply of no interest dropped; `None` when nothing waits.
    pub fn at_prompt(&self) -> Option<AtPrompt> {
        loop {
            let reply = self.waited().or_else(|| self.client.try_next())?;
            let session = self.session();
            match reply {
                Reply::Sent { session: of, sent } if of == session => {
                    return Some(AtPrompt::Sent(sent));
                }
                Reply::Done { session: of, yours: false, .. } if of == session => {
                    return Some(AtPrompt::OtherDone);
                }
                reply => self.unasked(&session, reply, &mut drop),
            }
        }
    }

    /// Waits for the end of a command the daemon runs of itself (the
    /// server's start of `open`); what the session sends before it goes to
    /// `each`. `None` when the daemon is gone.
    pub fn wait(&self, each: &mut dyn FnMut(proto::Sent)) -> Option<proto::Done> {
        let session = self.session();
        loop {
            match self.waited().map_or_else(|| self.client.next(), Ok).ok()? {
                Reply::Done { session: of, done, yours: true } if of == session => return Some(done),
                reply => self.unasked(&session, reply, each),
            }
        }
    }

    /// What the daemon sent unasked: the session's reports, a question.
    fn unasked(&self, session: &str, reply: Reply, each: &mut dyn FnMut(proto::Sent)) {
        match reply {
            Reply::Sent { session: of, sent } if of == session => each(sent),
            Reply::Question { question, ask, text, .. } => {
                let answer = (self.answer)(ask, &text);
                let _ = self.client.send(&Request::Answer { question, answer });
            }
            _ => {}
        }
    }
}

/// The end of a command the daemon refused (D5): the refusal is the turn's
/// error of a `Say`, the saying's of the others, and the session is named by
/// its id alone.
fn refused(session: &str, turn: bool, text: String) -> proto::Done {
    let (turn, said) = if turn {
        (Some(Err(text)), None)
    } else {
        (None, Some(Err(text)))
    };
    proto::Done {
        turn,
        why: None,
        said,
        session: proto::SessionView {
            id: session.to_owned(),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::thread;

    use agentlibre_proto::{Lines, Read};

    use super::*;

    /// What the daemon of a test got and sends: its lines in and out.
    struct Daemon {
        lines: Lines<BufReader<UnixStream>>,
        out: UnixStream,
    }

    impl Daemon {
        fn request(&mut self) -> Request {
            match self.lines.read::<Request>().unwrap() {
                Read::Message(request) => request,
                other => panic!("no request: {other:?}"),
            }
        }

        fn say(&mut self, reply: Reply) {
            proto::send(&mut self.out, &reply).unwrap();
        }
    }

    /// A client on one end of a pair of sockets, and a thread that plays the
    /// daemon on the other: it answers `hello`, then does `script`.
    fn held(script: impl FnOnce(&mut Daemon) + Send + 'static) -> (Remote, thread::JoinHandle<()>) {
        let (client, server) = UnixStream::pair().unwrap();
        let play = thread::spawn(move || {
            let mut daemon = Daemon {
                lines: Lines::new(BufReader::new(server.try_clone().unwrap())),
                out: server,
            };
            assert!(matches!(daemon.request(), Request::Hello { .. }));
            daemon.say(Reply::Hello {
                version: proto::VERSION,
                pid: 1,
            });
            script(&mut daemon);
        });
        let client = Client::of(client, false, "en").unwrap();
        (Remote::new(client, "s1".into()), play)
    }

    fn view(id: &str) -> proto::SessionView {
        proto::SessionView {
            id: id.into(),
            ..Default::default()
        }
    }

    fn done_of_say(id: &str) -> proto::Done {
        proto::Done {
            turn: Some(Ok(proto::TurnEnd::Answer("hi".into()))),
            why: None,
            said: None,
            session: view(id),
        }
    }

    fn refuse(text: &str) -> Reply {
        Reply::Refused { error: text.into() }
    }

    fn say() -> Command {
        Command::Say {
            message: "hello".into(),
            attached: Vec::new(),
            reasoning: None,
        }
    }

    #[test]
    fn a_refusal_of_a_message_is_the_end_of_its_turn() {
        let (remote, play) = held(|daemon| {
            assert!(matches!(daemon.request(), Request::Say { .. }));
            daemon.say(refuse("no such session"));
        });
        let done = remote.ask(say(), &mut |_| {}).unwrap();
        assert_eq!(done.turn, Some(Err("no such session".to_owned())));
        assert_eq!(done.said, None);
        assert_eq!(done.session.id, "s1");
        play.join().unwrap();
    }

    #[test]
    fn a_refusal_of_a_slash_command_is_what_it_said() {
        let (remote, play) = held(|daemon| {
            assert!(matches!(daemon.request(), Request::Rename { .. }));
            daemon.say(refuse("busy"));
        });
        let done = remote.ask(Command::Rename("x".into()), &mut |_| {}).unwrap();
        assert_eq!(done.turn, None);
        assert_eq!(done.said, Some(Err("busy".to_owned())));
        play.join().unwrap();
    }

    #[test]
    fn the_end_of_the_stream_is_the_daemon_gone() {
        let (remote, play) = held(|daemon| {
            assert!(matches!(daemon.request(), Request::Say { .. }));
            daemon.say(Reply::Ok);
            // The stream ends here, before the command's end.
        });
        let error = remote.ask(say(), &mut |_| {}).unwrap_err();
        assert!(matches!(error, Error::Gone), "{error:?}");
        play.join().unwrap();
    }

    #[test]
    fn what_the_session_sends_goes_to_each_and_the_end_names_the_session() {
        let (remote, play) = held(|daemon| {
            assert!(matches!(daemon.request(), Request::New { .. }));
            daemon.say(Reply::Ok);
            daemon.say(Reply::Sent {
                session: "s1".into(),
                sent: proto::Sent::Line("up".into()),
            });
            // Another session's, and another client's end: not for this command.
            daemon.say(Reply::Sent {
                session: "other".into(),
                sent: proto::Sent::Over,
            });
            daemon.say(Reply::Done {
                session: "s1".into(),
                done: done_of_say("not mine"),
                yours: false,
            });
            daemon.say(Reply::Done {
                session: "s1".into(),
                done: proto::Done {
                    turn: None,
                    why: None,
                    said: Some(Ok(proto::Text::default())),
                    session: view("s2"),
                },
                yours: true,
            });
        });
        let mut sent = Vec::new();
        let done = remote.ask(Command::New, &mut |item| sent.push(item)).unwrap();
        assert_eq!(sent, [proto::Sent::Line("up".into())]);
        assert_eq!(done.session.id, "s2");
        assert_eq!(remote.session(), "s2", "the session is the new one");
        play.join().unwrap();
    }

    #[test]
    fn a_question_is_answered_by_the_answer() {
        let (answers, asked) = mpsc::channel();
        let (mut remote, play) = held(move |daemon| {
            assert!(matches!(daemon.request(), Request::Say { .. }));
            daemon.say(Reply::Question {
                question: 7,
                session: "s1".into(),
                ask: proto::Ask::Yes,
                text: "restart?".into(),
            });
            let request = daemon.request();
            answers.send(request).unwrap();
            daemon.say(Reply::Ok);
            daemon.say(Reply::Done {
                session: "s1".into(),
                done: done_of_say("s1"),
                yours: true,
            });
        });
        remote.answer = Box::new(|ask, text| {
            assert_eq!((ask, text), (proto::Ask::Yes, "restart?"));
            Some("yes".into())
        });
        let done = remote.ask(say(), &mut |_| {}).unwrap();
        assert_eq!(done.turn, Some(Ok(proto::TurnEnd::Answer("hi".into()))));
        assert_eq!(
            asked.recv().unwrap(),
            Request::Answer {
                question: 7,
                answer: Some("yes".into())
            }
        );
        play.join().unwrap();
    }

    #[test]
    fn the_past_drops_a_told_event_and_keeps_an_end() {
        let (remote, play) = held(|daemon| {
            assert!(matches!(daemon.request(), Request::History { .. }));
            // The session's own event is in the past below: not given twice.
            daemon.say(Reply::Sent {
                session: "s1".into(),
                sent: proto::Sent::Report(proto::Report::Event(proto::Event::User("hi".into()))),
            });
            daemon.say(Reply::Done {
                session: "s1".into(),
                done: done_of_say("s1"),
                yours: false,
            });
            daemon.say(Reply::History {
                session: "s1".into(),
                events: vec![proto::Event::User("hi".into())],
                transitions: Vec::new(),
                writing: proto::Writing::default(),
            });
        });
        let (events, transitions, writing) = remote.past().unwrap();
        assert_eq!(events, [proto::Event::User("hi".into())]);
        assert!(transitions.is_empty() && writing.is_empty());
        assert_eq!(remote.at_prompt(), Some(AtPrompt::OtherDone));
        assert_eq!(remote.at_prompt(), None);
        play.join().unwrap();
    }
}
