use agl_matrix::{
    MatrixDeliveryResult, MatrixEnqueueError, MatrixOperationId, MatrixOutboxDraft, MatrixOutboxId,
    MatrixOutboxMachine, MatrixOutboxState,
};

fn operation(value: &str) -> MatrixOperationId {
    MatrixOperationId::new(value).unwrap()
}

fn draft(body: &str) -> MatrixOutboxDraft {
    MatrixOutboxDraft::new(
        "matrix-room:!room:example.org",
        "cron",
        "cron-run-1",
        "cron-run-1:terminal",
        body,
    )
    .unwrap()
}

#[test]
fn enqueue_exact_replay_and_changed_payload_conflict_are_distinct() {
    let machine = MatrixOutboxMachine::enqueue(
        MatrixOutboxId::new("matrix_outbox_1").unwrap(),
        draft("done"),
        10,
    );
    assert_eq!(
        machine.exact_replay(&draft("done")).unwrap().id,
        machine.record().id
    );

    let variants = [
        MatrixOutboxDraft::new(
            "matrix-room:!other:example.org",
            "cron",
            "cron-run-1",
            "cron-run-1:terminal",
            "done",
        )
        .unwrap(),
        MatrixOutboxDraft::new(
            "matrix-room:!room:example.org",
            "agent",
            "cron-run-1",
            "cron-run-1:terminal",
            "done",
        )
        .unwrap(),
        MatrixOutboxDraft::new(
            "matrix-room:!room:example.org",
            "cron",
            "cron-run-2",
            "cron-run-1:terminal",
            "done",
        )
        .unwrap(),
        draft("different"),
    ];
    for changed in variants {
        assert!(matches!(
            machine.exact_replay(&changed),
            Err(MatrixEnqueueError::IdempotencyConflict { .. })
        ));
    }
}

#[test]
fn lease_retry_and_terminal_transitions_are_fenced() {
    let mut machine = MatrixOutboxMachine::enqueue(
        MatrixOutboxId::new("matrix_outbox_2").unwrap(),
        draft("retry"),
        100,
    );
    assert!(
        machine
            .claim(operation("too-early"), "worker-a", 99, 150)
            .is_err()
    );

    let claimed = machine
        .claim(operation("claim-1"), "worker-a", 100, 150)
        .unwrap();
    assert!(matches!(
        claimed.new_state,
        MatrixOutboxState::Delivering {
            ref lease_owner,
            lease_expires_at_ms: 150,
            attempt: 1,
        } if lease_owner == "worker-a"
    ));
    let transaction_id = machine.record().transaction_id.clone();

    assert!(
        machine
            .complete(
                operation("wrong-owner"),
                "worker-b",
                MatrixDeliveryResult::Delivered,
            )
            .is_err()
    );
    let retried = machine
        .complete(
            operation("retry"),
            "worker-a",
            MatrixDeliveryResult::Retryable {
                not_before_ms: 200,
                error: "rate limited".to_owned(),
            },
        )
        .unwrap();
    assert!(matches!(
        retried.new_state,
        MatrixOutboxState::Queued { not_before_ms: 200 }
    ));
    assert_eq!(machine.record().transaction_id, transaction_id);

    machine
        .claim(operation("claim-2"), "worker-b", 200, 260)
        .unwrap();
    let sent = machine
        .complete(
            operation("sent"),
            "worker-b",
            MatrixDeliveryResult::Delivered,
        )
        .unwrap();
    assert_eq!(sent.new_state, MatrixOutboxState::Sent);
    assert_eq!(machine.record().transaction_id, transaction_id);
    assert!(
        machine
            .complete(
                operation("after-terminal"),
                "worker-b",
                MatrixDeliveryResult::Delivered,
            )
            .is_err()
    );
}

#[test]
fn expired_lease_requeues_with_same_remote_transaction_identity() {
    let mut machine = MatrixOutboxMachine::enqueue(
        MatrixOutboxId::new("matrix_outbox_3").unwrap(),
        draft("recover"),
        0,
    );
    machine
        .claim(operation("claim"), "crashed-worker", 0, 50)
        .unwrap();
    let transaction_id = machine.record().transaction_id.clone();
    assert!(machine.recover_expired(operation("early"), 49).is_err());
    let recovered = machine.recover_expired(operation("recover"), 50).unwrap();
    assert_eq!(
        recovered.new_state,
        MatrixOutboxState::Queued { not_before_ms: 50 }
    );
    assert_eq!(machine.record().transaction_id, transaction_id);
}

#[test]
fn permanent_failure_is_terminal() {
    let mut machine = MatrixOutboxMachine::enqueue(
        MatrixOutboxId::new("matrix_outbox_4").unwrap(),
        draft("invalid room"),
        0,
    );
    machine.claim(operation("claim"), "worker", 0, 50).unwrap();
    let failed = machine
        .complete(
            operation("fail"),
            "worker",
            MatrixDeliveryResult::Permanent {
                error: "room is invalid".to_owned(),
            },
        )
        .unwrap();
    assert!(matches!(
        failed.new_state,
        MatrixOutboxState::Failed { ref error } if error == "room is invalid"
    ));
    assert!(
        machine
            .claim(operation("retry"), "worker", 100, 150)
            .is_err()
    );
}
