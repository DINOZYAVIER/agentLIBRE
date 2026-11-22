//! What a client of the daemon needs, and nothing else (AGL-076, D5, D6,
//! D7; AGL-121, D29, D65): its socket, the connection that says `hello`,
//! the daemon started when none answers, a stop reason's name, and the fold
//! of what a session reports into a conversation.
//!
//! Only `proto` is beneath the crate, so a client that cannot link the core
//! (AGL-076 D2) links this instead of repeating it: the terminal and
//! `agl notify` take it through the core, the chat takes it directly.

mod conversation;

pub use conversation::{Conversation, Row, Step, Who};

use std::ffi::OsString;
use std::fmt;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use agentlibre_proto::{self as proto, Lines, Read, Reply, Request};

/// How long a daemon that was started may take to answer.
const START_WAIT: Duration = Duration::from_secs(10);

/// The unit of the daemon, asked for its start where it is installed
/// (AGL-120, D4): the name a home module installs it under, so a unit and
/// a forked process no longer race for the socket.
pub const DAEMON_UNIT: &str = "agl-daemon";

/// The directory of the daemon's socket under `$XDG_RUNTIME_DIR`. agl's
/// `Paths` name it too; a client that has no `Paths` takes it from here.
const APP_DIR: &str = "agentLIBRE";

/// A client's error: the daemon, not the session behind it.
#[derive(Debug)]
pub enum Error {
    /// There is no runtime directory, so no place for the daemon's socket (D7).
    NoRuntime,
    /// The daemon cannot be reached, read or written.
    Daemon { detail: String },
    /// The daemon refused the request, in its own words.
    Said { text: String },
    /// The daemon's stream ended.
    Gone,
}

impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoRuntime => write!(out, "XDG_RUNTIME_DIR is not set, so it has no place for its socket"),
            Error::Daemon { detail } => write!(out, "the daemon: {detail}"),
            Error::Said { text } => write!(out, "{text}"),
            Error::Gone => write!(out, "the daemon went"),
        }
    }
}

impl std::error::Error for Error {}

/// The daemon's socket in the runtime directory `runtime` (AGL-076, D7).
pub fn socket_path(runtime: &Path) -> PathBuf {
    runtime.join(proto::SOCKET)
}

/// The daemon's socket of this machine, `$XDG_RUNTIME_DIR/agentLIBRE/agl.sock`
/// (AGL-076, D7): `None` with no absolute runtime directory. A client that
/// is not agl takes this; agl has its own `Paths`.
pub fn socket_of_env(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let runtime = env("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    runtime.is_absolute().then(|| socket_path(&runtime.join(APP_DIR)))
}

/// How a client reaches a daemon that is not there (AGL-076, D6).
pub enum Start {
    /// Ask only what answers, start nothing: the notifier and the bridge (AGL-083 D18).
    No,
    /// Fork this executable as `agl daemon`: what agl does.
    ThisExe,
    /// Run `agl daemon` as the PATH names it: a client that is not agl.
    OnPath,
}

pub struct Client {
    out: Mutex<UnixStream>,
    from: Mutex<Receiver<Reply>>,
    /// The daemon's pid, from its `hello`.
    pub pid: u32,
}

impl Client {
    /// Connects to the daemon at `socket`, which is started when none
    /// answers and `start` allows (D6); `human` says whether a human
    /// answers the planes' questions (D11), `lang` is the code of the
    /// language the daemon answers in.
    pub fn connect(socket: &Path, human: bool, lang: &str, start: Start) -> Result<Client, Error> {
        let stream = match UnixStream::connect(socket) {
            Ok(stream) => stream,
            Err(source) => match start {
                Start::No => {
                    return Err(Error::Daemon {
                        detail: format!("{}: {source}", socket.display()),
                    });
                }
                Start::ThisExe => start_daemon(socket, Program::ThisExe)?,
                Start::OnPath => start_daemon(socket, Program::OnPath)?,
            },
        };
        Client::of(stream, human, lang)
    }

    /// A client on `stream`, its `hello` said.
    pub fn of(stream: UnixStream, human: bool, lang: &str) -> Result<Client, Error> {
        let fail = |source: std::io::Error| Error::Daemon {
            detail: source.to_string(),
        };
        let reader = stream.try_clone().map_err(fail)?;
        let (to, from) = mpsc::channel();
        thread::spawn(move || {
            let mut lines = Lines::new(BufReader::new(reader));
            while let Ok(Read::Message(reply)) = lines.read::<Reply>() {
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
        match client.ask(
            &Request::Hello {
                version: proto::VERSION,
                human,
                lang: lang.to_owned(),
                pid: std::process::id(),
            },
            &mut |_| {},
        )? {
            Reply::Hello { pid, .. } => Ok(Client { pid, ..client }),
            other => Err(Error::Daemon {
                detail: format!("an unexpected reply: {other:?}"),
            }),
        }
    }

    /// Sends `request` and waits for its reply; what the daemon sends
    /// meanwhile unasked goes to `each`. A refusal is an error.
    pub fn ask(&self, request: &Request, each: &mut dyn FnMut(Reply)) -> Result<Reply, Error> {
        self.send(request)?;
        loop {
            match self.next()? {
                reply @ Reply::Refused { .. } => return Err(refused(reply)),
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

    /// What the daemon sent that waits, if anything.
    pub fn try_next(&self) -> Option<Reply> {
        let from = self.from.lock().unwrap_or_else(|e| e.into_inner());
        from.try_recv().ok()
    }

    /// What the daemon sends next.
    pub fn next(&self) -> Result<Reply, Error> {
        let from = self.from.lock().unwrap_or_else(|e| e.into_inner());
        from.recv().map_err(|_| Error::Gone)
    }
}

/// What the daemon sends without a request: what a session does, a plane's
/// question.
pub fn is_unasked(reply: &Reply) -> bool {
    matches!(reply, Reply::Sent { .. } | Reply::Done { .. } | Reply::Question { .. })
}

fn refused(reply: Reply) -> Error {
    match reply {
        Reply::Refused { error } => Error::Said { text: error },
        other => Error::Daemon {
            detail: format!("an unexpected reply: {other:?}"),
        },
    }
}

/// A stop's reason as the session's file names it (`step_limit`…): the tag
/// of the protocol, which is what the file writes.
pub fn reason_name(reason: proto::StopReason) -> String {
    serde_json::to_value(reason)
        .ok()
        .and_then(|json| json.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The program to fork as `agl daemon` (D6).
enum Program {
    /// This executable: what agl does.
    ThisExe,
    /// `agl` as the PATH names it: a client that is not agl.
    OnPath,
}

/// Starts the daemon apart from this process and waits until its socket
/// answers (D6). Where the `agl-daemon` unit is installed, systemd owns
/// that one: the client asks the user manager for it and forks nothing
/// (AGL-120, D4), so a unit and a terminal process no longer race for the
/// socket; where there is no such unit, `agl daemon` still forks.
fn start_daemon(socket: &Path, program: Program) -> Result<UnixStream, Error> {
    let fail = |detail: String| Error::Daemon { detail };
    if unit_installed_of(DAEMON_UNIT) {
        let _ = Command::new("systemctl")
            .args(["--user", "start", DAEMON_UNIT])
            .stdin(Stdio::null())
            .output();
    } else {
        let mut command = match program {
            Program::ThisExe => {
                let program = std::env::current_exe().map_err(|error| fail(error.to_string()))?;
                Command::new(program)
            }
            Program::OnPath => Command::new("agl"),
        };
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
    }
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

/// Whether `systemctl --user cat {unit}` says the unit is there: the `# /`
/// marker before a unit file, or the `###` word for a masked one. No
/// manager, no such unit, no answer of that kind: `false`, and the client
/// forks as before.
fn unit_installed(output: &str) -> bool {
    output.lines().any(|line| line.starts_with("# /") || line.starts_with("###"))
}

/// Whether the daemon's unit is installed for this user's manager
/// (AGL-120, D4).
fn unit_installed_of(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "cat", unit])
        .stdin(Stdio::null())
        .output()
        .map(|output| unit_installed(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The socket of an absolute runtime directory only (AGL-076, D7).
    #[test]
    fn the_socket_wants_an_absolute_runtime_directory() {
        let vars = |vars: &[(&str, &str)]| {
            let owned: Vec<(String, OsString)> = vars
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).into()))
                .collect();
            move |name: &str| owned.iter().find(|(key, _)| key == name).map(|(_, v)| v.clone())
        };
        assert_eq!(
            socket_of_env(vars(&[("XDG_RUNTIME_DIR", "/run/user/1000")])),
            Some(PathBuf::from("/run/user/1000/agentLIBRE/agl.sock"))
        );
        assert_eq!(socket_of_env(vars(&[("XDG_RUNTIME_DIR", "relative")])), None);
        assert_eq!(socket_of_env(vars(&[])), None);
    }

    /// The marker `systemctl --user cat` prints before a unit's text, and
    /// its absence for a unit that has no files (AGL-120, D4).
    #[test]
    fn an_installed_unit_is_named_by_its_file() {
        assert!(unit_installed("# /nix/store/abcd-agl-daemon.service/agl-daemon.service\n[Unit]\n"));
        // A masked unit is installed too: the marker is a link, not a path.
        assert!(unit_installed("### Unit agl-daemon.service is masked. ###\n"));
        // No manager, or no unit: `cat` prints nothing to stdout.
        assert!(!unit_installed(""));
        assert!(!unit_installed("No files found for agl-daemon.service.\n"));
    }

    /// A stop's name is the tag of the protocol (AGL-121, D29).
    #[test]
    fn a_stop_names_what_the_file_writes() {
        assert_eq!(reason_name(proto::StopReason::StepLimit), "step_limit");
        assert_eq!(reason_name(proto::StopReason::StirLimit), "stir_limit");
    }
}
