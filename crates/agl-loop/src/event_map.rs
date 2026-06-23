use agl_actions::MalformedToolJsonKind;
use agl_events::{
    AgentEvent, HookBatchOutcomeEvent, HookResultEvent, ParsedActionEvent, StopReasonEvent,
    ToolJsonMalformedKind, TurnFinishStatus,
};
use agl_turn::{
    HookBatchOutcome, HookBatchSummary, HookId, HookResultSummary, StopReason,
    ToolJsonMalformedClassification, TurnFailureOperation, TurnTerminalStatus, TurnTransition,
    TurnTransitionRecord,
};

pub(crate) fn event_for_record(record: &TurnTransitionRecord) -> AgentEvent {
    match &record.transition {
        TurnTransition::Start { user_input } => AgentEvent::TurnStarted {
            turn_id: record.turn_id.clone(),
            user_input: user_input.clone(),
        },
        TurnTransition::PrepareModelRequest { message_count } => AgentEvent::ModelRequestPrepared {
            turn_id: record.turn_id.clone(),
            message_count: *message_count,
        },
        TurnTransition::PrepareHookBatch { summary } => AgentEvent::HookBatchPrepared {
            turn_id: record.turn_id.clone(),
            event: hook_event_id(summary),
            required_hooks: hook_ids(&summary.required_hooks),
            optional_hooks: hook_ids(&summary.optional_hooks),
            required_count: summary.required_count(),
            optional_count: summary.optional_count(),
        },
        TurnTransition::RunHookBatch { summary } => AgentEvent::HookBatchStarted {
            turn_id: record.turn_id.clone(),
            event: hook_event_id(summary),
            hook_ids: hook_ids(&summary.hook_ids()),
            required_count: summary.required_count(),
            optional_count: summary.optional_count(),
        },
        TurnTransition::FinishHookBatch { summary } => AgentEvent::HookBatchFinished {
            turn_id: record.turn_id.clone(),
            event: hook_event_id(summary),
            outcome: hook_outcome_event(summary.outcome()),
            required_count: summary.required_count(),
            optional_count: summary.optional_count(),
            failed_required_count: summary.failed_required_count(),
            warning_count: summary.warning_count(),
            repair_count: summary.repair_count(),
            missing_required_hooks: hook_ids(&summary.missing_required_hooks),
            message_codes: summary.message_codes.clone(),
            results: hook_results(&summary.results),
            duration_ms: summary.duration_ms,
        },
        TurnTransition::RejectHookFailure { summary, message } => AgentEvent::HookBatchBlocked {
            turn_id: record.turn_id.clone(),
            event: hook_event_id(summary),
            outcome: hook_outcome_event(summary.outcome()),
            message: message.clone(),
            failed_required_count: summary.failed_required_count(),
            missing_required_hooks: hook_ids(&summary.missing_required_hooks),
            message_codes: summary.message_codes.clone(),
        },
        TurnTransition::PrepareRepair { summary, attempt } => AgentEvent::HookRepairPrepared {
            turn_id: record.turn_id.clone(),
            event: hook_event_id(summary),
            attempt: *attempt,
            hook_ids: hook_ids(&summary.hook_ids()),
            message_codes: summary.message_codes.clone(),
        },
        TurnTransition::RequestModel { request_index } => AgentEvent::ModelRequested {
            turn_id: record.turn_id.clone(),
            request_index: *request_index,
        },
        TurnTransition::ReceiveModelResponse {
            request_index,
            content,
        } => AgentEvent::ModelResponseReceived {
            turn_id: record.turn_id.clone(),
            request_index: *request_index,
            content: content.clone(),
        },
        TurnTransition::ParseAnswer => AgentEvent::ModelActionParsed {
            turn_id: record.turn_id.clone(),
            action: ParsedActionEvent::Answer,
        },
        TurnTransition::ParseToolCall { name } => AgentEvent::ModelActionParsed {
            turn_id: record.turn_id.clone(),
            action: ParsedActionEvent::ToolCall { name: name.clone() },
        },
        TurnTransition::DetectMalformedToolJson {
            classification,
            raw_json,
        } => AgentEvent::ToolJsonMalformed {
            turn_id: record.turn_id.clone(),
            classification: malformed_classification(*classification),
            raw_json: raw_json.clone(),
        },
        TurnTransition::AttemptToolJsonRepair { strategy } => AgentEvent::ToolJsonRepairAttempted {
            turn_id: record.turn_id.clone(),
            strategy: strategy.clone(),
        },
        TurnTransition::SucceedToolJsonRepair {
            strategy,
            repaired_json,
        } => AgentEvent::ToolJsonRepairSucceeded {
            turn_id: record.turn_id.clone(),
            strategy: strategy.clone(),
            repaired_json: repaired_json.clone(),
        },
        TurnTransition::FailToolJsonRepair { strategy, message } => {
            AgentEvent::ToolJsonRepairFailed {
                turn_id: record.turn_id.clone(),
                strategy: strategy.clone(),
                message: message.clone(),
            }
        }
        TurnTransition::ValidateToolArgs { name, arguments } => AgentEvent::ToolArgsValidated {
            turn_id: record.turn_id.clone(),
            name: name.clone(),
            arguments: arguments.clone(),
        },
        TurnTransition::RejectToolLimit { limit } => AgentEvent::ToolLimitReached {
            turn_id: record.turn_id.clone(),
            limit: *limit,
        },
        TurnTransition::RejectHiddenTool { name } => AgentEvent::ToolHiddenRejected {
            turn_id: record.turn_id.clone(),
            name: name.clone(),
        },
        TurnTransition::RejectToolArgs { name, message } => AgentEvent::ToolArgsInvalid {
            turn_id: record.turn_id.clone(),
            name: name.clone(),
            message: message.clone(),
        },
        TurnTransition::StartToolCall { name, arguments } => AgentEvent::ToolCallStarted {
            turn_id: record.turn_id.clone(),
            name: name.clone(),
            arguments: arguments.clone(),
        },
        TurnTransition::FinishToolCall { name, observation } => AgentEvent::ToolCallFinished {
            turn_id: record.turn_id.clone(),
            name: name.clone(),
            observation: observation.clone(),
        },
        TurnTransition::AppendObservation { name, observation } => {
            AgentEvent::ObservationAppended {
                turn_id: record.turn_id.clone(),
                name: name.clone(),
                observation: observation.clone(),
            }
        }
        TurnTransition::FinalAnswer { answer } => AgentEvent::AnswerFinal {
            turn_id: record.turn_id.clone(),
            answer: answer.clone(),
        },
        TurnTransition::Stop { reason, visible } => AgentEvent::TurnStopped {
            turn_id: record.turn_id.clone(),
            reason: stop_reason_event(reason),
            visible: *visible,
        },
        TurnTransition::Fail { operation, message } => match operation {
            TurnFailureOperation::ModelRequest { request_index } => {
                AgentEvent::ModelRequestFailed {
                    turn_id: record.turn_id.clone(),
                    request_index: *request_index,
                    message: message.clone(),
                }
            }
            TurnFailureOperation::ToolDispatch { name } => AgentEvent::ToolCallFailed {
                turn_id: record.turn_id.clone(),
                name: name.clone(),
                message: message.clone(),
            },
        },
        TurnTransition::Finish { status } => AgentEvent::TurnFinished {
            turn_id: record.turn_id.clone(),
            status: finish_status_event(*status),
        },
    }
}

pub(crate) fn malformed_kind(kind: MalformedToolJsonKind) -> ToolJsonMalformedClassification {
    match kind {
        MalformedToolJsonKind::MissingTerminator => {
            ToolJsonMalformedClassification::MissingTerminator
        }
        MalformedToolJsonKind::Syntax => ToolJsonMalformedClassification::Syntax,
        MalformedToolJsonKind::InvalidShape => ToolJsonMalformedClassification::InvalidShape,
    }
}

fn malformed_classification(kind: ToolJsonMalformedClassification) -> ToolJsonMalformedKind {
    match kind {
        ToolJsonMalformedClassification::MissingTerminator => {
            ToolJsonMalformedKind::MissingTerminator
        }
        ToolJsonMalformedClassification::Syntax => ToolJsonMalformedKind::Syntax,
        ToolJsonMalformedClassification::InvalidShape => ToolJsonMalformedKind::InvalidShape,
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

fn hook_event_id(summary: &HookBatchSummary) -> String {
    summary.event.as_str().to_string()
}

fn hook_ids(ids: &[HookId]) -> Vec<String> {
    ids.iter()
        .map(|hook_id| hook_id.as_str().to_string())
        .collect()
}

fn hook_results(results: &[HookResultSummary]) -> Vec<HookResultEvent> {
    results
        .iter()
        .map(|result| HookResultEvent {
            hook_id: result.hook_id.as_str().to_string(),
            status: hook_outcome_event(result.status),
            message_codes: result.message_codes.clone(),
        })
        .collect()
}

fn hook_outcome_event(outcome: HookBatchOutcome) -> HookBatchOutcomeEvent {
    match outcome {
        HookBatchOutcome::Pass => HookBatchOutcomeEvent::Pass,
        HookBatchOutcome::Warn => HookBatchOutcomeEvent::Warn,
        HookBatchOutcome::Fail => HookBatchOutcomeEvent::Fail,
        HookBatchOutcome::Repair => HookBatchOutcomeEvent::Repair,
    }
}

fn finish_status_event(status: TurnTerminalStatus) -> TurnFinishStatus {
    match status {
        TurnTerminalStatus::Answered => TurnFinishStatus::Answered,
        TurnTerminalStatus::Stopped => TurnFinishStatus::Stopped,
        TurnTerminalStatus::Failed => TurnFinishStatus::Failed,
    }
}
