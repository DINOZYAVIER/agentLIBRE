use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::InferenceOutputEvent;

use super::{Result, WorkerProtocolError, WorkerProtocolErrorCode};

pub const WORKER_BINARY_NAME: &str = "agl-inference-worker";
pub const WORKER_FRAME_VERSION: u16 = 1;
pub const WORKER_PROTOCOL_ID: &str = concat!("agl-inference-worker.v1/", env!("CARGO_PKG_VERSION"));
pub const WORKER_BUILD_ID: &str = env!("AGL_INFERENCE_WORKER_BUILD_ID");
/// Dedicated worker exit status after a typed device-loss receipt cannot be
/// relied upon to reach the host. Unlike stderr text, this closed status is an
/// authoritative worker/host contract.
pub const WORKER_DEVICE_LOST_EXIT_STATUS: i32 = 86;

pub const MAX_CONTROL_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_CONTROL_DESCRIPTORS: usize = 8;
pub const MAX_SEALED_PAYLOAD_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_DEVICE_SNAPSHOT_ENTRIES: usize = 32;
pub const MAX_WORKER_MODELS: usize = 64;
pub const MAX_WORKER_CONTEXTS: usize = 256;
pub const MAX_SANDBOX_PATHS_PER_CLASS: usize = 256;
pub const MAX_SANDBOX_TOTAL_PATH_BYTES: usize = 128 * 1024;
pub const MAX_PROTOCOL_LABEL_BYTES: usize = 256;
pub const MAX_DEVICE_DESCRIPTION_BYTES: usize = 1024;
pub const MAX_WORKER_FAILURE_MESSAGE_BYTES: usize = 2048;
pub const MAX_WORKER_LOG_FIELDS: usize = 16;
pub const MAX_WORKER_LOG_CODE_BYTES: usize = 96;
pub const MAX_WORKER_LOG_FIELD_KEY_BYTES: usize = 64;
pub const MAX_WORKER_LOG_FIELD_VALUE_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerIdentity {
    protocol_id: String,
    build_id: String,
}

impl WorkerIdentity {
    pub fn new(protocol_id: impl Into<String>, build_id: impl Into<String>) -> Self {
        Self {
            protocol_id: protocol_id.into(),
            build_id: build_id.into(),
        }
    }

    pub fn current() -> Self {
        Self {
            protocol_id: WORKER_PROTOCOL_ID.to_owned(),
            build_id: WORKER_BUILD_ID.to_owned(),
        }
    }

    pub fn protocol_id(&self) -> &str {
        &self.protocol_id
    }

    pub fn build_id(&self) -> &str {
        &self.build_id
    }

    pub fn validate_exact(&self) -> std::result::Result<(), HandshakeRejectionCode> {
        if self.protocol_id != WORKER_PROTOCOL_ID {
            return Err(HandshakeRejectionCode::ProtocolMismatch);
        }
        if self.build_id != WORKER_BUILD_ID {
            return Err(HandshakeRejectionCode::BuildMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolLimits {
    control_frame_bytes: u32,
    control_descriptors: u16,
    sealed_payload_bytes: u64,
}

impl ProtocolLimits {
    pub const fn new(
        control_frame_bytes: u32,
        control_descriptors: u16,
        sealed_payload_bytes: u64,
    ) -> Self {
        Self {
            control_frame_bytes,
            control_descriptors,
            sealed_payload_bytes,
        }
    }

    pub const fn current() -> Self {
        Self {
            control_frame_bytes: MAX_CONTROL_FRAME_BYTES as u32,
            control_descriptors: MAX_CONTROL_DESCRIPTORS as u16,
            sealed_payload_bytes: MAX_SEALED_PAYLOAD_BYTES,
        }
    }

    pub const fn control_frame_bytes(self) -> u32 {
        self.control_frame_bytes
    }

    pub const fn control_descriptors(self) -> u16 {
        self.control_descriptors
    }

    pub const fn sealed_payload_bytes(self) -> u64 {
        self.sealed_payload_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handshake {
    identity: WorkerIdentity,
    limits: ProtocolLimits,
}

impl Handshake {
    pub const fn new(identity: WorkerIdentity, limits: ProtocolLimits) -> Self {
        Self { identity, limits }
    }

    pub fn current() -> Self {
        Self {
            identity: WorkerIdentity::current(),
            limits: ProtocolLimits::current(),
        }
    }

    pub fn identity(&self) -> &WorkerIdentity {
        &self.identity
    }

    pub const fn limits(&self) -> ProtocolLimits {
        self.limits
    }

    pub fn validate_exact(&self) -> std::result::Result<(), HandshakeRejectionCode> {
        self.identity.validate_exact()?;
        if self.limits != ProtocolLimits::current() {
            return Err(HandshakeRejectionCode::LimitMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCapabilities {
    sealed_payloads: bool,
    native_cancellation: bool,
    device_inventory: bool,
    resource_inventory: bool,
    status: bool,
    sandbox_admission: bool,
}

impl WorkerCapabilities {
    pub const fn current() -> Self {
        Self {
            sealed_payloads: true,
            native_cancellation: true,
            device_inventory: true,
            resource_inventory: true,
            status: true,
            sandbox_admission: true,
        }
    }

    pub const fn sealed_payloads(self) -> bool {
        self.sealed_payloads
    }

    pub const fn native_cancellation(self) -> bool {
        self.native_cancellation
    }

    pub const fn device_inventory(self) -> bool {
        self.device_inventory
    }

    pub const fn resource_inventory(self) -> bool {
        self.resource_inventory
    }

    pub const fn status(self) -> bool {
        self.status
    }

    pub const fn sandbox_admission(self) -> bool {
        self.sandbox_admission
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Cpu,
    DiscreteGpu,
    IntegratedGpu,
    Accelerator,
    Metadata,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceSnapshotEntry {
    /// Stable physical-device identity, not a display name.
    device_id: String,
    driver_build_id: String,
    /// Backend selector understood by the exact native runtime generation.
    /// This is presentation/selection data and never an authority identity.
    backend_name: String,
    description: String,
    kind: DeviceKind,
    free_memory_bytes: u64,
    total_memory_bytes: u64,
    usable: bool,
    supports_gpu_offload: bool,
}

impl DeviceSnapshotEntry {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device_id: impl Into<String>,
        driver_build_id: impl Into<String>,
        backend_name: impl Into<String>,
        description: impl Into<String>,
        kind: DeviceKind,
        free_memory_bytes: u64,
        total_memory_bytes: u64,
        usable: bool,
        supports_gpu_offload: bool,
    ) -> Result<Self> {
        let entry = Self {
            device_id: device_id.into(),
            driver_build_id: driver_build_id.into(),
            backend_name: backend_name.into(),
            description: description.into(),
            kind,
            free_memory_bytes,
            total_memory_bytes,
            usable,
            supports_gpu_offload,
        };
        entry.validate()?;
        Ok(entry)
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn backend_name(&self) -> &str {
        &self.backend_name
    }

    pub fn driver_build_id(&self) -> &str {
        &self.driver_build_id
    }

    pub const fn kind(&self) -> DeviceKind {
        self.kind
    }

    pub const fn free_memory_bytes(&self) -> u64 {
        self.free_memory_bytes
    }

    pub const fn total_memory_bytes(&self) -> u64 {
        self.total_memory_bytes
    }

    pub const fn usable(&self) -> bool {
        self.usable
    }

    pub const fn supports_gpu_offload(&self) -> bool {
        self.supports_gpu_offload
    }

    fn validate(&self) -> Result<()> {
        validate_label("device_id", &self.device_id, MAX_PROTOCOL_LABEL_BYTES)?;
        validate_label(
            "driver_build_id",
            &self.driver_build_id,
            MAX_PROTOCOL_LABEL_BYTES,
        )?;
        validate_label("backend_name", &self.backend_name, MAX_PROTOCOL_LABEL_BYTES)?;
        validate_label(
            "device description",
            &self.description,
            MAX_DEVICE_DESCRIPTION_BYTES,
        )?;
        if self.free_memory_bytes > self.total_memory_bytes {
            return Err(invalid_message(
                "device snapshot free memory exceeds total memory",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceSnapshot {
    devices: Vec<DeviceSnapshotEntry>,
}

impl DeviceSnapshot {
    pub fn new(devices: Vec<DeviceSnapshotEntry>) -> Result<Self> {
        let snapshot = Self { devices };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn empty() -> Self {
        Self::default()
    }

    pub fn devices(&self) -> &[DeviceSnapshotEntry] {
        &self.devices
    }

    fn validate(&self) -> Result<()> {
        if self.devices.len() > MAX_DEVICE_SNAPSHOT_ENTRIES {
            return Err(invalid_message(format!(
                "device snapshot has {} entries; the limit is {MAX_DEVICE_SNAPSHOT_ENTRIES}",
                self.devices.len()
            )));
        }
        let mut identities = BTreeSet::new();
        for device in &self.devices {
            device.validate()?;
            if !identities.insert(device.device_id()) {
                return Err(invalid_message(
                    "device snapshot contains duplicate device IDs",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    identity: WorkerIdentity,
    native_bundle_id: String,
    limits: ProtocolLimits,
    tools: WorkerCapabilities,
    device_snapshot: DeviceSnapshot,
}

impl Ready {
    pub fn current() -> Self {
        Self::with_native_bundle_id(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        )
        .expect("an empty current worker device snapshot is valid")
    }

    pub fn with_device_snapshot(device_snapshot: DeviceSnapshot) -> Result<Self> {
        Self::with_native_bundle_id_and_device_snapshot(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            device_snapshot,
        )
    }

    pub fn with_native_bundle_id(native_bundle_id: impl Into<String>) -> Result<Self> {
        Self::with_native_bundle_id_and_device_snapshot(native_bundle_id, DeviceSnapshot::empty())
    }

    pub fn with_native_bundle_id_and_device_snapshot(
        native_bundle_id: impl Into<String>,
        device_snapshot: DeviceSnapshot,
    ) -> Result<Self> {
        device_snapshot.validate()?;
        let ready = Self {
            identity: WorkerIdentity::current(),
            native_bundle_id: native_bundle_id.into(),
            limits: ProtocolLimits::current(),
            tools: WorkerCapabilities::current(),
            device_snapshot,
        };
        validate_sha256_identity("native_bundle_id", &ready.native_bundle_id)?;
        Ok(ready)
    }

    pub fn identity(&self) -> &WorkerIdentity {
        &self.identity
    }

    pub fn native_bundle_id(&self) -> &str {
        &self.native_bundle_id
    }

    pub const fn limits(&self) -> ProtocolLimits {
        self.limits
    }

    pub const fn tools(&self) -> WorkerCapabilities {
        self.tools
    }

    pub fn device_snapshot(&self) -> &DeviceSnapshot {
        &self.device_snapshot
    }

    pub fn validate_exact(&self) -> Result<()> {
        self.identity.validate_exact().map_err(identity_error)?;
        validate_sha256_identity("native_bundle_id", &self.native_bundle_id)?;
        if self.limits != ProtocolLimits::current() {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::IdentityMismatch,
                "inference worker protocol limits do not match the host generation",
            ));
        }
        if self.tools != WorkerCapabilities::current() {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::IdentityMismatch,
                "inference worker tools do not match the host generation",
            ));
        }
        self.device_snapshot.validate()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandshakeRejectionCode {
    ProtocolMismatch,
    BuildMismatch,
    LimitMismatch,
}

impl HandshakeRejectionCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProtocolMismatch => "protocol_mismatch",
            Self::BuildMismatch => "build_mismatch",
            Self::LimitMismatch => "limit_mismatch",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandshakeRejected {
    code: HandshakeRejectionCode,
}

impl HandshakeRejected {
    pub const fn new(code: HandshakeRejectionCode) -> Self {
        Self { code }
    }

    pub const fn code(self) -> HandshakeRejectionCode {
        self.code
    }
}

macro_rules! opaque_id {
    ($name:ident, $label:literal) => {
        #[derive(
            Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
        )]
        #[serde(try_from = "u64", into = "u64")]
        pub struct $name(u64);

        impl $name {
            pub fn new(value: u64) -> Result<Self> {
                Self::try_from(value).map_err(invalid_message)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl TryFrom<u64> for $name {
            type Error = &'static str;

            fn try_from(value: u64) -> std::result::Result<Self, Self::Error> {
                if value == 0 {
                    Err(concat!($label, " must be non-zero"))
                } else {
                    Ok(Self(value))
                }
            }
        }

        impl From<$name> for u64 {
            fn from(value: $name) -> Self {
                value.get()
            }
        }
    };
}

opaque_id!(OperationId, "operation ID");
opaque_id!(ModelResourceId, "model resource ID");
opaque_id!(ContextResourceId, "context resource ID");

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedPayload {
    pub(crate) descriptor_index: u16,
    pub(crate) byte_len: u64,
    pub(crate) sha256: [u8; 32],
}

impl SealedPayload {
    pub const fn descriptor_index(&self) -> u16 {
        self.descriptor_index
    }

    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }

    pub const fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfiguration {
    model_roots: Vec<String>,
    projector_roots: Vec<String>,
    runtime_roots: Vec<String>,
    gpu_device_paths: Vec<String>,
    private_temp_root: String,
}

impl SandboxConfiguration {
    pub fn new(
        model_roots: Vec<String>,
        projector_roots: Vec<String>,
        runtime_roots: Vec<String>,
        gpu_device_paths: Vec<String>,
        private_temp_root: impl Into<String>,
    ) -> Result<Self> {
        let configuration = Self {
            model_roots,
            projector_roots,
            runtime_roots,
            gpu_device_paths,
            private_temp_root: private_temp_root.into(),
        };
        configuration.validate()?;
        Ok(configuration)
    }

    pub fn model_roots(&self) -> &[String] {
        &self.model_roots
    }

    pub fn projector_roots(&self) -> &[String] {
        &self.projector_roots
    }

    pub fn runtime_roots(&self) -> &[String] {
        &self.runtime_roots
    }

    pub fn gpu_device_paths(&self) -> &[String] {
        &self.gpu_device_paths
    }

    pub fn private_temp_root(&self) -> &str {
        &self.private_temp_root
    }

    fn validate(&self) -> Result<()> {
        let classes = [
            ("model_roots", self.model_roots.as_slice()),
            ("projector_roots", self.projector_roots.as_slice()),
            ("runtime_roots", self.runtime_roots.as_slice()),
            ("gpu_device_paths", self.gpu_device_paths.as_slice()),
        ];
        let mut total_bytes = self.private_temp_root.len();
        validate_absolute_path("private_temp_root", &self.private_temp_root)?;
        for (name, paths) in classes {
            if paths.len() > MAX_SANDBOX_PATHS_PER_CLASS {
                return Err(invalid_message(format!(
                    "sandbox {name} has {} entries; the limit is {MAX_SANDBOX_PATHS_PER_CLASS}",
                    paths.len()
                )));
            }
            let mut unique = BTreeSet::new();
            for path in paths {
                validate_absolute_path(name, path)?;
                if !unique.insert(path) {
                    return Err(invalid_message(format!(
                        "sandbox {name} contains a duplicate path"
                    )));
                }
                total_bytes = total_bytes.checked_add(path.len()).ok_or_else(|| {
                    invalid_message("sandbox path byte count overflowed its bound")
                })?;
            }
        }
        if total_bytes > MAX_SANDBOX_TOTAL_PATH_BYTES {
            return Err(invalid_message(format!(
                "sandbox configuration uses {total_bytes} path bytes; the limit is {MAX_SANDBOX_TOTAL_PATH_BYTES}"
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadedModelInventoryEntry {
    resource_id: ModelResourceId,
    model_key_digest: String,
}

impl LoadedModelInventoryEntry {
    pub fn new(resource_id: ModelResourceId, model_key_digest: impl Into<String>) -> Result<Self> {
        let entry = Self {
            resource_id,
            model_key_digest: model_key_digest.into(),
        };
        validate_digest(&entry.model_key_digest)?;
        Ok(entry)
    }

    pub const fn resource_id(&self) -> ModelResourceId {
        self.resource_id
    }

    pub fn model_key_digest(&self) -> &str {
        &self.model_key_digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveContextInventoryEntry {
    resource_id: ContextResourceId,
    model_resource_id: ModelResourceId,
    context_key_digest: String,
}

impl LiveContextInventoryEntry {
    pub fn new(
        resource_id: ContextResourceId,
        model_resource_id: ModelResourceId,
        context_key_digest: impl Into<String>,
    ) -> Result<Self> {
        let entry = Self {
            resource_id,
            model_resource_id,
            context_key_digest: context_key_digest.into(),
        };
        validate_digest(&entry.context_key_digest)?;
        Ok(entry)
    }

    pub const fn resource_id(&self) -> ContextResourceId {
        self.resource_id
    }

    pub const fn model_resource_id(&self) -> ModelResourceId {
        self.model_resource_id
    }

    pub fn context_key_digest(&self) -> &str {
        &self.context_key_digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventorySnapshot {
    devices: DeviceSnapshot,
    loaded_models: Vec<LoadedModelInventoryEntry>,
    live_contexts: Vec<LiveContextInventoryEntry>,
}

impl InventorySnapshot {
    pub fn new(
        devices: DeviceSnapshot,
        loaded_models: Vec<LoadedModelInventoryEntry>,
        live_contexts: Vec<LiveContextInventoryEntry>,
    ) -> Result<Self> {
        let snapshot = Self {
            devices,
            loaded_models,
            live_contexts,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn devices(&self) -> &DeviceSnapshot {
        &self.devices
    }

    pub fn loaded_models(&self) -> &[LoadedModelInventoryEntry] {
        &self.loaded_models
    }

    pub fn live_contexts(&self) -> &[LiveContextInventoryEntry] {
        &self.live_contexts
    }

    fn validate(&self) -> Result<()> {
        self.devices.validate()?;
        if self.loaded_models.len() > MAX_WORKER_MODELS {
            return Err(invalid_message("worker inventory exceeds the model bound"));
        }
        if self.live_contexts.len() > MAX_WORKER_CONTEXTS {
            return Err(invalid_message(
                "worker inventory exceeds the context bound",
            ));
        }
        let mut models = BTreeSet::new();
        for model in &self.loaded_models {
            validate_digest(model.model_key_digest())?;
            if !models.insert(model.resource_id()) {
                return Err(invalid_message(
                    "worker inventory contains duplicate model resource IDs",
                ));
            }
        }
        let mut contexts = BTreeSet::new();
        for context in &self.live_contexts {
            validate_digest(context.context_key_digest())?;
            if !models.contains(&context.model_resource_id()) {
                return Err(invalid_message(
                    "worker inventory context references an absent model",
                ));
            }
            if !contexts.insert(context.resource_id()) {
                return Err(invalid_message(
                    "worker inventory contains duplicate context resource IDs",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerHealth {
    Ready,
    Busy,
    ShuttingDown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOperationKind {
    ConfigureSandbox,
    Inventory,
    Status,
    LoadModel,
    CreateContext,
    Generate,
    ClearContext,
    ReleaseContext,
    ReleaseModel,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveOperationStatus {
    operation_id: OperationId,
    kind: WorkerOperationKind,
}

impl ActiveOperationStatus {
    pub const fn new(operation_id: OperationId, kind: WorkerOperationKind) -> Self {
        Self { operation_id, kind }
    }

    pub const fn operation_id(self) -> OperationId {
        self.operation_id
    }

    pub const fn kind(self) -> WorkerOperationKind {
        self.kind
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerStatusSnapshot {
    health: WorkerHealth,
    loaded_models: u16,
    live_contexts: u16,
    queued_commands: u16,
    active_operation: Option<ActiveOperationStatus>,
    completed_operations: u64,
    failed_operations: u64,
    cancellation_requests: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocationReceipt {
    model_bytes: u64,
    context_bytes: u64,
    transient_bytes: u64,
    device_id: Option<String>,
}

impl AllocationReceipt {
    pub fn new(
        model_bytes: u64,
        context_bytes: u64,
        transient_bytes: u64,
        device_id: Option<String>,
    ) -> Result<Self> {
        let receipt = Self {
            model_bytes,
            context_bytes,
            transient_bytes,
            device_id,
        };
        receipt.validate()?;
        Ok(receipt)
    }

    pub const fn model_bytes(&self) -> u64 {
        self.model_bytes
    }

    pub const fn context_bytes(&self) -> u64 {
        self.context_bytes
    }

    pub const fn transient_bytes(&self) -> u64 {
        self.transient_bytes
    }

    pub fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref()
    }

    pub fn total_bytes(&self) -> Result<u64> {
        self.model_bytes
            .checked_add(self.context_bytes)
            .and_then(|bytes| bytes.checked_add(self.transient_bytes))
            .ok_or_else(|| invalid_message("worker allocation receipt byte count overflowed"))
    }

    fn validate(&self) -> Result<()> {
        let _ = self.total_bytes()?;
        if let Some(device_id) = &self.device_id {
            validate_label("allocation device_id", device_id, MAX_PROTOCOL_LABEL_BYTES)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerLogField {
    key: String,
    value: String,
}

impl WorkerLogField {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Result<Self> {
        let field = Self {
            key: key.into(),
            value: value.into(),
        };
        field.validate()?;
        Ok(field)
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    fn validate(&self) -> Result<()> {
        validate_label(
            "worker log field key",
            &self.key,
            MAX_WORKER_LOG_FIELD_KEY_BYTES,
        )?;
        validate_label_allow_empty(
            "worker log field value",
            &self.value,
            MAX_WORKER_LOG_FIELD_VALUE_BYTES,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerLogRecord {
    level: WorkerLogLevel,
    code: String,
    fields: Vec<WorkerLogField>,
}

impl WorkerLogRecord {
    pub fn new(
        level: WorkerLogLevel,
        code: impl Into<String>,
        fields: Vec<WorkerLogField>,
    ) -> Result<Self> {
        let record = Self {
            level,
            code: code.into(),
            fields,
        };
        record.validate()?;
        Ok(record)
    }

    pub const fn level(&self) -> WorkerLogLevel {
        self.level
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn fields(&self) -> &[WorkerLogField] {
        &self.fields
    }

    fn validate(&self) -> Result<()> {
        validate_label("worker log code", &self.code, MAX_WORKER_LOG_CODE_BYTES)?;
        if self.fields.len() > MAX_WORKER_LOG_FIELDS {
            return Err(invalid_message("worker log record exceeds the field bound"));
        }
        let mut keys = BTreeSet::new();
        for field in &self.fields {
            field.validate()?;
            if !keys.insert(field.key()) {
                return Err(invalid_message(
                    "worker log record contains duplicate field keys",
                ));
            }
        }
        Ok(())
    }
}

impl WorkerStatusSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        health: WorkerHealth,
        loaded_models: usize,
        live_contexts: usize,
        queued_commands: usize,
        active_operation: Option<ActiveOperationStatus>,
        completed_operations: u64,
        failed_operations: u64,
        cancellation_requests: u64,
    ) -> Result<Self> {
        if loaded_models > MAX_WORKER_MODELS
            || live_contexts > MAX_WORKER_CONTEXTS
            || queued_commands > MAX_WORKER_CONTEXTS
        {
            return Err(invalid_message("worker status count exceeds its bound"));
        }
        Ok(Self {
            health,
            loaded_models: loaded_models as u16,
            live_contexts: live_contexts as u16,
            queued_commands: queued_commands as u16,
            active_operation,
            completed_operations,
            failed_operations,
            cancellation_requests,
        })
    }

    pub const fn health(&self) -> WorkerHealth {
        self.health
    }

    pub const fn loaded_models(&self) -> u16 {
        self.loaded_models
    }

    pub const fn live_contexts(&self) -> u16 {
        self.live_contexts
    }

    pub const fn queued_commands(&self) -> u16 {
        self.queued_commands
    }

    pub const fn active_operation(&self) -> Option<ActiveOperationStatus> {
        self.active_operation
    }

    pub const fn completed_operations(&self) -> u64 {
        self.completed_operations
    }

    pub const fn failed_operations(&self) -> u64 {
        self.failed_operations
    }

    pub const fn cancellation_requests(&self) -> u64 {
        self.cancellation_requests
    }

    fn validate(&self) -> Result<()> {
        if usize::from(self.loaded_models) > MAX_WORKER_MODELS
            || usize::from(self.live_contexts) > MAX_WORKER_CONTEXTS
            || usize::from(self.queued_commands) > MAX_WORKER_CONTEXTS
        {
            return Err(invalid_message("worker status count exceeds its bound"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFailureCode {
    SandboxNotConfigured,
    SandboxAlreadyConfigured,
    InvalidRequest,
    DuplicateOperation,
    ResourceConflict,
    ResourceNotFound,
    ResourceMismatch,
    ResourceBusy,
    ResourceLimit,
    CancelTargetNotActive,
    Cancelled,
    DeadlineExceeded,
    DeviceLost,
    RuntimeFailure,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerFailure {
    code: WorkerFailureCode,
    message: String,
}

impl WorkerFailure {
    pub fn new(code: WorkerFailureCode, message: impl Into<String>) -> Result<Self> {
        let failure = Self {
            code,
            message: message.into(),
        };
        failure.validate()?;
        Ok(failure)
    }

    pub fn bounded(code: WorkerFailureCode, message: impl AsRef<str>) -> Self {
        let message = truncate_utf8(message.as_ref(), MAX_WORKER_FAILURE_MESSAGE_BYTES);
        Self { code, message }
    }

    pub const fn code(&self) -> WorkerFailureCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    fn validate(&self) -> Result<()> {
        validate_label(
            "worker failure message",
            &self.message,
            MAX_WORKER_FAILURE_MESSAGE_BYTES,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    Requested,
    HostShutdown,
    Upgrade,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shutdown {
    reason: ShutdownReason,
}

impl Shutdown {
    pub const fn new(reason: ShutdownReason) -> Self {
        Self { reason }
    }

    pub const fn reason(self) -> ShutdownReason {
        self.reason
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownComplete {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum HostCommand {
    Handshake(Handshake),
    ConfigureSandbox {
        operation_id: OperationId,
        configuration: SandboxConfiguration,
    },
    Inventory {
        operation_id: OperationId,
    },
    Status {
        operation_id: OperationId,
    },
    LoadModel {
        operation_id: OperationId,
        model_resource_id: ModelResourceId,
        job: SealedPayload,
    },
    CreateContext {
        operation_id: OperationId,
        model_resource_id: ModelResourceId,
        context_resource_id: ContextResourceId,
        job: SealedPayload,
    },
    Generate {
        operation_id: OperationId,
        model_resource_id: ModelResourceId,
        context_resource_id: ContextResourceId,
        job: SealedPayload,
    },
    ClearContext {
        operation_id: OperationId,
        context_resource_id: ContextResourceId,
    },
    ReleaseContext {
        operation_id: OperationId,
        context_resource_id: ContextResourceId,
    },
    ReleaseModel {
        operation_id: OperationId,
        model_resource_id: ModelResourceId,
    },
    Cancel {
        operation_id: OperationId,
        target_operation_id: OperationId,
    },
    Shutdown(Shutdown),
}

impl HostCommand {
    pub const fn operation_id(&self) -> Option<OperationId> {
        match self {
            Self::Handshake(_) | Self::Shutdown(_) => None,
            Self::ConfigureSandbox { operation_id, .. }
            | Self::Inventory { operation_id }
            | Self::Status { operation_id }
            | Self::LoadModel { operation_id, .. }
            | Self::CreateContext { operation_id, .. }
            | Self::Generate { operation_id, .. }
            | Self::ClearContext { operation_id, .. }
            | Self::ReleaseContext { operation_id, .. }
            | Self::ReleaseModel { operation_id, .. }
            | Self::Cancel { operation_id, .. } => Some(*operation_id),
        }
    }

    pub const fn operation_kind(&self) -> Option<WorkerOperationKind> {
        match self {
            Self::Handshake(_) | Self::Shutdown(_) => None,
            Self::ConfigureSandbox { .. } => Some(WorkerOperationKind::ConfigureSandbox),
            Self::Inventory { .. } => Some(WorkerOperationKind::Inventory),
            Self::Status { .. } => Some(WorkerOperationKind::Status),
            Self::LoadModel { .. } => Some(WorkerOperationKind::LoadModel),
            Self::CreateContext { .. } => Some(WorkerOperationKind::CreateContext),
            Self::Generate { .. } => Some(WorkerOperationKind::Generate),
            Self::ClearContext { .. } => Some(WorkerOperationKind::ClearContext),
            Self::ReleaseContext { .. } => Some(WorkerOperationKind::ReleaseContext),
            Self::ReleaseModel { .. } => Some(WorkerOperationKind::ReleaseModel),
            Self::Cancel { .. } => Some(WorkerOperationKind::Cancel),
        }
    }

    pub(crate) fn validate_descriptor_contract(&self, descriptor_count: usize) -> Result<()> {
        let expected = match self {
            Self::LoadModel { job, .. }
            | Self::CreateContext { job, .. }
            | Self::Generate { job, .. } => {
                validate_payload_manifest(job, 0)?;
                1
            }
            _ => 0,
        };
        validate_descriptor_count(expected, descriptor_count)?;
        if let Self::ConfigureSandbox { configuration, .. } = self {
            configuration.validate()?;
        }
        if let Self::Cancel {
            operation_id,
            target_operation_id,
        } = self
            && operation_id == target_operation_id
        {
            return Err(invalid_message(
                "cancel operation cannot target its own operation ID",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerEvent {
    Ready(Ready),
    HandshakeRejected(HandshakeRejected),
    SandboxReady {
        operation_id: OperationId,
    },
    Inventory {
        operation_id: OperationId,
        snapshot: InventorySnapshot,
    },
    Status {
        operation_id: OperationId,
        snapshot: WorkerStatusSnapshot,
    },
    ModelLoaded {
        operation_id: OperationId,
        model_resource_id: ModelResourceId,
        log: Option<SealedPayload>,
    },
    ContextCreated {
        operation_id: OperationId,
        model_resource_id: ModelResourceId,
        context_resource_id: ContextResourceId,
        log: Option<SealedPayload>,
    },
    ContextCleared {
        operation_id: OperationId,
        context_resource_id: ContextResourceId,
        log: Option<SealedPayload>,
    },
    ContextReleased {
        operation_id: OperationId,
        context_resource_id: ContextResourceId,
    },
    ModelReleased {
        operation_id: OperationId,
        model_resource_id: ModelResourceId,
    },
    CancelAccepted {
        operation_id: OperationId,
        target_operation_id: OperationId,
    },
    Started {
        operation_id: OperationId,
        allocation_receipt: AllocationReceipt,
    },
    Output {
        operation_id: OperationId,
        event: InferenceOutputEvent,
    },
    Log {
        operation_id: Option<OperationId>,
        record: WorkerLogRecord,
    },
    Completed {
        operation_id: OperationId,
        result: SealedPayload,
        log: Option<SealedPayload>,
    },
    Failed {
        operation_id: OperationId,
        failure: WorkerFailure,
        log: Option<SealedPayload>,
    },
    ShutdownComplete(ShutdownComplete),
}

impl WorkerEvent {
    pub const fn operation_id(&self) -> Option<OperationId> {
        match self {
            Self::Ready(_)
            | Self::HandshakeRejected(_)
            | Self::Log {
                operation_id: None, ..
            }
            | Self::ShutdownComplete(_) => None,
            Self::SandboxReady { operation_id }
            | Self::Inventory { operation_id, .. }
            | Self::Status { operation_id, .. }
            | Self::ModelLoaded { operation_id, .. }
            | Self::ContextCreated { operation_id, .. }
            | Self::ContextCleared { operation_id, .. }
            | Self::ContextReleased { operation_id, .. }
            | Self::ModelReleased { operation_id, .. }
            | Self::CancelAccepted { operation_id, .. }
            | Self::Started { operation_id, .. }
            | Self::Output { operation_id, .. }
            | Self::Log {
                operation_id: Some(operation_id),
                ..
            }
            | Self::Completed { operation_id, .. }
            | Self::Failed { operation_id, .. } => Some(*operation_id),
        }
    }

    pub const fn is_operation_terminal(&self) -> bool {
        matches!(
            self,
            Self::SandboxReady { .. }
                | Self::Inventory { .. }
                | Self::Status { .. }
                | Self::ModelLoaded { .. }
                | Self::ContextCreated { .. }
                | Self::ContextCleared { .. }
                | Self::ContextReleased { .. }
                | Self::ModelReleased { .. }
                | Self::CancelAccepted { .. }
                | Self::Completed { .. }
                | Self::Failed { .. }
        )
    }

    pub(crate) fn validate_descriptor_contract(&self, descriptor_count: usize) -> Result<()> {
        let expected = match self {
            Self::ModelLoaded { log, .. }
            | Self::ContextCreated { log, .. }
            | Self::ContextCleared { log, .. }
            | Self::Failed { log, .. } => validate_optional_log(log, 0)?,
            Self::Completed { result, log, .. } => {
                validate_payload_manifest(result, 0)?;
                match log {
                    Some(log) => {
                        validate_payload_manifest(log, 1)?;
                        2
                    }
                    None => 1,
                }
            }
            _ => 0,
        };
        validate_descriptor_count(expected, descriptor_count)?;
        match self {
            Self::Ready(ready) => ready.validate_exact()?,
            Self::Inventory { snapshot, .. } => snapshot.validate()?,
            Self::Status { snapshot, .. } => snapshot.validate()?,
            Self::Started {
                allocation_receipt, ..
            } => allocation_receipt.validate()?,
            Self::Log { record, .. } => record.validate()?,
            Self::Failed { failure, .. } => failure.validate()?,
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Frame<T> {
    pub frame_version: u16,
    pub sequence: u64,
    pub message: T,
}

impl<T> Frame<T> {
    pub fn new(sequence: u64, message: T) -> Self {
        Self {
            frame_version: WORKER_FRAME_VERSION,
            sequence,
            message,
        }
    }
}

fn validate_optional_log(log: &Option<SealedPayload>, index: u16) -> Result<usize> {
    match log {
        Some(log) => {
            validate_payload_manifest(log, index)?;
            Ok(1)
        }
        None => Ok(0),
    }
}

fn validate_payload_manifest(payload: &SealedPayload, index: u16) -> Result<()> {
    if payload.descriptor_index() != index {
        return Err(invalid_message(format!(
            "sealed payload descriptor index must be {index}, received {}",
            payload.descriptor_index()
        )));
    }
    if payload.byte_len() > MAX_SEALED_PAYLOAD_BYTES {
        return Err(WorkerProtocolError::new(
            WorkerProtocolErrorCode::PayloadTooLarge,
            "sealed payload manifest exceeds the payload bound",
        ));
    }
    Ok(())
}

fn validate_descriptor_count(expected: usize, actual: usize) -> Result<()> {
    if actual != expected {
        return Err(WorkerProtocolError::new(
            WorkerProtocolErrorCode::UnexpectedDescriptors,
            format!(
                "inference worker message requires exactly {expected} descriptors, received {actual}"
            ),
        ));
    }
    Ok(())
}

fn validate_absolute_path(name: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_DEVICE_DESCRIPTION_BYTES * 4
        || value.as_bytes().contains(&0)
        || !Path::new(value).is_absolute()
    {
        return Err(invalid_message(format!(
            "sandbox {name} entry must be a bounded absolute UTF-8 path"
        )));
    }
    Ok(())
}

fn validate_digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_message(
            "worker resource digest must be 64 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

fn validate_sha256_identity(name: &str, value: &str) -> Result<()> {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return Err(invalid_message(format!(
            "{name} must be a canonical sha256 identity"
        )));
    };
    validate_digest(digest)
}

fn validate_label(name: &str, value: &str, maximum: usize) -> Result<()> {
    if value.is_empty() || value.len() > maximum {
        return Err(invalid_message(format!(
            "{name} must contain between 1 and {maximum} UTF-8 bytes"
        )));
    }
    Ok(())
}

fn validate_label_allow_empty(name: &str, value: &str, maximum: usize) -> Result<()> {
    if value.len() > maximum {
        return Err(invalid_message(format!(
            "{name} must contain at most {maximum} UTF-8 bytes"
        )));
    }
    Ok(())
}

fn truncate_utf8(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn identity_error(code: HandshakeRejectionCode) -> WorkerProtocolError {
    WorkerProtocolError::new(
        WorkerProtocolErrorCode::IdentityMismatch,
        format!(
            "inference worker identity does not match the host generation: {}",
            code.as_str()
        ),
    )
}

fn invalid_message(message: impl Into<String>) -> WorkerProtocolError {
    WorkerProtocolError::new(WorkerProtocolErrorCode::MalformedFrame, message)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn current_identity_and_limits_validate_exactly() {
        assert_eq!(Handshake::current().validate_exact(), Ok(()));
        assert!(WORKER_BUILD_ID.starts_with("sha256:"));
        assert_eq!(WORKER_BUILD_ID.len(), "sha256:".len() + 64);
        assert_eq!(Ready::current().validate_exact(), Ok(()));
    }

    #[test]
    fn mismatched_identity_or_limits_are_rejected() {
        let mut handshake = Handshake::current();
        handshake.identity.protocol_id.push_str("-old");
        assert_eq!(
            handshake.validate_exact(),
            Err(HandshakeRejectionCode::ProtocolMismatch)
        );

        let mut handshake = Handshake::current();
        handshake.identity.build_id.push_str("-other");
        assert_eq!(
            handshake.validate_exact(),
            Err(HandshakeRejectionCode::BuildMismatch)
        );

        let mut handshake = Handshake::current();
        handshake.limits.control_descriptors -= 1;
        assert_eq!(
            handshake.validate_exact(),
            Err(HandshakeRejectionCode::LimitMismatch)
        );
    }

    #[test]
    fn opaque_ids_and_device_snapshots_reject_invalid_wire_values() {
        assert!(OperationId::new(0).is_err());
        assert!(serde_json::from_value::<OperationId>(json!(0)).is_err());
        assert!(
            DeviceSnapshotEntry::new(
                "gpu0",
                "driver-test",
                "Vulkan0",
                "GPU",
                DeviceKind::DiscreteGpu,
                11,
                10,
                true,
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn sandbox_configuration_requires_bounded_absolute_unique_paths() {
        assert!(
            SandboxConfiguration::new(
                vec!["/models".to_string()],
                Vec::new(),
                vec!["/runtime".to_string()],
                vec!["/dev/dri/renderD128".to_string()],
                "/tmp/agl-worker",
            )
            .is_ok()
        );
        assert!(
            SandboxConfiguration::new(
                vec!["relative".to_string()],
                Vec::new(),
                Vec::new(),
                Vec::new(),
                "/tmp/agl-worker",
            )
            .is_err()
        );
    }

    #[test]
    fn worker_failure_is_bounded_on_utf8_boundaries() {
        let failure = WorkerFailure::bounded(
            WorkerFailureCode::RuntimeFailure,
            "🦀".repeat(MAX_WORKER_FAILURE_MESSAGE_BYTES),
        );
        assert!(failure.message().len() <= MAX_WORKER_FAILURE_MESSAGE_BYTES);
        assert!(failure.validate().is_ok());
    }
}
