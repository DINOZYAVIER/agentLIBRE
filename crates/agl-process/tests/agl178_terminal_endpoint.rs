use agl_process::test_support::{TerminalEndpointFixture, TerminalIdentityMutation};

// AGL178-AGENT-ENDPOINT-001 / selected 3A. A cold client opens the socket with
// only its sealed installed-generation expectation; the response supplies the
// process generation and must match the private runtime projection.
#[test]
fn cold_bootstrap_is_pair_first_and_live_response_is_authoritative() {
    let fixture = TerminalEndpointFixture::canonical_cold_service();
    let connected = fixture.connect().unwrap();
    assert_eq!(fixture.connection_count(), 1);
    assert!(fixture.first_request_has_installed_generation());
    assert!(!fixture.first_request_has_process_generation());
    assert!(fixture.socket_activation_was_triggered());
    assert_eq!(connected.live_identity(), fixture.service_live_identity());
    assert_eq!(
        connected.runtime_projection(),
        fixture.service_live_identity()
    );
}

// AGL178-AGENT-ENDPOINT-002. Manifest/source/service/protocol/process and
// projection mismatches all fail before a terminal effect is dispatched.
#[test]
fn every_installed_or_live_identity_mismatch_fails_closed() {
    for mutation in TerminalIdentityMutation::each_installed_and_live_field() {
        let fixture = TerminalEndpointFixture::canonical_cold_service().mutate(mutation);
        assert!(fixture.connect().is_err(), "{}", fixture.label());
        assert_eq!(fixture.effect_dispatch_count(), 0, "{}", fixture.label());
    }
}

// AGL178-AGENT-ENDPOINT-003. Projection bytes are never independent authority,
// unsafe file metadata/path swaps are rejected and a restart rotates only the
// expected live process identity.
#[test]
fn projection_is_safe_corroboration_not_file_first_authority() {
    for fixture in TerminalEndpointFixture::stale_and_unsafe_projection_cases() {
        assert!(fixture.connect().is_err(), "{}", fixture.label());
        assert_eq!(fixture.effect_dispatch_count(), 0, "{}", fixture.label());
    }

    let restarted = TerminalEndpointFixture::canonical_cold_service()
        .restart_service()
        .unwrap();
    assert_eq!(
        restarted.installed_generation_before(),
        restarted.installed_generation_after()
    );
    assert_ne!(
        restarted.process_generation_before(),
        restarted.process_generation_after()
    );
    assert!(restarted.old_connection_is_rejected());
    assert!(restarted.new_connection_is_admitted());
}
