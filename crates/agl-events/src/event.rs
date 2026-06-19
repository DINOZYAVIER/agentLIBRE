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

    pub fn to_safe_jsonl_line(&self) -> serde_json::Result<String> {
        serde_json::to_string(&SafeAgentEvent::from(self))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SafeAgentEvent {
    #[serde(rename = "turn.started")]
    TurnStarted {
        turn_id: String,
        user_input_bytes: usize,
    },
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
        content_bytes: usize,
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
        raw_json_bytes: usize,
    },
    #[serde(rename = "tool.json_repair_attempted")]
    ToolJsonRepairAttempted { turn_id: String, strategy: String },
    #[serde(rename = "tool.json_repair_succeeded")]
    ToolJsonRepairSucceeded {
        turn_id: String,
        strategy: String,
        repaired_json_bytes: usize,
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
        arguments: JsonMetadata,
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
        arguments: JsonMetadata,
    },
    #[serde(rename = "tool.call_finished")]
    ToolCallFinished {
        turn_id: String,
        name: String,
        observation_bytes: usize,
    },
    #[serde(rename = "observation.appended")]
    ObservationAppended {
        turn_id: String,
        name: String,
        observation_bytes: usize,
    },
    #[serde(rename = "answer.final")]
    AnswerFinal {
        turn_id: String,
        answer_bytes: usize,
    },
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "json_kind", rename_all = "snake_case")]
pub enum JsonMetadata {
    Object { keys: Vec<String> },
    Array { len: usize },
    String { bytes: usize },
    Number,
    Bool,
    Null,
}

impl SafeAgentEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            SafeAgentEvent::TurnStarted { .. } => "turn.started",
            SafeAgentEvent::PromptRendered { .. } => "prompt.rendered",
            SafeAgentEvent::ModelRequested { .. } => "model.requested",
            SafeAgentEvent::ModelResponseReceived { .. } => "model.response_received",
            SafeAgentEvent::ModelActionParsed { .. } => "model.action_parsed",
            SafeAgentEvent::ToolJsonMalformed { .. } => "tool.json_malformed",
            SafeAgentEvent::ToolJsonRepairAttempted { .. } => "tool.json_repair_attempted",
            SafeAgentEvent::ToolJsonRepairSucceeded { .. } => "tool.json_repair_succeeded",
            SafeAgentEvent::ToolJsonRepairFailed { .. } => "tool.json_repair_failed",
            SafeAgentEvent::ToolArgsValidated { .. } => "tool.args_validated",
            SafeAgentEvent::ToolArgsInvalid { .. } => "tool.args_invalid",
            SafeAgentEvent::ToolHiddenRejected { .. } => "tool.hidden_rejected",
            SafeAgentEvent::ToolLimitReached { .. } => "tool.limit_reached",
            SafeAgentEvent::ToolCallStarted { .. } => "tool.call_started",
            SafeAgentEvent::ToolCallFinished { .. } => "tool.call_finished",
            SafeAgentEvent::ObservationAppended { .. } => "observation.appended",
            SafeAgentEvent::AnswerFinal { .. } => "answer.final",
            SafeAgentEvent::TurnStopped { .. } => "turn.stopped",
            SafeAgentEvent::TurnFinished { .. } => "turn.finished",
        }
    }
}

impl From<&AgentEvent> for SafeAgentEvent {
    fn from(event: &AgentEvent) -> Self {
        match event {
            AgentEvent::TurnStarted {
                turn_id,
                user_input,
            } => SafeAgentEvent::TurnStarted {
                turn_id: turn_id.clone(),
                user_input_bytes: user_input.len(),
            },
            AgentEvent::PromptRendered {
                turn_id,
                message_count,
            } => SafeAgentEvent::PromptRendered {
                turn_id: turn_id.clone(),
                message_count: *message_count,
            },
            AgentEvent::ModelRequested {
                turn_id,
                request_index,
            } => SafeAgentEvent::ModelRequested {
                turn_id: turn_id.clone(),
                request_index: *request_index,
            },
            AgentEvent::ModelResponseReceived {
                turn_id,
                request_index,
                content,
            } => SafeAgentEvent::ModelResponseReceived {
                turn_id: turn_id.clone(),
                request_index: *request_index,
                content_bytes: content.len(),
            },
            AgentEvent::ModelActionParsed { turn_id, action } => {
                SafeAgentEvent::ModelActionParsed {
                    turn_id: turn_id.clone(),
                    action: action.clone(),
                }
            }
            AgentEvent::ToolJsonMalformed {
                turn_id,
                classification,
                raw_json,
            } => SafeAgentEvent::ToolJsonMalformed {
                turn_id: turn_id.clone(),
                classification: classification.clone(),
                raw_json_bytes: raw_json.len(),
            },
            AgentEvent::ToolJsonRepairAttempted { turn_id, strategy } => {
                SafeAgentEvent::ToolJsonRepairAttempted {
                    turn_id: turn_id.clone(),
                    strategy: strategy.clone(),
                }
            }
            AgentEvent::ToolJsonRepairSucceeded {
                turn_id,
                strategy,
                repaired_json,
            } => SafeAgentEvent::ToolJsonRepairSucceeded {
                turn_id: turn_id.clone(),
                strategy: strategy.clone(),
                repaired_json_bytes: repaired_json.len(),
            },
            AgentEvent::ToolJsonRepairFailed {
                turn_id,
                strategy,
                message,
            } => SafeAgentEvent::ToolJsonRepairFailed {
                turn_id: turn_id.clone(),
                strategy: strategy.clone(),
                message: message.clone(),
            },
            AgentEvent::ToolArgsValidated {
                turn_id,
                name,
                arguments,
            } => SafeAgentEvent::ToolArgsValidated {
                turn_id: turn_id.clone(),
                name: name.clone(),
                arguments: JsonMetadata::from(arguments),
            },
            AgentEvent::ToolArgsInvalid {
                turn_id,
                name,
                message,
            } => SafeAgentEvent::ToolArgsInvalid {
                turn_id: turn_id.clone(),
                name: name.clone(),
                message: message.clone(),
            },
            AgentEvent::ToolHiddenRejected { turn_id, name } => {
                SafeAgentEvent::ToolHiddenRejected {
                    turn_id: turn_id.clone(),
                    name: name.clone(),
                }
            }
            AgentEvent::ToolLimitReached { turn_id, limit } => SafeAgentEvent::ToolLimitReached {
                turn_id: turn_id.clone(),
                limit: *limit,
            },
            AgentEvent::ToolCallStarted {
                turn_id,
                name,
                arguments,
            } => SafeAgentEvent::ToolCallStarted {
                turn_id: turn_id.clone(),
                name: name.clone(),
                arguments: JsonMetadata::from(arguments),
            },
            AgentEvent::ToolCallFinished {
                turn_id,
                name,
                observation,
            } => SafeAgentEvent::ToolCallFinished {
                turn_id: turn_id.clone(),
                name: name.clone(),
                observation_bytes: observation.len(),
            },
            AgentEvent::ObservationAppended {
                turn_id,
                name,
                observation,
            } => SafeAgentEvent::ObservationAppended {
                turn_id: turn_id.clone(),
                name: name.clone(),
                observation_bytes: observation.len(),
            },
            AgentEvent::AnswerFinal { turn_id, answer } => SafeAgentEvent::AnswerFinal {
                turn_id: turn_id.clone(),
                answer_bytes: answer.len(),
            },
            AgentEvent::TurnStopped {
                turn_id,
                reason,
                visible,
            } => SafeAgentEvent::TurnStopped {
                turn_id: turn_id.clone(),
                reason: reason.clone(),
                visible: *visible,
            },
            AgentEvent::TurnFinished { turn_id, status } => SafeAgentEvent::TurnFinished {
                turn_id: turn_id.clone(),
                status: status.clone(),
            },
        }
    }
}

impl From<&Value> for JsonMetadata {
    fn from(value: &Value) -> Self {
        match value {
            Value::Object(object) => {
                let mut keys = object.keys().cloned().collect::<Vec<_>>();
                keys.sort();
                JsonMetadata::Object { keys }
            }
            Value::Array(array) => JsonMetadata::Array { len: array.len() },
            Value::String(value) => JsonMetadata::String { bytes: value.len() },
            Value::Number(_) => JsonMetadata::Number,
            Value::Bool(_) => JsonMetadata::Bool,
            Value::Null => JsonMetadata::Null,
        }
    }
}
