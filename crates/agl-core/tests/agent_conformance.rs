use std::num::NonZeroU32;

use agl_core::Fsm;
use agl_core::agent::{
    AgentCheckpoint, AgentEventData, AgentFsm, AgentFsmInput, AgentFsmState, AgentOperation,
    AgentOperationDeliveryState, AgentOperationFsm, AgentOperationFsmInput, AgentOperationKey,
    AgentOperationRequest, AgentOperationResult, AgentRunFailureKind, AgentRunLimits,
    AgentRunStatus, AgentRunUsage, DeliveryClass, ModelGenerationRequest, ToolResult,
};
use agl_core::{AgentRunId, ConversationId, MessageId};

fn limits() -> AgentRunLimits {
    AgentRunLimits {
        deadline_ms: i64::MAX,
        model_input_tokens: Some(100),
        model_output_tokens: 100,
        model_calls: 2,
        correction_input_tokens: 100,
        correction_output_tokens: 100,
        correction_calls: 2,
        tool_calls: 2,
        tool_result_bytes: 65_536,
    }
}

#[test]
fn absent_input_limit_removes_only_the_aggregate_input_cap() {
    for (input_limit, output_tokens, model_calls, expected) in [
        (None, 0, 0, AgentRunStatus::Running),
        (Some(1_000_000), 0, 0, AgentRunStatus::Failed),
        (
            None,
            limits().model_output_tokens,
            0,
            AgentRunStatus::Failed,
        ),
        (None, 0, limits().model_calls, AgentRunStatus::Failed),
    ] {
        let mut configured = limits();
        configured.model_input_tokens = input_limit;
        let machine = AgentFsm::new(AgentRunId::generate(), None, configured, 100, &[]);
        let state = AgentFsmState {
            status: AgentRunStatus::Running,
            checkpoint: AgentCheckpoint::Ready {
                context: vec![MessageId::generate()],
                next_operation_ordinal: NonZeroU32::MIN,
            },
            usage: AgentRunUsage {
                model_input_tokens: 2_000_000,
                model_output_tokens: output_tokens,
                model_calls,
                ..AgentRunUsage::default()
            },
        };
        let next = machine.transition(&state, AgentFsmInput::Drive).unwrap();
        assert_eq!(next.state.status, expected);
        assert_eq!(next.state.usage.model_input_tokens, 2_000_000);
    }
}

#[test]
fn operation_fsm_rejects_a_result_of_the_wrong_kind() {
    let operation = running_operation(DeliveryClass::Retryable);
    assert!(
        AgentOperationFsm::for_operation(&operation)
            .transition(
                &operation.state,
                AgentOperationFsmInput::Result {
                    key: operation.key.clone(),
                    delivery_attempt: operation.delivery_attempt,
                    result: AgentOperationResult::Tool(ToolResult {
                        content: agl_core::Content::text("wrong result kind").unwrap(),
                        effect_receipts: vec![],
                    }),
                },
            )
            .is_err()
    );
}

#[test]
fn exhausted_limits_are_a_terminal_transition_not_a_stuck_ready_run() {
    let run_id = AgentRunId::generate();
    let machine = AgentFsm::new(run_id, None, limits(), 100, &[]);
    let state = AgentFsmState {
        status: AgentRunStatus::Running,
        checkpoint: AgentCheckpoint::Ready {
            context: vec![MessageId::generate()],
            next_operation_ordinal: NonZeroU32::MIN,
        },
        usage: AgentRunUsage {
            model_calls: limits().model_calls,
            ..AgentRunUsage::default()
        },
    };
    let transitioned = machine.transition(&state, AgentFsmInput::Drive).unwrap();
    assert_eq!(transitioned.state.status, AgentRunStatus::Failed);
    assert!(transitioned.output.operation.is_none());
    assert!(transitioned.output.events.iter().any(|event| matches!(
        event,
        agl_core::agent::AgentEventData::RunStatusChanged {
            failure_kind: Some(AgentRunFailureKind::LimitsExceeded),
            ..
        }
    )));
}

fn running_operation(delivery: DeliveryClass) -> AgentOperation {
    AgentOperation {
        key: AgentOperationKey {
            run_id: AgentRunId::generate(),
            ordinal: NonZeroU32::MIN,
        },
        request: AgentOperationRequest::ModelGeneration(ModelGenerationRequest {
            context: vec![MessageId::generate()],
            max_output_tokens: 10,
        }),
        delivery,
        delivery_attempt: NonZeroU32::MIN,
        state: AgentOperationDeliveryState::Running,
        result: None,
        failure: None,
        revision: 0,
    }
}

#[test]
fn recovery_distinguishes_retryable_and_at_most_once_delivery() {
    let operation = running_operation(DeliveryClass::Retryable);
    let machine = AgentOperationFsm::for_operation(&operation);
    let retryable = machine
        .transition(
            &operation.state,
            AgentOperationFsmInput::Recover {
                key: operation.key.clone(),
                delivery_attempt: operation.delivery_attempt,
            },
        )
        .unwrap();
    assert_eq!(retryable.state, AgentOperationDeliveryState::RetryScheduled);
    let retryable_operation = operation
        .apply_fsm_transition(retryable.state, &retryable.output)
        .unwrap();
    let retried = machine
        .transition(
            &retryable_operation.state,
            AgentOperationFsmInput::RetryDue {
                key: retryable_operation.key.clone(),
                delivery_attempt: retryable_operation.delivery_attempt,
                request: retryable_operation.request.clone(),
            },
        )
        .unwrap();
    let retried_operation = retryable_operation
        .apply_fsm_transition(retried.state, &retried.output)
        .unwrap();
    assert_eq!(retried_operation.delivery_attempt.get(), 2);
    assert_eq!(retried.state, AgentOperationDeliveryState::Running);

    let operation = running_operation(DeliveryClass::AtMostOnce);
    let at_most_once = AgentOperationFsm::for_operation(&operation)
        .transition(
            &operation.state,
            AgentOperationFsmInput::Recover {
                key: operation.key.clone(),
                delivery_attempt: operation.delivery_attempt,
            },
        )
        .unwrap();
    assert_eq!(
        at_most_once.state,
        AgentOperationDeliveryState::OutcomeUnknown
    );
}

#[test]
fn interrupted_tool_is_unknown_even_when_its_declaration_is_retryable() {
    let mut operation = running_operation(DeliveryClass::Retryable);
    operation.request = AgentOperationRequest::Tool(agl_core::agent::ToolRequest {
        tool_id: agl_core::ToolId::new("fixture:tool").unwrap(),
        input: serde_json::json!({}),
    });
    let recovered = AgentOperationFsm::for_operation(&operation)
        .transition(
            &operation.state,
            AgentOperationFsmInput::Recover {
                key: operation.key.clone(),
                delivery_attempt: operation.delivery_attempt,
            },
        )
        .unwrap();
    assert_eq!(recovered.state, AgentOperationDeliveryState::OutcomeUnknown);
    assert!(recovered.output.dispatch.is_none());
    let persisted = operation
        .apply_fsm_transition(recovered.state, &recovered.output)
        .unwrap();
    assert_eq!(persisted.delivery_attempt.get(), 1);
    assert_eq!(
        persisted.failure.unwrap().kind,
        agl_core::agent::AgentOperationFailureKind::OutcomeUnknown
    );
}

#[test]
fn operation_entity_rejects_wrong_identity_and_retry_timestamp() {
    let operation = running_operation(DeliveryClass::Retryable);
    let machine = AgentOperationFsm::for_operation(&operation);
    assert!(
        machine
            .transition(
                &operation.state,
                AgentOperationFsmInput::Recover {
                    key: AgentOperationKey {
                        run_id: AgentRunId::generate(),
                        ordinal: operation.key.ordinal,
                    },
                    delivery_attempt: operation.delivery_attempt,
                },
            )
            .is_err()
    );
    assert!(
        machine
            .transition(
                &operation.state,
                AgentOperationFsmInput::Failure {
                    key: operation.key.clone(),
                    delivery_attempt: operation.delivery_attempt,
                    failure: agl_core::agent::AgentOperationFailure {
                        kind: agl_core::agent::AgentOperationFailureKind::Unavailable,
                    },
                    retry_at_ms: Some(-1),
                },
            )
            .is_err()
    );

    let retry = machine
        .transition(
            &operation.state,
            AgentOperationFsmInput::Recover {
                key: operation.key.clone(),
                delivery_attempt: operation.delivery_attempt,
            },
        )
        .unwrap();
    let mut forged = retry.output;
    forged.events = vec![AgentEventData::OperationRetryScheduled {
        key: AgentOperationKey {
            run_id: AgentRunId::generate(),
            ordinal: NonZeroU32::MIN,
        },
        delivery_attempt: operation.delivery_attempt,
        retry_at_ms: 0,
    }];
    assert!(
        operation
            .apply_fsm_transition(retry.state, &forged)
            .is_err()
    );
}

#[test]
fn agent_fsm_rejects_a_different_operation_identity() {
    let run_id = AgentRunId::generate();
    let input_message = MessageId::generate();
    let machine = AgentFsm::new(run_id, Some(ConversationId::generate()), limits(), 100, &[]);
    let ready = AgentFsmState {
        status: AgentRunStatus::Pending,
        checkpoint: AgentCheckpoint::Ready {
            context: vec![input_message],
            next_operation_ordinal: NonZeroU32::MIN,
        },
        usage: AgentRunUsage::default(),
    };
    let waiting = machine.transition(&ready, AgentFsmInput::Drive).unwrap();
    let mut foreign = waiting.output.operation.unwrap();
    foreign.key.run_id = AgentRunId::generate();
    foreign.state = AgentOperationDeliveryState::Succeeded;
    assert!(
        machine
            .transition(
                &waiting.state,
                AgentFsmInput::OperationFinished {
                    operation: foreign,
                    message_id: Some(MessageId::generate()),
                },
            )
            .is_err()
    );
}
