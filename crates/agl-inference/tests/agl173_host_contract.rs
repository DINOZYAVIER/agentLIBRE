use agl_inference::test_support::{
    AdmissionFixture, ConversionFixture, HostFixture, LeaseFixture, PoolCharge, QueueFixture,
    ReceiptMutation, ResidentFixture, SupervisorFixture,
};
use agl_inference::{
    InferenceFailure, InferenceHost, InferenceHostStartError, InferenceQueueRejection,
    LiveAdmissionRejection,
};

fn start(fixture: &HostFixture) -> InferenceHost {
    InferenceHost::start(fixture.config()).expect("host fixture must start")
}

// MIW-TYPE-001. Canonical host conversion is exhaustive and rejects every
// impossible or foreign native value.
#[test]
fn host_engine_native_conversions_are_checked() {
    for fixture in ConversionFixture::valid_variants() {
        let host = InferenceHost::start(fixture.config()).unwrap();
        assert_eq!(host.static_capabilities(), fixture.expected_capabilities());
    }
    for fixture in ConversionFixture::invalid_variants() {
        assert!(matches!(
            InferenceHost::start(fixture.config()),
            Err(InferenceHostStartError::InvalidEngineInventory { .. })
        ));
    }
    for mutation in ReceiptMutation::invalid_variants() {
        let fixture = AdmissionFixture::canonical().mutate_receipt(mutation);
        assert!(matches!(
            fixture.run(),
            Err(InferenceFailure::InvalidAllocationReceipt { .. })
        ));
    }
}

// MIW-ADM-001. CPU still reserves host RAM and rejects before any engine
// request when the complete envelope does not fit.
#[test]
fn cpu_generation_is_admitted_through_the_host_ram_ledger() {
    let fixture = AdmissionFixture::cpu().available_host_bytes(63 << 20);
    let before = fixture.ledger_snapshot();
    assert!(matches!(
        fixture.run(),
        Err(InferenceFailure::Admission(
            LiveAdmissionRejection::InsufficientHostMemory { .. }
        ))
    ));
    assert_eq!(fixture.ledger_snapshot(), before);
    assert_eq!(fixture.engine_dispatch_count(), 0);
}

// MIW-ADM-002. RAM, every VRAM pool and shared memory commit atomically.
#[test]
fn complete_resource_envelope_is_reserved_atomically() {
    for fixture in AdmissionFixture::each_insufficient_dimension() {
        let before = fixture.ledger_snapshot();
        assert!(fixture.run().is_err());
        assert_eq!(fixture.ledger_snapshot(), before, "{}", fixture.label());
        assert_eq!(fixture.engine_dispatch_count(), 0);
    }

    let fixture = AdmissionFixture::canonical();
    fixture.run().unwrap();
    assert_eq!(
        fixture.reservation().host_bytes(),
        fixture.required_host_bytes()
    );
    assert_eq!(
        fixture.reservation().device_bytes(),
        fixture.required_device_bytes()
    );
    assert_eq!(
        fixture.reservation().shared_bytes(),
        fixture.required_shared_bytes()
    );
}

// MIW-ADM-003. Every component maps to exactly one physical pool for discrete
// and unified-memory hosts.
#[test]
fn discrete_and_unified_memory_components_are_charged_once() {
    let discrete = AdmissionFixture::discrete().run_and_observe().unwrap();
    assert_eq!(
        discrete.charges(),
        &[
            PoolCharge::host_private(discrete.expected_host_private()),
            PoolCharge::device_private(discrete.expected_device_private()),
            PoolCharge::shared(discrete.expected_shared()),
        ]
    );
    discrete.assert_every_component_classified_once();

    let unified = AdmissionFixture::unified().run_and_observe().unwrap();
    assert_eq!(unified.physical_shared_charge(), unified.expected_shared());
    unified.assert_every_component_classified_once();
    assert_eq!(
        unified.total_physical_charge(),
        unified.expected_total_without_double_count()
    );
}

// MIW-ADM-004. Daemon clients share one queue/ledger; standalone owns host and
// selected-device lifetime leases.
#[test]
fn daemon_and_standalone_have_one_live_resource_authority_each() {
    let daemon = AdmissionFixture::two_daemon_clients();
    daemon.submit_both();
    assert_eq!(daemon.ledger_identity_count(), 1);
    assert!(daemon.combined_reservations_fit_one_ledger());

    let standalone = LeaseFixture::standalone();
    let host = start(standalone.host_fixture());
    assert!(standalone.host_lease_is_held());
    assert_eq!(
        standalone.held_device_leases(),
        standalone.selected_devices()
    );
    drop(host);
    standalone.assert_all_leases_released();

    let blocked = LeaseFixture::standalone().with_device_lease_held_elsewhere();
    assert!(matches!(
        InferenceHost::start(blocked.host_fixture().config()),
        Err(InferenceHostStartError::LeaseUnavailable { .. })
    ));
}

// MIW-ADM-005. Only idle unpinned entries evict and bytes return after the
// matching unload acknowledgement, followed by exactly one re-evaluation.
#[test]
fn eviction_waits_for_exact_engine_unload_acknowledgement() {
    let fixture = AdmissionFixture::needs_eviction();
    fixture.run().unwrap();
    assert_eq!(
        fixture.evicted_entries(),
        fixture.expected_idle_unpinned_entries()
    );
    assert!(
        fixture
            .active_entries()
            .iter()
            .all(|entry| !entry.evicted())
    );
    assert!(
        fixture
            .pinned_entries()
            .iter()
            .all(|entry| !entry.evicted())
    );
    assert!(fixture.bytes_release_happened_after_unload_ack());
    assert_eq!(fixture.admission_evaluation_count(), 2);

    let missing_ack = AdmissionFixture::needs_eviction().without_unload_ack();
    assert!(missing_ack.run().is_err());
    assert_eq!(missing_ack.released_bytes(), 0);
}

// MIW-ADM-006 and MIW-ADM-007. Receipts bind all authority identities, and
// rejection has no fallback/reselection/retry side effects.
#[test]
fn receipt_mismatch_and_capacity_rejection_fail_closed() {
    for mutation in ReceiptMutation::plan_device_reservation_and_generation() {
        let fixture = AdmissionFixture::canonical().mutate_receipt(mutation);
        assert!(matches!(
            fixture.run(),
            Err(InferenceFailure::InvalidAllocationReceipt { .. })
        ));
        assert!(fixture.quarantined());
    }

    let rejected = AdmissionFixture::insufficient_vram();
    let selected = rejected.selected_shape();
    assert!(rejected.run().is_err());
    assert_eq!(rejected.selected_shape(), selected);
    assert_eq!(rejected.engine_allocation_count(), 0);
    assert_eq!(rejected.retry_count(), 0);
    assert!(!rejected.cpu_fallback_attempted());
}

// MIW-ADM-008. Resolved media, transport copies and decoder allowance join the
// same atomic host-RAM reservation and always release at terminal.
#[test]
fn media_memory_is_admitted_before_engine_visibility() {
    let fixture = AdmissionFixture::with_media();
    fixture.run().unwrap();
    assert_eq!(
        fixture.reserved_media_bytes(),
        fixture.content_bytes() + fixture.transport_bytes() + fixture.decoder_allowance_bytes()
    );
    assert!(fixture.media_became_visible_after_admission());
    assert_eq!(fixture.media_bytes_after_terminal(), 0);

    let rejected = AdmissionFixture::with_media().insufficient_for_media();
    assert!(rejected.run().is_err());
    assert_eq!(rejected.engine_media_bytes_seen(), 0);
    assert_eq!(rejected.media_bytes_after_terminal(), 0);
}

// MIW-ADM-009. Exact resident identity earns only the already charged model
// credit; any key/generation/receipt drift earns none.
#[test]
fn resident_model_credit_requires_exact_key_and_generation() {
    let exact = ResidentFixture::exact_second_plan();
    exact.run().unwrap();
    assert_eq!(exact.selected_device(), exact.original_device());
    assert_eq!(exact.newly_reserved_bytes(), exact.incremental_bytes());
    assert_eq!(exact.model_load_count(), 1);

    for fixture in ResidentFixture::mismatched_reuse_cases() {
        assert_eq!(fixture.reuse_credit_bytes(), 0, "{}", fixture.label());
        assert!(fixture.run().is_err());
        assert!(!fixture.cpu_fallback_attempted());
    }
}

// MIW-QUE-001. Queue cancellation/deadline removal is immediate, FIFO and
// exactly-once under pop races.
#[test]
fn queued_cancellation_and_deadline_races_preserve_fifo_and_capacity() {
    for mut fixture in QueueFixture::cancel_and_deadline_races() {
        let cancelled = fixture.target_id();
        fixture.run_race();
        assert_eq!(fixture.completion_count(cancelled), 1);
        assert!(!fixture.pending_ids().contains(&cancelled));
        assert_eq!(fixture.pending_len(), fixture.capacity_used());
        assert_eq!(
            fixture.survivor_pop_order(),
            fixture.original_survivor_order()
        );
    }
}

// MIW-QUE-002. One fixed slot serializes generation; active work is separate
// from pending capacity and shutdown closes every waiter deterministically.
#[test]
fn one_slot_queue_is_bounded_and_shutdown_is_total() {
    let mut fixture = QueueFixture::one_active_and_full_pending();
    assert_eq!(fixture.active_generation_count(), 1);
    assert_eq!(fixture.engine_slot_count(), 1);
    assert_eq!(fixture.pending_len(), fixture.pending_capacity());
    assert!(matches!(
        fixture.submit_one_more(),
        Err(InferenceFailure::Queue(InferenceQueueRejection::Full {
            retryable: true,
            ..
        }))
    ));

    fixture.shutdown();
    assert!(matches!(
        fixture.submit_one_more(),
        Err(InferenceFailure::Queue(
            InferenceQueueRejection::ShuttingDown
        ))
    ));
    assert!(fixture.all_pending_cancelled());
    assert!(fixture.active_attempt_was_signalled());
}

// MIW-SUP-001. Crash/device loss closes the active attempt once, keeps pending
// FIFO work and starts a clean generation only for a later explicit request.
#[test]
fn engine_failure_does_not_retry_the_current_attempt() {
    for mut fixture in SupervisorFixture::crash_and_device_loss() {
        fixture.fail_active_generation();
        assert_eq!(fixture.active_terminal_count(), 1);
        assert_eq!(fixture.current_attempt_retry_count(), 0);
        assert_eq!(fixture.pending_ids(), fixture.original_pending_ids());
        assert!(fixture.cooldown_is_bounded_and_lazy());
        assert_eq!(fixture.generation_start_count(), 1);
        fixture.submit_after_cooldown();
        assert_eq!(fixture.generation_start_count(), 2);
        assert_ne!(fixture.latest_generation(), fixture.failed_generation());
    }
}

// MIW-SUP-002. Every host/process termination route reaps the exact child,
// closes unrelated FDs and reconciles reservations without losing durable
// quarantine/cooldown identity.
#[test]
fn supervisor_reaps_and_reconciles_every_generation_boundary() {
    for mut fixture in SupervisorFixture::termination_cases() {
        fixture.terminate();
        assert_eq!(fixture.exact_child_reap_count(), 1);
        assert!(fixture.unrelated_fds_are_closed());
        assert_eq!(fixture.unreconciled_reservation_bytes(), 0);
        let health = fixture.durable_health_identity();
        fixture.handoff_to_new_host();
        assert_eq!(fixture.durable_health_identity(), health);
        assert!(fixture.cooldown_and_quarantine_preserved());
    }
}

// MIW-SEC-001. Same-UID access is not a sandbox boundary.
#[test]
fn native_process_has_only_admitted_descriptors_and_devices() {
    let fixture = HostFixture::sandbox_negative_probes();
    let host = start(&fixture);
    let probes = fixture.run_same_uid_probes(&host);
    assert!(probes.network_denied());
    assert!(probes.workspace_denied());
    assert!(probes.content_store_denied());
    assert!(probes.database_denied());
    assert!(probes.pty_denied());
    assert!(probes.unrelated_fds_denied());
    assert!(probes.unadmitted_devices_denied());
}
