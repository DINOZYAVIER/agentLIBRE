use agl_ids::RunId;
use agl_kernel::RunState;
use agl_permission::{
    PermissionDuration, PermissionGrantMachine, PermissionGrantState, PermissionMachineError,
    PermissionOperationId, PermissionRequestMachine, PermissionRequestResolution,
    PermissionRequestState, permission_grant_is_live_for_run,
};

fn operation(value: &str) -> PermissionOperationId {
    PermissionOperationId::new(value).unwrap()
}

#[test]
fn request_resolution_is_strict_terminal_and_replay_safe() {
    for (operation_id, resolution, expected) in [
        (
            "grant",
            PermissionRequestResolution::Granted,
            PermissionRequestState::Granted,
        ),
        (
            "deny",
            PermissionRequestResolution::Denied,
            PermissionRequestState::Denied,
        ),
        (
            "revoke",
            PermissionRequestResolution::Revoked,
            PermissionRequestState::Revoked,
        ),
    ] {
        let mut machine = PermissionRequestMachine::new();
        let accepted = machine
            .resolve(operation(operation_id), resolution)
            .unwrap();
        assert_eq!(accepted.previous_state, PermissionRequestState::Pending);
        assert_eq!(accepted.new_state, expected);
        assert_eq!(accepted.previous_revision.get(), 1);
        assert_eq!(accepted.new_revision.get(), 2);

        let replay = machine
            .resolve(operation(operation_id), resolution)
            .unwrap();
        assert_eq!(replay, accepted);
        assert_eq!(machine.revision().get(), 2);

        assert!(matches!(
            machine.resolve(operation("different-operation"), resolution),
            Err(PermissionMachineError::TerminalRequest { .. })
        ));
        let changed_resolution = if resolution == PermissionRequestResolution::Denied {
            PermissionRequestResolution::Granted
        } else {
            PermissionRequestResolution::Denied
        };
        assert!(matches!(
            machine.resolve(operation(operation_id), changed_resolution),
            Err(PermissionMachineError::IdempotencyConflict { .. })
        ));
    }
}

#[test]
fn one_turn_grant_is_consumed_for_exact_run_not_expired() {
    let run_id = RunId::generate();
    let mut machine = PermissionGrantMachine::new(PermissionDuration::OneTurn);
    let accepted = machine.admit(operation("admit"), run_id.clone()).unwrap();
    assert_eq!(
        accepted.new_state,
        PermissionGrantState::Consumed {
            run_id: run_id.clone()
        }
    );
    assert_eq!(accepted.new_revision.get(), 2);

    let replay = machine.admit(operation("admit"), run_id.clone()).unwrap();
    assert_eq!(replay, accepted);
    assert!(matches!(
        machine.revoke(operation("revoke")),
        Err(PermissionMachineError::TerminalGrant { .. })
    ));
}

#[test]
fn session_admission_remains_active_and_exact_replay_does_not_advance_revision() {
    let first_run = RunId::generate();
    let second_run = RunId::generate();
    let mut machine = PermissionGrantMachine::new(PermissionDuration::Session);

    let first = machine
        .admit(operation("first"), first_run.clone())
        .unwrap();
    assert_eq!(first.new_state, PermissionGrantState::Active);
    assert_eq!(first.new_revision.get(), 2);
    assert_eq!(
        machine
            .admit(operation("first"), first_run)
            .unwrap()
            .new_revision
            .get(),
        2
    );

    let second = machine.admit(operation("second"), second_run).unwrap();
    assert_eq!(second.new_state, PermissionGrantState::Active);
    assert_eq!(second.new_revision.get(), 3);

    let expired = machine.expire(operation("session-end")).unwrap();
    assert_eq!(expired.new_state, PermissionGrantState::Expired);
    assert!(matches!(
        machine.admit(operation("after-end"), RunId::generate()),
        Err(PermissionMachineError::TerminalGrant { .. })
    ));
}

#[test]
fn consumed_grant_lifetime_uses_typed_run_state_and_exact_identity() {
    let run_id = RunId::generate();
    let other_run_id = RunId::generate();
    let state = PermissionGrantState::Consumed {
        run_id: run_id.clone(),
    };

    for live in [RunState::Queued, RunState::Running, RunState::Waiting] {
        assert!(permission_grant_is_live_for_run(
            &state,
            Some((&run_id, live))
        ));
    }
    for terminal in [
        RunState::Succeeded,
        RunState::Incomplete,
        RunState::Failed,
        RunState::Cancelled,
    ] {
        assert!(!permission_grant_is_live_for_run(
            &state,
            Some((&run_id, terminal))
        ));
    }
    assert!(!permission_grant_is_live_for_run(&state, None));
    assert!(!permission_grant_is_live_for_run(
        &state,
        Some((&other_run_id, RunState::Running))
    ));
}
