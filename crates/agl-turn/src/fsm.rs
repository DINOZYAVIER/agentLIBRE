use std::error::Error;
use std::fmt;

use agl_ids::{RunId, TurnId};

use serde_json::Value;

use crate::{HookBatchSummary, HookEvent, StopReason};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnPhase {
    Initialized,
    Started,
    HookBatchPrepared,
    HookBatchRunning,
    ModelRequestPrepared,
    AwaitingModel,
    ModelResponded,
    ActionParsed,
    RepairingToolJson,
    ToolReady,
    ToolRunning,
    ObservationAppended,
    AnswerReady,
    Stopped,
    Failed,
    Finished,
}

impl TurnPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnPhase::Initialized => "initialized",
            TurnPhase::Started => "started",
            TurnPhase::HookBatchPrepared => "hook_batch_prepared",
            TurnPhase::HookBatchRunning => "hook_batch_running",
            TurnPhase::ModelRequestPrepared => "model_request_prepared",
            TurnPhase::AwaitingModel => "awaiting_model",
            TurnPhase::ModelResponded => "model_responded",
            TurnPhase::ActionParsed => "action_parsed",
            TurnPhase::RepairingToolJson => "repairing_tool_json",
            TurnPhase::ToolReady => "tool_ready",
            TurnPhase::ToolRunning => "tool_running",
            TurnPhase::ObservationAppended => "observation_appended",
            TurnPhase::AnswerReady => "answer_ready",
            TurnPhase::Stopped => "stopped",
            TurnPhase::Failed => "failed",
            TurnPhase::Finished => "finished",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TurnTransition {
    Start {
        user_input: String,
    },
    PrepareModelRequest {
        message_count: usize,
    },
    PrepareHookBatch {
        summary: HookBatchSummary,
    },
    RunHookBatch {
        summary: HookBatchSummary,
    },
    FinishHookBatch {
        summary: HookBatchSummary,
    },
    RejectHookFailure {
        summary: HookBatchSummary,
        message: String,
    },
    PrepareRepair {
        summary: HookBatchSummary,
        attempt: usize,
    },
    RequestModel {
        request_index: usize,
    },
    ReceiveModelResponse {
        request_index: usize,
        content: String,
    },
    ParseAnswer,
    ParseToolCall {
        name: String,
    },
    DetectMalformedToolJson {
        classification: ToolJsonMalformedClassification,
        raw_json: String,
    },
    AttemptToolJsonRepair {
        strategy: String,
    },
    SucceedToolJsonRepair {
        strategy: String,
        repaired_json: String,
    },
    FailToolJsonRepair {
        strategy: String,
        message: String,
    },
    ValidateToolArgs {
        name: String,
        arguments: Value,
    },
    RejectToolLimit {
        limit: usize,
    },
    RejectHiddenTool {
        name: String,
    },
    RejectToolArgs {
        name: String,
        message: String,
    },
    StartToolCall {
        name: String,
        arguments: Value,
    },
    FinishToolCall {
        name: String,
        observation: String,
    },
    AppendObservation {
        name: String,
        observation: String,
    },
    FinalAnswer {
        answer: String,
    },
    Stop {
        reason: StopReason,
        visible: bool,
    },
    Fail {
        operation: TurnFailureOperation,
        message: String,
    },
    Finish {
        status: TurnTerminalStatus,
    },
}

impl TurnTransition {
    pub fn as_str(&self) -> &'static str {
        match self {
            TurnTransition::Start { .. } => "start",
            TurnTransition::PrepareModelRequest { .. } => "prepare_model_request",
            TurnTransition::PrepareHookBatch { .. } => "prepare_hook_batch",
            TurnTransition::RunHookBatch { .. } => "run_hook_batch",
            TurnTransition::FinishHookBatch { .. } => "finish_hook_batch",
            TurnTransition::RejectHookFailure { .. } => "reject_hook_failure",
            TurnTransition::PrepareRepair { .. } => "prepare_repair",
            TurnTransition::RequestModel { .. } => "request_model",
            TurnTransition::ReceiveModelResponse { .. } => "receive_model_response",
            TurnTransition::ParseAnswer => "parse_answer",
            TurnTransition::ParseToolCall { .. } => "parse_tool_call",
            TurnTransition::DetectMalformedToolJson { .. } => "detect_malformed_tool_json",
            TurnTransition::AttemptToolJsonRepair { .. } => "attempt_tool_json_repair",
            TurnTransition::SucceedToolJsonRepair { .. } => "succeed_tool_json_repair",
            TurnTransition::FailToolJsonRepair { .. } => "fail_tool_json_repair",
            TurnTransition::ValidateToolArgs { .. } => "validate_tool_args",
            TurnTransition::RejectToolLimit { .. } => "reject_tool_limit",
            TurnTransition::RejectHiddenTool { .. } => "reject_hidden_tool",
            TurnTransition::RejectToolArgs { .. } => "reject_tool_args",
            TurnTransition::StartToolCall { .. } => "start_tool_call",
            TurnTransition::FinishToolCall { .. } => "finish_tool_call",
            TurnTransition::AppendObservation { .. } => "append_observation",
            TurnTransition::FinalAnswer { .. } => "final_answer",
            TurnTransition::Stop { .. } => "stop",
            TurnTransition::Fail { .. } => "fail",
            TurnTransition::Finish { .. } => "finish",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolJsonMalformedClassification {
    MissingTerminator,
    Syntax,
    InvalidShape,
}

impl ToolJsonMalformedClassification {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolJsonMalformedClassification::MissingTerminator => "missing_terminator",
            ToolJsonMalformedClassification::Syntax => "syntax",
            ToolJsonMalformedClassification::InvalidShape => "invalid_shape",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnFailureOperation {
    ModelRequest { request_index: usize },
    ToolDispatch { name: String },
}

impl TurnFailureOperation {
    pub fn as_str(&self) -> &'static str {
        match self {
            TurnFailureOperation::ModelRequest { .. } => "model_request",
            TurnFailureOperation::ToolDispatch { .. } => "tool_dispatch",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnTerminalStatus {
    Answered,
    Stopped,
    Failed,
}

impl TurnTerminalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnTerminalStatus::Answered => "answered",
            TurnTerminalStatus::Stopped => "stopped",
            TurnTerminalStatus::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnTransitionRecord {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub sequence: usize,
    pub from: TurnPhase,
    pub to: TurnPhase,
    pub transition: TurnTransition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnMachine {
    run_id: RunId,
    turn_id: TurnId,
    phase: TurnPhase,
    sequence: usize,
    hook_context: Option<HookTransitionContext>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HookTransitionContext {
    event: HookEvent,
    return_phase: TurnPhase,
}

impl TurnMachine {
    pub fn new(run_id: RunId, turn_id: TurnId) -> Self {
        Self {
            run_id,
            turn_id,
            phase: TurnPhase::Initialized,
            sequence: 0,
            hook_context: None,
        }
    }

    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub fn phase(&self) -> TurnPhase {
        self.phase
    }

    pub fn sequence(&self) -> usize {
        self.sequence
    }

    pub fn apply(
        &mut self,
        transition: TurnTransition,
    ) -> Result<TurnTransitionRecord, TurnTransitionError> {
        let from = self.phase;
        let Some(update) = phase_update(from, self.hook_context, &transition) else {
            return Err(TurnTransitionError {
                phase: from,
                transition: transition.as_str(),
            });
        };

        self.sequence += 1;
        self.phase = update.to;
        match update.hook_context {
            HookContextUpdate::Keep => {}
            HookContextUpdate::Set(context) => self.hook_context = Some(context),
            HookContextUpdate::Clear => self.hook_context = None,
        }
        Ok(TurnTransitionRecord {
            run_id: self.run_id.clone(),
            turn_id: self.turn_id.clone(),
            sequence: self.sequence,
            from,
            to: update.to,
            transition,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnTransitionError {
    pub phase: TurnPhase,
    pub transition: &'static str,
}

impl fmt::Display for TurnTransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "illegal turn transition `{}` from phase `{}`",
            self.transition,
            self.phase.as_str()
        )
    }
}

impl Error for TurnTransitionError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PhaseUpdate {
    to: TurnPhase,
    hook_context: HookContextUpdate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HookContextUpdate {
    Keep,
    Set(HookTransitionContext),
    Clear,
}

fn phase_update(
    from: TurnPhase,
    hook_context: Option<HookTransitionContext>,
    transition: &TurnTransition,
) -> Option<PhaseUpdate> {
    if let Some(update) = hook_phase_update(from, hook_context, transition) {
        return Some(update);
    }

    base_next_phase(from, transition).map(|to| PhaseUpdate {
        to,
        hook_context: HookContextUpdate::Keep,
    })
}

fn hook_phase_update(
    from: TurnPhase,
    hook_context: Option<HookTransitionContext>,
    transition: &TurnTransition,
) -> Option<PhaseUpdate> {
    use TurnPhase::*;
    use TurnTransition::*;

    match (from, hook_context, transition) {
        (phase, None, PrepareHookBatch { summary })
            if hook_boundary_matches(phase, summary.event) =>
        {
            Some(PhaseUpdate {
                to: HookBatchPrepared,
                hook_context: HookContextUpdate::Set(HookTransitionContext {
                    event: summary.event,
                    return_phase: phase,
                }),
            })
        }
        (HookBatchPrepared, Some(context), RunHookBatch { summary })
            if context.event == summary.event =>
        {
            Some(PhaseUpdate {
                to: HookBatchRunning,
                hook_context: HookContextUpdate::Keep,
            })
        }
        (HookBatchRunning, Some(context), FinishHookBatch { summary })
            if context.event == summary.event =>
        {
            Some(PhaseUpdate {
                to: context.return_phase,
                hook_context: HookContextUpdate::Clear,
            })
        }
        (phase, None, RejectHookFailure { summary, .. })
            if hook_boundary_matches(phase, summary.event) =>
        {
            Some(PhaseUpdate {
                to: Failed,
                hook_context: HookContextUpdate::Keep,
            })
        }
        (phase, None, PrepareRepair { summary, attempt })
            if *attempt > 0 && hook_boundary_matches(phase, summary.event) =>
        {
            Some(PhaseUpdate {
                to: phase,
                hook_context: HookContextUpdate::Keep,
            })
        }
        _ => None,
    }
}

fn hook_boundary_matches(phase: TurnPhase, event: HookEvent) -> bool {
    matches!(
        (phase, event),
        (TurnPhase::Started, HookEvent::ContextPrepare)
            | (TurnPhase::AwaitingModel, HookEvent::ModelRequest)
            | (TurnPhase::ModelResponded, HookEvent::ModelResponse)
            | (TurnPhase::AnswerReady, HookEvent::ArtifactWrite)
            | (TurnPhase::AnswerReady, HookEvent::TurnFinish)
    )
}

fn base_next_phase(from: TurnPhase, transition: &TurnTransition) -> Option<TurnPhase> {
    use TurnPhase::*;
    use TurnTransition::*;

    match (from, transition) {
        (Initialized, Start { .. }) => Some(Started),
        (Started, PrepareModelRequest { .. }) => Some(ModelRequestPrepared),
        (ModelRequestPrepared | ObservationAppended | AnswerReady, RequestModel { .. }) => {
            Some(AwaitingModel)
        }
        (AwaitingModel, ReceiveModelResponse { .. }) => Some(ModelResponded),
        (
            AwaitingModel,
            Fail {
                operation: TurnFailureOperation::ModelRequest { .. },
                ..
            },
        ) => Some(Failed),
        (ModelResponded, ParseAnswer) => Some(ActionParsed),
        (ModelResponded | RepairingToolJson, ParseToolCall { .. }) => Some(ActionParsed),
        (ModelResponded, DetectMalformedToolJson { .. }) => Some(RepairingToolJson),
        (RepairingToolJson, AttemptToolJsonRepair { .. }) => Some(RepairingToolJson),
        (RepairingToolJson, SucceedToolJsonRepair { .. }) => Some(RepairingToolJson),
        (RepairingToolJson, FailToolJsonRepair { .. }) => Some(RepairingToolJson),
        (ActionParsed, ValidateToolArgs { .. }) => Some(ToolReady),
        (ActionParsed, RejectToolLimit { .. }) => Some(ActionParsed),
        (ActionParsed, RejectHiddenTool { .. }) => Some(ActionParsed),
        (ActionParsed, RejectToolArgs { .. }) => Some(ActionParsed),
        (ToolReady, StartToolCall { .. }) => Some(ToolRunning),
        (ToolRunning, FinishToolCall { .. }) => Some(ToolRunning),
        (
            ToolRunning,
            Fail {
                operation: TurnFailureOperation::ToolDispatch { .. },
                ..
            },
        ) => Some(Failed),
        (ToolRunning, AppendObservation { .. }) => Some(ObservationAppended),
        (ActionParsed, FinalAnswer { .. }) => Some(AnswerReady),
        (ActionParsed | RepairingToolJson, Stop { .. }) => Some(Stopped),
        (
            AnswerReady,
            Finish {
                status: TurnTerminalStatus::Answered,
            },
        ) => Some(Finished),
        (
            Stopped,
            Finish {
                status: TurnTerminalStatus::Stopped,
            },
        ) => Some(Finished),
        (
            Failed,
            Finish {
                status: TurnTerminalStatus::Failed,
            },
        ) => Some(Finished),
        _ => None,
    }
}
