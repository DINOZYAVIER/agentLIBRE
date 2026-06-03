use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParsedActionEvent {
    Answer,
    ToolCall { name: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolJsonMalformedKind {
    MissingTerminator,
    Syntax,
    InvalidShape,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReasonEvent {
    ToolJsonUnrepairable,
    ToolLimitReached,
    HiddenTool,
    InvalidToolArguments,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFinishStatus {
    Answered,
    Stopped,
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn example_events() -> Vec<AgentEvent> {
        let turn_id = "turn-1".to_string();
        vec![
            AgentEvent::TurnStarted {
                turn_id: turn_id.clone(),
                user_input: "read README".to_string(),
            },
            AgentEvent::PromptRendered {
                turn_id: turn_id.clone(),
                message_count: 1,
            },
            AgentEvent::ModelRequested {
                turn_id: turn_id.clone(),
                request_index: 0,
            },
            AgentEvent::ModelResponseReceived {
                turn_id: turn_id.clone(),
                request_index: 0,
                content: "answer\nwith newline".to_string(),
            },
            AgentEvent::ModelActionParsed {
                turn_id: turn_id.clone(),
                action: ParsedActionEvent::Answer,
            },
            AgentEvent::ModelActionParsed {
                turn_id: turn_id.clone(),
                action: ParsedActionEvent::ToolCall {
                    name: "read_file".to_string(),
                },
            },
            AgentEvent::ToolJsonMalformed {
                turn_id: turn_id.clone(),
                classification: ToolJsonMalformedKind::Syntax,
                raw_json: "{bad".to_string(),
            },
            AgentEvent::ToolJsonRepairAttempted {
                turn_id: turn_id.clone(),
                strategy: "unescape_quoted_json".to_string(),
            },
            AgentEvent::ToolJsonRepairSucceeded {
                turn_id: turn_id.clone(),
                strategy: "unescape_quoted_json".to_string(),
                repaired_json: r#"{"name":"read_file","arguments":{"path":"README.MD"}}"#
                    .to_string(),
            },
            AgentEvent::ToolJsonRepairFailed {
                turn_id: turn_id.clone(),
                strategy: "unescape_quoted_json".to_string(),
                message: "expected value".to_string(),
            },
            AgentEvent::ToolArgsValidated {
                turn_id: turn_id.clone(),
                name: "read_file".to_string(),
                arguments: json!({"path":"README.MD"}),
            },
            AgentEvent::ToolArgsInvalid {
                turn_id: turn_id.clone(),
                name: "read_file".to_string(),
                message: "missing required argument path".to_string(),
            },
            AgentEvent::ToolHiddenRejected {
                turn_id: turn_id.clone(),
                name: "write_file".to_string(),
            },
            AgentEvent::ToolLimitReached {
                turn_id: turn_id.clone(),
                limit: 0,
            },
            AgentEvent::ToolCallStarted {
                turn_id: turn_id.clone(),
                name: "read_file".to_string(),
                arguments: json!({"path":"README.MD"}),
            },
            AgentEvent::ToolCallFinished {
                turn_id: turn_id.clone(),
                name: "read_file".to_string(),
                observation: "README contents".to_string(),
            },
            AgentEvent::ObservationAppended {
                turn_id: turn_id.clone(),
                name: "read_file".to_string(),
                observation: "README contents".to_string(),
            },
            AgentEvent::AnswerFinal {
                turn_id: turn_id.clone(),
                answer: "done".to_string(),
            },
            AgentEvent::TurnStopped {
                turn_id: turn_id.clone(),
                reason: StopReasonEvent::ToolJsonUnrepairable,
                visible: true,
            },
            AgentEvent::TurnFinished {
                turn_id,
                status: TurnFinishStatus::Answered,
            },
        ]
    }

    #[test]
    fn serializes_every_event_as_jsonl() {
        for event in example_events() {
            let line = event.to_jsonl_line().expect("event serializes");
            assert!(!line.contains('\n'), "{line}");
            let decoded: AgentEvent = serde_json::from_str(&line).expect("event round trips");
            assert_eq!(decoded, event);
        }
    }
}
