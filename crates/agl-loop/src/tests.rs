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

#[derive(Default)]
struct FakeHost {
    model_results: Vec<FakeModelResult>,
    tool_results: Vec<FakeToolResult>,
    requests: Vec<&'static str>,
    operations: Vec<String>,
    events: Vec<AgentEvent>,
    transitions: Vec<TurnTransitionRecord>,
    dispatches: Vec<ToolDispatchRequest>,
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
    fn generate(&mut self, _request: ModelRequest) -> Result<ModelResponse> {
        self.requests.push("generate");
        self.operations.push("generate".to_string());
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

    fn emit_event(&mut self, event: AgentEvent) -> Result<()> {
        self.events.push(event);
        Ok(())
    }

    fn emit_transition(&mut self, record: &TurnTransitionRecord, event: &AgentEvent) -> Result<()> {
        self.operations
            .push(format!("transition:{}", record.transition.as_str()));
        self.transitions.push(record.clone());
        self.emit_event(event.clone())
    }
}

fn read_file_tool() -> VisibleTool {
    VisibleTool::new("read_file").require_argument("path")
}

fn tool_call(path: &str) -> String {
    format!(r#"<tool_call>{{"name":"read_file","arguments":{{"path":"{path}"}}}}</tool_call>"#)
}

#[test]
fn answers_without_tools_when_model_returns_plain_text() {
    let mut host = FakeHost::default().with_model_response("done");
    let output = run_turn(&mut host, TurnInput::user("answer")).unwrap();

    assert_eq!(output.answer, Some("done".to_string()));
    assert_eq!(output.stop_reason, None);
    assert_eq!(host.request_kinds(), ["generate"]);
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "prompt.rendered",
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
            "render_prompt",
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
            "transition:render_prompt",
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

    assert_eq!(output.answer, Some("README says agentLIBRE.".to_string()));
    assert_eq!(
        host.request_kinds(),
        ["generate", "dispatch_tool", "generate"]
    );
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "prompt.rendered",
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
        host.operation_kinds(),
        [
            "transition:start",
            "transition:render_prompt",
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

    assert_eq!(output.answer, Some("repaired and done".to_string()));
    assert_eq!(
        host.request_kinds(),
        ["generate", "dispatch_tool", "generate"]
    );
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "prompt.rendered",
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

    assert_eq!(output.answer, None);
    assert_eq!(output.stop_reason, Some(StopReason::ToolJsonUnrepairable));
    assert_eq!(host.request_kinds(), ["generate"]);
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "prompt.rendered",
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

    assert_eq!(output.stop_reason, Some(StopReason::ToolLimitReached));
    assert_eq!(host.request_kinds(), ["generate"]);
    assert!(host.dispatches.is_empty());
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "prompt.rendered",
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

    assert_eq!(output.stop_reason, Some(StopReason::HiddenTool));
    assert_eq!(host.request_kinds(), ["generate"]);
    assert!(host.dispatches.is_empty());
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "prompt.rendered",
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

    assert_eq!(output.stop_reason, Some(StopReason::InvalidToolArguments));
    assert_eq!(host.request_kinds(), ["generate"]);
    assert!(host.dispatches.is_empty());
    assert_eq!(
        host.event_kinds(),
        [
            "turn.started",
            "prompt.rendered",
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
            "prompt.rendered",
            "model.requested",
            "model.request_failed",
            "turn.finished",
        ]
    );
    assert_eq!(
        host.transition_kinds(),
        ["start", "render_prompt", "request_model", "fail", "finish",]
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
            "prompt.rendered",
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
            "render_prompt",
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
