use super::*;
use std::io::{BufRead, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::mpsc::Receiver;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme};
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::HandshakeError;

use crate::store::tests::temporary;

type Client = WebSocket<StreamOwned<ClientConnection, TcpStream>>;

/// What the app does (D11): the bridge's certificate, by its bytes, and no
/// other.
#[derive(Debug)]
struct Pinned(Vec<u8>);

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.0 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("not the pinned certificate".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls12_signature(message, cert, dss, &algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls13_signature(message, cert, dss, &algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A daemon that answers `sessions` with no session, and tells each line it
/// read.
fn fake_daemon(socket: &Path) -> Receiver<String> {
    let listener = UnixListener::bind(socket).unwrap();
    let (to, from) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let to = to.clone();
            thread::spawn(move || {
                let mut out = stream.try_clone().unwrap();
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { return };
                    if line.contains("\"sessions\"") {
                        out.write_all(b"{\"reply\":\"sessions\",\"sessions\":[]}\n").unwrap();
                    }
                    if to.send(line).is_err() {
                        return;
                    }
                }
            });
        }
    });
    from
}

struct Bridge {
    address: SocketAddr,
    certificate: Certificate,
    token: String,
    socket: PathBuf,
}

/// A bridge on a free port of localhost, its directory with the device
/// `iphone`; its daemon's socket, which nothing answers yet.
fn bridge(what: &str) -> Bridge {
    let dir = temporary(what);
    let store = Store::new(dir.join("bridge"));
    let certificate = store.certificate().unwrap();
    let token = store.pair("iphone").unwrap();
    let socket = dir.join("agl.sock");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let served = socket.clone();
    thread::spawn(move || serve_on(listener, store, served));
    Bridge {
        address,
        certificate,
        token,
        socket,
    }
}

fn connect(bridge: &Bridge, token: Option<&str>) -> Result<Client, tungstenite::Error> {
    let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned(bridge.certificate.der.clone())))
        .with_no_client_auth();
    let tls = ClientConnection::new(Arc::new(config), ServerName::try_from("agl-bridge").unwrap()).unwrap();
    let tcp = TcpStream::connect(bridge.address).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut request = format!("wss://{}/", bridge.address).into_client_request().unwrap();
    if let Some(token) = token {
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    match tungstenite::client(request, StreamOwned::new(tls, tcp)) {
        Ok((socket, _)) => Ok(socket),
        Err(HandshakeError::Failure(error)) => Err(error),
        Err(HandshakeError::Interrupted(_)) => panic!("the handshake was interrupted"),
    }
}

fn refused_with(result: Result<Client, tungstenite::Error>) -> StatusCode {
    match result {
        Err(tungstenite::Error::Http(response)) => response.status(),
        Err(error) => panic!("not an HTTP refusal: {error}"),
        Ok(_) => panic!("connected"),
    }
}

#[test]
fn an_allowed_request_reaches_the_daemon_and_its_reply_comes_back() {
    let bridge = bridge("relay");
    let daemon = fake_daemon(&bridge.socket);
    let mut client = connect(&bridge, Some(&bridge.token)).unwrap();
    // A frame's newlines never reach the daemon: the request goes as one line.
    client
        .send(Message::text("{\"request\":\n\"sessions\"}"))
        .unwrap();
    assert_eq!(
        daemon.recv_timeout(Duration::from_secs(5)).unwrap(),
        r#"{"request":"sessions"}"#
    );
    match client.read().unwrap() {
        Message::Text(text) => assert_eq!(text.as_str(), r#"{"reply":"sessions","sessions":[]}"#),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_wrong_or_missing_token_is_unauthorized() {
    let bridge = bridge("token");
    let _daemon = fake_daemon(&bridge.socket);
    assert_eq!(refused_with(connect(&bridge, Some("0123"))), StatusCode::UNAUTHORIZED);
    assert_eq!(refused_with(connect(&bridge, None)), StatusCode::UNAUTHORIZED);
}

#[test]
fn a_revoked_device_is_unauthorized() {
    let bridge = bridge("revoked");
    let _daemon = fake_daemon(&bridge.socket);
    connect(&bridge, Some(&bridge.token)).unwrap();
    Store::new(bridge.socket.with_file_name("bridge")).revoke("iphone").unwrap();
    assert_eq!(refused_with(connect(&bridge, Some(&bridge.token))), StatusCode::UNAUTHORIZED);
}

#[test]
fn with_no_daemon_the_bridge_is_unavailable() {
    let bridge = bridge("no-daemon");
    assert_eq!(
        refused_with(connect(&bridge, Some(&bridge.token))),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[test]
fn a_request_not_allowed_closes_with_1008_and_never_reaches_the_daemon() {
    let bridge = bridge("policy");
    let daemon = fake_daemon(&bridge.socket);
    let mut client = connect(&bridge, Some(&bridge.token)).unwrap();
    client
        .send(Message::text(r#"{"request":"new","session":"s"}"#))
        .unwrap();
    let code = loop {
        match client.read() {
            Ok(Message::Close(Some(frame))) => break frame.code,
            Ok(Message::Close(None)) => panic!("closed with no code"),
            Ok(_) => {}
            Err(error) => panic!("{error}"),
        }
    };
    assert_eq!(code, CloseCode::Policy);
    assert!(daemon.recv_timeout(Duration::from_millis(300)).is_err());
}

#[test]
fn a_frame_that_is_no_request_closes_with_1008() {
    let bridge = bridge("garbage");
    let _daemon = fake_daemon(&bridge.socket);
    let mut client = connect(&bridge, Some(&bridge.token)).unwrap();
    client.send(Message::text("rm -rf /")).unwrap();
    loop {
        match client.read() {
            Ok(Message::Close(Some(frame))) => {
                assert_eq!(frame.code, CloseCode::Policy);
                break;
            }
            Ok(_) => {}
            Err(error) => panic!("{error}"),
        }
    }
}

/// A daemon whose session `s` is busy while `busy` is set: it answers `open`
/// with `s` opened and `sessions` with `s`; it tells each line it read, and
/// `EOF` when a connection ends.
fn busy_daemon(socket: &Path, busy: Arc<std::sync::atomic::AtomicBool>) -> Receiver<String> {
    let listener = UnixListener::bind(socket).unwrap();
    let (to, from) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let to = to.clone();
            let busy = Arc::clone(&busy);
            thread::spawn(move || answer_as_busy(stream, &busy, &to));
        }
    });
    from
}

fn answer_as_busy(stream: UnixStream, busy: &std::sync::atomic::AtomicBool, to: &mpsc::Sender<String>) {
    let mut out = stream.try_clone().unwrap();
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let reply = if line.contains("\"open\"") {
            Some(Reply::Opened(proto::Opened {
                session: proto::SessionView {
                    id: "s".to_owned(),
                    ..Default::default()
                },
                ..Default::default()
            }))
        } else if line.contains("\"sessions\"") {
            Some(Reply::Sessions {
                sessions: vec![proto::SessionInfo {
                    id: "s".to_owned(),
                    busy: busy.load(Ordering::SeqCst),
                    ..Default::default()
                }],
            })
        } else {
            None
        };
        if let Some(reply) = reply
            && proto::send(&mut out, &reply).is_err()
        {
            break;
        }
        if to.send(line).is_err() {
            return;
        }
    }
    let _ = to.send("EOF".to_owned());
}

/// The lines the daemon read until a connection ended, or none in `wait`.
fn until_eof(daemon: &Receiver<String>, wait: Duration) -> Option<Vec<String>> {
    let deadline = Instant::now() + wait;
    let mut lines = Vec::new();
    loop {
        match daemon.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(line) if line == "EOF" => return Some(lines),
            Ok(line) => lines.push(line),
            Err(_) => return None,
        }
    }
}


#[test]
fn a_device_gone_during_a_turn_keeps_its_daemon_connection_until_the_turn_ends() {
    let bridge = bridge("kept");
    let busy = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let daemon = busy_daemon(&bridge.socket, Arc::clone(&busy));
    let mut client = connect(&bridge, Some(&bridge.token)).unwrap();
    client.send(Message::text(open_request())).unwrap();
    // The opened reply comes back before the device goes.
    client.read().unwrap();
    drop(client);
    // The turn runs: the connection stays, and asks the daemon.
    assert!(until_eof(&daemon, Duration::from_millis(500)).is_none());
    busy.store(false, Ordering::SeqCst);
    let lines = until_eof(&daemon, POLL * 3).expect("let go once the turn is over");
    assert!(lines.iter().any(|line| line.contains("\"sessions\"")));
}

#[test]
fn a_device_gone_with_no_session_open_lets_its_daemon_connection_go() {
    let bridge = bridge("not-kept");
    let busy = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let daemon = busy_daemon(&bridge.socket, busy);
    let client = connect(&bridge, Some(&bridge.token)).unwrap();
    drop(client);
    assert_eq!(until_eof(&daemon, Duration::from_secs(5)), Some(Vec::new()));
}

#[test]
fn a_device_that_closed_its_session_lets_its_daemon_connection_go() {
    let bridge = bridge("closed");
    let busy = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let daemon = busy_daemon(&bridge.socket, busy);
    let mut client = connect(&bridge, Some(&bridge.token)).unwrap();
    client.send(Message::text(open_request())).unwrap();
    client.read().unwrap();
    client
        .send(Message::text(r#"{"request":"close","session":"s"}"#))
        .unwrap();
    // The close reaches the daemon before the device goes.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if daemon.recv_timeout(Duration::from_secs(5)).unwrap().contains("\"close\"") {
            break;
        }
    }
    drop(client);
    let lines = until_eof(&daemon, Duration::from_secs(5)).expect("let go at once");
    assert!(!lines.iter().any(|line| line.contains("\"sessions\"")));
}

#[test]
fn a_kept_connection_is_let_go_at_its_bound() {
    let (bridge_end, daemon_end) = UnixStream::pair().unwrap();
    let busy = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (to, daemon) = mpsc::channel();
    thread::spawn(move || answer_as_busy(daemon_end, &busy, &to));
    let kept = Kept {
        device: "iphone".to_owned(),
        from: lines_of(&bridge_end).unwrap(),
        daemon: bridge_end,
        held: HashSet::from(["s".to_owned()]),
    };
    let started = Instant::now();
    assert_eq!(kept.hold(Duration::from_millis(20), Duration::from_millis(200)), "the bound");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(until_eof(&daemon, Duration::from_secs(5)).is_some());
}

#[test]
fn a_busy_session_the_device_does_not_hold_keeps_nothing() {
    let (bridge_end, daemon_end) = UnixStream::pair().unwrap();
    let busy = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (to, _daemon) = mpsc::channel();
    thread::spawn(move || answer_as_busy(daemon_end, &busy, &to));
    let kept = Kept {
        device: "iphone".to_owned(),
        from: lines_of(&bridge_end).unwrap(),
        daemon: bridge_end,
        held: HashSet::from(["other".to_owned()]),
    };
    assert_eq!(kept.hold(Duration::from_millis(20), Duration::from_secs(5)), "the turn is over");
}

/// An `open` that resumes `s`, as the app sends it.
fn open_request() -> String {
    serde_json::to_string(&Request::Open(proto::Open {
        resume: Some("s".to_owned()),
        ..Default::default()
    }))
    .unwrap()
}
