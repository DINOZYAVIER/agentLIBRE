//! Private durable worker-health and unsafe-estimate quarantine records.
//!
//! The caller supplies one exact absolute Linux directory. This module never
//! consults environment variables or the process working directory. Records
//! are addressed only by SHA-256 digests, bounded before parsing, and replaced
//! with the usual write/fsync/rename/fsync durability sequence.

use std::error::Error;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
use std::os::linux::fs::MetadataExt as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};

use crate::admission::{AllocationEstimate, AllocationReceipt, ReceiptValidationError};
use crate::worker_supervisor::{
    WorkerCircuitBreakerPolicy, WorkerHealthKey, WorkerHealthState, WorkerHealthStateError,
};

pub const MAX_DURABLE_HEALTH_RECORD_BYTES: usize = 8 * 1024;
pub const MAX_DURABLE_HEALTH_RECORDS: usize = 256;
const MAX_DURABLE_TEMP_FILES: usize = 16;
const SHA256_HEX_BYTES: usize = 64;
const WORKER_RECORD_PREFIX: &str = "worker-health-";
const QUARANTINE_RECORD_PREFIX: &str = "resource-quarantine-";
const RECORD_SUFFIX: &str = ".json";
const TEMP_PREFIX: &str = ".agl-inference-health-tmp-";

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Exact digest-only identity of one resource estimate.
///
/// Raw model paths, prompts, display names and backend diagnostics cannot be
/// represented by this type. Each component is an exact lowercase SHA-256
/// digest supplied by the authority which resolved that identity.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ResourceQuarantineKey {
    model_digest: String,
    config_digest: String,
    physical_device_digest: String,
    driver_digest: String,
    worker_build_digest: String,
}

impl ResourceQuarantineKey {
    pub fn new(
        model_digest: impl Into<String>,
        config_digest: impl Into<String>,
        physical_device_digest: impl Into<String>,
        driver_digest: impl Into<String>,
        worker_build_digest: impl Into<String>,
    ) -> Result<Self, ResourceQuarantineKeyError> {
        let key = Self {
            model_digest: model_digest.into(),
            config_digest: config_digest.into(),
            physical_device_digest: physical_device_digest.into(),
            driver_digest: driver_digest.into(),
            worker_build_digest: worker_build_digest.into(),
        };
        for value in [
            &key.model_digest,
            &key.config_digest,
            &key.physical_device_digest,
            &key.driver_digest,
            &key.worker_build_digest,
        ] {
            if !is_sha256_hex(value.as_bytes()) {
                return Err(ResourceQuarantineKeyError);
            }
        }
        Ok(key)
    }

    pub fn model_digest(&self) -> &str {
        &self.model_digest
    }

    pub fn config_digest(&self) -> &str {
        &self.config_digest
    }

    pub fn physical_device_digest(&self) -> &str {
        &self.physical_device_digest
    }

    pub fn driver_digest(&self) -> &str {
        &self.driver_digest
    }

    pub fn worker_build_digest(&self) -> &str {
        &self.worker_build_digest
    }

    fn same_runtime_identity(&self, other: &Self) -> bool {
        self.physical_device_digest == other.physical_device_digest
            && self.driver_digest == other.driver_digest
            && self.worker_build_digest == other.worker_build_digest
    }

    fn has_replacement_profile(&self, other: &Self) -> bool {
        self.model_digest != other.model_digest || self.config_digest != other.config_digest
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceQuarantineKeyRepr {
    model_digest: String,
    config_digest: String,
    physical_device_digest: String,
    driver_digest: String,
    worker_build_digest: String,
}

impl<'de> Deserialize<'de> for ResourceQuarantineKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ResourceQuarantineKeyRepr::deserialize(deserializer)?;
        Self::new(
            value.model_digest,
            value.config_digest,
            value.physical_device_digest,
            value.driver_digest,
            value.worker_build_digest,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceQuarantineKeyError;

impl ResourceQuarantineKeyError {
    pub fn code(self) -> &'static str {
        "resource_quarantine_identity_invalid"
    }
}

impl fmt::Display for ResourceQuarantineKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("resource quarantine identity must contain five lowercase SHA-256 digests")
    }
}

impl Error for ResourceQuarantineKeyError {}

/// A receipt which proved one exact resource estimate unsafe.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceEstimateQuarantine {
    key: ResourceQuarantineKey,
    admitted: AllocationEstimate,
    reported: AllocationReceipt,
}

impl ResourceEstimateQuarantine {
    pub fn new(
        key: ResourceQuarantineKey,
        admitted: AllocationEstimate,
        reported: AllocationReceipt,
    ) -> Result<Self, ResourceEstimateQuarantineError> {
        match reported.validate_against(admitted) {
            Err(ReceiptValidationError::EnvelopeExceeded { .. }) => Ok(Self {
                key,
                admitted,
                reported,
            }),
            Ok(()) => Err(ResourceEstimateQuarantineError::ReceiptWithinEnvelope),
            Err(ReceiptValidationError::ArithmeticOverflow) => {
                Err(ResourceEstimateQuarantineError::ArithmeticOverflow)
            }
        }
    }

    pub fn key(&self) -> &ResourceQuarantineKey {
        &self.key
    }

    pub fn admitted(&self) -> AllocationEstimate {
        self.admitted
    }

    pub fn reported(&self) -> AllocationReceipt {
        self.reported
    }

    fn validate(&self) -> Result<(), ResourceEstimateQuarantineError> {
        match self.reported.validate_against(self.admitted) {
            Err(ReceiptValidationError::EnvelopeExceeded { .. }) => Ok(()),
            Ok(()) => Err(ResourceEstimateQuarantineError::ReceiptWithinEnvelope),
            Err(ReceiptValidationError::ArithmeticOverflow) => {
                Err(ResourceEstimateQuarantineError::ArithmeticOverflow)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceEstimateQuarantineError {
    ReceiptWithinEnvelope,
    ArithmeticOverflow,
}

impl ResourceEstimateQuarantineError {
    pub fn code(self) -> &'static str {
        "resource_quarantine_invalid"
    }
}

impl fmt::Display for ResourceEstimateQuarantineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReceiptWithinEnvelope => formatter
                .write_str("a resource receipt within its admitted envelope cannot be quarantined"),
            Self::ArithmeticOverflow => {
                formatter.write_str("resource quarantine byte arithmetic overflowed")
            }
        }
    }
}

impl Error for ResourceEstimateQuarantineError {}

/// A pinned, process-safe handle to the private durable store.
#[derive(Debug)]
pub struct DurableHealthStore {
    root: PathBuf,
    root_directory: File,
    thread_lock: Mutex<()>,
}

impl DurableHealthStore {
    /// Opens or creates only the final component of an exact absolute root.
    ///
    /// Every path component is opened with `O_NOFOLLOW`. The final directory
    /// must be owned by the effective UID with exact mode 0700.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, DurableHealthStoreError> {
        let root = root.as_ref();
        let components = validated_absolute_components(root)?;
        let root_directory = open_or_create_store_root(&components)?;
        validate_store_root(&root_directory)?;
        let store = Self {
            root: root.to_path_buf(),
            root_directory,
            thread_lock: Mutex::new(()),
        };
        let _lock = store.exclusive_lock()?;
        store.prepare_locked()?;
        drop(_lock);
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn load_worker_health(
        &self,
        key: &WorkerHealthKey,
        policy: WorkerCircuitBreakerPolicy,
    ) -> Result<Option<WorkerHealthState>, DurableHealthStoreError> {
        let _lock = self.exclusive_lock()?;
        self.prepare_locked()?;
        self.load_worker_health_locked(key, policy)
    }

    pub fn store_worker_health(
        &self,
        state: &WorkerHealthState,
        policy: WorkerCircuitBreakerPolicy,
    ) -> Result<(), DurableHealthStoreError> {
        state
            .validate(policy)
            .map_err(DurableHealthStoreError::InvalidWorkerHealth)?;
        let _lock = self.exclusive_lock()?;
        let inventory = self.prepare_locked()?;
        let exists = self
            .load_worker_health_locked(state.key(), policy)?
            .is_some();
        if !exists && inventory.records >= MAX_DURABLE_HEALTH_RECORDS {
            return Err(DurableHealthStoreError::RecordLimitExceeded);
        }
        if inventory.temporary >= MAX_DURABLE_TEMP_FILES {
            return Err(DurableHealthStoreError::TemporaryFileLimitExceeded);
        }

        let record = WorkerHealthRecord {
            schema: WorkerHealthSchema::V1,
            state: state.clone(),
        };
        let bytes = serialize_record(&record, DurableRecordKind::WorkerHealth)?;
        let name = worker_record_name(state.key())?;
        write_record_atomic(&self.root_directory, &name, &bytes)
    }

    /// Removes a valid record for this exact health generation.
    ///
    /// Corrupt, oversized, insecure or identity-mismatched records are never
    /// deleted by this method; the error must be resolved explicitly.
    pub fn clear_worker_health(
        &self,
        key: &WorkerHealthKey,
        policy: WorkerCircuitBreakerPolicy,
    ) -> Result<bool, DurableHealthStoreError> {
        let _lock = self.exclusive_lock()?;
        self.prepare_locked()?;
        if self.load_worker_health_locked(key, policy)?.is_none() {
            return Ok(false);
        }
        let name = worker_record_name(key)?;
        unlink_record(&self.root_directory, &name)?;
        Ok(true)
    }

    pub fn load_resource_quarantine(
        &self,
        key: &ResourceQuarantineKey,
    ) -> Result<Option<ResourceEstimateQuarantine>, DurableHealthStoreError> {
        let _lock = self.exclusive_lock()?;
        self.prepare_locked()?;
        self.load_resource_quarantine_locked(key)
    }

    pub fn store_resource_quarantine(
        &self,
        quarantine: &ResourceEstimateQuarantine,
    ) -> Result<(), DurableHealthStoreError> {
        quarantine
            .validate()
            .map_err(DurableHealthStoreError::InvalidResourceQuarantine)?;
        let _lock = self.exclusive_lock()?;
        let inventory = self.prepare_locked()?;
        let exists = self
            .load_resource_quarantine_locked(quarantine.key())?
            .is_some();
        if !exists && inventory.records >= MAX_DURABLE_HEALTH_RECORDS {
            return Err(DurableHealthStoreError::RecordLimitExceeded);
        }
        if inventory.temporary >= MAX_DURABLE_TEMP_FILES {
            return Err(DurableHealthStoreError::TemporaryFileLimitExceeded);
        }

        let record = ResourceQuarantineRecord {
            schema: ResourceQuarantineSchema::V1,
            quarantine: quarantine.clone(),
        };
        let bytes = serialize_record(&record, DurableRecordKind::ResourceQuarantine)?;
        let name = quarantine_record_name(quarantine.key())?;
        write_record_atomic(&self.root_directory, &name, &bytes)
    }

    /// Clears an exact quarantine only after a replacement model/profile has
    /// been independently validated for the same device/driver/worker domain.
    /// Changed device or software identities naturally select another key and
    /// are not grounds for deleting the old quarantine.
    pub fn clear_resource_quarantine_after_replacement(
        &self,
        quarantined: &ResourceQuarantineKey,
        validated_replacement: &ResourceQuarantineKey,
    ) -> Result<bool, DurableHealthStoreError> {
        if !quarantined.same_runtime_identity(validated_replacement)
            || !quarantined.has_replacement_profile(validated_replacement)
        {
            return Err(DurableHealthStoreError::InvalidQuarantineClear);
        }

        let _lock = self.exclusive_lock()?;
        self.prepare_locked()?;
        if self.load_resource_quarantine_locked(quarantined)?.is_none() {
            return Ok(false);
        }
        let name = quarantine_record_name(quarantined)?;
        unlink_record(&self.root_directory, &name)?;
        Ok(true)
    }

    fn load_worker_health_locked(
        &self,
        key: &WorkerHealthKey,
        policy: WorkerCircuitBreakerPolicy,
    ) -> Result<Option<WorkerHealthState>, DurableHealthStoreError> {
        let name = worker_record_name(key)?;
        let Some(bytes) = read_record(&self.root_directory, &name)? else {
            return Ok(None);
        };
        let record: WorkerHealthRecord =
            deserialize_record(&bytes, DurableRecordKind::WorkerHealth)?;
        if record.schema != WorkerHealthSchema::V1 || record.state.key() != key {
            return Err(DurableHealthStoreError::RecordIdentityMismatch {
                kind: DurableRecordKind::WorkerHealth,
            });
        }
        record
            .state
            .validate(policy)
            .map_err(DurableHealthStoreError::InvalidWorkerHealth)?;
        Ok(Some(record.state))
    }

    fn load_resource_quarantine_locked(
        &self,
        key: &ResourceQuarantineKey,
    ) -> Result<Option<ResourceEstimateQuarantine>, DurableHealthStoreError> {
        let name = quarantine_record_name(key)?;
        let Some(bytes) = read_record(&self.root_directory, &name)? else {
            return Ok(None);
        };
        let record: ResourceQuarantineRecord =
            deserialize_record(&bytes, DurableRecordKind::ResourceQuarantine)?;
        if record.schema != ResourceQuarantineSchema::V1 || record.quarantine.key() != key {
            return Err(DurableHealthStoreError::RecordIdentityMismatch {
                kind: DurableRecordKind::ResourceQuarantine,
            });
        }
        record
            .quarantine
            .validate()
            .map_err(DurableHealthStoreError::InvalidResourceQuarantine)?;
        Ok(Some(record.quarantine))
    }

    fn exclusive_lock(&self) -> Result<StoreLock<'_>, DurableHealthStoreError> {
        let thread = self
            .thread_lock
            .lock()
            .map_err(|_| DurableHealthStoreError::LockPoisoned)?;
        loop {
            let result = unsafe { libc::flock(self.root_directory.as_raw_fd(), libc::LOCK_EX) };
            if result == 0 {
                return Ok(StoreLock {
                    descriptor: self.root_directory.as_raw_fd(),
                    _thread: thread,
                });
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(DurableHealthStoreError::io(
                    "lock durable health root",
                    error,
                ));
            }
        }
    }

    fn prepare_locked(&self) -> Result<StoreInventory, DurableHealthStoreError> {
        validate_store_root(&self.root_directory)?;
        inspect_store_inventory(&self.root_directory)
    }
}

struct StoreLock<'a> {
    descriptor: RawFd,
    _thread: MutexGuard<'a, ()>,
}

impl Drop for StoreLock<'_> {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.descriptor, libc::LOCK_UN) };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum WorkerHealthSchema {
    #[serde(rename = "agentlibre.inference_worker_health.v1")]
    V1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerHealthRecord {
    schema: WorkerHealthSchema,
    state: WorkerHealthState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum ResourceQuarantineSchema {
    #[serde(rename = "agentlibre.inference_resource_quarantine.v1")]
    V1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceQuarantineRecord {
    schema: ResourceQuarantineSchema,
    quarantine: ResourceEstimateQuarantine,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableRecordKind {
    WorkerHealth,
    ResourceQuarantine,
}

impl fmt::Display for DurableRecordKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkerHealth => formatter.write_str("worker health"),
            Self::ResourceQuarantine => formatter.write_str("resource quarantine"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableStoreSecurityObject {
    Root,
    Record,
    DirectoryEntry,
}

impl fmt::Display for DurableStoreSecurityObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root => formatter.write_str("durable health root"),
            Self::Record => formatter.write_str("durable health record"),
            Self::DirectoryEntry => formatter.write_str("durable health directory entry"),
        }
    }
}

#[derive(Debug)]
pub enum DurableHealthStoreError {
    InvalidRoot,
    SecurityViolation {
        object: DurableStoreSecurityObject,
    },
    RecordLimitExceeded,
    TemporaryFileLimitExceeded,
    RecordTooLarge {
        kind: DurableRecordKind,
    },
    RecordCorrupt {
        kind: DurableRecordKind,
    },
    RecordIdentityMismatch {
        kind: DurableRecordKind,
    },
    InvalidWorkerHealth(WorkerHealthStateError),
    InvalidResourceQuarantine(ResourceEstimateQuarantineError),
    InvalidQuarantineClear,
    Serialization {
        kind: DurableRecordKind,
    },
    LockPoisoned,
    Io {
        operation: &'static str,
        source: io::Error,
    },
}

impl DurableHealthStoreError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRoot => "inference_health_root_invalid",
            Self::SecurityViolation { .. } => "inference_health_store_security",
            Self::RecordLimitExceeded => "inference_health_store_full",
            Self::TemporaryFileLimitExceeded => "inference_health_store_temporary_full",
            Self::RecordTooLarge { .. } => "inference_health_record_oversized",
            Self::RecordCorrupt { .. } => "inference_health_record_corrupt",
            Self::RecordIdentityMismatch { .. } => "inference_health_identity_mismatch",
            Self::InvalidWorkerHealth(_) => "inference_worker_health_invalid",
            Self::InvalidResourceQuarantine(_) => "resource_quarantine_invalid",
            Self::InvalidQuarantineClear => "resource_quarantine_clear_invalid",
            Self::Serialization { .. } => "inference_health_record_serialize",
            Self::LockPoisoned => "inference_health_store_unavailable",
            Self::Io { .. } => "inference_health_store_io",
        }
    }

    fn security(object: DurableStoreSecurityObject) -> Self {
        Self::SecurityViolation { object }
    }

    fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Io { operation, source }
    }
}

impl fmt::Display for DurableHealthStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoot => formatter.write_str(
                "durable health root must be an exact absolute Linux path with an existing parent",
            ),
            Self::SecurityViolation { object } => write!(
                formatter,
                "{object} does not satisfy the same-owner, no-symlink, private-mode requirements",
            ),
            Self::RecordLimitExceeded => {
                formatter.write_str("durable health record count exceeds its bound")
            }
            Self::TemporaryFileLimitExceeded => {
                formatter.write_str("durable health temporary file count exceeds its bound")
            }
            Self::RecordTooLarge { kind } => {
                write!(formatter, "{kind} record exceeds its byte bound")
            }
            Self::RecordCorrupt { kind } => write!(formatter, "{kind} record is corrupt"),
            Self::RecordIdentityMismatch { kind } => {
                write!(formatter, "{kind} record identity does not match its lookup key")
            }
            Self::InvalidWorkerHealth(error) => write!(formatter, "{error}"),
            Self::InvalidResourceQuarantine(error) => write!(formatter, "{error}"),
            Self::InvalidQuarantineClear => formatter.write_str(
                "resource quarantine clear requires a changed model/profile on the same runtime identity",
            ),
            Self::Serialization { kind } => write!(formatter, "failed to serialize {kind} record"),
            Self::LockPoisoned => formatter.write_str("durable health store lock is unavailable"),
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
        }
    }
}

impl Error for DurableHealthStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidWorkerHealth(error) => Some(error),
            Self::InvalidResourceQuarantine(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn serialize_record<T: Serialize>(
    record: &T,
    kind: DurableRecordKind,
) -> Result<Vec<u8>, DurableHealthStoreError> {
    let bytes =
        serde_json::to_vec(record).map_err(|_| DurableHealthStoreError::Serialization { kind })?;
    if bytes.len() > MAX_DURABLE_HEALTH_RECORD_BYTES {
        return Err(DurableHealthStoreError::RecordTooLarge { kind });
    }
    Ok(bytes)
}

fn deserialize_record<'de, T: Deserialize<'de>>(
    bytes: &'de [u8],
    kind: DurableRecordKind,
) -> Result<T, DurableHealthStoreError> {
    serde_json::from_slice(bytes).map_err(|_| DurableHealthStoreError::RecordCorrupt { kind })
}

fn worker_record_name(key: &WorkerHealthKey) -> Result<String, DurableHealthStoreError> {
    Ok(format!(
        "{WORKER_RECORD_PREFIX}{}{RECORD_SUFFIX}",
        serialized_digest(key, DurableRecordKind::WorkerHealth)?
    ))
}

fn quarantine_record_name(key: &ResourceQuarantineKey) -> Result<String, DurableHealthStoreError> {
    Ok(format!(
        "{QUARANTINE_RECORD_PREFIX}{}{RECORD_SUFFIX}",
        serialized_digest(key, DurableRecordKind::ResourceQuarantine)?
    ))
}

fn serialized_digest<T: Serialize>(
    value: &T,
    kind: DurableRecordKind,
) -> Result<String, DurableHealthStoreError> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| DurableHealthStoreError::Serialization { kind })?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = digest.as_ref();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn is_sha256_hex(value: &[u8]) -> bool {
    value.len() == SHA256_HEX_BYTES
        && value
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn validated_absolute_components(root: &Path) -> Result<Vec<OsString>, DurableHealthStoreError> {
    let bytes = root.as_os_str().as_bytes();
    if bytes.len() <= 1
        || bytes.first() != Some(&b'/')
        || bytes.starts_with(b"//")
        || bytes.ends_with(b"/")
    {
        return Err(DurableHealthStoreError::InvalidRoot);
    }

    let mut components = Vec::new();
    for component in bytes[1..].split(|byte| *byte == b'/') {
        if component.is_empty() || component == b"." || component == b".." || component.contains(&0)
        {
            return Err(DurableHealthStoreError::InvalidRoot);
        }
        components.push(OsString::from_vec(component.to_vec()));
    }
    Ok(components)
}

fn open_or_create_store_root(components: &[OsString]) -> Result<File, DurableHealthStoreError> {
    let mut directory = open_filesystem_root()?;
    for (index, component) in components.iter().enumerate() {
        let final_component = index + 1 == components.len();
        match open_directory_at(&directory, component) {
            Ok(next) => directory = next,
            Err(error) if error.kind() == io::ErrorKind::NotFound && final_component => {
                let created = create_directory_at(&directory, component)?;
                let next = open_directory_at(&directory, component).map_err(|error| {
                    classify_root_open_error("open new durable health root", error)
                })?;
                if created && unsafe { libc::fchmod(next.as_raw_fd(), 0o700) } != 0 {
                    return Err(DurableHealthStoreError::io(
                        "set durable health root mode",
                        io::Error::last_os_error(),
                    ));
                }
                directory = next;
            }
            Err(error) => {
                return Err(classify_root_open_error(
                    "open durable health root component",
                    error,
                ));
            }
        }
    }
    Ok(directory)
}

fn open_filesystem_root() -> Result<File, DurableHealthStoreError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open("/")
        .map_err(|error| DurableHealthStoreError::io("open filesystem root", error))
}

fn open_directory_at(parent: &File, component: &OsStr) -> io::Result<File> {
    let component = os_string_to_cstring(component)?;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            component.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn create_directory_at(parent: &File, component: &OsStr) -> Result<bool, DurableHealthStoreError> {
    let component = os_string_to_cstring(component)
        .map_err(|error| DurableHealthStoreError::io("encode durable health root", error))?;
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o700) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::AlreadyExists {
        return Ok(false);
    }
    Err(DurableHealthStoreError::io(
        "create durable health root",
        error,
    ))
}

fn validate_store_root(directory: &File) -> Result<(), DurableHealthStoreError> {
    let metadata = directory
        .metadata()
        .map_err(|error| DurableHealthStoreError::io("inspect durable health root", error))?;
    if !metadata.is_dir()
        || metadata.st_uid() != effective_uid()
        || metadata.st_mode() & 0o7777 != 0o700
        || !descriptor_has_cloexec(directory.as_raw_fd())?
    {
        return Err(DurableHealthStoreError::security(
            DurableStoreSecurityObject::Root,
        ));
    }
    Ok(())
}

fn classify_root_open_error(operation: &'static str, error: io::Error) -> DurableHealthStoreError {
    if is_security_path_error(&error) {
        DurableHealthStoreError::security(DurableStoreSecurityObject::Root)
    } else if error.kind() == io::ErrorKind::NotFound {
        DurableHealthStoreError::InvalidRoot
    } else {
        DurableHealthStoreError::io(operation, error)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct StoreInventory {
    records: usize,
    temporary: usize,
}

fn inspect_store_inventory(directory: &File) -> Result<StoreInventory, DurableHealthStoreError> {
    let duplicate = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(DurableHealthStoreError::io(
            "duplicate durable health root descriptor",
            io::Error::last_os_error(),
        ));
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        let _ = unsafe { libc::close(duplicate) };
        return Err(DurableHealthStoreError::io(
            "open durable health directory stream",
            error,
        ));
    }
    let stream = DirectoryStream(stream);
    let mut inventory = StoreInventory::default();

    loop {
        unsafe { *libc::__errno_location() = 0 };
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error_number = unsafe { *libc::__errno_location() };
            if error_number != 0 {
                return Err(DurableHealthStoreError::io(
                    "read durable health directory",
                    io::Error::from_raw_os_error(error_number),
                ));
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }

        let class = classify_entry_name(bytes).ok_or_else(|| {
            DurableHealthStoreError::security(DurableStoreSecurityObject::DirectoryEntry)
        })?;
        validate_named_entry(directory, name)?;
        match class {
            StoreEntryClass::Record => inventory.records += 1,
            StoreEntryClass::Temporary => inventory.temporary += 1,
        }
        if inventory.records > MAX_DURABLE_HEALTH_RECORDS {
            return Err(DurableHealthStoreError::RecordLimitExceeded);
        }
        if inventory.temporary > MAX_DURABLE_TEMP_FILES {
            return Err(DurableHealthStoreError::TemporaryFileLimitExceeded);
        }
    }
    Ok(inventory)
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        let _ = unsafe { libc::closedir(self.0) };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoreEntryClass {
    Record,
    Temporary,
}

fn classify_entry_name(name: &[u8]) -> Option<StoreEntryClass> {
    if name.starts_with(TEMP_PREFIX.as_bytes())
        && name.len() <= TEMP_PREFIX.len() + 48
        && name[TEMP_PREFIX.len()..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b'-')
    {
        return Some(StoreEntryClass::Temporary);
    }
    for prefix in [WORKER_RECORD_PREFIX, QUARANTINE_RECORD_PREFIX] {
        let expected = prefix.len() + SHA256_HEX_BYTES + RECORD_SUFFIX.len();
        if name.len() == expected
            && name.starts_with(prefix.as_bytes())
            && name.ends_with(RECORD_SUFFIX.as_bytes())
            && is_sha256_hex(&name[prefix.len()..prefix.len() + SHA256_HEX_BYTES])
        {
            return Some(StoreEntryClass::Record);
        }
    }
    None
}

fn validate_named_entry(directory: &File, name: &CStr) -> Result<(), DurableHealthStoreError> {
    let stat = stat_at(directory.as_raw_fd(), name)?;
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG
        || stat.st_uid != effective_uid()
        || stat.st_mode & 0o7777 != 0o600
        || stat.st_nlink != 1
    {
        return Err(DurableHealthStoreError::security(
            DurableStoreSecurityObject::DirectoryEntry,
        ));
    }
    Ok(())
}

fn read_record(directory: &File, name: &str) -> Result<Option<Vec<u8>>, DurableHealthStoreError> {
    let name = CString::new(name)
        .map_err(|_| DurableHealthStoreError::security(DurableStoreSecurityObject::Record))?;
    let flags =
        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY;
    let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        if is_security_path_error(&error)
            || matches!(error.raw_os_error(), Some(libc::EISDIR | libc::ENXIO))
        {
            return Err(DurableHealthStoreError::security(
                DurableStoreSecurityObject::Record,
            ));
        }
        return Err(DurableHealthStoreError::io(
            "open durable health record",
            error,
        ));
    }
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    validate_open_record(directory, &name, &file)?;
    let metadata = file
        .metadata()
        .map_err(|error| DurableHealthStoreError::io("inspect durable health record", error))?;
    if metadata.st_size() > MAX_DURABLE_HEALTH_RECORD_BYTES as u64 {
        return Err(DurableHealthStoreError::RecordTooLarge {
            kind: record_kind_from_name(name.to_bytes()),
        });
    }

    let mut bytes = Vec::with_capacity(metadata.st_size() as usize);
    (&mut file)
        .take((MAX_DURABLE_HEALTH_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| DurableHealthStoreError::io("read durable health record", error))?;
    if bytes.len() > MAX_DURABLE_HEALTH_RECORD_BYTES {
        return Err(DurableHealthStoreError::RecordTooLarge {
            kind: record_kind_from_name(name.to_bytes()),
        });
    }
    validate_open_record(directory, &name, &file)?;
    Ok(Some(bytes))
}

fn record_kind_from_name(name: &[u8]) -> DurableRecordKind {
    if name.starts_with(WORKER_RECORD_PREFIX.as_bytes()) {
        DurableRecordKind::WorkerHealth
    } else {
        DurableRecordKind::ResourceQuarantine
    }
}

fn validate_open_record(
    directory: &File,
    name: &CStr,
    file: &File,
) -> Result<(), DurableHealthStoreError> {
    let metadata = file
        .metadata()
        .map_err(|error| DurableHealthStoreError::io("inspect durable health record", error))?;
    let entry = stat_at(directory.as_raw_fd(), name)?;
    if !metadata.is_file()
        || metadata.st_uid() != effective_uid()
        || metadata.st_mode() & 0o7777 != 0o600
        || metadata.st_nlink() != 1
        || entry.st_mode & libc::S_IFMT != libc::S_IFREG
        || entry.st_uid != effective_uid()
        || entry.st_mode & 0o7777 != 0o600
        || entry.st_nlink != 1
        || entry.st_dev != metadata.st_dev()
        || entry.st_ino != metadata.st_ino()
        || !descriptor_has_cloexec(file.as_raw_fd())?
    {
        return Err(DurableHealthStoreError::security(
            DurableStoreSecurityObject::Record,
        ));
    }
    Ok(())
}

fn write_record_atomic(
    directory: &File,
    final_name: &str,
    bytes: &[u8],
) -> Result<(), DurableHealthStoreError> {
    if bytes.len() > MAX_DURABLE_HEALTH_RECORD_BYTES {
        return Err(DurableHealthStoreError::RecordTooLarge {
            kind: record_kind_from_name(final_name.as_bytes()),
        });
    }
    let final_name = CString::new(final_name)
        .map_err(|_| DurableHealthStoreError::security(DurableStoreSecurityObject::Record))?;
    let mut temporary = create_temporary_record(directory)?;
    temporary
        .file
        .write_all(bytes)
        .map_err(|error| DurableHealthStoreError::io("write durable health record", error))?;
    temporary
        .file
        .sync_all()
        .map_err(|error| DurableHealthStoreError::io("sync durable health record", error))?;
    validate_temporary_record(&temporary.file)?;

    let result = unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            temporary.name.as_ptr(),
            directory.as_raw_fd(),
            final_name.as_ptr(),
        )
    };
    if result != 0 {
        return Err(DurableHealthStoreError::io(
            "replace durable health record",
            io::Error::last_os_error(),
        ));
    }
    temporary.promoted = true;
    validate_open_record(directory, &final_name, &temporary.file)?;
    directory
        .sync_all()
        .map_err(|error| DurableHealthStoreError::io("sync durable health directory", error))?;
    Ok(())
}

struct TemporaryRecord {
    directory: RawFd,
    name: CString,
    file: File,
    promoted: bool,
}

impl Drop for TemporaryRecord {
    fn drop(&mut self) {
        if !self.promoted {
            let _ = unsafe { libc::unlinkat(self.directory, self.name.as_ptr(), 0) };
        }
    }
}

fn create_temporary_record(directory: &File) -> Result<TemporaryRecord, DurableHealthStoreError> {
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = CString::new(format!("{TEMP_PREFIX}{}-{sequence}", std::process::id()))
            .map_err(|_| {
                DurableHealthStoreError::security(DurableStoreSecurityObject::DirectoryEntry)
            })?;
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if descriptor >= 0 {
            let file = unsafe { File::from_raw_fd(descriptor) };
            if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
                return Err(DurableHealthStoreError::io(
                    "set durable health temporary mode",
                    io::Error::last_os_error(),
                ));
            }
            validate_temporary_record(&file)?;
            return Ok(TemporaryRecord {
                directory: directory.as_raw_fd(),
                name,
                file,
                promoted: false,
            });
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(DurableHealthStoreError::io(
                "create durable health temporary record",
                error,
            ));
        }
    }
    Err(DurableHealthStoreError::TemporaryFileLimitExceeded)
}

fn validate_temporary_record(file: &File) -> Result<(), DurableHealthStoreError> {
    let metadata = file
        .metadata()
        .map_err(|error| DurableHealthStoreError::io("inspect durable health temporary", error))?;
    if !metadata.is_file()
        || metadata.st_uid() != effective_uid()
        || metadata.st_mode() & 0o7777 != 0o600
        || metadata.st_nlink() != 1
        || !descriptor_has_cloexec(file.as_raw_fd())?
    {
        return Err(DurableHealthStoreError::security(
            DurableStoreSecurityObject::DirectoryEntry,
        ));
    }
    Ok(())
}

fn unlink_record(directory: &File, name: &str) -> Result<(), DurableHealthStoreError> {
    let name = CString::new(name)
        .map_err(|_| DurableHealthStoreError::security(DurableStoreSecurityObject::Record))?;
    let result = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    if result != 0 {
        return Err(DurableHealthStoreError::io(
            "remove durable health record",
            io::Error::last_os_error(),
        ));
    }
    directory
        .sync_all()
        .map_err(|error| DurableHealthStoreError::io("sync durable health directory", error))?;
    Ok(())
}

fn stat_at(root: RawFd, name: &CStr) -> Result<libc::stat, DurableHealthStoreError> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            root,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound || is_security_path_error(&error) {
            return Err(DurableHealthStoreError::security(
                DurableStoreSecurityObject::DirectoryEntry,
            ));
        }
        return Err(DurableHealthStoreError::io(
            "inspect durable health directory entry",
            error,
        ));
    }
    Ok(unsafe { stat.assume_init() })
}

fn descriptor_has_cloexec(descriptor: RawFd) -> Result<bool, DurableHealthStoreError> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 {
        return Err(DurableHealthStoreError::io(
            "inspect durable health descriptor flags",
            io::Error::last_os_error(),
        ));
    }
    Ok(flags & libc::FD_CLOEXEC != 0)
}

fn is_security_path_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ELOOP | libc::ENOTDIR | libc::EACCES | libc::EPERM)
    )
}

fn os_string_to_cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::worker_supervisor::{
        WorkerFailureKind, WorkerGenerationIdentity, WorkerSupervisorState,
    };

    use super::*;

    static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        parent: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let parent = std::env::temp_dir().join(format!(
                "agl-durable-health-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&parent).unwrap();
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
            let root = parent.join("health");
            Self { parent, root }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
    }

    fn policy() -> WorkerCircuitBreakerPolicy {
        WorkerCircuitBreakerPolicy::new(100, 800, 4).unwrap()
    }

    fn health_key(driver: &str, worker: &str) -> WorkerHealthKey {
        WorkerHealthKey::new("pci:0000:03:00.0", driver, worker).unwrap()
    }

    fn failed_health(key: WorkerHealthKey, failed_at: u64) -> WorkerHealthState {
        let worker = WorkerGenerationIdentity::new(42, 1, key.worker_build_id()).unwrap();
        let mut supervisor =
            WorkerSupervisorState::restore(WorkerHealthState::new(key), policy(), failed_at)
                .unwrap();
        supervisor.begin_start(worker.clone()).unwrap();
        supervisor.mark_ready(&worker).unwrap();
        supervisor
            .record_worker_failure(&worker, WorkerFailureKind::DeviceLost, failed_at)
            .unwrap();
        supervisor.health().clone()
    }

    fn digest(byte: u8) -> String {
        format!("{byte:02x}").repeat(32)
    }

    fn quarantine_key(config: u8, device: u8) -> ResourceQuarantineKey {
        ResourceQuarantineKey::new(
            digest(1),
            digest(config),
            digest(device),
            digest(4),
            digest(5),
        )
        .unwrap()
    }

    fn quarantine(key: ResourceQuarantineKey) -> ResourceEstimateQuarantine {
        ResourceEstimateQuarantine::new(
            key,
            AllocationEstimate {
                model_bytes: 10,
                context_bytes: 20,
                transient_bytes: 30,
                uncertainty_bytes: 5,
            },
            AllocationReceipt {
                model_bytes: 10,
                context_bytes: 21,
                transient_bytes: 30,
            },
        )
        .unwrap()
    }

    #[test]
    fn restart_restores_cooldown_and_quarantine_for_the_exact_identity() {
        let fixture = Fixture::new("restart");
        let key = health_key("radv-26.1", "worker-a");
        let health = failed_health(key.clone(), 10_000);
        let resource_key = quarantine_key(2, 3);
        let resource = quarantine(resource_key.clone());

        {
            let store = DurableHealthStore::open(&fixture.root).unwrap();
            store.store_worker_health(&health, policy()).unwrap();
            store.store_resource_quarantine(&resource).unwrap();
        }

        let restarted = DurableHealthStore::open(&fixture.root).unwrap();
        assert_eq!(
            restarted.load_worker_health(&key, policy()).unwrap(),
            Some(health)
        );
        assert_eq!(
            restarted.load_resource_quarantine(&resource_key).unwrap(),
            Some(resource)
        );
    }

    #[test]
    fn daemon_to_standalone_handoff_cannot_bypass_cooldown_or_quarantine() {
        let fixture = Fixture::new("daemon-standalone-handoff");
        let key = health_key("radv-26.1", "worker-a");
        let failed_at = 20_000;
        let health = failed_health(key.clone(), failed_at);
        let not_before = health.cooldown_not_before_unix_ms().unwrap();
        let resource_key = quarantine_key(2, 3);
        let unsafe_estimate = quarantine(resource_key.clone());

        {
            let daemon_authority = DurableHealthStore::open(&fixture.root).unwrap();
            daemon_authority
                .store_worker_health(&health, policy())
                .unwrap();
            daemon_authority
                .store_resource_quarantine(&unsafe_estimate)
                .unwrap();
        }

        // A no-daemon standalone host opens the same UID-global authority
        // root. A different process role is deliberately not part of either
        // identity, so it cannot turn this into a clean generation.
        let standalone_authority = DurableHealthStore::open(&fixture.root).unwrap();
        let restored_health = standalone_authority
            .load_worker_health(&key, policy())
            .unwrap()
            .expect("daemon cooldown is visible to standalone admission");
        let mut standalone =
            WorkerSupervisorState::restore(restored_health, policy(), not_before.saturating_sub(1))
                .unwrap();
        assert_eq!(
            standalone.phase(),
            crate::worker_supervisor::WorkerLifecyclePhase::CoolingDown
        );
        let candidate = WorkerGenerationIdentity::new(99, 2, key.worker_build_id()).unwrap();
        assert!(matches!(
            standalone.begin_start(candidate.clone()),
            Err(crate::worker_supervisor::WorkerSupervisorError::UnexpectedTransition { .. })
        ));
        assert_eq!(
            standalone_authority
                .load_resource_quarantine(&resource_key)
                .unwrap(),
            Some(unsafe_estimate)
        );

        standalone.release_cooldown(not_before).unwrap();
        standalone.begin_start(candidate).unwrap();
        // Cooldown expiry permits a clean process start, but it does not clear
        // an unsafe allocation estimate. A replacement profile remains the
        // only authorized quarantine-clear path.
        assert!(
            standalone_authority
                .load_resource_quarantine(&resource_key)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn changed_driver_worker_config_or_device_uses_a_distinct_key() {
        let fixture = Fixture::new("identity");
        let store = DurableHealthStore::open(&fixture.root).unwrap();
        let original = health_key("driver-a", "worker-a");
        store
            .store_worker_health(&failed_health(original.clone(), 1_000), policy())
            .unwrap();
        assert!(
            store
                .load_worker_health(&health_key("driver-b", "worker-a"), policy())
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .load_worker_health(&health_key("driver-a", "worker-b"), policy())
                .unwrap()
                .is_none()
        );

        let original_resource = quarantine_key(2, 3);
        store
            .store_resource_quarantine(&quarantine(original_resource.clone()))
            .unwrap();
        assert!(
            store
                .load_resource_quarantine(&quarantine_key(9, 3))
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .load_resource_quarantine(&quarantine_key(2, 9))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn root_symlink_and_wrong_root_mode_fail_closed() {
        let symlink_fixture = Fixture::new("root-symlink");
        let actual = symlink_fixture.parent.join("actual");
        fs::create_dir(&actual).unwrap();
        fs::set_permissions(&actual, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&actual, &symlink_fixture.root).unwrap();
        assert!(matches!(
            DurableHealthStore::open(&symlink_fixture.root),
            Err(DurableHealthStoreError::SecurityViolation {
                object: DurableStoreSecurityObject::Root,
            })
        ));

        let mode_fixture = Fixture::new("root-mode");
        fs::create_dir(&mode_fixture.root).unwrap();
        fs::set_permissions(&mode_fixture.root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            DurableHealthStore::open(&mode_fixture.root),
            Err(DurableHealthStoreError::SecurityViolation {
                object: DurableStoreSecurityObject::Root,
            })
        ));
    }

    #[test]
    fn record_symlink_mode_and_hardlink_fail_closed() {
        let symlink_fixture = Fixture::new("record-symlink");
        let key = health_key("driver", "worker");
        let store = DurableHealthStore::open(&symlink_fixture.root).unwrap();
        let record_path = symlink_fixture.root.join(worker_record_name(&key).unwrap());
        let outside = symlink_fixture.parent.join("outside");
        fs::write(&outside, b"{}").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&outside, &record_path).unwrap();
        assert!(matches!(
            store.load_worker_health(&key, policy()),
            Err(DurableHealthStoreError::SecurityViolation { .. })
        ));

        let mode_fixture = Fixture::new("record-mode");
        let mode_store = DurableHealthStore::open(&mode_fixture.root).unwrap();
        let health = failed_health(key.clone(), 2_000);
        mode_store.store_worker_health(&health, policy()).unwrap();
        let mode_path = mode_fixture.root.join(worker_record_name(&key).unwrap());
        fs::set_permissions(&mode_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            mode_store.load_worker_health(&key, policy()),
            Err(DurableHealthStoreError::SecurityViolation { .. })
        ));

        let link_fixture = Fixture::new("record-hardlink");
        let link_store = DurableHealthStore::open(&link_fixture.root).unwrap();
        link_store.store_worker_health(&health, policy()).unwrap();
        let link_path = link_fixture.root.join(worker_record_name(&key).unwrap());
        fs::hard_link(&link_path, link_fixture.parent.join("outside-link")).unwrap();
        assert!(matches!(
            link_store.load_worker_health(&key, policy()),
            Err(DurableHealthStoreError::SecurityViolation { .. })
        ));
    }

    #[test]
    fn corruption_oversize_and_wrong_identity_never_clear_a_record() {
        let corrupt_fixture = Fixture::new("corrupt");
        let key = health_key("driver", "worker");
        let store = DurableHealthStore::open(&corrupt_fixture.root).unwrap();
        let health = failed_health(key.clone(), 3_000);
        store.store_worker_health(&health, policy()).unwrap();
        let path = corrupt_fixture.root.join(worker_record_name(&key).unwrap());
        fs::write(&path, b"{not-json").unwrap();
        assert!(matches!(
            store.clear_worker_health(&key, policy()),
            Err(DurableHealthStoreError::RecordCorrupt { .. })
        ));
        assert!(path.exists());

        let oversized_fixture = Fixture::new("oversized");
        let oversized_store = DurableHealthStore::open(&oversized_fixture.root).unwrap();
        oversized_store
            .store_worker_health(&health, policy())
            .unwrap();
        let oversized_path = oversized_fixture
            .root
            .join(worker_record_name(&key).unwrap());
        fs::write(
            &oversized_path,
            vec![b'x'; MAX_DURABLE_HEALTH_RECORD_BYTES + 1],
        )
        .unwrap();
        assert!(matches!(
            oversized_store.clear_worker_health(&key, policy()),
            Err(DurableHealthStoreError::RecordTooLarge { .. })
        ));
        assert!(oversized_path.exists());

        let identity_fixture = Fixture::new("wrong-identity");
        let identity_store = DurableHealthStore::open(&identity_fixture.root).unwrap();
        identity_store
            .store_worker_health(&health, policy())
            .unwrap();
        let other = failed_health(health_key("other-driver", "worker"), 3_000);
        let wrong_record = WorkerHealthRecord {
            schema: WorkerHealthSchema::V1,
            state: other,
        };
        let identity_path = identity_fixture
            .root
            .join(worker_record_name(&key).unwrap());
        fs::write(&identity_path, serde_json::to_vec(&wrong_record).unwrap()).unwrap();
        assert!(matches!(
            identity_store.clear_worker_health(&key, policy()),
            Err(DurableHealthStoreError::RecordIdentityMismatch { .. })
        ));
        assert!(identity_path.exists());
    }

    #[test]
    fn clear_is_exact_and_quarantine_requires_a_replacement_profile() {
        let fixture = Fixture::new("clear");
        let store = DurableHealthStore::open(&fixture.root).unwrap();
        let first = health_key("driver-a", "worker");
        let second = health_key("driver-b", "worker");
        store
            .store_worker_health(&failed_health(first.clone(), 4_000), policy())
            .unwrap();
        store
            .store_worker_health(&failed_health(second.clone(), 4_000), policy())
            .unwrap();
        assert!(store.clear_worker_health(&first, policy()).unwrap());
        assert!(!store.clear_worker_health(&first, policy()).unwrap());
        assert!(
            store
                .load_worker_health(&second, policy())
                .unwrap()
                .is_some()
        );

        let quarantined = quarantine_key(2, 3);
        store
            .store_resource_quarantine(&quarantine(quarantined.clone()))
            .unwrap();
        assert!(matches!(
            store.clear_resource_quarantine_after_replacement(&quarantined, &quarantined),
            Err(DurableHealthStoreError::InvalidQuarantineClear)
        ));
        assert!(matches!(
            store.clear_resource_quarantine_after_replacement(&quarantined, &quarantine_key(9, 8),),
            Err(DurableHealthStoreError::InvalidQuarantineClear)
        ));
        let replacement = quarantine_key(9, 3);
        assert!(
            store
                .clear_resource_quarantine_after_replacement(&quarantined, &replacement)
                .unwrap()
        );
        assert!(
            store
                .load_resource_quarantine(&quarantined)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn quarantine_json_is_strict_bounded_and_contains_no_secret_or_path_fields() {
        let key = quarantine_key(2, 3);
        let record = ResourceQuarantineRecord {
            schema: ResourceQuarantineSchema::V1,
            quarantine: quarantine(key),
        };
        let bytes = serialize_record(&record, DurableRecordKind::ResourceQuarantine).unwrap();
        let json = String::from_utf8(bytes).unwrap();
        assert!(json.len() <= MAX_DURABLE_HEALTH_RECORD_BYTES);
        for forbidden in ["prompt", "path", "secret", "/home/", "model_name"] {
            assert!(!json.contains(forbidden));
        }
        assert!(json.contains("model_digest"));
        assert!(json.contains("config_digest"));
        assert!(json.contains("model_bytes"));
        assert!(json.contains("context_bytes"));

        let mut value = serde_json::to_value(record).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("prompt".to_string(), serde_json::json!("do not persist"));
        assert!(serde_json::from_value::<ResourceQuarantineRecord>(value).is_err());
    }

    #[test]
    fn replacement_is_atomic_private_and_leaves_no_temporary_entry() {
        use std::os::unix::fs::MetadataExt as _;

        let fixture = Fixture::new("replace");
        let store = DurableHealthStore::open(&fixture.root).unwrap();
        let key = health_key("driver", "worker");
        let first = failed_health(key.clone(), 5_000);
        let second = failed_health(key.clone(), 6_000);
        store.store_worker_health(&first, policy()).unwrap();
        store.store_worker_health(&second, policy()).unwrap();

        assert_eq!(
            store.load_worker_health(&key, policy()).unwrap(),
            Some(second)
        );
        let entries = fs::read_dir(&fixture.root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].as_bytes().starts_with(TEMP_PREFIX.as_bytes()));
        let metadata = fs::metadata(fixture.root.join(&entries[0])).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(metadata.nlink(), 1);
    }
}
