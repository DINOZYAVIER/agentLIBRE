use agl_actions::{ModelAction, RepairStrategy, ToolCall, ToolJsonRepair};
use agl_events::AgentEvent;
use agl_turn::policy::{ToolCallDecision, ToolCallStop, decide_tool_call};
use agl_turn::{
    ModelRequest, StopReason, TurnFailureOperation, TurnInput, TurnOutput, TurnState,
    TurnTerminalStatus, TurnTransition, TurnTransitionRecord,
};
use anyhow::{Context, Result};

use crate::AgentLoopHost;
use crate::event_map::{event_for_record, malformed_kind};

pub fn run_turn<H: AgentLoopHost>(host: &mut H, input: TurnInput) -> Result<TurnOutput> {
    let mut state = TurnState::new(input);
    let user_input = state.input.user_input.clone();
    apply_emit(host, &mut state, TurnTransition::Start { user_input })?;
    let message_count = state.messages.len();
    apply_emit(
        host,
        &mut state,
        TurnTransition::RenderPrompt { message_count },
    )?;

    loop {
        let request_index = state.request_index;
        apply_emit(
            host,
            &mut state,
            TurnTransition::RequestModel { request_index },
        )?;
        let response = match host.generate(ModelRequest {
            turn_id: state.input.turn_id.clone(),
            request_index,
            messages: state.messages.clone(),
            visible_tools: state.input.visible_tools.clone(),
        }) {
            Ok(response) => response,
            Err(err) => {
                let message = format!("{err:#}");
                fail_turn(
                    host,
                    &mut state,
                    TurnFailureOperation::ModelRequest { request_index },
                    message,
                )?;
                return Err(err).context("model request failed");
            }
        };
        state.request_index += 1;
        apply_emit(
            host,
            &mut state,
            TurnTransition::ReceiveModelResponse {
                request_index,
                content: response.content.clone(),
            },
        )?;

        match agl_actions::parse_model_action(&response.content) {
            ModelAction::Answer(answer) => {
                apply_emit(host, &mut state, TurnTransition::ParseAnswer)?;
                return finish_answer(host, &mut state, answer);
            }
            ModelAction::ToolCall(tool_call) => {
                emit_tool_call_parsed(host, &mut state, &tool_call)?;
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

    apply_emit(
        host,
        state,
        TurnTransition::ValidateToolArgs {
            name: dispatch_request.name.clone(),
            arguments: dispatch_request.arguments.clone(),
        },
    )?;
    apply_emit(
        host,
        state,
        TurnTransition::StartToolCall {
            name: dispatch_request.name.clone(),
            arguments: dispatch_request.arguments.clone(),
        },
    )?;
    let response = match host.dispatch_tool(dispatch_request.clone()) {
        Ok(response) => response,
        Err(err) => {
            let message = format!("{err:#}");
            fail_turn(
                host,
                state,
                TurnFailureOperation::ToolDispatch {
                    name: dispatch_request.name.clone(),
                },
                message,
            )?;
            return Err(err)
                .with_context(|| format!("tool dispatch `{}` failed", dispatch_request.name));
        }
    };
    apply_emit(
        host,
        state,
        TurnTransition::FinishToolCall {
            name: dispatch_request.name.clone(),
            observation: response.observation.clone(),
        },
    )?;
    apply_emit(
        host,
        state,
        TurnTransition::AppendObservation {
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
    apply_emit(
        host,
        state,
        TurnTransition::DetectMalformedToolJson {
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
            apply_emit(
                host,
                state,
                TurnTransition::SucceedToolJsonRepair {
                    strategy: strategy.as_str().to_string(),
                    repaired_json,
                },
            )?;
            emit_tool_call_parsed(host, state, &tool_call)?;
            handle_tool_call(host, state, tool_call)
        }
        Some(ToolJsonRepair::Failed { strategy, message }) => {
            emit_repair_attempted(host, state, strategy)?;
            apply_emit(
                host,
                state,
                TurnTransition::FailToolJsonRepair {
                    strategy: strategy.as_str().to_string(),
                    message,
                },
            )?;
            stop_turn(host, state, StopReason::ToolJsonUnrepairable).map(Some)
        }
        None => {
            emit_repair_attempted(host, state, RepairStrategy::None)?;
            apply_emit(
                host,
                state,
                TurnTransition::FailToolJsonRepair {
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
    state: &mut TurnState,
    answer: String,
) -> Result<TurnOutput> {
    apply_emit(
        host,
        state,
        TurnTransition::FinalAnswer {
            answer: answer.clone(),
        },
    )?;
    apply_emit(
        host,
        state,
        TurnTransition::Finish {
            status: TurnTerminalStatus::Answered,
        },
    )?;
    Ok(TurnOutput::Answered { answer })
}

fn stop_turn<H: AgentLoopHost>(
    host: &mut H,
    state: &mut TurnState,
    reason: StopReason,
) -> Result<TurnOutput> {
    apply_emit(
        host,
        state,
        TurnTransition::Stop {
            reason,
            visible: true,
        },
    )?;
    apply_emit(
        host,
        state,
        TurnTransition::Finish {
            status: TurnTerminalStatus::Stopped,
        },
    )?;
    Ok(TurnOutput::Stopped { reason })
}

fn fail_turn<H: AgentLoopHost>(
    host: &mut H,
    state: &mut TurnState,
    operation: TurnFailureOperation,
    message: String,
) -> Result<()> {
    apply_emit(host, state, TurnTransition::Fail { operation, message })?;
    apply_emit(
        host,
        state,
        TurnTransition::Finish {
            status: TurnTerminalStatus::Failed,
        },
    )?;
    Ok(())
}

fn emit_tool_call_parsed<H: AgentLoopHost>(
    host: &mut H,
    state: &mut TurnState,
    tool_call: &ToolCall,
) -> Result<()> {
    apply_emit(
        host,
        state,
        TurnTransition::ParseToolCall {
            name: tool_call.name.clone(),
        },
    )?;
    Ok(())
}

fn emit_repair_attempted<H: AgentLoopHost>(
    host: &mut H,
    state: &mut TurnState,
    strategy: RepairStrategy,
) -> Result<()> {
    apply_emit(
        host,
        state,
        TurnTransition::AttemptToolJsonRepair {
            strategy: strategy.as_str().to_string(),
        },
    )?;
    Ok(())
}

fn emit_tool_call_stop<H: AgentLoopHost>(
    host: &mut H,
    state: &mut TurnState,
    stop: &ToolCallStop,
) -> Result<()> {
    match stop {
        ToolCallStop::ToolLimitReached { limit } => apply_emit(
            host,
            state,
            TurnTransition::RejectToolLimit { limit: *limit },
        ),
        ToolCallStop::HiddenTool { name } => apply_emit(
            host,
            state,
            TurnTransition::RejectHiddenTool { name: name.clone() },
        ),
        ToolCallStop::InvalidArguments { name, message } => apply_emit(
            host,
            state,
            TurnTransition::RejectToolArgs {
                name: name.clone(),
                message: message.clone(),
            },
        ),
    }?;
    Ok(())
}

fn apply_emit<H: AgentLoopHost>(
    host: &mut H,
    state: &mut TurnState,
    transition: TurnTransition,
) -> Result<TurnTransitionRecord> {
    let record = state.apply_transition(transition)?;
    let event: AgentEvent = event_for_record(&record);
    host.emit_transition(&record, &event)?;
    Ok(record)
}
