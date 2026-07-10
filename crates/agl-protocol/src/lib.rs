use std::collections::BTreeMap;

use agl_events::SafeRuntimeEventEnvelope;
use agl_ids::{AttemptId, MessageId, RequestId, RunId, SessionId, TurnId};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

pub const REQUEST_SCHEMA: &str = "agentlibre.daemon.request.v2alpha";
pub const EVENT_SCHEMA: &str = "agentlibre.daemon.event.v2alpha";
pub const PROTOCOL_VERSION: &str = "v2alpha";

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DaemonRequest {
    pub schema: String,
    pub request_id: RequestId,
    #[serde(flatten)]
    pub kind: DaemonRequestKind,
}

impl DaemonRequest {
    pub fn new(request_id: RequestId, kind: DaemonRequestKind) -> Self {
        Self {
            schema: REQUEST_SCHEMA.to_string(),
            request_id,
            kind,
        }
    }
}

impl<'de> Deserialize<'de> for DaemonRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireRequest {
            schema: String,
            request_id: RequestId,
            kind: String,
            payload: Value,
        }

        let wire = WireRequest::deserialize(deserializer)?;
        require_schema::<D::Error>(&wire.schema, REQUEST_SCHEMA)?;
        let kind = decode_tagged::<DaemonRequestKind, D::Error>(wire.kind, wire.payload)?;
        Ok(Self {
            schema: wire.schema,
            request_id: wire.request_id,
            kind,
        })
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
    SessionStatus(SessionStatusRequest),
    SessionList(SessionListRequest),
    SessionTranscript(SessionTranscriptRequest),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DaemonEvent {
    pub schema: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub safe_metadata: BTreeMap<String, String>,
    #[serde(flatten)]
    pub kind: DaemonEventKind,
}

impl DaemonEvent {
    pub fn new(request_id: Option<RequestId>, kind: DaemonEventKind) -> Self {
        Self {
            schema: EVENT_SCHEMA.to_string(),
            request_id,
            safe_metadata: BTreeMap::new(),
            kind,
        }
    }
}

impl<'de> Deserialize<'de> for DaemonEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireEvent {
            schema: String,
            #[serde(default)]
            request_id: Option<RequestId>,
            #[serde(default)]
            safe_metadata: BTreeMap<String, String>,
            kind: String,
            payload: Value,
        }

        let wire = WireEvent::deserialize(deserializer)?;
        require_schema::<D::Error>(&wire.schema, EVENT_SCHEMA)?;
        let kind = decode_tagged::<DaemonEventKind, D::Error>(wire.kind, wire.payload)?;
        Ok(Self {
            schema: wire.schema,
            request_id: wire.request_id,
            safe_metadata: wire.safe_metadata,
            kind,
        })
    }
}

fn require_schema<E>(actual: &str, expected: &'static str) -> Result<(), E>
where
    E: serde::de::Error,
{
    if actual == expected {
        Ok(())
    } else {
        Err(E::custom(format_args!(
            "unsupported schema `{actual}`; expected `{expected}`"
        )))
    }
}

fn decode_tagged<T, E>(kind: String, payload: Value) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    let mut value = serde_json::Map::new();
    value.insert("kind".to_string(), Value::String(kind));
    value.insert("payload".to_string(), payload);
    serde_json::from_value(Value::Object(value)).map_err(E::custom)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum DaemonEventKind {
    Hello(HelloEvent),
    SessionOpened(SessionOpenedEvent),
    TurnStarted(TurnStartedEvent),
    RuntimeEvent(Box<SafeRuntimeEventEnvelope>),
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
#[serde(deny_unknown_fields)]
pub struct HelloRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_protocol_versions: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    SessionList,
    SessionTranscript,
    FinalAssistantMessage,
    RuntimeEvents,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionOpenRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
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
#[serde(deny_unknown_fields)]
pub struct SessionOpenedEvent {
    pub session_id: SessionId,
    pub resumed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTurnRequest {
    pub session_id: SessionId,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnStartedEvent {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_id: TurnId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantMessageEvent {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub content: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnStoppedEvent {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnFinishedEvent {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub status: TurnTerminalStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnFailedEvent {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionClearRequest {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFinishRequest {
    pub session_id: SessionId,
    pub reason: SessionFinishReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStatusRequest {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListRequest {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTranscriptRequest {
    pub session_id: SessionId,
    #[serde(default)]
    pub include_content: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFinishedEvent {
    pub session_id: SessionId,
    pub reason: SessionFinishReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStatusEvent {
    pub session_id: SessionId,
    pub status: SessionStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListEvent {
    pub sessions: Vec<SessionSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTranscriptEvent {
    pub session_id: SessionId,
    pub events: Vec<TranscriptEvent>,
    pub content_included: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    pub session_id: SessionId,
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
    Execute,
    Approve,
    Admin,
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
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TranscriptEvent {
    UserMessage {
        run_id: RunId,
        turn_id: TurnId,
        message_id: MessageId,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    AssistantMessage {
        run_id: RunId,
        turn_id: TurnId,
        message_id: MessageId,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    AssistantToolCall {
        run_id: RunId,
        turn_id: TurnId,
        message_id: MessageId,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<serde_json::Value>,
    },
    ToolMessage {
        run_id: RunId,
        turn_id: TurnId,
        message_id: MessageId,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    ModelAttemptLinked {
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: AttemptId,
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
#[serde(deny_unknown_fields)]
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

    const REQUEST_ID: &str = "req_01890f17-4a00-7000-8000-000000000001";
    const SESSION_ID: &str = "ses_01890f17-4a00-7000-8000-000000000002";
    const RUN_ID: &str = "run_01890f17-4a00-7000-8000-000000000003";
    const TURN_ID: &str = "turn_01890f17-4a00-7000-8000-000000000004";
    const MESSAGE_ID_1: &str = "msg_01890f17-4a00-7000-8000-000000000005";
    const MESSAGE_ID_2: &str = "msg_01890f17-4a00-7000-8000-000000000006";
    const MESSAGE_ID_3: &str = "msg_01890f17-4a00-7000-8000-000000000007";
    const ATTEMPT_ID: &str = "attempt_01890f17-4a00-7000-8000-000000000008";

    fn request_id() -> RequestId {
        RequestId::parse(REQUEST_ID).unwrap()
    }

    fn session_id() -> SessionId {
        SessionId::parse(SESSION_ID).unwrap()
    }

    fn run_id() -> RunId {
        RunId::parse(RUN_ID).unwrap()
    }

    fn turn_id() -> TurnId {
        TurnId::parse(TURN_ID).unwrap()
    }

    fn message_id(value: &str) -> MessageId {
        MessageId::parse(value).unwrap()
    }

    fn attempt_id() -> AttemptId {
        AttemptId::parse(ATTEMPT_ID).unwrap()
    }

    #[test]
    fn session_turn_request_round_trips_as_jsonl_shape() {
        let request = DaemonRequest::new(
            request_id(),
            DaemonRequestKind::SessionTurn(SessionTurnRequest {
                session_id: session_id(),
                text: "hello".to_string(),
                idempotency_key: Some("matrix-event-001".to_string()),
            }),
        );

        let json = serde_json::to_string(&request).unwrap();

        assert!(json.contains("\"schema\":\"agentlibre.daemon.request.v2alpha\""));
        assert!(json.contains(&format!("\"request_id\":\"{REQUEST_ID}\"")));
        assert!(json.contains("\"kind\":\"session_turn\""));
        let decoded: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn hello_event_declares_version_and_capabilities() {
        let event = DaemonEvent::new(
            Some(request_id()),
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
        assert_eq!(serde_json::from_value::<DaemonEvent>(value).unwrap(), event);
    }

    #[test]
    fn turn_control_frames_carry_the_admitted_identity() {
        let started = DaemonEvent::new(
            Some(request_id()),
            DaemonEventKind::TurnStarted(TurnStartedEvent {
                session_id: session_id(),
                run_id: run_id(),
                turn_id: turn_id(),
            }),
        );
        let finished = DaemonEvent::new(
            Some(request_id()),
            DaemonEventKind::TurnFinished(TurnFinishedEvent {
                session_id: session_id(),
                run_id: run_id(),
                turn_id: turn_id(),
                status: TurnTerminalStatus::Answered,
            }),
        );

        for event in [started, finished] {
            let value = serde_json::to_value(&event).unwrap();
            assert_eq!(value["payload"]["session_id"], SESSION_ID);
            assert_eq!(value["payload"]["run_id"], RUN_ID);
            assert_eq!(value["payload"]["turn_id"], TURN_ID);
            assert_eq!(serde_json::from_value::<DaemonEvent>(value).unwrap(), event);
        }
    }

    #[test]
    fn transcript_can_omit_content_by_default() {
        let event = DaemonEvent::new(
            Some(request_id()),
            DaemonEventKind::SessionTranscript(SessionTranscriptEvent {
                session_id: session_id(),
                content_included: false,
                events: vec![
                    TranscriptEvent::UserMessage {
                        run_id: run_id(),
                        turn_id: turn_id(),
                        message_id: message_id(MESSAGE_ID_1),
                        content: None,
                    },
                    TranscriptEvent::AssistantToolCall {
                        run_id: run_id(),
                        turn_id: turn_id(),
                        message_id: message_id(MESSAGE_ID_2),
                        name: "fs.read".to_string(),
                        arguments: None,
                    },
                    TranscriptEvent::ToolMessage {
                        run_id: run_id(),
                        turn_id: turn_id(),
                        message_id: message_id(MESSAGE_ID_3),
                        name: "fs.read".to_string(),
                        content: None,
                    },
                    TranscriptEvent::ModelAttemptLinked {
                        run_id: run_id(),
                        turn_id: turn_id(),
                        attempt_id: attempt_id(),
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
        let event = DaemonEvent::new(Some(request_id()), DaemonEventKind::Error(error));

        let value = serde_json::to_value(&event).unwrap();

        assert_eq!(value["kind"], "error");
        assert_eq!(value["payload"]["code"], "unsupported_protocol_version");
        assert_eq!(value["payload"]["retryable"], false);
    }

    #[test]
    fn previous_alpha_and_untyped_id_shapes_are_rejected() {
        let previous_alpha = serde_json::json!({
            "schema": "agentlibre.daemon.request.v1alpha",
            "request_id": REQUEST_ID,
            "kind": "session_turn",
            "payload": {
                "session_id": SESSION_ID,
                "text": "hello"
            }
        });
        assert!(serde_json::from_value::<DaemonRequest>(previous_alpha).is_err());

        let untyped_ids = serde_json::json!({
            "schema": REQUEST_SCHEMA,
            "request_id": "req-001",
            "kind": "session_turn",
            "payload": {
                "session_id": "session-001",
                "text": "hello"
            }
        });
        assert!(serde_json::from_value::<DaemonRequest>(untyped_ids).is_err());
    }

    #[test]
    fn previous_transcript_and_session_opened_shapes_are_rejected() {
        let previous_transcript = serde_json::json!({
            "schema": EVENT_SCHEMA,
            "request_id": REQUEST_ID,
            "kind": "session_transcript",
            "payload": {
                "session_id": SESSION_ID,
                "content_included": false,
                "events": [{
                    "kind": "user_message",
                    "message_id": MESSAGE_ID_1
                }]
            }
        });
        assert!(serde_json::from_value::<DaemonEvent>(previous_transcript).is_err());

        let previous_opened = serde_json::json!({
            "schema": EVENT_SCHEMA,
            "request_id": REQUEST_ID,
            "kind": "session_opened",
            "payload": {
                "session_id": SESSION_ID,
                "run_id": RUN_ID,
                "resumed": false
            }
        });
        assert!(serde_json::from_value::<DaemonEvent>(previous_opened).is_err());
    }

    #[test]
    fn protocol_envelopes_and_payloads_reject_unknown_fields() {
        let unknown_envelope_field = serde_json::json!({
            "schema": REQUEST_SCHEMA,
            "request_id": REQUEST_ID,
            "kind": "session_list",
            "payload": {},
            "legacy": true
        });
        assert!(serde_json::from_value::<DaemonRequest>(unknown_envelope_field).is_err());

        let unknown_payload_field = serde_json::json!({
            "schema": REQUEST_SCHEMA,
            "request_id": REQUEST_ID,
            "kind": "session_list",
            "payload": { "legacy": true }
        });
        assert!(serde_json::from_value::<DaemonRequest>(unknown_payload_field).is_err());
    }
}
