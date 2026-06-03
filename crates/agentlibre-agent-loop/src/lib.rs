use agentlibre_observability::{
    AgentEvent, ParsedActionEvent, StopReasonEvent, ToolJsonMalformedKind, TurnFinishStatus,
};
use agentlibre_tool_protocol::{
    MalformedToolJsonKind, ModelAction, RepairStrategy, ToolCall, ToolJsonRepair,
};
use anyhow::Result;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct TurnInput {
    pub turn_id: String,
    pub user_input: String,
    pub visible_tools: Vec<VisibleTool>,
    pub max_tool_calls: usize,
}

impl TurnInput {
    pub fn user(user_input: impl Into<String>) -> Self {
        Self {
            turn_id: "turn-1".to_string(),
            user_input: user_input.into(),
            visible_tools: Vec::new(),
            max_tool_calls: 0,
        }
    }

    pub fn with_visible_tool(mut self, tool: VisibleTool) -> Self {
        self.visible_tools.push(tool);
        self
    }

    pub fn with_max_tool_calls(mut self, max_tool_calls: usize) -> Self {
        self.max_tool_calls = max_tool_calls;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleTool {
    pub name: String,
    pub required_arguments: Vec<String>,
}

impl VisibleTool {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            required_arguments: Vec::new(),
        }
    }

    pub fn require_argument(mut self, name: impl Into<String>) -> Self {
        self.required_arguments.push(name.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelMessage {
    pub role: MessageRole,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRequest {
    pub turn_id: String,
    pub request_index: usize,
    pub messages: Vec<ModelMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelResponse {
    pub content: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolDispatchRequest {
    pub turn_id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDispatchResponse {
    pub observation: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnOutput {
    pub answer: Option<String>,
    pub stop_reason: Option<StopReason>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    ToolJsonUnrepairable,
    ToolLimitReached,
    HiddenTool,
    InvalidToolArguments,
}

pub trait AgentLoopHost {
    fn generate(&mut self, request: ModelRequest) -> Result<ModelResponse>;
    fn dispatch_tool(&mut self, request: ToolDispatchRequest) -> Result<ToolDispatchResponse>;
    fn emit_event(&mut self, event: AgentEvent) -> Result<()>;
}

pub fn run_turn<H: AgentLoopHost>(host: &mut H, input: TurnInput) -> Result<TurnOutput> {
    let mut state = TurnState::new(input);
    emit(
        host,
        AgentEvent::TurnStarted {
            turn_id: state.input.turn_id.clone(),
            user_input: state.input.user_input.clone(),
        },
    )?;
    emit(
        host,
        AgentEvent::PromptRendered {
            turn_id: state.input.turn_id.clone(),
            message_count: state.messages.len(),
        },
    )?;

    loop {
        let request_index = state.request_index;
        emit(
            host,
            AgentEvent::ModelRequested {
                turn_id: state.input.turn_id.clone(),
                request_index,
            },
        )?;
        let response = host.generate(ModelRequest {
            turn_id: state.input.turn_id.clone(),
            request_index,
            messages: state.messages.clone(),
        })?;
        state.request_index += 1;
        emit(
            host,
            AgentEvent::ModelResponseReceived {
                turn_id: state.input.turn_id.clone(),
                request_index,
                content: response.content.clone(),
            },
        )?;

        match agentlibre_tool_protocol::parse_model_action(&response.content) {
            ModelAction::Answer(answer) => return finish_answer(host, &state, answer),
            ModelAction::ToolCall(tool_call) => {
                if let Some(output) = handle_tool_call(host, &mut state, tool_call)? {
                    return Ok(output);
                }
            }
            ModelAction::MalformedToolCall(malformed) => {
                emit(
                    host,
                    AgentEvent::ToolJsonMalformed {
                        turn_id: state.input.turn_id.clone(),
                        classification: malformed_kind(malformed.classification.clone()),
                        raw_json: malformed.raw_json,
                    },
                )?;

                match malformed.repair {
                    Some(ToolJsonRepair::Succeeded {
                        strategy,
                        repaired_json,
                        tool_call,
                    }) => {
                        emit_repair_attempted(host, &state, strategy)?;
                        emit(
                            host,
                            AgentEvent::ToolJsonRepairSucceeded {
                                turn_id: state.input.turn_id.clone(),
                                strategy: strategy.as_str().to_string(),
                                repaired_json,
                            },
                        )?;
                        if let Some(output) = handle_tool_call(host, &mut state, tool_call)? {
                            return Ok(output);
                        }
                    }
                    Some(ToolJsonRepair::Failed { strategy, message }) => {
                        emit_repair_attempted(host, &state, strategy)?;
                        emit(
                            host,
                            AgentEvent::ToolJsonRepairFailed {
                                turn_id: state.input.turn_id.clone(),
                                strategy: strategy.as_str().to_string(),
                                message,
                            },
                        )?;
                        return stop_turn(host, &state, StopReason::ToolJsonUnrepairable);
                    }
                    None => {
                        emit_repair_attempted(host, &state, RepairStrategy::None)?;
                        emit(
                            host,
                            AgentEvent::ToolJsonRepairFailed {
                                turn_id: state.input.turn_id.clone(),
                                strategy: RepairStrategy::None.as_str().to_string(),
                                message: "no repair returned".to_string(),
                            },
                        )?;
                        return stop_turn(host, &state, StopReason::ToolJsonUnrepairable);
                    }
                }
            }
        }
    }
}

struct TurnState {
    input: TurnInput,
    messages: Vec<ModelMessage>,
    request_index: usize,
    tool_call_count: usize,
}

impl TurnState {
    fn new(input: TurnInput) -> Self {
        let messages = vec![ModelMessage {
            role: MessageRole::User,
            content: input.user_input.clone(),
        }];
        Self {
            input,
            messages,
            request_index: 0,
            tool_call_count: 0,
        }
    }
}

fn handle_tool_call<H: AgentLoopHost>(
    host: &mut H,
    state: &mut TurnState,
    tool_call: ToolCall,
) -> Result<Option<TurnOutput>> {
    emit(
        host,
        AgentEvent::ModelActionParsed {
            turn_id: state.input.turn_id.clone(),
            action: ParsedActionEvent::ToolCall {
                name: tool_call.name.clone(),
            },
        },
    )?;

    if state.tool_call_count >= state.input.max_tool_calls {
        emit(
            host,
            AgentEvent::ToolLimitReached {
                turn_id: state.input.turn_id.clone(),
                limit: state.input.max_tool_calls,
            },
        )?;
        return stop_turn(host, state, StopReason::ToolLimitReached).map(Some);
    }

    let Some(visible_tool) = state
        .input
        .visible_tools
        .iter()
        .find(|tool| tool.name == tool_call.name)
    else {
        emit(
            host,
            AgentEvent::ToolHiddenRejected {
                turn_id: state.input.turn_id.clone(),
                name: tool_call.name,
            },
        )?;
        return stop_turn(host, state, StopReason::HiddenTool).map(Some);
    };

    if let Err(message) = validate_tool_arguments(visible_tool, &tool_call.arguments) {
        emit(
            host,
            AgentEvent::ToolArgsInvalid {
                turn_id: state.input.turn_id.clone(),
                name: tool_call.name,
                message,
            },
        )?;
        return stop_turn(host, state, StopReason::InvalidToolArguments).map(Some);
    }

    emit(
        host,
        AgentEvent::ToolArgsValidated {
            turn_id: state.input.turn_id.clone(),
            name: tool_call.name.clone(),
            arguments: tool_call.arguments.clone(),
        },
    )?;
    emit(
        host,
        AgentEvent::ToolCallStarted {
            turn_id: state.input.turn_id.clone(),
            name: tool_call.name.clone(),
            arguments: tool_call.arguments.clone(),
        },
    )?;
    let response = host.dispatch_tool(ToolDispatchRequest {
        turn_id: state.input.turn_id.clone(),
        name: tool_call.name.clone(),
        arguments: tool_call.arguments.clone(),
    })?;
    state.tool_call_count += 1;
    emit(
        host,
        AgentEvent::ToolCallFinished {
            turn_id: state.input.turn_id.clone(),
            name: tool_call.name.clone(),
            observation: response.observation.clone(),
        },
    )?;
    emit(
        host,
        AgentEvent::ObservationAppended {
            turn_id: state.input.turn_id.clone(),
            name: tool_call.name.clone(),
            observation: response.observation.clone(),
        },
    )?;
    state.messages.push(ModelMessage {
        role: MessageRole::Assistant,
        content: format!(
            "<tool_call>{}</tool_call>",
            serde_json::json!({
                "name": tool_call.name,
                "arguments": tool_call.arguments,
            })
        ),
    });
    state.messages.push(ModelMessage {
        role: MessageRole::Tool,
        content: response.observation,
    });

    Ok(None)
}

fn validate_tool_arguments(
    tool: &VisibleTool,
    arguments: &Value,
) -> std::result::Result<(), String> {
    let Some(object) = arguments.as_object() else {
        return Err("tool arguments must be an object".to_string());
    };

    for required in &tool.required_arguments {
        if !object.contains_key(required) {
            return Err(format!("missing required argument `{required}`"));
        }
    }

    Ok(())
}

fn finish_answer<H: AgentLoopHost>(
    host: &mut H,
    state: &TurnState,
    answer: String,
) -> Result<TurnOutput> {
    emit(
        host,
        AgentEvent::ModelActionParsed {
            turn_id: state.input.turn_id.clone(),
            action: ParsedActionEvent::Answer,
        },
    )?;
    emit(
        host,
        AgentEvent::AnswerFinal {
            turn_id: state.input.turn_id.clone(),
            answer: answer.clone(),
        },
    )?;
    emit(
        host,
        AgentEvent::TurnFinished {
            turn_id: state.input.turn_id.clone(),
            status: TurnFinishStatus::Answered,
        },
    )?;
    Ok(TurnOutput {
        answer: Some(answer),
        stop_reason: None,
    })
}

fn stop_turn<H: AgentLoopHost>(
    host: &mut H,
    state: &TurnState,
    reason: StopReason,
) -> Result<TurnOutput> {
    emit(
        host,
        AgentEvent::TurnStopped {
            turn_id: state.input.turn_id.clone(),
            reason: stop_reason_event(&reason),
            visible: true,
        },
    )?;
    emit(
        host,
        AgentEvent::TurnFinished {
            turn_id: state.input.turn_id.clone(),
            status: TurnFinishStatus::Stopped,
        },
    )?;
    Ok(TurnOutput {
        answer: None,
        stop_reason: Some(reason),
    })
}

fn emit_repair_attempted<H: AgentLoopHost>(
    host: &mut H,
    state: &TurnState,
    strategy: RepairStrategy,
) -> Result<()> {
    emit(
        host,
        AgentEvent::ToolJsonRepairAttempted {
            turn_id: state.input.turn_id.clone(),
            strategy: strategy.as_str().to_string(),
        },
    )
}

fn emit<H: AgentLoopHost>(host: &mut H, event: AgentEvent) -> Result<()> {
    host.emit_event(event)
}

fn malformed_kind(kind: MalformedToolJsonKind) -> ToolJsonMalformedKind {
    match kind {
        MalformedToolJsonKind::MissingTerminator => ToolJsonMalformedKind::MissingTerminator,
        MalformedToolJsonKind::Syntax => ToolJsonMalformedKind::Syntax,
        MalformedToolJsonKind::InvalidShape => ToolJsonMalformedKind::InvalidShape,
    }
}

fn stop_reason_event(reason: &StopReason) -> StopReasonEvent {
    match reason {
        StopReason::ToolJsonUnrepairable => StopReasonEvent::ToolJsonUnrepairable,
        StopReason::ToolLimitReached => StopReasonEvent::ToolLimitReached,
        StopReason::HiddenTool => StopReasonEvent::HiddenTool,
        StopReason::InvalidToolArguments => StopReasonEvent::InvalidToolArguments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
