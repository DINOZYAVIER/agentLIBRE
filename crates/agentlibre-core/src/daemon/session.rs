//! The session's half of the daemon: `open`, `tell`, the plain commands (AGL-121, D28): split off `daemon/mod.rs`.

use super::*;

use super::flow::Message;

impl Daemon {
    /// A command that is not a turn.
    pub(super) fn plain(&self, client: u64, session: &str, command: Command, lang: Lang) -> Option<Reply> {
        self.accept(client, session, |_| command, false, lang)
    }

    /// Sends the run `client`'s command and answers it: `Ok`, before anything
    /// the command does can be told, its end too; a refusal is the reply.
    pub(super) fn accept(
        &self,
        client: u64,
        session: &str,
        make: impl FnOnce(&mut Entry) -> Command,
        busy: bool,
        lang: Lang,
    ) -> Option<Reply> {
        match self.command(client, session, make, busy, true) {
            Ok(()) => None,
            Err(error) => Some(Reply::Refused { error: error.message(lang) }),
        }
    }

    /// Sends the session's run the command `make` gives; `busy` marks a
    /// turn until its end; `answer` says `Ok` to the client under the lock
    /// that `tell` sends under.
    pub(super) fn command(
        &self,
        client: u64,
        session: &str,
        make: impl FnOnce(&mut Entry) -> Command,
        busy: bool,
        answer: bool,
    ) -> Result<(), Error> {
        let mut state = self.lock();
        let entry = state
            .sessions
            .get_mut(session)
            .ok_or_else(|| super::error::Error::NoSession { id: session.into() })?;
        let command = make(entry);
        entry.commands.send(command).map_err(|_| Error::PlaneGone)?;
        entry.busy |= busy;
        entry.origins.push_back(client);
        if answer && let Some(client) = state.clients.get(&client) {
            write(&client.out, &Reply::Ok);
        }
        Ok(())
    }

    /// Loads the session `id` from its file, as `--resume` does, for a client
    /// that follows it and does not hold it (AGL-125, D1); its id.
    pub(super) fn load(self: &Arc<Self>, client: u64, lang: Lang, id: &str) -> Result<String, Error> {
        let session = Session::open(&self.paths.sessions(), id)?;
        let open = proto::Open {
            dir: session.workspace.to_string_lossy().into_owned(),
            resume: Some(session.id.clone()),
            ..proto::Open::default()
        };
        Ok(self.open(client, lang, open, false)?.session.id)
    }

    /// Opens a session's run for `client`, or attaches it to the running
    /// one (D12).
    /// `hold` says whether `client` holds it or only follows it.
    pub(super) fn open(
        self: &Arc<Self>,
        client: u64,
        lang: Lang,
        open: proto::Open,
        hold: bool,
    ) -> Result<proto::Opened, Error> {
        let user = UserFile::load(&self.paths.settings_file())?;
        let cwd = PathBuf::from(&open.dir);
        let auto_rehome = user.forge.auto_rehome.unwrap_or(true);
        let project = forge::project_of(&self.paths, &cwd, auto_rehome, lang, &|options| {
            Err(agentlibre_forge::forge::error::Error::RehomeNoStore {
                stores: options[..options.len().saturating_sub(1)].to_vec(),
            }
            .into())
        })?;
        if open.resume.is_some() && open.function.is_some() {
            return Err(Error::FunctionWithResume);
        }
        // A running session is attached to (D12); one that is not is loaded
        // once, whoever asks for it meanwhile (AGL-125, D1).
        let mut _loading = None;
        if let Some(key) = open.resume.as_deref() {
            let sessions = self.paths.sessions();
            let id = match key {
                "last" => Session::latest(&sessions, &project.project, 1)
                    .into_iter()
                    .next()
                    .map(|info| info.id),
                key => Some(
                    Session::id_named(&sessions, &project.project, key)
                        .or_else(|| Session::full_id(&sessions, key).ok())
                        .unwrap_or_else(|| key.into()),
                ),
            };
            _loading = id.as_deref().map(|id| self.load_alone(id));
            let mut state = self.lock();
            if let Some(entry) = id.as_ref().and_then(|id| state.sessions.get_mut(id)) {
                if hold {
                    entry.holders.insert(client);
                }
                entry.subscribers.insert(client);
                // It was there before this client, whoever made it.
                let opened = proto::Opened {
                    resumed: true,
                    ..entry.opened.clone()
                };
                // A question that waits is the first thing it sees (AGL-125,
                // D7), before the session's own reply: a terminal asks it
                // while it is plain.
                state.show_questions(client, &opened.session.id);
                return Ok(opened);
            }
        }
        let (human, say) = {
            let state = self.lock();
            let human = state.clients.get(&client).is_some_and(|c| c.human);
            (human, Arc::clone(self))
        };
        // The store is made at the first run, as the terminal did.
        if !project.dir.join(forge::MARKER_FILE).is_file() {
            for line in forge::materialize(&project, lang)? {
                say.line(client, &line);
            }
        }
        let stop = Stop::new();
        let asking = Arc::new(Asking {
            client,
            human,
            idle: Duration::from_secs(user.session_idle.unwrap_or(agentlibre_common::settings::SESSION_IDLE)),
            stop: stop.clone(),
            session: Mutex::new(None),
        });
        let asker = self.asker(&asking, open.yes);
        let engine = Arc::new(Mutex::new(None));
        let reasoning = open.reasoning.as_deref().and_then(ReasoningLevel::parse);
        let env = |name: &str| std::env::var_os(name);
        let opened = Run::open(plane::Opening {
            env: &env,
            paths: &self.paths,
            user: &user,
            cwd: &cwd,
            project: &project,
            resume: open.resume.as_deref(),
            function: open.function.as_deref(),
            plane: open.plane.as_deref(),
            reasoning,
            one_request: open.one_request,
            engine: Arc::clone(&engine),
            lang,
            stop,
            asker,
        })?;
        let plane::Opened {
            mut run,
            resumed,
            settings,
            observer,
        } = opened;
        // A plane of the run is held by the daemon from here (AGL-121, D38):
        // its driver, made once, is the one the run's `flow` tool asks (D17).
        let plane = run.file.clone();
        let flows = plane
            .as_ref()
            .map(|file| self.lock().hold(file, Some(run.workspace.clone()), &self.needs));
        run.flows = flows.clone();
        let plane_name = plane.as_ref().map(|file| file.name.clone());
        let plane_dir = plane.as_ref().map(|file| file.dir.clone());
        // The draft server of the dictation is the daemon's (AGL-127, D4):
        // made here once, with the run's `PATH`.
        let draft = run
            .autostart
            .project
            .unit_key()
            .and_then(|key| {
                Draft::of(
                    &run.autostart.whisper,
                    &run.autostart.models,
                    &key,
                    run.autostart.path_var.clone(),
                )
            })
            .map(Arc::new);
        let reply = proto::Opened {
            session: wire::view(&plane::SessionView::of(&run.session), plane_name.as_deref()),
            resumed,
            function: run.function_name.clone(),
            workspace: run.workspace.to_string_lossy().into_owned(),
            server_url: settings.server_url.clone(),
            context: run.context,
            reasoning: run.reasoning.map(|level| level.name().to_owned()),
            drafts: draft.is_some(),
        };
        let voice = Arc::new(Voice::of(&run.autostart));
        let id = run.session.id.clone();
        asking.set_session(&id);
        let past = plane::Past::of(&run.session.events);
        // The FSM of a run of one request writes to the session from now.
        let observer = open.one_request.then(|| {
            let mut observer = observer;
            observer.attach(&run.session);
            observer
        });
        let parts = run.start().into_parts();
        // The clock of D2: a server stops after `idle_stop` seconds
        // without a turn; 0, an external server (AGL-052, D4) has none.
        // The unit to stop is the id's (AGL-120, D6), as `Autostart::unit`
        // names it: two servers of one project stop their own. The clock is
        // the engine's, keyed by that unit: a session that moved to a fresh
        // port in the middle of a turn, and one opened there after it, share
        // the one clock of the one engine (AGL-121, D43).
        let unit = project.unit_key().map(|_| unit_name(&settings.server));
        let clock_key = unit.clone().unwrap_or_else(|| reply.server_url.clone());
        let clock = settings
            .server
            .idle_stop
            .filter(|seconds| *seconds > 0 && !settings.server.external())
            .map(|seconds| Clock {
                idle: Duration::from_secs(seconds),
                last: Instant::now(),
                unit,
                engine: Arc::clone(&engine),
                dir: project.dir.clone(),
            });
        let entry = Entry {
            commands: parts.commands,
            stop: parts.stop,
            opened: reply.clone(),
            holders: if hold { HashSet::from([client]) } else { HashSet::new() },
            subscribers: HashSet::from([client]),
            origins: VecDeque::new(),
            busy: false,
            asking,
            last: Instant::now(),
            past,
            observer,
            voice,
            draft,
            engine,
            project_dir: project.dir.clone(),
            clock: clock_key.clone(),
        };
        let mut state = self.lock();
        state.sessions.insert(id.clone(), entry);
        if let Some(dir) = &plane_dir {
            state.follow(dir, &id, true);
        }
        if let Some(clock) = clock {
            state.clocks.insert(clock_key, clock);
        }
        drop(state);
        // A session of the plane opened: wakes that missed it may land now (D20).
        self.poke(flows.as_ref());
        let daemon = Arc::clone(self);
        let sent = parts.sent;
        thread::spawn(move || {
            let mut id = id;
            while let Ok(item) = sent.recv() {
                id = daemon.tell(&id, item);
            }
            let _ = parts.thread.join();
        });
        Ok(reply)
    }

    /// What a run sent goes to the clients that follow it; a command's
    /// end clears the session's turn and follows a new session's id.
    pub(super) fn tell(&self, id: &str, item: Sent) -> String {
        let mut state = self.lock();
        // The plane this run is on, for the wire's name and its flows (D38).
        let plane = state
            .plane_of(id)
            .map(|plane| (plane.file.dir.clone(), plane.file.name.clone()));
        let plane_name = plane.as_ref().map(|(_, name)| name.clone());
        let Some(entry) = state.sessions.get_mut(id) else {
            return id.to_owned();
        };
        let mut targets: HashSet<u64> = entry.subscribers.clone();
        // The key of its server's idle clock (AGL-120, D2), taken before
        // `entry`'s borrow of `state` ends.
        let clock_key = entry.clock.clone();
        let mut next = id.to_owned();
        let mut origin = None;
        let mut turn_ended = false;
        let sender = entry.origins.front().copied();
        entry.past.take(&item);
        let message = match item {
            Sent::Done(done) => {
                turn_ended = true;
                origin = entry.origins.pop_front();
                entry.busy = !entry.origins.is_empty();
                entry.last = Instant::now();
                let done = *done;
                // `/new`: the entry follows the new session.
                if done.session.id != id {
                    next = done.session.id.clone();
                    entry.asking.set_session(&next);
                    entry.opened.session = wire::view(&done.session, plane_name.as_deref());
                    let entry = state.sessions.remove(id).expect("the entry");
                    state.sessions.insert(next.clone(), entry);
                    // The plane follows the session to its new id (D38).
                    if let Some((dir, _)) = &plane {
                        state.follow(dir, id, false);
                        state.follow(dir, &next, true);
                    }
                } else {
                    entry.opened.session = wire::view(&done.session, plane_name.as_deref());
                }
                Message::Done(Box::new(done))
            }
            sent => match wire::sent(sent) {
                Some(sent) => Message::Sent(sent),
                None => return next,
            },
        };
        targets.extend(state.everyone.iter().copied());
        // The human's message is told to all but the client it came from,
        // which drew it (AGL-126, D11).
        if let (Some(sender), Message::Sent(proto::Sent::Report(proto::Report::Event(proto::Event::User(_))))) =
            (sender, &message)
        {
            targets.remove(&sender);
        }
        for client in targets {
            let Some(to) = state.clients.get(&client) else { continue };
            let reply = match &message {
                Message::Sent(sent) => Reply::Sent {
                    session: id.to_owned(),
                    sent: sent.clone(),
                },
                Message::Done(done) => Reply::Done {
                    session: id.to_owned(),
                    done: wire::done(done, plane_name.as_deref(), to.lang),
                    yours: origin == Some(client),
                },
            };
            write(&to.out, &reply);
        }
        // A turn of the run ended: its flows look again at what they wait
        // for (D18) — a review's verdict asks through the next poke — and
        // the flow whose woken session just turned gets its `turn.end`
        // through the pair the wake left (D22).
        if turn_ended {
            // Any turn on a server rewinds its idle clock (AGL-120, D2).
            if let Some(clock) = state.clocks.get_mut(&clock_key) {
                clock.last = Instant::now();
            }
            // `state` is held here already: collect the due pairs as owned
            // values, and post to the holders without touching it twice.
            let due: Vec<(PathBuf, String)> = state
                .flow_wakes
                .iter()
                .filter(|(_, session)| *session == id || *session == &next)
                .map(|((dir, flow), _)| (dir.clone(), flow.clone()))
                .collect();
            for (dir, flow) in due {
                state.flow_wakes.remove(&(dir.clone(), flow.clone()));
                // The woken turn is over: its gate goes back (D58).
                self.flow_gate.release();
                if let Some(handle) = state.plane(&dir).map(|plane| plane.holder.clone()) {
                    handle.post(plane::flow::holder::Letter::TurnEnd { flow });
                }
            }
            if let Some((dir, _)) = &plane {
                let holder = state.plane(dir).map(|plane| plane.holder.clone());
                self.poke(holder.as_ref());
            }
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::daemon::tests::{Client, daemon, open};
    use agentlibre_common::testutil::TempDir;

    /// A session open in a daemon whose turns go on until they are stopped.
    fn held_turns(dir: &TempDir) -> (PathBuf, PathBuf, Client, String) {
        held_turns_with(dir, "")
    }

    fn held_turns_with(dir: &TempDir, settings: &str) -> (PathBuf, PathBuf, Client, String) {
        let (paths, socket) = daemon(dir);
        let project = dir.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            paths.config.join("settings.nix"),
            format!(
                r#"{{ agl, ... }}: {{ function = "fixture-minimal"; {settings} server.llama = {{ url = "{}"; external = true; }}; }}"#,
                crate::daemon::tests::holding_server()
            ),
        )
        .unwrap();
        let (mut client, _) = Client::new(&socket, proto::VERSION);
        let Reply::Opened(opened) = client.ask(&open(&project, None)) else {
            panic!("not opened")
        };
        (socket, project, client, opened.session.id)
    }

    /// What `open` answers in a daemon whose settings are `settings`, with
    /// the weights `models` in its folder of them.
    fn opened_with(dir: &TempDir, settings: &str, models: &[&str]) -> (Client, PathBuf, proto::Opened) {
        let (paths, socket) = daemon(dir);
        let project = dir.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        // A store of its own gives the project an origin, which names the
        // unit of a draft server (AGL-122, D9).
        let (workspace, _) = agentlibre_forge::forge::resolve(
            &paths.data,
            &project,
            agentlibre_common::i18n::Lang::En,
            true,
            &|_| unreachable!("a fresh project asks nothing"),
        )
        .unwrap();
        agentlibre_forge::forge::materialize(&workspace, agentlibre_common::i18n::Lang::En).unwrap();
        std::fs::create_dir_all(paths.models()).unwrap();
        for model in models {
            std::fs::write(paths.models().join(model), b"weights").unwrap();
        }
        std::fs::write(
            paths.config.join("settings.nix"),
            format!(
                r#"{{ agl, ... }}: {{ function = "fixture-minimal"; {settings} server.llama = {{ url = "{}"; external = true; }}; }}"#,
                crate::daemon::tests::holding_server()
            ),
        )
        .unwrap();
        let (mut client, _) = Client::new(&socket, proto::VERSION);
        let Reply::Opened(opened) = client.ask(&open(&project, None)) else {
            panic!("not opened")
        };
        (client, project, opened)
    }

    fn chunk(dir: &Path, draft: bool) -> Request {
        Request::Transcribe {
            dir: dir.to_string_lossy().into_owned(),
            plane: None,
            audio: proto::Attached {
                number: 1,
                kind: proto::MediaKind::Audio,
                name: "chunk.wav".into(),
                ext: "wav".into(),
                base64: proto::base64::encode(b"RIFF"),
            },
            draft,
        }
    }

    #[test]
    fn the_drafts_are_set_with_a_url_a_model_and_the_file() {
        // AGL-127, D4: `Opened.drafts` is true only with `draft_url`,
        // `draft_model` and the weights there.
        let whisper = r#"whisper = { draft_url = "http://127.0.0.1:9"; draft_model = "draft.bin"; };"#;
        let dir = TempDir::new();
        let (_, _, opened) = opened_with(&dir, whisper, &["draft.bin"]);
        assert!(opened.drafts, "url, model and file");
        let dir = TempDir::new();
        let (_, _, opened) = opened_with(&dir, whisper, &[]);
        assert!(!opened.drafts, "no file");
        let dir = TempDir::new();
        let url = r#"whisper = { draft_url = "http://127.0.0.1:9"; };"#;
        let (_, _, opened) = opened_with(&dir, url, &["draft.bin"]);
        assert!(!opened.drafts, "no model");
        let dir = TempDir::new();
        let model = r#"whisper = { draft_model = "draft.bin"; };"#;
        let (_, _, opened) = opened_with(&dir, model, &["draft.bin"]);
        assert!(!opened.drafts, "no url");
        let dir = TempDir::new();
        let (_, _, opened) = opened_with(&dir, "", &[]);
        assert!(!opened.drafts, "no whisper settings");
    }

    #[test]
    fn a_draft_with_none_set_is_refused() {
        // AGL-127, D4.
        let dir = TempDir::new();
        let (mut client, project, opened) = opened_with(&dir, "", &[]);
        assert!(!opened.drafts);
        let Reply::Refused { error } = client.ask(&chunk(&project, true)) else {
            panic!("a draft was taken")
        };
        assert_eq!(error, "no draft server is set for this session's whisper");
    }

    fn say(session: &str, message: &str) -> Request {
        Request::Say {
            session: session.into(),
            message: message.into(),
            attached: Vec::new(),
            reasoning: None,
        }
    }

    #[test]
    fn history_is_answered_while_a_turn_runs() {
        // AGL-126, D10.
        let dir = TempDir::new();
        let (socket, _, mut first, id) = held_turns(&dir);
        proto::send(&mut first.out, &say(&id, "hello")).unwrap();
        // The turn is in its step: its state is told.
        loop {
            let reply = first.next();
            assert!(!matches!(reply, Reply::Done { .. }), "{reply:?}");
            if let Reply::Sent {
                sent: proto::Sent::Report(proto::Report::Prefill),
                ..
            } = reply
            {
                break;
            }
        }
        let (mut second, _) = Client::new(&socket, proto::VERSION);
        let Reply::History { session, events, .. } = second.ask(&Request::History { session: id.clone() }) else {
            panic!("no history")
        };
        assert_eq!(session, id);
        assert!(events.contains(&proto::Event::User("hello".into())), "{events:?}");
        let Reply::Sessions { sessions } = second.ask(&Request::Sessions) else {
            panic!()
        };
        assert!(sessions[0].busy, "the turn runs");
    }

    /// Reads `client` until the turn of its session is in its step.
    fn until_step(client: &mut Client) {
        loop {
            let reply = client.next();
            assert!(!matches!(reply, Reply::Done { .. }), "{reply:?}");
            if let Reply::Sent {
                sent: proto::Sent::Report(proto::Report::Prefill),
                ..
            } = reply
            {
                return;
            }
        }
    }

    #[test]
    fn a_turn_goes_on_with_nobody_watching() {
        // AGL-125, D1, D2.
        let dir = TempDir::new();
        let (socket, project, mut first, id) = held_turns(&dir);
        proto::send(&mut first.out, &say(&id, "hello")).unwrap();
        until_step(&mut first);
        drop(first);
        thread::sleep(Duration::from_millis(300));
        let (mut second, _) = Client::new(&socket, proto::VERSION);
        let Reply::Sessions { sessions } = second.ask(&Request::Sessions) else {
            panic!()
        };
        assert!(sessions[0].busy, "the turn runs");
        assert!(sessions[0].pids.is_empty(), "nobody holds it");
        // Whoever comes back has it, and may stop it.
        let Reply::Opened(opened) = second.ask(&open(&project, Some(id.clone()))) else {
            panic!("not attached")
        };
        assert!(opened.resumed);
        proto::send(&mut second.out, &Request::Stop { session: id.clone() }).unwrap();
        let done = loop {
            if let Reply::Done { done, .. } = second.next() {
                break done;
            }
        };
        assert!(done.turn.is_some(), "{done:?}");
    }

    #[test]
    fn a_message_to_a_session_that_is_not_loaded_loads_it_once() {
        // AGL-125, D1, D4.
        let dir = TempDir::new();
        let (socket, _, first, id) = held_turns_with(&dir, "session_idle = 1;");
        drop(first);
        let (mut client, _) = Client::new(&socket, proto::VERSION);
        let since = Instant::now();
        loop {
            let Reply::Sessions { sessions } = client.answer(&Request::Sessions) else {
                panic!()
            };
            if sessions.is_empty() {
                break;
            }
            assert!(since.elapsed() < Duration::from_secs(10), "it was never unloaded");
            thread::sleep(Duration::from_millis(50));
        }
        // Two clients at once, one by the start of the id: one run.
        let (mut other, _) = Client::new(&socket, proto::VERSION);
        proto::send(&mut client.out, &say(&id, "one")).unwrap();
        proto::send(&mut other.out, &say(&id[..24], "two")).unwrap();
        until_step(&mut client);
        let Reply::Sessions { sessions } = client.answer(&Request::Sessions) else {
            panic!()
        };
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].busy && sessions[0].pids.is_empty(), "sent, and not held");
        assert!(matches!(
            client.answer(&say("0000", "nothing")),
            Reply::Refused { .. }
        ));
    }

    #[test]
    fn a_message_is_told_to_the_clients_that_did_not_send_it() {
        // AGL-126, D11.
        let dir = TempDir::new();
        let (socket, project, mut first, id) = held_turns(&dir);
        let (mut second, _) = Client::new(&socket, proto::VERSION);
        let Reply::Opened(_) = second.ask(&open(&project, Some(id.clone()))) else {
            panic!("not attached")
        };
        proto::send(&mut first.out, &say(&id, "hello")).unwrap();
        // What each is told until the turn's step begins.
        let users = |client: &mut Client| {
            let mut users = 0;
            loop {
                match client.next() {
                    Reply::Sent {
                        sent: proto::Sent::Report(proto::Report::Event(proto::Event::User(text))),
                        ..
                    } => {
                        assert_eq!(text, "hello");
                        users += 1;
                    }
                    Reply::Sent {
                        sent: proto::Sent::Report(proto::Report::Prefill),
                        ..
                    } => return users,
                    reply => assert!(!matches!(reply, Reply::Done { .. }), "{reply:?}"),
                }
            }
        };
        assert_eq!(users(&mut first), 0, "its own");
        assert_eq!(users(&mut second), 1);
    }

    #[test]
    fn history_gives_a_sessions_past() {
        // AGL-073, D10.
        let dir = TempDir::new();
        let (paths, socket) = daemon(&dir);
        let project = dir.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            paths.config.join("settings.nix"),
            r#"{ agl, ... }: { function = "fixture-minimal"; server.llama = { url = "http://127.0.0.1:9"; external = true; }; }"#,
        )
        .unwrap();
        let (mut client, _) = Client::new(&socket, proto::VERSION);
        let Reply::Opened(opened) = client.ask(&open(&project, None)) else {
            panic!("not opened")
        };
        let id = opened.session.id.clone();
        let line = proto::CommandLine {
            line: "/help".into(),
            failed: false,
            output: None,
        };
        assert_eq!(
            client.ask(&Request::Record {
                session: id.clone(),
                lines: vec![line.clone()],
            }),
            Reply::Ok
        );
        // The past is the session's as it is, not after what waits in it.
        assert!(matches!(client.next(), Reply::Done { .. }));
        proto::send(&mut client.out, &Request::History { session: id.clone() }).unwrap();
        let events = loop {
            match client.next() {
                Reply::History { session, events, .. } => {
                    assert_eq!(session, id);
                    break events;
                }
                // The ends of the commands, told to none here.
                Reply::Done { .. } | Reply::Ok => {}
                other => panic!("not the history: {other:?}"),
            }
        };
        assert!(events.contains(&proto::Event::Command(line)), "{events:?}");
        // The `Done` of the recorded command can arrive while this answer is
        // on its way, so the read skips what no request asked for.
        proto::send(&mut client.out, &Request::History { session: "none".into() }).unwrap();
        let refused = loop {
            match client.next() {
                Reply::Done { .. } | Reply::Ok => {}
                reply => break reply,
            }
        };
        assert!(matches!(refused, Reply::Refused { .. }), "{refused:?}");
    }

    #[test]
    fn a_second_open_of_a_session_attaches_and_its_idle_time_unloads_it() {
        // D12: one run for the session, held by both clients; AGL-125, D6:
        // it is unloaded once nobody has held it for its time.
        let dir = TempDir::new();
        let (paths, socket) = daemon(&dir);
        let project = dir.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            paths.config.join("settings.nix"),
            r#"{ agl, ... }: { function = "fixture-minimal"; session_idle = 1; server.llama = { url = "http://127.0.0.1:9"; external = true; }; }"#,
        )
        .unwrap();
        let (mut first, _) = Client::new(&socket, proto::VERSION);
        let opened = match first.ask(&open(&project, None)) {
            Reply::Opened(opened) => opened,
            other => panic!("not opened: {other:?}"),
        };
        assert!(!opened.resumed);
        let id = opened.session.id.clone();
        let (mut second, _) = Client::new(&socket, proto::VERSION);
        let Reply::Opened(again) = second.ask(&open(&project, Some(id.clone()))) else {
            panic!("not attached")
        };
        assert_eq!(again.session.id, id);
        assert!(again.resumed, "it was there before");
        let Reply::Sessions { sessions } = first.ask(&Request::Sessions) else {
            panic!()
        };
        assert_eq!(sessions.len(), 1, "one run for both");
        // A command of one is told to both.
        assert_eq!(
            first.ask(&Request::Rename {
                session: id.clone(),
                name: "both".into()
            }),
            Reply::Ok
        );
        for client in [&mut first, &mut second] {
            let Reply::Done { session, done, .. } = client.next() else {
                panic!("no end")
            };
            assert_eq!(session, id);
            assert_eq!(done.session.name.as_deref(), Some("both"));
        }
        assert_eq!(first.ask(&Request::Close { session: id.clone() }), Reply::Ok);
        let Reply::Sessions { sessions } = second.ask(&Request::Sessions) else {
            panic!()
        };
        assert_eq!(sessions.len(), 1, "the second still holds it");
        // Attached by the start of its id, too, and no second run is made.
        let (mut third, _) = Client::new(&socket, proto::VERSION);
        let Reply::Opened(same) = third.ask(&open(&project, Some(id[..20].into()))) else {
            panic!("not attached")
        };
        assert_eq!(same.session.id, id);
        drop(third);
        drop(second);
        thread::sleep(Duration::from_millis(300));
        let held = |client: &mut Client| {
            let Reply::Sessions { sessions } = client.answer(&Request::Sessions) else {
                panic!()
            };
            sessions.len()
        };
        assert_eq!(held(&mut first), 1, "nobody holds it, and it lives");
        let since = Instant::now();
        while held(&mut first) > 0 {
            assert!(since.elapsed() < Duration::from_secs(10), "it was never unloaded");
            thread::sleep(Duration::from_millis(50));
        }
        // Its file is its record.
        let Reply::Opened(again) = first.answer(&open(&project, Some(id.clone()))) else {
            panic!("not loaded again")
        };
        assert!(again.resumed);
        assert_eq!(again.session.name.as_deref(), Some("both"));
    }
}
