use agl_ids::{MessageId, RunId, TurnId};
use agl_kernel::{
    ModelRequest, RunBudget, RunBudgetError, RunBudgetLedger, RunChildReservationRequest,
    RunChildUsageCommit, RunDelivery, RunMachine, RunMachineError, RunOperationId, RunRequest,
    RunRequestResult, RunState, RunStepMachine, RunStepMachineError, RunStepOperationId,
    RunStepState, RunTerminalOutcome, TurnRequest, TurnRequestKey, TurnRequestOutcome,
    TurnRequestResult,
};

fn model_request(sequence: u64, delivery: RunDelivery) -> RunRequest {
    let turn_id = TurnId::generate();
    RunRequest::new(
        delivery,
        TurnRequest::ModelGeneration {
            key: TurnRequestKey {
                turn_id: turn_id.clone(),
                sequence,
            },
            provisional_message_id: MessageId::generate(),
            request: ModelRequest {
                run_id: RunId::generate(),
                turn_id,
                request_index: 0,
                messages: Vec::new(),
                visible_tools: Vec::new(),
            },
        },
    )
}

fn cancelled_result(request: &RunRequest) -> RunRequestResult {
    RunRequestResult::new(TurnRequestResult::ModelGeneration {
        key: request.key().clone(),
        outcome: TurnRequestOutcome::Cancelled,
    })
}

#[test]
fn run_machine_owns_exact_transition_table_and_terminal_immutability() {
    let mut machine = RunMachine::new();
    assert_eq!(machine.state(), None);

    machine
        .admit(RunOperationId::new("admit"))
        .expect("admission is legal");
    assert_eq!(machine.state(), Some(RunState::Queued));
    assert_eq!(machine.revision().get(), 1);

    machine
        .claim(RunOperationId::new("claim"))
        .expect("queued run can be claimed");
    machine
        .request_cancellation(RunOperationId::new("cancel"))
        .expect("running cancellation is recorded without inventing an outcome");
    assert_eq!(machine.state(), Some(RunState::Running));
    assert!(machine.cancellation_requested());

    let terminal = RunTerminalOutcome::Cancelled {
        error_code: None,
        error_message: None,
    };
    let accepted = machine
        .finish(RunOperationId::new("finish"), terminal.clone())
        .expect("driver terminal input closes the run");
    assert_eq!(accepted.to, RunState::Cancelled);
    let terminal_revision = machine.revision();

    let replay = machine
        .finish(RunOperationId::new("finish"), terminal)
        .expect("exact duplicate terminal operation is idempotent");
    assert_eq!(replay, accepted);
    assert_eq!(machine.revision(), terminal_revision);

    assert!(matches!(
        machine.request_cancellation(RunOperationId::new("late-cancel")),
        Err(RunMachineError::TerminalImmutable(RunState::Cancelled))
    ));
    assert_eq!(machine.revision(), terminal_revision);
}

#[test]
fn run_machine_rejects_illegal_and_conflicting_replays_without_revision_change() {
    let mut machine = RunMachine::new();
    assert!(matches!(
        machine.claim(RunOperationId::new("claim-before-admit")),
        Err(RunMachineError::InvalidTransition { .. })
    ));
    assert_eq!(machine.revision().get(), 0);

    machine.admit(RunOperationId::new("op")).unwrap();
    let revision = machine.revision();
    assert!(matches!(
        machine.claim(RunOperationId::new("op")),
        Err(RunMachineError::OperationConflict { .. })
    ));
    assert_eq!(machine.revision(), revision);
}

#[test]
fn step_machine_preserves_typed_request_identity_across_retry_and_recovery() {
    let request = model_request(7, RunDelivery::Idempotent);
    let mut step = RunStepMachine::new();
    step.publish(RunStepOperationId::new("publish"), request.clone())
        .unwrap();
    step.claim(RunStepOperationId::new("claim-1"), 3).unwrap();
    step.retry(RunStepOperationId::new("retry"), "temporary", true, 3)
        .unwrap();
    assert_eq!(step.state(), Some(RunStepState::Pending));
    assert_eq!(step.request(), Some(&request));

    step.claim(RunStepOperationId::new("claim-2"), 3).unwrap();
    step.recover_expired_lease(RunStepOperationId::new("recover"))
        .unwrap();
    assert_eq!(step.state(), Some(RunStepState::Pending));
    assert_eq!(step.request(), Some(&request));
}

#[test]
fn at_most_once_recovery_is_terminal_outcome_unknown() {
    let request = model_request(1, RunDelivery::AtMostOnce);
    let mut step = RunStepMachine::new();
    step.publish(RunStepOperationId::new("publish"), request)
        .unwrap();
    step.claim(RunStepOperationId::new("claim"), 2).unwrap();
    step.recover_expired_lease(RunStepOperationId::new("recover"))
        .unwrap();
    assert_eq!(step.state(), Some(RunStepState::OutcomeUnknown));
    let revision = step.revision();
    assert!(matches!(
        step.retry(
            RunStepOperationId::new("retry-after-unknown"),
            "cannot know",
            true,
            2,
        ),
        Err(RunStepMachineError::TerminalImmutable(
            RunStepState::OutcomeUnknown
        ))
    ));
    assert_eq!(step.revision(), revision);
}

#[test]
fn step_completion_requires_exact_request_result_identity() {
    let request = model_request(9, RunDelivery::ReplaySafe);
    let mut step = RunStepMachine::new();
    step.publish(RunStepOperationId::new("publish"), request.clone())
        .unwrap();
    step.claim(RunStepOperationId::new("claim"), 1).unwrap();

    let other = model_request(10, RunDelivery::ReplaySafe);
    assert!(
        RunRequestResult::for_request(&request, cancelled_result(&other).into_inner()).is_err()
    );
    let result = cancelled_result(&request);
    step.complete(
        RunStepOperationId::new("complete"),
        RunStepState::Cancelled,
        Some(result),
    )
    .unwrap();
    assert_eq!(step.state(), Some(RunStepState::Cancelled));
}

#[test]
fn budget_ledger_is_monotonic_checked_and_keeps_actual_overrun() {
    let mut ledger = RunBudgetLedger::new(RunBudget {
        wall_time_ms: 100,
        model_input_tokens: 10,
        model_output_tokens: 10,
        model_attempts: 2,
        tool_calls: 2,
    });
    ledger
        .observe_usage(
            "usage-1",
            agl_kernel::RunUsage {
                wall_time_ms: 80,
                model_input_tokens: 8,
                model_output_tokens: 8,
                model_attempts: 1,
                tool_calls: 1,
            },
        )
        .unwrap();
    let accepted = ledger
        .observe_usage(
            "usage-2",
            agl_kernel::RunUsage {
                wall_time_ms: 120,
                model_input_tokens: 8,
                model_output_tokens: 14,
                model_attempts: 1,
                tool_calls: 1,
            },
        )
        .unwrap();
    assert_eq!(accepted.usage.wall_time_ms, 120);
    assert_eq!(accepted.usage.model_output_tokens, 14);
    assert!(!accepted.exhausted.is_empty());
    assert!(matches!(
        ledger.authorize_model_request(),
        Err(RunBudgetError::BudgetExhausted { .. })
    ));

    let before = ledger.usage().clone();
    assert!(matches!(
        ledger.observe_usage("usage-3", agl_kernel::RunUsage::default()),
        Err(RunBudgetError::UsageDecreased { .. })
    ));
    assert_eq!(ledger.usage(), &before);
}

#[test]
fn budget_ledger_clamps_child_and_commits_actual_overrun_once() {
    let limits = RunBudget {
        wall_time_ms: 100,
        model_input_tokens: 100,
        model_output_tokens: 100,
        model_attempts: 10,
        tool_calls: 10,
    };
    let usage = agl_kernel::RunUsage {
        wall_time_ms: 20,
        model_input_tokens: 30,
        model_output_tokens: 40,
        model_attempts: 2,
        tool_calls: 3,
    };
    let mut ledger = RunBudgetLedger::restore(limits, usage)
        .with_delegated_output(10, 5)
        .unwrap();
    let request = RunChildReservationRequest {
        reservation_id: "child-1".to_owned(),
        requested: RunBudget {
            wall_time_ms: 90,
            model_input_tokens: 90,
            model_output_tokens: 90,
            model_attempts: 9,
            tool_calls: 9,
        },
        tree_wall_time_remaining_ms: 60,
        tree_output_tokens_remaining: 30,
    };
    let reservation = ledger.reserve_child("reserve-1", request.clone()).unwrap();
    assert_eq!(reservation.effective_budget.wall_time_ms, 60);
    assert_eq!(reservation.effective_budget.model_input_tokens, 70);
    assert_eq!(reservation.effective_budget.model_output_tokens, 30);
    assert_eq!(reservation.effective_budget.model_attempts, 8);
    assert_eq!(reservation.effective_budget.tool_calls, 7);
    assert_eq!(
        ledger.reserve_child("reserve-1", request).unwrap(),
        reservation
    );

    let commit = RunChildUsageCommit {
        reservation_id: "child-1".to_owned(),
        reserved_output_tokens: 30,
        actual_output_tokens: 60,
    };
    let accepted = ledger
        .commit_child_usage("commit-1", commit.clone())
        .unwrap();
    assert_eq!(accepted.released_output_tokens, 30);
    assert_eq!(accepted.committed_output_tokens, 60);
    assert_eq!(
        ledger.commit_child_usage("commit-1", commit).unwrap(),
        accepted
    );
    assert!(matches!(
        ledger.authorize_model_request(),
        Err(RunBudgetError::BudgetExhausted { .. })
    ));
}

#[test]
fn budget_ledger_rejects_under_reservation_and_conflicting_replay_without_mutation() {
    let limits = RunBudget {
        wall_time_ms: 100,
        model_input_tokens: 100,
        model_output_tokens: 100,
        model_attempts: 10,
        tool_calls: 10,
    };
    let mut ledger = RunBudgetLedger::restore(limits, agl_kernel::RunUsage::default())
        .with_delegated_output(4, 0)
        .unwrap();
    assert!(matches!(
        ledger.commit_child_usage(
            "commit",
            RunChildUsageCommit {
                reservation_id: "child".to_owned(),
                reserved_output_tokens: 5,
                actual_output_tokens: 1,
            },
        ),
        Err(RunBudgetError::UnderReserved { .. })
    ));

    let request = RunChildReservationRequest {
        reservation_id: "new-child".to_owned(),
        requested: RunBudget {
            wall_time_ms: 10,
            model_input_tokens: 10,
            model_output_tokens: 10,
            model_attempts: 1,
            tool_calls: 1,
        },
        tree_wall_time_remaining_ms: 10,
        tree_output_tokens_remaining: 10,
    };
    ledger.reserve_child("reserve", request.clone()).unwrap();
    let mut mismatch = request;
    mismatch.requested.model_output_tokens = 9;
    assert!(matches!(
        ledger.reserve_child("reserve", mismatch),
        Err(RunBudgetError::OperationConflict { .. })
    ));
}

#[test]
fn budget_ledger_overflow_does_not_release_the_child_reservation() {
    let limits = RunBudget {
        wall_time_ms: u64::MAX,
        model_input_tokens: u64::MAX,
        model_output_tokens: u64::MAX,
        model_attempts: u32::MAX,
        tool_calls: u32::MAX,
    };
    let mut ledger = RunBudgetLedger::restore(limits, agl_kernel::RunUsage::default())
        .with_delegated_output(5, u64::MAX - 5)
        .unwrap();
    assert!(matches!(
        ledger.commit_child_usage(
            "overflow",
            RunChildUsageCommit {
                reservation_id: "child".to_owned(),
                reserved_output_tokens: 5,
                actual_output_tokens: 6,
            },
        ),
        Err(RunBudgetError::ArithmeticOverflow { .. })
    ));
    ledger
        .commit_child_usage(
            "commit",
            RunChildUsageCommit {
                reservation_id: "child".to_owned(),
                reserved_output_tokens: 5,
                actual_output_tokens: 5,
            },
        )
        .unwrap();
}
