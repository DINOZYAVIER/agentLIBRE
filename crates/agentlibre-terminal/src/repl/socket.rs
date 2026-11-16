//! The REPL's control socket (AGL-074, D3, D4): a surface beside the
//! terminal sends the session its next message or stops its turn
//! ([`crate::control`] holds the protocol).
//!
//! The socket is made when the session starts and removed when it ends; a
//! `/new` moves it to the new session's id. Its directory is 0700 and the
//! socket 0600: only the human's own programs reach it.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::control::{self, Reply, Request};

/// How often the listener looks whether the session ended.
const POLL: Duration = Duration::from_millis(100);
/// How long a connection may wait between its lines.
const IDLE: Duration = Duration::from_secs(30);

/// A session's socket, listened on by a thread of its own.
pub struct Socket {
    runtime: PathBuf,
    path: Mutex<PathBuf>,
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Why a session has no socket.
#[derive(Debug)]
pub enum NoSocket {
    /// No runtime directory (`XDG_RUNTIME_DIR` unset).
    NoRuntime,
    /// Another running agl has the session's socket.
    Taken(PathBuf),
    /// The directory or the socket could not be made.
    Io(PathBuf, io::Error),
}

impl Socket {
    /// Listens on the socket of the session `id`, answering each request
    /// with `answer`.
    pub fn start(
        runtime: Option<&Path>,
        id: &str,
        answer: impl FnMut(Request) -> Reply + Send + 'static,
    ) -> Result<Socket, NoSocket> {
        let runtime = runtime.ok_or(NoSocket::NoRuntime)?;
        let dir = control::sockets_dir(runtime);
        fs::create_dir_all(&dir)
            .and_then(|()| fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)))
            .map_err(|error| NoSocket::Io(dir.clone(), error))?;
        let path = control::socket_path(runtime, id);
        let listener = bind(&path)?;
        let done = Arc::new(AtomicBool::new(false));
        let thread = {
            let done = done.clone();
            thread::spawn(move || listen(listener, &done, answer))
        };
        Ok(Socket {
            runtime: runtime.to_owned(),
            path: Mutex::new(path),
            done,
            thread: Some(thread),
        })
    }

    /// Moves the socket to the session `id` (after a `/new`).
    pub fn rename(&self, id: &str) -> io::Result<()> {
        let mut path = self.path.lock().unwrap_or_else(|e| e.into_inner());
        let to = control::socket_path(&self.runtime, id);
        if *path != to {
            fs::rename(&*path, &to)?;
            *path = to;
        }
        Ok(())
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let path = self.path.lock().unwrap_or_else(|e| e.into_inner());
        let _ = fs::remove_file(&*path);
    }
}

/// Binds `path`: one left by an agl that ended is removed, one another agl
/// answers on is not taken.
fn bind(path: &Path) -> Result<UnixListener, NoSocket> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(NoSocket::Taken(path.to_owned()));
        }
        let _ = fs::remove_file(path);
    }
    let io_error = |error| NoSocket::Io(path.to_owned(), error);
    let listener = UnixListener::bind(path).map_err(io_error)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io_error)?;
    listener.set_nonblocking(true).map_err(io_error)?;
    Ok(listener)
}

/// Takes the connections one after another until `done`.
fn listen(listener: UnixListener, done: &AtomicBool, mut answer: impl FnMut(Request) -> Reply) {
    while !done.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let served = stream
                    .set_nonblocking(false)
                    .and_then(|()| stream.set_read_timeout(Some(IDLE)))
                    .and_then(|()| control::serve(stream, &mut answer));
                // A client that went or idled is no error of the session.
                drop(served);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
            Err(_) => thread::sleep(POLL),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Hello, ask};

    fn runtime() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agl-socket-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_session_answers_on_its_socket_and_removes_it_at_its_end() {
        let runtime = runtime();
        let said = Arc::new(Mutex::new(Vec::new()));
        let socket = {
            let said = said.clone();
            Socket::start(Some(&runtime), "s1", move |request| match request {
                Request::Say(text) => {
                    said.lock().unwrap().push(text);
                    Reply::Said { queued: false }
                }
                Request::Stop => Reply::Stopped,
                Request::Hello => Reply::Hello(Hello {
                    pid: 7,
                    session: "s1".into(),
                    name: Some("tea".into()),
                    busy: false,
                }),
            })
            .unwrap()
        };
        let path = control::socket_path(&runtime, "s1");
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&control::sockets_dir(&runtime)), 0o700);
        assert_eq!(
            ask(&path, &Request::Say("go on".into())).unwrap(),
            Reply::Said { queued: false }
        );
        assert_eq!(ask(&path, &Request::Stop).unwrap(), Reply::Stopped);
        let Reply::Hello(hello) = ask(&path, &Request::Hello).unwrap() else {
            panic!("no hello");
        };
        assert_eq!(hello.name.as_deref(), Some("tea"));
        assert_eq!(*said.lock().unwrap(), ["go on"]);

        // Taken while it answers; moved by a `/new`.
        assert!(matches!(
            Socket::start(Some(&runtime), "s1", |_| Reply::Stopped),
            Err(NoSocket::Taken(_))
        ));
        socket.rename("s2").unwrap();
        let moved = control::socket_path(&runtime, "s2");
        assert!(!path.exists() && moved.exists());
        assert_eq!(ask(&moved, &Request::Stop).unwrap(), Reply::Stopped);
        drop(socket);
        assert!(!moved.exists(), "removed at the end");

        // One left behind by an agl that ended is taken over.
        let _stale = UnixListener::bind(&path).unwrap();
        drop(_stale);
        let again = Socket::start(Some(&runtime), "s1", |_| Reply::Stopped).unwrap();
        assert_eq!(ask(&path, &Request::Stop).unwrap(), Reply::Stopped);
        drop(again);
        assert!(matches!(
            Socket::start(None, "s1", |_| Reply::Stopped),
            Err(NoSocket::NoRuntime)
        ));
        let _ = fs::remove_dir_all(&runtime);
    }
}
