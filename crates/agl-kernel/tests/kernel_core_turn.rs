#[path = "core/support/mod.rs"]
mod support;
#[path = "core/support/turn.rs"]
mod turn_support;

use agl_content::Content;
use agl_ids::TurnId;
use agl_kernel::{HookEvent, HookStatus};
use agl_kernel::{
    IncompleteOutputReason, ModelResponse, ModelResponseOutcome, StopReason, TurnHookBatch,
    TurnOutput,
};
use agl_kernel::{
    TurnAdvanceState, TurnMachine, TurnMachineError, TurnRequest, TurnRequestKey, TurnRequestKind,
    TurnRequestOutcome, TurnRequestResult, TurnTerminal,
};
use serde_json::{Value, json};
use turn_support::{
    RequestKind, Script, context_hook_batch, run_script, text, tool_call, turn_input,
    visible_read_tool,
};

// KCT-TURN-001. Mutation: keep continuation state outside the checkpoint.
#[test]
fn answer_path_is_identical_after_restore_at_every_request_boundary() {
    let uninterrupted = run_script(turn_input(), Script::default().model("done"), false);
    let restored = run_script(turn_input(), Script::default().model("done"), true);

    assert_eq!(uninterrupted, restored);
    assert_eq!(
        restored.terminal,
        TurnTerminal::Completed {
            output: TurnOutput::Answered {
                answer: "done".to_string(),
            },
        }
    );
    assert_eq!(
        restored.request_kinds,
        [RequestKind::ModelGeneration, RequestKind::TranscriptAppend,]
    );
}

// KCT-TURN-002. Mutation: require the outer driver to decide whether Tool output returns to model.
#[test]
fn tool_observation_returns_to_model_without_driver_policy() {
    let result = run_script(
        turn_input()
            .with_visible_tool(visible_read_tool())
            .with_max_tool_calls(1),
        Script::default()
            .model(tool_call(
                "core.workspace:fs.read",
                json!({"path": "README.md"}),
            ))
            .tool_result(json!({"text": "contents"}))
            .model("final"),
        true,
    );

    assert_eq!(
        result.request_kinds,
        [
            RequestKind::ModelGeneration,
            RequestKind::ToolDispatch,
            RequestKind::ModelGeneration,
            RequestKind::TranscriptAppend,
        ]
    );
    assert!(
        result
            .events
            .iter()
            .any(|event| event.kind() == "observation.appended")
    );
    assert!(matches!(
        result.terminal,
        TurnTerminal::Completed {
            output: TurnOutput::Answered { ref answer }
        } if answer == "final"
    ));
}

fn pending_model(machine: &mut TurnMachine) -> TurnRequest {
    let advance = machine.next_request().expect("first advance succeeds");
    let TurnAdvanceState::Pending { request } = advance.state else {
        panic!("first advance is not pending")
    };
    assert_eq!(request.kind(), TurnRequestKind::ModelGeneration);
    request
}

fn model_result(request: &TurnRequest, content: &str) -> TurnRequestResult {
    TurnRequestResult::ModelGeneration {
        key: request.key().clone(),
        outcome: TurnRequestOutcome::Succeeded(ModelResponse {
            content: text(content),
            outcome: ModelResponseOutcome::Complete,
        }),
    }
}

// KCT-REQ-001. Mutation: allocate a new key or provisional MessageId on repeated advance.
#[test]
fn repeated_advance_returns_the_identical_pending_request_without_events() {
    let mut machine = TurnMachine::new(turn_input());
    let first = machine.next_request().expect("first advance succeeds");
    let repeated = machine.next_request().expect("repeated advance succeeds");

    assert!(repeated.events.is_empty());
    assert_eq!(
        serde_json::to_vec(&first.state).unwrap(),
        serde_json::to_vec(&repeated.state).unwrap()
    );
}

// KCT-REQ-002 and KCT-TURN-004. Mutation: mutate state before rejecting a bad result.
#[test]
fn invalid_request_results_fail_without_changing_checkpoint_bytes() {
    let mut cases = Vec::new();

    let mut wrong_kind = TurnMachine::new(turn_input());
    let request = pending_model(&mut wrong_kind);
    cases.push((
        wrong_kind,
        TurnRequestResult::TranscriptAppend {
            key: request.key().clone(),
            outcome: TurnRequestOutcome::Succeeded(()),
        },
        "wrong kind",
    ));

    let mut foreign_turn = TurnMachine::new(turn_input());
    let request = pending_model(&mut foreign_turn);
    cases.push((
        foreign_turn,
        TurnRequestResult::ModelGeneration {
            key: TurnRequestKey {
                turn_id: TurnId::generate(),
                sequence: request.key().sequence,
            },
            outcome: TurnRequestOutcome::Succeeded(ModelResponse {
                content: text("foreign"),
                outcome: ModelResponseOutcome::Complete,
            }),
        },
        "foreign turn",
    ));

    let mut future = TurnMachine::new(turn_input());
    let request = pending_model(&mut future);
    cases.push((
        future,
        TurnRequestResult::ModelGeneration {
            key: TurnRequestKey {
                turn_id: request.key().turn_id.clone(),
                sequence: request.key().sequence + 1,
            },
            outcome: TurnRequestOutcome::Succeeded(ModelResponse {
                content: text("future"),
                outcome: ModelResponseOutcome::Complete,
            }),
        },
        "future sequence",
    ));

    for (mut machine, result, label) in cases {
        let before = serde_json::to_vec(&machine.checkpoint()).unwrap();
        assert!(machine.resume(result).is_err(), "accepted {label}");
        assert_eq!(
            serde_json::to_vec(&machine.checkpoint()).unwrap(),
            before,
            "checkpoint changed after rejecting {label}"
        );
    }
}

// KCT-REQ-002. Mutation: forget consumed request keys after a successful resume.
#[test]
fn consumed_request_result_is_rejected_exactly_once_without_state_change() {
    let mut machine = TurnMachine::new(turn_input());
    let request = pending_model(&mut machine);
    let result = model_result(&request, "done");
    machine
        .resume(result.clone())
        .expect("first result is accepted");
    let before = serde_json::to_vec(&machine.checkpoint()).unwrap();

    assert!(matches!(
        machine.resume(result),
        Err(TurnMachineError::DuplicateRequestKey(_))
    ));
    assert_eq!(serde_json::to_vec(&machine.checkpoint()).unwrap(), before);
}

// KCT-CHK-001 and KCT-CHK-002. Mutation: restore TurnContinuation or Hook repair state.
#[test]
fn checkpoint_is_strict_and_contains_only_one_turn_state_authority() {
    let mut machine = TurnMachine::new(turn_input());
    pending_model(&mut machine);
    let checkpoint = serde_json::to_value(machine.checkpoint()).unwrap();
    let object = checkpoint.as_object().expect("checkpoint is an object");

    for removed in [
        "phase",
        "executor_phase",
        "hook_repair_attempts",
        "pending_repair_message",
        "max_hook_repair_attempts",
    ] {
        assert!(
            !object.contains_key(removed),
            "checkpoint retains removed field {removed}: {checkpoint}"
        );
    }

    let bytes = serde_json::to_vec(&checkpoint).unwrap();
    let round_trip: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(round_trip, checkpoint);

    let mut unknown = checkpoint;
    unknown["legacy_state"] = json!(true);
    assert!(
        serde_json::from_value::<agl_kernel::TurnCheckpoint>(unknown).is_err(),
        "checkpoint accepted an unknown field"
    );
}

// KCT-HOOK-006. Mutation: issue a model request after Hook Repair.
#[test]
fn hook_repair_preserves_messages_and_stops_without_regeneration() {
    let input = turn_input().with_hook_batch(context_hook_batch([], []));
    let artifact = agl_kernel::TurnHookBatch::new(HookEvent::ArtifactWrite)
        .with_required_hook(support::hook_id("guard:repair"));
    let input = input.with_hook_batch(artifact);
    let result = run_script(
        input,
        Script::default().model("invalid answer").hook_status(
            HookEvent::ArtifactWrite,
            "guard:repair",
            HookStatus::Repair,
        ),
        true,
    );

    assert_eq!(
        result
            .request_kinds
            .iter()
            .filter(|kind| **kind == RequestKind::ModelGeneration)
            .count(),
        1,
        "Hook Repair scheduled another model request"
    );

    let terminal = serde_json::to_value(&result.terminal).unwrap();
    assert_eq!(
        terminal.pointer("/output/reason"),
        Some(&json!("repair_required")),
        "Hook Repair did not stop with repair_required: {terminal}"
    );
    let encoded = serde_json::to_string(&terminal).unwrap();
    for preserved in ["core.repair", "repair diagnostic", "repair guidance"] {
        assert!(
            encoded.contains(preserved),
            "Hook Repair lost {preserved:?}: {encoded}"
        );
    }
}

// Existing Tool JSON repair is explicitly separate from Hook Repair.
#[test]
fn tool_json_repair_remains_enabled_and_configurable() {
    let repaired = run_script(
        turn_input()
            .with_visible_tool(visible_read_tool())
            .with_max_tool_calls(1),
        Script::default()
            .model(
                r#"<tool_call>{"name":"core.workspace:fs.read","arguments":{"path":"README.md"}}"#,
            )
            .tool_result(json!({"ok": true}))
            .model("done"),
        true,
    );
    assert!(
        repaired
            .events
            .iter()
            .any(|event| event.kind() == "tool.json_repair_succeeded")
    );

    let stopped = run_script(
        turn_input().with_repair_malformed_tool_calls(false),
        Script::default().model("<tool_call>{bad"),
        true,
    );
    assert!(matches!(
        stopped.terminal,
        TurnTerminal::Completed {
            output: TurnOutput::Stopped {
                reason: StopReason::ToolJsonUnrepairable,
                ..
            }
        }
    ));
}

// KCT-TURN-003. Mutation: collapse typed model failure into a normal answer or stop.
#[test]
fn model_failure_remains_a_typed_terminal_failure() {
    let result = run_script(
        turn_input(),
        Script::default().model_failure(
            agl_kernel::TurnRequestFailureCode::Inference,
            "private backend detail",
        ),
        true,
    );
    assert!(matches!(result.terminal, TurnTerminal::Failed { .. }));
    assert!(
        !serde_json::to_string(&result.events)
            .unwrap()
            .contains("private backend detail")
    );
}

// KCT-TURN-003. Mutation: collapse Tool, Hook or transcript request failure into one code.
#[test]
fn every_external_request_failure_retains_its_typed_operation() {
    let tool = run_script(
        turn_input()
            .with_visible_tool(visible_read_tool())
            .with_max_tool_calls(1),
        Script::default()
            .model(tool_call(
                "core.workspace:fs.read",
                json!({"path": "README.md"}),
            ))
            .tool_failure("private Tool adapter detail"),
        true,
    );
    let hook = run_script(
        turn_input().with_hook_batch(
            TurnHookBatch::new(HookEvent::ContextPrepare)
                .with_required_hook(support::hook_id("guard:context")),
        ),
        Script::default().hook_failure("private Hook adapter detail"),
        true,
    );
    let transcript = run_script(
        turn_input(),
        Script::default()
            .model("done")
            .transcript_failure("private transcript adapter detail"),
        true,
    );

    for (result, expected_code, private_detail) in [
        (tool, "tool", "private Tool adapter detail"),
        (hook, "hook", "private Hook adapter detail"),
        (
            transcript,
            "transcript",
            "private transcript adapter detail",
        ),
    ] {
        let terminal = serde_json::to_value(&result.terminal).unwrap();
        assert_eq!(
            terminal.pointer("/failure/code"),
            Some(&json!(expected_code))
        );
        assert!(
            !serde_json::to_string(&result.events)
                .unwrap()
                .contains(private_detail),
            "private failure detail leaked into runtime events"
        );
    }
}

// KCT-TURN-003. Mutation: serialize incomplete output as an answer or skip a selected stop path.
#[test]
fn incomplete_and_stopped_outputs_remain_distinct_terminal_values() {
    let partial = "token ".repeat(64);
    let incomplete = run_script(
        turn_input(),
        Script::default().model_incomplete(partial.clone()),
        true,
    );
    assert_eq!(
        incomplete.terminal,
        TurnTerminal::Completed {
            output: TurnOutput::Incomplete {
                partial,
                reason: IncompleteOutputReason::ModelLength,
            },
        }
    );
    assert!(
        !incomplete
            .events
            .iter()
            .any(|event| event.kind() == "answer.final")
    );

    let hidden = run_script(
        turn_input()
            .with_visible_tool(visible_read_tool())
            .with_max_tool_calls(1),
        Script::default().model(tool_call("example.hidden:run", json!({}))),
        true,
    );
    assert!(matches!(
        hidden.terminal,
        TurnTerminal::Completed {
            output: TurnOutput::Stopped {
                reason: StopReason::HiddenTool,
                ..
            }
        }
    ));
    assert!(!hidden.request_kinds.contains(&RequestKind::ToolDispatch));
}

// KCT-REQ-003. Mutation: reset or reuse the request sequence after restore.
#[test]
fn request_keys_are_monotonic_across_every_request_kind_and_restore() {
    let input = turn_input()
        .with_visible_tool(visible_read_tool())
        .with_max_tool_calls(1)
        .with_hook_batch(
            TurnHookBatch::new(HookEvent::ContextPrepare)
                .with_required_hook(support::hook_id("guard:context")),
        )
        .with_hook_batch(
            TurnHookBatch::new(HookEvent::ToolCallBefore)
                .with_required_hook(support::hook_id("guard:before")),
        )
        .with_hook_batch(
            TurnHookBatch::new(HookEvent::ToolCallAfter)
                .with_required_hook(support::hook_id("guard:after")),
        );
    let result = run_script(
        input,
        Script::default()
            .model(tool_call(
                "core.workspace:fs.read",
                json!({"path": "README.md"}),
            ))
            .tool_result(json!({"text": "contents"}))
            .model("done"),
        true,
    );

    for pair in result.request_keys.windows(2) {
        assert_eq!(pair[1].turn_id, pair[0].turn_id);
        assert_eq!(pair[1].sequence, pair[0].sequence + 1);
    }
    for required in [
        RequestKind::HookBatch,
        RequestKind::ModelGeneration,
        RequestKind::ToolDispatch,
        RequestKind::TranscriptAppend,
    ] {
        assert!(
            result.request_kinds.contains(&required),
            "missing {required:?}"
        );
    }
}

// KCT-CANCEL-001. Mutation: continue after explicit cancellation or emit more work.
#[test]
fn cancellation_before_and_during_non_effectful_work_is_terminal() {
    let mut before = TurnMachine::new(turn_input());
    before.request_cancellation().unwrap();
    let advance = before.next_request().unwrap();
    assert!(matches!(
        advance.state,
        TurnAdvanceState::Terminal {
            terminal: TurnTerminal::Cancelled
        }
    ));
    assert_eq!(
        before.request_cancellation().unwrap_err(),
        TurnMachineError::AlreadyTerminal
    );

    let mut during_model = TurnMachine::new(turn_input());
    let request = pending_model(&mut during_model);
    let cancelled = during_model
        .resume(TurnRequestResult::ModelGeneration {
            key: request.key().clone(),
            outcome: TurnRequestOutcome::Cancelled,
        })
        .unwrap();
    assert!(matches!(
        cancelled.state,
        TurnAdvanceState::Terminal {
            terminal: TurnTerminal::Cancelled
        }
    ));

    let tool_cancelled = run_script(
        turn_input()
            .with_visible_tool(visible_read_tool())
            .with_max_tool_calls(1),
        Script::default()
            .model(tool_call(
                "core.workspace:fs.read",
                json!({"path": "README.md"}),
            ))
            .tool_cancelled(),
        true,
    );
    assert_eq!(tool_cancelled.terminal, TurnTerminal::Cancelled);
    assert_eq!(
        tool_cancelled.request_kinds.last(),
        Some(&RequestKind::ToolDispatch)
    );
}

// Ensure the test fixture itself uses text-only responses accepted by the parser.
#[test]
fn content_fixture_is_text_only() {
    let value = Content::text("fixture").unwrap();
    assert_eq!(value.text_only().as_deref(), Some("fixture"));
}
