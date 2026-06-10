use agl_actions::{ModelAction, RepairStrategy, ToolCall, ToolJsonRepair};
use agl_events::{AgentEvent, ParsedActionEvent, TurnFinishStatus};
use anyhow::Result;

use crate::event_map::{malformed_kind, stop_reason_event};
use crate::state::TurnState;
use crate::tool::validate_tool_arguments;
use crate::{
    AgentLoopHost, MessageRole, ModelMessage, ModelRequest, StopReason, ToolDispatchRequest,
    TurnInput, TurnOutput,
};

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

        match agl_actions::parse_model_action(&response.content) {
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
