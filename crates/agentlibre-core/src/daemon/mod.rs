//! The daemon (AGL-076): it holds the runs of the sessions its clients
//! open and serves them on its one socket (D7), in the protocol of
//! `agentlibre-proto` (D2, D8). A run's question goes to the client that
//! opened it when that client has a human, and is refused otherwise (D11);
//! a second `open` of a running session attaches to its run (D12); an
//! engine a run launched is stopped when no run uses it any more (D16).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufReader};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use agentlibre_proto::{self as proto, Ask, Lines, Read, Reply, Request};

use crate::backend::llama::Llama;
use crate::backend::strata::Strata;
use crate::backend::{Backend, Message as ChatMessage, Prompt, ReasoningLevel};
use crate::engine::{Engine, Others, rebind_url, remove_record, systemctl, unit_active, unit_main_pid, unit_name, unit_name_id};
use crate::error::Error;
use crate::forge;
use crate::function::Function;
use crate::grammar::{Expr, Grammar};
use crate::i18n::{Lang, m};
use crate::memory::MemoryAction;
use crate::paths::Paths;
use crate::phase::Observer;
use crate::plane::{self, Command, Run, Sent, wire};
use crate::session::Session;
use crate::settings::{ServerKind, Settings, UserFile};
use crate::stop::Stop;
use crate::tr;
use crate::whisper::{Dictation, Draft, Voice};

use servers::{Clock, SWEEP_POLL};


mod clients;
mod flow;
mod servers;
mod session;

/// The daemon's socket of `runtime`, agentLIBRE's runtime directory (D7):
/// the clients' crate names it, so client and server name the same file
/// (AGL-121, D29).
pub fn socket_path(runtime: &Path) -> PathBuf {
    agentlibre_client::socket_path(runtime)
}

/// Serves the planes on the socket until the process ends.
pub fn serve(paths: Paths) -> Result<(), Error> {
    let runtime = paths.runtime.clone().ok_or(Error::DaemonNoRuntime)?;
    let listener = bind(&runtime)?;
    // The daemon's ear for the wakes its flows wait on (D18, D20).
    let (needs, inbox) = mpsc::channel();
    let daemon = Arc::new(Daemon {
        needs,
        paths,
        state: Mutex::new(State::default()),
    });
    thread::spawn({
        let daemon = Arc::clone(&daemon);
        move || daemon.flow_turns(inbox)
    });
    // The idle clock of D2: a server no turn rewinds is stopped once its
    // `idle_stop` passes, so a loaded model does not sit on the GPU while
    // nobody talks; the next turn asks for a server again as on a cold start.
    thread::spawn({
        let daemon = Arc::clone(&daemon);
        move || loop {
            thread::sleep(SWEEP_POLL);
            daemon.sweep();
        }
    });
    // Every plane that declares a flow gets its driver now (D12, D17): the
    // timers of a flow run with no session open and no model. A plane made
    // after this moment waits for the verb that first asks for its thread.
    for file in plane::PlaneFile::list(&daemon.paths).unwrap_or_default() {
        if plane::flow::FlowFile::load(&file.dir).is_ok_and(|decls| !decls.flows.is_empty()) {
            daemon.plane_open(&file);
        }
    }
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
    /// The daemon's ear for its flows' needs (AGL-118, D18): every holder
    /// of a plane opened here asks its turns through this channel.
    needs: Sender<plane::flow::holder::Need>,
    paths: Paths,
    state: Mutex<State>,
}

/// A plane of the daemon (AGL-121, D38): the human's plane (AGL-077) with
/// the sessions of it that are open and its one flow driver (AGL-118, D17),
/// the same thread the CLI verbs and the agent's tool ask.
struct Plane {
    /// Its store: id, name, directories (AGL-077).
    file: plane::PlaneFile,
    /// The sessions of it that are open here.
    sessions: HashSet<String>,
    /// Its driver, made once; the runs of this plane share it (D17).
    holder: plane::flow::holder::Holder,
}

#[derive(Default)]
struct State {
    next: u64,
    clients: HashMap<u64, Client>,
    /// The clients that follow every session (D3).
    everyone: HashSet<u64>,
    sessions: HashMap<String, Entry>,
    /// The planes the daemon holds, by their store (AGL-121, D38): a plane
    /// with a session open, or with its flows driving, is held here.
    planes: HashMap<PathBuf, Plane>,
    /// The questions that wait for their answer, and their client.
    questions: HashMap<u64, (u64, Sender<Option<String>>)>,
    /// The wake's pair (AGL-118, D22): (plane store, flow) to the session
    /// its wake woke; the end of that turn carries `turn.end` back.
    flow_wakes: HashMap<(PathBuf, String), String>,
    /// The engines of runs that ended, kept while a run uses their
    /// server (D16): its url, the engine, the store of its record.
    engines: Vec<(String, Engine, PathBuf)>,
    /// The servers an idle stop watches (AGL-120, D2), by url: a server
    /// stops after its `idle` without a turn, any turn's end rewinding it.
    clocks: HashMap<String, Clock>,
}

impl State {
    /// Holds `file` as a plane (AGL-121, D38) and gives its driver: made
    /// when the plane is first held, and asking its turns of `needs` (D18).
    /// `workdir` is where its states' commands run; the first to hold the
    /// plane says it, as the registry did before (D17).
    fn hold(
        &mut self,
        file: &plane::PlaneFile,
        workdir: Option<PathBuf>,
        needs: &Sender<plane::flow::holder::Need>,
    ) -> plane::flow::holder::Holder {
        if let Some(plane) = self.planes.get(&file.dir) {
            return plane.holder.clone();
        }
        let workdir = workdir
            .or_else(|| file.directories.first().cloned())
            .unwrap_or_else(|| file.dir.clone());
        let holder = plane::flow::holder::Holder::spawn(
            file.dir.clone(),
            workdir,
            Some(needs.clone()),
        );
        self.planes.insert(
            file.dir.clone(),
            Plane {
                file: file.clone(),
                sessions: HashSet::new(),
                holder: holder.clone(),
            },
        );
        holder
    }

    /// The plane held at `dir` (D38): no driver is made by looking.
    fn plane(&self, dir: &Path) -> Option<&Plane> {
        self.planes.get(dir)
    }

    /// The plane the human names `name` (D38).
    fn plane_named(&self, name: &str) -> Option<&Plane> {
        self.planes.values().find(|plane| plane.file.name == name)
    }

    /// The plane `session` runs on, if any (D38).
    fn plane_of(&self, session: &str) -> Option<&Plane> {
        self.planes
            .values()
            .find(|plane| plane.sessions.contains(session))
    }

    /// The session joins its plane, or leaves it (`/new`, a release, D38).
    fn follow(&mut self, dir: &Path, session: &str, open: bool) {
        if let Some(plane) = self.planes.get_mut(dir) {
            if open {
                plane.sessions.insert(session.to_owned());
            } else {
                plane.sessions.remove(session);
            }
        }
    }
}

/// The idle clock of a server (AGL-120, D2): the seconds of no turn that
/// stop it, the last turn's end, how to stop it (its unit, or the child
struct Client {
    out: Arc<Mutex<UnixStream>>,
    human: bool,
    lang: Lang,
    pid: u32,
}

/// A session's run in the daemon.
struct Entry {
    commands: Sender<Command>,
    stop: Stop,
    opened: proto::Opened,
    /// The clients that opened it (D12) and those that follow it (D3).
    holders: HashSet<u64>,
    subscribers: HashSet<u64>,
    /// Who sent each command that waits for its end, in their order (D12).
    origins: VecDeque<u64>,
    busy: bool,
    /// The FSM that saw the server's start of a run of one request: its
    /// first message goes on from it (AGL-046, D10).
    observer: Option<Observer>,
    /// The transcription of its run (D15).
    voice: Arc<Voice>,
    /// The engine its run launched, if it did (D16), and its record's store.
    engine: Arc<Mutex<Option<Engine>>>,
    project_dir: PathBuf,
}

impl Daemon {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

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

    /// A server's clock is due once its idle passes with no turn on it
    /// A daemon on the stores of `dir`, its user's server at a port where
    /// nothing answers, so that no test reaches a real server.
    pub(super) fn daemon(dir: &TempDir) -> (Paths, PathBuf) {
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

    pub(super) struct Client {
        pub(super) out: UnixStream,
        lines: Lines<BufReader<UnixStream>>,
    }

    impl Client {
        pub(super) fn new(socket: &Path, version: u32) -> (Client, Reply) {
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

        pub(super) fn ask(&mut self, request: &Request) -> Reply {
            proto::send(&mut self.out, request).unwrap();
            self.next()
        }

        /// The next reply that is not a line told while a run opens.
        pub(super) fn next(&mut self) -> Reply {
            loop {
                match self.lines.read::<Reply>().unwrap() {
                    Read::Message(Reply::Sent { session, .. }) if session.is_empty() => continue,
                    Read::Message(reply) => return reply,
                    Read::End => panic!("the daemon went"),
                    Read::TooLong => continue,
                }
            }
        }
    }

    pub(super) fn open(dir: &Path, resume: Option<String>) -> Request {
        Request::Open(proto::Open {
            dir: dir.to_string_lossy().into_owned(),
            resume,
            function: None,
            ..proto::Open::default()
        })
    }

}
