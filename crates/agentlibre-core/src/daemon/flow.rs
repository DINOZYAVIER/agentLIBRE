//! The flow's half of the daemon: the wakes, the review's turns, the poke (AGL-121, D28): split off `daemon/mod.rs`.

use super::*;

impl Daemon {
    /// The daemon's holder of the plane `name` (AGL-118, D17): its store
    /// gives the journals, its first directory the place a state's commands
    /// run — where its builds are (D9).
    pub(super) fn flow_handle(&self, name: &str) -> Result<plane::flow::holder::Handle, Error> {
        let file = plane::PlaneFile::open(&self.paths, name)?;
        let workdir = file
            .directories
            .first()
            .cloned()
            .unwrap_or_else(|| file.dir.clone());
        Ok(self.flows.holder(&file.dir, &workdir))
    }

    /// The daemon's arm for the turns its flows wait on (D18): wakes go to
    /// open sessions, and every ask gets its word back. Review turns (D19)
    /// join here when the daemon runs one.
    pub(super) fn flow_turns(&self, inbox: mpsc::Receiver<plane::flow::holder::Need>) {
        use plane::flow::holder::{self, Letter, Need};
        for need in inbox {
            match need {
                Need::Wake { plane_dir, wake } => {
                    let delivered = self.deliver_wake(&plane_dir, &wake);
                    if let Some(handle) = self.flows.peek(&plane_dir) {
                        handle.post(if delivered {
                            Letter::Delivered {
                                flow: wake.flow,
                                session: wake.session,
                                text: wake.text,
                            }
                        } else {
                            Letter::Miss {
                                flow: wake.flow,
                                kind: holder::SAY,
                            }
                        });
                    }
                }
                Need::Review { plane_dir, review } => {
                    let verdict = self.run_review(&plane_dir, &review);
                    if let Some(handle) = self.flows.peek(&plane_dir) {
                        handle.post(match verdict {
                            Ok((answer, session)) => Letter::Verdict {
                                flow: review.flow,
                                event: crate::machine::Event::Answer {
                                    check: holder::REVIEW.into(),
                                    answer,
                                },
                                note: Some(format!("session {session}")),
                            },
                            Err(_) => Letter::Miss {
                                flow: review.flow,
                                kind: holder::REVIEW,
                            },
                        });
                    }
                }
            }
        }
    }

    /// The wake of D18: of the plane's open sessions, the one its name names
    /// (AGL-062: `last`, a name, a start of an id); that session's queue
    /// takes the line as a human's `say` does. None answers — no delivery,
    /// and the flow stands (D20).
    pub(super) fn deliver_wake(&self, plane_dir: &Path, wake: &plane::flow::host::Wake) -> bool {
        let mut state = self.lock();
        let mut dirs: HashMap<String, PathBuf> = HashMap::new();
        let mut ids: Vec<String> = state
            .sessions
            .iter()
            .filter(|(_, entry)| {
                let Some(name) = entry.plane.as_deref() else {
                    return false;
                };
                let dir = dirs.entry(name.to_owned()).or_insert_with(|| {
                    plane::PlaneFile::open(&self.paths, name)
                        .map(|file| file.dir)
                        .unwrap_or_default()
                });
                dir == plane_dir
            })
            .filter(|(id, entry)| match wake.session.as_str() {
                "last" => true,
                key => {
                    entry.opened.session.name.as_deref() == Some(key) || id.starts_with(key)
                }
            })
            .map(|(id, _)| id.clone())
            .collect();
        // `last` of the open ones: session ids grow with time (AGL-062).
        ids.sort();
        let Some(id) = ids.pop() else { return false };
        let Some(entry) = state.sessions.get_mut(&id) else {
            return false;
        };
        let command = Command::Say {
            message: wake.text.clone(),
            attached: Vec::new(),
            reasoning: None,
            observer: entry.observer.take(),
        };
        if entry.commands.send(command).is_ok() {
            entry.busy = true;
            // The pair of D22: the end of this session's turn is the flow's
            // `turn.end`, no other's.
            state
                .flow_wakes
                .insert((plane_dir.to_path_buf(), wake.flow.clone()), id);
            return true;
        }
        false
    }

    /// A review turn as D19 gives it: its own session of the plane under
    /// the reviewer's Function, one `one_of`-constrained turn, the file
    /// kept. No engine, no verdict — the miss says so and the state stands
    /// until a poke asks again (D12).
    pub(super) fn run_review(
        &self,
        plane_dir: &Path,
        review: &plane::flow::host::Review,
    ) -> Result<(String, String), Error> {
        let user = UserFile::load(&self.paths.settings_file())?;
        let file = plane::PlaneFile::list(&self.paths)?
            .into_iter()
            .find(|found| found.dir == plane_dir)
            .ok_or_else(|| Error::PlaneNotFound {
                name: plane_dir.display().to_string(),
            })?;
        let plane_settings = file.settings()?;
        let plane_functions = file.functions_dir();
        let user_functions = self.paths.functions();
        let function = Function::load(&[plane_functions.as_path(), user_functions.as_path()], &review.function)?;
        let settings = Settings::resolve(
            &user,
            &self.paths.settings_file(),
            (&file.settings_file(), &plane_settings),
            None,
            &function.model,
        )?;
        let backend: Arc<dyn Backend + Send + Sync> =
            // A server of the reviewer's id already up elsewhere moves this
            // one-turn review to it (AGL-120, D5), as a session's open does.
            match rebind_url(self.paths.runtime.as_deref(), &settings.server) {
                Some(found) if settings.server.kind() == ServerKind::Strata => {
                    Arc::new(Strata::with_ca(&found, settings.server.private_ca())?)
                }
                Some(found) => Arc::new(Llama::with_ca(&found, settings.server.private_ca())?),
                None if settings.server.kind() == ServerKind::Strata => Arc::new(Strata::with_ca(
                    &settings.server_url,
                    settings.server.private_ca(),
                )?),
                None => Arc::new(Llama::with_ca(&settings.server_url, settings.server.private_ca())?),
            };
        let workspace = file
            .directories
            .first()
            .cloned()
            .unwrap_or_else(|| file.dir.clone());
        let mut session = Session::create_run(
            &self.paths.sessions(),
            &review.function,
            &workspace,
            Some(&function.snapshot()),
        )?;
        let text = format!(
            "flow `{}` stands in `{}`; answer with exactly one of: {}",
            review.flow,
            review.state,
            review.events.join(" ")
        );
        session.append(crate::session::Event::User(text))?;
        let answer = review_answer(&*backend, &function, &review.events)?;
        session.append(crate::session::Event::Answer(answer.clone()))?;
        Ok((answer, session.id))
    }

    /// A word to the plane's flows that something of theirs may have moved
    /// (D18): only a plane whose driver already runs is poked.
    pub(super) fn poke(&self, plane: Option<&str>) {
        use plane::flow::holder::Letter;
        let Some(name) = plane else { return };
        let Ok(file) = plane::PlaneFile::open(&self.paths, name) else {
            return;
        };
        if let Some(handle) = self.flows.peek(&file.dir) {
            handle.post(Letter::Poke);
        }
    }
}

/// What a plane sent, before it is put in each client's language.
pub(super) enum Message {
    Sent(proto::Sent),
    Done(Box<plane::Done>),
}

/// One `one_of` turn (D5, D19): the reviewer's system, the flow's line, and
/// a grammar that allows only the declared event names. An answer outside
/// them is no verdict — the machine's names are law — and fails to a miss.
fn review_answer(
    backend: &dyn Backend,
    function: &Function,
    events: &[String],
) -> Result<String, Error> {
    let context = backend.context_size().unwrap_or(8192);
    let grammar = Grammar::new();
    let root = Expr::one_of(events);
    let grammar_str = grammar.render(&root);
    let messages = vec![
        ChatMessage::System(function.system_at(context)),
        ChatMessage::User(format!(
            "answer with exactly one of: {}",
            events.join(" ")
        )),
    ];
    let structured = Prompt {
        messages: &messages,
        instruction: None,
        tools: &[],
        preserve_thinking: false,
        reasoning: Some(ReasoningLevel::Off),
        format: &function.format,
    };
    let prompt = function.format.start_step(backend.render(&structured)?, false);
    let generated = backend.generate(
        &crate::backend::Request {
            prompt: &prompt,
            grammar: Some(&grammar_str),
            stop: &[],
            max_tokens: 32,
            sampling: &function.sampling,
            media: &[],
            structured: backend.stirred().then_some(&structured),
        },
        &mut |_| {},
        &mut |_| {},
    )?;
    let answer = generated.text.trim().to_owned();
    if events.iter().any(|event| event == &answer) {
        Ok(answer)
    } else {
        Err(Error::PlaneFile {
            path: function.path.clone(),
            detail: format!("`{answer}` is no event of the review"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::daemon::tests::{Client, daemon};
    use crate::forge::testutil::TempDir;

    #[test]
    fn a_flow_verb_turns_with_no_session_open() {
        // AGL-118, D12, D17: `agl plane flow` over the socket; the daemon's
        // holder turns the flow with no session open and no model on.
        let dir = TempDir::new();
        let (paths, socket) = daemon(&dir);
        let project = dir.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let plane = plane::PlaneFile::create(&paths, "work", std::slice::from_ref(&project)).unwrap();
        std::fs::write(
            plane.dir.join("flow.nix"),
            r#"{ agl, ... }: {
  flow.one = {
    initial = "build";
    state.build = {
      action.command = { cmd = "true"; timeout = 10; };
      next.exit.ok = "done";
      next.exit.any = "broken";
    };
    state.done = { };
    state.broken = { };
  };
}
"#,
        )
        .unwrap();
        let (mut client, _) = Client::new(&socket, proto::VERSION);
        let flow = |client: &mut Client, verb: proto::FlowVerb| {
            client.ask(&Request::Flow { plane: "work".into(), verb })
        };
        // The holder of the plane was made when the verb asked, not before.
        let Reply::Said { plane, said } = flow(&mut client, proto::FlowVerb::List) else {
            panic!("no flow answer")
        };
        assert_eq!(plane, "work");
        assert_eq!(said.lines, [proto::Told::Output("one: not started".into())]);
        // The verb runs there: the state's command runs, its exit moves.
        let Reply::Said { said, .. } = flow(&mut client, proto::FlowVerb::Start { name: "one".into() })
        else {
            panic!("no flow answer")
        };
        let text = said
            .lines
            .iter()
            .map(|told| match told {
                proto::Told::Output(text)
                | proto::Told::Notice(text)
                | proto::Told::Refusal(text)
                | proto::Told::Fault(text)
                | proto::Told::Pending(text) => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text, "one: done\n  started build\n  exit.ok -> done");
        // A flow the plane does not declare is a failed answer, not a
        // refusal; a plane that is not there is a refusal.
        let Reply::Said { said, .. } = flow(&mut client, proto::FlowVerb::Start { name: "two".into() })
        else {
            panic!("no flow answer")
        };
        assert!(said.failed && format!("{:?}", said.lines).contains("no flow `two`"), "{said:?}");
        assert!(matches!(
            client.ask(&Request::Flow { plane: "nowhere".into(), verb: proto::FlowVerb::List }),
            Reply::Refused { .. }
        ));
    }

    /// A server that answers one fixed word, for the review turn's law (D5).
    struct Echo(&'static str);

    impl crate::backend::Backend for Echo {
        fn render(&self, _prompt: &crate::backend::Prompt) -> Result<String, Error> {
            Ok("prompt".into())
        }
        fn generate(
            &self,
            _request: &crate::backend::Request,
            _on_thinking: &mut dyn FnMut(&str),
            _on_text: &mut dyn FnMut(&str),
        ) -> Result<crate::backend::Generated, Error> {
            Ok(crate::backend::Generated {
                text: self.0.into(),
                stop: crate::backend::Stop::End,
                thinking: None,
                tool_calls: Vec::new(),
                prompt_tokens: None,
                completion_tokens: None,
                prefill: None,
            })
        }
        fn count_tokens(&self, text: &str) -> Result<usize, Error> {
            Ok(text.len())
        }
        fn context_size(&self) -> Result<usize, Error> {
            Ok(16384)
        }
        fn health(&self) -> Option<u16> {
            Some(200)
        }
        fn url(&self) -> &str {
            "http://127.0.0.1:1"
        }
    }

    #[test]
    fn a_review_answer_outside_the_names_is_no_verdict() {
        // D5, D19: the machine's names are law; prose is not a verdict.
        use crate::function::Function;
        let function = Function::load(&[Path::new("/none")], "fixture-minimal").unwrap();
        let events = ["ok".to_string(), "broken".to_string()];
        assert_eq!(review_answer(&Echo("ok"), &function, &events).unwrap(), "ok");
        assert!(review_answer(&Echo("maybe"), &function, &events).is_err());
    }

    #[test]
    fn a_flows_wake_reaches_an_open_session_of_its_plane() {
        // AGL-118, D18, D20: a flow standing at a `say` asks the daemon,
        // and the daemon carries its line to an open session of the plane;
        // the journal keeps the `said` line the delivery wrote.
        let dir = TempDir::new();
        let (paths, socket) = daemon(&dir);
        let project = dir.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            paths.config.join("settings.nix"),
            r#"{ agl, ... }: { function = "fixture-minimal"; server.llama = { url = "http://127.0.0.1:9"; external = true; }; }"#,
        )
        .unwrap();
        let plane = plane::PlaneFile::create(&paths, "work", std::slice::from_ref(&project)).unwrap();
        std::fs::write(
            plane.dir.join("flow.nix"),
            r#"{ agl, ... }: {
  flow.talk = {
    initial = "stood";
    state.stood = { action.say = "last"; next.message.done = "stood"; };
  };
}
"#,
        )
        .unwrap();
        // A session of the plane opens, and the flow starts into its wake.
        let (mut client, _) = Client::new(&socket, proto::VERSION);
        let Reply::Opened(_) = client.ask(&Request::Open(proto::Open {
            dir: project.display().to_string(),
            plane: Some("work".into()),
            ..Default::default()
        })) else {
            panic!("not opened");
        };
        let Reply::Said { said, .. } = client.ask(&Request::Flow {
            plane: "work".into(),
            verb: proto::FlowVerb::Start { name: "talk".into() },
        }) else {
            panic!("no flow answer");
        };
        assert!(
            said.lines.iter().any(|line| matches!(
                line,
                proto::Told::Output(text) if text.contains("stood")
            )),
            "no place: {:?}",
            said.lines
        );
        // The wake lands: the holder wrote the journal's `said` line.
        let file = plane.dir.join("flow").join("talk.jsonl");
        let since = std::time::Instant::now();
        loop {
            if std::fs::read_to_string(&file).is_ok_and(|text| text.contains("\"type\":\"said\"")) {
                break;
            }
            assert!(since.elapsed() < std::time::Duration::from_secs(10), "the wake never landed");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn a_wakes_turn_pairs_its_end_to_the_flow() {
        // AGL-118, D22: the daemon remembers the session a wake woke, and
        // that turn's end — no other — carries `turn.end` to the flow.
        let dir = TempDir::new();
        let (paths, socket) = daemon(&dir);
        let project = dir.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            paths.config.join("settings.nix"),
            r#"{ agl, ... }: { function = "fixture-minimal"; server.llama = { url = "http://127.0.0.1:9"; external = true; }; }"#,
        )
        .unwrap();
        let plane = plane::PlaneFile::create(&paths, "work", std::slice::from_ref(&project)).unwrap();
        std::fs::write(
            plane.dir.join("flow.nix"),
            r#"{ agl, ... }: {
  flow.talk = {
    initial = "stood";
    state.stood = { action.say = "last"; next.turn.end = "gone"; };
    state.gone = { };
  };
}
"#,
        )
        .unwrap();
        let (mut client, _) = Client::new(&socket, proto::VERSION);
        let Reply::Opened(_) = client.ask(&Request::Open(proto::Open {
            dir: project.display().to_string(),
            plane: Some("work".into()),
            ..Default::default()
        })) else {
            panic!("not opened");
        };
        let Reply::Said { .. } = client.ask(&Request::Flow {
            plane: "work".into(),
            verb: proto::FlowVerb::Start { name: "talk".into() },
        }) else {
            panic!("no flow answer");
        };
        // The session turns on the dead url, ends, and the pair fires:
        // the journal keeps `turn.end -> gone`.
        let file = plane.dir.join("flow").join("talk.jsonl");
        let since = std::time::Instant::now();
        loop {
            if std::fs::read_to_string(&file).is_ok_and(|text| text.contains("\"name\":\"turn\"")) {
                break;
            }
            assert!(since.elapsed() < std::time::Duration::from_secs(10), "the pair never fired");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
