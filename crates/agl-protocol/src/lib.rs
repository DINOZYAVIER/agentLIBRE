use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const REQUEST_SCHEMA: &str = "agentlibre.daemon.request.v1alpha";
pub const EVENT_SCHEMA: &str = "agentlibre.daemon.event.v1alpha";
pub const PROTOCOL_VERSION: &str = "v1alpha";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DaemonRequest {
    pub schema: String,
    pub request_id: String,
    #[serde(flatten)]
    pub kind: DaemonRequestKind,
}

impl DaemonRequest {
    pub fn new(request_id: impl Into<String>, kind: DaemonRequestKind) -> Self {
        Self {
            schema: REQUEST_SCHEMA.to_string(),
            request_id: request_id.into(),
            kind,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum DaemonRequestKind {
    Hello(HelloRequest),
    SessionOpen(SessionOpenRequest),
    SessionTurn(SessionTurnRequest),
    SessionClear(SessionClearRequest),
    SessionFinish(SessionFinishRequest),
    SessionCancel(SessionCancelRequest),
    SessionStatus(SessionStatusRequest),
    SessionList(SessionListRequest),
    SessionTranscript(SessionTranscriptRequest),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DaemonEvent {
    pub schema: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub safe_metadata: BTreeMap<String, String>,
    #[serde(flatten)]
    pub kind: DaemonEventKind,
}

impl DaemonEvent {
    pub fn new(request_id: Option<String>, kind: DaemonEventKind) -> Self {
        Self {
            schema: EVENT_SCHEMA.to_string(),
            request_id,
            safe_metadata: BTreeMap::new(),
            kind,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum DaemonEventKind {
    Hello(HelloEvent),
    SessionOpened(SessionOpenedEvent),
    TurnStarted(TurnStartedEvent),
    AssistantMessage(AssistantMessageEvent),
    TurnStopped(TurnStoppedEvent),
    TurnFinished(TurnFinishedEvent),
    TurnFailed(TurnFailedEvent),
    SessionFinished(SessionFinishedEvent),
    SessionStatus(SessionStatusEvent),
    SessionList(SessionListEvent),
    SessionTranscript(SessionTranscriptEvent),
    Error(ProtocolError),
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct HelloRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_protocol_versions: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HelloEvent {
    pub protocol_version: String,
    pub product_version: String,
    pub capabilities: Vec<DaemonCapability>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonCapability {
    SessionOpen,
    SessionTurn,
    SessionClear,
    SessionFinish,
    SessionStatus,
    SessionTranscript,
    FinalAssistantMessage,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionOpenRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub new_session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default)]
    pub tool_mode: ProtocolToolMode,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionOpenedEvent {
    pub session_id: String,
    pub run_id: String,
    pub resumed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionTurnRequest {
    pub session_id: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TurnStartedEvent {
    pub session_id: String,
    pub turn_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessageEvent {
    pub session_id: String,
    pub content: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TurnStoppedEvent {
    pub session_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TurnFinishedEvent {
    pub session_id: String,
    pub status: TurnTerminalStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TurnFailedEvent {
    pub session_id: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionClearRequest {
    pub session_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionFinishRequest {
    pub session_id: String,
    pub reason: SessionFinishReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionCancelRequest {
    pub session_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionStatusRequest {
    pub session_id: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionListRequest {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionTranscriptRequest {
    pub session_id: String,
    #[serde(default)]
    pub include_content: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionFinishedEvent {
    pub session_id: String,
    pub reason: SessionFinishReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionStatusEvent {
    pub session_id: String,
    pub status: SessionStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionListEvent {
    pub sessions: Vec<SessionSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionTranscriptEvent {
    pub session_id: String,
    pub events: Vec<TranscriptEvent>,
    pub content_included: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub status: SessionStatus,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProtocolToolMode {
    #[default]
    ReadOnly,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnTerminalStatus {
    Answered,
    Stopped,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionFinishReason {
    Eof,
    ExitCommand,
    HostShutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Open,
    Busy,
    Finished,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEvent {
    UserMessage {
        message_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    AssistantMessage {
        message_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    AssistantToolCall {
        message_id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<serde_json::Value>,
    },
    ToolMessage {
        message_id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    ModelAttemptLinked {
        run_id: String,
        attempt_id: String,
    },
    ContextCleared,
    SessionFinished {
        reason: SessionFinishReason,
    },
    SessionFailed {
        message: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub safe_metadata: BTreeMap<String, String>,
}

impl ProtocolError {
    pub fn new(code: ProtocolErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            safe_metadata: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    UnsupportedProtocolVersion,
    InvalidRequest,
    Unauthorized,
    NotFound,
    Busy,
    Unsupported,
    RuntimeFailure,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_turn_request_round_trips_as_jsonl_shape() {
        let request = DaemonRequest::new(
            "req-001",
            DaemonRequestKind::SessionTurn(SessionTurnRequest {
                session_id: "session-001".to_string(),
                text: "hello".to_string(),
                idempotency_key: Some("matrix-event-001".to_string()),
            }),
        );

        let json = serde_json::to_string(&request).unwrap();

        assert!(json.contains("\"schema\":\"agentlibre.daemon.request.v1alpha\""));
        assert!(json.contains("\"request_id\":\"req-001\""));
        assert!(json.contains("\"kind\":\"session_turn\""));
        let decoded: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn hello_event_declares_version_and_capabilities() {
        let event = DaemonEvent::new(
            Some("req-hello".to_string()),
            DaemonEventKind::Hello(HelloEvent {
                protocol_version: PROTOCOL_VERSION.to_string(),
                product_version: "1.0.0-alpha.6".to_string(),
                capabilities: vec![
                    DaemonCapability::SessionOpen,
                    DaemonCapability::SessionTurn,
                    DaemonCapability::FinalAssistantMessage,
                ],
            }),
        );

        let value = serde_json::to_value(&event).unwrap();

        assert_eq!(value["schema"], EVENT_SCHEMA);
        assert_eq!(value["kind"], "hello");
        assert_eq!(value["payload"]["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(value["payload"]["capabilities"][1], "session_turn");
    }

    #[test]
    fn transcript_can_omit_content_by_default() {
        let event = DaemonEvent::new(
            Some("req-transcript".to_string()),
            DaemonEventKind::SessionTranscript(SessionTranscriptEvent {
                session_id: "session-001".to_string(),
                content_included: false,
                events: vec![
                    TranscriptEvent::UserMessage {
                        message_id: "message-0001".to_string(),
                        content: None,
                    },
                    TranscriptEvent::AssistantToolCall {
                        message_id: "message-0002".to_string(),
                        name: "fs.read".to_string(),
                        arguments: None,
                    },
                    TranscriptEvent::ToolMessage {
                        message_id: "message-0003".to_string(),
                        name: "fs.read".to_string(),
                        content: None,
                    },
                    TranscriptEvent::ModelAttemptLinked {
                        run_id: "run-001".to_string(),
                        attempt_id: "attempt-0002".to_string(),
                    },
                ],
            }),
        );

        let json = serde_json::to_string(&event).unwrap();

        assert!(json.contains("\"content_included\":false"));
        assert!(!json.contains("secret prompt"));
        assert!(!json.contains("\"arguments\""));
        assert!(!json.contains("\"content\""));
        let decoded: DaemonEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn protocol_error_has_stable_shape() {
        let error = ProtocolError::new(
            ProtocolErrorCode::UnsupportedProtocolVersion,
            "unsupported protocol version",
            false,
        );
        let event = DaemonEvent::new(Some("req-001".to_string()), DaemonEventKind::Error(error));

        let value = serde_json::to_value(&event).unwrap();

        assert_eq!(value["kind"], "error");
        assert_eq!(value["payload"]["code"], "unsupported_protocol_version");
        assert_eq!(value["payload"]["retryable"], false);
    }
}
