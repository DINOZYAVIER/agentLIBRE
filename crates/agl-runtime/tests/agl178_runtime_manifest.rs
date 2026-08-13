use agl_runtime::test_support::{RuntimeManifestFixture, TerminalPairMutation};

// AGL178-AGENT-MAN-001 / selected 1A and 2A. One sealed agent manifest v3
// contains the exact independently verified terminal generation alongside the
// already selected engine closure, and public runtime identity is v2.
#[test]
fn runtime_manifest_v3_seals_exact_terminal_generation() {
    let sealed = RuntimeManifestFixture::canonical().seal().unwrap();
    assert_eq!(sealed.manifest_schema(), "agentlibre.runtime-manifest/v3");
    assert_eq!(
        sealed.runtime_identity_schema(),
        "agentlibre.runtime-identity/v2"
    );
    assert_eq!(
        sealed.terminal_generation(),
        sealed.expected_terminal_generation()
    );
    assert_eq!(
        sealed.engine_protocol_id(),
        sealed.expected_engine_protocol_id()
    );
    assert!(sealed.engine_library_count() > 0);
    assert!(sealed.generation_id_uses_v3_domain());
}

// AGL178-AGENT-MAN-002. Every terminal-pair field contributes to the agent
// generation identity, including two exact builds of one source revision.
#[test]
fn every_terminal_pair_field_changes_agent_generation_identity() {
    let baseline = RuntimeManifestFixture::canonical().seal().unwrap();
    for mutation in TerminalPairMutation::each_identity_field() {
        let changed = RuntimeManifestFixture::canonical()
            .mutate_terminal(mutation)
            .seal()
            .unwrap();
        assert_ne!(baseline.generation_id(), changed.generation_id());
    }

    let alternate_build = RuntimeManifestFixture::same_terminal_source_different_build()
        .seal()
        .unwrap();
    assert_eq!(
        baseline.terminal_source_revision(),
        alternate_build.terminal_source_revision()
    );
    assert_ne!(baseline.generation_id(), alternate_build.generation_id());
}

// AGL178-AGENT-MAN-003. AGL-173 v2, mismatched compiled Git source and any
// terminal artifact drift fail rather than entering a compatibility path.
#[test]
fn old_or_mismatched_terminal_pair_is_rejected() {
    assert!(RuntimeManifestFixture::legacy_v2().load().is_err());
    assert!(
        RuntimeManifestFixture::terminal_source_mismatch()
            .seal()
            .is_err()
    );
    for fixture in RuntimeManifestFixture::each_terminal_artifact_drift() {
        assert!(fixture.load().is_err(), "{}", fixture.label());
    }
}
