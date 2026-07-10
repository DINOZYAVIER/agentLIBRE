use std::time::Instant;

use agl_actions::{ModelAction, RepairStrategy, ToolCall, ToolJsonRepair};
use agl_capabilities::{
    CapabilityId, DispatchDenialCode, HookBatchRequest, HookBatchResult, HookEvent,
};
use agl_events::RuntimeEvent;
use agl_turn::policy::{ToolCallDecision, ToolCallStop, decide_tool_call};
use agl_turn::{
    HookBatchOutcome, HookBatchSummary, ModelRequest, StopDetail, StopReason, TurnFailureOperation,
    TurnHookBatch, TurnInput, TurnMessage, TurnOutput, TurnState, TurnTerminalStatus,
    TurnTransition, TurnTransitionRecord,
};
use anyhow::{Context, Result, anyhow};
use serde_json::json;

use crate::AgentLoopHost;
use crate::event_map::{event_for_record, malformed_kind};

pub fn run_turn<H: AgentLoopHost>(host: &mut H, input: TurnInput) -> Result<TurnOutput> {
    let mut state = TurnState::new(input);
    let mut hook_repair_attempts = 0usize;
    let mut pending_repair_message: Option<String> = None;
    let user_input = state.input.user_input.clone();
    apply_emit(host, &mut state, TurnTransition::Start { user_input })?;
    let payload = context_prepare_payload(&state);
    run_hook_batch(host, &mut state, HookEvent::ContextPrepare, payload)?;
    let message_count = state.messages.len();
    apply_emit(
        host,
        &mut state,
        TurnTransition::PrepareModelRequest { message_count },
    )?;

    loop {
        let request_index = state.request_index;
        apply_emit(
            host,
            &mut state,
            TurnTransition::RequestModel { request_index },
        )?;
        let payload = model_request_payload(&state, request_index);
        run_hook_batch(host, &mut state, HookEvent::ModelRequest, payload)?;
        let mut request_messages = state.messages.clone();
        if let Some(message) = pending_repair_message.take() {
            request_messages.push(TurnMessage::System { content: message });
        }
        let response = match host.generate(ModelRequest {
            run_id: state.input.run_id.clone(),
            turn_id: state.input.turn_id.clone(),
            request_index,
            messages: request_messages,
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
        let payload = model_response_payload(&state, request_index, response.content.len());
        run_hook_batch(host, &mut state, HookEvent::ModelResponse, payload)?;

        match agl_actions::parse_model_action(&response.content) {
            ModelAction::Answer(answer) => {
                apply_emit(host, &mut state, TurnTransition::ParseAnswer)?;
                match finish_answer(host, &mut state, answer, &mut hook_repair_attempts)? {
                    FinishAnswerOutcome::Answered(output) => return Ok(output),
                    FinishAnswerOutcome::Repair { message } => {
                        pending_repair_message = Some(message);
                    }
                }
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
            record_capability_denial(host, &stop)?;
            emit_tool_call_stop(host, state, &stop)?;
            return stop_turn(host, state, stop.reason(), Some(stop.detail())).map(Some);
        }
    };
    let capability_name = dispatch_request.capability_id.as_str().to_owned();

    apply_emit(
        host,
        state,
        TurnTransition::ValidateToolArgs {
            name: capability_name.clone(),
            arguments: dispatch_request.arguments.clone(),
        },
    )?;
    apply_emit(
        host,
        state,
        TurnTransition::StartToolCall {
            name: capability_name.clone(),
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
                    name: capability_name.clone(),
                },
                message,
            )?;
            return Err(err).with_context(|| format!("tool dispatch `{capability_name}` failed"));
        }
    };
    apply_emit(
        host,
        state,
        TurnTransition::FinishToolCall {
            name: capability_name.clone(),
            result: response.result.clone(),
        },
    )?;
    apply_emit(
        host,
        state,
        TurnTransition::AppendObservation {
            name: capability_name,
            result: response.result.clone(),
        },
    )?;
    state.append_tool_result(tool_call, response.result);

    Ok(None)
}

fn record_capability_denial<H: AgentLoopHost>(host: &mut H, stop: &ToolCallStop) -> Result<()> {
    match stop {
        ToolCallStop::HiddenTool { name } => host.record_capability_denial(
            CapabilityId::new(name.clone()).ok(),
            DispatchDenialCode::CapabilityNotEffective,
        ),
        ToolCallStop::InvalidArguments { name, .. } => host.record_capability_denial(
            CapabilityId::new(name.clone()).ok(),
            DispatchDenialCode::InvalidArguments,
        ),
        ToolCallStop::ToolLimitReached { .. } => Ok(()),
    }
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
            stop_turn(host, state, StopReason::ToolJsonUnrepairable, None).map(Some)
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
            stop_turn(host, state, StopReason::ToolJsonUnrepairable, None).map(Some)
        }
    }
}

fn finish_answer<H: AgentLoopHost>(
    host: &mut H,
    state: &mut TurnState,
    answer: String,
    hook_repair_attempts: &mut usize,
) -> Result<FinishAnswerOutcome> {
    apply_emit(
        host,
        state,
        TurnTransition::FinalAnswer {
            answer: answer.clone(),
        },
    )?;
    let payload = artifact_write_payload(state, &answer);
    if let HookBatchAction::Repair { message } = run_hook_batch_for_answer(
        host,
        state,
        HookEvent::ArtifactWrite,
        payload,
        hook_repair_attempts,
    )? {
        return Ok(FinishAnswerOutcome::Repair { message });
    }
    let payload = turn_finish_payload(state, answer.len());
    run_hook_batch(host, state, HookEvent::TurnFinish, payload)?;
    apply_emit(
        host,
        state,
        TurnTransition::Finish {
            status: TurnTerminalStatus::Answered,
        },
    )?;
    let mut messages = state.messages.clone();
    messages.push(TurnMessage::Assistant {
        content: answer.clone(),
    });
    host.record_turn_messages(&messages)?;
    Ok(FinishAnswerOutcome::Answered(TurnOutput::Answered {
        answer,
    }))
}

enum FinishAnswerOutcome {
    Answered(TurnOutput),
    Repair { message: String },
}

enum HookBatchAction {
    Passed,
    Repair { message: String },
}

fn run_hook_batch<H: AgentLoopHost>(
    host: &mut H,
    state: &mut TurnState,
    event: HookEvent,
    payload: serde_json::Value,
) -> Result<()> {
    match run_hook_batch_inner(host, state, event, payload, None)? {
        HookBatchAction::Passed => Ok(()),
        HookBatchAction::Repair { .. } => Err(anyhow!(
            "hook batch `{}` requested unsupported repair",
            event.as_str()
        )),
    }
}

fn run_hook_batch_for_answer<H: AgentLoopHost>(
    host: &mut H,
    state: &mut TurnState,
    event: HookEvent,
    payload: serde_json::Value,
    hook_repair_attempts: &mut usize,
) -> Result<HookBatchAction> {
    run_hook_batch_inner(host, state, event, payload, Some(hook_repair_attempts))
}

fn run_hook_batch_inner<H: AgentLoopHost>(
    host: &mut H,
    state: &mut TurnState,
    event: HookEvent,
    payload: serde_json::Value,
    hook_repair_attempts: Option<&mut usize>,
) -> Result<HookBatchAction> {
    let batch = hook_batch_for_event(&state.input, event);
    if batch.is_empty() {
        return Ok(HookBatchAction::Passed);
    }

    let prepared_summary = batch.summary();
    apply_emit(
        host,
        state,
        TurnTransition::PrepareHookBatch {
            summary: prepared_summary.clone(),
        },
    )?;
    apply_emit(
        host,
        state,
        TurnTransition::RunHookBatch {
            summary: prepared_summary,
        },
    )?;

    let started = Instant::now();
    let result = host.run_hooks(HookBatchRequest {
        event,
        hooks: batch.hook_ids(),
        payload,
    });
    let duration_ms = Some(elapsed_millis(started));
    let (summary, hook_result_for_repair) = match result {
        Ok(result) if result.event == event => {
            let repair_result = result.clone();
            (
                HookBatchSummary::from_batch_result(&batch, result, duration_ms),
                Some(repair_result),
            )
        }
        Ok(result) => {
            let summary = HookBatchSummary::failed_without_results(
                &batch,
                duration_ms,
                "hook.event_mismatch",
            );
            finish_and_reject_hook_batch(
                host,
                state,
                summary,
                format!(
                    "hook batch `{}` returned mismatched event `{}`",
                    event.as_str(),
                    result.event.as_str()
                ),
            )?;
            return Err(anyhow!(
                "hook batch `{}` returned mismatched event",
                event.as_str()
            ));
        }
        Err(err) => {
            let summary =
                HookBatchSummary::failed_without_results(&batch, duration_ms, "hook.host_error");
            finish_and_reject_hook_batch(
                host,
                state,
                summary,
                format!(
                    "hook batch `{}` host callback failed: {err:#}",
                    event.as_str()
                ),
            )?;
            return Err(err).with_context(|| format!("hook batch `{}` failed", event.as_str()));
        }
    };

    apply_emit(
        host,
        state,
        TurnTransition::FinishHookBatch {
            summary: summary.clone(),
        },
    )?;

    match summary.outcome() {
        HookBatchOutcome::Pass | HookBatchOutcome::Warn => Ok(HookBatchAction::Passed),
        HookBatchOutcome::Repair => {
            if let Some(attempts) = hook_repair_attempts
                && *attempts < state.input.max_hook_repair_attempts
            {
                *attempts += 1;
                let attempt = *attempts;
                apply_emit(
                    host,
                    state,
                    TurnTransition::PrepareRepair {
                        summary: summary.clone(),
                        attempt,
                    },
                )?;
                let message =
                    build_hook_repair_message(event, &summary, hook_result_for_repair.as_ref());
                return Ok(HookBatchAction::Repair { message });
            }
            if state.input.max_hook_repair_attempts > 0 {
                let exhausted_attempt = state.input.max_hook_repair_attempts + 1;
                if exhausted_attempt > 0 {
                    apply_emit(
                        host,
                        state,
                        TurnTransition::PrepareRepair {
                            summary: summary.clone(),
                            attempt: exhausted_attempt,
                        },
                    )?;
                }
            } else {
                apply_emit(
                    host,
                    state,
                    TurnTransition::PrepareRepair {
                        summary: summary.clone(),
                        attempt: 1,
                    },
                )?;
            }
            reject_hook_batch(
                host,
                state,
                summary,
                format!(
                    "hook batch `{}` requested repair but no repair handler is available",
                    event.as_str()
                ),
            )?;
            Err(anyhow!(
                "hook batch `{}` requested unsupported repair",
                event.as_str()
            ))
        }
        HookBatchOutcome::Fail => {
            let failed_required_count = summary.failed_required_count();
            let missing_required_count = summary.missing_required_count();
            reject_hook_batch(
                host,
                state,
                summary,
                format!(
                    "required hook batch `{}` failed (failed_required_count={}, missing_required_count={})",
                    event.as_str(),
                    failed_required_count,
                    missing_required_count
                ),
            )?;
            Err(anyhow!("required hook batch `{}` failed", event.as_str()))
        }
    }
}

fn finish_and_reject_hook_batch<H: AgentLoopHost>(
    host: &mut H,
    state: &mut TurnState,
    summary: HookBatchSummary,
    message: String,
) -> Result<()> {
    apply_emit(
        host,
        state,
        TurnTransition::FinishHookBatch {
            summary: summary.clone(),
        },
    )?;
    reject_hook_batch(host, state, summary, message)
}

fn reject_hook_batch<H: AgentLoopHost>(
    host: &mut H,
    state: &mut TurnState,
    summary: HookBatchSummary,
    message: String,
) -> Result<()> {
    apply_emit(
        host,
        state,
        TurnTransition::RejectHookFailure { summary, message },
    )?;
    apply_emit(
        host,
        state,
        TurnTransition::Finish {
            status: TurnTerminalStatus::Failed,
        },
    )?;
    Ok(())
}

fn hook_batch_for_event(input: &TurnInput, event: HookEvent) -> TurnHookBatch {
    let mut batch = TurnHookBatch::new(event);
    for hook_batch in input
        .hook_batches
        .iter()
        .filter(|hook_batch| hook_batch.event == event)
    {
        batch
            .required_hooks
            .extend(hook_batch.required_hooks.iter().cloned());
        batch
            .optional_hooks
            .extend(hook_batch.optional_hooks.iter().cloned());
    }
    batch
}

fn context_prepare_payload(state: &TurnState) -> serde_json::Value {
    json!({
        "turn_id": state.input.turn_id,
        "message_count": state.messages.len(),
        "visible_tool_count": state.input.visible_tools.len(),
    })
}

fn model_request_payload(state: &TurnState, request_index: usize) -> serde_json::Value {
    json!({
        "turn_id": state.input.turn_id,
        "request_index": request_index,
        "message_count": state.messages.len(),
        "visible_tool_count": state.input.visible_tools.len(),
    })
}

fn model_response_payload(
    state: &TurnState,
    request_index: usize,
    content_bytes: usize,
) -> serde_json::Value {
    json!({
        "turn_id": state.input.turn_id,
        "request_index": request_index,
        "content_bytes": content_bytes,
    })
}

fn artifact_write_payload(state: &TurnState, answer: &str) -> serde_json::Value {
    let mut payload = json!({
        "turn_id": state.input.turn_id,
        "artifact_kind": "answer",
        "content": answer,
        "content_bytes": answer.len(),
    });
    merge_hook_payload(&mut payload, &state.input.hook_payload);
    payload
}

fn turn_finish_payload(state: &TurnState, answer_bytes: usize) -> serde_json::Value {
    json!({
        "turn_id": state.input.turn_id,
        "answer_bytes": answer_bytes,
    })
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn merge_hook_payload(payload: &mut serde_json::Value, extra: &serde_json::Value) {
    let (Some(payload), Some(extra)) = (payload.as_object_mut(), extra.as_object()) else {
        return;
    };
    for (key, value) in extra {
        payload.insert(key.clone(), value.clone());
    }
}

fn build_hook_repair_message(
    event: HookEvent,
    summary: &HookBatchSummary,
    result: Option<&HookBatchResult>,
) -> String {
    let fixes = result
        .into_iter()
        .flat_map(|result| result.results.iter())
        .flat_map(|result| result.messages.iter())
        .filter_map(|message| message.fix.as_deref())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let codes = summary.message_codes.join(", ");
    let mut message = format!(
        "The previous answer failed AgentLIBRE hook validation for `{}`.",
        event.as_str()
    );
    if !codes.is_empty() {
        message.push_str(" Message codes: ");
        message.push_str(&codes);
        message.push('.');
    }
    if !fixes.is_empty() {
        message.push_str(" Required fix: ");
        message.push_str(&fixes.join(" "));
    }
    message.push_str(
        " Rewrite the answer. Do not invent runtime ids. Keep all other user-requested content.",
    );
    message
}

fn stop_turn<H: AgentLoopHost>(
    host: &mut H,
    state: &mut TurnState,
    reason: StopReason,
    detail: Option<StopDetail>,
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
    host.record_turn_messages(&state.messages)?;
    Ok(TurnOutput::Stopped { reason, detail })
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
    let event: RuntimeEvent = event_for_record(&record);
    host.emit_transition(&record, &event)?;
    Ok(record)
}
