use std::path::{Path, PathBuf};

use agl_ids::MessageId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    EventEnvelope, HookBatchOutcomeEvent, InferenceFinishStatus, ParsedActionEvent,
    SafeParsedActionEvent, StopReasonEvent, ToolJsonMalformedKind, TurnFinishStatus,
};

pub type RuntimeEventEnvelope = EventEnvelope<RuntimeEvent>;
pub type SafeRuntimeEventEnvelope = EventEnvelope<SafeRuntimeEvent>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum RuntimeEvent {
    #[serde(rename = "turn.started")]
    TurnStarted { user_input: String },
    #[serde(rename = "model.request_prepared")]
    ModelRequestPrepared { message_count: usize },
    #[serde(rename = "hook.batch_prepared")]
    HookBatchPrepared {
        event: String,
        required_hooks: Vec<String>,
        optional_hooks: Vec<String>,
        required_count: usize,
        optional_count: usize,
    },
    #[serde(rename = "hook.batch_started")]
    HookBatchStarted {
        event: String,
        hook_ids: Vec<String>,
        required_count: usize,
        optional_count: usize,
    },
    #[serde(rename = "hook.batch_finished")]
    HookBatchFinished {
        event: String,
        outcome: HookBatchOutcomeEvent,
        required_count: usize,
        optional_count: usize,
        failed_required_count: usize,
        warning_count: usize,
        repair_count: usize,
        missing_required_hooks: Vec<String>,
        message_codes: Vec<String>,
        results: Vec<HookResultEvent>,
        duration_ms: Option<u64>,
    },
    #[serde(rename = "hook.batch_blocked")]
    HookBatchBlocked {
        event: String,
        outcome: HookBatchOutcomeEvent,
        message: String,
        failed_required_count: usize,
        missing_required_hooks: Vec<String>,
        message_codes: Vec<String>,
    },
    #[serde(rename = "hook.repair_prepared")]
    HookRepairPrepared {
        event: String,
        attempt: usize,
        hook_ids: Vec<String>,
        message_codes: Vec<String>,
    },
    #[serde(rename = "model.requested")]
    ModelRequested { request_index: usize },
    #[serde(rename = "model.response_received")]
    ModelResponseReceived {
        request_index: usize,
        content: String,
    },
    #[serde(rename = "model.request_failed")]
    ModelRequestFailed {
        request_index: usize,
        message: String,
    },
    #[serde(rename = "model.action_parsed")]
    ModelActionParsed { action: ParsedActionEvent },
    #[serde(rename = "capability.policy_resolved")]
    CapabilityPolicyResolved {
        policy_hash: String,
        capability_ids: Vec<String>,
        exclusions: Vec<CapabilityExclusionEvent>,
    },
    #[serde(rename = "capability.call_admitted")]
    CapabilityCallAdmitted {
        policy_hash: String,
        capability_id: String,
        provider_id: String,
        declaration_digest: String,
    },
    #[serde(rename = "capability.call_denied")]
    CapabilityCallDenied {
        policy_hash: String,
        capability_id: Option<String>,
        reason_code: String,
    },
    #[serde(rename = "tool.json_malformed")]
    ToolJsonMalformed {
        classification: ToolJsonMalformedKind,
        raw_json: String,
    },
    #[serde(rename = "tool.json_repair_attempted")]
    ToolJsonRepairAttempted { strategy: String },
    #[serde(rename = "tool.json_repair_succeeded")]
    ToolJsonRepairSucceeded {
        strategy: String,
        repaired_json: String,
    },
    #[serde(rename = "tool.json_repair_failed")]
    ToolJsonRepairFailed { strategy: String, message: String },
    #[serde(rename = "tool.args_validated")]
    ToolArgsValidated { name: String, arguments: Value },
    #[serde(rename = "tool.args_invalid")]
    ToolArgsInvalid { name: String, message: String },
    #[serde(rename = "tool.hidden_rejected")]
    ToolHiddenRejected { name: String },
    #[serde(rename = "tool.limit_reached")]
    ToolLimitReached { limit: usize },
    #[serde(rename = "tool.call_started")]
    ToolCallStarted { name: String, arguments: Value },
    #[serde(rename = "tool.call_finished")]
    ToolCallFinished { name: String, data: Value },
    #[serde(rename = "tool.call_failed")]
    ToolCallFailed { name: String, message: String },
    #[serde(rename = "observation.appended")]
    ObservationAppended { name: String, data: Value },
    #[serde(rename = "answer.final")]
    AnswerFinal { answer: String },
    #[serde(rename = "turn.stopped")]
    TurnStopped {
        reason: StopReasonEvent,
        visible: bool,
    },
    #[serde(rename = "turn.finished")]
    TurnFinished { status: TurnFinishStatus },
    #[serde(rename = "user_message")]
    UserMessage {
        message_id: MessageId,
        content: String,
    },
    #[serde(rename = "assistant_message")]
    AssistantMessage {
        message_id: MessageId,
        content: String,
    },
    #[serde(rename = "assistant_tool_call")]
    AssistantToolCall {
        message_id: MessageId,
        name: String,
        arguments: Value,
    },
    #[serde(rename = "tool_message")]
    ToolMessage {
        message_id: MessageId,
        name: String,
        data: Value,
    },
    #[serde(rename = "model_attempt_linked")]
    ModelAttemptLinked,
    #[serde(rename = "inference.attempt_started")]
    InferenceAttemptStarted {
        backend: String,
        request_path: PathBuf,
    },
    #[serde(rename = "inference.request_recorded")]
    InferenceRequestRecorded { path: PathBuf },
    #[serde(rename = "inference.response_recorded")]
    InferenceResponseRecorded { path: PathBuf },
    #[serde(rename = "inference.attempt_finished")]
    InferenceAttemptFinished {
        finish_status: InferenceFinishStatus,
    },
    #[serde(rename = "inference.attempt_failed")]
    InferenceAttemptFailed { message: String },
}

impl RuntimeEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::TurnStarted { .. } => "turn.started",
            Self::ModelRequestPrepared { .. } => "model.request_prepared",
            Self::HookBatchPrepared { .. } => "hook.batch_prepared",
            Self::HookBatchStarted { .. } => "hook.batch_started",
            Self::HookBatchFinished { .. } => "hook.batch_finished",
            Self::HookBatchBlocked { .. } => "hook.batch_blocked",
            Self::HookRepairPrepared { .. } => "hook.repair_prepared",
            Self::ModelRequested { .. } => "model.requested",
            Self::ModelResponseReceived { .. } => "model.response_received",
            Self::ModelRequestFailed { .. } => "model.request_failed",
            Self::ModelActionParsed { .. } => "model.action_parsed",
            Self::CapabilityPolicyResolved { .. } => "capability.policy_resolved",
            Self::CapabilityCallAdmitted { .. } => "capability.call_admitted",
            Self::CapabilityCallDenied { .. } => "capability.call_denied",
            Self::ToolJsonMalformed { .. } => "tool.json_malformed",
            Self::ToolJsonRepairAttempted { .. } => "tool.json_repair_attempted",
            Self::ToolJsonRepairSucceeded { .. } => "tool.json_repair_succeeded",
            Self::ToolJsonRepairFailed { .. } => "tool.json_repair_failed",
            Self::ToolArgsValidated { .. } => "tool.args_validated",
            Self::ToolArgsInvalid { .. } => "tool.args_invalid",
            Self::ToolHiddenRejected { .. } => "tool.hidden_rejected",
            Self::ToolLimitReached { .. } => "tool.limit_reached",
            Self::ToolCallStarted { .. } => "tool.call_started",
            Self::ToolCallFinished { .. } => "tool.call_finished",
            Self::ToolCallFailed { .. } => "tool.call_failed",
            Self::ObservationAppended { .. } => "observation.appended",
            Self::AnswerFinal { .. } => "answer.final",
            Self::TurnStopped { .. } => "turn.stopped",
            Self::TurnFinished { .. } => "turn.finished",
            Self::UserMessage { .. } => "user_message",
            Self::AssistantMessage { .. } => "assistant_message",
            Self::AssistantToolCall { .. } => "assistant_tool_call",
            Self::ToolMessage { .. } => "tool_message",
            Self::ModelAttemptLinked => "model_attempt_linked",
            Self::InferenceAttemptStarted { .. } => "inference.attempt_started",
            Self::InferenceRequestRecorded { .. } => "inference.request_recorded",
            Self::InferenceResponseRecorded { .. } => "inference.response_recorded",
            Self::InferenceAttemptFinished { .. } => "inference.attempt_finished",
            Self::InferenceAttemptFailed { .. } => "inference.attempt_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum SafeRuntimeEvent {
    #[serde(rename = "turn.started")]
    TurnStarted { user_input_bytes: usize },
    #[serde(rename = "model.request_prepared")]
    ModelRequestPrepared { message_count: usize },
    #[serde(rename = "hook.batch_prepared")]
    HookBatchPrepared {
        event: String,
        required_hooks: Vec<String>,
        optional_hooks: Vec<String>,
        required_count: usize,
        optional_count: usize,
    },
    #[serde(rename = "hook.batch_started")]
    HookBatchStarted {
        event: String,
        hook_ids: Vec<String>,
        required_count: usize,
        optional_count: usize,
    },
    #[serde(rename = "hook.batch_finished")]
    HookBatchFinished {
        event: String,
        outcome: HookBatchOutcomeEvent,
        required_count: usize,
        optional_count: usize,
        failed_required_count: usize,
        warning_count: usize,
        repair_count: usize,
        missing_required_hooks: Vec<String>,
        message_codes: Vec<String>,
        results: Vec<HookResultEvent>,
        duration_ms: Option<u64>,
    },
    #[serde(rename = "hook.batch_blocked")]
    HookBatchBlocked {
        event: String,
        outcome: HookBatchOutcomeEvent,
        message_bytes: usize,
        failed_required_count: usize,
        missing_required_hooks: Vec<String>,
        message_codes: Vec<String>,
    },
    #[serde(rename = "hook.repair_prepared")]
    HookRepairPrepared {
        event: String,
        attempt: usize,
        hook_ids: Vec<String>,
        message_codes: Vec<String>,
    },
    #[serde(rename = "model.requested")]
    ModelRequested { request_index: usize },
    #[serde(rename = "model.response_received")]
    ModelResponseReceived {
        request_index: usize,
        content_bytes: usize,
    },
    #[serde(rename = "model.request_failed")]
    ModelRequestFailed {
        request_index: usize,
        message_bytes: usize,
    },
    #[serde(rename = "model.action_parsed")]
    ModelActionParsed { action: SafeParsedActionEvent },
    #[serde(rename = "capability.policy_resolved")]
    CapabilityPolicyResolved {
        policy_hash: String,
        capability_ids: Vec<String>,
        exclusions: Vec<CapabilityExclusionEvent>,
    },
    #[serde(rename = "capability.call_admitted")]
    CapabilityCallAdmitted {
        policy_hash: String,
        capability_id: String,
        provider_id: String,
        declaration_digest: String,
    },
    #[serde(rename = "capability.call_denied")]
    CapabilityCallDenied {
        policy_hash: String,
        capability_id: Option<String>,
        reason_code: String,
    },
    #[serde(rename = "tool.json_malformed")]
    ToolJsonMalformed {
        classification: ToolJsonMalformedKind,
        raw_json_bytes: usize,
    },
    #[serde(rename = "tool.json_repair_attempted")]
    ToolJsonRepairAttempted { strategy: String },
    #[serde(rename = "tool.json_repair_succeeded")]
    ToolJsonRepairSucceeded {
        strategy: String,
        repaired_json_bytes: usize,
    },
    #[serde(rename = "tool.json_repair_failed")]
    ToolJsonRepairFailed {
        strategy: String,
        message_bytes: usize,
    },
    #[serde(rename = "tool.args_validated")]
    ToolArgsValidated {
        capability_id: Option<String>,
        arguments: JsonMetadata,
    },
    #[serde(rename = "tool.args_invalid")]
    ToolArgsInvalid {
        capability_id: Option<String>,
        message_bytes: usize,
    },
    #[serde(rename = "tool.hidden_rejected")]
    ToolHiddenRejected { capability_id: Option<String> },
    #[serde(rename = "tool.limit_reached")]
    ToolLimitReached { limit: usize },
    #[serde(rename = "tool.call_started")]
    ToolCallStarted {
        capability_id: Option<String>,
        arguments: JsonMetadata,
    },
    #[serde(rename = "tool.call_finished")]
    ToolCallFinished {
        capability_id: Option<String>,
        data: JsonMetadata,
    },
    #[serde(rename = "tool.call_failed")]
    ToolCallFailed {
        capability_id: Option<String>,
        message_bytes: usize,
    },
    #[serde(rename = "observation.appended")]
    ObservationAppended {
        capability_id: Option<String>,
        data: JsonMetadata,
    },
    #[serde(rename = "answer.final")]
    AnswerFinal { answer_bytes: usize },
    #[serde(rename = "turn.stopped")]
    TurnStopped {
        reason: StopReasonEvent,
        visible: bool,
    },
    #[serde(rename = "turn.finished")]
    TurnFinished { status: TurnFinishStatus },
    #[serde(rename = "user_message")]
    UserMessage {
        message_id: MessageId,
        content_bytes: usize,
    },
    #[serde(rename = "assistant_message")]
    AssistantMessage {
        message_id: MessageId,
        content_bytes: usize,
    },
    #[serde(rename = "assistant_tool_call")]
    AssistantToolCall {
        message_id: MessageId,
        capability_id: Option<String>,
        arguments: JsonMetadata,
    },
    #[serde(rename = "tool_message")]
    ToolMessage {
        message_id: MessageId,
        capability_id: Option<String>,
        data: JsonMetadata,
    },
    #[serde(rename = "model_attempt_linked")]
    ModelAttemptLinked,
    #[serde(rename = "inference.attempt_started")]
    InferenceAttemptStarted {
        backend: String,
        request_path_bytes: usize,
    },
    #[serde(rename = "inference.request_recorded")]
    InferenceRequestRecorded { path_bytes: usize },
    #[serde(rename = "inference.response_recorded")]
    InferenceResponseRecorded { path_bytes: usize },
    #[serde(rename = "inference.attempt_finished")]
    InferenceAttemptFinished {
        finish_status: InferenceFinishStatus,
    },
    #[serde(rename = "inference.attempt_failed")]
    InferenceAttemptFailed { message_bytes: usize },
}

impl SafeRuntimeEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::TurnStarted { .. } => "turn.started",
            Self::ModelRequestPrepared { .. } => "model.request_prepared",
            Self::HookBatchPrepared { .. } => "hook.batch_prepared",
            Self::HookBatchStarted { .. } => "hook.batch_started",
            Self::HookBatchFinished { .. } => "hook.batch_finished",
            Self::HookBatchBlocked { .. } => "hook.batch_blocked",
            Self::HookRepairPrepared { .. } => "hook.repair_prepared",
            Self::ModelRequested { .. } => "model.requested",
            Self::ModelResponseReceived { .. } => "model.response_received",
            Self::ModelRequestFailed { .. } => "model.request_failed",
            Self::ModelActionParsed { .. } => "model.action_parsed",
            Self::CapabilityPolicyResolved { .. } => "capability.policy_resolved",
            Self::CapabilityCallAdmitted { .. } => "capability.call_admitted",
            Self::CapabilityCallDenied { .. } => "capability.call_denied",
            Self::ToolJsonMalformed { .. } => "tool.json_malformed",
            Self::ToolJsonRepairAttempted { .. } => "tool.json_repair_attempted",
            Self::ToolJsonRepairSucceeded { .. } => "tool.json_repair_succeeded",
            Self::ToolJsonRepairFailed { .. } => "tool.json_repair_failed",
            Self::ToolArgsValidated { .. } => "tool.args_validated",
            Self::ToolArgsInvalid { .. } => "tool.args_invalid",
            Self::ToolHiddenRejected { .. } => "tool.hidden_rejected",
            Self::ToolLimitReached { .. } => "tool.limit_reached",
            Self::ToolCallStarted { .. } => "tool.call_started",
            Self::ToolCallFinished { .. } => "tool.call_finished",
            Self::ToolCallFailed { .. } => "tool.call_failed",
            Self::ObservationAppended { .. } => "observation.appended",
            Self::AnswerFinal { .. } => "answer.final",
            Self::TurnStopped { .. } => "turn.stopped",
            Self::TurnFinished { .. } => "turn.finished",
            Self::UserMessage { .. } => "user_message",
            Self::AssistantMessage { .. } => "assistant_message",
            Self::AssistantToolCall { .. } => "assistant_tool_call",
            Self::ToolMessage { .. } => "tool_message",
            Self::ModelAttemptLinked => "model_attempt_linked",
            Self::InferenceAttemptStarted { .. } => "inference.attempt_started",
            Self::InferenceRequestRecorded { .. } => "inference.request_recorded",
            Self::InferenceResponseRecorded { .. } => "inference.response_recorded",
            Self::InferenceAttemptFinished { .. } => "inference.attempt_finished",
            Self::InferenceAttemptFailed { .. } => "inference.attempt_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HookResultEvent {
    pub hook_id: String,
    pub status: HookBatchOutcomeEvent,
    pub message_codes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityExclusionEvent {
    pub capability_id: String,
    pub reason_code: String,
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

impl From<&Value> for JsonMetadata {
    fn from(value: &Value) -> Self {
        match value {
            Value::Object(values) => {
                let mut keys = values.keys().cloned().collect::<Vec<_>>();
                keys.sort();
                Self::Object { keys }
            }
            Value::Array(values) => Self::Array { len: values.len() },
            Value::String(value) => Self::String { bytes: value.len() },
            Value::Number(_) => Self::Number,
            Value::Bool(_) => Self::Bool,
            Value::Null => Self::Null,
        }
    }
}

fn safe_capability_id(value: &str) -> Option<String> {
    let valid = !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b':')
        })
        && value.matches(':').count() <= 1
        && !value.starts_with(':')
        && !value.ends_with(':');
    valid.then(|| value.to_owned())
}

impl From<&RuntimeEvent> for SafeRuntimeEvent {
    fn from(event: &RuntimeEvent) -> Self {
        match event {
            RuntimeEvent::TurnStarted { user_input } => Self::TurnStarted {
                user_input_bytes: user_input.len(),
            },
            RuntimeEvent::ModelRequestPrepared { message_count } => Self::ModelRequestPrepared {
                message_count: *message_count,
            },
            RuntimeEvent::HookBatchPrepared {
                event,
                required_hooks,
                optional_hooks,
                required_count,
                optional_count,
            } => Self::HookBatchPrepared {
                event: event.clone(),
                required_hooks: required_hooks.clone(),
                optional_hooks: optional_hooks.clone(),
                required_count: *required_count,
                optional_count: *optional_count,
            },
            RuntimeEvent::HookBatchStarted {
                event,
                hook_ids,
                required_count,
                optional_count,
            } => Self::HookBatchStarted {
                event: event.clone(),
                hook_ids: hook_ids.clone(),
                required_count: *required_count,
                optional_count: *optional_count,
            },
            RuntimeEvent::HookBatchFinished {
                event,
                outcome,
                required_count,
                optional_count,
                failed_required_count,
                warning_count,
                repair_count,
                missing_required_hooks,
                message_codes,
                results,
                duration_ms,
            } => Self::HookBatchFinished {
                event: event.clone(),
                outcome: *outcome,
                required_count: *required_count,
                optional_count: *optional_count,
                failed_required_count: *failed_required_count,
                warning_count: *warning_count,
                repair_count: *repair_count,
                missing_required_hooks: missing_required_hooks.clone(),
                message_codes: message_codes.clone(),
                results: results.clone(),
                duration_ms: *duration_ms,
            },
            RuntimeEvent::HookBatchBlocked {
                event,
                outcome,
                message,
                failed_required_count,
                missing_required_hooks,
                message_codes,
            } => Self::HookBatchBlocked {
                event: event.clone(),
                outcome: *outcome,
                message_bytes: message.len(),
                failed_required_count: *failed_required_count,
                missing_required_hooks: missing_required_hooks.clone(),
                message_codes: message_codes.clone(),
            },
            RuntimeEvent::HookRepairPrepared {
                event,
                attempt,
                hook_ids,
                message_codes,
            } => Self::HookRepairPrepared {
                event: event.clone(),
                attempt: *attempt,
                hook_ids: hook_ids.clone(),
                message_codes: message_codes.clone(),
            },
            RuntimeEvent::ModelRequested { request_index } => Self::ModelRequested {
                request_index: *request_index,
            },
            RuntimeEvent::ModelResponseReceived {
                request_index,
                content,
            } => Self::ModelResponseReceived {
                request_index: *request_index,
                content_bytes: content.len(),
            },
            RuntimeEvent::ModelRequestFailed {
                request_index,
                message,
            } => Self::ModelRequestFailed {
                request_index: *request_index,
                message_bytes: message.len(),
            },
            RuntimeEvent::ModelActionParsed { action } => Self::ModelActionParsed {
                action: match action {
                    ParsedActionEvent::Answer => SafeParsedActionEvent::Answer,
                    ParsedActionEvent::ToolCall { name } => SafeParsedActionEvent::ToolCall {
                        capability_id: safe_capability_id(name),
                    },
                },
            },
            RuntimeEvent::CapabilityPolicyResolved {
                policy_hash,
                capability_ids,
                exclusions,
            } => Self::CapabilityPolicyResolved {
                policy_hash: policy_hash.clone(),
                capability_ids: capability_ids.clone(),
                exclusions: exclusions.clone(),
            },
            RuntimeEvent::CapabilityCallAdmitted {
                policy_hash,
                capability_id,
                provider_id,
                declaration_digest,
            } => Self::CapabilityCallAdmitted {
                policy_hash: policy_hash.clone(),
                capability_id: capability_id.clone(),
                provider_id: provider_id.clone(),
                declaration_digest: declaration_digest.clone(),
            },
            RuntimeEvent::CapabilityCallDenied {
                policy_hash,
                capability_id,
                reason_code,
            } => Self::CapabilityCallDenied {
                policy_hash: policy_hash.clone(),
                capability_id: capability_id.clone(),
                reason_code: reason_code.clone(),
            },
            RuntimeEvent::ToolJsonMalformed {
                classification,
                raw_json,
            } => Self::ToolJsonMalformed {
                classification: classification.clone(),
                raw_json_bytes: raw_json.len(),
            },
            RuntimeEvent::ToolJsonRepairAttempted { strategy } => Self::ToolJsonRepairAttempted {
                strategy: strategy.clone(),
            },
            RuntimeEvent::ToolJsonRepairSucceeded {
                strategy,
                repaired_json,
            } => Self::ToolJsonRepairSucceeded {
                strategy: strategy.clone(),
                repaired_json_bytes: repaired_json.len(),
            },
            RuntimeEvent::ToolJsonRepairFailed { strategy, message } => {
                Self::ToolJsonRepairFailed {
                    strategy: strategy.clone(),
                    message_bytes: message.len(),
                }
            }
            RuntimeEvent::ToolArgsValidated { name, arguments } => Self::ToolArgsValidated {
                capability_id: safe_capability_id(name),
                arguments: JsonMetadata::from(arguments),
            },
            RuntimeEvent::ToolArgsInvalid { name, message } => Self::ToolArgsInvalid {
                capability_id: safe_capability_id(name),
                message_bytes: message.len(),
            },
            RuntimeEvent::ToolHiddenRejected { name } => Self::ToolHiddenRejected {
                capability_id: safe_capability_id(name),
            },
            RuntimeEvent::ToolLimitReached { limit } => Self::ToolLimitReached { limit: *limit },
            RuntimeEvent::ToolCallStarted { name, arguments } => Self::ToolCallStarted {
                capability_id: safe_capability_id(name),
                arguments: JsonMetadata::from(arguments),
            },
            RuntimeEvent::ToolCallFinished { name, data } => Self::ToolCallFinished {
                capability_id: safe_capability_id(name),
                data: JsonMetadata::from(data),
            },
            RuntimeEvent::ToolCallFailed { name, message } => Self::ToolCallFailed {
                capability_id: safe_capability_id(name),
                message_bytes: message.len(),
            },
            RuntimeEvent::ObservationAppended { name, data } => Self::ObservationAppended {
                capability_id: safe_capability_id(name),
                data: JsonMetadata::from(data),
            },
            RuntimeEvent::AnswerFinal { answer } => Self::AnswerFinal {
                answer_bytes: answer.len(),
            },
            RuntimeEvent::TurnStopped { reason, visible } => Self::TurnStopped {
                reason: reason.clone(),
                visible: *visible,
            },
            RuntimeEvent::TurnFinished { status } => Self::TurnFinished {
                status: status.clone(),
            },
            RuntimeEvent::UserMessage {
                message_id,
                content,
            } => Self::UserMessage {
                message_id: message_id.clone(),
                content_bytes: content.len(),
            },
            RuntimeEvent::AssistantMessage {
                message_id,
                content,
            } => Self::AssistantMessage {
                message_id: message_id.clone(),
                content_bytes: content.len(),
            },
            RuntimeEvent::AssistantToolCall {
                message_id,
                name,
                arguments,
            } => Self::AssistantToolCall {
                message_id: message_id.clone(),
                capability_id: safe_capability_id(name),
                arguments: JsonMetadata::from(arguments),
            },
            RuntimeEvent::ToolMessage {
                message_id,
                name,
                data,
            } => Self::ToolMessage {
                message_id: message_id.clone(),
                capability_id: safe_capability_id(name),
                data: JsonMetadata::from(data),
            },
            RuntimeEvent::ModelAttemptLinked => Self::ModelAttemptLinked,
            RuntimeEvent::InferenceAttemptStarted {
                backend,
                request_path,
            } => Self::InferenceAttemptStarted {
                backend: backend.clone(),
                request_path_bytes: path_bytes(request_path),
            },
            RuntimeEvent::InferenceRequestRecorded { path } => Self::InferenceRequestRecorded {
                path_bytes: path_bytes(path),
            },
            RuntimeEvent::InferenceResponseRecorded { path } => Self::InferenceResponseRecorded {
                path_bytes: path_bytes(path),
            },
            RuntimeEvent::InferenceAttemptFinished { finish_status } => {
                Self::InferenceAttemptFinished {
                    finish_status: *finish_status,
                }
            }
            RuntimeEvent::InferenceAttemptFailed { message } => Self::InferenceAttemptFailed {
                message_bytes: message.len(),
            },
        }
    }
}

impl From<RuntimeEvent> for SafeRuntimeEvent {
    fn from(event: RuntimeEvent) -> Self {
        Self::from(&event)
    }
}

impl EventEnvelope<RuntimeEvent> {
    pub fn redacted(&self) -> EventEnvelope<SafeRuntimeEvent> {
        EventEnvelope {
            schema: self.schema.clone(),
            event_id: self.event_id.clone(),
            sequence: self.sequence,
            occurred_at_unix_ms: self.occurred_at_unix_ms,
            scope: self.scope.clone(),
            request_id: self.request_id.clone(),
            caused_by: self.caused_by.clone(),
            payload: SafeRuntimeEvent::from(&self.payload),
        }
    }

    pub fn into_redacted(self) -> EventEnvelope<SafeRuntimeEvent> {
        self.map_payload(SafeRuntimeEvent::from)
    }
}

impl From<&EventEnvelope<RuntimeEvent>> for EventEnvelope<SafeRuntimeEvent> {
    fn from(envelope: &EventEnvelope<RuntimeEvent>) -> Self {
        envelope.redacted()
    }
}

fn path_bytes(path: &Path) -> usize {
    path.as_os_str().to_string_lossy().len()
}
