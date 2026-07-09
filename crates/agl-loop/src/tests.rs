use crate::*;
use agl_events::AgentEvent;
use anyhow::{Result, anyhow};
use serde_json::json;

enum FakeModelResult {
    Response(String),
    Error(String),
}

enum FakeToolResult {
    Observation(String),
    Error(String),
}

enum FakeHookResult {
    Batch(HookBatchResult),
}

#[derive(Default)]
struct FakeHost {
    model_results: Vec<FakeModelResult>,
    tool_results: Vec<FakeToolResult>,
    hook_results: Vec<FakeHookResult>,
    requests: Vec<&'static str>,
    operations: Vec<String>,
    events: Vec<AgentEvent>,
    transitions: Vec<TurnTransitionRecord>,
    model_requests: Vec<ModelRequest>,
    dispatches: Vec<ToolDispatchRequest>,
    hook_requests: Vec<HookBatchRequest>,
    turn_messages: Vec<TurnMessage>,
}

impl FakeHost {
    fn with_model_response(mut self, response: impl Into<String>) -> Self {
        self.model_results
            .push(FakeModelResult::Response(response.into()));
        self
    }

    fn with_model_error(mut self, message: impl Into<String>) -> Self {
        self.model_results
            .push(FakeModelResult::Error(message.into()));
        self
    }

    fn with_tool_observation(mut self, observation: impl Into<String>) -> Self {
        self.tool_results
            .push(FakeToolResult::Observation(observation.into()));
        self
    }

    fn with_tool_error(mut self, message: impl Into<String>) -> Self {
        self.tool_results
            .push(FakeToolResult::Error(message.into()));
        self
    }

    fn with_hook_result(mut self, result: HookBatchResult) -> Self {
        self.hook_results.push(FakeHookResult::Batch(result));
        self
    }

    fn request_kinds(&self) -> Vec<&'static str> {
        self.requests.clone()
    }

    fn event_kinds(&self) -> Vec<&'static str> {
        self.events.iter().map(AgentEvent::kind).collect()
    }

    fn transition_kinds(&self) -> Vec<&'static str> {
        self.transitions
            .iter()
            .map(|record| record.transition.as_str())
            .collect()
    }

    fn operation_kinds(&self) -> Vec<&str> {
        self.operations.iter().map(String::as_str).collect()
    }
}

impl AgentLoopHost for FakeHost {
    fn run_hooks(&mut self, request: HookBatchRequest) -> Result<HookBatchResult> {
        self.requests.push("run_hooks");
        self.operations
            .push(format!("run_hooks:{}", request.event.as_str()));
        self.hook_requests.push(request);
        match self.hook_results.remove(0) {
            FakeHookResult::Batch(result) => Ok(result),
        }
    }

    fn generate(&mut self, request: ModelRequest) -> Result<ModelResponse> {
        self.requests.push("generate");
        self.operations.push("generate".to_string());
        self.model_requests.push(request);
        match self.model_results.remove(0) {
            FakeModelResult::Response(content) => Ok(ModelResponse { content }),
            FakeModelResult::Error(message) => Err(anyhow!(message)),
        }
    }

    fn dispatch_tool(&mut self, request: ToolDispatchRequest) -> Result<ToolDispatchResponse> {
        self.requests.push("dispatch_tool");
        self.operations.push("dispatch_tool".to_string());
        self.dispatches.push(request);
        match self.tool_results.remove(0) {
            FakeToolResult::Observation(observation) => Ok(ToolDispatchResponse { observation }),
            FakeToolResult::Error(message) => Err(anyhow!(message)),
        }
    }

    fn record_turn_messages(&mut self, messages: &[TurnMessage]) -> Result<()> {
        self.turn_messages = messages.to_vec();
        Ok(())
    }

    fn emit_transition(&mut self, record: &TurnTransitionRecord, event: &AgentEvent) -> Result<()> {
        self.operations
            .push(format!("transition:{}", record.transition.as_str()));
        self.transitions.push(record.clone());
        self.events.push(event.clone());
        Ok(())
    }
}

fn read_file_tool() -> VisibleTool {
    VisibleTool::new("read_file").require_argument("path")
}

fn tool_call(path: &str) -> String {
    format!(r#"<tool_call>{{"name":"read_file","arguments":{{"path":"{path}"}}}}</tool_call>"#)
}

fn hook_id(value: &str) -> HookId {
    HookId::new(value).unwrap()
}

fn finish_hook_batch() -> TurnHookBatch {
    TurnHookBatch::new(HookEvent::TurnFinish).with_required_hook(hook_id("guard.answer"))
}

fn artifact_write_hook_batch() -> TurnHookBatch {
    TurnHookBatch::new(HookEvent::ArtifactWrite).with_required_hook(hook_id("guard.artifact"))
}

fn hook_message(code: &str) -> HookMessage {
    HookMessage {
        code: code.to_string(),
        message: "hidden hook diagnostic".to_string(),
        fix: Some("hidden hook fix".to_string()),
    }
}

fn hook_result(id: &str, status: HookStatus, codes: &[&str]) -> HookResult {
    HookResult {
        hook_id: hook_id(id),
        status,
        messages: codes.iter().map(|code| hook_message(code)).collect(),
    }
}

fn hook_batch_result(
    event: HookEvent,
    results: impl IntoIterator<Item = HookResult>,
) -> HookBatchResult {
    HookBatchResult {
        event,
        results: results.into_iter().collect(),
    }
}

#[test]
fn required_turn_finish_hook_pass_allows_answer() {
    let mut host = FakeHost::default()
        .with_model_response("done")
        .with_hook_result(hook_batch_result(
            HookEvent::TurnFinish,
            [hook_result("guard.answer", HookStatus::Pass, &[])],
        ));
    let input = TurnInput::user("answer").with_hook_batch(finish_hook_batch());

    let output = run_turn(&mut host, input).unwrap();

    assert_eq!(
        output,
        TurnOutput::Answered {
            answer: "done".to_string()
        }
    );
    assert_eq!(host.request_kinds(), ["generate", "run_hooks"]);
    assert_eq!(host.hook_requests[0].event, HookEvent::TurnFinish);
    assert_eq!(host.hook_requests[0].hooks, [hook_id("guard.answer")]);
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "model.request_prepared",
            "model.requested",
            "model.response_received",
            "model.action_parsed",
            "answer.final",
            "hook.batch_prepared",
            "hook.batch_started",
            "hook.batch_finished",
            "turn.finished",
        ]
    );
    assert_eq!(
        host.operation_kinds(),
        [
            "transition:start",
            "transition:prepare_model_request",
            "transition:request_model",
            "generate",
            "transition:receive_model_response",
            "transition:parse_answer",
            "transition:final_answer",
            "transition:prepare_hook_batch",
            "transition:run_hook_batch",
            "run_hooks:turn.finish",
            "transition:finish_hook_batch",
            "transition:finish",
        ]
    );
}

#[test]
fn required_artifact_write_hook_runs_before_answer_is_accepted() {
    let mut host = FakeHost::default()
        .with_model_response("done")
        .with_hook_result(hook_batch_result(
            HookEvent::ArtifactWrite,
            [hook_result("guard.artifact", HookStatus::Pass, &[])],
        ));
    let input = TurnInput::user("answer").with_hook_batch(artifact_write_hook_batch());

    let output = run_turn(&mut host, input).unwrap();

    assert_eq!(
        output,
        TurnOutput::Answered {
            answer: "done".to_string()
        }
    );
    assert_eq!(host.request_kinds(), ["generate", "run_hooks"]);
    assert_eq!(host.hook_requests[0].event, HookEvent::ArtifactWrite);
    assert_eq!(
        host.operation_kinds(),
        [
            "transition:start",
            "transition:prepare_model_request",
            "transition:request_model",
            "generate",
            "transition:receive_model_response",
            "transition:parse_answer",
            "transition:final_answer",
            "transition:prepare_hook_batch",
            "transition:run_hook_batch",
            "run_hooks:artifact.write",
            "transition:finish_hook_batch",
            "transition:finish",
        ]
    );
}

#[test]
fn warning_turn_finish_hook_continues_and_records_warning() {
    let mut host = FakeHost::default()
        .with_model_response("done")
        .with_hook_result(hook_batch_result(
            HookEvent::TurnFinish,
            [hook_result(
                "guard.answer",
                HookStatus::Warn,
                &["answer.warning"],
            )],
        ));
    let input = TurnInput::user("answer").with_hook_batch(finish_hook_batch());

    let output = run_turn(&mut host, input).unwrap();

    assert_eq!(
        output,
        TurnOutput::Answered {
            answer: "done".to_string()
        }
    );
    assert!(matches!(
        host.events
            .iter()
            .find(|event| event.kind() == "hook.batch_finished"),
        Some(AgentEvent::HookBatchFinished {
            outcome: agl_events::HookBatchOutcomeEvent::Warn,
            warning_count: 1,
            message_codes,
            ..
        }) if message_codes == &["answer.warning".to_string()]
    ));
}

#[test]
fn repairs_answer_when_required_hook_requests_repair() {
    let mut host = FakeHost::default()
        .with_model_response("function=wrong")
        .with_hook_result(hook_batch_result(
            HookEvent::ArtifactWrite,
            [hook_result(
                "guard.artifact",
                HookStatus::Repair,
                &["runtime_identity_mismatch"],
            )],
        ))
        .with_model_response("function=repo-analyst")
        .with_hook_result(hook_batch_result(
            HookEvent::ArtifactWrite,
            [hook_result("guard.artifact", HookStatus::Pass, &[])],
        ));
    let input = TurnInput::user("what is loaded?")
        .with_hook_batch(artifact_write_hook_batch())
        .with_max_hook_repair_attempts(1);

    let output = run_turn(&mut host, input).unwrap();

    assert_eq!(
        output,
        TurnOutput::Answered {
            answer: "function=repo-analyst".to_string()
        }
    );
    assert_eq!(
        host.request_kinds(),
        ["generate", "run_hooks", "generate", "run_hooks"]
    );
    assert!(
        host.transition_kinds()
            .iter()
            .any(|transition| *transition == "prepare_repair")
    );
    assert_eq!(
        host.turn_messages,
        vec![
            TurnMessage::User {
                content: "what is loaded?".to_string()
            },
            TurnMessage::Assistant {
                content: "function=repo-analyst".to_string()
            }
        ]
    );
    assert!(host.model_requests[1].messages.iter().any(|message| {
        matches!(
            message,
            TurnMessage::System { content }
                if content.contains("runtime_identity_mismatch")
                    && content.contains("hidden hook fix")
        )
    }));
}

#[test]
fn failed_required_turn_finish_hook_fails_closed_before_accepting_answer() {
    let mut host = FakeHost::default()
        .with_model_response("blocked answer")
        .with_hook_result(hook_batch_result(
            HookEvent::TurnFinish,
            [hook_result(
                "guard.answer",
                HookStatus::Fail,
                &["answer.blocked"],
            )],
        ));
    let input = TurnInput::user("answer").with_hook_batch(finish_hook_batch());

    let err = run_turn(&mut host, input).unwrap_err();

    assert!(format!("{err:#}").contains("required hook batch `turn.finish` failed"));
    assert_eq!(host.request_kinds(), ["generate", "run_hooks"]);
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "model.request_prepared",
            "model.requested",
            "model.response_received",
            "model.action_parsed",
            "answer.final",
            "hook.batch_prepared",
            "hook.batch_started",
            "hook.batch_finished",
            "hook.batch_blocked",
            "turn.finished",
        ]
    );
    assert!(matches!(
        host.events.last(),
        Some(AgentEvent::TurnFinished {
            status: agl_events::TurnFinishStatus::Failed,
            ..
        })
    ));
}

#[test]
fn missing_required_turn_finish_hook_fails_closed() {
    let mut host = FakeHost::default()
        .with_model_response("done")
        .with_hook_result(hook_batch_result(
            HookEvent::TurnFinish,
            Vec::<HookResult>::new(),
        ));
    let input = TurnInput::user("answer").with_hook_batch(finish_hook_batch());

    let err = run_turn(&mut host, input).unwrap_err();

    assert!(format!("{err:#}").contains("required hook batch `turn.finish` failed"));
    assert!(matches!(
        host.events
            .iter()
            .find(|event| event.kind() == "hook.batch_finished"),
        Some(AgentEvent::HookBatchFinished {
            outcome: agl_events::HookBatchOutcomeEvent::Fail,
            failed_required_count: 0,
            missing_required_hooks,
            ..
        }) if missing_required_hooks == &["guard.answer".to_string()]
    ));
    assert!(matches!(
        host.events
            .iter()
            .find(|event| event.kind() == "hook.batch_blocked"),
        Some(AgentEvent::HookBatchBlocked {
            missing_required_hooks,
            ..
        }) if missing_required_hooks == &["guard.answer".to_string()]
    ));
}

#[test]
fn answers_without_tools_when_model_returns_plain_text() {
    let mut host = FakeHost::default().with_model_response("done");
    let output = run_turn(&mut host, TurnInput::user("answer")).unwrap();

    assert_eq!(
        output,
        TurnOutput::Answered {
            answer: "done".to_string()
        }
    );
    assert_eq!(host.request_kinds(), ["generate"]);
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "model.request_prepared",
            "model.requested",
            "model.response_received",
            "model.action_parsed",
            "answer.final",
            "turn.finished",
        ]
    );
    assert_eq!(
        host.transition_kinds(),
        [
            "start",
            "prepare_model_request",
            "request_model",
            "receive_model_response",
            "parse_answer",
            "final_answer",
            "finish",
        ]
    );
    assert_eq!(
        host.operation_kinds(),
        [
            "transition:start",
            "transition:prepare_model_request",
            "transition:request_model",
            "generate",
            "transition:receive_model_response",
            "transition:parse_answer",
            "transition:final_answer",
            "transition:finish",
        ]
    );
}

#[test]
fn runs_one_tool_then_answers_with_observation() {
    let mut host = FakeHost::default()
        .with_model_response(tool_call("README.MD"))
        .with_tool_observation("agentLIBRE readme")
        .with_model_response("README says agentLIBRE.");
    let input = TurnInput::user("read README")
        .with_visible_tool(read_file_tool())
        .with_max_tool_calls(1);

    let output = run_turn(&mut host, input).unwrap();

    assert_eq!(
        output,
        TurnOutput::Answered {
            answer: "README says agentLIBRE.".to_string()
        }
    );
    assert_eq!(
        host.request_kinds(),
        ["generate", "dispatch_tool", "generate"]
    );
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "model.request_prepared",
            "model.requested",
            "model.response_received",
            "model.action_parsed",
            "tool.args_validated",
            "tool.call_started",
            "tool.call_finished",
            "observation.appended",
            "model.requested",
            "model.response_received",
            "model.action_parsed",
            "answer.final",
            "turn.finished",
        ]
    );
    assert_eq!(host.dispatches[0].name, "read_file");
    assert_eq!(host.dispatches[0].arguments, json!({"path": "README.MD"}));
    assert_eq!(
        host.turn_messages,
        vec![
            TurnMessage::User {
                content: "read README".to_string(),
            },
            TurnMessage::AssistantToolCall {
                name: "read_file".to_string(),
                arguments: json!({"path": "README.MD"}),
            },
            TurnMessage::ToolObservation {
                name: "read_file".to_string(),
                content: "agentLIBRE readme".to_string(),
            },
            TurnMessage::Assistant {
                content: "README says agentLIBRE.".to_string(),
            },
        ]
    );
    assert_eq!(
        host.operation_kinds(),
        [
            "transition:start",
            "transition:prepare_model_request",
            "transition:request_model",
            "generate",
            "transition:receive_model_response",
            "transition:parse_tool_call",
            "transition:validate_tool_args",
            "transition:start_tool_call",
            "dispatch_tool",
            "transition:finish_tool_call",
            "transition:append_observation",
            "transition:request_model",
            "generate",
            "transition:receive_model_response",
            "transition:parse_answer",
            "transition:final_answer",
            "transition:finish",
        ]
    );
}

#[test]
fn repairs_malformed_tool_json_before_dispatch() {
    let mut host = FakeHost::default()
        .with_model_response(
            r#"<tool_call>"{\"name\":\"read_file\",\"arguments\":{\"path\":\"README.MD\"}}"</tool_call>"#,
        )
        .with_tool_observation("agentLIBRE readme")
        .with_model_response("repaired and done");
    let input = TurnInput::user("read README")
        .with_visible_tool(read_file_tool())
        .with_max_tool_calls(1);

    let output = run_turn(&mut host, input).unwrap();

    assert_eq!(
        output,
        TurnOutput::Answered {
            answer: "repaired and done".to_string()
        }
    );
    assert_eq!(
        host.request_kinds(),
        ["generate", "dispatch_tool", "generate"]
    );
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "model.request_prepared",
            "model.requested",
            "model.response_received",
            "tool.json_malformed",
            "tool.json_repair_attempted",
            "tool.json_repair_succeeded",
            "model.action_parsed",
            "tool.args_validated",
            "tool.call_started",
            "tool.call_finished",
            "observation.appended",
            "model.requested",
            "model.response_received",
            "model.action_parsed",
            "answer.final",
            "turn.finished",
        ]
    );
    assert_eq!(host.dispatches[0].arguments, json!({"path": "README.MD"}));
}

#[test]
fn stops_visibly_when_tool_json_cannot_be_repaired() {
    let mut host = FakeHost::default()
        .with_model_response(r#"<tool_call>{"name":,"arguments":42</tool_call>"#);
    let input = TurnInput::user("bad tool")
        .with_visible_tool(read_file_tool())
        .with_max_tool_calls(1);

    let output = run_turn(&mut host, input).unwrap();

    assert_eq!(
        output,
        TurnOutput::Stopped {
            reason: StopReason::ToolJsonUnrepairable,
            detail: None
        }
    );
    assert_eq!(host.request_kinds(), ["generate"]);
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "model.request_prepared",
            "model.requested",
            "model.response_received",
            "tool.json_malformed",
            "tool.json_repair_attempted",
            "tool.json_repair_failed",
            "turn.stopped",
            "turn.finished",
        ]
    );
}

#[test]
fn stops_before_dispatch_when_tool_limit_is_reached() {
    let mut host = FakeHost::default().with_model_response(tool_call("README.MD"));
    let input = TurnInput::user("read README")
        .with_visible_tool(read_file_tool())
        .with_max_tool_calls(0);

    let output = run_turn(&mut host, input).unwrap();

    assert_eq!(
        output,
        TurnOutput::Stopped {
            reason: StopReason::ToolLimitReached,
            detail: Some(StopDetail::ToolLimitReached { limit: 0 })
        }
    );
    assert_eq!(host.request_kinds(), ["generate"]);
    assert!(host.dispatches.is_empty());
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "model.request_prepared",
            "model.requested",
            "model.response_received",
            "model.action_parsed",
            "tool.limit_reached",
            "turn.stopped",
            "turn.finished",
        ]
    );
}

#[test]
fn rejects_hidden_tool_before_dispatch() {
    let mut host = FakeHost::default().with_model_response(
        r#"<tool_call>{"name":"write_file","arguments":{"path":"README.MD"}}</tool_call>"#,
    );
    let input = TurnInput::user("write README")
        .with_visible_tool(read_file_tool())
        .with_max_tool_calls(1);

    let output = run_turn(&mut host, input).unwrap();

    assert_eq!(
        output,
        TurnOutput::Stopped {
            reason: StopReason::HiddenTool,
            detail: Some(StopDetail::HiddenTool {
                name: "write_file".to_string()
            })
        }
    );
    assert_eq!(host.request_kinds(), ["generate"]);
    assert!(host.dispatches.is_empty());
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "model.request_prepared",
            "model.requested",
            "model.response_received",
            "model.action_parsed",
            "tool.hidden_rejected",
            "turn.stopped",
            "turn.finished",
        ]
    );
}

#[test]
fn validates_tool_args_before_dispatch() {
    let mut host = FakeHost::default().with_model_response(
        r#"<tool_call>{"name":"read_file","arguments":{"other":"README.MD"}}</tool_call>"#,
    );
    let input = TurnInput::user("read README")
        .with_visible_tool(read_file_tool())
        .with_max_tool_calls(1);

    let output = run_turn(&mut host, input).unwrap();

    assert_eq!(
        output,
        TurnOutput::Stopped {
            reason: StopReason::InvalidToolArguments,
            detail: Some(StopDetail::InvalidToolArguments {
                name: "read_file".to_string(),
                message: "missing required argument `path`".to_string()
            })
        }
    );
    assert_eq!(host.request_kinds(), ["generate"]);
    assert!(host.dispatches.is_empty());
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "model.request_prepared",
            "model.requested",
            "model.response_received",
            "model.action_parsed",
            "tool.args_invalid",
            "turn.stopped",
            "turn.finished",
        ]
    );
}

#[test]
fn model_request_failure_finishes_failed_turn() {
    let mut host = FakeHost::default().with_model_error("backend unavailable");

    let err = run_turn(&mut host, TurnInput::user("answer")).unwrap_err();

    assert!(format!("{err:#}").contains("model request failed"));
    assert_eq!(host.request_kinds(), ["generate"]);
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "model.request_prepared",
            "model.requested",
            "model.request_failed",
            "turn.finished",
        ]
    );
    assert_eq!(
        host.transition_kinds(),
        [
            "start",
            "prepare_model_request",
            "request_model",
            "fail",
            "finish",
        ]
    );
    assert!(matches!(
        host.events.last(),
        Some(AgentEvent::TurnFinished {
            status: agl_events::TurnFinishStatus::Failed,
            ..
        })
    ));
}

#[test]
fn tool_dispatch_failure_finishes_failed_turn() {
    let mut host = FakeHost::default()
        .with_model_response(tool_call("README.MD"))
        .with_tool_error("tool unavailable");
    let input = TurnInput::user("read README")
        .with_visible_tool(read_file_tool())
        .with_max_tool_calls(1);

    let err = run_turn(&mut host, input).unwrap_err();

    assert!(format!("{err:#}").contains("tool dispatch `read_file` failed"));
    assert_eq!(host.request_kinds(), ["generate", "dispatch_tool"]);
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "model.request_prepared",
            "model.requested",
            "model.response_received",
            "model.action_parsed",
            "tool.args_validated",
            "tool.call_started",
            "tool.call_failed",
            "turn.finished",
        ]
    );
    assert_eq!(
        host.transition_kinds(),
        [
            "start",
            "prepare_model_request",
            "request_model",
            "receive_model_response",
            "parse_tool_call",
            "validate_tool_args",
            "start_tool_call",
            "fail",
            "finish",
        ]
    );
    assert!(matches!(
        host.events.last(),
        Some(AgentEvent::TurnFinished {
            status: agl_events::TurnFinishStatus::Failed,
            ..
        })
    ));
}
