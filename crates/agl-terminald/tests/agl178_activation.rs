use agl_terminald::test_support::{
    ActivatedDescriptorMutation, ActivationFixture, ReadinessFault, ReadinessFixture,
};

// AGL178-TERM-ACT-003. The one valid systemd descriptor is consumed once
// inside Tokio, matches the configured accepting Unix socket, becomes
// close-on-exec and leaves no activation environment behind.
#[test]
fn valid_descriptor_is_adopted_once_inside_the_runtime() {
    let observation = ActivationFixture::canonical()
        .run_inside_tokio()
        .expect("canonical activated listener must be admitted");
    assert!(observation.adopted_inside_runtime());
    assert_eq!(observation.adoption_count(), 1);
    assert!(observation.is_accepting_unix_stream());
    assert!(observation.address_matches_configuration());
    assert!(observation.close_on_exec());
    assert!(observation.activation_environment_cleared());
    assert!(!observation.launcher_inherited_listener());
}

// AGL178-TERM-ACT-004. PID/count/name/type/accept-state/address and duplicate
// adoption mismatches all fail before a readiness projection exists.
#[test]
fn invalid_descriptors_fail_before_identity_publication() {
    for mutation in ActivatedDescriptorMutation::invalid_cases() {
        let observation = ActivationFixture::canonical()
            .mutate(mutation)
            .run_inside_tokio();
        assert!(observation.is_err());
        let state = observation.unwrap_err().observation();
        assert!(!state.identity_projection_exists());
        assert!(!state.descriptor_leaked());
    }
}

// AGL178-TERM-READY-001. Every pre-listener failure is projection-free; only
// a fully usable listener publishes the private runtime-root projection.
#[test]
fn readiness_projection_is_published_only_after_dependencies_and_listener() {
    for fault in ReadinessFault::before_listener_ready() {
        let result = ReadinessFixture::canonical().fault_at(fault).run();
        assert!(result.is_err());
        assert!(!result.unwrap_err().observation().projection_was_published());
    }

    let ready = ReadinessFixture::canonical().run().unwrap();
    assert!(ready.dependencies_ready_before_projection());
    assert!(ready.listener_ready_before_projection());
    assert!(ready.projection_is_private());
    assert!(ready.projection_is_below_runtime_root());
    assert!(!ready.projection_is_below_state_root());
}

// AGL178-TERM-READY-002. Stale bytes after an uncatchable death are diagnostic
// only; a live response is always required and normal shutdown removes them.
#[test]
fn stale_projection_never_authenticates_a_dead_service() {
    let killed = ReadinessFixture::canonical().simulate_sigkill().unwrap();
    assert!(killed.projection_exists());
    assert!(!killed.client_accepts_without_live_response());

    let stopped = ReadinessFixture::canonical().normal_shutdown().unwrap();
    assert!(!stopped.projection_exists());
    assert!(stopped.durable_terminal_data_exists());
}

// AGL178-TERM-READY-003. Restart preserves installed identity, rotates the
// process generation and one lifetime lock excludes a concurrent service.
#[test]
fn restart_rotates_only_process_identity_and_lifetime_is_exclusive() {
    let restarted = ReadinessFixture::canonical().restart().unwrap();
    assert_eq!(
        restarted.first_installed_identity(),
        restarted.second_installed_identity()
    );
    assert_ne!(
        restarted.first_process_generation(),
        restarted.second_process_generation()
    );

    let concurrent = ReadinessFixture::canonical().start_concurrently();
    assert_eq!(concurrent.accepted_service_count(), 1);
    assert_eq!(concurrent.rejected_service_count(), 1);
}
