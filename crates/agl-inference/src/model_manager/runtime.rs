use std::fmt;

use crate::InferenceFinishReason;
use crate::admission::{AllocationEstimate, DeviceMemorySnapshot};
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub struct RuntimeOperation<T> {
    pub value: T,
    pub log: String,
}

impl<T> RuntimeOperation<T> {
    pub fn new(value: T, log: impl Into<String>) -> Self {
        Self {
            value,
            log: log.into(),
        }
    }

    pub fn without_log(value: T) -> Self {
        Self::new(value, String::new())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeFailure {
    message: String,
    log: String,
    kind: RuntimeFailureKind,
    code: String,
    resource_admission_details: Option<Box<ResourceAdmissionDetails>>,
}

/// Machine-readable evidence for a resource-admission decision made before
/// any model load starts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceAdmissionDetails {
    pub selected_profile_id: String,
    pub context_tokens: u32,
    pub model_key: String,
    pub context_key: String,
    pub snapshot: DeviceMemorySnapshot,
    pub estimate: AllocationEstimate,
    pub required_bytes: u64,
    pub available_bytes: u64,
    pub reserved_bytes: u64,
    pub pressure_bytes: u64,
    pub reserve_bytes: u64,
    pub fallback_allowed: bool,
    pub model_load_started: bool,
    pub tool_effect_started: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeFailureKind {
    General,
    MultimodalEncode,
    BackendLost,
    ResourceAdmission,
}

impl RuntimeFailure {
    pub fn new(message: impl Into<String>, log: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            log: log.into(),
            kind: RuntimeFailureKind::General,
            code: "runtime_failure".to_string(),
            resource_admission_details: None,
        }
    }

    #[doc(hidden)]
    pub fn into_multimodal_encode(mut self) -> Self {
        self.kind = RuntimeFailureKind::MultimodalEncode;
        self
    }

    #[doc(hidden)]
    pub fn backend_lost(message: impl Into<String>, log: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            log: log.into(),
            kind: RuntimeFailureKind::BackendLost,
            code: "backend_lost".to_string(),
            resource_admission_details: None,
        }
    }

    /// A typed fail-closed resource-admission outcome which did not create or
    /// invalidate a native worker generation.
    pub fn resource_admission(
        code: impl Into<String>,
        message: impl Into<String>,
        log: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            log: log.into(),
            kind: RuntimeFailureKind::ResourceAdmission,
            code: code.into(),
            resource_admission_details: None,
        }
    }

    /// A typed pre-load resource rejection with the exact reviewed profile,
    /// device snapshot, allocation components and no-fallback guarantees.
    pub fn resource_admission_with_details(
        code: impl Into<String>,
        message: impl Into<String>,
        log: impl Into<String>,
        details: ResourceAdmissionDetails,
    ) -> Self {
        Self {
            message: message.into(),
            log: log.into(),
            kind: RuntimeFailureKind::ResourceAdmission,
            code: code.into(),
            resource_admission_details: Some(Box::new(details)),
        }
    }

    /// A typed resource/protocol failure after native allocation. The worker
    /// generation has already been reaped, so manager-owned remote handles
    /// must be discarded just as they are for any other backend loss.
    pub fn reaped_resource_generation(
        code: impl Into<String>,
        message: impl Into<String>,
        log: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            log: log.into(),
            kind: RuntimeFailureKind::BackendLost,
            code: code.into(),
            resource_admission_details: None,
        }
    }

    #[doc(hidden)]
    pub fn is_backend_lost(&self) -> bool {
        self.kind == RuntimeFailureKind::BackendLost
    }

    pub(crate) fn is_multimodal_encode(&self) -> bool {
        self.kind == RuntimeFailureKind::MultimodalEncode
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn kind(&self) -> RuntimeFailureKind {
        self.kind
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn resource_admission_details(&self) -> Option<&ResourceAdmissionDetails> {
        self.resource_admission_details.as_deref()
    }

    pub(crate) fn is_resource_admission(&self) -> bool {
        self.kind == RuntimeFailureKind::ResourceAdmission
            || (self.kind == RuntimeFailureKind::BackendLost && self.code != "backend_lost")
    }

    pub fn log(&self) -> &str {
        &self.log
    }
}

impl fmt::Display for RuntimeFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeFailure {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelGeneration {
    pub content: String,
    pub finish_reason: InferenceFinishReason,
    pub selected_device: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_admission: Option<ResourceAdmissionDetails>,
}
