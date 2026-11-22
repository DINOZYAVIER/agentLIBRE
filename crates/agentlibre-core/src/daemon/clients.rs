//! The clients' half of the daemon: the connection, `request`'s parse and send, the asking (AGL-121, D28): split off `daemon/mod.rs`.

use super::*;

impl Daemon {
    /// A client's connection: its `hello`, then its requests.
    pub(super) fn client(self: Arc<Self>, stream: UnixStream) {
        let Ok(out) = stream.try_clone() else { return };
        let out = Arc::new(Mutex::new(out));
        let mut lines = Lines::with_limit(BufReader::new(stream), proto::LINE_LIMIT);
        let (human, lang, pid) = match lines.read::<Request>() {
            Ok(Read::Message(Request::Hello {
                version,
                human,
                lang,
                pid,
            })) if version == proto::VERSION => (human, Lang::from_code(&lang).unwrap_or(Lang::En), pid),
            Ok(Read::Message(Request::Hello { version, lang, .. })) => {
                let lang = Lang::from_code(&lang).unwrap_or(Lang::En);
                let error = tr!(lang, m::DAEMON_VERSION, theirs = version, ours = proto::VERSION);
                write(&out, &Reply::Refused { error });
                return;
            }
            _ => return,
        };
        let id = {
            let mut state = self.lock();
            state.next += 1;
            let id = state.next;
            state.clients.insert(
                id,
                Client {
                    out: Arc::clone(&out),
                    human,
                    lang,
                    pid,
                },
            );
            id
        };
        write(
            &out,
            &Reply::Hello {
                version: proto::VERSION,
                pid: std::process::id(),
            },
        );
        loop {
            match lines.read::<Request>() {
                Ok(Read::Message(request)) => {
                    if let Some(reply) = Arc::clone(&self).request(id, lang, request) {
                        write(&out, &reply);
                    }
                }
                // AGL-121, D54: a line over the limit is dropped; `Refused`
                // answers it, and the connection stays.
                Ok(Read::TooLong) => write(
                    &out,
                    &Reply::Refused {
                        error: tr!(lang, m::DAEMON_LINE_TOO_LONG, max = proto::LINE_LIMIT),
                    },
                ),
                _ => break,
            }
        }
        self.gone(id);
    }

    /// A request's reply; `None` when another thread sends it (`open`,
    /// `transcribe`).
    pub(super) fn request(self: Arc<Self>, client: u64, lang: Lang, request: Request) -> Option<Reply> {
        let refused = |error: Error| Some(Reply::Refused { error: error.message(lang) });
        match request {
            Request::Hello { .. } => Some(Reply::Ok),
            Request::Open(open) => {
                thread::spawn(move || {
                    let start = open.start_server;
                    match self.open(client, lang, open) {
                        Ok(opened) => {
                            let id = opened.session.id.clone();
                            self.send_to(client, &Reply::Opened(opened));
                            // The server's start follows the reply, so that
                            // the client has the session before its reports.
                            if start {
                                let _ = self.command(client, &id, |_| Command::Start, true);
                            }
                        }
                        Err(error) => self.send_to(client, &Reply::Refused { error: error.message(lang) }),
                    }
                });
                None
            }
            Request::Sessions => {
                let state = self.lock();
                let mut sessions: Vec<proto::SessionInfo> = state
                    .sessions
                    .values()
                    .map(|entry| proto::SessionInfo {
                        id: entry.opened.session.id.clone(),
                        name: entry.opened.session.name.clone(),
                        plane: entry.plane.clone(),
                        workspace: entry.opened.workspace.clone(),
                        function: entry.opened.function.clone(),
                        pids: entry
                            .holders
                            .iter()
                            .filter_map(|holder| state.clients.get(holder).map(|client| client.pid))
                            .collect(),
                        busy: entry.busy,
                    })
                    .collect();
                sessions.sort_by(|a, b| a.id.cmp(&b.id));
                Some(Reply::Sessions { sessions })
            }
            Request::Subscribe { session: None } => {
                self.lock().everyone.insert(client);
                Some(Reply::Ok)
            }
            Request::Subscribe { session: Some(id) } => match self.lock().sessions.get_mut(&id) {
                Some(entry) => {
                    entry.subscribers.insert(client);
                    Some(Reply::Ok)
                }
                None => refused(Error::DaemonNoSession { id }),
            },
            Request::Say {
                session,
                message,
                attached,
                reasoning,
            } => {
                let attached = attached.into_iter().filter_map(wire::attached_of).collect();
                let reasoning = reasoning.as_deref().and_then(ReasoningLevel::parse);
                self.command(client, &session, |entry| Command::Say {
                    message,
                    attached,
                    reasoning,
                    observer: entry.observer.take(),
                }, true)
                .map_or_else(refused, |()| Some(Reply::Ok))
            }
            // `stop` and `answer` have no reply: a client sends them from any
            // of its threads while another waits for a reply.
            Request::Stop { session } => {
                if let Some(entry) = self.lock().sessions.get(&session) {
                    entry.stop.request();
                }
                None
            }
            Request::Remember {
                session,
                name,
                text,
                workspace,
            } => self.plain(client, &session, Command::Remember { name, text, workspace }, lang),
            Request::Remove { session, name } => self.plain(client, &session, Command::Remove { name }, lang),
            Request::Look { session, look } => {
                let action = match look {
                    proto::Look::List => MemoryAction::List,
                    proto::Look::Show { name } => MemoryAction::Show { name },
                };
                self.plain(client, &session, Command::LookAt(action), lang)
            }
            Request::Rename { session, name } => self.plain(client, &session, Command::Rename(name), lang),
            Request::New { session } => self.plain(client, &session, Command::New, lang),
            Request::Compact { session, reasoning } => {
                let reasoning = reasoning.as_deref().and_then(ReasoningLevel::parse);
                self.command(client, &session, |_| Command::Compact { reasoning }, true)
                    .map_or_else(refused, |()| Some(Reply::Ok))
            }
            Request::Record { session, lines } => {
                let events = lines
                    .into_iter()
                    .map(|line| crate::session::Event::Command(wire::command_of(line)))
                    .collect();
                self.plain(client, &session, Command::Append(events), lang)
            }
            Request::Transcribe { dir, plane, audio } => {
                thread::spawn(move || {
                    let reply = match self.transcribe(&dir, plane.as_deref(), audio) {
                        Ok(text) => Reply::Transcribed { text },
                        Err(error) => Reply::Refused { error: error.message(lang) },
                    };
                    self.send_to(client, &reply);
                });
                None
            }
            Request::Answer { question, answer } => {
                if let Some((_, waiting)) = self.lock().questions.remove(&question) {
                    let _ = waiting.send(answer);
                }
                None
            }
            Request::Close { session } => {
                self.release(client, Some(&session));
                Some(Reply::Ok)
            }
            // The plane's thread answers when its command comes, after the
            // one it runs (AGL-073, D10).
            Request::History { session } => {
                let (to, from) = mpsc::channel();
                if let Err(error) = self.command(client, &session, |_| Command::History(to), false) {
                    return refused(error);
                }
                thread::spawn(move || {
                    let events = from.recv().unwrap_or_default();
                    self.send_to(
                        client,
                        &Reply::History {
                            session,
                            events: events.into_iter().map(wire::event).collect(),
                        },
                    );
                });
                None
            }
            // A verb of a plane's flows, done by the plane's holder: no
            // session and no model turn a flow (AGL-118, D12, D17).
            Request::Flow { plane, verb } => {
                let verb = match verb {
                    proto::FlowVerb::List => plane::flow::Verb::List,
                    proto::FlowVerb::Start { name } => plane::flow::Verb::Start { name },
                    proto::FlowVerb::Stop { name } => plane::flow::Verb::Stop { name },
                    proto::FlowVerb::Status { name } => plane::flow::Verb::Status { name },
                    proto::FlowVerb::Reset { name } => plane::flow::Verb::Reset { name },
                    proto::FlowVerb::Message { name, event } => {
                        plane::flow::Verb::Message { name, event }
                    }
                };
                match self.flow_holder(&plane) {
                    Ok(handle) => {
                        let said = handle.ask(verb, lang);
                        Some(Reply::Said { plane, said: wire::said(&said, lang) })
                    }
                    Err(error) => refused(error),
                }
            }
        }
    }

    /// A client gone: its sessions released, its questions unanswered.
    pub(super) fn gone(&self, client: u64) {
        {
            let mut state = self.lock();
            state.clients.remove(&client);
            state.everyone.remove(&client);
            state.questions.retain(|_, (asker, _)| *asker != client);
        }
        self.release(client, None);
    }

    pub(super) fn send_to(&self, client: u64, reply: &Reply) {
        let out = self.lock().clients.get(&client).map(|c| Arc::clone(&c.out));
        if let Some(out) = out {
            write(&out, reply);
        }
    }    /// Who answers a plane's questions (D11): the client that opened it
    /// when it has a human; the refusal otherwise, unless it said `yes`.
    pub(super) fn asker(self: &Arc<Self>, client: u64, human: bool, yes: bool) -> plane::Asker {
        let say = {
            let daemon = Arc::clone(self);
            Arc::new(move |line: &str| daemon.line(client, line))
        };
        let ask = |kind: Ask| {
            let daemon = Arc::clone(self);
            move |text: &str| daemon.ask(client, kind, text)
        };
        let others = if yes {
            Others::Yes
        } else if human {
            let ask = ask(Ask::Yes);
            Others::Ask(Arc::new(move |text, _| ask(text).as_deref().is_some_and(crate::tty::is_yes)))
        } else {
            Others::Refuse
        };
        plane::Asker {
            say,
            line: human.then(|| Arc::new(ask(Ask::Line)) as plane::AskLine),
            others,
        }
    }

    /// A line told while a plane opens, before it has a session.
    pub(super) fn line(&self, client: u64, line: &str) {
        self.send_to(
            client,
            &Reply::Sent {
                session: String::new(),
                sent: proto::Sent::Line(line.to_owned()),
            },
        );
    }

    /// A question to `client`, and its answer; `None` when it is gone.
    pub(super) fn ask(&self, client: u64, ask: Ask, text: &str) -> Option<String> {
        let (tell, answer) = mpsc::channel();
        let question = {
            let mut state = self.lock();
            if !state.clients.contains_key(&client) {
                return None;
            }
            state.next += 1;
            let question = state.next;
            state.questions.insert(question, (client, tell));
            question
        };
        self.send_to(
            client,
            &Reply::Question {
                question,
                session: String::new(),
                ask,
                text: text.to_owned(),
            },
        );
        answer.recv().ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    use crate::daemon::tests::{Client, daemon};
    use crate::forge::testutil::TempDir;

    #[test]
    fn a_client_of_another_version_is_refused() {
        let dir = TempDir::new();
        let (_, socket) = daemon(&dir);
        let (_, hello) = Client::new(&socket, proto::VERSION + 1);
        assert!(matches!(hello, Reply::Refused { error } if error.contains("protocol")));
        let (mut client, hello) = Client::new(&socket, proto::VERSION);
        assert!(matches!(hello, Reply::Hello { version: proto::VERSION, .. }));
        assert_eq!(client.ask(&Request::Sessions), Reply::Sessions { sessions: Vec::new() });
        assert!(matches!(
            client.ask(&Request::Subscribe { session: Some("none".into()) }),
            Reply::Refused { .. }
        ));
    }

    #[test]
    fn a_line_over_the_limit_is_refused_and_the_connection_stays() {
        // AGL-121, D54: the daemon drops the line, answers `Refused`,
        // and goes on serving.
        let dir = TempDir::new();
        let (_, socket) = daemon(&dir);
        let (mut client, hello) = Client::new(&socket, proto::VERSION);
        assert!(matches!(hello, Reply::Hello { .. }));
        let long = format!("{}{}\n", r#"{"request":"sessions"} "#, "x".repeat(proto::LINE_LIMIT));
        client.out.write_all(long.as_bytes()).unwrap();
        assert!(matches!(
            client.next(),
            Reply::Refused { error } if error.contains("bytes")
        ));
        assert_eq!(client.ask(&Request::Sessions), Reply::Sessions { sessions: Vec::new() });
    }
}
