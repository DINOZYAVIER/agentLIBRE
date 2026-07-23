use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use agl_config::ResolvedInferenceConfig;
use agl_content::{ArtifactRef, ContentPart};
use agl_ids::{AttemptId, RequestId, RunId, SessionId, TurnId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::evidence::InferenceArtifactRoot;
use crate::{InferenceOutputSink, InferenceRequest};

const MAX_RESOLVED_IMAGES: usize = 8;
const MAX_RESOLVED_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RESOLVED_MEDIA_BYTES: u64 = 64 * 1024 * 1024;

pub const DEFAULT_MAX_LOADED_MODELS: usize = 1;
pub const DEFAULT_MAX_CONTEXTS_PER_MODEL: usize = 2;
pub const DEFAULT_MODEL_MANAGER_QUEUE_CAPACITY: usize = 32;
pub const DEFAULT_CONTEXT_IDLE_DURATION: Duration = Duration::from_secs(15 * 60);
pub const DEFAULT_MODEL_IDLE_DURATION: Duration = Duration::from_secs(5 * 60);
pub const MAX_STATUS_MODEL_DIGESTS: usize = 16;

const MAX_LOADED_MODELS: usize = 64;
const MAX_CONTEXTS_PER_MODEL: usize = 64;
const MAX_QUEUE_CAPACITY: usize = 4096;
const MAX_IDLE_DURATION: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelManagerOptions {
    pub max_loaded_models: usize,
    pub max_contexts_per_model: usize,
    pub queue_capacity: usize,
    pub context_idle_duration: Duration,
    pub model_idle_duration: Duration,
    pub model_lease_root: Option<PathBuf>,
}

impl Default for ModelManagerOptions {
    fn default() -> Self {
        Self {
            max_loaded_models: DEFAULT_MAX_LOADED_MODELS,
            max_contexts_per_model: DEFAULT_MAX_CONTEXTS_PER_MODEL,
            queue_capacity: DEFAULT_MODEL_MANAGER_QUEUE_CAPACITY,
            context_idle_duration: DEFAULT_CONTEXT_IDLE_DURATION,
            model_idle_duration: DEFAULT_MODEL_IDLE_DURATION,
            model_lease_root: None,
        }
    }
}

impl ModelManagerOptions {
    pub fn with_model_lease_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.model_lease_root = Some(path.into());
        self
    }

    pub fn with_residency_durations(
        mut self,
        context_idle_duration: Duration,
        model_idle_duration: Duration,
    ) -> Self {
        self.context_idle_duration = context_idle_duration;
        self.model_idle_duration = model_idle_duration;
        self
    }

    pub fn validate(&self) -> Result<(), ModelManagerError> {
        validate_bounded(
            "max_loaded_models",
            self.max_loaded_models,
            MAX_LOADED_MODELS,
        )?;
        validate_bounded(
            "max_contexts_per_model",
            self.max_contexts_per_model,
            MAX_CONTEXTS_PER_MODEL,
        )?;
        validate_bounded("queue_capacity", self.queue_capacity, MAX_QUEUE_CAPACITY)?;
        validate_idle_duration("context_idle_duration", self.context_idle_duration)?;
        validate_idle_duration("model_idle_duration", self.model_idle_duration)?;
        if self
            .model_lease_root
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(ModelManagerError::InvalidOptions {
                message: "model_lease_root cannot be empty".to_string(),
            });
        }
        Ok(())
    }
}

fn validate_idle_duration(name: &str, value: Duration) -> Result<(), ModelManagerError> {
    if value < Duration::from_secs(1) || value > MAX_IDLE_DURATION || value.subsec_nanos() != 0 {
        return Err(ModelManagerError::InvalidOptions {
            message: format!(
                "{name} must be a whole number of seconds between 1 and {}",
                MAX_IDLE_DURATION.as_secs()
            ),
        });
    }
    Ok(())
}

fn validate_bounded(name: &str, value: usize, maximum: usize) -> Result<(), ModelManagerError> {
    if value == 0 || value > maximum {
        return Err(ModelManagerError::InvalidOptions {
            message: format!("{name} must be between 1 and {maximum}"),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ModelKey(String);

impl ModelKey {
    pub fn from_config(config: &ResolvedInferenceConfig) -> Result<Self, ModelManagerError> {
        config
            .validate()
            .map_err(|error| ModelManagerError::ProfileInvalid {
                message: format!("{error:#}"),
            })?;
        let draft = config.runtime.mtp.enabled.then_some(ModelDraftIdentity {
            model: config.runtime.mtp.draft_model.as_ref(),
            gpu_layers: config
                .runtime
                .mtp
                .gpu_layers
                .unwrap_or(config.runtime.gpu_layers),
        });
        let identity = ModelLoadIdentity {
            backend: config.backend.kind.as_str(),
            model: &config.backend.model,
            multimodal_projector: config.backend.multimodal_projector.as_deref(),
            gpu_layers: config.runtime.gpu_layers,
            device: config.runtime.device.as_deref(),
            mmap: config.runtime.mmap,
            draft,
        };
        let normalized =
            serde_json::to_vec(&identity).map_err(|error| ModelManagerError::ProfileInvalid {
                message: format!("failed to normalize inference profile: {error}"),
            })?;
        Ok(Self(sha256_hex(&normalized)))
    }

    pub fn digest(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.digest())
    }
}

fn validate_model_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContextKey {
    model_key: ModelKey,
    digest: String,
}

impl ContextKey {
    pub fn for_conversation(
        config: &ResolvedInferenceConfig,
        conversation: impl AsRef<str>,
    ) -> Result<Self, ModelManagerError> {
        let conversation = conversation.as_ref();
        if conversation.trim().is_empty() {
            return Err(ModelManagerError::ProfileInvalid {
                message: "conversation context key cannot be empty".to_string(),
            });
        }
        let model_key = ModelKey::from_config(config)?;
        let mut hasher = Sha256::new();
        hasher.update(b"agentlibre.context-key.v1\0");
        hasher.update(model_key.digest().as_bytes());
        hasher.update(b"\0");
        let normalized = serde_json::to_vec(&config.runtime).map_err(|error| {
            ModelManagerError::ProfileInvalid {
                message: format!("failed to normalize inference context: {error}"),
            }
        })?;
        hasher.update(&normalized);
        hasher.update(b"\0");
        hasher.update(conversation.as_bytes());
        let digest = hex_digest(hasher.finalize().as_slice());
        Ok(Self { model_key, digest })
    }

    pub fn model_key(&self) -> &ModelKey {
        &self.model_key
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }
}

impl fmt::Display for ContextKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.digest())
    }
}

#[derive(Clone, Default)]
pub struct InferenceCancellation {
    cancelled: Arc<AtomicBool>,
}

impl InferenceCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl fmt::Debug for InferenceCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InferenceCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InferenceJobScope {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub attempt_id: AttemptId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
}

#[derive(Clone)]
pub struct InferenceJob {
    config: ResolvedInferenceConfig,
    request: InferenceRequest,
    model_key: ModelKey,
    context_key: ContextKey,
    artifact_root: InferenceArtifactRoot,
    content_store_root: PathBuf,
    resolved_content: Option<ResolvedModelContent>,
    max_output_tokens: u32,
    deadline: Option<Instant>,
    cancellation: InferenceCancellation,
    output_sink: Arc<dyn InferenceOutputSink>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerJobPayload {
    config: ResolvedInferenceConfig,
    request: InferenceRequest,
    model_key_digest: String,
    context_key_digest: String,
    resolved_content: ResolvedModelContent,
    max_output_tokens: u32,
    deadline_after_ms: Option<u64>,
}

impl fmt::Debug for InferenceJob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InferenceJob")
            .field("config", &self.config)
            .field("request", &self.request)
            .field("model_key", &self.model_key)
            .field("context_key", &self.context_key)
            .field("artifact_root", &self.artifact_root)
            .field("content_store_root", &self.content_store_root)
            .field("resolved_content", &self.resolved_content)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("deadline", &self.deadline)
            .field("cancellation", &self.cancellation)
            .field("output_sink", &"<volatile>")
            .finish()
    }
}

impl InferenceJob {
    pub fn new(
        config: ResolvedInferenceConfig,
        request: InferenceRequest,
        context_key: ContextKey,
        artifact_root: InferenceArtifactRoot,
        content_store_root: PathBuf,
        max_output_tokens: u32,
        output_sink: Arc<dyn InferenceOutputSink>,
    ) -> Result<Self, ModelManagerError> {
        let model_key = ModelKey::from_config(&config)?;
        if context_key.model_key() != &model_key {
            return Err(ModelManagerError::ProfileInvalid {
                message: "context key does not match the inference profile".to_string(),
            });
        }
        if max_output_tokens == 0 {
            return Err(ModelManagerError::ProfileInvalid {
                message: "max_output_tokens must be positive".to_string(),
            });
        }
        if request.run_id != request.rendered.run_id || request.turn_id != request.rendered.turn_id
        {
            return Err(ModelManagerError::ProfileInvalid {
                message: "inference scope does not match the rendered request".to_string(),
            });
        }
        if content_store_root.as_os_str().is_empty() {
            return Err(ModelManagerError::ProfileInvalid {
                message: "content store root cannot be empty".to_string(),
            });
        }
        validate_content_profile(&config, &request)?;
        Ok(Self {
            config,
            request,
            model_key,
            context_key,
            artifact_root,
            content_store_root,
            resolved_content: None,
            max_output_tokens,
            deadline: None,
            cancellation: InferenceCancellation::new(),
            output_sink,
        })
    }

    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    pub fn with_cancellation(mut self, cancellation: InferenceCancellation) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn config(&self) -> &ResolvedInferenceConfig {
        &self.config
    }

    pub fn request(&self) -> &InferenceRequest {
        &self.request
    }

    pub fn model_key(&self) -> &ModelKey {
        &self.model_key
    }

    pub fn context_key(&self) -> &ContextKey {
        &self.context_key
    }

    pub fn artifact_root(&self) -> &InferenceArtifactRoot {
        &self.artifact_root
    }

    pub fn resolved_content(&self) -> Option<&ResolvedModelContent> {
        self.resolved_content.as_ref()
    }

    pub fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn cancellation(&self) -> &InferenceCancellation {
        &self.cancellation
    }

    pub fn output_sink(&self) -> &dyn InferenceOutputSink {
        self.output_sink.as_ref()
    }

    pub(super) fn output_sink_handle(&self) -> Arc<dyn InferenceOutputSink> {
        Arc::clone(&self.output_sink)
    }

    pub(super) fn replace_output_sink(&mut self, output_sink: Arc<dyn InferenceOutputSink>) {
        self.output_sink = output_sink;
    }

    pub fn scope(&self) -> InferenceJobScope {
        InferenceJobScope {
            run_id: self.request.run_id.clone(),
            turn_id: self.request.turn_id.clone(),
            attempt_id: self.request.attempt_id.clone(),
            session_id: self.request.session_id.clone(),
            request_id: self.request.request_id.clone(),
        }
    }

    pub fn deadline_exceeded(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub fn should_abort(&self) -> bool {
        self.cancellation.is_cancelled() || self.deadline_exceeded()
    }

    pub(crate) fn worker_payload(
        &self,
        now: Instant,
    ) -> Result<WorkerJobPayload, ModelManagerError> {
        let resolved_content =
            self.resolved_content
                .clone()
                .ok_or_else(|| ModelManagerError::UnsupportedContent {
                    message: "inference worker dispatch requires host-resolved content".to_string(),
                })?;
        Ok(WorkerJobPayload {
            config: self.config.clone(),
            request: self.request.clone(),
            model_key_digest: self.model_key.digest().to_string(),
            context_key_digest: self.context_key.digest().to_string(),
            resolved_content,
            max_output_tokens: self.max_output_tokens,
            deadline_after_ms: self.deadline.map(|deadline| {
                u64::try_from(deadline.saturating_duration_since(now).as_millis())
                    .unwrap_or(u64::MAX)
            }),
        })
    }

    #[doc(hidden)]
    pub fn encode_worker_payload(&self, now: Instant) -> Result<Vec<u8>, ModelManagerError> {
        serde_json::to_vec(&self.worker_payload(now)?).map_err(|error| {
            ModelManagerError::ProfileInvalid {
                message: format!("failed to encode inference worker job: {error}"),
            }
        })
    }

    pub(crate) fn from_worker_payload(
        payload: WorkerJobPayload,
        cancellation: InferenceCancellation,
        output_sink: Arc<dyn InferenceOutputSink>,
        now: Instant,
    ) -> Result<Self, ModelManagerError> {
        let model_key = ModelKey::from_config(&payload.config)?;
        if model_key.digest() != payload.model_key_digest
            || !valid_digest(&payload.context_key_digest)
        {
            return Err(ModelManagerError::ProfileInvalid {
                message: "inference worker job identity is invalid".to_string(),
            });
        }
        let context_key = ContextKey {
            model_key,
            digest: payload.context_key_digest,
        };
        let mut job = Self::new(
            payload.config,
            payload.request,
            context_key,
            InferenceArtifactRoot::new("/worker-private/evidence-disabled"),
            PathBuf::from("/worker-private/content-store-disabled"),
            payload.max_output_tokens,
            output_sink,
        )?
        .with_cancellation(cancellation);
        job.resolved_content = Some(payload.resolved_content);
        if let Some(after_ms) = payload.deadline_after_ms {
            let deadline = now
                .checked_add(Duration::from_millis(after_ms))
                .ok_or_else(|| ModelManagerError::ProfileInvalid {
                    message: "inference worker deadline overflows monotonic time".to_string(),
                })?;
            job = job.with_deadline(deadline);
        }
        Ok(job)
    }

    #[doc(hidden)]
    pub fn decode_worker_payload(
        bytes: &[u8],
        cancellation: InferenceCancellation,
        output_sink: Arc<dyn InferenceOutputSink>,
        now: Instant,
    ) -> Result<Self, ModelManagerError> {
        let payload =
            serde_json::from_slice(bytes).map_err(|error| ModelManagerError::ProfileInvalid {
                message: format!("failed to decode inference worker job: {error}"),
            })?;
        Self::from_worker_payload(payload, cancellation, output_sink, now)
    }

    #[doc(hidden)]
    pub fn resolve_content_for_worker_dispatch(
        &mut self,
    ) -> Result<(usize, u64), ModelManagerError> {
        self.resolve_content()
    }

    pub(super) fn resolve_content(&mut self) -> Result<(usize, u64), ModelManagerError> {
        let mut store = None;
        let mut image_count = 0_usize;
        let mut total_bytes = 0_u64;
        let mut messages = Vec::with_capacity(self.request.rendered.messages.len());
        for message in &self.request.rendered.messages {
            let mut parts = Vec::new();
            if let Some(content) = &message.content {
                for part in &content.parts {
                    match part {
                        ContentPart::Text { text } => {
                            parts.push(ResolvedContentPart::Text { text: text.clone() });
                        }
                        ContentPart::Artifact { artifact } => {
                            image_count = image_count.saturating_add(1);
                            if image_count > MAX_RESOLVED_IMAGES
                                || artifact.byte_length > MAX_RESOLVED_IMAGE_BYTES
                                || total_bytes.saturating_add(artifact.byte_length)
                                    > MAX_RESOLVED_MEDIA_BYTES
                            {
                                return Err(ModelManagerError::UnsupportedContent {
                                    message: "media request exceeds manager resolution limits"
                                        .to_string(),
                                });
                            }
                            if store.is_none() {
                                store = Some(
                                    agl_store::AglStore::open_current_at(&self.content_store_root)
                                        .map_err(|error| {
                                            ModelManagerError::ArtifactUnavailable {
                                                artifact_id: artifact.artifact_id.to_string(),
                                                message: error.to_string(),
                                            }
                                        })?,
                                );
                            }
                            let resolved = store
                                .as_ref()
                                .expect("artifact store was initialized above")
                                .resolve_artifact(&self.request.run_id, artifact)
                                .map_err(|error| map_artifact_error(artifact, error))?;
                            total_bytes = total_bytes.saturating_add(artifact.byte_length);
                            parts.push(ResolvedContentPart::Image {
                                artifact: artifact.clone(),
                                bytes: resolved.bytes,
                            });
                        }
                    }
                }
            }
            messages.push(ResolvedMessageContent { parts });
        }
        self.resolved_content = Some(ResolvedModelContent { messages });
        Ok((image_count, total_bytes))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedModelContent {
    messages: Vec<ResolvedMessageContent>,
}

impl ResolvedModelContent {
    pub fn messages(&self) -> &[ResolvedMessageContent] {
        &self.messages
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedMessageContent {
    parts: Vec<ResolvedContentPart>,
}

impl ResolvedMessageContent {
    pub fn parts(&self) -> &[ResolvedContentPart] {
        &self.parts
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolvedContentPart {
    Text {
        text: String,
    },
    Image {
        artifact: ArtifactRef,
        bytes: Vec<u8>,
    },
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl ResolvedContentPart {
    pub fn image(&self) -> Option<(&ArtifactRef, &[u8])> {
        match self {
            Self::Image { artifact, bytes } => Some((artifact, bytes)),
            Self::Text { .. } => None,
        }
    }
}

impl fmt::Debug for ResolvedContentPart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text { text } => formatter.debug_struct("Text").field("text", text).finish(),
            Self::Image { artifact, bytes } => formatter
                .debug_struct("Image")
                .field("artifact", artifact)
                .field("byte_length", &bytes.len())
                .finish(),
        }
    }
}

fn validate_content_profile(
    config: &ResolvedInferenceConfig,
    request: &InferenceRequest,
) -> Result<(), ModelManagerError> {
    let has_media = request
        .rendered
        .messages
        .iter()
        .filter_map(|message| message.content.as_ref())
        .any(|content| content.has_artifacts());
    if !has_media {
        return Ok(());
    }
    if config.runtime.mtp.enabled {
        return Err(ModelManagerError::UnsupportedContent {
            message: "media requests cannot use speculative MTP".to_string(),
        });
    }
    if config.backend.multimodal_projector.is_none() {
        return Err(ModelManagerError::UnsupportedContent {
            message: "text-only inference profile cannot consume artifact content".to_string(),
        });
    }
    Ok(())
}

fn map_artifact_error(artifact: &ArtifactRef, error: agl_store::StoreError) -> ModelManagerError {
    match error {
        agl_store::StoreError::ArtifactIntegrityFailed { reason, .. } => {
            ModelManagerError::ArtifactIntegrityFailed {
                artifact_id: artifact.artifact_id.to_string(),
                message: reason,
            }
        }
        other => ModelManagerError::ArtifactUnavailable {
            artifact_id: artifact.artifact_id.to_string(),
            message: other.to_string(),
        },
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ModelManagerStatusDetail {
    #[default]
    Aggregate,
    ModelDigests,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelUnloadTarget {
    All,
    Digest(String),
}

impl ModelUnloadTarget {
    pub fn digest(digest: impl Into<String>) -> Result<Self, ModelManagerError> {
        let digest = digest.into();
        if !validate_model_digest(&digest) {
            return Err(ModelManagerError::InvalidUnloadTarget {
                message: "model digest must contain exactly 64 lowercase hexadecimal characters"
                    .to_string(),
            });
        }
        Ok(Self::Digest(digest))
    }

    pub fn validate(&self) -> Result<(), ModelManagerError> {
        match self {
            Self::All => Ok(()),
            Self::Digest(digest) if validate_model_digest(digest) => Ok(()),
            Self::Digest(_) => Err(ModelManagerError::InvalidUnloadTarget {
                message: "model digest must contain exactly 64 lowercase hexadecimal characters"
                    .to_string(),
            }),
        }
    }

    pub(crate) fn matches_digest(&self, digest: &str) -> bool {
        matches!(self, Self::All) || matches!(self, Self::Digest(candidate) if candidate == digest)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelUnloadOutcome {
    Released,
    NotResident,
    Busy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ModelUnloadResult {
    pub matched_models: u32,
    pub released_models: u32,
    pub released_contexts: u32,
    pub outcome: ModelUnloadOutcome,
}

impl ModelUnloadResult {
    pub(crate) const fn busy() -> Self {
        Self {
            matched_models: 0,
            released_models: 0,
            released_contexts: 0,
            outcome: ModelUnloadOutcome::Busy,
        }
    }

    pub(crate) const fn not_resident() -> Self {
        Self {
            matched_models: 0,
            released_models: 0,
            released_contexts: 0,
            outcome: ModelUnloadOutcome::NotResident,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelReleaseReason {
    IdleContext,
    IdleModel,
    Manual,
    Shutdown,
    Capacity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelReleaseOutcome {
    Released,
    Failed,
    BackendLost,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ModelManagerStatus {
    pub queue_depth: usize,
    pub active_scope: Option<InferenceJobScope>,
    pub resident_models: usize,
    pub resident_contexts: usize,
    pub next_residency_deadline_after_ms: Option<u64>,
    pub resident_model_digests: Vec<String>,
    pub resident_model_digests_truncated: bool,
    pub last_release_reason: Option<ModelReleaseReason>,
    pub last_release_outcome: Option<ModelReleaseOutcome>,
    pub automatic_context_unloads: u64,
    pub automatic_model_unloads: u64,
    pub manual_unloads: u64,
    pub unload_failures: u64,
    pub model_loads: u64,
    pub context_loads: u64,
    pub model_evictions: u64,
    pub context_evictions: u64,
    pub completed_jobs: u64,
    pub incomplete_jobs: u64,
    pub cancellations: u64,
    pub deadline_exceeded: u64,
    pub failures: u64,
}

#[derive(Debug, Default)]
pub(super) struct SharedManagerStatus {
    pub(super) status: ModelManagerStatus,
    pub(super) next_residency_deadline: Option<Instant>,
    pub(super) resident_model_digests: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelManagerError {
    InvalidOptions {
        message: String,
    },
    QueueFull {
        capacity: usize,
    },
    InvalidUnloadTarget {
        message: String,
    },
    DeadlineExceeded,
    Cancelled,
    ProfileInvalid {
        message: String,
    },
    ResourceAdmission {
        code: String,
        message: String,
    },
    LoadFailed {
        model_digest: String,
        message: String,
    },
    ContextFailed {
        context_digest: String,
        message: String,
    },
    GenerationFailed {
        message: String,
    },
    UnsupportedContent {
        message: String,
    },
    ArtifactUnavailable {
        artifact_id: String,
        message: String,
    },
    ArtifactIntegrityFailed {
        artifact_id: String,
        message: String,
    },
    MultimodalEncodeFailed {
        message: String,
    },
    DeviceInventoryFailed {
        message: String,
    },
    BackendLost {
        message: String,
    },
    ManagerUnavailable,
}

impl ModelManagerError {
    pub fn retryable(&self) -> bool {
        matches!(self, Self::QueueFull { .. } | Self::ManagerUnavailable)
    }

    pub fn code(&self) -> &str {
        match self {
            Self::InvalidOptions { .. } => "manager.invalid_options",
            Self::QueueFull { .. } => "manager.queue_full",
            Self::InvalidUnloadTarget { .. } => "manager.invalid_unload_target",
            Self::DeadlineExceeded => "manager.deadline_exceeded",
            Self::Cancelled => "manager.cancelled",
            Self::ProfileInvalid { .. } => "manager.profile_invalid",
            Self::ResourceAdmission { code, .. } => code,
            Self::LoadFailed { .. } => "manager.load_failed",
            Self::ContextFailed { .. } => "manager.context_failed",
            Self::GenerationFailed { .. } => "manager.generation_failed",
            Self::UnsupportedContent { .. } => "unsupported_content",
            Self::ArtifactUnavailable { .. } => "artifact_unavailable",
            Self::ArtifactIntegrityFailed { .. } => "artifact_integrity_failed",
            Self::MultimodalEncodeFailed { .. } => "multimodal_encode_failed",
            Self::DeviceInventoryFailed { .. } => "manager.device_inventory_failed",
            Self::BackendLost { .. } => "manager.backend_lost",
            Self::ManagerUnavailable => "manager.unavailable",
        }
    }
}

impl fmt::Display for ModelManagerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOptions { message } => {
                write!(formatter, "invalid model manager options: {message}")
            }
            Self::QueueFull { capacity } => write!(
                formatter,
                "model manager queue is full (capacity {capacity})"
            ),
            Self::InvalidUnloadTarget { message } => {
                write!(formatter, "invalid model unload target: {message}")
            }
            Self::DeadlineExceeded => formatter.write_str("inference deadline exceeded"),
            Self::Cancelled => formatter.write_str("inference job cancelled"),
            Self::ProfileInvalid { message } => {
                write!(formatter, "invalid inference profile: {message}")
            }
            Self::ResourceAdmission { code, message } => {
                write!(
                    formatter,
                    "inference resource admission failed ({code}): {message}"
                )
            }
            Self::LoadFailed {
                model_digest,
                message,
            } => write!(formatter, "model {model_digest} failed to load: {message}"),
            Self::ContextFailed {
                context_digest,
                message,
            } => write!(formatter, "context {context_digest} failed: {message}"),
            Self::GenerationFailed { message } => {
                write!(formatter, "inference generation failed: {message}")
            }
            Self::UnsupportedContent { message } => {
                write!(formatter, "unsupported inference content: {message}")
            }
            Self::ArtifactUnavailable {
                artifact_id,
                message,
            } => write!(
                formatter,
                "artifact {artifact_id} is unavailable: {message}"
            ),
            Self::ArtifactIntegrityFailed {
                artifact_id,
                message,
            } => write!(
                formatter,
                "artifact {artifact_id} failed integrity validation: {message}"
            ),
            Self::MultimodalEncodeFailed { message } => {
                write!(formatter, "multimodal encoding failed: {message}")
            }
            Self::DeviceInventoryFailed { message } => {
                write!(formatter, "inference device inventory failed: {message}")
            }
            Self::BackendLost { message } => {
                write!(formatter, "inference backend was lost: {message}")
            }
            Self::ManagerUnavailable => formatter.write_str("model manager is unavailable"),
        }
    }
}

impl std::error::Error for ModelManagerError {}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes).as_slice())
}

#[derive(Serialize)]
struct ModelLoadIdentity<'a> {
    backend: &'a str,
    model: &'a std::path::Path,
    multimodal_projector: Option<&'a std::path::Path>,
    gpu_layers: u32,
    device: Option<&'a str>,
    mmap: Option<bool>,
    draft: Option<ModelDraftIdentity<'a>>,
}

#[derive(Serialize)]
struct ModelDraftIdentity<'a> {
    model: Option<&'a std::path::PathBuf>,
    gpu_layers: u32,
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}
