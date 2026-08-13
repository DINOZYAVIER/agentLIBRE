use agl_runtime::test_support::{
    CrossProductInstallFixture, InstallFault, UninstallOrder, UnitConflict,
};

// AGL178-AGENT-TXN-001. Every publication boundary is fail-closed: neither a
// partial agent generation nor a newly selected terminal can become active,
// and no obsolete generation is started as rollback.
#[test]
fn install_faults_leave_both_new_generations_stopped_and_recoverable() {
    for fault in InstallFault::every_publication_boundary() {
        let fixture = CrossProductInstallFixture::canonical().fault_at(fault);
        let result = fixture.install();
        assert!(result.is_err(), "{}", fixture.label());
        let observation = fixture.observation();
        assert!(!observation.partial_agent_generation_is_current());
        assert!(!observation.new_terminal_service_is_running());
        assert!(!observation.new_agent_service_is_running());
        assert!(!observation.legacy_generation_was_started());
        assert!(observation.typed_recovery_report_exists());
        assert!(observation.rerun_is_idempotent());
    }
}

// AGL178-AGENT-TXN-002. Terminal and agent operation locks have one order,
// span verify-through-seal, and concurrent install/uninstall loses before any
// mutable publication.
#[test]
fn coordinated_operations_hold_both_locks_in_one_order() {
    let fixture = CrossProductInstallFixture::canonical();
    let installed = fixture.install().unwrap();
    assert_eq!(installed.lock_order(), ["terminal", "agent"]);
    assert!(installed.terminal_lock_spans_offline_verify_and_agent_seal());
    assert!(installed.agent_lock_spans_generation_and_unit_publication());

    for conflict in CrossProductInstallFixture::concurrent_operation_cases() {
        let result = conflict.run();
        assert_eq!(result.accepted_operation_count(), 1, "{}", conflict.label());
        assert_eq!(result.rejected_operation_count(), 1, "{}", conflict.label());
        assert!(!result.partial_publication_exists(), "{}", conflict.label());
    }
}

// AGL178-AGENT-TXN-003. Unmanaged units/drop-ins and loaded fragments are
// terminal conflicts rather than configuration silently preserved by an exact
// install.
#[test]
fn unmanaged_systemd_surfaces_are_rejected_before_publication() {
    for conflict in UnitConflict::all_terminal_and_agent_cases() {
        let fixture = CrossProductInstallFixture::canonical().with_unit_conflict(conflict);
        assert!(fixture.install().is_err(), "{}", fixture.label());
        assert!(!fixture.observation().any_generation_was_published());
    }
}

// AGL178-AGENT-TXN-004. Switching terminal generation invalidates the old
// pair until a newly sealed agent generation is selected.
#[test]
fn terminal_switch_requires_a_new_paired_agent_generation() {
    let fixture = CrossProductInstallFixture::canonical().install().unwrap();
    let switched = fixture.switch_terminal_generation_only().unwrap();
    assert!(switched.agent_start_is_rejected());
    assert!(switched.effect_is_rejected_before_dispatch());
    assert!(!switched.replacement_was_discovered());

    let repaired = switched.publish_new_paired_agent_generation().unwrap();
    assert!(repaired.exact_live_pair_is_admitted());
}

// AGL178-AGENT-UNINSTALL-001. Products uninstall independently, never delete a
// running generation, and terminal DB/spool remain durable in either order.
#[test]
fn independent_uninstall_orders_preserve_owners_and_running_bytes() {
    for order in [
        UninstallOrder::AgentThenTerminal,
        UninstallOrder::TerminalThenAgent,
    ] {
        let fixture = CrossProductInstallFixture::canonical().install().unwrap();
        let removed = fixture.uninstall(order).unwrap();
        assert!(!removed.deleted_running_generation());
        assert!(removed.terminal_database_retained());
        assert!(removed.terminal_spool_retained());
        assert!(removed.each_product_removed_only_owned_surfaces());
        assert!(removed.remaining_product_reports_typed_missing_pair());
        assert!(!removed.replacement_was_discovered());
    }
}
