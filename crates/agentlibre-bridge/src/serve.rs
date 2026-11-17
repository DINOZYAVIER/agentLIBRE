//! The bridge's server: TLS with the bridge's certificate (D11), the
//! WebSocket's upgrade with a device's token (D10), then each text frame a
//! request to the daemon and each line of the daemon a text frame (D9).
//!
//! A connection is a thread, and its own connection to the daemon, so the
//! daemon tells it apart as any client: the questions of a plane it opened
//! come to it. Its frames are read with a short timeout, and what the daemon
//! sent meanwhile goes out between two reads.

use std::io::{self, BufRead, BufReader};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use agentlibre_proto::{self as proto, Request};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use tungstenite::handshake::server::{ErrorResponse, Request as Upgrade, Response};
use tungstenite::http::StatusCode;
use tungstenite::protocol::CloseFrame;
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
            if let Err(error) = connection(&shared, stream, &peer) {
                eprintln!("agl-bridge: {peer}: {error}");
            }
            shared.open.fetch_sub(1, Ordering::SeqCst);
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

/// A connection: the handshake, then the relay.
fn connection(shared: &Shared, stream: TcpStream, peer: &str) -> io::Result<()> {
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
            Some(Ok(None)) | None => return Err(status(StatusCode::UNAUTHORIZED)),
        }
        // No daemon, no connection: the bridge never starts one (D18).
        match UnixStream::connect(&shared.socket) {
            Ok(stream) => daemon = Some(stream),
            Err(_) => return Err(status(StatusCode::SERVICE_UNAVAILABLE)),
        }
        Ok(response)
    };
    let mut socket = match tungstenite::accept_hdr(tls, upgrade) {
        Ok(socket) => socket,
        // A refused upgrade has been answered; a handshake cut short or
        // too slow is just dropped.
        Err(_) => return Ok(()),
    };
    let (Some(device), Some(daemon)) = (device, daemon) else {
        return Ok(());
    };
    eprintln!("agl-bridge: {peer}: {device} connected");
    socket.get_mut().sock.set_read_timeout(Some(TICK))?;
    socket.get_mut().sock.set_write_timeout(Some(WRITE))?;
    let end = relay(&mut socket, daemon);
    eprintln!("agl-bridge: {peer}: {device} gone ({end})");
    Ok(())
}

fn status(code: StatusCode) -> ErrorResponse {
    let mut response = ErrorResponse::new(None);
    *response.status_mut() = code;
    response
}

/// The daemon's lines to `socket`, and its requests to the daemon, until
/// either ends; what ended it.
fn relay(socket: &mut WebSocket<Tls>, daemon: UnixStream) -> &'static str {
    let Ok(reader) = daemon.try_clone() else {
        return "no daemon";
    };
    let (to, from) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut lines = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match lines.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if to.send(line.trim_end_matches('\n').to_owned()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut daemon = daemon;
    let mut heard = Instant::now();
    let mut pinged = Instant::now();
    let end = 'relay: loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                heard = Instant::now();
                // A request goes on as the bridge reads it, one line: a
                // frame's own newlines never reach the daemon.
                match serde_json::from_str::<Request>(text.as_str()) {
                    Ok(request) if allowed(&request) => {
                        if proto::send(&mut daemon, &request).is_err() {
                            close(socket, CloseCode::Away, "the daemon is gone");
                            break "the daemon is gone";
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
                    if socket.send(Message::text(line)).is_err() {
                        break 'relay "a broken connection";
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    close(socket, CloseCode::Away, "the daemon is gone");
                    break 'relay "the daemon is gone";
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
    };
    let _ = daemon.shutdown(Shutdown::Both);
    end
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
