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
                    match self.open(client, lang, open, true) {
                        Ok(opened) => {
                            let id = opened.session.id.clone();
                            self.send_to(client, &Reply::Opened(opened));
                            // The server's start follows the reply, so that
                            // the client has the session before its reports.
                            if start {
                                let _ = self.command(client, &id, |_| Command::Start, true, false);
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
                        plane: state
                            .plane_of(&entry.opened.session.id)
                            .map(|plane| plane.file.name.clone()),
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
                None => refused(super::error::Error::NoSession { id }.into()),
            },
            Request::Say {
                session,
                message,
                attached,
                reasoning,
            } => {
                let attached = attached.into_iter().filter_map(wire::attached_of).collect();
                let reasoning = reasoning.as_deref().and_then(ReasoningLevel::parse);
                let say = move |daemon: &Daemon, session: &str| {
                    daemon.accept(client, session, |entry| Command::Say {
                        message,
                        attached,
                        reasoning,
                        observer: entry.observer.take(),
                    }, true, lang)
                };
                if self.lock().sessions.contains_key(&session) {
                    return say(&self, &session);
                }
                // A session that is not loaded is loaded from its file
                // (AGL-125, D1, D4); the client follows it and does not hold
                // it.
                thread::spawn(move || {
                    let refused = match self.load(client, lang, &session) {
                        Ok(id) => say(&self, &id),
                        Err(error) => Some(Reply::Refused { error: error.message(lang) }),
                    };
                    if let Some(reply) = refused {
                        self.send_to(client, &reply);
                    }
                });
                None
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
                self.accept(client, &session, |_| Command::Compact { reasoning }, true, lang)
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
                let mut state = self.lock();
                if answer.is_some() {
                    if let Some(question) = state.questions.remove(&question) {
                        let _ = question.tell.send(answer);
                    }
                } else if let Some(waiting) = state.questions.get_mut(&question) {
                    // One that declines leaves it to the others; the last
                    // one's is the answer no.
                    waiting.asked.remove(&client);
                    if waiting.asked.is_empty()
                        && let Some(question) = state.questions.remove(&question)
                    {
                        let _ = question.tell.send(None);
                    }
                }
                None
            }
            Request::Close { session } => {
                self.release(client, Some(&session));
                Some(Reply::Ok)
            }
            // Answered while a command runs (AGL-126, D10), at one point of
            // what the session sends: under the lock `tell` sends under.
            Request::History { session } => {
                thread::spawn(move || {
                    let transitions = Session::open(&self.paths.sessions(), &session)
                        .map(|session| session.transitions)
                        .unwrap_or_default();
                    let state = self.lock();
                    let reply = match state.sessions.get(&session) {
                        Some(entry) => Reply::History {
                            events: entry.past.events.iter().cloned().map(wire::event).collect(),
                            transitions: transitions.into_iter().map(wire::phase_time).collect(),
                            writing: entry.past.writing.clone(),
                            session,
                        },
                        None => Reply::Refused {
                            error: Error::from(super::error::Error::NoSession { id: session }).message(lang),
                        },
                    };
                    if let Some(client) = state.clients.get(&client) {
                        write(&client.out, &reply);
                    }
                });
                None
            }
            // The stores, read apart from the sessions' threads (AGL-126): a
            // Function's load evaluates Nix.
            request @ (Request::Past { .. }
            | Request::Planes { .. }
            | Request::Functions { .. }
            | Request::Function { .. }) => {
                thread::spawn(move || {
                    let reply = self
                        .look(request, lang)
                        .unwrap_or_else(|error| Reply::Refused { error: error.message(lang) });
                    self.send_to(client, &reply);
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
                        Some(Reply::Text { plane, said: wire::said(&said, lang) })
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
            // A question of an opening goes with its client; a session's
            // stays for whoever holds it next (AGL-125, D7).
            state
                .questions
                .retain(|_, question| question.asking.is_some() || !question.asked.contains(&client));
            for question in state.questions.values_mut() {
                question.asked.remove(&client);
            }
        }
        self.release(client, None);
    }

    pub(super) fn send_to(&self, client: u64, reply: &Reply) {
        let out = self.lock().clients.get(&client).map(|c| Arc::clone(&c.out));
        if let Some(out) = out {
            write(&out, reply);
        }
    }

    /// Who answers a run's questions (D11; AGL-125, D7): the client that
    /// opens it, when it has a human, then whoever holds its session; the
    /// refusal when they have none, unless the client said `yes`.
    pub(super) fn asker(self: &Arc<Self>, asking: &Arc<Asking>, yes: bool) -> plane::Asker {
        let say = {
            let daemon = Arc::clone(self);
            let client = asking.client;
            Arc::new(move |line: &str| daemon.line(client, line))
        };
        let others = if yes {
            Others::Yes
        } else {
            let (daemon, asking) = (Arc::clone(self), Arc::clone(asking));
            Others::Session(Arc::new(move |text, _| match daemon.ask(&asking, Ask::Yes, text) {
                Said::Answer(answer) if answer.as_deref().is_some_and(agentlibre_common::tty::is_yes) => Consent::Yes,
                Said::Answer(_) => Consent::No,
                Said::Nobody => Consent::Nobody,
                Said::Unanswered => Consent::Unanswered,
            }))
        };
        plane::Asker {
            say,
            line: asking.human.then(|| {
                let (daemon, asking) = (Arc::clone(self), Arc::clone(asking));
                Arc::new(move |text: &str| match daemon.ask(&asking, Ask::Line, text) {
                    Said::Answer(answer) => answer,
                    Said::Nobody | Said::Unanswered => None,
                }) as plane::AskLine
            }),
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

    /// A question, and what it got: of the client that opens a run, or of
    /// those who hold its session; one that nobody holds waits for the first
    /// to come, at most its `session_idle` (AGL-125, D7).
    pub(super) fn ask(&self, asking: &Arc<Asking>, kind: Ask, text: &str) -> Said {
        let (tell, answer) = mpsc::channel();
        let number = {
            let mut state = self.lock();
            let session = asking.session();
            let asked: HashSet<u64> = match &session {
                None if !asking.human => return Said::Nobody,
                None if !state.clients.contains_key(&asking.client) => return Said::Answer(None),
                None => HashSet::from([asking.client]),
                Some(id) => {
                    let humans = state.humans(id);
                    if humans.is_empty() && state.sessions.get(id).is_some_and(|entry| !entry.holders.is_empty()) {
                        return Said::Nobody;
                    }
                    humans
                }
            };
            state.next += 1;
            let number = state.next;
            for client in &asked {
                state.question_to(*client, number, session.as_deref(), kind, text);
            }
            state.questions.insert(
                number,
                Question {
                    asking: session.map(|_| Arc::clone(asking)),
                    asked,
                    kind,
                    text: text.to_owned(),
                    tell,
                },
            );
            number
        };
        let mut unwatched: Option<Instant> = None;
        loop {
            match answer.recv_timeout(Duration::from_millis(200)) {
                Ok(said) => return Said::Answer(said),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Said::Answer(None),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            let mut state = self.lock();
            // Answered, or its opening's client gone, meanwhile.
            let Some(question) = state.questions.get(&number) else { continue };
            let Some(id) = question.asking.as_ref().and_then(|asking| asking.session()) else {
                continue;
            };
            if asking.stop.requested() {
                state.questions.remove(&number);
                return Said::Answer(None);
            }
            if !state.humans(&id).is_empty() {
                unwatched = None;
                continue;
            }
            let since = *unwatched.get_or_insert_with(Instant::now);
            if !asking.idle.is_zero() && since.elapsed() >= asking.idle {
                state.questions.remove(&number);
                return Said::Unanswered;
            }
        }
    }
}

impl State {
    /// The clients with a human that hold the session `id`.
    fn humans(&self, id: &str) -> HashSet<u64> {
        self.sessions
            .get(id)
            .map(|entry| {
                entry
                    .holders
                    .iter()
                    .copied()
                    .filter(|holder| self.clients.get(holder).is_some_and(|client| client.human))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn question_to(&self, client: u64, number: u64, session: Option<&str>, ask: Ask, text: &str) {
        if let Some(client) = self.clients.get(&client) {
            write(
                &client.out,
                &Reply::Question {
                    question: number,
                    session: session.unwrap_or_default().to_owned(),
                    ask,
                    text: text.to_owned(),
                },
            );
        }
    }

    /// The questions of the session `id` that wait, to a client that came.
    pub(super) fn show_questions(&mut self, client: u64, id: &str) {
        if !self.clients.get(&client).is_some_and(|client| client.human) {
            return;
        }
        let waiting: Vec<(u64, Ask, String)> = self
            .questions
            .iter_mut()
            .filter(|(_, question)| {
                question.asking.as_ref().and_then(|asking| asking.session()).as_deref() == Some(id)
            })
            .map(|(number, question)| {
                question.asked.insert(client);
                (*number, question.kind, question.text.clone())
            })
            .collect();
        for (number, kind, text) in waiting {
            self.question_to(client, number, Some(id), kind, &text);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    use crate::daemon::tests::{Client, daemon};
    use agentlibre_common::testutil::TempDir;

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

    /// A daemon on `dir` without its socket, and a session of it that nobody
    /// holds, whose questions wait for `idle`.
    fn asked(idle: Duration) -> (Arc<Daemon>, Arc<Asking>) {
        let (needs, _) = mpsc::channel();
        let daemon = Arc::new(Daemon {
            needs,
            paths: Paths {
                config: PathBuf::new(),
                data: PathBuf::new(),
                state: PathBuf::new(),
                runtime: None,
            },
            state: Mutex::new(State::default()),
            loading: Mutex::new(HashSet::new()),
            loaded: Condvar::new(),
            flow_gate: flow::Gate::default(),
        });
        let asking = Arc::new(Asking {
            client: 1,
            human: false,
            idle,
            stop: Stop::new(),
            session: Mutex::new(Some("s".into())),
        });
        (daemon, asking)
    }

    /// A client of `daemon`, and its end of the socket.
    fn joined(daemon: &Daemon, number: u64) -> Lines<BufReader<UnixStream>> {
        let (theirs, ours) = UnixStream::pair().unwrap();
        ours.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let client = super::super::Client {
            out: Arc::new(Mutex::new(theirs)),
            human: true,
            lang: Lang::En,
            pid: 0,
        };
        daemon.lock().clients.insert(number, client);
        Lines::new(BufReader::new(ours))
    }

    fn question(lines: &mut Lines<BufReader<UnixStream>>) -> u64 {
        match lines.read::<Reply>().unwrap() {
            Read::Message(Reply::Question { question, session, text, .. }) => {
                assert_eq!((session.as_str(), text.as_str()), ("s", "start it?"));
                question
            }
            other => panic!("no question: {other:?}"),
        }
    }

    fn waits(daemon: &Arc<Daemon>, asking: &Arc<Asking>) -> thread::JoinHandle<Said> {
        let (daemon, asking) = (Arc::clone(daemon), Arc::clone(asking));
        let asks = thread::spawn({
            let daemon = Arc::clone(&daemon);
            move || daemon.ask(&asking, Ask::Yes, "start it?")
        });
        while daemon.lock().questions.is_empty() {
            thread::sleep(Duration::from_millis(5));
        }
        asks
    }

    #[test]
    fn a_question_nobody_is_there_for_waits_for_the_first_to_come() {
        // AGL-125, D7.
        let (daemon, asking) = asked(Duration::from_secs(60));
        let asks = waits(&daemon, &asking);
        let mut first = joined(&daemon, 7);
        let mut second = joined(&daemon, 8);
        for client in [7, 8] {
            daemon.lock().show_questions(client, "s");
        }
        let number = question(&mut first);
        assert_eq!(question(&mut second), number);
        // One that declines leaves it to the other.
        let answer = |client: u64, answer: Option<&str>| {
            Arc::clone(&daemon).request(client, Lang::En, Request::Answer {
                question: number,
                answer: answer.map(str::to_owned),
            })
        };
        assert_eq!(answer(7, None), None);
        assert!(!asks.is_finished() && !daemon.lock().questions.is_empty());
        assert_eq!(answer(8, Some("y")), None);
        assert_eq!(asks.join().unwrap(), Said::Answer(Some("y".into())));
    }

    #[test]
    fn a_question_nobody_came_for_ends_at_its_time_and_at_a_stop() {
        // AGL-125, D7.
        let (daemon, asking) = asked(Duration::from_millis(300));
        assert_eq!(waits(&daemon, &asking).join().unwrap(), Said::Unanswered);
        assert!(daemon.lock().questions.is_empty());
        let (daemon, asking) = asked(Duration::ZERO);
        let asks = waits(&daemon, &asking);
        thread::sleep(Duration::from_millis(600));
        assert!(!asks.is_finished(), "0 is no time at all");
        asking.stop.request();
        assert_eq!(asks.join().unwrap(), Said::Answer(None));
    }

    #[test]
    fn a_session_question_outlives_the_client_that_saw_it() {
        // AGL-125, D7: an opening's does not.
        let (daemon, asking) = asked(Duration::from_secs(60));
        let asks = waits(&daemon, &asking);
        let mut first = joined(&daemon, 7);
        daemon.lock().show_questions(7, "s");
        let number = question(&mut first);
        daemon.gone(7);
        assert!(daemon.lock().questions.contains_key(&number) && !asks.is_finished());
        let mut second = joined(&daemon, 8);
        daemon.lock().show_questions(8, "s");
        assert_eq!(question(&mut second), number);
        let _ = Arc::clone(&daemon).request(8, Lang::En, Request::Answer {
            question: number,
            answer: None,
        });
        assert_eq!(asks.join().unwrap(), Said::Answer(None));
        let opening = Arc::new(Asking {
            client: 9,
            human: true,
            idle: Duration::from_secs(60),
            stop: Stop::new(),
            session: Mutex::new(None),
        });
        let _third = joined(&daemon, 9);
        let asks = waits(&daemon, &opening);
        daemon.gone(9);
        assert_eq!(asks.join().unwrap(), Said::Answer(None));
    }
}
