//! The bridge's server: TLS with the bridge's certificate (D11), the
//! WebSocket's upgrade with a device's token (D10), then each text frame a
//! request to the daemon and each line of the daemon a text frame (D9).
//!
//! A connection is a thread, and its own connection to the daemon, so the
//! daemon tells it apart as any client: the questions of a plane it opened
//! come to it. Its frames are read with a short timeout, and what the daemon
//! sent meanwhile goes out between two reads.
//!
//! A device gone while a session it opened runs a turn keeps its daemon
//! connection a while (AGL-083, D29 to D31): the daemon stops a session's
//! turn when its last holder leaves, so the bridge holds on until the turn
//! ends, at most `KEEP`. A device that comes back gets a new connection.

use std::collections::HashSet;
use std::io::{self, BufReader};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use agentlibre_proto::{self as proto, Lines, Reply, Request};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use tungstenite::handshake::server::{ErrorResponse, Request as Upgrade, Response};
use tungstenite::http::StatusCode;
use tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::{Message, WebSocket};

use crate::allow::allowed;
use crate::store::{Certificate, Store};

/// How long the TLS handshake and the upgrade may take together.
const HANDSHAKE: Duration = Duration::from_secs(10);
/// How long a read waits before what the daemon sent goes out.
const TICK: Duration = Duration::from_millis(50);
/// How long a write may block.
const WRITE: Duration = Duration::from_secs(30);
/// A ping after this much quiet; the connection ends after `SILENT`.
const PING: Duration = Duration::from_secs(30);
const SILENT: Duration = Duration::from_secs(90);
/// The connections at once; one more is closed at once.
const CONNECTIONS: usize = 16;
/// A failed token is answered after this pause, so that trying tokens
/// in batch is no faster than one by one (AGL-121, D31).
const TOKEN_PAUSE: Duration = Duration::from_millis(1000);
/// How long a gone device's daemon connection is kept at most while its
/// session's turn runs (AGL-083, D30).
const KEEP: Duration = Duration::from_secs(30 * 60);
/// How often a kept connection asks the daemon whether the turn still runs.
const POLL: Duration = Duration::from_secs(2);

/// What the bridge serves (D18).
pub struct Config {
    pub listen: SocketAddr,
    pub store: Store,
    /// The daemon's socket.
    pub socket: PathBuf,
}

type Tls = StreamOwned<ServerConnection, TcpStream>;

struct Shared {
    store: Store,
    socket: PathBuf,
    tls: Arc<ServerConfig>,
    open: AtomicUsize,
}

/// Serves until the process ends. The certificate must exist: `agl bridge
/// start` makes it.
pub fn serve(config: Config) -> io::Result<()> {
    let listener = TcpListener::bind(config.listen)?;
    eprintln!("agl-bridge: listening on {}", listener.local_addr()?);
    serve_on(listener, config.store, config.socket)
}

/// Serves on `listener`.
pub fn serve_on(listener: TcpListener, store: Store, socket: PathBuf) -> io::Result<()> {
    let certificate = store.read_certificate().map_err(io::Error::other)?;
    let shared = Arc::new(Shared {
        tls: Arc::new(tls_config(&certificate)?),
        store,
        socket,
        open: AtomicUsize::new(0),
    });
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if shared.open.fetch_add(1, Ordering::SeqCst) >= CONNECTIONS {
            shared.open.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            let peer = stream.peer_addr().map_or_else(|_| "?".to_owned(), |a| a.to_string());
            let kept = connection(&shared, stream, &peer).unwrap_or_else(|error| {
                eprintln!("agl-bridge: {peer}: {error}");
                None
            });
            // A kept connection holds no place of the devices' (D29).
            shared.open.fetch_sub(1, Ordering::SeqCst);
            if let Some(kept) = kept {
                let device = kept.device.clone();
                let end = kept.hold(POLL, KEEP);
                eprintln!("agl-bridge: {peer}: {device}'s daemon connection let go ({end})");
            }
        });
    }
    Ok(())
}

fn tls_config(certificate: &Certificate) -> io::Result<ServerConfig> {
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certificate.key.clone()));
    ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(certificate.der.clone())], key)
        .map_err(io::Error::other)
}

/// A connection: the handshake, then the relay; the daemon connection to
/// keep when the device left a session it opened (D29).
fn connection(shared: &Shared, stream: TcpStream, peer: &str) -> io::Result<Option<Kept>> {
    stream.set_read_timeout(Some(HANDSHAKE))?;
    stream.set_write_timeout(Some(HANDSHAKE))?;
    let tls = StreamOwned::new(
        ServerConnection::new(Arc::clone(&shared.tls)).map_err(io::Error::other)?,
        stream,
    );
    let mut device = None;
    let mut daemon = None;
    // tungstenite's callback answers a refusal with a whole HTTP response.
    #[allow(clippy::result_large_err)]
    let upgrade = |request: &Upgrade, response: Response| -> Result<Response, ErrorResponse> {
        let token = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        match token.map(|token| shared.store.device_of(token)) {
            Some(Ok(Some(name))) => device = Some(name),
            Some(Err(error)) => {
                eprintln!("agl-bridge: {error}");
                return Err(status(StatusCode::INTERNAL_SERVER_ERROR));
            }
            Some(Ok(None)) | None => {
                thread::sleep(TOKEN_PAUSE);
                return Err(status(StatusCode::UNAUTHORIZED));
            }
        }
        // No daemon, no connection: the bridge never starts one (D18).
        match UnixStream::connect(&shared.socket) {
            Ok(stream) => daemon = Some(stream),
            Err(_) => return Err(status(StatusCode::SERVICE_UNAVAILABLE)),
        }
        Ok(response)
    };
    // AGL-121, D31: a frame from a device is capped at the protocol's
    // line limit, so one frame cannot hold more than a request line.
    let config = WebSocketConfig::default()
        .max_message_size(Some(proto::LINE_LIMIT))
        .max_frame_size(Some(proto::LINE_LIMIT));
    let mut socket = match tungstenite::accept_hdr_with_config(tls, upgrade, Some(config)) {
        Ok(socket) => socket,
        // A refused upgrade has been answered; a handshake cut short or
        // too slow is just dropped.
        Err(_) => return Ok(None),
    };
    let (Some(device), Some(mut daemon)) = (device, daemon) else {
        return Ok(None);
    };
    eprintln!("agl-bridge: {peer}: {device} connected");
    socket.get_mut().sock.set_read_timeout(Some(TICK))?;
    socket.get_mut().sock.set_write_timeout(Some(WRITE))?;
    let from = lines_of(&daemon)?;
    let mut held = HashSet::new();
    let end = relay(&mut socket, &mut daemon, &from, &mut held);
    eprintln!("agl-bridge: {peer}: {device} gone ({end})");
    if end == DAEMON_GONE || held.is_empty() {
        let _ = daemon.shutdown(Shutdown::Both);
        return Ok(None);
    }
    Ok(Some(Kept {
        device,
        daemon,
        from,
        held,
    }))
}

/// The daemon's lines, read on a thread of their own.
fn lines_of(daemon: &UnixStream) -> io::Result<Receiver<String>> {
    let reader = daemon.try_clone()?;
    let (to, from) = mpsc::channel::<String>();
    thread::spawn(move || {
        // AGL-121, D31: the daemon's lines come through `proto::Lines`,
        // unparsed, and with no limit of their own (D54).
        let mut lines = Lines::new(BufReader::new(reader));
        while let Ok(Some(line)) = lines.read_raw() {
            if to.send(line).is_err() {
                break;
            }
        }
    });
    Ok(from)
}

const DAEMON_GONE: &str = "the daemon is gone";

/// A gone device's connection to the daemon, and the sessions it holds
/// there (D29).
struct Kept {
    device: String,
    daemon: UnixStream,
    from: Receiver<String>,
    held: HashSet<String>,
}

impl Kept {
    /// Holds the connection while a held session is busy, asking the
    /// daemon every `poll`, at most `bound`; then lets it go, and the
    /// daemon stops a turn no other client holds (D30). What the daemon
    /// sends meanwhile, a question among it, has no one to go to.
    fn hold(mut self, poll: Duration, bound: Duration) -> &'static str {
        let deadline = Instant::now() + bound;
        let end = 'hold: loop {
            if proto::send(&mut self.daemon, &Request::Sessions).is_err() {
                break DAEMON_GONE;
            }
            let busy = loop {
                match self.from.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(line) => {
                        if let Some(Reply::Sessions { sessions }) = reply_of(&line, "sessions") {
                            break sessions.iter().any(|s| s.busy && self.held.contains(&s.id));
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => break 'hold "the bound",
                    Err(RecvTimeoutError::Disconnected) => break 'hold DAEMON_GONE,
                }
            };
            if !busy {
                break "the turn is over";
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break "the bound";
            }
            thread::sleep(poll.min(left));
        };
        let _ = self.daemon.shutdown(Shutdown::Both);
        end
    }
}

/// The daemon's `line` as a reply, when it is one of `kind`; other lines
/// are not parsed.
fn reply_of(line: &str, kind: &str) -> Option<Reply> {
    let head = format!("{{\"reply\":\"{kind}\"");
    if !line.starts_with(&head) {
        return None;
    }
    serde_json::from_str(line).ok()
}

fn status(code: StatusCode) -> ErrorResponse {
    let mut response = ErrorResponse::new(None);
    *response.status_mut() = code;
    response
}

/// The daemon's lines to `socket`, and its requests to the daemon, until
/// either ends; what ended it. `held` follows the sessions the device
/// opened and did not close (D29).
fn relay(
    socket: &mut WebSocket<Tls>,
    daemon: &mut UnixStream,
    from: &Receiver<String>,
    held: &mut HashSet<String>,
) -> &'static str {
    let mut heard = Instant::now();
    let mut pinged = Instant::now();
    'relay: loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                heard = Instant::now();
                // A request goes on as the bridge reads it, one line: a
                // frame's own newlines never reach the daemon.
                match serde_json::from_str::<Request>(text.as_str()) {
                    Ok(request) if allowed(&request) => {
                        if let Request::Close { session } = &request {
                            held.remove(session);
                        }
                        if proto::send(daemon, &request).is_err() {
                            close(socket, CloseCode::Away, DAEMON_GONE);
                            break DAEMON_GONE;
                        }
                    }
                    _ => {
                        close(socket, CloseCode::Policy, "not allowed");
                        break "a request not allowed";
                    }
                }
            }
            Ok(Message::Binary(_)) => {
                close(socket, CloseCode::Unsupported, "text frames only");
                break "a binary frame";
            }
            Ok(_) => heard = Instant::now(),
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                break "closed";
            }
            Err(_) => break "a broken connection",
        }
        loop {
            match from.try_recv() {
                Ok(line) => {
                    if let Some(Reply::Opened(opened)) = reply_of(&line, "opened") {
                        held.insert(opened.session.id);
                    }
                    if socket.send(Message::text(line)).is_err() {
                        break 'relay "a broken connection";
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    close(socket, CloseCode::Away, DAEMON_GONE);
                    break 'relay DAEMON_GONE;
                }
            }
        }
        if heard.elapsed() > SILENT {
            break "silent";
        }
        if pinged.elapsed() > PING && heard.elapsed() > PING {
            pinged = Instant::now();
            if socket.send(Message::Ping(Vec::new().into())).is_err() {
                break "a broken connection";
            }
        }
    }
}

/// Closes with `code`, and gives the other side a second to answer.
fn close(socket: &mut WebSocket<Tls>, code: CloseCode, reason: &str) {
    let _ = socket.close(Some(CloseFrame {
        code,
        reason: reason.to_owned().into(),
    }));
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        match socket.read() {
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
            {
                let _ = socket.flush();
            }
            Err(_) => break,
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests;
