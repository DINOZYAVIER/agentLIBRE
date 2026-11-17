//! The daemon (AGL-076): it holds the planes of the sessions its clients
//! open and serves them on its one socket (D7), in the protocol of
//! `agentlibre-proto` (D2, D8). A plane's question goes to the client that
//! opened it when that client has a human, and is refused otherwise (D11);
//! a second `open` of a running session attaches to its plane (D12); an
//! engine a plane launched is stopped when no plane uses it any more (D16).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufReader};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use agentlibre_proto::{self as proto, Ask, Lines, Reply, Request};

use crate::backend::ReasoningLevel;
use crate::engine::{Engine, Others};
use crate::error::Error;
use crate::forge;
use crate::i18n::{Lang, m};
use crate::memory::MemoryAction;
use crate::paths::Paths;
use crate::phase::Observer;
use crate::plane::{self, Command, Plane, Sent, wire};
use crate::session::Session;
use crate::settings::UserFile;
use crate::stop::Stop;
use crate::tr;
use crate::whisper::{Dictation, Voice};

/// The daemon's socket of `runtime`, agentLIBRE's runtime directory (D7).
pub fn socket_path(runtime: &Path) -> PathBuf {
    runtime.join(proto::SOCKET)
}

/// Serves the planes on the socket until the process ends.
pub fn serve(paths: Paths) -> Result<(), Error> {
    let runtime = paths.runtime.clone().ok_or(Error::DaemonNoRuntime)?;
    let listener = bind(&runtime)?;
    let daemon = Arc::new(Daemon {
        paths,
        state: Mutex::new(State::default()),
    });
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let daemon = Arc::clone(&daemon);
        thread::spawn(move || daemon.client(stream));
    }
    Ok(())
}

/// The socket, 0600 in a 0700 directory; a socket that no daemon answers
/// is taken over, one that answers is another daemon's.
fn bind(runtime: &Path) -> Result<UnixListener, Error> {
    let fail = |path: &Path, source: io::Error| Error::Daemon {
        detail: format!("{}: {source}", path.display()),
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(runtime)
        .map_err(|source| fail(runtime, source))?;
    let path = socket_path(runtime);
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Err(Error::DaemonRunning { path });
        }
        std::fs::remove_file(&path).map_err(|source| fail(&path, source))?;
    }
    let listener = UnixListener::bind(&path).map_err(|source| fail(&path, source))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|source| fail(&path, source))?;
    Ok(listener)
}

struct Daemon {
    paths: Paths,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    next: u64,
    clients: HashMap<u64, Client>,
    /// The clients that follow every session (D3).
    everyone: HashSet<u64>,
    sessions: HashMap<String, Entry>,
    /// The questions that wait for their answer, and their client.
    questions: HashMap<u64, (u64, Sender<Option<String>>)>,
    /// The engines of planes that ended, kept while a plane uses their
    /// server (D16): its url, the engine, the store of its record.
    engines: Vec<(String, Engine, PathBuf)>,
}

struct Client {
    out: Arc<Mutex<UnixStream>>,
    human: bool,
    lang: Lang,
    pid: u32,
}

/// A session's plane in the daemon.
struct Entry {
    commands: Sender<Command>,
    stop: Stop,
    opened: proto::Opened,
    plane: Option<String>,
    /// The clients that opened it (D12) and those that follow it (D3).
    holders: HashSet<u64>,
    subscribers: HashSet<u64>,
    /// Who sent each command that waits for its end, in their order (D12).
    origins: VecDeque<u64>,
    busy: bool,
    /// The FSM that saw the server's start of a run of one request: its
    /// first message goes on from it (AGL-046, D10).
    observer: Option<Observer>,
    /// The transcription of its plane (D15).
    voice: Arc<Voice>,
    /// The engine its plane launched, if it did (D16), and its record's store.
    engine: Arc<Mutex<Option<Engine>>>,
    project_dir: PathBuf,
}

impl Daemon {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A client's connection: its `hello`, then its requests.
    fn client(self: Arc<Self>, stream: UnixStream) {
        let Ok(out) = stream.try_clone() else { return };
        let out = Arc::new(Mutex::new(out));
        let mut lines = Lines::new(BufReader::new(stream));
        let (human, lang, pid) = match lines.read::<Request>() {
            Ok(Some(Request::Hello {
                version,
                human,
                lang,
                pid,
            })) if version == proto::VERSION => (human, Lang::from_code(&lang).unwrap_or(Lang::En), pid),
            Ok(Some(Request::Hello { version, lang, .. })) => {
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
        while let Ok(Some(request)) = lines.read::<Request>() {
            if let Some(reply) = Arc::clone(&self).request(id, lang, request) {
                write(&out, &reply);
            }
        }
        self.gone(id);
    }

    /// A request's reply; `None` when another thread sends it (`open`,
    /// `transcribe`).
    fn request(self: Arc<Self>, client: u64, lang: Lang, request: Request) -> Option<Reply> {
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
        }
    }

    /// A command that is not a turn.
    fn plain(&self, client: u64, session: &str, command: Command, lang: Lang) -> Option<Reply> {
        let mut command = Some(command);
        Some(match self.command(client, session, |_| command.take().expect("one command"), false) {
            Ok(()) => Reply::Ok,
            Err(error) => Reply::Refused { error: error.message(lang) },
        })
    }

    /// Sends the session's plane the command `make` gives; `busy` marks a
    /// turn until its end.
    fn command(
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
            .ok_or_else(|| Error::DaemonNoSession { id: session.into() })?;
        let command = make(entry);
        entry.commands.send(command).map_err(|_| Error::PlaneGone)?;
        entry.busy |= busy;
        entry.origins.push_back(client);
        Ok(())
    }

    /// Opens a session's plane for `client`, or attaches it to the running
    /// one (D12).
    fn open(self: &Arc<Self>, client: u64, lang: Lang, open: proto::Open) -> Result<proto::Opened, Error> {
        let user = UserFile::load(&self.paths.settings_file())?;
        let cwd = PathBuf::from(&open.dir);
        let auto_rehome = user.forge.auto_rehome.unwrap_or(true);
        let project = forge::project_of(&self.paths, &cwd, auto_rehome, lang, &|options| {
            Err(Error::RehomeNoStore {
                stores: options[..options.len().saturating_sub(1)].to_vec(),
            })
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
        let opened = Plane::open(plane::Opening {
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
            plane,
            resumed,
            settings,
            observer,
        } = opened;
        let plane_name = plane.file.as_ref().map(|file| file.name.clone());
        let reply = proto::Opened {
            session: wire::view(&plane::SessionView::of(&plane.session), plane_name.as_deref()),
            resumed,
            function: plane.function_name.clone(),
            workspace: plane.workspace.to_string_lossy().into_owned(),
            server_url: settings.server_url.clone(),
            context: plane.context,
            reasoning: plane.reasoning.map(|level| level.name().to_owned()),
        };
        let voice = Arc::new(Voice::of(&plane.autostart));
        let id = plane.session.id.clone();
        // The FSM of a run of one request writes to the session from now.
        let observer = open.one_request.then(|| {
            let mut observer = observer;
            observer.attach(&plane.session);
            observer
        });
        let parts = plane.start().into_parts();
        let entry = Entry {
            commands: parts.commands,
            stop: parts.stop,
            opened: reply.clone(),
            plane: plane_name,
            holders: HashSet::from([client]),
            subscribers: HashSet::from([client]),
            origins: VecDeque::new(),
            busy: false,
            observer,
            voice,
            engine,
            project_dir: project.dir.clone(),
        };
        self.lock().sessions.insert(id.clone(), entry);
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

    /// What a plane sent goes to the clients that follow it; a command's
    /// end clears the session's turn and follows a new session's id.
    fn tell(&self, id: &str, item: Sent) -> String {
        let mut state = self.lock();
        let Some(entry) = state.sessions.get_mut(id) else {
            return id.to_owned();
        };
        let mut targets: HashSet<u64> = entry.subscribers.clone();
        let plane = entry.plane.clone();
        let mut next = id.to_owned();
        let mut origin = None;
        let message = match item {
            Sent::Done(done) => {
                origin = entry.origins.pop_front();
                entry.busy = !entry.origins.is_empty();
                let done = *done;
                // `/new`: the entry follows the new session.
                if done.session.id != id {
                    next = done.session.id.clone();
                    entry.opened.session = wire::view(&done.session, plane.as_deref());
                    let entry = state.sessions.remove(id).expect("the entry");
                    state.sessions.insert(next.clone(), entry);
                } else {
                    entry.opened.session = wire::view(&done.session, plane.as_deref());
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
                    done: wire::done(done, plane.as_deref(), to.lang),
                    yours: origin == Some(client),
                },
            };
            write(&to.out, &reply);
        }
        next
    }

    /// Who answers a plane's questions (D11): the client that opened it
    /// when it has a human; the refusal otherwise, unless it said `yes`.
    fn asker(self: &Arc<Self>, client: u64, human: bool, yes: bool) -> plane::Asker {
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
    fn line(&self, client: u64, line: &str) {
        self.send_to(
            client,
            &Reply::Sent {
                session: String::new(),
                sent: proto::Sent::Line(line.to_owned()),
            },
        );
    }

    /// A question to `client`, and its answer; `None` when it is gone.
    fn ask(&self, client: u64, ask: Ask, text: &str) -> Option<String> {
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

    /// An audio's text (D15), by the transcription of the plane of a session
    /// open on `dir` in `plane`.
    fn transcribe(&self, dir: &str, plane: Option<&str>, audio: proto::Attached) -> Result<String, Error> {
        let voice = {
            let state = self.lock();
            let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| PathBuf::from(dir));
            state
                .sessions
                .values()
                .find(|entry| {
                    entry.plane.as_deref() == plane
                        && std::fs::canonicalize(&entry.opened.workspace)
                            .is_ok_and(|workspace| workspace == dir)
                })
                .map(|entry| Arc::clone(&entry.voice))
                .ok_or_else(|| Error::DaemonNoTranscriber { dir: dir.clone() })?
        };
        let source = wire::attached_of(audio).ok_or(Error::DaemonBadAudio)?.source;
        let wav = if source.ext.eq_ignore_ascii_case("wav") {
            source.bytes
        } else {
            crate::media::audio_to_wav(&source.bytes)?
        };
        voice.warm_up(&mut |_| {})?;
        voice.transcribe(&wav, &mut |_| {})
    }

    /// `client` no longer holds `session`, or anything when it is gone.
    fn release(&self, client: u64, session: Option<&str>) {
        let mut state = self.lock();
        let ids: Vec<String> = match session {
            Some(id) => vec![id.to_owned()],
            None => state.sessions.keys().cloned().collect(),
        };
        for id in ids {
            let Some(entry) = state.sessions.get_mut(&id) else { continue };
            entry.holders.remove(&client);
            entry.subscribers.remove(&client);
            if entry.holders.is_empty() {
                // Its commands dropped, the plane's thread ends once its
                // command is done; its turn is stopped first.
                let entry = state.sessions.remove(&id).expect("the entry");
                entry.stop.request();
                let url = entry.opened.server_url.clone();
                if let Some(engine) = entry.engine.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    state.engines.push((url, engine, entry.project_dir.clone()));
                }
            }
        }
        // An engine no plane uses any more is stopped (D16).
        let used: HashSet<String> = state
            .sessions
            .values()
            .map(|entry| entry.opened.server_url.clone())
            .collect();
        let runtime = self.paths.runtime.clone();
        let (keep, stop): (Vec<_>, Vec<_>) =
            std::mem::take(&mut state.engines).into_iter().partition(|(url, _, _)| used.contains(url));
        state.engines = keep;
        drop(state);
        for (_, engine, dir) in stop {
            let pid = engine.pid();
            engine.shutdown();
            crate::engine::remove_record(&dir, runtime.as_deref(), Some(pid));
        }
    }

    /// A client gone: its sessions released, its questions unanswered.
    fn gone(&self, client: u64) {
        {
            let mut state = self.lock();
            state.clients.remove(&client);
            state.everyone.remove(&client);
            state.questions.retain(|_, (asker, _)| *asker != client);
        }
        self.release(client, None);
    }

    fn send_to(&self, client: u64, reply: &Reply) {
        let out = self.lock().clients.get(&client).map(|c| Arc::clone(&c.out));
        if let Some(out) = out {
            write(&out, reply);
        }
    }
}

/// What a plane sent, before it is put in each client's language.
enum Message {
    Sent(proto::Sent),
    Done(Box<plane::Done>),
}

fn write(out: &Mutex<UnixStream>, reply: &Reply) {
    let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
    let _ = proto::send(&mut *out, reply);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::forge::testutil::TempDir;

    /// A daemon on the stores of `dir`, its user's server at a port where
    /// nothing answers, so that no test reaches a real server.
    fn daemon(dir: &TempDir) -> (Paths, PathBuf) {
        let paths = Paths {
            config: dir.0.join("config"),
            data: dir.0.join("data"),
            state: dir.0.join("state"),
            runtime: Some(dir.0.join("run")),
        };
        std::fs::create_dir_all(&paths.config).unwrap();
        std::fs::write(
            paths.settings_file(),
            r#"{ agl, ... }: { server.llama.url = "http://127.0.0.1:9"; }"#,
        )
        .unwrap();
        let socket = socket_path(paths.runtime.as_deref().unwrap());
        let served = paths.clone();
        thread::spawn(move || serve(served));
        for _ in 0..200 {
            if UnixStream::connect(&socket).is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        (paths, socket)
    }

    struct Client {
        out: UnixStream,
        lines: Lines<BufReader<UnixStream>>,
    }

    impl Client {
        fn new(socket: &Path, version: u32) -> (Client, Reply) {
            let out = UnixStream::connect(socket).unwrap();
            out.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
            let mut client = Client {
                lines: Lines::new(BufReader::new(out.try_clone().unwrap())),
                out,
            };
            let hello = client.ask(&Request::Hello {
                version,
                human: false,
                lang: "en".into(),
                pid: std::process::id(),
            });
            (client, hello)
        }

        fn ask(&mut self, request: &Request) -> Reply {
            proto::send(&mut self.out, request).unwrap();
            self.next()
        }

        /// The next reply that is not a line told while a plane opens.
        fn next(&mut self) -> Reply {
            loop {
                match self.lines.read::<Reply>().unwrap().unwrap() {
                    Reply::Sent { session, .. } if session.is_empty() => continue,
                    reply => return reply,
                }
            }
        }
    }

    fn open(dir: &Path, resume: Option<String>) -> Request {
        Request::Open(proto::Open {
            dir: dir.to_string_lossy().into_owned(),
            resume,
            function: None,
            ..proto::Open::default()
        })
    }

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
        // D12: one plane for the session, held by both clients.
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
        assert_eq!(sessions.len(), 1, "one plane for both");
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
