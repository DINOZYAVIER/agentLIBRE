use crate::*;
use agl_events::AgentEvent;
use anyhow::Result;
use serde_json::json;

#[derive(Default)]
struct FakeHost {
    model_responses: Vec<String>,
    tool_observations: Vec<String>,
    requests: Vec<&'static str>,
    events: Vec<AgentEvent>,
    dispatches: Vec<ToolDispatchRequest>,
}

impl FakeHost {
    fn with_model_response(mut self, response: impl Into<String>) -> Self {
        self.model_responses.push(response.into());
        self
    }

    fn with_tool_observation(mut self, observation: impl Into<String>) -> Self {
        self.tool_observations.push(observation.into());
        self
    }

    fn request_kinds(&self) -> Vec<&'static str> {
        self.requests.clone()
    }

    fn event_kinds(&self) -> Vec<&'static str> {
        self.events.iter().map(AgentEvent::kind).collect()
    }
}

impl AgentLoopHost for FakeHost {
    fn generate(&mut self, _request: ModelRequest) -> Result<ModelResponse> {
        self.requests.push("generate");
        let content = self.model_responses.remove(0);
        Ok(ModelResponse { content })
    }

    fn dispatch_tool(&mut self, request: ToolDispatchRequest) -> Result<ToolDispatchResponse> {
        self.requests.push("dispatch_tool");
        self.dispatches.push(request);
        let observation = self.tool_observations.remove(0);
        Ok(ToolDispatchResponse { observation })
    }

    fn emit_event(&mut self, event: AgentEvent) -> Result<()> {
        self.events.push(event);
        Ok(())
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
