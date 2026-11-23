//! The session's half of the daemon: `open`, `tell`, the plain commands (AGL-121, D28): split off `daemon/mod.rs`.

use super::*;

use super::flow::Message;

impl Daemon {
    /// A command that is not a turn.
    pub(super) fn plain(&self, client: u64, session: &str, command: Command, lang: Lang) -> Option<Reply> {
        let mut command = Some(command);
        Some(match self.command(client, session, |_| command.take().expect("one command"), false) {
            Ok(()) => Reply::Ok,
            Err(error) => Reply::Refused { error: error.message(lang) },
        })
    }

    /// Sends the session's run the command `make` gives; `busy` marks a
    /// turn until its end.
    pub(super) fn command(
        &self,
        client: u64,
        session: &str,
        make: impl FnOnce(&mut Entry) -> Command,
        busy: bool,
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
        Ok(())
    }

    /// Opens a session's run for `client`, or attaches it to the running
    /// one (D12).
    pub(super) fn open(self: &Arc<Self>, client: u64, lang: Lang, open: proto::Open) -> Result<proto::Opened, Error> {
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
        // A running session is attached to (D12).
        if let Some(key) = open.resume.as_deref() {
            let sessions = self.paths.sessions();
            let id = match key {
                "last" => Session::latest(&sessions, &project.project, 1)
                    .into_iter()
                    .next()
                    .map(|info| info.id),
                key => Some(Session::id_named(&sessions, &project.project, key).unwrap_or_else(|| key.into())),
            };
            let mut state = self.lock();
            if let Some(entry) = id.as_ref().and_then(|id| state.sessions.get_mut(id)) {
                entry.holders.insert(client);
                entry.subscribers.insert(client);
                return Ok(entry.opened.clone());
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
        let asker = self.asker(client, human, open.yes);
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
            stop: Stop::new(),
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
        let reply = proto::Opened {
            session: wire::view(&plane::SessionView::of(&run.session), plane_name.as_deref()),
            resumed,
            function: run.function_name.clone(),
            workspace: run.workspace.to_string_lossy().into_owned(),
            server_url: settings.server_url.clone(),
            context: run.context,
            reasoning: run.reasoning.map(|level| level.name().to_owned()),
            draft: run.autostart.project.unit_key().and_then(|key| {
                Draft::of(
                    &run.autostart.whisper,
                    &run.autostart.models,
                    &key,
                    run.autostart.path_var.clone(),
                )
                .map(|draft| proto::DraftSpec {
                    url: draft.server.url,
                    unit: draft.server.unit,
                    model: draft.server.model,
                    program: draft.server.program,
                    args: draft.server.args,
                })
            }),
        };
        let voice = Arc::new(Voice::of(&run.autostart));
        let id = run.session.id.clone();
        // The FSM of a run of one request writes to the session from now.
        let observer = open.one_request.then(|| {
            let mut observer = observer;
            observer.attach(&run.session);
            observer
        });
        let parts = run.start().into_parts();
        // The clock of D2: a server stops after `idle_stop` seconds
        // without a turn; 0, an external server (AGL-052, D4) has none.
        // The unit to stop is named by the id when the run moved to a
        // server of its own (AGL-120, D3, D5), as `Autostart::unit` does:
        // two servers of one project then stop their own, not each other's.
        let clock = settings
            .server
            .idle_stop
            .filter(|seconds| *seconds > 0 && !settings.server.external())
            .map(|seconds| Clock {
                idle: Duration::from_secs(seconds),
                last: Instant::now(),
                unit: project.unit_key().map(|key| {
                    if reply.server_url == settings.server.url() {
                        unit_name(&key)
                    } else {
                        unit_name_id(&key, &settings.server)
                    }
                }),
                engine: Arc::clone(&engine),
                dir: project.dir.clone(),
            });
        let entry = Entry {
            commands: parts.commands,
            stop: parts.stop,
            opened: reply.clone(),
            holders: HashSet::from([client]),
            subscribers: HashSet::from([client]),
            origins: VecDeque::new(),
            busy: false,
            observer,
            voice,
            engine,
            project_dir: project.dir.clone(),
        };
        let url = reply.server_url.clone();
        let mut state = self.lock();
        state.sessions.insert(id.clone(), entry);
        if let Some(dir) = &plane_dir {
            state.follow(dir, &id, true);
        }
        if let Some(clock) = clock {
            state.clocks.insert(url, clock);
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
        // The url of the server this session runs on, for its idle clock
        // (AGL-120, D2), taken before `entry`'s borrow of `state` ends.
        let server_url = entry.opened.server_url.clone();
        let mut next = id.to_owned();
        let mut origin = None;
        let mut turn_ended = false;
        let message = match item {
            Sent::Done(done) => {
                turn_ended = true;
                origin = entry.origins.pop_front();
                entry.busy = !entry.origins.is_empty();
                let done = *done;
                // `/new`: the entry follows the new session.
                if done.session.id != id {
                    next = done.session.id.clone();
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
            if let Some(clock) = state.clocks.get_mut(&server_url) {
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
        proto::send(&mut client.out, &Request::History { session: id.clone() }).unwrap();
        let events = loop {
            match client.next() {
                Reply::History { session, events } => {
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
    fn a_second_open_of_a_session_attaches_and_the_last_close_ends_it() {
        // D12: one run for the session, held by both clients.
        let dir = TempDir::new();
        let (paths, socket) = daemon(&dir);
        let project = dir.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            paths.config.join("settings.nix"),
            r#"{ agl, ... }: { function = "fixture-minimal"; server.llama = { url = "http://127.0.0.1:9"; external = true; }; }"#,
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
        drop(second);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(first.ask(&Request::Sessions), Reply::Sessions { sessions: Vec::new() });
    }
}
