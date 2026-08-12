use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agl_model::HostCapabilityDevice;
use thiserror::Error;

use super::descriptors::DescriptorSetError;
use super::resource_ledger::{LiveAdmissionRejection, ResourcePools};

const MAX_MEDIA_BYTES_PER_ATTEMPT: u64 = 64 * 1024 * 1024;
const MAX_PRIVATE_REQUEST_BYTES: u64 = 72 * 1024 * 1024;
const MAX_MEDIA_PARTS_PER_ATTEMPT: usize = agl_content::MAX_CONTENT_PARTS;
const MEDIA_TRANSPORT_OVERHEAD_PER_PART: u64 = 4 * 1024;

#[derive(Clone, Debug)]
pub struct ResolvedMediaAttachment {
    pub reference: agl_content::ContentAttachmentRef,
    pub bytes: Arc<[u8]>,
}

impl ResolvedMediaAttachment {
    pub fn new(
        reference: agl_content::ContentAttachmentRef,
        bytes: impl Into<Arc<[u8]>>,
    ) -> Result<Self, InferenceFailure> {
        let bytes = bytes.into();
        reference
            .validate()
            .map_err(|error| InferenceFailure::InvalidMedia {
                reason: error.to_string(),
            })?;
        let actual_length =
            u64::try_from(bytes.len()).map_err(|_| InferenceFailure::InvalidMedia {
                reason: "attachment length exceeds u64".to_owned(),
            })?;
        if reference.byte_length != actual_length
            || reference.digest != agl_content::BlobDigest::from_bytes(&bytes)
        {
            return Err(InferenceFailure::InvalidMedia {
                reason: format!(
                    "attachment {} bytes do not match its reference",
                    reference.content_attachment_id
                ),
            });
        }
        if !matches!(
            reference.media_type,
            agl_content::MediaType::ImagePng | agl_content::MediaType::ImageJpeg
        ) {
            return Err(InferenceFailure::InvalidMedia {
                reason: format!(
                    "attachment {} has unsupported inference media type {}",
                    reference.content_attachment_id,
                    reference.media_type.mime()
                ),
            });
        }
        Ok(Self { reference, bytes })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineInventory {
    pub physical_host_bytes: u64,
    pub physical_cpu_cores: usize,
    pub logical_cpu_cores: usize,
    pub devices: Vec<HostCapabilityDevice>,
    pub runtime_devices: Vec<EngineDeviceRuntimeIdentity>,
    pub llama_cpp_commit: String,
    pub executable: EngineExecutable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineDeviceRuntimeIdentity {
    pub identity: String,
    pub description: String,
    pub native_device_id: String,
    pub driver_build_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineExecutable {
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferenceHostConfig {
    pub executable: EngineExecutable,
    pub queue_capacity: usize,
    pub external_host_reserve_bytes: u64,
    pub authority_root: PathBuf,
    pub context_idle_duration: Duration,
    pub model_idle_duration: Duration,
    pub evidence_root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum InferenceHostStartError {
    #[error("invalid engine inventory: {reason}")]
    InvalidEngineInventory { reason: String },
    #[error("host or selected-device lifetime lease is unavailable: {reason}")]
    LeaseUnavailable { reason: String },
    #[error("failed to start the inference engine: {reason}")]
    EngineStart { reason: String },
    #[error("llama-server digest mismatch: expected {expected_sha256}, got {actual_sha256}")]
    ExecutableIdentityMismatch {
        expected_sha256: String,
        actual_sha256: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum InferenceQueueRejection {
    #[error("inference queue is full (capacity {capacity})")]
    Full { capacity: usize, retryable: bool },
    #[error("inference host is shutting down")]
    ShuttingDown,
}

#[derive(Debug, Error)]
pub enum InferenceFailure {
    #[error(transparent)]
    Admission(#[from] LiveAdmissionRejection),
    #[error(transparent)]
    Queue(#[from] InferenceQueueRejection),
    #[error("artifact descriptor set is invalid: {0}")]
    DescriptorSet(#[from] DescriptorSetError),
    #[error("artifact descriptor changed while hashing: {basename}")]
    DescriptorChanged { basename: String },
    #[error("engine protocol failed: {reason}")]
    EngineProtocol { reason: String },
    #[error("allocation receipt does not match the admitted identities: {reason}")]
    InvalidAllocationReceipt {
        reason: String,
        admitted: ResourcePools,
        reported: ResourcePools,
    },
    #[error("inference engine identity is cooling down until {not_before_unix_ms}")]
    CoolingDown { not_before_unix_ms: u64 },
    #[error("inference resource estimate is quarantined for {identity}")]
    Quarantined { identity: String },
    #[error("durable inference health authority failed: {reason}")]
    HealthAuthority { reason: String },
    #[error("context exceeds the exact profile: {required_tokens} > {context_tokens}")]
    ContextOverflow {
        required_tokens: u64,
        context_tokens: u64,
    },
    #[error("model is busy: {model_key}")]
    Busy { model_key: String },
    #[error("invalid inference media: {reason}")]
    InvalidMedia { reason: String },
    #[error("inference attempt was cancelled")]
    Cancelled,
    #[error("inference attempt deadline was exceeded")]
    DeadlineExceeded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferenceHostStatus {
    pub shutting_down: bool,
    pub pending: usize,
    pub active: usize,
    pub resident_models: usize,
    pub resident_contexts: usize,
    pub resident_model_digest: Option<String>,
    pub reserved: ResourcePools,
    pub authority_leases: usize,
}

#[derive(Clone)]
pub struct VolatileHandles {
    pub media: Vec<ResolvedMediaAttachment>,
    pub cancellation: crate::InferenceCancellation,
    pub deadline: Option<Instant>,
    pub output_sink: Arc<dyn crate::InferenceOutputSink>,
    pub evidence_root: Option<PathBuf>,
    pub product_resolution: Option<serde_json::Value>,
}

impl Default for VolatileHandles {
    fn default() -> Self {
        Self {
            media: Vec::new(),
            cancellation: crate::InferenceCancellation::new(),
            deadline: None,
            output_sink: Arc::new(crate::NoopInferenceOutputSink),
            evidence_root: None,
            product_resolution: None,
        }
    }
}

impl std::fmt::Debug for VolatileHandles {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VolatileHandles")
            .field("media_parts", &self.media.len())
            .field("cancellation", &self.cancellation)
            .field("deadline", &self.deadline)
            .field("output_sink", &"<volatile>")
            .field("evidence_root", &self.evidence_root)
            .field(
                "product_resolution",
                &self.product_resolution.as_ref().map(|_| "<typed-json>"),
            )
            .finish()
    }
}

impl VolatileHandles {
    pub(super) fn media_accounting(
        &self,
        request: &crate::InferenceRequest,
    ) -> Result<MediaAccounting, InferenceFailure> {
        if self.media.len() > MAX_MEDIA_PARTS_PER_ATTEMPT {
            return Err(InferenceFailure::InvalidMedia {
                reason: format!(
                    "attachment count {} exceeds {}",
                    self.media.len(),
                    MAX_MEDIA_PARTS_PER_ATTEMPT
                ),
            });
        }
        let expected = request
            .rendered
            .messages
            .iter()
            .filter_map(|message| message.content.as_ref())
            .flat_map(agl_content::Content::attachments)
            .collect::<Vec<_>>();
        if expected.len() != self.media.len()
            || expected
                .iter()
                .zip(&self.media)
                .any(|(expected, actual)| *expected != &actual.reference)
        {
            return Err(InferenceFailure::InvalidMedia {
                reason: "resolved media does not exactly match ordered request attachments"
                    .to_owned(),
            });
        }
        let mut resolved_bytes = 0_u64;
        let mut decoder_allowance_bytes = 0_u64;
        for media in &self.media {
            let checked = Self::check_media(media)?;
            resolved_bytes = resolved_bytes
                .checked_add(checked.reference.byte_length)
                .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?;
            let image = checked
                .reference
                .image
                .ok_or_else(|| InferenceFailure::InvalidMedia {
                    reason: "image attachment omitted dimensions".to_owned(),
                })?;
            decoder_allowance_bytes = decoder_allowance_bytes
                .checked_add(
                    image
                        .pixels()
                        .checked_mul(4)
                        .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?,
                )
                .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?;
        }
        if resolved_bytes > MAX_MEDIA_BYTES_PER_ATTEMPT {
            return Err(InferenceFailure::InvalidMedia {
                reason: format!(
                    "resolved media bytes {resolved_bytes} exceed {MAX_MEDIA_BYTES_PER_ATTEMPT}"
                ),
            });
        }
        let request_bytes = u64::try_from(
            serde_json::to_vec(request)
                .map_err(|error| InferenceFailure::InvalidMedia {
                    reason: format!("failed to size inference request: {error}"),
                })?
                .len(),
        )
        .map_err(|_| LiveAdmissionRejection::ArithmeticOverflow)?;
        let part_overhead = u64::try_from(self.media.len())
            .map_err(|_| LiveAdmissionRejection::ArithmeticOverflow)?
            .checked_mul(MEDIA_TRANSPORT_OVERHEAD_PER_PART)
            .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?;
        let transport_bytes = resolved_bytes
            .checked_add(request_bytes)
            .and_then(|value| value.checked_add(part_overhead))
            .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?;
        if transport_bytes > MAX_PRIVATE_REQUEST_BYTES {
            return Err(InferenceFailure::InvalidMedia {
                reason: format!(
                    "private request bytes {transport_bytes} exceed {MAX_PRIVATE_REQUEST_BYTES}"
                ),
            });
        }
        let admitted_host_bytes = resolved_bytes
            .checked_add(transport_bytes)
            .and_then(|value| value.checked_add(decoder_allowance_bytes))
            .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?;
        Ok(MediaAccounting {
            resolved_bytes,
            transport_bytes,
            decoder_allowance_bytes,
            admitted_host_bytes,
        })
    }

    fn check_media(
        media: &ResolvedMediaAttachment,
    ) -> Result<ResolvedMediaAttachment, InferenceFailure> {
        ResolvedMediaAttachment::new(media.reference.clone(), Arc::clone(&media.bytes))
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct MediaAccounting {
    pub(super) resolved_bytes: u64,
    pub(super) transport_bytes: u64,
    pub(super) decoder_allowance_bytes: u64,
    pub(super) admitted_host_bytes: u64,
}
