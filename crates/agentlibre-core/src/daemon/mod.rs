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
use std::time::{Duration, Instant};

use agentlibre_proto::{self as proto, Ask, Lines, Reply, Request};

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
use crate::plane::{self, Command, Plane, Sent, wire};
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

/// The daemon's socket of `runtime`, agentLIBRE's runtime directory (D7).
pub fn socket_path(runtime: &Path) -> PathBuf {
    runtime.join(proto::SOCKET)
}

/// Serves the planes on the socket until the process ends.
pub fn serve(paths: Paths) -> Result<(), Error> {
    let runtime = paths.runtime.clone().ok_or(Error::DaemonNoRuntime)?;
    let listener = bind(&runtime)?;
    // The daemon's ear for the wakes its flows wait on (D18, D20).
    let (needs, inbox) = mpsc::channel();
    let daemon = Arc::new(Daemon {
        flows: Arc::new(plane::flow::holder::Registry::with_delivery(needs)),
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
            let workdir = file.directories.first().cloned().unwrap_or_else(|| file.dir.clone());
            daemon.flows.holder(&file.dir, &workdir);
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
    /// The flow holders, one per plane store, shared with the planes opened
    /// here (AGL-118, D17).
    flows: plane::flow::holder::Shared,
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
    /// The wake's pair (AGL-118, D22): (plane store, flow) to the session
    /// its wake woke; the end of that turn carries `turn.end` back.
    flow_wakes: HashMap<(PathBuf, String), String>,
    /// The engines of planes that ended, kept while a plane uses their
    /// server (D16): its url, the engine, the store of its record.
    engines: Vec<(String, Engine, PathBuf)>,
    /// The servers an idle stop watches (AGL-120, D2), by url: a server
    /// stops after its `idle` without a turn, any turn's end rewinding it.
    clocks: HashMap<String, Clock>,
}

/// The idle clock of a server (AGL-120, D2): the seconds of no turn that
/// stop it, the last turn's end, how to stop it (its unit, or the child
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

        /// The next reply that is not a line told while a plane opens.
        pub(super) fn next(&mut self) -> Reply {
            loop {
                match self.lines.read::<Reply>().unwrap().unwrap() {
                    Reply::Sent { session, .. } if session.is_empty() => continue,
                    reply => return reply,
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
