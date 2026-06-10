use crate::{ParsedActionEvent, StopReasonEvent, ToolJsonMalformedKind, TurnFinishStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    #[serde(rename = "turn.started")]
    TurnStarted { turn_id: String, user_input: String },
    #[serde(rename = "prompt.rendered")]
    PromptRendered {
        turn_id: String,
        message_count: usize,
    },
    #[serde(rename = "model.requested")]
    ModelRequested {
        turn_id: String,
        request_index: usize,
    },
    #[serde(rename = "model.response_received")]
    ModelResponseReceived {
        turn_id: String,
        request_index: usize,
        content: String,
    },
    #[serde(rename = "model.action_parsed")]
    ModelActionParsed {
        turn_id: String,
        action: ParsedActionEvent,
    },
    #[serde(rename = "tool.json_malformed")]
    ToolJsonMalformed {
        turn_id: String,
        classification: ToolJsonMalformedKind,
        raw_json: String,
    },
    #[serde(rename = "tool.json_repair_attempted")]
    ToolJsonRepairAttempted { turn_id: String, strategy: String },
    #[serde(rename = "tool.json_repair_succeeded")]
    ToolJsonRepairSucceeded {
        turn_id: String,
        strategy: String,
        repaired_json: String,
    },
    #[serde(rename = "tool.json_repair_failed")]
    ToolJsonRepairFailed {
        turn_id: String,
        strategy: String,
        message: String,
    },
    #[serde(rename = "tool.args_validated")]
    ToolArgsValidated {
        turn_id: String,
        name: String,
        arguments: Value,
    },
    #[serde(rename = "tool.args_invalid")]
    ToolArgsInvalid {
        turn_id: String,
        name: String,
        message: String,
    },
    #[serde(rename = "tool.hidden_rejected")]
    ToolHiddenRejected { turn_id: String, name: String },
    #[serde(rename = "tool.limit_reached")]
    ToolLimitReached { turn_id: String, limit: usize },
    #[serde(rename = "tool.call_started")]
    ToolCallStarted {
        turn_id: String,
        name: String,
        arguments: Value,
    },
    #[serde(rename = "tool.call_finished")]
    ToolCallFinished {
        turn_id: String,
        name: String,
        observation: String,
    },
    #[serde(rename = "observation.appended")]
    ObservationAppended {
        turn_id: String,
        name: String,
        observation: String,
    },
    #[serde(rename = "answer.final")]
    AnswerFinal { turn_id: String, answer: String },
    #[serde(rename = "turn.stopped")]
    TurnStopped {
        turn_id: String,
        reason: StopReasonEvent,
        visible: bool,
    },
    #[serde(rename = "turn.finished")]
    TurnFinished {
        turn_id: String,
        status: TurnFinishStatus,
    },
}

impl AgentEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            AgentEvent::TurnStarted { .. } => "turn.started",
            AgentEvent::PromptRendered { .. } => "prompt.rendered",
            AgentEvent::ModelRequested { .. } => "model.requested",
            AgentEvent::ModelResponseReceived { .. } => "model.response_received",
            AgentEvent::ModelActionParsed { .. } => "model.action_parsed",
            AgentEvent::ToolJsonMalformed { .. } => "tool.json_malformed",
            AgentEvent::ToolJsonRepairAttempted { .. } => "tool.json_repair_attempted",
            AgentEvent::ToolJsonRepairSucceeded { .. } => "tool.json_repair_succeeded",
            AgentEvent::ToolJsonRepairFailed { .. } => "tool.json_repair_failed",
            AgentEvent::ToolArgsValidated { .. } => "tool.args_validated",
            AgentEvent::ToolArgsInvalid { .. } => "tool.args_invalid",
            AgentEvent::ToolHiddenRejected { .. } => "tool.hidden_rejected",
            AgentEvent::ToolLimitReached { .. } => "tool.limit_reached",
            AgentEvent::ToolCallStarted { .. } => "tool.call_started",
            AgentEvent::ToolCallFinished { .. } => "tool.call_finished",
            AgentEvent::ObservationAppended { .. } => "observation.appended",
            AgentEvent::AnswerFinal { .. } => "answer.final",
            AgentEvent::TurnStopped { .. } => "turn.stopped",
            AgentEvent::TurnFinished { .. } => "turn.finished",
        }
    }

    pub fn to_jsonl_line(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}
