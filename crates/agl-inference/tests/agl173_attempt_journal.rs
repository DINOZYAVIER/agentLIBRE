use agl_inference::test_support::{
    AttemptFixture, ExternalEffect, JournalFault, RejectionStage, RestartFixture,
};
use agl_inference::{
    AttemptJournal, InferenceAttemptOutcome, InferenceAttemptPhase, InferenceAttemptTransition,
};

// MIW-FSM-001. Success and bounded incomplete output traverse the same
// recorded pipeline but finish in distinct immutable phases.
#[test]
fn success_and_incomplete_output_have_distinct_terminal_phases() {
    for (outcome, expected) in [
        (
            InferenceAttemptOutcome::Succeeded,
            InferenceAttemptPhase::Succeeded,
        ),
        (
            InferenceAttemptOutcome::IncompleteOutput,
            InferenceAttemptPhase::IncompleteOutput,
        ),
    ] {
        let mut fixture = AttemptFixture::new();
        for transition in fixture.success_path(outcome) {
            fixture.record(transition).unwrap();
        }
        assert_eq!(fixture.machine().phase(), expected);
        let sequence = fixture.machine().sequence();
        assert!(
            fixture
                .record(InferenceAttemptTransition::FinishAttempt { outcome })
                .is_err()
        );
        assert_eq!(fixture.machine().phase(), expected);
        assert_eq!(fixture.machine().sequence(), sequence);
    }
}

// MIW-FSM-002. Failure and cancellation are separate recorded facts and only
// their matching FinishAttempt is legal.
#[test]
fn failure_and_cancellation_never_share_a_recorded_transition() {
    let mut failed = AttemptFixture::at_runtime_generation();
    failed
        .record(InferenceAttemptTransition::RecordFailure {
            failure: failed.typed_failure("engine_lost"),
        })
        .unwrap();
    assert_eq!(
        failed.machine().phase(),
        InferenceAttemptPhase::FailureRecorded
    );
    assert!(
        failed
            .record(InferenceAttemptTransition::FinishAttempt {
                outcome: InferenceAttemptOutcome::Cancelled,
            })
            .is_err()
    );
    failed
        .record(InferenceAttemptTransition::FinishAttempt {
            outcome: InferenceAttemptOutcome::Failed,
        })
        .unwrap();
    assert_eq!(failed.machine().phase(), InferenceAttemptPhase::Failed);

    let mut cancelled = AttemptFixture::at_runtime_generation();
    cancelled
        .record(InferenceAttemptTransition::RecordCancellation {
            cancellation: cancelled.cancellation("user"),
        })
        .unwrap();
    assert_eq!(
        cancelled.machine().phase(),
        InferenceAttemptPhase::CancellationRecorded
    );
    assert!(cancelled.journal().records().iter().all(|record| !matches!(
        record.transition(),
        InferenceAttemptTransition::RecordFailure { .. }
    )));
    cancelled
        .record(InferenceAttemptTransition::FinishAttempt {
            outcome: InferenceAttemptOutcome::Cancelled,
        })
        .unwrap();
    assert_eq!(
        cancelled.machine().phase(),
        InferenceAttemptPhase::Cancelled
    );
}

// MIW-FSM-003. Illegal, repeated, skipped and post-terminal inputs are checked
// before canonical phase or sequence changes.
#[test]
fn rejected_transitions_do_not_mutate_the_machine() {
    for mut fixture in AttemptFixture::illegal_transition_cases() {
        let before = fixture.machine().clone();
        let before_records = fixture.journal().records().to_vec();
        assert!(fixture.record(fixture.illegal_transition()).is_err());
        assert_eq!(fixture.machine(), &before);
        assert_eq!(fixture.journal().records(), before_records);
    }
}

// MIW-FSM-004. Every accepted-command rejection is inside the same durable
// attempt identity rather than escaping before IDs/evidence exist.
#[test]
fn every_pre_dispatch_rejection_has_attempt_identity_and_durable_failure() {
    for stage in [
        RejectionStage::Plan,
        RejectionStage::Content,
        RejectionStage::Descriptor,
        RejectionStage::Lease,
        RejectionStage::Admission,
        RejectionStage::Queue,
        RejectionStage::Dispatch,
    ] {
        let fixture = AttemptFixture::rejected_at(stage);
        let terminal = fixture.journal().records().last().unwrap();
        assert_eq!(terminal.run_id(), fixture.run_id());
        assert_eq!(terminal.attempt_id(), fixture.attempt_id());
        assert_eq!(fixture.machine().phase(), InferenceAttemptPhase::Failed);
        assert_eq!(
            fixture.runtime_resolution().terminal_attempt_id(),
            fixture.attempt_id()
        );
        assert_eq!(fixture.runtime_resolution().rejection_stage(), Some(stage));
    }
}

// MIW-JRN-001. Append+sync is the commit point; failure leaves state unchanged
// and prevents the next external action.
#[test]
fn journal_sync_precedes_state_advance_and_external_effects() {
    for fault in JournalFault::every_append_boundary() {
        let mut fixture = AttemptFixture::new();
        fixture.journal_mut().inject_fault(fault);
        let before = fixture.machine().clone();
        assert!(fixture.record(fixture.next_legal_transition()).is_err());
        assert_eq!(fixture.machine(), &before);
        assert!(fixture.observed_effects().is_empty());
    }

    let mut fixture = AttemptFixture::new();
    fixture.record(fixture.next_legal_transition()).unwrap();
    assert_eq!(
        fixture.ordering(),
        &[
            ExternalEffect::JournalAppend,
            ExternalEffect::JournalSync,
            ExternalEffect::WriteRequest
        ]
    );
}

// MIW-JRN-002. Replay after every legal transition is byte-equivalent to the
// live machine identity, phase and sequence.
#[test]
fn every_legal_prefix_replays_exactly() {
    let mut fixture = AttemptFixture::new();
    for transition in fixture.success_path(InferenceAttemptOutcome::Succeeded) {
        fixture.record(transition).unwrap();
        let restored = AttemptJournal::replay(fixture.journal().bytes()).unwrap();
        assert_eq!(restored.machine(), fixture.machine());
        assert_eq!(restored.journal_bytes(), fixture.journal().bytes());
    }
}

// MIW-JRN-003. Events and runtime-resolution are projections. Missing/stale
// files rebuild from the journal; projection failure keeps the record and
// blocks its following effect.
#[test]
fn projections_rebuild_from_journal_and_never_become_authority() {
    let mut fixture = AttemptFixture::at_admitted();
    let expected = fixture.canonical_projection_bytes();
    fixture.delete_projections();
    fixture.rebuild_projections().unwrap();
    assert_eq!(fixture.projection_bytes(), expected);

    fixture.write_stale_projections();
    fixture.rebuild_projections().unwrap();
    assert_eq!(fixture.projection_bytes(), expected);

    let mut failing = AttemptFixture::at_content_ready();
    failing.fail_next_projection();
    let records_before = failing.journal().records().len();
    assert!(failing.record(failing.admission_grant()).is_err());
    assert_eq!(failing.journal().records().len(), records_before + 1);
    assert!(
        !failing
            .observed_effects()
            .contains(&ExternalEffect::Dispatch)
    );
}

// MIW-JRN-004. Restart never resumes native generation. Each nonterminal phase
// is reaped and closed once as a typed failed attempt.
#[test]
fn restart_fails_every_nonterminal_phase_without_generation_retry() {
    for phase in AttemptFixture::nonterminal_phases() {
        let mut fixture = RestartFixture::at_phase(phase);
        fixture.restart().unwrap();
        assert_eq!(fixture.machine().phase(), InferenceAttemptPhase::Failed);
        assert_eq!(fixture.engine_reap_count(), 1);
        assert_eq!(fixture.engine_generation_start_count(), 0);
        assert_eq!(fixture.failed_terminal_count(), 1);
    }
}

// MIW-JRN-005. Resolution evidence carries exact authority identities for
// success and every failure before native load.
#[test]
fn runtime_resolution_contains_complete_plan_and_live_evidence() {
    for fixture in AttemptFixture::resolution_evidence_cases() {
        let resolution = fixture.runtime_resolution();
        assert_eq!(resolution.run_id(), fixture.run_id());
        assert_eq!(resolution.attempt_id(), fixture.attempt_id());
        assert_eq!(resolution.plan_digest(), fixture.plan_digest());
        assert_eq!(resolution.package_refs(), fixture.package_refs());
        assert_eq!(resolution.profile_id(), fixture.profile_id());
        assert_eq!(
            resolution.artifact_descriptors(),
            fixture.descriptor_identities()
        );
        assert_eq!(
            resolution.resource_components(),
            fixture.resource_components()
        );
        assert_eq!(resolution.reservation(), fixture.reservation_identity());
        assert_eq!(resolution.allocation_receipt(), fixture.receipt_identity());
        assert_eq!(resolution.terminal_phase(), fixture.machine().phase());
    }
}
