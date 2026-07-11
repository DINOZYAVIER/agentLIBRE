use agl_actions::ToolCall;
use agl_capabilities::{ActionDeclaration, ActionResult, CapabilityId, OperationKind};
use agl_content::Content;
use agl_ids::{RunId, TurnId};
use serde_json::json;

use crate::policy::{ToolCallDecision, ToolCallStop, decide_tool_call};
use crate::*;

const TEST_RUN_ID: &str = "run_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b3c";
const TEST_TURN_ID: &str = "turn_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b3d";

fn run_id() -> RunId {
    RunId::parse(TEST_RUN_ID).unwrap()
}

fn turn_id() -> TurnId {
    TurnId::parse(TEST_TURN_ID).unwrap()
}

fn test_input(user_input: impl Into<String>) -> TurnInput {
    TurnInput::user(run_id(), turn_id(), text(user_input))
}

fn text(value: impl Into<String>) -> Content {
    Content::text(value).unwrap()
}

fn test_machine() -> TurnMachine {
    TurnMachine::new(run_id(), turn_id())
}

fn read_file_tool() -> VisibleTool {
    let declaration = ActionDeclaration::new(
        CapabilityId::new("read_file").unwrap(),
        "Read a file",
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        }),
        OperationKind::Read,
    )
    .unwrap();
    VisibleTool::from_declaration(&declaration)
}

fn tool_call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.to_string(),
        arguments,
    }
}

fn hook_id(value: &str) -> HookId {
    HookId::new(value).unwrap()
}

fn response_guard_batch() -> TurnHookBatch {
    TurnHookBatch::new(HookEvent::ModelResponse)
        .with_required_hook(hook_id("guard.response_required"))
        .with_optional_hook(hook_id("guard.response_optional"))
}

fn artifact_guard_batch() -> TurnHookBatch {
    TurnHookBatch::new(HookEvent::ArtifactWrite).with_required_hook(hook_id("guard.artifact"))
}

#[test]
fn stop_reason_names_are_stable() {
    assert_eq!(
        StopReason::ToolJsonUnrepairable.as_str(),
        "tool_json_unrepairable"
    );
    assert_eq!(StopReason::ToolLimitReached.as_str(), "tool_limit_reached");
    assert_eq!(StopReason::HiddenTool.as_str(), "hidden_tool");
    assert_eq!(
        StopReason::InvalidToolArguments.as_str(),
        "invalid_tool_arguments"
    );
}

#[test]
fn initializes_turn_state_with_user_message() {
    let state = TurnState::new(test_input("hello"));

    assert_eq!(state.request_index, 0);
    assert_eq!(state.tool_call_count, 0);
    assert_eq!(
        state.messages,
        [TurnMessage::User {
            content: text("hello"),
        }]
    );
}

#[test]
fn initializes_turn_state_with_context_and_request_index() {
    let state = TurnState::new(
        test_input("new")
            .with_context_messages(vec![
                TurnMessage::User {
                    content: text("old"),
                },
                TurnMessage::Assistant {
                    content: text("previous"),
                },
            ])
            .with_request_index_start(7),
    );

    assert_eq!(state.input.run_id, run_id());
    assert_eq!(state.input.turn_id, turn_id());
    assert_eq!(state.request_index, 7);
    assert_eq!(
        state.messages,
        [
            TurnMessage::User {
                content: text("old"),
            },
            TurnMessage::Assistant {
                content: text("previous"),
            },
            TurnMessage::User {
                content: text("new"),
            },
        ]
    );
}

#[test]
fn initializes_turn_state_with_hook_batches() {
    let hook_batch =
        TurnHookBatch::new(HookEvent::TurnFinish).with_required_hook(hook_id("guard.answer"));
    let state = TurnState::new(test_input("new").with_hook_batch(hook_batch.clone()));

    assert_eq!(state.input.hook_batches, [hook_batch]);
}

#[test]
fn hook_batch_summary_serializes_without_hook_message_content() {
    let batch = response_guard_batch();
    let summary = HookBatchSummary::from_batch_result(
        &batch,
        HookBatchResult {
            event: HookEvent::ModelResponse,
            results: vec![HookResult {
                hook_id: hook_id("guard.response_required"),
                status: HookStatus::Warn,
                messages: vec![HookMessage {
                    code: "response.too_long".to_string(),
                    message: "secret response text".to_string(),
                    fix: Some("secret fix text".to_string()),
                }],
            }],
        },
        Some(7),
    );

    let json = serde_json::to_string(&summary).unwrap();

    assert!(json.contains(r#""event":"model.response""#), "{json}");
    assert!(json.contains("guard.response_required"), "{json}");
    assert!(json.contains("response.too_long"), "{json}");
    assert!(json.contains(r#""outcome":"warn""#), "{json}");
    assert!(!json.contains("secret response text"), "{json}");
    assert!(!json.contains("secret fix text"), "{json}");
}

#[test]
fn policy_dispatches_visible_tool_with_schema_validated_arguments() {
    let state = TurnState::new(
        test_input("read README")
            .with_visible_tool(read_file_tool())
            .with_max_tool_calls(1),
    );

    let decision = decide_tool_call(
        &state,
        &tool_call("read_file", json!({"path": "README.MD"})),
    );

    assert_eq!(
        decision,
        ToolCallDecision::Dispatch(ToolDispatchRequest {
            run_id: run_id(),
            turn_id: turn_id(),
            capability_id: CapabilityId::new("read_file").unwrap(),
            arguments: json!({"path": "README.MD"}),
        })
    );
}

#[test]
fn policy_stops_at_tool_limit_before_visibility_check() {
    let state = TurnState::new(test_input("read README").with_max_tool_calls(0));

    let decision = decide_tool_call(&state, &tool_call("hidden_tool", json!({})));

    assert_eq!(
        decision,
        ToolCallDecision::Stop(ToolCallStop::ToolLimitReached { limit: 0 })
    );
}

#[test]
fn policy_stops_hidden_tool_before_dispatch() {
    let state = TurnState::new(
        test_input("write README")
            .with_visible_tool(read_file_tool())
            .with_max_tool_calls(1),
    );

    let decision = decide_tool_call(
        &state,
        &tool_call("write_file", json!({"path": "README.MD"})),
    );

    assert_eq!(
        decision,
        ToolCallDecision::Stop(ToolCallStop::HiddenTool {
            name: "write_file".to_string(),
        })
    );
}

#[test]
fn policy_stops_invalid_tool_arguments_before_dispatch() {
    let state = TurnState::new(
        test_input("read README")
            .with_visible_tool(read_file_tool())
            .with_max_tool_calls(1),
    );

    let decision = decide_tool_call(
        &state,
        &tool_call("read_file", json!({"other": "README.MD"})),
    );

    assert_eq!(
        decision,
        ToolCallDecision::Stop(ToolCallStop::InvalidArguments {
            name: "read_file".to_string(),
            message: "action arguments failed schema validation; /: Additional properties are not allowed ('other' was unexpected); /: \"path\" is a required property".to_string(),
        })
    );
}

#[test]
fn append_tool_result_records_assistant_tool_pair() {
    let mut state = TurnState::new(test_input("read README"));

    state.append_tool_result(
        tool_call("read_file", json!({"path": "README.MD"})),
        ActionResult::new(json!({"text": "agentLIBRE readme"})),
    );

    assert_eq!(state.tool_call_count, 1);
    assert_eq!(state.messages.len(), 3);
    assert_eq!(
        state.messages[1],
        TurnMessage::AssistantToolCall {
            name: "read_file".to_string(),
            arguments: json!({"path": "README.MD"}),
        }
    );
    assert_eq!(
        state.messages[2],
        TurnMessage::ToolObservation {
            name: "read_file".to_string(),
            result: ActionResult::new(json!({"text": "agentLIBRE readme"})),
        }
    );
}

fn apply(machine: &mut TurnMachine, transition: TurnTransition) -> TurnPhase {
    machine.apply(transition).unwrap().to
}

#[test]
fn turn_machine_accepts_answer_path() {
    let mut machine = test_machine();

    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::Start {
                user_input: text("hello"),
            },
        ),
        TurnPhase::Started
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::PrepareModelRequest { message_count: 1 },
        ),
        TurnPhase::ModelRequestPrepared
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::RequestModel { request_index: 1 },
        ),
        TurnPhase::AwaitingModel
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::ReceiveModelResponse {
                request_index: 1,
                content: text("done"),
            },
        ),
        TurnPhase::ModelResponded
    );
    assert_eq!(
        apply(&mut machine, TurnTransition::ParseAnswer),
        TurnPhase::ActionParsed
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::FinalAnswer {
                answer: "done".to_string(),
            },
        ),
        TurnPhase::AnswerReady
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::Finish {
                status: TurnTerminalStatus::Answered,
            },
        ),
        TurnPhase::Finished
    );
    assert_eq!(machine.sequence(), 7);
}

#[test]
fn turn_machine_accepts_tool_loop_back_to_model() {
    let mut machine = test_machine();

    apply(
        &mut machine,
        TurnTransition::Start {
            user_input: text("read"),
        },
    );
    apply(
        &mut machine,
        TurnTransition::PrepareModelRequest { message_count: 1 },
    );
    apply(
        &mut machine,
        TurnTransition::RequestModel { request_index: 1 },
    );
    apply(
        &mut machine,
        TurnTransition::ReceiveModelResponse {
            request_index: 1,
            content: text(tool_call("read_file", json!({"path": "README.MD"})).name),
        },
    );
    apply(
        &mut machine,
        TurnTransition::ParseToolCall {
            name: "read_file".to_string(),
        },
    );
    apply(
        &mut machine,
        TurnTransition::ValidateToolArgs {
            name: "read_file".to_string(),
            arguments: json!({"path": "README.MD"}),
        },
    );
    apply(
        &mut machine,
        TurnTransition::StartToolCall {
            name: "read_file".to_string(),
            arguments: json!({"path": "README.MD"}),
        },
    );
    apply(
        &mut machine,
        TurnTransition::FinishToolCall {
            name: "read_file".to_string(),
            result: ActionResult::new(json!({"text": "readme"})),
        },
    );

    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::AppendObservation {
                name: "read_file".to_string(),
                result: ActionResult::new(json!({"text": "readme"})),
            },
        ),
        TurnPhase::ObservationAppended
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::RequestModel { request_index: 2 },
        ),
        TurnPhase::AwaitingModel
    );
}

#[test]
fn turn_machine_accepts_repaired_malformed_tool_json() {
    let mut machine = test_machine();

    apply(
        &mut machine,
        TurnTransition::Start {
            user_input: text("read"),
        },
    );
    apply(
        &mut machine,
        TurnTransition::PrepareModelRequest { message_count: 1 },
    );
    apply(
        &mut machine,
        TurnTransition::RequestModel { request_index: 1 },
    );
    apply(
        &mut machine,
        TurnTransition::ReceiveModelResponse {
            request_index: 1,
            content: text("<tool_call>"),
        },
    );
    apply(
        &mut machine,
        TurnTransition::DetectMalformedToolJson {
            classification: ToolJsonMalformedClassification::Syntax,
            raw_json: "{bad".to_string(),
        },
    );
    apply(
        &mut machine,
        TurnTransition::AttemptToolJsonRepair {
            strategy: "quoted_json_string".to_string(),
        },
    );
    apply(
        &mut machine,
        TurnTransition::SucceedToolJsonRepair {
            strategy: "quoted_json_string".to_string(),
            repaired_json: "{}".to_string(),
        },
    );

    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::ParseToolCall {
                name: "read_file".to_string(),
            },
        ),
        TurnPhase::ActionParsed
    );
}

#[test]
fn turn_machine_accepts_hook_batch_before_model_response_parse() {
    let mut machine = test_machine();
    let prepared = response_guard_batch().summary();
    let finished = HookBatchSummary::from_batch_result(
        &response_guard_batch(),
        HookBatchResult {
            event: HookEvent::ModelResponse,
            results: vec![
                HookResult {
                    hook_id: hook_id("guard.response_required"),
                    status: HookStatus::Pass,
                    messages: Vec::new(),
                },
                HookResult {
                    hook_id: hook_id("guard.response_optional"),
                    status: HookStatus::Warn,
                    messages: vec![HookMessage {
                        code: "style.warning".to_string(),
                        message: "non-secret diagnostic".to_string(),
                        fix: None,
                    }],
                },
            ],
        },
        Some(3),
    );

    apply(
        &mut machine,
        TurnTransition::Start {
            user_input: text("answer"),
        },
    );
    apply(
        &mut machine,
        TurnTransition::PrepareModelRequest { message_count: 1 },
    );
    apply(
        &mut machine,
        TurnTransition::RequestModel { request_index: 1 },
    );
    apply(
        &mut machine,
        TurnTransition::ReceiveModelResponse {
            request_index: 1,
            content: text("done"),
        },
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::PrepareHookBatch {
                summary: prepared.clone(),
            },
        ),
        TurnPhase::HookBatchPrepared
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::RunHookBatch { summary: prepared },
        ),
        TurnPhase::HookBatchRunning
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::FinishHookBatch { summary: finished },
        ),
        TurnPhase::ModelResponded
    );
    assert_eq!(
        apply(&mut machine, TurnTransition::ParseAnswer),
        TurnPhase::ActionParsed
    );
}

#[test]
fn turn_machine_accepts_artifact_write_hook_before_finish() {
    let mut machine = test_machine();
    let batch = artifact_guard_batch();
    let prepared = batch.summary();
    let finished = HookBatchSummary::from_batch_result(
        &batch,
        HookBatchResult {
            event: HookEvent::ArtifactWrite,
            results: vec![HookResult {
                hook_id: hook_id("guard.artifact"),
                status: HookStatus::Pass,
                messages: Vec::new(),
            }],
        },
        Some(1),
    );

    apply(
        &mut machine,
        TurnTransition::Start {
            user_input: text("answer"),
        },
    );
    apply(
        &mut machine,
        TurnTransition::PrepareModelRequest { message_count: 1 },
    );
    apply(
        &mut machine,
        TurnTransition::RequestModel { request_index: 0 },
    );
    apply(
        &mut machine,
        TurnTransition::ReceiveModelResponse {
            request_index: 0,
            content: text("done"),
        },
    );
    apply(&mut machine, TurnTransition::ParseAnswer);
    apply(
        &mut machine,
        TurnTransition::FinalAnswer {
            answer: "done".to_string(),
        },
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::PrepareHookBatch {
                summary: prepared.clone(),
            },
        ),
        TurnPhase::HookBatchPrepared
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::RunHookBatch { summary: prepared },
        ),
        TurnPhase::HookBatchRunning
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::FinishHookBatch { summary: finished },
        ),
        TurnPhase::AnswerReady
    );
}

#[test]
fn turn_machine_rejects_illegal_hook_transitions() {
    let mut machine = test_machine();
    let summary = response_guard_batch().summary();

    let err = machine
        .apply(TurnTransition::RunHookBatch { summary })
        .unwrap_err();

    assert_eq!(err.phase, TurnPhase::Initialized);
    assert_eq!(err.transition, "run_hook_batch");
}

#[test]
fn turn_machine_accepts_failed_required_hook_terminal_path() {
    let mut machine = test_machine();
    let batch = response_guard_batch();
    let prepared = batch.summary();
    let failed = HookBatchSummary::from_batch_result(
        &batch,
        HookBatchResult {
            event: HookEvent::ModelResponse,
            results: vec![HookResult {
                hook_id: hook_id("guard.response_required"),
                status: HookStatus::Fail,
                messages: vec![HookMessage {
                    code: "response.blocked".to_string(),
                    message: "unsafe text".to_string(),
                    fix: None,
                }],
            }],
        },
        Some(2),
    );

    apply(
        &mut machine,
        TurnTransition::Start {
            user_input: text("answer"),
        },
    );
    apply(
        &mut machine,
        TurnTransition::PrepareModelRequest { message_count: 1 },
    );
    apply(
        &mut machine,
        TurnTransition::RequestModel { request_index: 1 },
    );
    apply(
        &mut machine,
        TurnTransition::ReceiveModelResponse {
            request_index: 1,
            content: text("done"),
        },
    );
    apply(
        &mut machine,
        TurnTransition::PrepareHookBatch {
            summary: prepared.clone(),
        },
    );
    apply(
        &mut machine,
        TurnTransition::RunHookBatch { summary: prepared },
    );
    apply(
        &mut machine,
        TurnTransition::FinishHookBatch {
            summary: failed.clone(),
        },
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::RejectHookFailure {
                summary: failed,
                message: "required hook failed".to_string(),
            },
        ),
        TurnPhase::Failed
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::Finish {
                status: TurnTerminalStatus::Failed,
            },
        ),
        TurnPhase::Finished
    );
}

#[test]
fn turn_machine_accepts_stopped_tool_path() {
    let mut machine = test_machine();

    apply(
        &mut machine,
        TurnTransition::Start {
            user_input: text("read"),
        },
    );
    apply(
        &mut machine,
        TurnTransition::PrepareModelRequest { message_count: 1 },
    );
    apply(
        &mut machine,
        TurnTransition::RequestModel { request_index: 1 },
    );
    apply(
        &mut machine,
        TurnTransition::ReceiveModelResponse {
            request_index: 1,
            content: text("tool"),
        },
    );
    apply(
        &mut machine,
        TurnTransition::ParseToolCall {
            name: "read_file".to_string(),
        },
    );
    apply(&mut machine, TurnTransition::RejectToolLimit { limit: 0 });
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::Stop {
                reason: StopReason::ToolLimitReached,
                visible: true,
            },
        ),
        TurnPhase::Stopped
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::Finish {
                status: TurnTerminalStatus::Stopped,
            },
        ),
        TurnPhase::Finished
    );
}

#[test]
fn turn_machine_accepts_model_failure_path() {
    let mut machine = test_machine();

    apply(
        &mut machine,
        TurnTransition::Start {
            user_input: text("hello"),
        },
    );
    apply(
        &mut machine,
        TurnTransition::PrepareModelRequest { message_count: 1 },
    );
    apply(
        &mut machine,
        TurnTransition::RequestModel { request_index: 1 },
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::Fail {
                operation: TurnFailureOperation::ModelRequest { request_index: 1 },
                message: "backend failed".to_string(),
            },
        ),
        TurnPhase::Failed
    );
    assert_eq!(
        apply(
            &mut machine,
            TurnTransition::Finish {
                status: TurnTerminalStatus::Failed,
            },
        ),
        TurnPhase::Finished
    );
}

#[test]
fn turn_machine_rejects_illegal_transition_and_finished_is_terminal() {
    let mut machine = test_machine();

    let err = machine
        .apply(TurnTransition::RequestModel { request_index: 1 })
        .unwrap_err();
    assert_eq!(err.phase, TurnPhase::Initialized);
    assert_eq!(err.transition, "request_model");

    apply(
        &mut machine,
        TurnTransition::Start {
            user_input: text("hello"),
        },
    );
    apply(
        &mut machine,
        TurnTransition::PrepareModelRequest { message_count: 1 },
    );
    apply(
        &mut machine,
        TurnTransition::RequestModel { request_index: 1 },
    );
    apply(
        &mut machine,
        TurnTransition::Fail {
            operation: TurnFailureOperation::ModelRequest { request_index: 1 },
            message: "backend failed".to_string(),
        },
    );
    apply(
        &mut machine,
        TurnTransition::Finish {
            status: TurnTerminalStatus::Failed,
        },
    );

    let err = machine
        .apply(TurnTransition::RequestModel { request_index: 2 })
        .unwrap_err();
    assert_eq!(err.phase, TurnPhase::Finished);
    assert_eq!(err.transition, "request_model");
}
