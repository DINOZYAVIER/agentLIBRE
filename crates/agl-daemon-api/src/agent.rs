pub use crate::progress::AgentProgress;
pub use agl_core::agent::{
    AgentEvent, AgentEventData, AgentEventId, AgentEventPage, AgentMessagePage, AgentOperationKey,
    AgentOperationView, AgentRunOrigin, AgentRunSpec, AgentRunStatus, AgentRunView,
    ConversationView, ExactPackageRef, MessageRole, PackageDigest,
};
use agl_core::implementation_plan::{
    ImplementationPlan, PlanDigest, PlanId, PlanState, SliceResult, SliceState,
};
use agl_core::{AgentRunId, Content, ConversationId, MessageId, RequestId};
use serde::{Deserialize, Serialize};

use crate::MAX_JSONL_FRAME_BYTES;

pub const AGENT_PROTOCOL_SCHEMA: &str = "agentlibre.agent.v1alpha";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProtocolRequest {
    pub schema: String,
    pub request_id: RequestId,
    pub command: AgentCommand,
}

impl AgentProtocolRequest {
    pub fn new(request_id: RequestId, command: AgentCommand) -> Self {
        Self {
            schema: AGENT_PROTOCOL_SCHEMA.into(),
            request_id,
            command,
        }
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.schema != AGENT_PROTOCOL_SCHEMA {
            return Err(ProtocolError::new(
                ProtocolErrorCode::SchemaMismatch,
                "unsupported Agent protocol schema",
                false,
            ));
        }
        let bounded_limit = match self.command {
            AgentCommand::Events { limit, .. }
            | AgentCommand::Messages { limit, .. }
            | AgentCommand::Conversations { limit, .. } => Some(limit),
            _ => None,
        };
        if bounded_limit.is_some_and(|limit| !(1..=1_000).contains(&limit)) {
            return Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "page limit must be between 1 and 1000",
                false,
            ));
        }
        match &self.command {
            AgentCommand::PlanCreate {
                function_path,
                workspace_path,
                prompt,
            } => {
                for (name, path) in [
                    ("function_path", function_path),
                    ("workspace_path", workspace_path),
                ] {
                    if path.is_empty()
                        || path.len() > 32_768
                        || !std::path::Path::new(path).is_absolute()
                    {
                        return Err(ProtocolError::invalid(format!(
                            "{name} must be an absolute path containing 1 to 32768 UTF-8 bytes"
                        )));
                    }
                }
                prompt
                    .validate()
                    .map_err(|_| ProtocolError::invalid("plan prompt is invalid"))?;
            }
            AgentCommand::PlanImplement { function_path, .. } => {
                if function_path.is_empty()
                    || function_path.len() > 32_768
                    || !std::path::Path::new(function_path).is_absolute()
                {
                    return Err(ProtocolError::invalid(
                        "function_path must be an absolute path containing 1 to 32768 UTF-8 bytes",
                    ));
                }
            }
            AgentCommand::OpenConversation {
                function_path,
                workspace_path,
                ..
            } => {
                for (name, path) in [
                    ("function_path", function_path),
                    ("workspace_path", workspace_path),
                ] {
                    if path.is_empty()
                        || path.len() > 32_768
                        || !std::path::Path::new(path).is_absolute()
                    {
                        return Err(ProtocolError::invalid(format!(
                            "{name} must be an absolute path containing 1 to 32768 UTF-8 bytes"
                        )));
                    }
                }
            }
            AgentCommand::ResolveConversation { selector }
            | AgentCommand::RenameConversation { selector, .. } => {
                if selector.is_empty() || selector.len() > 256 {
                    return Err(ProtocolError::invalid(
                        "Conversation selector must contain 1 to 256 UTF-8 bytes",
                    ));
                }
            }
            AgentCommand::Conversations {
                workspace_path: Some(path),
                ..
            } if path.len() > 32_768 || !std::path::Path::new(path).is_absolute() => {
                return Err(ProtocolError::invalid(
                    "workspace_path must be an absolute path containing at most 32768 UTF-8 bytes",
                ));
            }
            _ => {}
        }
        if let AgentCommand::RenameConversation { display_name, .. } = &self.command
            && (display_name.trim().is_empty() || display_name.trim().len() > 256)
        {
            return Err(ProtocolError::invalid(
                "Conversation name must contain 1 to 256 UTF-8 bytes after trimming",
            ));
        }
        let size = serde_json::to_vec(self)
            .map_err(|_| ProtocolError::invalid("request is not encodable"))?
            .len();
        if size > MAX_JSONL_FRAME_BYTES {
            return Err(ProtocolError::new(
                ProtocolErrorCode::FrameTooLarge,
                "request exceeds the 8 MiB frame limit",
                false,
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentCommand {
    PlanCreate {
        function_path: String,
        workspace_path: String,
        prompt: Content,
    },
    PlanView {
        plan_id: PlanId,
    },
    PlanApprove {
        plan_id: PlanId,
        expected_digest: PlanDigest,
    },
    PlanImplement {
        plan_id: PlanId,
        expected_digest: PlanDigest,
        function_path: String,
    },
    PlanStatus {
        plan_id: PlanId,
    },
    OpenConversation {
        conversation_id: ConversationId,
        function_path: String,
        workspace_path: String,
    },
    Conversations {
        workspace_path: Option<String>,
        limit: u16,
    },
    ResolveConversation {
        selector: String,
    },
    RenameConversation {
        selector: String,
        display_name: String,
    },
    StartRun {
        spec: AgentRunSpec,
    },
    CancelRun {
        run_id: AgentRunId,
    },
    RunView {
        run_id: AgentRunId,
    },
    OperationView {
        key: AgentOperationKey,
    },
    Events {
        after: Option<AgentEventId>,
        limit: u16,
    },
    Messages {
        conversation_id: ConversationId,
        after: Option<MessageId>,
        limit: u16,
    },
    Subscribe {
        run_id: AgentRunId,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProtocolStreamFrame {
    pub schema: String,
    pub request_id: RequestId,
    pub frame: AgentSubscriptionFrame,
}

impl AgentProtocolStreamFrame {
    pub fn new(request_id: RequestId, frame: AgentSubscriptionFrame) -> Self {
        Self {
            schema: AGENT_PROTOCOL_SCHEMA.into(),
            request_id,
            frame,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentSubscriptionFrame {
    Subscribed {
        run_view: AgentRunView,
        cursor: AgentEventId,
    },
    Event {
        event: AgentEvent,
    },
    Progress {
        progress: AgentProgress,
    },
    Lagged {
        last_durable_cursor: AgentEventId,
    },
    Ended {
        run_view: AgentRunView,
        cursor: AgentEventId,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProtocolResponse {
    pub schema: String,
    pub request_id: RequestId,
    pub reply: AgentReply,
}

impl AgentProtocolResponse {
    pub fn ok(request_id: RequestId, response: AgentResponse) -> Self {
        Self {
            schema: AGENT_PROTOCOL_SCHEMA.into(),
            request_id,
            reply: AgentReply::Ok {
                response: Box::new(response),
            },
        }
    }

    pub fn error(request_id: RequestId, error: ProtocolError) -> Self {
        Self {
            schema: AGENT_PROTOCOL_SCHEMA.into(),
            request_id,
            reply: AgentReply::Error { error },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentReply {
    Ok { response: Box<AgentResponse> },
    Error { error: ProtocolError },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentResponse {
    PlanCreated {
        plan: Box<PlanArtifactView>,
        conversation: ConversationView,
    },
    PlanViewed {
        plan: Box<PlanArtifactView>,
    },
    PlanImplementStarted {
        plan: Box<PlanArtifactView>,
    },
    PlanStatus {
        plan: Box<PlanArtifactView>,
    },
    PlanApproved {
        plan: Box<PlanArtifactView>,
    },
    ConversationOpened {
        conversation: ConversationView,
        activation: Box<FunctionActivationView>,
    },
    Conversations {
        conversations: Vec<ConversationView>,
    },
    ConversationResolved {
        conversation: ConversationView,
    },
    ConversationRenamed {
        conversation: ConversationView,
    },
    RunStarted {
        run_id: AgentRunId,
    },
    RunCancelled {
        run_id: AgentRunId,
    },
    RunView {
        view: AgentRunView,
    },
    OperationView {
        operation: AgentOperationView,
    },
    Events {
        page: AgentEventPage,
    },
    Messages {
        page: AgentMessagePage,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanArtifactView {
    pub plan: ImplementationPlan,
    pub digest: PlanDigest,
    pub state: PlanState,
    pub results: Vec<SliceResult>,
    pub slices: Vec<SliceStatusView>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SliceStatusView {
    pub slice_id: String,
    pub state: SliceState,
    pub result: Option<SliceResult>,
    pub stale_paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionActivationView {
    pub function: ExactPackageRef,
    pub dependencies: String,
    pub model_artifact: String,
    pub model_service: String,
    pub runtime_profile: PackageDigest,
    pub active_slots: u32,
    pub continuous_batching: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    InvalidRequest,
    SchemaMismatch,
    FrameTooLarge,
    NotFound,
    Conflict,
    Busy,
    Unavailable,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl ProtocolError {
    pub fn new(code: ProtocolErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(ProtocolErrorCode::InvalidRequest, message, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_page_limit_is_bounded() {
        let request = AgentProtocolRequest::new(
            RequestId::generate(),
            AgentCommand::Events {
                after: None,
                limit: 0,
            },
        );
        assert_eq!(
            request.validate().unwrap_err().code,
            ProtocolErrorCode::InvalidRequest
        );
    }

    #[test]
    fn operation_view_command_has_strict_wire_shape() {
        let key = AgentOperationKey {
            run_id: agl_core::AgentRunId::generate(),
            ordinal: std::num::NonZeroU32::new(7).unwrap(),
        };
        let request = AgentProtocolRequest::new(
            RequestId::generate(),
            AgentCommand::OperationView { key: key.clone() },
        );
        request.validate().unwrap();
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["command"]["type"], "operation_view");
        assert_eq!(value["command"]["key"]["ordinal"], 7);
        let decoded: AgentProtocolRequest = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn plan_status_wire_view_preserves_deterministic_stale_paths() {
        let status = SliceStatusView {
            slice_id: "slice".into(),
            state: SliceState::Stale,
            result: None,
            stale_paths: vec!["a.rs".into(), "z.rs".into()],
        };
        let encoded = serde_json::to_value(&status).unwrap();
        let decoded: SliceStatusView = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, status);
    }
}
