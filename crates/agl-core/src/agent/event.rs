use std::num::NonZeroU32;

use crate::{AgentRunId, MessageId};
use serde::{Deserialize, Serialize};

use crate::ToolId;

use super::{
    AgentOperationKey, AgentOperationKind, AgentOperationTerminalStatus, AgentRunFailureKind,
    AgentRunOrigin, AgentRunStatus, AgentRunUsage, DeliveryClass, MemoryClaimRejection,
    MessageRole, MessageVisibility,
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentEventId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentEventData {
    ModelOutputRejected {
        key: AgentOperationKey,
        delivery_attempt: NonZeroU32,
        correction_attempt: u32,
        diagnostic: ModelOutputDiagnostic,
        usage: super::ModelUsage,
        realization: Option<super::InferenceRealizationRef>,
    },
    /// A memory claim rejected during compaction extraction (M1 D4). The key
    /// identifies the extracting operation; `slug` and `text` may be absent
    /// when the claim data was malformed (Q4). Verbatim duplicates and I/O
    /// write failures emit no event.
    MemoryClaimRejected {
        key: AgentOperationKey,
        slug: Option<String>,
        reason: MemoryClaimRejection,
        text: Option<String>,
        sources: Vec<MessageId>,
    },
    RunAdmitted {
        origin: AgentRunOrigin,
    },
    RunStatusChanged {
        from: AgentRunStatus,
        to: AgentRunStatus,
        failure_kind: Option<AgentRunFailureKind>,
    },
    OperationCreated {
        key: AgentOperationKey,
        kind: AgentOperationKind,
        tool_id: Option<ToolId>,
        delivery: DeliveryClass,
    },
    OperationStarted {
        key: AgentOperationKey,
        delivery_attempt: NonZeroU32,
    },
    OperationRetryScheduled {
        key: AgentOperationKey,
        delivery_attempt: NonZeroU32,
        retry_at_ms: i64,
    },
    OperationCompleted {
        key: AgentOperationKey,
        delivery_attempt: NonZeroU32,
        status: AgentOperationTerminalStatus,
    },
    OperationOutcomeUnknown {
        key: AgentOperationKey,
        delivery_attempt: NonZeroU32,
    },
    MessageAppended {
        message_id: MessageId,
        role: MessageRole,
        visibility: MessageVisibility,
    },
    UsageUpdated {
        usage: AgentRunUsage,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelOutputFailureClass {
    MissingTerminator,
    Syntax,
    InvalidShape,
    InvalidToolId,
    InvalidContent,
    ReasoningProjection,
    StructuredToolCall,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOutputDiagnostic {
    pub class: ModelOutputFailureClass,
    pub field: Option<String>,
    pub finish_reason: Option<super::ModelFinishReason>,
    pub output_bytes: u64,
    pub output_digest: super::PackageDigest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEvent {
    pub id: AgentEventId,
    pub agent_run_id: AgentRunId,
    pub operation: Option<AgentOperationKey>,
    pub run_revision: u64,
    pub operation_revision: Option<u64>,
    pub committed_at_ms: i64,
    pub data: AgentEventData,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEventPage {
    pub events: Vec<AgentEvent>,
    pub next_cursor: Option<AgentEventId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> AgentOperationKey {
        AgentOperationKey {
            run_id: crate::AgentRunId::generate(),
            ordinal: NonZeroU32::new(1).unwrap(),
        }
    }

    fn event_with(
        reason: MemoryClaimRejection,
        slug: Option<&str>,
        text: Option<&str>,
    ) -> AgentEventData {
        AgentEventData::MemoryClaimRejected {
            key: key(),
            slug: slug.map(str::to_owned),
            reason,
            text: text.map(str::to_owned),
            sources: vec![MessageId::generate()],
        }
    }

    fn event(slug: Option<&str>, text: Option<&str>) -> AgentEventData {
        event_with(MemoryClaimRejection::MissingSources, slug, text)
    }

    #[test]
    fn memory_claim_rejected_round_trips() {
        let data = event(Some("repl"), Some("The REPL is the default `agl` mode."));
        let json = serde_json::to_string(&data).unwrap();
        let restored: AgentEventData = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn memory_claim_rejected_wire_shape_is_snake_case() {
        let data = event(Some("repl"), Some("The REPL is the default `agl` mode."));
        let value: serde_json::Value = serde_json::to_value(&data).unwrap();
        assert_eq!(value["type"], "memory_claim_rejected");
        assert_eq!(value["reason"], "missing_sources");
        assert_eq!(value["slug"], "repl");
        assert_eq!(value["text"], "The REPL is the default `agl` mode.");
        assert!(value["key"]["run_id"].is_string());
        assert_eq!(value["key"]["ordinal"], 1);
        assert_eq!(value["sources"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn memory_claim_rejected_reasons_serialize_as_recorded() {
        for (reason, expected) in [
            (MemoryClaimRejection::InvalidSlug, "invalid_slug"),
            (MemoryClaimRejection::MissingSources, "missing_sources"),
            (
                MemoryClaimRejection::UnreadableOrEscapingPath,
                "unreadable_or_escaping_path",
            ),
            (MemoryClaimRejection::IndexCapReached, "index_cap_reached"),
            (MemoryClaimRejection::InvalidShape, "invalid_shape"),
            (MemoryClaimRejection::InvalidSources, "invalid_sources"),
            (
                MemoryClaimRejection::InvalidSupersession,
                "invalid_supersession",
            ),
            (MemoryClaimRejection::NotDurable, "not_durable"),
        ] {
            let data = event_with(reason, Some("repl"), Some("Claim text."));
            let value: serde_json::Value = serde_json::to_value(&data).unwrap();
            assert_eq!(value["reason"], expected);
        }
    }

    #[test]
    fn memory_claim_rejected_may_omit_slug_and_text() {
        let data = AgentEventData::MemoryClaimRejected {
            key: key(),
            slug: None,
            reason: MemoryClaimRejection::InvalidShape,
            text: None,
            sources: vec![],
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: AgentEventData = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, data);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value["slug"].is_null());
        assert!(value["text"].is_null());
        assert_eq!(value["sources"].as_array().unwrap().len(), 0);
    }
}
