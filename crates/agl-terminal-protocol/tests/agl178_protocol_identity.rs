use agl_exec::{AuthorityFingerprint, ServiceGenerationId};
use agl_terminal_protocol::{
    ServiceIdentity, TERMINAL_EVENT_SCHEMA, TERMINAL_PROTOCOL_VERSION, TERMINAL_REQUEST_SCHEMA,
    TERMINAL_RESPONSE_SCHEMA, TerminalGenerationIdentity, TerminalRequest, TerminalRequestKind,
};

fn digest(byte: char) -> AuthorityFingerprint {
    AuthorityFingerprint::new(format!("sha256:{}", byte.to_string().repeat(64))).unwrap()
}

fn installed() -> TerminalGenerationIdentity {
    TerminalGenerationIdentity::new(digest('a'), "b".repeat(40), digest('c'), 2).unwrap()
}

// AGL178-TERM-PROTO-002. The first Hello carries only the stable installed
// generation and can therefore trigger a cold socket-activated service.
#[test]
fn hello_is_pair_first_without_a_process_generation() {
    assert_eq!(TERMINAL_PROTOCOL_VERSION, 2);
    assert_eq!(
        TERMINAL_REQUEST_SCHEMA,
        "agentlibre.terminal.request.v2alpha"
    );
    assert_eq!(
        TERMINAL_RESPONSE_SCHEMA,
        "agentlibre.terminal.response.v2alpha"
    );
    assert_eq!(TERMINAL_EVENT_SCHEMA, "agentlibre.terminal.event.v2alpha");

    let request = TerminalRequest::hello(installed()).unwrap();
    assert!(matches!(request.request, TerminalRequestKind::Hello));
    assert_eq!(request.expected_generation, installed());
    assert!(request.expected_process_generation.is_none());
    let value = serde_json::to_value(&request).unwrap();
    assert!(value.get("expected_generation").is_some());
    assert!(value.get("expected_process_generation").is_none());
    assert!(value.get("expected_service").is_none());
}

// AGL178-TERM-PROTO-003. The live identity separates the stable installed
// generation from the rotating process generation and exact comparison covers
// every installed field.
#[test]
fn service_identity_separates_installed_and_process_generations() {
    let first = ServiceIdentity::new(installed(), ServiceGenerationId::generate()).unwrap();
    let restarted = ServiceIdentity::new(installed(), ServiceGenerationId::generate()).unwrap();
    assert_eq!(
        first.installed_generation(),
        restarted.installed_generation()
    );
    assert_ne!(
        first.process_generation_id(),
        restarted.process_generation_id()
    );
    first
        .installed_generation()
        .require_exact(&installed())
        .unwrap();

    for changed in [
        TerminalGenerationIdentity::new(digest('d'), "b".repeat(40), digest('c'), 2).unwrap(),
        TerminalGenerationIdentity::new(digest('a'), "d".repeat(40), digest('c'), 2).unwrap(),
        TerminalGenerationIdentity::new(digest('a'), "b".repeat(40), digest('d'), 2).unwrap(),
        TerminalGenerationIdentity::new(digest('a'), "b".repeat(40), digest('c'), 3).unwrap(),
    ] {
        assert!(installed().require_exact(&changed).is_err());
    }
}

// AGL178-TERM-PROTO-004. Old file-first/full-process Hello frames are not
// decoded by the new protocol.
#[test]
fn v1_and_full_process_hello_shapes_are_rejected() {
    let legacy = format!(
        r#"{{"schema":"agentlibre.terminal.request.v1alpha","request_id":"{}","expected_service":{{"protocol_version":1,"crate_version":"1.0.0-alpha.1","build_id":"{}","generation_id":"{}"}},"request":{{"kind":"hello"}}}}"#,
        uuid::Uuid::now_v7(),
        digest('a'),
        ServiceGenerationId::generate()
    );
    assert!(TerminalRequest::decode_json(legacy.as_bytes()).is_err());
}
