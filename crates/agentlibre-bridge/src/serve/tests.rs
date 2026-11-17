use super::*;
use std::io::Write;
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
