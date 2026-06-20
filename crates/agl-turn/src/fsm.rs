use std::error::Error;
use std::fmt;

use serde_json::Value;

use crate::StopReason;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnPhase {
    Initialized,
    Started,
    PromptRendered,
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
            TurnPhase::PromptRendered => "prompt_rendered",
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
    RenderPrompt {
        message_count: usize,
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
            TurnTransition::RenderPrompt { .. } => "render_prompt",
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
    pub turn_id: String,
    pub sequence: usize,
    pub from: TurnPhase,
    pub to: TurnPhase,
    pub transition: TurnTransition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnMachine {
    turn_id: String,
    phase: TurnPhase,
    sequence: usize,
}

impl TurnMachine {
    pub fn new(turn_id: impl Into<String>) -> Self {
        Self {
            turn_id: turn_id.into(),
            phase: TurnPhase::Initialized,
            sequence: 0,
        }
    }

    pub fn turn_id(&self) -> &str {
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
        let Some(to) = next_phase(from, &transition) else {
            return Err(TurnTransitionError {
                phase: from,
                transition: transition.as_str(),
            });
        };

        self.sequence += 1;
        self.phase = to;
        Ok(TurnTransitionRecord {
            turn_id: self.turn_id.clone(),
            sequence: self.sequence,
            from,
            to,
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

fn next_phase(from: TurnPhase, transition: &TurnTransition) -> Option<TurnPhase> {
    use TurnPhase::*;
    use TurnTransition::*;

    match (from, transition) {
        (Initialized, Start { .. }) => Some(Started),
        (Started, RenderPrompt { .. }) => Some(PromptRendered),
        (PromptRendered | ObservationAppended, RequestModel { .. }) => Some(AwaitingModel),
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
