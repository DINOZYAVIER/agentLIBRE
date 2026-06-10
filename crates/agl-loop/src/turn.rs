use agl_actions::{ModelAction, RepairStrategy, ToolCall, ToolJsonRepair};
use agl_events::{AgentEvent, ParsedActionEvent, TurnFinishStatus};
use agl_turn::policy::{decide_tool_call, ToolCallDecision, ToolCallStop};
use agl_turn::{ModelRequest, StopReason, TurnInput, TurnOutput, TurnState};
use anyhow::Result;

use crate::event_map::{malformed_kind, stop_reason_event};
use crate::AgentLoopHost;

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
            ModelAction::Answer(answer) => {
                emit(
                    host,
                    AgentEvent::ModelActionParsed {
                        turn_id: state.input.turn_id.clone(),
                        action: ParsedActionEvent::Answer,
                    },
                )?;
                return finish_answer(host, &state, answer);
            }
            ModelAction::ToolCall(tool_call) => {
                emit_tool_call_parsed(host, &state, &tool_call)?;
                if let Some(output) = handle_tool_call(host, &mut state, tool_call)? {
                    return Ok(output);
                }
            }
            ModelAction::MalformedToolCall(malformed) => {
                if let Some(output) = handle_malformed_tool_call(host, &mut state, malformed)? {
                    return Ok(output);
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
    let dispatch_request = match decide_tool_call(state, &tool_call) {
        ToolCallDecision::Dispatch(dispatch_request) => dispatch_request,
        ToolCallDecision::Stop(stop) => {
            emit_tool_call_stop(host, state, &stop)?;
            return stop_turn(host, state, stop.reason()).map(Some);
        }
    };

    emit(
        host,
        AgentEvent::ToolArgsValidated {
            turn_id: state.input.turn_id.clone(),
            name: dispatch_request.name.clone(),
            arguments: dispatch_request.arguments.clone(),
        },
    )?;
    emit(
        host,
        AgentEvent::ToolCallStarted {
            turn_id: state.input.turn_id.clone(),
            name: dispatch_request.name.clone(),
            arguments: dispatch_request.arguments.clone(),
        },
    )?;
    let response = host.dispatch_tool(dispatch_request.clone())?;
    emit(
        host,
        AgentEvent::ToolCallFinished {
            turn_id: state.input.turn_id.clone(),
            name: dispatch_request.name.clone(),
            observation: response.observation.clone(),
        },
    )?;
    emit(
        host,
        AgentEvent::ObservationAppended {
            turn_id: state.input.turn_id.clone(),
            name: dispatch_request.name.clone(),
            observation: response.observation.clone(),
        },
    )?;
    state.append_tool_observation(tool_call, response.observation);

    Ok(None)
}

fn handle_malformed_tool_call<H: AgentLoopHost>(
    host: &mut H,
    state: &mut TurnState,
    malformed: agl_actions::MalformedToolCall,
) -> Result<Option<TurnOutput>> {
    emit(
        host,
        AgentEvent::ToolJsonMalformed {
            turn_id: state.input.turn_id.clone(),
            classification: malformed_kind(malformed.classification),
            raw_json: malformed.raw_json,
        },
    )?;

    match malformed.repair {
        Some(ToolJsonRepair::Succeeded {
            strategy,
            repaired_json,
            tool_call,
        }) => {
            emit_repair_attempted(host, state, strategy)?;
            emit(
                host,
                AgentEvent::ToolJsonRepairSucceeded {
                    turn_id: state.input.turn_id.clone(),
                    strategy: strategy.as_str().to_string(),
                    repaired_json,
                },
            )?;
            emit_tool_call_parsed(host, state, &tool_call)?;
            handle_tool_call(host, state, tool_call)
        }
        Some(ToolJsonRepair::Failed { strategy, message }) => {
            emit_repair_attempted(host, state, strategy)?;
            emit(
                host,
                AgentEvent::ToolJsonRepairFailed {
                    turn_id: state.input.turn_id.clone(),
                    strategy: strategy.as_str().to_string(),
                    message,
                },
            )?;
            stop_turn(host, state, StopReason::ToolJsonUnrepairable).map(Some)
        }
        None => {
            emit_repair_attempted(host, state, RepairStrategy::None)?;
            emit(
                host,
                AgentEvent::ToolJsonRepairFailed {
                    turn_id: state.input.turn_id.clone(),
                    strategy: RepairStrategy::None.as_str().to_string(),
                    message: "no repair returned".to_string(),
                },
            )?;
            stop_turn(host, state, StopReason::ToolJsonUnrepairable).map(Some)
        }
    }
}

fn finish_answer<H: AgentLoopHost>(
    host: &mut H,
    state: &TurnState,
    answer: String,
) -> Result<TurnOutput> {
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

fn emit_tool_call_parsed<H: AgentLoopHost>(
    host: &mut H,
    state: &TurnState,
    tool_call: &ToolCall,
) -> Result<()> {
    emit(
        host,
        AgentEvent::ModelActionParsed {
            turn_id: state.input.turn_id.clone(),
            action: ParsedActionEvent::ToolCall {
                name: tool_call.name.clone(),
            },
        },
    )
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

fn emit_tool_call_stop<H: AgentLoopHost>(
    host: &mut H,
    state: &TurnState,
    stop: &ToolCallStop,
) -> Result<()> {
    match stop {
        ToolCallStop::ToolLimitReached { limit } => emit(
            host,
            AgentEvent::ToolLimitReached {
                turn_id: state.input.turn_id.clone(),
                limit: *limit,
            },
        ),
        ToolCallStop::HiddenTool { name } => emit(
            host,
            AgentEvent::ToolHiddenRejected {
                turn_id: state.input.turn_id.clone(),
                name: name.clone(),
            },
        ),
        ToolCallStop::InvalidArguments { name, message } => emit(
            host,
            AgentEvent::ToolArgsInvalid {
                turn_id: state.input.turn_id.clone(),
                name: name.clone(),
                message: message.clone(),
            },
        ),
    }
}

fn emit<H: AgentLoopHost>(host: &mut H, event: AgentEvent) -> Result<()> {
    host.emit_event(event)
}
