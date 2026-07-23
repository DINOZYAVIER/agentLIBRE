//! Host-side proxy for the exact sandboxed native inference worker.
//!
//! This module deliberately contains no native inference dependency. The
//! existing host-owned [`ModelManager`](crate::ModelManager) remains the FIFO,
//! deadline, cancellation and evidence authority; this runtime serializes only
//! the active native operation over the private worker protocol.

#![cfg(target_os = "linux")]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agl_ids::AttemptId;
use sha2::{Digest as _, Sha256};

use crate::admission::{
    AdmissionPolicy, AllocationReceipt as HostAllocationReceipt, DeviceMemoryEnvelope,
    DeviceMemorySnapshot, ReservationLedger, ReservationLedgerError, ReservationRequest,
    ReservationToken, SnapshotPolicy,
};
use crate::durable_health::{
    DurableHealthStore, ResourceEstimateQuarantine, ResourceQuarantineKey,
};
use crate::gpu_profile::GpuProfileVerifier;
use crate::private_directory::ensure_private_directory;
use crate::worker_protocol::{
    AllocationReceipt, ContextResourceId, DescriptorSet, DeviceKind, DeviceSnapshot, HostCommand,
    ModelResourceId, OperationId, SandboxConfiguration, SealedPayloadTransfer, ShutdownReason,
    WORKER_BUILD_ID, WORKER_DEVICE_LOST_EXIT_STATUS, WorkerEvent, WorkerExecutable, WorkerFailure,
    WorkerFailureCode, WorkerLogRecord, WorkerProcess, WorkerProtocolError,
    WorkerProtocolErrorCode,
};
use crate::worker_supervisor::{
    ActiveAttemptIdentity, WorkerCircuitBreakerPolicy, WorkerFailureKind, WorkerGenerationIdentity,
    WorkerHealthKey, WorkerHealthState, WorkerLifecyclePhase, WorkerSupervisorState,
};
use crate::{
    DeviceAuthorityLease, InferenceDeviceInfo, InferenceDeviceKind, InferenceJob,
    InferenceOutputEvent, InferenceStageValidator, ModelGeneration, ModelRuntime, OutputDelivery,
    RuntimeFailure, RuntimeOperation,
};

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CANCEL_GRACE: Duration = Duration::from_secs(2);
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const RECEIVE_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_COLLECTED_WORKER_LOG_BYTES: usize = 256 * 1024;
const GLOBAL_RUNTIME_PARENT: &str = "/run/user";
const DEFAULT_INITIAL_COOLDOWN_MS: u64 = 5_000;
const DEFAULT_MAXIMUM_COOLDOWN_MS: u64 = 60_000;
const DEFAULT_MAXIMUM_CRASH_STREAK: u8 = 5;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerRuntimeOptions {
    worker_executable: Option<PathBuf>,
    private_temp_root: PathBuf,
    runtime_roots: Vec<PathBuf>,
    gpu_device_paths: Vec<PathBuf>,
    admitted_devices: BTreeMap<String, crate::worker_resources::RenderDeviceResource>,
    device_lease_root: Option<PathBuf>,
    health_root: Option<PathBuf>,
    environment: BTreeMap<String, OsString>,
    handshake_timeout: Duration,
    operation_timeout: Duration,
    cancellation_grace: Duration,
    shutdown_timeout: Duration,
    circuit_breaker_policy: WorkerCircuitBreakerPolicy,
}

impl WorkerRuntimeOptions {
    pub fn new(private_temp_root: impl Into<PathBuf>) -> Self {
        Self {
            worker_executable: None,
            private_temp_root: private_temp_root.into(),
            runtime_roots: Vec::new(),
            gpu_device_paths: Vec::new(),
            admitted_devices: BTreeMap::new(),
            device_lease_root: None,
            health_root: None,
            environment: BTreeMap::new(),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
            cancellation_grace: DEFAULT_CANCEL_GRACE,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            circuit_breaker_policy: default_circuit_breaker_policy(),
        }
    }

    pub fn from_launch_resources(
        private_temp_root: impl Into<PathBuf>,
        resources: &crate::worker_resources::WorkerLaunchResources,
    ) -> Self {
        let private_temp_root = private_temp_root.into();
        let mut environment = resources.environment().to_process_environment();
        let admitted_devices = resources
            .render_devices()
            .iter()
            .map(|resource| (resource.physical_device_id().to_string(), resource.clone()))
            .collect();
        environment.insert(
            "TMPDIR".to_string(),
            private_temp_root.as_os_str().to_owned(),
        );
        environment.insert(
            "XDG_CACHE_HOME".to_string(),
            private_temp_root.as_os_str().to_owned(),
        );
        Self::new(&private_temp_root)
            .with_runtime_roots(resources.runtime_roots().to_vec())
            .with_gpu_device_paths(
                resources
                    .render_devices()
                    .iter()
                    .map(|resource| resource.render_node().to_path_buf())
                    .collect(),
            )
            .with_admitted_devices(admitted_devices)
            .with_environment(environment)
    }

    pub fn with_worker_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.worker_executable = Some(path.into());
        self
    }

    pub fn with_runtime_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.runtime_roots = roots;
        self
    }

    pub fn with_gpu_device_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.gpu_device_paths = paths;
        self
    }

    fn with_admitted_devices(
        mut self,
        devices: BTreeMap<String, crate::worker_resources::RenderDeviceResource>,
    ) -> Self {
        self.admitted_devices = devices;
        self
    }

    pub fn with_device_lease_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.device_lease_root = Some(path.into());
        self
    }

    pub fn with_health_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.health_root = Some(path.into());
        self
    }

    pub fn with_environment(mut self, environment: BTreeMap<String, OsString>) -> Self {
        self.environment = environment;
        self
    }

    #[doc(hidden)]
    pub fn with_timeouts(
        mut self,
        handshake: Duration,
        operation: Duration,
        cancellation: Duration,
        shutdown: Duration,
    ) -> Self {
        self.handshake_timeout = handshake;
        self.operation_timeout = operation;
        self.cancellation_grace = cancellation;
        self.shutdown_timeout = shutdown;
        self
    }

    #[doc(hidden)]
    pub fn with_circuit_breaker_policy(mut self, policy: WorkerCircuitBreakerPolicy) -> Self {
        self.circuit_breaker_policy = policy;
        self
    }

    fn validate(&self) -> Result<(), RuntimeFailure> {
        for (name, duration) in [
            ("handshake", self.handshake_timeout),
            ("operation", self.operation_timeout),
            ("cancellation", self.cancellation_grace),
            ("shutdown", self.shutdown_timeout),
        ] {
            if duration.is_zero() {
                return Err(runtime_failure(format!(
                    "inference worker {name} timeout must be positive"
                )));
            }
        }
        validate_absolute_path("private inference temp root", &self.private_temp_root)?;
        validate_unique_paths("runtime root", &self.runtime_roots)?;
        validate_unique_paths("GPU device", &self.gpu_device_paths)?;
        if self.gpu_device_paths.len() != self.admitted_devices.len() {
            return Err(runtime_failure(
                "every admitted GPU render node requires one host-verified physical identity",
            ));
        }
        if !self.admitted_devices.is_empty() && self.device_lease_root.is_none() {
            return Err(runtime_failure(
                "GPU inference requires an explicit shared device authority lease root",
            ));
        }
        if !self.admitted_devices.is_empty() && self.health_root.is_none() {
            return Err(runtime_failure(
                "GPU inference requires an explicit durable health/quarantine root",
            ));
        }
        for (physical_device_id, resource) in &self.admitted_devices {
            validate_bounded_identity("physical device ID", physical_device_id)?;
            validate_bounded_identity("driver build ID", resource.driver_build_id())?;
            if resource.physical_device_id() != physical_device_id {
                return Err(runtime_failure(
                    "inference device authority map contains a mismatched physical identity",
                ));
            }
        }
        if let Some(path) = &self.device_lease_root {
            validate_absolute_path("device authority lease root", path)?;
        }
        if let Some(path) = &self.health_root {
            validate_absolute_path("inference health root", path)?;
        }
        if let Some(path) = &self.worker_executable {
            validate_absolute_path("inference worker executable", path)?;
        }
        for (name, value) in &self.environment {
            match name.as_str() {
                "TMPDIR" | "XDG_CACHE_HOME" => {
                    validate_absolute_path(name, Path::new(value))?;
                }
                "VK_DRIVER_FILES" => {
                    let value = value.to_str().ok_or_else(|| {
                        runtime_failure("VK_DRIVER_FILES must contain UTF-8 absolute paths")
                    })?;
                    if value.is_empty() {
                        return Err(runtime_failure("VK_DRIVER_FILES cannot be empty"));
                    }
                    for path in value.split(':') {
                        if path.is_empty() {
                            return Err(runtime_failure(
                                "VK_DRIVER_FILES cannot contain empty path entries",
                            ));
                        }
                        validate_absolute_path("VK_DRIVER_FILES entry", Path::new(path))?;
                    }
                }
                _ => {
                    return Err(runtime_failure(format!(
                        "inference worker environment key is not allowed: {name}"
                    )));
                }
            }
        }
        Ok(())
    }

    fn executable(&self) -> Result<WorkerExecutable, RuntimeFailure> {
        let result = match &self.worker_executable {
            Some(path) => WorkerExecutable::open_exact(path),
            None => WorkerExecutable::sibling_of_current_executable(),
        };
        result.map_err(protocol_failure)
    }

    fn worker_executable_path(&self) -> Result<PathBuf, RuntimeFailure> {
        if let Some(path) = &self.worker_executable {
            return Ok(path.clone());
        }
        let host = std::env::current_exe().map_err(|error| {
            runtime_failure(format!("failed to resolve current AGL executable: {error}"))
        })?;
        let parent = host
            .parent()
            .ok_or_else(|| runtime_failure("current AGL executable has no sibling directory"))?;
        Ok(parent.join(crate::worker_protocol::WORKER_BINARY_NAME))
    }
}

#[derive(Debug)]
pub struct WorkerModelRuntime {
    options: WorkerRuntimeOptions,
    device_leases: BTreeMap<String, DeviceAuthorityLease>,
    health_store: Option<DurableHealthStore>,
    pending_durable_quarantine: Option<ResourceEstimateQuarantine>,
    profile_verifier: GpuProfileVerifier,
    admissions: BTreeMap<String, DeviceAdmission>,
    pending_admissions: BTreeMap<String, PendingAdmission>,
    active_admissions: BTreeMap<String, ActiveAdmission>,
    device_selectors: BTreeMap<String, String>,
    supervisors: BTreeMap<String, WorkerSupervisorState>,
    session: Option<WorkerSession>,
    reap_pending: bool,
    next_operation_id: u64,
    next_resource_id: u64,
    next_launch_generation: u64,
    next_attempt_generation: u64,
    live_models: usize,
    live_contexts: usize,
    status: WorkerRuntimeStatusHandle,
    native_bundle_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerRuntimeStatus {
    worker_build_id: String,
    phase: WorkerLifecyclePhase,
    worker_pid: Option<u32>,
    launch_generation: Option<u64>,
    physical_device_id: Option<String>,
    reserved_bytes: u64,
    cooldown_not_before_unix_ms: Option<u64>,
}

impl WorkerRuntimeStatus {
    pub fn worker_build_id(&self) -> &str {
        &self.worker_build_id
    }

    pub const fn phase(&self) -> WorkerLifecyclePhase {
        self.phase
    }

    pub const fn worker_pid(&self) -> Option<u32> {
        self.worker_pid
    }

    pub const fn launch_generation(&self) -> Option<u64> {
        self.launch_generation
    }

    pub fn physical_device_id(&self) -> Option<&str> {
        self.physical_device_id.as_deref()
    }

    pub const fn reserved_bytes(&self) -> u64 {
        self.reserved_bytes
    }

    pub const fn cooldown_not_before_unix_ms(&self) -> Option<u64> {
        self.cooldown_not_before_unix_ms
    }
}

#[derive(Clone, Debug)]
pub struct WorkerRuntimeStatusHandle {
    inner: Arc<Mutex<WorkerRuntimeStatus>>,
}

impl WorkerRuntimeStatusHandle {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(WorkerRuntimeStatus {
                worker_build_id: WORKER_BUILD_ID.to_string(),
                phase: WorkerLifecyclePhase::Cold,
                worker_pid: None,
                launch_generation: None,
                physical_device_id: None,
                reserved_bytes: 0,
                cooldown_not_before_unix_ms: None,
            })),
        }
    }

    pub fn snapshot(&self) -> WorkerRuntimeStatus {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn replace(&self, status: WorkerRuntimeStatus) {
        *self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = status;
    }
}

impl Default for WorkerRuntimeStatusHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct DeviceAdmission {
    envelope: DeviceMemoryEnvelope,
    ledger: ReservationLedger,
}

#[derive(Clone, Debug)]
struct PendingAdmission {
    physical_device_id: String,
    token: ReservationToken,
    model_key: String,
    full_estimate: crate::admission::AllocationEstimate,
    quarantine_key: ResourceQuarantineKey,
}

#[derive(Clone, Debug)]
struct ActiveAdmission {
    physical_device_id: String,
    token: ReservationToken,
}

impl WorkerModelRuntime {
    pub fn discover(private_temp_root: impl Into<PathBuf>) -> Result<Self, RuntimeFailure> {
        let (device_lease_root, health_root) = production_authority_roots()?;
        let resources =
            crate::worker_resources::discover_worker_launch_resources().map_err(|error| {
                runtime_failure(format!(
                    "failed to discover exact inference worker resources ({}): {error}",
                    error.code().as_str()
                ))
            })?;
        Self::new(
            WorkerRuntimeOptions::from_launch_resources(private_temp_root, &resources)
                .with_device_lease_root(device_lease_root)
                .with_health_root(health_root),
        )
    }

    pub fn new(options: WorkerRuntimeOptions) -> Result<Self, RuntimeFailure> {
        options.validate()?;
        prepare_private_root("private inference temp root", &options.private_temp_root)?;
        if let Some(lease_root) = &options.device_lease_root {
            prepare_private_root("device authority lease root", lease_root)?;
        }
        if let Some(health_root) = &options.health_root {
            prepare_private_root("inference health root", health_root)?;
        }
        let health_store = options
            .health_root
            .as_deref()
            .map(DurableHealthStore::open)
            .transpose()
            .map_err(|error| {
                RuntimeFailure::resource_admission(
                    error.code(),
                    format!("failed to open inference health store: {error}"),
                    "",
                )
            })?;
        let status = WorkerRuntimeStatusHandle::new();
        Ok(Self {
            options,
            device_leases: BTreeMap::new(),
            health_store,
            pending_durable_quarantine: None,
            profile_verifier: GpuProfileVerifier::new(),
            admissions: BTreeMap::new(),
            pending_admissions: BTreeMap::new(),
            active_admissions: BTreeMap::new(),
            device_selectors: BTreeMap::new(),
            supervisors: BTreeMap::new(),
            session: None,
            reap_pending: false,
            next_operation_id: 1,
            next_resource_id: 1,
            next_launch_generation: 1,
            next_attempt_generation: 1,
            live_models: 0,
            live_contexts: 0,
            status,
            native_bundle_id: None,
        })
    }

    pub fn status_handle(&self) -> WorkerRuntimeStatusHandle {
        self.status.clone()
    }

    fn acquire_device_authority(
        &mut self,
        physical_device_id: &str,
    ) -> Result<bool, RuntimeFailure> {
        if self.device_leases.contains_key(physical_device_id) {
            return Ok(false);
        }
        if !self
            .options
            .admitted_devices
            .contains_key(physical_device_id)
        {
            return Err(RuntimeFailure::resource_admission(
                "device_identity_mismatch",
                "cannot acquire authority for a device outside the admitted set",
                "",
            ));
        }
        let lease_root = self.options.device_lease_root.as_deref().ok_or_else(|| {
            RuntimeFailure::resource_admission(
                "device_authority_unavailable",
                "GPU inference has no shared device authority root",
                "",
            )
        })?;
        let lease =
            DeviceAuthorityLease::acquire(lease_root, physical_device_id).map_err(|error| {
                RuntimeFailure::resource_admission(
                    error.code(),
                    format!(
                        "failed to acquire inference device authority ({}): {error}",
                        error.code()
                    ),
                    "",
                )
            })?;
        self.device_leases
            .insert(physical_device_id.to_string(), lease);
        Ok(true)
    }

    fn acquire_inventory_device_authority(&mut self) -> Result<Vec<String>, RuntimeFailure> {
        let physical_device_ids = self
            .options
            .admitted_devices
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut newly_acquired = Vec::new();
        for physical_device_id in physical_device_ids {
            match self.acquire_device_authority(&physical_device_id) {
                Ok(true) => newly_acquired.push(physical_device_id),
                Ok(false) => {}
                Err(error) => {
                    for acquired in newly_acquired {
                        self.device_leases.remove(&acquired);
                    }
                    return Err(error);
                }
            }
        }
        Ok(newly_acquired)
    }

    fn release_device_authority_if_idle(&mut self, physical_device_id: &str) {
        let has_reservations = self
            .admissions
            .get(physical_device_id)
            .is_some_and(|admission| {
                admission.ledger.status().map_or(true, |status| {
                    status.resident.total_bytes() != Ok(0)
                        || status.pending_reservations != 0
                        || status.active_reservations != 0
                })
            });
        let has_worker = self.session.as_ref().is_some_and(|session| {
            session.physical_device_id.as_deref() == Some(physical_device_id)
        });
        if !has_reservations && !has_worker {
            self.device_leases.remove(physical_device_id);
        }
    }

    fn release_all_device_authority_if_idle(&mut self) -> Result<(), RuntimeFailure> {
        if self.live_models != 0 || self.live_contexts != 0 {
            return Ok(());
        }
        let has_reservations = self.admissions.values().any(|admission| {
            admission.ledger.status().map_or(true, |status| {
                status.resident.total_bytes() != Ok(0)
                    || status.pending_reservations != 0
                    || status.active_reservations != 0
            })
        });
        if has_reservations {
            return Ok(());
        }
        self.shutdown_session()?;
        self.device_leases.clear();
        self.refresh_status(None);
        Ok(())
    }

    fn ensure_job_admission(&mut self, job: &InferenceJob) -> Result<(), RuntimeFailure> {
        self.flush_pending_quarantine()?;
        let context_key = job.context_key().digest().to_string();
        if self.pending_admissions.contains_key(&context_key)
            || self.active_admissions.contains_key(&context_key)
        {
            return Ok(());
        }

        let profile = self
            .profile_verifier
            .resolve(job.config())
            .map_err(|error| {
                RuntimeFailure::resource_admission(error.code(), error.to_string(), "")
            })?;
        let Some(profile) = profile else {
            return Ok(());
        };
        let selector = job.config().runtime.device.as_deref().ok_or_else(|| {
            RuntimeFailure::resource_admission(
                "resource_estimate_unknown",
                "reviewed GPU profile requires an exact backend device selector",
                "",
            )
        })?;
        let physical_device_id = self.resolve_gpu_selector(selector)?;
        let resource = self
            .options
            .admitted_devices
            .get(&physical_device_id)
            .cloned()
            .ok_or_else(|| {
                RuntimeFailure::resource_admission(
                    "device_identity_mismatch",
                    "selected inference GPU is outside the host-admitted render-node set",
                    "",
                )
            })?;
        self.wait_for_device_cooldown(job, &physical_device_id)?;
        self.acquire_device_authority(&physical_device_id)?;
        let result = (|| {
            let observation = resource
                .memory_observation()
                .map_err(|error| {
                    RuntimeFailure::resource_admission(
                        "device_snapshot_invalid",
                        format!("failed to read fresh kernel VRAM snapshot: {error}"),
                        "",
                    )
                })?
                .ok_or_else(|| {
                    RuntimeFailure::resource_admission(
                        "device_snapshot_invalid",
                        "selected GPU has no kernel-owned VRAM total/used counters",
                        "",
                    )
                })?;
            if resource.pci_device_id() != Some(profile.pci_device_id())
                || resource.pci_subsystem_id() != Some(profile.pci_subsystem_id())
            {
                return Err(RuntimeFailure::resource_admission(
                    "resource_profile_hardware_mismatch",
                    format!(
                        "reviewed GPU allocation profile {} does not cover the selected PCI device",
                        profile.id()
                    ),
                    "",
                ));
            }
            let now_unix_ms = unix_time_millis()?;
            let worker_generation_id = self.native_generation_build_id()?;

            if !self.admissions.contains_key(&physical_device_id) {
                let ledger = ReservationLedger::new(
                    physical_device_id.clone(),
                    AdmissionPolicy {
                        reserve_bytes: profile.reserve_bytes(),
                    },
                )
                .map_err(resource_ledger_failure)?;
                self.admissions.insert(
                    physical_device_id.clone(),
                    DeviceAdmission {
                        envelope: DeviceMemoryEnvelope {
                            physical_device_id: physical_device_id.clone(),
                            minimum_total_bytes: profile.total_device_bytes(),
                            maximum_total_bytes: profile.total_device_bytes(),
                        },
                        ledger,
                    },
                );
            }
            let admission = self
                .admissions
                .get_mut(&physical_device_id)
                .expect("device admission was inserted above");
            let quarantine_key = resource_quarantine_key(job, &resource, &worker_generation_id)?;
            let health_store = self.health_store.as_ref().ok_or_else(|| {
                RuntimeFailure::resource_admission(
                    "inference_health_store_unavailable",
                    "GPU admission has no durable quarantine authority",
                    "",
                )
            })?;
            if health_store
                .load_resource_quarantine(&quarantine_key)
                .map_err(durable_health_failure)?
                .is_some()
            {
                return Err(RuntimeFailure::resource_admission(
                    "resource_estimate_quarantined",
                    format!(
                        "reviewed GPU allocation profile {} is quarantined after an over-envelope worker receipt",
                        profile.id()
                    ),
                    "",
                ));
            }

            let model_key = job.model_key().digest().to_string();
            let pending = admission
                .ledger
                .validate_and_reserve(
                    DeviceMemorySnapshot {
                        physical_device_id: physical_device_id.clone(),
                        driver_id: resource.driver_build_id().to_string(),
                        total_bytes: observation.total_bytes,
                        available_bytes: observation.available_bytes,
                        observed_at_unix_ms: now_unix_ms,
                    },
                    &admission.envelope,
                    SnapshotPolicy::default(),
                    now_unix_ms,
                    ReservationRequest {
                        model_key: model_key.clone(),
                        context_key: context_key.clone(),
                        estimate: profile.estimate(),
                    },
                )
                .map_err(resource_ledger_failure)?;
            self.pending_admissions.insert(
                context_key,
                PendingAdmission {
                    physical_device_id: physical_device_id.clone(),
                    token: pending.token,
                    model_key,
                    full_estimate: profile.estimate(),
                    quarantine_key,
                },
            );
            self.refresh_status(Some(&physical_device_id));
            Ok(())
        })();
        if result.is_err() {
            self.release_device_authority_if_idle(&physical_device_id);
        }
        result
    }

    fn flush_pending_quarantine(&mut self) -> Result<(), RuntimeFailure> {
        let Some(quarantine) = self.pending_durable_quarantine.clone() else {
            return Ok(());
        };
        let store = self.health_store.as_ref().ok_or_else(|| {
            RuntimeFailure::resource_admission(
                "inference_health_store_unavailable",
                "unsafe GPU estimate is pending but no durable quarantine store exists",
                "",
            )
        })?;
        store
            .store_resource_quarantine(&quarantine)
            .map_err(|error| {
                RuntimeFailure::resource_admission(
                    error.code(),
                    format!(
                        "GPU admission remains poisoned until its unsafe estimate quarantine is durable: {error}"
                    ),
                    "",
                )
            })?;
        self.pending_durable_quarantine = None;
        Ok(())
    }

    fn resolve_gpu_selector(&mut self, selector: &str) -> Result<String, RuntimeFailure> {
        if let Some(physical_device_id) = self.device_selectors.get(selector) {
            return Ok(physical_device_id.clone());
        }
        let snapshot = self.worker_device_snapshot()?;
        let devices = self.map_device_snapshot(&snapshot)?;
        for device in devices {
            if matches!(
                device.kind,
                InferenceDeviceKind::DiscreteGpu | InferenceDeviceKind::IntegratedGpu
            ) && device.usable
                && device.supports_gpu_offload
            {
                self.device_selectors.insert(
                    device.backend_name.clone(),
                    device.physical_device_id.clone(),
                );
            }
        }
        self.device_selectors.get(selector).cloned().ok_or_else(|| {
            RuntimeFailure::resource_admission(
                "accelerator_device_unavailable",
                format!(
                    "configured GPU selector {selector:?} is not a usable admitted llama.cpp device"
                ),
                "",
            )
        })
    }

    fn reconcile_started_receipt(
        &mut self,
        job: &InferenceJob,
        receipt: &AllocationReceipt,
    ) -> Result<(), RuntimeFailure> {
        validate_receipt(receipt)?;
        let context_key = job.context_key().digest().to_string();
        let Some(pending) = self.pending_admissions.get(&context_key).cloned() else {
            if receipt.total_bytes().map_err(protocol_failure)? == 0
                && receipt.device_id().is_none()
            {
                return Ok(());
            }
            return Err(self.protocol_loss(
                "worker emitted a GPU allocation receipt without a host reservation",
            ));
        };
        if receipt.device_id() != Some(pending.physical_device_id.as_str())
            || receipt.total_bytes().map_err(protocol_failure)? == 0
        {
            return Err(self.protocol_loss(
                "worker allocation receipt used no bytes or the wrong physical device",
            ));
        }
        let reported = HostAllocationReceipt {
            model_bytes: receipt.model_bytes(),
            context_bytes: receipt.context_bytes(),
            transient_bytes: receipt.transient_bytes(),
        };
        let commit = self
            .admissions
            .get_mut(&pending.physical_device_id)
            .ok_or_else(|| runtime_failure("device admission ledger disappeared"))?
            .ledger
            .commit(pending.token, reported);
        if let Err(error) = commit {
            let mut detail = format!("worker allocation receipt was rejected: {error}");
            let mut code = error.code().to_string();
            if matches!(
                error,
                ReservationLedgerError::ReceiptInvalid(
                    crate::admission::ReceiptValidationError::EnvelopeExceeded { .. }
                )
            ) && reported.validate_against(pending.full_estimate).is_err()
            {
                code = "resource_estimate_exceeded".to_string();
                match ResourceEstimateQuarantine::new(
                    pending.quarantine_key.clone(),
                    pending.full_estimate,
                    reported,
                ) {
                    Ok(quarantine) => {
                        // Poison this runtime before attempting durable I/O.
                        // A failed store can therefore never permit the same
                        // tuple to allocate again in this process.
                        self.pending_durable_quarantine = Some(quarantine.clone());
                        let stored = self
                            .health_store
                            .as_ref()
                            .ok_or_else(|| {
                                RuntimeFailure::resource_admission(
                                    "inference_health_store_unavailable",
                                    "over-envelope receipt has no durable quarantine store",
                                    "",
                                )
                            })
                            .and_then(|store| {
                                store
                                    .store_resource_quarantine(&quarantine)
                                    .map_err(durable_health_failure)
                            });
                        if let Err(store_error) = stored {
                            code = store_error.code().to_string();
                            detail.push_str(&format!(
                                "; admission remains poisoned because durable quarantine failed ({}): {store_error}",
                                store_error.code()
                            ));
                        } else {
                            self.pending_durable_quarantine = None;
                        }
                    }
                    Err(quarantine_error) => {
                        code = quarantine_error.code().to_string();
                        detail
                            .push_str(&format!("; quarantine record rejected: {quarantine_error}"));
                    }
                }
            }
            let supervision = self.record_session_failure(
                WorkerFailureKind::ProtocolViolation,
                "worker allocation receipt violated its admitted envelope",
            );
            if supervision.code() != WorkerFailureKind::ProtocolViolation.code() {
                return Err(supervision);
            }
            return Err(RuntimeFailure::reaped_resource_generation(
                code,
                detail,
                "worker generation reaped before accepting token output",
            ));
        }
        self.pending_admissions.remove(&context_key);
        let physical_device_id = pending.physical_device_id.clone();
        self.active_admissions.insert(
            context_key,
            ActiveAdmission {
                physical_device_id: pending.physical_device_id,
                token: pending.token,
            },
        );
        self.refresh_status(Some(&physical_device_id));
        Ok(())
    }

    fn finish_job_admission(&mut self, job: &InferenceJob) -> Result<(), RuntimeFailure> {
        let context_key = job.context_key().digest();
        let Some(active) = self.active_admissions.remove(context_key) else {
            return Ok(());
        };
        let result = self
            .admissions
            .get_mut(&active.physical_device_id)
            .ok_or_else(|| runtime_failure("active device admission ledger disappeared"))?
            .ledger
            .finish_active(active.token)
            .map_err(resource_ledger_failure);
        self.refresh_status(Some(&active.physical_device_id));
        result
    }

    fn release_context_admission(&mut self, context_key: &str) -> Result<(), RuntimeFailure> {
        if let Some(pending) = self.pending_admissions.remove(context_key) {
            self.admissions
                .get_mut(&pending.physical_device_id)
                .ok_or_else(|| runtime_failure("pending device admission ledger disappeared"))?
                .ledger
                .abort_pending(pending.token)
                .map_err(resource_ledger_failure)?;
        }
        for admission in self.admissions.values_mut() {
            if admission.ledger.contains_context(context_key) {
                admission
                    .ledger
                    .release_context(context_key)
                    .map_err(resource_ledger_failure)?;
            }
        }
        self.refresh_status(None);
        Ok(())
    }

    fn release_model_admission(&mut self, model_key: &str) -> Result<(), RuntimeFailure> {
        let pending_contexts = self
            .pending_admissions
            .iter()
            .filter(|(_, pending)| pending.model_key == model_key)
            .map(|(context_key, _)| context_key.clone())
            .collect::<Vec<_>>();
        for context_key in pending_contexts {
            self.release_context_admission(&context_key)?;
        }
        for admission in self.admissions.values_mut() {
            if admission.ledger.contains_model(model_key) {
                admission
                    .ledger
                    .release_model(model_key)
                    .map_err(resource_ledger_failure)?;
            }
        }
        self.refresh_status(None);
        Ok(())
    }

    fn release_admission_generation(&mut self) {
        for admission in self.admissions.values_mut() {
            admission.ledger.release_worker_generation();
        }
        self.pending_admissions.clear();
        self.active_admissions.clear();
        self.device_leases.clear();
    }

    /// Releases only the native generation owned by one physical device.
    ///
    /// A device-scoped worker loss must not discard reservations or authority
    /// held for another physical device. The all-device release above remains
    /// reserved for a generation whose device identity is unavailable (for
    /// example an inventory worker) and final host teardown.
    fn release_device_admission_generation(&mut self, physical_device_id: &str) {
        if let Some(admission) = self.admissions.get_mut(physical_device_id) {
            admission.ledger.release_worker_generation();
        }
        self.pending_admissions
            .retain(|_, pending| pending.physical_device_id != physical_device_id);
        self.active_admissions
            .retain(|_, active| active.physical_device_id != physical_device_id);
        self.device_leases.remove(physical_device_id);
    }

    fn missing_receipt_failure(&mut self, error: &RuntimeFailure) -> RuntimeFailure {
        let message = error.message().to_string();
        let log = error.log().to_string();
        if let Err(retirement) = self.retire_session_without_cooldown() {
            return retirement;
        }
        RuntimeFailure::reaped_resource_generation(
            "allocation_receipt_missing",
            format!("inference worker failed before its allocation receipt: {message}"),
            log,
        )
    }

    /// Performs native inventory without replacing a resident worker generation.
    ///
    /// A cold runtime uses an ephemeral inventory sandbox and retires it after
    /// the response. A resident runtime sends the already-supported Inventory
    /// operation to its current worker. The mapped GPU availability remains a
    /// fresh kernel observation and includes resident AGL allocations; this
    /// method never predicts eviction by adding conservative ledger estimates.
    fn worker_device_snapshot(&mut self) -> Result<DeviceSnapshot, RuntimeFailure> {
        match inventory_session_mode(
            self.session.is_some(),
            self.live_models,
            self.live_contexts,
            self.pending_admissions.len(),
            self.active_admissions.len(),
        )? {
            InventorySessionMode::Resident => return self.request_worker_device_snapshot(),
            InventorySessionMode::Ephemeral => {}
        }
        self.acquire_inventory_device_authority()?;
        let result = self.worker_device_snapshot_with_authority();
        self.retire_session_without_cooldown()?;
        result
    }

    fn worker_device_snapshot_with_authority(&mut self) -> Result<DeviceSnapshot, RuntimeFailure> {
        let configuration = self.inventory_sandbox_configuration()?;
        self.ensure_session(configuration, None)?;
        self.request_worker_device_snapshot()
    }

    fn request_worker_device_snapshot(&mut self) -> Result<DeviceSnapshot, RuntimeFailure> {
        let operation_id = self.allocate_operation_id()?;
        self.send(HostCommand::Inventory { operation_id })?;
        let deadline = deadline_after(self.options.operation_timeout);
        loop {
            let received = self.receive_until(deadline)?;
            let (event, descriptors) = received;
            match event {
                WorkerEvent::Inventory {
                    operation_id: received,
                    snapshot,
                } if received == operation_id => {
                    if let Err(error) = descriptors.ensure_empty() {
                        return Err(self.protocol_loss(format!(
                            "worker inventory carried unexpected descriptors: {error}"
                        )));
                    }
                    if let Err(message) = validate_inventory_resource_counts(
                        self.live_models,
                        self.live_contexts,
                        snapshot.loaded_models().len(),
                        snapshot.live_contexts().len(),
                    ) {
                        return Err(self.protocol_loss(message));
                    }
                    return Ok(snapshot.devices().clone());
                }
                WorkerEvent::Log {
                    operation_id: None, ..
                } => {
                    if let Err(error) = descriptors.ensure_empty() {
                        return Err(self.protocol_loss(format!(
                            "worker inventory log carried unexpected descriptors: {error}"
                        )));
                    }
                }
                other => {
                    return Err(
                        self.protocol_loss(format!("unexpected worker inventory event: {other:?}"))
                    );
                }
            }
        }
    }

    fn map_device_snapshot(
        &mut self,
        snapshot: &DeviceSnapshot,
    ) -> Result<Vec<InferenceDeviceInfo>, RuntimeFailure> {
        let mapped = map_worker_device_snapshot(snapshot, &self.options.admitted_devices);
        match mapped {
            Ok(devices) => Ok(devices),
            Err(DeviceSnapshotMappingError::WorkerProtocol(message)) => {
                Err(self.protocol_loss(message))
            }
            Err(DeviceSnapshotMappingError::HostObservation(message)) => Err(
                RuntimeFailure::resource_admission("device_snapshot_invalid", message, ""),
            ),
        }
    }

    fn sandbox_configuration_for_job(
        &self,
        job: &InferenceJob,
    ) -> Result<SandboxConfiguration, RuntimeFailure> {
        let mut model_roots = vec![path_string(job.config().backend.model.as_path())?];
        if let Some(draft) = &job.config().runtime.mtp.draft_model {
            model_roots.push(path_string(draft)?);
        }
        let projector_roots = job
            .config()
            .backend
            .multimodal_projector
            .as_deref()
            .map(path_string)
            .transpose()?
            .into_iter()
            .collect();
        let gpu_device_paths = job
            .config()
            .runtime
            .device
            .as_deref()
            .and_then(|selector| self.device_selectors.get(selector))
            .and_then(|physical_device_id| self.options.admitted_devices.get(physical_device_id))
            .map(|resource| vec![resource.render_node().to_path_buf()])
            .unwrap_or_default();
        self.sandbox_configuration(model_roots, projector_roots, &gpu_device_paths)
    }

    fn inventory_sandbox_configuration(&self) -> Result<SandboxConfiguration, RuntimeFailure> {
        self.sandbox_configuration(Vec::new(), Vec::new(), &self.options.gpu_device_paths)
    }

    fn sandbox_configuration(
        &self,
        model_roots: Vec<String>,
        projector_roots: Vec<String>,
        gpu_device_paths: &[PathBuf],
    ) -> Result<SandboxConfiguration, RuntimeFailure> {
        SandboxConfiguration::new(
            model_roots,
            projector_roots,
            self.options
                .runtime_roots
                .iter()
                .map(|path| path_string(path))
                .collect::<Result<Vec<_>, _>>()?,
            gpu_device_paths
                .iter()
                .map(|path| path_string(path))
                .collect::<Result<Vec<_>, _>>()?,
            path_string(&self.options.private_temp_root)?,
        )
        .map_err(protocol_failure)
    }

    fn physical_device_for_job(&self, job: &InferenceJob) -> Option<String> {
        job.config()
            .runtime
            .device
            .as_deref()
            .and_then(|selector| self.device_selectors.get(selector))
            .cloned()
    }

    fn ensure_device_supervisor(
        &mut self,
        physical_device_id: &str,
        now_unix_ms: u64,
    ) -> Result<(), RuntimeFailure> {
        if self.supervisors.contains_key(physical_device_id) {
            return Ok(());
        }
        let resource = self
            .options
            .admitted_devices
            .get(physical_device_id)
            .ok_or_else(|| runtime_failure("worker supervisor device identity disappeared"))?;
        let driver_build_id = resource.driver_build_id().to_owned();
        let worker_generation_id = self.native_generation_build_id()?;
        let key = WorkerHealthKey::new(physical_device_id, driver_build_id, worker_generation_id)
            .map_err(supervisor_identity_failure)?;
        let store = self.health_store.as_ref().ok_or_else(|| {
            RuntimeFailure::resource_admission(
                "inference_health_store_unavailable",
                "GPU worker supervision has no durable health store",
                "",
            )
        })?;
        let health = store
            .load_worker_health(&key, self.options.circuit_breaker_policy)
            .map_err(durable_health_failure)?
            .unwrap_or_else(|| WorkerHealthState::new(key));
        let supervisor = WorkerSupervisorState::restore(
            health,
            self.options.circuit_breaker_policy,
            now_unix_ms,
        )
        .map_err(supervisor_failure)?;
        self.supervisors
            .insert(physical_device_id.to_string(), supervisor);
        Ok(())
    }

    fn wait_for_device_cooldown(
        &mut self,
        job: &InferenceJob,
        physical_device_id: &str,
    ) -> Result<(), RuntimeFailure> {
        loop {
            let now_unix_ms = unix_time_millis()?;
            self.ensure_device_supervisor(physical_device_id, now_unix_ms)?;
            let not_before = self
                .supervisors
                .get(physical_device_id)
                .and_then(WorkerSupervisorState::cooldown_not_before_unix_ms);
            let Some(not_before) = not_before else {
                return Ok(());
            };
            if now_unix_ms >= not_before {
                self.supervisors
                    .get_mut(physical_device_id)
                    .expect("supervisor was ensured above")
                    .release_cooldown(now_unix_ms)
                    .map_err(supervisor_failure)?;
                self.refresh_status(Some(physical_device_id));
                return Ok(());
            }
            self.refresh_status(Some(physical_device_id));
            if job.should_abort() {
                return Err(runtime_failure(
                    "inference request stopped while its accelerator was cooling down",
                ));
            }
            let remaining = Duration::from_millis(not_before - now_unix_ms);
            std::thread::sleep(remaining.min(RECEIVE_POLL_INTERVAL));
        }
    }

    fn persist_supervisor(&self, physical_device_id: &str) -> Result<(), RuntimeFailure> {
        let supervisor = self
            .supervisors
            .get(physical_device_id)
            .ok_or_else(|| runtime_failure("worker supervisor disappeared before persistence"))?;
        self.health_store
            .as_ref()
            .ok_or_else(|| {
                RuntimeFailure::resource_admission(
                    "inference_health_store_unavailable",
                    "GPU worker supervision has no durable health store",
                    "",
                )
            })?
            .store_worker_health(supervisor.health(), self.options.circuit_breaker_policy)
            .map_err(durable_health_failure)
    }

    fn record_prestart_process_failure(
        &mut self,
        mut process: WorkerProcess,
        physical_device_id: Option<&str>,
        mut kind: WorkerFailureKind,
        detail: impl Into<String>,
    ) -> RuntimeFailure {
        let mut detail = detail.into();
        if let Err(error) = process.terminate_and_reap_with_timeout(self.options.shutdown_timeout) {
            kind = WorkerFailureKind::ReapTimedOut;
            detail.push_str(&format!("; bounded worker reap failed: {error}"));
        }
        let private_log = process.private_stderr_evidence().unwrap_or_default();
        match physical_device_id {
            Some(physical_device_id) => {
                self.record_start_failure_with_log(physical_device_id, kind, detail, private_log)
            }
            None => typed_worker_loss(
                kind,
                detail,
                worker_loss_log("worker was killed and reaped before start", &private_log),
            ),
        }
    }

    fn record_start_failure(
        &mut self,
        physical_device_id: &str,
        kind: WorkerFailureKind,
        detail: impl Into<String>,
    ) -> RuntimeFailure {
        self.record_start_failure_with_log(physical_device_id, kind, detail, "")
    }

    fn record_start_failure_with_log(
        &mut self,
        physical_device_id: &str,
        kind: WorkerFailureKind,
        detail: impl Into<String>,
        private_log: impl AsRef<str>,
    ) -> RuntimeFailure {
        let detail = detail.into();
        let result = (|| {
            let now_unix_ms = unix_time_millis()?;
            self.ensure_device_supervisor(physical_device_id, now_unix_ms)?;
            self.supervisors
                .get_mut(physical_device_id)
                .expect("supervisor was ensured above")
                .record_start_failure(now_unix_ms)
                .map_err(supervisor_failure)?;
            self.persist_supervisor(physical_device_id)?;
            Ok::<(), RuntimeFailure>(())
        })();
        self.live_models = 0;
        self.live_contexts = 0;
        self.release_device_admission_generation(physical_device_id);
        self.refresh_status(Some(physical_device_id));
        match result {
            Ok(()) => typed_worker_loss(
                kind,
                detail,
                worker_loss_log("worker failed before a start receipt", private_log.as_ref()),
            ),
            Err(error) => RuntimeFailure::reaped_resource_generation(
                error.code(),
                format!("{detail}; failed to persist worker cooldown: {error}"),
                worker_loss_log(error.log(), private_log.as_ref()),
            ),
        }
    }

    fn record_session_failure(
        &mut self,
        mut kind: WorkerFailureKind,
        detail: impl Into<String>,
    ) -> RuntimeFailure {
        let mut detail = detail.into();
        let session = self.session.take();
        let Some(mut session) = session else {
            if !self.reap_pending {
                self.release_admission_generation();
            }
            self.refresh_status(None);
            return typed_worker_loss(kind, detail, "worker session was already absent");
        };
        let proven_reaped = if let Err(error) = session
            .process
            .terminate_and_reap_with_timeout(self.options.shutdown_timeout)
        {
            kind = WorkerFailureKind::ReapTimedOut;
            detail.push_str(&format!("; bounded worker reap failed: {error}"));
            false
        } else {
            true
        };
        let private_log = session
            .process
            .private_stderr_evidence()
            .unwrap_or_default();
        let failed_physical_device_id = session.physical_device_id.clone();
        let persistence = if let Some(physical_device_id) = &failed_physical_device_id {
            let now_unix_ms =
                self.record_worker_failure_identity(physical_device_id, &session.identity, kind);
            Some((physical_device_id.clone(), now_unix_ms))
        } else {
            None
        };
        self.live_models = 0;
        self.live_contexts = 0;
        if proven_reaped {
            match failed_physical_device_id.as_deref() {
                Some(physical_device_id) => {
                    self.release_device_admission_generation(physical_device_id)
                }
                None => self.release_admission_generation(),
            }
        } else {
            self.reap_pending = true;
        }
        let preferred = persistence.as_ref().map(|(physical, _)| physical.as_str());
        self.refresh_status(preferred);
        if let Some((_, Err(error))) = persistence {
            return RuntimeFailure::reaped_resource_generation(
                error.code(),
                format!("{detail}; failed to persist worker cooldown: {error}"),
                worker_loss_log(error.log(), &private_log),
            );
        }
        typed_worker_loss(
            kind,
            detail,
            worker_loss_log(
                if proven_reaped {
                    "exact worker generation was killed and reaped"
                } else {
                    "exact worker generation was not proven reaped; device authority remains held"
                },
                &private_log,
            ),
        )
    }

    fn record_worker_failure_identity(
        &mut self,
        physical_device_id: &str,
        identity: &WorkerGenerationIdentity,
        kind: WorkerFailureKind,
    ) -> Result<(), RuntimeFailure> {
        let now = unix_time_millis()?;
        self.supervisors
            .get_mut(physical_device_id)
            .ok_or_else(|| runtime_failure("worker supervisor disappeared during reap"))?
            .record_worker_failure(identity, kind, now)
            .map_err(supervisor_failure)?;
        self.persist_supervisor(physical_device_id)
    }

    fn refresh_status(&self, preferred_physical_device_id: Option<&str>) {
        let reserved_bytes = self
            .admissions
            .values()
            .filter_map(|admission| admission.ledger.status().ok())
            .filter_map(|status| status.resident.total_bytes().ok())
            .fold(0_u64, u64::saturating_add);
        let physical_device_id = self
            .session
            .as_ref()
            .and_then(|session| session.physical_device_id.clone())
            .or_else(|| preferred_physical_device_id.map(str::to_string))
            .or_else(|| {
                self.supervisors
                    .iter()
                    .find(|(_, supervisor)| supervisor.phase() == WorkerLifecyclePhase::CoolingDown)
                    .map(|(physical, _)| physical.clone())
            });
        let supervisor = physical_device_id
            .as_deref()
            .and_then(|physical| self.supervisors.get(physical));
        let session = self.session.as_ref();
        let phase = supervisor.map_or_else(
            || {
                if session.is_some() {
                    WorkerLifecyclePhase::Ready
                } else {
                    WorkerLifecyclePhase::Cold
                }
            },
            WorkerSupervisorState::phase,
        );
        self.status.replace(WorkerRuntimeStatus {
            worker_build_id: self
                .native_bundle_id
                .as_deref()
                .map_or_else(|| WORKER_BUILD_ID.to_string(), composite_worker_build_id),
            phase,
            worker_pid: session.map(|session| session.identity.pid()),
            launch_generation: session.map(|session| session.identity.launch_generation()),
            physical_device_id,
            reserved_bytes,
            cooldown_not_before_unix_ms: supervisor
                .and_then(WorkerSupervisorState::cooldown_not_before_unix_ms),
        });
    }

    fn ensure_session(
        &mut self,
        configuration: SandboxConfiguration,
        physical_device_id: Option<String>,
    ) -> Result<(), RuntimeFailure> {
        if self.reap_pending {
            return Err(RuntimeFailure::reaped_resource_generation(
                "inference_worker_reap_pending",
                "a prior inference worker could not be proven reaped; refusing another worker start",
                "",
            ));
        }
        let configuration = self.with_native_bundle_authority(configuration)?;
        match physical_device_id.as_deref() {
            Some(physical_device_id) if !self.device_leases.contains_key(physical_device_id) => {
                return Err(RuntimeFailure::resource_admission(
                    "device_authority_unavailable",
                    "GPU worker start requires an exact live device authority lease",
                    "",
                ));
            }
            None if !configuration.gpu_device_paths().is_empty()
                && self
                    .options
                    .admitted_devices
                    .keys()
                    .any(|physical| !self.device_leases.contains_key(physical)) =>
            {
                return Err(RuntimeFailure::resource_admission(
                    "device_authority_unavailable",
                    "GPU inventory worker start requires authority over every admitted device",
                    "",
                ));
            }
            _ => {}
        }
        if self.session.as_ref().is_some_and(|session| {
            session.configuration == configuration
                && session.physical_device_id == physical_device_id
        }) {
            return Ok(());
        }
        if self.live_models != 0 || self.live_contexts != 0 {
            return Err(runtime_failure(
                "inference worker sandbox roots cannot change while remote resources are live",
            ));
        }
        self.shutdown_session()?;

        if let Some(physical_device_id) = &physical_device_id {
            let now = unix_time_millis()?;
            self.ensure_device_supervisor(physical_device_id, now)?;
            if let Some(not_before) = self
                .supervisors
                .get(physical_device_id)
                .and_then(WorkerSupervisorState::cooldown_not_before_unix_ms)
            {
                return Err(RuntimeFailure::resource_admission(
                    "inference_worker_cooling_down",
                    format!("accelerator is cooling down until Unix millisecond {not_before}"),
                    "",
                ));
            }
        }

        let executable = match self.options.executable() {
            Ok(executable) => executable,
            Err(error) => {
                return Err(match &physical_device_id {
                    Some(physical) => self.record_start_failure(
                        physical,
                        WorkerFailureKind::SpawnFailed,
                        error.to_string(),
                    ),
                    None => error,
                });
            }
        };
        let mut process = match WorkerProcess::spawn_with_environment(
            &executable,
            self.options.handshake_timeout,
            &self.options.environment,
        ) {
            Ok(process) => process,
            Err(error) => {
                let kind = start_failure_kind(&error);
                return Err(match &physical_device_id {
                    Some(physical) => self.record_start_failure_with_log(
                        physical,
                        kind,
                        error.to_string(),
                        error.private_log(),
                    ),
                    None => typed_worker_loss(
                        kind,
                        error.to_string(),
                        worker_loss_log("worker start failed", error.private_log()),
                    ),
                });
            }
        };
        let pid = match process.child_id() {
            Some(pid) => pid,
            None => {
                return Err(self.record_prestart_process_failure(
                    process,
                    physical_device_id.as_deref(),
                    WorkerFailureKind::HandshakeFailed,
                    "worker handshake returned without a live child PID",
                ));
            }
        };
        let launch_generation = self.next_launch_generation;
        let Some(next_launch_generation) = self.next_launch_generation.checked_add(1) else {
            return Err(self.record_prestart_process_failure(
                process,
                physical_device_id.as_deref(),
                WorkerFailureKind::ProtocolViolation,
                "worker launch generation space exhausted",
            ));
        };
        self.next_launch_generation = next_launch_generation;
        let identity = match WorkerGenerationIdentity::new(
            pid,
            launch_generation,
            process.generation_build_id(),
        ) {
            Ok(identity) => identity,
            Err(error) => {
                return Err(self.record_prestart_process_failure(
                    process,
                    physical_device_id.as_deref(),
                    WorkerFailureKind::HandshakeFailed,
                    format!("invalid worker generation identity: {error}"),
                ));
            }
        };
        if let Some(physical) = &physical_device_id
            && let Err(error) = self
                .supervisors
                .get_mut(physical)
                .expect("device supervisor was ensured above")
                .begin_start(identity.clone())
        {
            return Err(self.record_prestart_process_failure(
                process,
                Some(physical),
                WorkerFailureKind::ProtocolViolation,
                format!("worker supervisor rejected start: {error}"),
            ));
        }
        let operation_id = match self.allocate_operation_id() {
            Ok(operation_id) => operation_id,
            Err(error) => {
                self.session = Some(WorkerSession {
                    process,
                    configuration,
                    identity,
                    physical_device_id,
                    active_attempt: None,
                    attempts: BTreeMap::new(),
                });
                return Err(self.record_session_failure(
                    WorkerFailureKind::ProtocolViolation,
                    error.to_string(),
                ));
            }
        };
        if let Err(error) = process.channel_mut().send(HostCommand::ConfigureSandbox {
            operation_id,
            configuration: configuration.clone(),
        }) {
            if physical_device_id.is_some() {
                self.session = Some(WorkerSession {
                    process,
                    configuration,
                    identity,
                    physical_device_id,
                    active_attempt: None,
                    attempts: BTreeMap::new(),
                });
                return Err(self.record_session_failure(
                    WorkerFailureKind::ProtocolViolation,
                    error.to_string(),
                ));
            }
            let reap = process.terminate_and_reap_with_timeout(self.options.shutdown_timeout);
            let kind = if reap.is_err() {
                WorkerFailureKind::ReapTimedOut
            } else {
                WorkerFailureKind::ProtocolViolation
            };
            let private_log = process.private_stderr_evidence().unwrap_or_default();
            return Err(typed_worker_loss(
                kind,
                error.to_string(),
                worker_loss_log("worker was killed and reaped", &private_log),
            ));
        }
        let event = match process
            .channel_mut()
            .receive_timeout(self.options.operation_timeout)
        {
            Ok(event) => event,
            Err(error) => {
                let kind = transport_failure_kind(&mut process, &error);
                if physical_device_id.is_some() {
                    self.session = Some(WorkerSession {
                        process,
                        configuration,
                        identity,
                        physical_device_id,
                        active_attempt: None,
                        attempts: BTreeMap::new(),
                    });
                    return Err(self.record_session_failure(kind, error.to_string()));
                }
                let kind = if process
                    .terminate_and_reap_with_timeout(self.options.shutdown_timeout)
                    .is_err()
                {
                    WorkerFailureKind::ReapTimedOut
                } else {
                    kind
                };
                let private_log = process.private_stderr_evidence().unwrap_or_default();
                return Err(typed_worker_loss(
                    kind,
                    error.to_string(),
                    worker_loss_log("worker was killed and reaped", &private_log),
                ));
            }
        };
        if !matches!(event, WorkerEvent::SandboxReady { operation_id: received } if received == operation_id)
        {
            self.session = Some(WorkerSession {
                process,
                configuration,
                identity,
                physical_device_id,
                active_attempt: None,
                attempts: BTreeMap::new(),
            });
            return Err(self.record_session_failure(
                WorkerFailureKind::ProtocolViolation,
                format!("unexpected sandbox admission event: {event:?}"),
            ));
        }
        if let Some(physical) = &physical_device_id
            && let Err(error) = self
                .supervisors
                .get_mut(physical)
                .expect("device supervisor was ensured above")
                .mark_ready(&identity)
        {
            self.session = Some(WorkerSession {
                process,
                configuration,
                identity,
                physical_device_id,
                active_attempt: None,
                attempts: BTreeMap::new(),
            });
            return Err(self.record_session_failure(
                WorkerFailureKind::ProtocolViolation,
                format!("worker supervisor rejected ready receipt: {error}"),
            ));
        }
        self.session = Some(WorkerSession {
            process,
            configuration,
            identity,
            physical_device_id,
            active_attempt: None,
            attempts: BTreeMap::new(),
        });
        self.refresh_status(None);
        Ok(())
    }

    fn with_native_bundle_authority(
        &mut self,
        configuration: SandboxConfiguration,
    ) -> Result<SandboxConfiguration, RuntimeFailure> {
        let worker_path = self.options.worker_executable_path()?;
        let native = crate::worker_resources::discover_native_bundle_for_worker(
            &worker_path,
            &worker_path,
            !configuration.gpu_device_paths().is_empty(),
        )
        .map_err(|error| {
            runtime_failure(format!(
                "failed to validate exact native inference bundle ({}): {error}",
                error.code().as_str()
            ))
        })?;
        if let Some(expected) = &self.native_bundle_id {
            if expected != native.identity() {
                return Err(runtime_failure(
                    "native inference bundle identity changed during the host runtime lifetime",
                ));
            }
        } else {
            self.native_bundle_id = Some(native.identity().to_owned());
        }
        let mut runtime_roots = configuration.runtime_roots().to_vec();
        runtime_roots.push(path_string(native.directory())?);
        runtime_roots.extend(
            native
                .external_dependencies()
                .iter()
                .map(|path| path_string(path))
                .collect::<Result<Vec<_>, _>>()?,
        );
        runtime_roots.sort();
        runtime_roots.dedup();
        SandboxConfiguration::new(
            configuration.model_roots().to_vec(),
            configuration.projector_roots().to_vec(),
            runtime_roots,
            configuration.gpu_device_paths().to_vec(),
            configuration.private_temp_root(),
        )
        .map_err(protocol_failure)
    }

    fn native_generation_build_id(&self) -> Result<String, RuntimeFailure> {
        self.native_bundle_id
            .as_deref()
            .map(composite_worker_build_id)
            .ok_or_else(|| runtime_failure("native inference bundle identity is not admitted"))
    }

    fn send(&mut self, command: HostCommand) -> Result<(), RuntimeFailure> {
        let result = {
            let session = self.session_mut()?;
            session.process.channel_mut().send(command)
        };
        result.map_err(|error| self.worker_transport_loss(error))
    }

    fn send_payload(
        &mut self,
        command: impl FnOnce(crate::worker_protocol::SealedPayload) -> HostCommand,
        bytes: &[u8],
    ) -> Result<(), RuntimeFailure> {
        let (payload, descriptor) = SealedPayloadTransfer::new(bytes, 0)
            .map_err(protocol_failure)?
            .into_parts();
        let result = {
            let session = self.session_mut()?;
            session
                .process
                .channel_mut()
                .send_with_descriptors(command(payload), vec![descriptor])
        };
        result.map_err(|error| self.worker_transport_loss(error))
    }

    fn receive_until(
        &mut self,
        deadline: Instant,
    ) -> Result<(WorkerEvent, DescriptorSet), RuntimeFailure> {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(self.record_session_failure(
                    WorkerFailureKind::ForcedAfterDeadline,
                    "inference worker operation timed out",
                ));
            }
            let wait = deadline
                .saturating_duration_since(now)
                .min(RECEIVE_POLL_INTERVAL);
            let received = {
                let session = self.session_mut()?;
                session
                    .process
                    .channel_mut()
                    .receive_timeout_with_descriptors(wait)
            };
            match received {
                Ok(received) => return Ok(received.into_parts()),
                Err(error)
                    if error.code()
                        == crate::worker_protocol::WorkerProtocolErrorCode::TimedOut => {}
                Err(error) => return Err(self.worker_transport_loss(error)),
            }
        }
    }

    fn run_job_operation<T>(
        &mut self,
        operation_id: OperationId,
        job: &InferenceJob,
        terminal: impl Fn(
            WorkerEvent,
            &mut DescriptorSet,
        ) -> Result<Option<RuntimeOperation<T>>, RuntimeFailure>,
        expect_started: bool,
        finish_attempt_on_success: bool,
    ) -> Result<RuntimeOperation<T>, RuntimeFailure> {
        if expect_started {
            self.begin_supervised_attempt(job)?;
        }
        let ordinary_deadline = deadline_after(self.options.operation_timeout);
        let mut cancellation_deadline = None;
        let mut cancel_operation = None;
        let mut started = false;
        let mut collected_log = String::new();

        loop {
            if job.should_abort() && cancel_operation.is_none() {
                let cancel_id = self.allocate_operation_id()?;
                self.send(HostCommand::Cancel {
                    operation_id: cancel_id,
                    target_operation_id: operation_id,
                })?;
                cancel_operation = Some(cancel_id);
                cancellation_deadline = Some(deadline_after(self.options.cancellation_grace));
            }
            let deadline = cancellation_deadline.unwrap_or(ordinary_deadline);
            let (event, mut descriptors) = match self.receive_until(deadline) {
                Ok(received) => received,
                Err(error) if cancellation_deadline.is_some() => {
                    return Err(self.record_session_failure(
                        WorkerFailureKind::ForcedAfterCancellation,
                        format!("inference worker ignored cancellation: {error}"),
                    ));
                }
                Err(error) => return Err(error),
            };

            match event {
                WorkerEvent::Started {
                    operation_id: received,
                    allocation_receipt,
                } if received == operation_id && expect_started && !started => {
                    if let Err(error) = descriptors.ensure_empty() {
                        return Err(self.protocol_loss(format!(
                            "worker allocation receipt carried unexpected descriptors: {error}"
                        )));
                    }
                    if let Err(error) = self.reconcile_started_receipt(job, &allocation_receipt) {
                        if error.is_backend_lost() && self.session.is_some() {
                            return Err(self.record_session_failure(
                                WorkerFailureKind::ProtocolViolation,
                                error.message().to_string(),
                            ));
                        }
                        return Err(error);
                    }
                    started = true;
                }
                WorkerEvent::Output {
                    operation_id: received,
                    event,
                } if received == operation_id => {
                    if let Err(error) = descriptors.ensure_empty() {
                        return Err(self.protocol_loss(format!(
                            "worker output carried unexpected descriptors: {error}"
                        )));
                    }
                    if matches!(event, InferenceOutputEvent::TextDelta { .. })
                        && expect_started
                        && !started
                    {
                        return Err(self.protocol_loss(
                            "inference worker emitted text before allocation receipt",
                        ));
                    }
                    if let Err(error) = self.validate_and_forward_output(job, event) {
                        let cancel_id = self.allocate_operation_id().ok();
                        if let Some(cancel_id) = cancel_id {
                            let _ = self.send(HostCommand::Cancel {
                                operation_id: cancel_id,
                                target_operation_id: operation_id,
                            });
                        }
                        return Err(self.protocol_loss(format!(
                            "inference output delivery failed; active worker was cancelled and reaped: {error}"
                        )));
                    }
                }
                WorkerEvent::Log {
                    operation_id: received,
                    record,
                } if received.is_none() || received == Some(operation_id) => {
                    if let Err(error) = descriptors.ensure_empty() {
                        return Err(self.protocol_loss(format!(
                            "worker log carried unexpected descriptors: {error}"
                        )));
                    }
                    append_structured_log(&mut collected_log, &record);
                }
                WorkerEvent::CancelAccepted {
                    operation_id: received,
                    target_operation_id,
                } if Some(received) == cancel_operation && target_operation_id == operation_id => {
                    if let Err(error) = descriptors.ensure_empty() {
                        return Err(self.protocol_loss(format!(
                            "worker cancellation receipt carried unexpected descriptors: {error}"
                        )));
                    }
                }
                WorkerEvent::Failed {
                    operation_id: received,
                    failure,
                    log,
                } if Some(received) == cancel_operation => {
                    if let Err(error) =
                        append_optional_log(&mut collected_log, log, &mut descriptors)
                    {
                        return Err(self.record_session_failure(
                            WorkerFailureKind::ProtocolViolation,
                            error.message().to_string(),
                        ));
                    }
                    if let Err(error) = descriptors.ensure_empty() {
                        return Err(self.protocol_loss(format!(
                            "worker cancellation failure carried unexpected descriptors: {error}"
                        )));
                    }
                    if failure.code() != WorkerFailureCode::CancelTargetNotActive {
                        return Err(self.protocol_loss(format!(
                            "worker cancellation command failed: {}",
                            failure.message()
                        )));
                    }
                }
                event => {
                    let terminal = terminal(event, &mut descriptors);
                    let terminal = match terminal {
                        Ok(terminal) => terminal,
                        Err(error) => {
                            self.finish_attempt(job);
                            if error.is_backend_lost() {
                                let kind = runtime_worker_failure_kind(&error);
                                return Err(
                                    self.record_session_failure(kind, error.message().to_string())
                                );
                            }
                            if expect_started && !started {
                                self.end_supervised_attempt_without_worker_failure(job)?;
                                return Err(self.missing_receipt_failure(&error));
                            }
                            self.end_supervised_attempt_without_worker_failure(job)?;
                            self.finish_job_admission(job)?;
                            return Err(error);
                        }
                    };
                    if let Some(mut operation) = terminal {
                        if let Err(error) = descriptors.ensure_empty() {
                            return Err(self.protocol_loss(format!(
                                "worker terminal event carried unexpected descriptors: {error}"
                            )));
                        }
                        append_bounded(&mut collected_log, &operation.log);
                        operation.log = collected_log;
                        if expect_started && !started {
                            return Err(self.protocol_loss(
                                "inference worker completed without allocation receipt",
                            ));
                        }
                        if finish_attempt_on_success {
                            self.finish_attempt(job);
                            self.finish_job_admission(job)?;
                            self.complete_supervised_attempt_success(job)?;
                        }
                        return Ok(operation);
                    }
                    return Err(self
                        .protocol_loss("inference worker emitted an event for another operation"));
                }
            }
        }
    }

    fn validate_and_forward_output(
        &mut self,
        job: &InferenceJob,
        event: InferenceOutputEvent,
    ) -> Result<(), RuntimeFailure> {
        let attempt_id = match &event {
            InferenceOutputEvent::TextDelta { attempt_id, .. } => attempt_id,
            InferenceOutputEvent::Stage(stage) => &stage.attempt_id,
        };
        if attempt_id != &job.request().attempt_id {
            return Err(self.protocol_loss("worker output used the wrong attempt identity"));
        }
        let validation = {
            let stream = self
                .session_mut()?
                .attempts
                .entry(attempt_id.clone())
                .or_insert_with(|| AttemptStream::new(attempt_id.clone()));
            match &event {
                InferenceOutputEvent::Stage(stage) => stream
                    .stages
                    .accept(stage)
                    .map_err(|error| error.to_string()),
                InferenceOutputEvent::TextDelta { sequence, .. } => {
                    if *sequence != stream.next_delta_sequence {
                        Err(format!(
                            "worker delta sequence mismatch: expected {}, received {sequence}",
                            stream.next_delta_sequence
                        ))
                    } else {
                        match stream.next_delta_sequence.checked_add(1) {
                            Some(next) => {
                                stream.next_delta_sequence = next;
                                Ok(())
                            }
                            None => Err("worker delta sequence exhausted".to_string()),
                        }
                    }
                }
            }
        };
        if let Err(message) = validation {
            return Err(self.protocol_loss(message));
        }
        if job.output_sink().try_emit(event) != OutputDelivery::Delivered {
            return Err(runtime_failure(
                "inference output consumer could not accept ordered worker output",
            ));
        }
        Ok(())
    }

    fn begin_supervised_attempt(&mut self, job: &InferenceJob) -> Result<(), RuntimeFailure> {
        let (physical_device_id, identity) = match self.session.as_ref() {
            Some(session) => match &session.physical_device_id {
                Some(physical) => (physical.clone(), session.identity.clone()),
                None => return Ok(()),
            },
            None => {
                return Err(runtime_failure(
                    "inference worker session is not configured",
                ));
            }
        };
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.active_attempt.is_some())
        {
            return Err(self.protocol_loss("worker session already has an active attempt"));
        }
        let attempt = ActiveAttemptIdentity::new(self.next_attempt_generation)
            .map_err(supervisor_identity_failure)?;
        self.next_attempt_generation = self
            .next_attempt_generation
            .checked_add(1)
            .ok_or_else(|| self.protocol_loss("worker attempt generation space exhausted"))?;
        self.supervisors
            .get_mut(&physical_device_id)
            .ok_or_else(|| runtime_failure("worker supervisor disappeared before dispatch"))?
            .begin_attempt(&identity, attempt)
            .map_err(supervisor_failure)?;
        self.session
            .as_mut()
            .expect("session was inspected above")
            .active_attempt = Some((job.request().attempt_id.clone(), attempt));
        self.refresh_status(Some(&physical_device_id));
        Ok(())
    }

    fn complete_supervised_attempt_success(
        &mut self,
        job: &InferenceJob,
    ) -> Result<(), RuntimeFailure> {
        let Some((physical, identity, attempt)) = self.active_supervision_for(job)? else {
            return Ok(());
        };
        self.supervisors
            .get_mut(&physical)
            .ok_or_else(|| runtime_failure("worker supervisor disappeared before completion"))?
            .complete_active_success(&identity, attempt)
            .map_err(supervisor_failure)?;
        self.persist_supervisor(&physical)?;
        self.session
            .as_mut()
            .expect("active supervision has a session")
            .active_attempt = None;
        self.refresh_status(Some(&physical));
        Ok(())
    }

    fn end_supervised_attempt_without_worker_failure(
        &mut self,
        job: &InferenceJob,
    ) -> Result<(), RuntimeFailure> {
        let Some((physical, identity, attempt)) = self.active_supervision_for(job)? else {
            return Ok(());
        };
        self.supervisors
            .get_mut(&physical)
            .ok_or_else(|| runtime_failure("worker supervisor disappeared before attempt end"))?
            .end_active_without_worker_failure(&identity, attempt)
            .map_err(supervisor_failure)?;
        self.session
            .as_mut()
            .expect("active supervision has a session")
            .active_attempt = None;
        self.refresh_status(Some(&physical));
        Ok(())
    }

    fn active_supervision_for(
        &self,
        job: &InferenceJob,
    ) -> Result<Option<(String, WorkerGenerationIdentity, ActiveAttemptIdentity)>, RuntimeFailure>
    {
        let Some(session) = &self.session else {
            return Ok(None);
        };
        let Some(physical) = &session.physical_device_id else {
            return Ok(None);
        };
        let Some((attempt_id, attempt)) = &session.active_attempt else {
            return Err(runtime_failure("supervised worker has no active attempt"));
        };
        if attempt_id != &job.request().attempt_id {
            return Err(runtime_failure(
                "supervised worker attempt belongs to another request",
            ));
        }
        Ok(Some((physical.clone(), session.identity.clone(), *attempt)))
    }

    fn finish_attempt(&mut self, job: &InferenceJob) {
        if let Some(session) = &mut self.session {
            session.attempts.remove(&job.request().attempt_id);
        }
    }

    fn session_mut(&mut self) -> Result<&mut WorkerSession, RuntimeFailure> {
        self.session
            .as_mut()
            .ok_or_else(|| runtime_failure("inference worker session is not configured"))
    }

    fn allocate_operation_id(&mut self) -> Result<OperationId, RuntimeFailure> {
        let value = self.next_operation_id;
        let Some(next) = self.next_operation_id.checked_add(1) else {
            return Err(self.protocol_loss("worker operation ID space exhausted"));
        };
        self.next_operation_id = next;
        OperationId::new(value).map_err(protocol_failure)
    }

    fn allocate_model_id(&mut self) -> Result<ModelResourceId, RuntimeFailure> {
        let value = self.allocate_resource_value()?;
        ModelResourceId::new(value).map_err(protocol_failure)
    }

    fn allocate_context_id(&mut self) -> Result<ContextResourceId, RuntimeFailure> {
        let value = self.allocate_resource_value()?;
        ContextResourceId::new(value).map_err(protocol_failure)
    }

    fn allocate_resource_value(&mut self) -> Result<u64, RuntimeFailure> {
        let value = self.next_resource_id;
        let Some(next) = self.next_resource_id.checked_add(1) else {
            return Err(self.protocol_loss("worker resource ID space exhausted"));
        };
        self.next_resource_id = next;
        Ok(value)
    }

    fn protocol_loss(&mut self, message: impl Into<String>) -> RuntimeFailure {
        self.record_session_failure(WorkerFailureKind::ProtocolViolation, message)
    }

    fn worker_transport_loss(&mut self, error: WorkerProtocolError) -> RuntimeFailure {
        let message = format!("inference worker transport failed: {error}");
        let kind = self
            .session
            .as_mut()
            .map(|session| transport_failure_kind(&mut session.process, &error))
            .unwrap_or(WorkerFailureKind::ProtocolViolation);
        self.record_session_failure(kind, message)
    }

    fn retire_session_without_cooldown(&mut self) -> Result<(), RuntimeFailure> {
        let mut retired_physical_device_id = None;
        let mut reap_failure = None;
        if let Some(mut session) = self.session.take() {
            retired_physical_device_id = session.physical_device_id.clone();
            if let Some(physical) = &session.physical_device_id
                && let Some(supervisor) = self.supervisors.get_mut(physical)
            {
                if let Some((_, attempt)) = session.active_attempt.take() {
                    let _ =
                        supervisor.end_active_without_worker_failure(&session.identity, attempt);
                }
                let _ = supervisor.retire_worker(&session.identity);
            }
            reap_failure = session
                .process
                .terminate_and_reap_with_timeout(self.options.shutdown_timeout)
                .err();
        }
        self.live_models = 0;
        self.live_contexts = 0;
        if reap_failure.is_none() {
            match retired_physical_device_id.as_deref() {
                Some(physical_device_id) => {
                    self.release_device_admission_generation(physical_device_id)
                }
                None => self.release_admission_generation(),
            }
        } else {
            self.reap_pending = true;
        }
        self.refresh_status(None);
        match reap_failure {
            Some(error) => Err(RuntimeFailure::reaped_resource_generation(
                "inference_worker_reap_pending",
                error.to_string(),
                "worker generation was not proven reaped; device authority remains held",
            )),
            None => Ok(()),
        }
    }

    fn shutdown_session(&mut self) -> Result<(), RuntimeFailure> {
        let mut retired_physical_device_id = None;
        let mut shutdown_failure = None;
        let mut proven_reaped = true;
        if let Some(mut session) = self.session.take() {
            let physical = session.physical_device_id.clone();
            retired_physical_device_id = physical.clone();
            let identity = session.identity.clone();
            let active_attempt = session.active_attempt.take();
            let (shutdown, reaped) = session.process.shutdown_with_reap_status(
                ShutdownReason::HostShutdown,
                self.options.shutdown_timeout,
            );
            proven_reaped = reaped;
            if let Some(physical) = &physical {
                match &shutdown {
                    Ok(()) => {
                        if let Some(supervisor) = self.supervisors.get_mut(physical) {
                            if let Some((_, attempt)) = active_attempt {
                                let _ = supervisor
                                    .end_active_without_worker_failure(&identity, attempt);
                            }
                            let _ = supervisor.retire_worker(&identity);
                        }
                    }
                    Err(error) => {
                        let kind = match error.code() {
                            WorkerProtocolErrorCode::PeerClosed | WorkerProtocolErrorCode::Io => {
                                WorkerFailureKind::Exited
                            }
                            WorkerProtocolErrorCode::TimedOut => WorkerFailureKind::ReapTimedOut,
                            _ => WorkerFailureKind::ProtocolViolation,
                        };
                        let _ = self.record_worker_failure_identity(physical, &identity, kind);
                    }
                }
            }
            shutdown_failure = shutdown.err();
        }
        self.live_models = 0;
        self.live_contexts = 0;
        if proven_reaped && let Some(physical_device_id) = retired_physical_device_id.as_deref() {
            self.release_device_admission_generation(physical_device_id);
        }
        if !proven_reaped {
            self.reap_pending = true;
        }
        self.refresh_status(None);
        match shutdown_failure {
            Some(error) => Err(RuntimeFailure::reaped_resource_generation(
                if proven_reaped {
                    "inference_worker_shutdown_failed"
                } else {
                    "inference_worker_reap_pending"
                },
                error.to_string(),
                if proven_reaped {
                    "worker generation was force-reaped after shutdown failure"
                } else {
                    "worker generation was not proven reaped; device authority remains held"
                },
            )),
            None => Ok(()),
        }
    }
}

impl Drop for WorkerModelRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown_session();
        if !self.reap_pending {
            self.release_admission_generation();
        }
    }
}

#[derive(Debug)]
pub struct RemoteModel {
    resource_id: ModelResourceId,
    key_digest: String,
}

#[derive(Debug)]
pub struct RemoteContext {
    resource_id: ContextResourceId,
    model_resource_id: ModelResourceId,
    key_digest: String,
}

impl ModelRuntime for WorkerModelRuntime {
    type Model = RemoteModel;
    type Context = RemoteContext;

    fn device_inventory(&mut self) -> Result<Vec<InferenceDeviceInfo>, RuntimeFailure> {
        let snapshot = self.worker_device_snapshot()?;
        self.map_device_snapshot(&snapshot)
    }

    fn load_model(
        &mut self,
        job: &InferenceJob,
    ) -> Result<RuntimeOperation<Self::Model>, RuntimeFailure> {
        self.ensure_job_admission(job)?;
        let configuration = self.sandbox_configuration_for_job(job)?;
        let physical_device_id = self.physical_device_for_job(job);
        self.ensure_session(configuration, physical_device_id)?;
        let operation_id = self.allocate_operation_id()?;
        let resource_id = self.allocate_model_id()?;
        let payload = job
            .encode_worker_payload(Instant::now())
            .map_err(|error| runtime_failure(error.to_string()))?;
        self.send_payload(
            |job| HostCommand::LoadModel {
                operation_id,
                model_resource_id: resource_id,
                job,
            },
            &payload,
        )?;
        let operation = match self.run_job_operation(
            operation_id,
            job,
            |event, descriptors| match event {
                WorkerEvent::ModelLoaded {
                    operation_id: received,
                    model_resource_id,
                    log,
                } if received == operation_id && model_resource_id == resource_id => {
                    let mut text = String::new();
                    append_optional_log(&mut text, log, descriptors)?;
                    Ok(Some(RuntimeOperation::new((), text)))
                }
                WorkerEvent::Failed {
                    operation_id: received,
                    failure,
                    log,
                } if received == operation_id => Err(worker_failure(failure, log, descriptors)),
                _ => Ok(None),
            },
            false,
            false,
        ) {
            Ok(operation) => operation,
            Err(error) => {
                if error.is_backend_lost() {
                    return Err(error);
                }
                let code = error.code().to_string();
                let message = error.message().to_string();
                let log = error.log().to_string();
                self.retire_session_without_cooldown()?;
                return Err(RuntimeFailure::reaped_resource_generation(
                    code,
                    format!("model allocation failed before a worker receipt: {message}"),
                    log,
                ));
            }
        };
        self.live_models = self.live_models.saturating_add(1);
        Ok(RuntimeOperation::new(
            RemoteModel {
                resource_id,
                key_digest: job.model_key().digest().to_string(),
            },
            operation.log,
        ))
    }

    fn create_context(
        &mut self,
        model: &mut Self::Model,
        job: &InferenceJob,
    ) -> Result<RuntimeOperation<Self::Context>, RuntimeFailure> {
        self.ensure_job_admission(job)?;
        if model.key_digest != job.model_key().digest() {
            return Err(runtime_failure("remote model identity does not match job"));
        }
        let operation_id = self.allocate_operation_id()?;
        let context_id = self.allocate_context_id()?;
        let model_id = model.resource_id;
        let payload = job
            .encode_worker_payload(Instant::now())
            .map_err(|error| runtime_failure(error.to_string()))?;
        self.send_payload(
            |job| HostCommand::CreateContext {
                operation_id,
                model_resource_id: model_id,
                context_resource_id: context_id,
                job,
            },
            &payload,
        )?;
        let operation = match self.run_job_operation(
            operation_id,
            job,
            |event, descriptors| match event {
                WorkerEvent::ContextCreated {
                    operation_id: received,
                    model_resource_id,
                    context_resource_id,
                    log,
                } if received == operation_id
                    && model_resource_id == model_id
                    && context_resource_id == context_id =>
                {
                    let mut text = String::new();
                    append_optional_log(&mut text, log, descriptors)?;
                    Ok(Some(RuntimeOperation::new((), text)))
                }
                WorkerEvent::Failed {
                    operation_id: received,
                    failure,
                    log,
                } if received == operation_id => Err(worker_failure(failure, log, descriptors)),
                _ => Ok(None),
            },
            false,
            false,
        ) {
            Ok(operation) => operation,
            Err(error) => {
                if error.is_backend_lost() {
                    return Err(error);
                }
                let code = error.code().to_string();
                let message = error.message().to_string();
                let log = error.log().to_string();
                self.retire_session_without_cooldown()?;
                return Err(RuntimeFailure::reaped_resource_generation(
                    code,
                    format!("context allocation failed before a worker receipt: {message}"),
                    log,
                ));
            }
        };
        self.live_contexts = self.live_contexts.saturating_add(1);
        Ok(RuntimeOperation::new(
            RemoteContext {
                resource_id: context_id,
                model_resource_id: model_id,
                key_digest: job.context_key().digest().to_string(),
            },
            operation.log,
        ))
    }

    fn generate(
        &mut self,
        model: &mut Self::Model,
        context: &mut Self::Context,
        job: &InferenceJob,
    ) -> Result<RuntimeOperation<ModelGeneration>, RuntimeFailure> {
        self.ensure_job_admission(job)?;
        if model.resource_id != context.model_resource_id
            || model.key_digest != job.model_key().digest()
            || context.key_digest != job.context_key().digest()
        {
            return Err(runtime_failure(
                "remote inference resource identity mismatch",
            ));
        }
        let operation_id = self.allocate_operation_id()?;
        let model_id = model.resource_id;
        let context_id = context.resource_id;
        let payload = job
            .encode_worker_payload(Instant::now())
            .map_err(|error| runtime_failure(error.to_string()))?;
        self.send_payload(
            |job| HostCommand::Generate {
                operation_id,
                model_resource_id: model_id,
                context_resource_id: context_id,
                job,
            },
            &payload,
        )?;
        self.run_job_operation(
            operation_id,
            job,
            |event, descriptors| match event {
                WorkerEvent::Completed {
                    operation_id: received,
                    result,
                    log,
                } if received == operation_id => {
                    let bytes = result.read_from(descriptors).map_err(protocol_failure)?;
                    let generation = serde_json::from_slice(&bytes).map_err(|error| {
                        runtime_failure(format!("invalid worker generation result: {error}"))
                    })?;
                    let mut text = String::new();
                    append_optional_log(&mut text, log, descriptors)?;
                    Ok(Some(RuntimeOperation::new(generation, text)))
                }
                WorkerEvent::Failed {
                    operation_id: received,
                    failure,
                    log,
                } if received == operation_id => Err(worker_failure(failure, log, descriptors)),
                _ => Ok(None),
            },
            true,
            true,
        )
    }

    fn clear_context(
        &mut self,
        model: &mut Self::Model,
        context: &mut Self::Context,
    ) -> Result<RuntimeOperation<()>, RuntimeFailure> {
        if model.resource_id != context.model_resource_id {
            return Err(runtime_failure("remote context belongs to another model"));
        }
        let operation_id = self.allocate_operation_id()?;
        let context_id = context.resource_id;
        self.send(HostCommand::ClearContext {
            operation_id,
            context_resource_id: context_id,
        })?;
        self.run_simple_operation(operation_id, |event, descriptors| match event {
            WorkerEvent::ContextCleared {
                operation_id: received,
                context_resource_id,
                log,
            } if received == operation_id && context_resource_id == context_id => {
                let mut text = String::new();
                append_optional_log(&mut text, log, descriptors)?;
                Ok(Some(RuntimeOperation::new((), text)))
            }
            WorkerEvent::Failed {
                operation_id: received,
                failure,
                log,
            } if received == operation_id => Err(worker_failure(failure, log, descriptors)),
            _ => Ok(None),
        })
    }

    fn release_context(
        &mut self,
        model: &mut Self::Model,
        context: &mut Self::Context,
    ) -> Result<RuntimeOperation<()>, RuntimeFailure> {
        if model.resource_id != context.model_resource_id {
            return Err(runtime_failure("remote context belongs to another model"));
        }
        let operation_id = self.allocate_operation_id()?;
        let context_id = context.resource_id;
        self.send(HostCommand::ReleaseContext {
            operation_id,
            context_resource_id: context_id,
        })?;
        let result = self.run_simple_operation(operation_id, |event, descriptors| match event {
            WorkerEvent::ContextReleased {
                operation_id: received,
                context_resource_id,
            } if received == operation_id && context_resource_id == context_id => {
                descriptors.ensure_empty().map_err(protocol_failure)?;
                Ok(Some(RuntimeOperation::without_log(())))
            }
            WorkerEvent::Failed {
                operation_id: received,
                failure,
                log,
            } if received == operation_id => Err(worker_failure(failure, log, descriptors)),
            _ => Ok(None),
        })?;
        self.live_contexts = self.live_contexts.saturating_sub(1);
        if let Err(error) = self.release_context_admission(&context.key_digest) {
            return Err(self.record_session_failure(
                WorkerFailureKind::ProtocolViolation,
                format!(
                    "context release was acknowledged but host admission reconciliation failed: {error}"
                ),
            ));
        }
        self.release_all_device_authority_if_idle()?;
        Ok(result)
    }

    fn release_model(
        &mut self,
        model: &mut Self::Model,
    ) -> Result<RuntimeOperation<()>, RuntimeFailure> {
        let operation_id = self.allocate_operation_id()?;
        let model_id = model.resource_id;
        self.send(HostCommand::ReleaseModel {
            operation_id,
            model_resource_id: model_id,
        })?;
        let result = self.run_simple_operation(operation_id, |event, descriptors| match event {
            WorkerEvent::ModelReleased {
                operation_id: received,
                model_resource_id,
            } if received == operation_id && model_resource_id == model_id => {
                descriptors.ensure_empty().map_err(protocol_failure)?;
                Ok(Some(RuntimeOperation::without_log(())))
            }
            WorkerEvent::Failed {
                operation_id: received,
                failure,
                log,
            } if received == operation_id => Err(worker_failure(failure, log, descriptors)),
            _ => Ok(None),
        })?;
        self.live_models = self.live_models.saturating_sub(1);
        if let Err(error) = self.release_model_admission(&model.key_digest) {
            return Err(self.record_session_failure(
                WorkerFailureKind::ProtocolViolation,
                format!(
                    "model release was acknowledged but host admission reconciliation failed: {error}"
                ),
            ));
        }
        self.release_all_device_authority_if_idle()?;
        Ok(result)
    }
}

impl WorkerModelRuntime {
    fn run_simple_operation<T>(
        &mut self,
        operation_id: OperationId,
        terminal: impl Fn(
            WorkerEvent,
            &mut DescriptorSet,
        ) -> Result<Option<RuntimeOperation<T>>, RuntimeFailure>,
    ) -> Result<RuntimeOperation<T>, RuntimeFailure> {
        let deadline = deadline_after(self.options.operation_timeout);
        loop {
            let (event, mut descriptors) = self.receive_until(deadline)?;
            match event {
                WorkerEvent::Log {
                    operation_id: received,
                    ..
                } if received.is_none() || received == Some(operation_id) => {
                    if let Err(error) = descriptors.ensure_empty() {
                        return Err(self.protocol_loss(format!(
                            "worker log carried unexpected descriptors: {error}"
                        )));
                    }
                }
                event => {
                    let terminal = match terminal(event, &mut descriptors) {
                        Ok(terminal) => terminal,
                        Err(error) => {
                            if error.is_backend_lost() {
                                let kind = runtime_worker_failure_kind(&error);
                                return Err(
                                    self.record_session_failure(kind, error.message().to_string())
                                );
                            }
                            return Err(error);
                        }
                    };
                    if let Some(operation) = terminal {
                        if let Err(error) = descriptors.ensure_empty() {
                            return Err(self.protocol_loss(format!(
                                "worker terminal event carried unexpected descriptors: {error}"
                            )));
                        }
                        return Ok(operation);
                    }
                    return Err(self
                        .protocol_loss("inference worker emitted an event for another operation"));
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InventorySessionMode {
    Ephemeral,
    Resident,
}

fn validate_inventory_resource_counts(
    host_models: usize,
    host_contexts: usize,
    worker_models: usize,
    worker_contexts: usize,
) -> Result<(), String> {
    if host_models != worker_models || host_contexts != worker_contexts {
        return Err(format!(
            "worker inventory resource counts drifted from host state: \
             host models={host_models} contexts={host_contexts}, \
             worker models={worker_models} contexts={worker_contexts}"
        ));
    }
    Ok(())
}

fn inventory_session_mode(
    has_session: bool,
    live_models: usize,
    live_contexts: usize,
    pending_admissions: usize,
    active_admissions: usize,
) -> Result<InventorySessionMode, RuntimeFailure> {
    let has_resident_generation =
        live_models != 0 || live_contexts != 0 || pending_admissions != 0 || active_admissions != 0;
    match (has_session, has_resident_generation) {
        (true, true) => Ok(InventorySessionMode::Resident),
        (false, false) | (true, false) => Ok(InventorySessionMode::Ephemeral),
        (false, true) => Err(runtime_failure(
            "resident inference resources have no worker session for device inventory",
        )),
    }
}

struct WorkerSession {
    process: WorkerProcess,
    configuration: SandboxConfiguration,
    identity: WorkerGenerationIdentity,
    physical_device_id: Option<String>,
    active_attempt: Option<(AttemptId, ActiveAttemptIdentity)>,
    attempts: BTreeMap<AttemptId, AttemptStream>,
}

impl std::fmt::Debug for WorkerSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerSession")
            .field("process", &self.process)
            .field("configuration", &self.configuration)
            .field("identity", &self.identity)
            .field("physical_device_id", &self.physical_device_id)
            .field("active_attempt", &self.active_attempt)
            .field("attempts", &self.attempts.len())
            .finish()
    }
}

struct AttemptStream {
    stages: InferenceStageValidator,
    next_delta_sequence: u64,
}

impl AttemptStream {
    fn new(attempt_id: AttemptId) -> Self {
        Self {
            stages: InferenceStageValidator::worker(attempt_id),
            next_delta_sequence: 1,
        }
    }
}

fn worker_failure(
    failure: WorkerFailure,
    log: Option<crate::worker_protocol::SealedPayload>,
    descriptors: &mut DescriptorSet,
) -> RuntimeFailure {
    let mut text = String::new();
    if let Err(error) = append_optional_log(&mut text, log, descriptors) {
        return error;
    }
    match failure.code() {
        WorkerFailureCode::DeviceLost => RuntimeFailure::reaped_resource_generation(
            WorkerFailureKind::DeviceLost.code(),
            format!("inference accelerator was lost: {}", failure.message()),
            text,
        ),
        WorkerFailureCode::Cancelled => RuntimeFailure::new("inference cancelled", text),
        WorkerFailureCode::DeadlineExceeded => {
            RuntimeFailure::new("inference deadline exceeded", text)
        }
        _ => RuntimeFailure::new(failure.message(), text),
    }
}

fn append_optional_log(
    destination: &mut String,
    payload: Option<crate::worker_protocol::SealedPayload>,
    descriptors: &mut DescriptorSet,
) -> Result<(), RuntimeFailure> {
    if let Some(payload) = payload {
        let bytes = payload.read_from(descriptors).map_err(protocol_failure)?;
        let text = String::from_utf8(bytes)
            .map_err(|_| runtime_failure("worker log payload is not UTF-8"))?;
        append_bounded(destination, &text);
    }
    Ok(())
}

fn append_structured_log(destination: &mut String, record: &WorkerLogRecord) {
    let mut line = format!("worker_event = {}", record.code());
    for field in record.fields() {
        line.push(' ');
        line.push_str(field.key());
        line.push('=');
        line.push_str(field.value());
    }
    line.push('\n');
    append_bounded(destination, &line);
}

fn append_bounded(destination: &mut String, value: &str) {
    let remaining = MAX_COLLECTED_WORKER_LOG_BYTES.saturating_sub(destination.len());
    if remaining == 0 {
        return;
    }
    let mut end = value.len().min(remaining);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    destination.push_str(&value[..end]);
}

fn validate_receipt(receipt: &AllocationReceipt) -> Result<(), RuntimeFailure> {
    receipt.total_bytes().map_err(protocol_failure)?;
    Ok(())
}

fn validate_unique_paths(label: &str, paths: &[PathBuf]) -> Result<(), RuntimeFailure> {
    let mut unique = BTreeSet::new();
    for path in paths {
        validate_absolute_path(label, path)?;
        if !unique.insert(path) {
            return Err(runtime_failure(format!(
                "duplicate {label} in inference worker configuration: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn validate_absolute_path(label: &str, path: &Path) -> Result<(), RuntimeFailure> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(runtime_failure(format!(
            "{label} must be an exact absolute path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_bounded_identity(label: &str, value: &str) -> Result<(), RuntimeFailure> {
    if value.is_empty()
        || value.len() > 256
        || !value.is_ascii()
        || value.bytes().any(|byte| !byte.is_ascii_graphic())
    {
        return Err(runtime_failure(format!(
            "{label} must be 1..=256 printable non-whitespace ASCII bytes"
        )));
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
enum DeviceSnapshotMappingError {
    WorkerProtocol(String),
    HostObservation(String),
}

fn map_worker_device_snapshot(
    snapshot: &DeviceSnapshot,
    admitted_devices: &BTreeMap<String, crate::worker_resources::RenderDeviceResource>,
) -> Result<Vec<InferenceDeviceInfo>, DeviceSnapshotMappingError> {
    let mut backend_names = BTreeSet::new();
    let mut result = Vec::with_capacity(snapshot.devices().len());
    for device in snapshot.devices() {
        if !backend_names.insert(device.backend_name()) {
            return Err(DeviceSnapshotMappingError::WorkerProtocol(
                "worker inventory contains duplicate backend selectors".to_string(),
            ));
        }
        let kind = match device.kind() {
            DeviceKind::Cpu => InferenceDeviceKind::Cpu,
            DeviceKind::DiscreteGpu => InferenceDeviceKind::DiscreteGpu,
            DeviceKind::IntegratedGpu => InferenceDeviceKind::IntegratedGpu,
            DeviceKind::Accelerator => InferenceDeviceKind::Accelerator,
            DeviceKind::Metadata => InferenceDeviceKind::Metadata,
            DeviceKind::Unknown => InferenceDeviceKind::Unknown,
        };
        let gpu = matches!(
            device.kind(),
            DeviceKind::DiscreteGpu | DeviceKind::IntegratedGpu
        );
        let (
            pci_device_id,
            pci_subsystem_id,
            driver_build_id,
            free_memory_bytes,
            total_memory_bytes,
            usable,
            supports_gpu_offload,
        ) = if gpu {
            let resource = admitted_devices.get(device.device_id()).ok_or_else(|| {
                DeviceSnapshotMappingError::WorkerProtocol(format!(
                    "worker reported unadmitted physical GPU identity {}",
                    device.device_id()
                ))
            })?;
            let observation = resource
                .memory_observation()
                .map_err(|error| {
                    DeviceSnapshotMappingError::HostObservation(format!(
                        "failed to read admitted GPU memory: {error}"
                    ))
                })?
                .ok_or_else(|| {
                    DeviceSnapshotMappingError::HostObservation(format!(
                        "physical GPU {} has no kernel-owned VRAM counters",
                        device.device_id()
                    ))
                })?;
            (
                resource.pci_device_id().map(str::to_owned),
                resource.pci_subsystem_id().map(str::to_owned),
                resource.driver_build_id().to_string(),
                observation.available_bytes,
                observation.total_bytes,
                device.usable(),
                device.supports_gpu_offload(),
            )
        } else {
            if device.supports_gpu_offload() {
                return Err(DeviceSnapshotMappingError::WorkerProtocol(
                    "non-GPU worker inventory entry claims GPU offload authority".to_string(),
                ));
            }
            (
                None,
                None,
                device.driver_build_id().to_string(),
                device.free_memory_bytes(),
                device.total_memory_bytes(),
                device.usable(),
                false,
            )
        };
        result.push(InferenceDeviceInfo {
            physical_device_id: device.device_id().to_string(),
            pci_device_id,
            pci_subsystem_id,
            driver_build_id,
            backend_name: device.backend_name().to_string(),
            description: device.description().to_string(),
            kind,
            free_memory_bytes,
            total_memory_bytes,
            usable,
            supports_gpu_offload,
        });
    }
    Ok(result)
}

fn prepare_private_root(label: &str, path: &Path) -> Result<(), RuntimeFailure> {
    validate_absolute_path(label, path)?;
    ensure_private_directory(path)
        .map_err(|error| runtime_failure(format!("failed to prepare {label}: {error}")))
}

fn production_authority_roots() -> Result<(PathBuf, PathBuf), RuntimeFailure> {
    let uid = unsafe { libc::geteuid() };
    let (user_runtime_root, device_lease_root, health_root) = authority_roots_for_uid(uid);
    prepare_private_root("per-UID kernel runtime root", &user_runtime_root)?;
    let inference_root = device_lease_root
        .parent()
        .expect("authority roots always have an inference parent");
    prepare_private_root("global inference authority root", inference_root)?;
    Ok((device_lease_root, health_root))
}

fn authority_roots_for_uid(uid: u32) -> (PathBuf, PathBuf, PathBuf) {
    let user_runtime_root = PathBuf::from(GLOBAL_RUNTIME_PARENT).join(uid.to_string());
    let inference_root = user_runtime_root.join("agentLIBRE/inference");
    (
        user_runtime_root,
        inference_root.join("device-leases"),
        inference_root.join("health"),
    )
}

fn path_string(path: &Path) -> Result<String, RuntimeFailure> {
    validate_absolute_path("inference sandbox path", path)?;
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| runtime_failure("inference sandbox paths must be UTF-8"))
}

fn deadline_after(duration: Duration) -> Instant {
    Instant::now()
        .checked_add(duration)
        .unwrap_or_else(Instant::now)
}

fn default_circuit_breaker_policy() -> WorkerCircuitBreakerPolicy {
    WorkerCircuitBreakerPolicy::new(
        DEFAULT_INITIAL_COOLDOWN_MS,
        DEFAULT_MAXIMUM_COOLDOWN_MS,
        DEFAULT_MAXIMUM_CRASH_STREAK,
    )
    .expect("default inference worker circuit-breaker policy is valid")
}

fn typed_worker_loss(
    kind: WorkerFailureKind,
    message: impl Into<String>,
    log: impl Into<String>,
) -> RuntimeFailure {
    RuntimeFailure::reaped_resource_generation(kind.code(), message, log)
}

fn worker_loss_log(summary: &str, private_stderr_evidence: &str) -> String {
    let mut log = String::new();
    append_bounded(&mut log, summary);
    if !private_stderr_evidence.is_empty() {
        if !log.is_empty() {
            append_bounded(&mut log, "\n");
        }
        append_bounded(&mut log, private_stderr_evidence);
    }
    log
}

fn supervisor_identity_failure(error: impl std::fmt::Display) -> RuntimeFailure {
    RuntimeFailure::resource_admission(
        "inference_worker_identity_invalid",
        format!("invalid inference worker supervision identity: {error}"),
        "",
    )
}

fn supervisor_failure(error: crate::worker_supervisor::WorkerSupervisorError) -> RuntimeFailure {
    RuntimeFailure::resource_admission(error.code(), error.to_string(), "")
}

fn start_failure_kind(error: &WorkerProtocolError) -> WorkerFailureKind {
    match error.code() {
        WorkerProtocolErrorCode::SpawnFailed
        | WorkerProtocolErrorCode::WorkerUnavailable
        | WorkerProtocolErrorCode::WorkerUntrusted => WorkerFailureKind::SpawnFailed,
        WorkerProtocolErrorCode::TimedOut => WorkerFailureKind::StartupTimedOut,
        WorkerProtocolErrorCode::IdentityMismatch => WorkerFailureKind::HandshakeFailed,
        WorkerProtocolErrorCode::PeerClosed | WorkerProtocolErrorCode::Io => {
            WorkerFailureKind::HandshakeFailed
        }
        WorkerProtocolErrorCode::FrameTooLarge
        | WorkerProtocolErrorCode::DescriptorLimit
        | WorkerProtocolErrorCode::MalformedFrame
        | WorkerProtocolErrorCode::SequenceViolation
        | WorkerProtocolErrorCode::UnexpectedDescriptors
        | WorkerProtocolErrorCode::UnexpectedMessage
        | WorkerProtocolErrorCode::PayloadTooLarge
        | WorkerProtocolErrorCode::InvalidPayload => WorkerFailureKind::ProtocolViolation,
    }
}

fn transport_failure_kind(
    process: &mut WorkerProcess,
    error: &WorkerProtocolError,
) -> WorkerFailureKind {
    for _ in 0..5 {
        match process.try_wait() {
            Ok(Some(status)) => {
                return exited_worker_failure_kind(status);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(_) => break,
        }
    }
    match error.code() {
        WorkerProtocolErrorCode::PeerClosed | WorkerProtocolErrorCode::Io => {
            WorkerFailureKind::Exited
        }
        _ => WorkerFailureKind::ProtocolViolation,
    }
}

fn exited_worker_failure_kind(status: std::process::ExitStatus) -> WorkerFailureKind {
    if status.code() == Some(WORKER_DEVICE_LOST_EXIT_STATUS) {
        WorkerFailureKind::DeviceLost
    } else if status.signal().is_some() {
        WorkerFailureKind::Signaled
    } else {
        WorkerFailureKind::Exited
    }
}

fn runtime_worker_failure_kind(error: &RuntimeFailure) -> WorkerFailureKind {
    match error.code() {
        "inference_device_lost" => WorkerFailureKind::DeviceLost,
        "inference_worker_spawn_failed" => WorkerFailureKind::SpawnFailed,
        "inference_worker_handshake_failed" => WorkerFailureKind::HandshakeFailed,
        "inference_worker_startup_timed_out" => WorkerFailureKind::StartupTimedOut,
        "inference_worker_exited" => WorkerFailureKind::Exited,
        "inference_worker_signaled" => WorkerFailureKind::Signaled,
        "inference_worker_forced_after_cancel" => WorkerFailureKind::ForcedAfterCancellation,
        "inference_worker_forced_after_deadline" => WorkerFailureKind::ForcedAfterDeadline,
        "inference_worker_reap_timed_out" => WorkerFailureKind::ReapTimedOut,
        _ => WorkerFailureKind::ProtocolViolation,
    }
}

fn unix_time_millis() -> Result<u64, RuntimeFailure> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| {
        RuntimeFailure::resource_admission(
            "device_snapshot_invalid",
            "system clock is before the Unix epoch",
            "",
        )
    })?;
    u64::try_from(elapsed.as_millis()).map_err(|_| {
        RuntimeFailure::resource_admission(
            "device_snapshot_invalid",
            "system clock millisecond value overflowed",
            "",
        )
    })
}

fn resource_ledger_failure(error: ReservationLedgerError) -> RuntimeFailure {
    RuntimeFailure::resource_admission(error.code(), error.to_string(), "")
}

fn durable_health_failure(error: crate::durable_health::DurableHealthStoreError) -> RuntimeFailure {
    RuntimeFailure::resource_admission(error.code(), error.to_string(), "")
}

fn resource_quarantine_key(
    job: &InferenceJob,
    resource: &crate::worker_resources::RenderDeviceResource,
    worker_generation_id: &str,
) -> Result<ResourceQuarantineKey, RuntimeFailure> {
    let config = serde_json::to_vec(job.config()).map_err(|error| {
        RuntimeFailure::resource_admission(
            "resource_quarantine_identity_invalid",
            format!("failed to normalize GPU profile identity: {error}"),
            "",
        )
    })?;
    ResourceQuarantineKey::new(
        job.model_key().digest(),
        sha256_hex(&config),
        sha256_hex(resource.physical_device_id().as_bytes()),
        sha256_hex(resource.driver_build_id().as_bytes()),
        sha256_hex(worker_generation_id.as_bytes()),
    )
    .map_err(|error| RuntimeFailure::resource_admission(error.code(), error.to_string(), ""))
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut value = String::with_capacity(digest.len() * 2);
    for byte in digest {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    value
}

fn runtime_failure(message: impl Into<String>) -> RuntimeFailure {
    RuntimeFailure::new(message, "")
}

fn composite_worker_build_id(native_bundle_id: &str) -> String {
    format!("{WORKER_BUILD_ID}+{native_bundle_id}")
}

fn protocol_failure(error: impl std::fmt::Display) -> RuntimeFailure {
    RuntimeFailure::backend_lost(
        format!("inference worker protocol failed: {error}"),
        "worker protocol generation is unusable",
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::Duration;

    use super::*;

    static TEST_ROOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_root(label: &str) -> PathBuf {
        PathBuf::from(format!(
            "/tmp/agl-worker-runtime-{label}-{}-{}",
            std::process::id(),
            TEST_ROOT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn digest(byte: u8) -> String {
        format!("{byte:02x}").repeat(32)
    }

    fn hostile_release_fixture(
        label: &str,
    ) -> (
        WorkerModelRuntime,
        crate::worker_protocol::WorkerControlChannel,
    ) {
        let root = test_root(label);
        let options = WorkerRuntimeOptions::new(root.join("worker-tmp")).with_timeouts(
            Duration::from_millis(20),
            Duration::from_millis(20),
            Duration::from_millis(20),
            Duration::from_millis(20),
        );
        let mut runtime = WorkerModelRuntime::new(options).unwrap();
        seed_device_generation(&mut runtime, "fixture-device", label, 9);
        let (host, worker) = crate::worker_protocol::control_channel_pair().unwrap();
        runtime.session = Some(WorkerSession {
            process: WorkerProcess::test_fixture(host),
            configuration: SandboxConfiguration::new(
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                "/tmp/agl-worker-fixture",
            )
            .unwrap(),
            identity: WorkerGenerationIdentity::new(1234, 1, "fixture-worker").unwrap(),
            physical_device_id: None,
            active_attempt: None,
            attempts: BTreeMap::new(),
        });
        runtime.live_models = 1;
        runtime.live_contexts = 1;
        (runtime, worker)
    }

    fn fixture_remote_resources() -> (RemoteModel, RemoteContext) {
        let model_id = ModelResourceId::new(41).unwrap();
        (
            RemoteModel {
                resource_id: model_id,
                key_digest: digest(7),
            },
            RemoteContext {
                resource_id: ContextResourceId::new(42).unwrap(),
                model_resource_id: model_id,
                key_digest: digest(8),
            },
        )
    }

    fn assert_hostile_release_reaped(runtime: &mut WorkerModelRuntime) {
        assert!(
            runtime.session.is_none(),
            "host must discard the hostile session"
        );
        assert_eq!(runtime.live_models, 0);
        assert_eq!(runtime.live_contexts, 0);
        assert!(runtime.pending_admissions.is_empty());
        assert!(runtime.active_admissions.is_empty());
        for admission in runtime.admissions.values_mut() {
            let status = admission.ledger.status().unwrap();
            assert_eq!(status.pending_reservations, 0);
            assert_eq!(status.active_reservations, 0);
            assert_eq!(
                status.resident,
                crate::admission::ResidentReservations::default()
            );
        }
        assert_eq!(runtime.status_handle().snapshot().reserved_bytes(), 0);
    }

    #[test]
    fn hostile_release_events_fail_closed_and_reconcile_host_generation() {
        enum HostileEvent {
            WrongOperation,
            WrongResource,
            Unsolicited,
        }

        for hostile in [
            HostileEvent::WrongOperation,
            HostileEvent::WrongResource,
            HostileEvent::Unsolicited,
        ] {
            let label = match hostile {
                HostileEvent::WrongOperation => "release-wrong-operation",
                HostileEvent::WrongResource => "release-wrong-resource",
                HostileEvent::Unsolicited => "release-unsolicited",
            };
            let (mut runtime, mut worker) = hostile_release_fixture(label);
            let (mut model, mut context) = fixture_remote_resources();
            let sender = thread::spawn(move || {
                let command = worker.receive().unwrap();
                let HostCommand::ReleaseContext { operation_id, .. } = command else {
                    panic!("host must send release-context");
                };
                let event = match hostile {
                    HostileEvent::WrongOperation => WorkerEvent::ModelReleased {
                        operation_id,
                        model_resource_id: ModelResourceId::new(41).unwrap(),
                    },
                    HostileEvent::WrongResource => WorkerEvent::ContextReleased {
                        operation_id,
                        context_resource_id: ContextResourceId::new(99).unwrap(),
                    },
                    HostileEvent::Unsolicited => WorkerEvent::ContextReleased {
                        operation_id: OperationId::new(operation_id.get() + 1).unwrap(),
                        context_resource_id: ContextResourceId::new(42).unwrap(),
                    },
                };
                worker.send(event).unwrap();
            });

            let failure = runtime
                .release_context(&mut model, &mut context)
                .unwrap_err();
            sender.join().unwrap();
            assert_eq!(failure.code(), "inference_worker_protocol_violation");
            assert!(failure.is_backend_lost());
            assert_hostile_release_reaped(&mut runtime);
        }
    }

    #[test]
    fn missing_release_acknowledgement_times_out_and_reaps_host_generation() {
        let (mut runtime, mut worker) = hostile_release_fixture("release-missing-ack");
        let (mut model, mut context) = fixture_remote_resources();
        let holder = thread::spawn(move || {
            assert!(matches!(
                worker.receive().unwrap(),
                HostCommand::ReleaseContext { .. }
            ));
            thread::sleep(Duration::from_millis(80));
        });

        let failure = runtime
            .release_context(&mut model, &mut context)
            .unwrap_err();
        holder.join().unwrap();
        assert_eq!(failure.code(), "inference_worker_forced_after_deadline");
        assert!(failure.is_backend_lost());
        assert_hostile_release_reaped(&mut runtime);
    }

    #[test]
    fn duplicate_release_acknowledgement_is_contained_by_the_next_release() {
        let (mut runtime, mut worker) = hostile_release_fixture("release-duplicate-post-ack");
        let (mut model, mut context) = fixture_remote_resources();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let sender = thread::spawn(move || {
            let command = worker.receive().unwrap();
            let HostCommand::ReleaseContext { operation_id, .. } = command else {
                panic!("host must send release-context");
            };
            let acknowledgement = WorkerEvent::ContextReleased {
                operation_id,
                context_resource_id: ContextResourceId::new(42).unwrap(),
            };
            worker.send(acknowledgement.clone()).unwrap();
            worker.send(acknowledgement).unwrap();
            release_receiver.recv().unwrap();
        });

        runtime.release_context(&mut model, &mut context).unwrap();
        let failure = runtime.release_model(&mut model).unwrap_err();
        release_sender.send(()).unwrap();
        sender.join().unwrap();
        assert_eq!(failure.code(), "inference_worker_protocol_violation");
        assert!(failure.is_backend_lost());
        assert_hostile_release_reaped(&mut runtime);
    }

    #[test]
    fn worker_exit_before_or_after_release_acknowledgement_is_contained() {
        for acknowledged in [false, true] {
            let label = if acknowledged {
                "release-exit-after-ack"
            } else {
                "release-exit-before-ack"
            };
            let (mut runtime, mut worker) = hostile_release_fixture(label);
            let (mut model, mut context) = fixture_remote_resources();
            let sender = thread::spawn(move || {
                let command = worker.receive().unwrap();
                let HostCommand::ReleaseContext { operation_id, .. } = command else {
                    panic!("host must send release-context");
                };
                if acknowledged {
                    worker
                        .send(WorkerEvent::ContextReleased {
                            operation_id,
                            context_resource_id: ContextResourceId::new(42).unwrap(),
                        })
                        .unwrap();
                }
            });

            if acknowledged {
                runtime.release_context(&mut model, &mut context).unwrap();
                let failure = runtime.release_model(&mut model).unwrap_err();
                assert_eq!(failure.code(), "inference_worker_exited");
                assert!(failure.is_backend_lost());
            } else {
                let failure = runtime
                    .release_context(&mut model, &mut context)
                    .unwrap_err();
                assert_eq!(failure.code(), "inference_worker_exited");
                assert!(failure.is_backend_lost());
            }
            sender.join().unwrap();
            assert_hostile_release_reaped(&mut runtime);
        }
    }

    #[test]
    fn inventory_reuses_a_resident_session_and_never_fabricates_one() {
        assert_eq!(
            inventory_session_mode(true, 1, 1, 0, 0).unwrap(),
            InventorySessionMode::Resident
        );
        assert_eq!(
            inventory_session_mode(true, 0, 0, 1, 0).unwrap(),
            InventorySessionMode::Resident
        );
        assert_eq!(
            inventory_session_mode(true, 0, 0, 0, 1).unwrap(),
            InventorySessionMode::Resident
        );
        assert_eq!(
            inventory_session_mode(false, 0, 0, 0, 0).unwrap(),
            InventorySessionMode::Ephemeral
        );
        assert_eq!(
            inventory_session_mode(true, 0, 0, 0, 0).unwrap(),
            InventorySessionMode::Ephemeral
        );

        let missing = inventory_session_mode(false, 1, 0, 0, 0).unwrap_err();
        assert_eq!(missing.code(), "runtime_failure");
        assert!(
            missing
                .message()
                .contains("resident inference resources have no worker session")
        );
    }

    #[test]
    fn inventory_resource_counts_must_match_host_owned_state() {
        validate_inventory_resource_counts(0, 0, 0, 0).unwrap();
        validate_inventory_resource_counts(2, 3, 2, 3).unwrap();

        let model_drift = validate_inventory_resource_counts(1, 1, 0, 1).unwrap_err();
        assert!(model_drift.contains("host models=1 contexts=1"));
        assert!(model_drift.contains("worker models=0 contexts=1"));

        let context_drift = validate_inventory_resource_counts(1, 1, 1, 0).unwrap_err();
        assert!(context_drift.contains("worker models=1 contexts=0"));
    }

    #[test]
    fn host_vram_observation_failure_is_not_a_worker_protocol_violation() {
        let root = test_root("inventory-host-observation");
        let render_node = PathBuf::from("/dev/dri/renderD128");
        let physical_device_id = "pci:0000:03:00.0";
        let resource = crate::worker_resources::RenderDeviceResource::fixture(
            &render_node,
            physical_device_id,
            "radv:test-driver",
        );
        let admitted_devices = BTreeMap::from([(physical_device_id.to_string(), resource.clone())]);
        let snapshot = DeviceSnapshot::new(vec![
            crate::worker_protocol::DeviceSnapshotEntry::new(
                physical_device_id,
                "worker-driver",
                "Vulkan0",
                "fixture GPU",
                DeviceKind::DiscreteGpu,
                1,
                2,
                true,
                true,
            )
            .unwrap(),
        ])
        .unwrap();

        let mapping = map_worker_device_snapshot(&snapshot, &admitted_devices).unwrap_err();
        assert!(matches!(
            mapping,
            DeviceSnapshotMappingError::HostObservation(ref message)
                if message.contains("has no kernel-owned VRAM counters")
        ));

        let mut runtime = WorkerModelRuntime::new(
            WorkerRuntimeOptions::new(root.join("worker-tmp"))
                .with_gpu_device_paths(vec![render_node])
                .with_admitted_devices(admitted_devices)
                .with_device_lease_root(root.join("device-leases"))
                .with_health_root(root.join("health")),
        )
        .unwrap();
        let error = runtime.map_device_snapshot(&snapshot).unwrap_err();
        assert_eq!(error.code(), "device_snapshot_invalid");
        assert!(!error.is_backend_lost());
        assert!(runtime.session.is_none());
        assert!(runtime.device_leases.is_empty());

        let unadmitted = map_worker_device_snapshot(&snapshot, &BTreeMap::new()).unwrap_err();
        assert!(matches!(
            unadmitted,
            DeviceSnapshotMappingError::WorkerProtocol(ref message)
                if message.contains("unadmitted physical GPU identity")
        ));

        drop(runtime);
        let _ = fs::remove_dir_all(root);
    }

    fn quarantine_key(device: u8, config: u8) -> ResourceQuarantineKey {
        ResourceQuarantineKey::new(
            digest(1),
            digest(config),
            digest(device),
            digest(4),
            digest(5),
        )
        .unwrap()
    }

    fn seed_device_generation(
        runtime: &mut WorkerModelRuntime,
        physical_device_id: &str,
        suffix: &str,
        quarantine_device: u8,
    ) {
        let total = crate::admission::mib(128).unwrap();
        let envelope = DeviceMemoryEnvelope {
            physical_device_id: physical_device_id.to_string(),
            minimum_total_bytes: total,
            maximum_total_bytes: total,
        };
        let snapshot = crate::admission::validate_device_snapshot(
            DeviceMemorySnapshot {
                physical_device_id: physical_device_id.to_string(),
                driver_id: format!("driver-{suffix}"),
                total_bytes: total,
                available_bytes: total,
                observed_at_unix_ms: 10_000,
            },
            &envelope,
            SnapshotPolicy::default(),
            10_000,
        )
        .unwrap();
        let mut ledger = ReservationLedger::new(
            physical_device_id,
            AdmissionPolicy {
                reserve_bytes: crate::admission::mib(2).unwrap(),
            },
        )
        .unwrap();
        let active_estimate = crate::admission::AllocationEstimate {
            model_bytes: crate::admission::mib(8).unwrap(),
            context_bytes: crate::admission::mib(12).unwrap(),
            transient_bytes: crate::admission::mib(2).unwrap(),
            uncertainty_bytes: crate::admission::mib(1).unwrap(),
        };
        let active = ledger
            .reserve(
                &snapshot,
                ReservationRequest {
                    model_key: format!("model-active-{suffix}"),
                    context_key: format!("context-active-{suffix}"),
                    estimate: active_estimate,
                },
            )
            .unwrap();
        ledger
            .commit(
                active.token,
                HostAllocationReceipt {
                    model_bytes: crate::admission::mib(8).unwrap(),
                    context_bytes: crate::admission::mib(12).unwrap(),
                    transient_bytes: crate::admission::mib(2).unwrap(),
                },
            )
            .unwrap();
        let pending_estimate = crate::admission::AllocationEstimate {
            model_bytes: crate::admission::mib(4).unwrap(),
            context_bytes: crate::admission::mib(6).unwrap(),
            transient_bytes: crate::admission::mib(1).unwrap(),
            uncertainty_bytes: 0,
        };
        let pending = ledger
            .reserve(
                &snapshot,
                ReservationRequest {
                    model_key: format!("model-pending-{suffix}"),
                    context_key: format!("context-pending-{suffix}"),
                    estimate: pending_estimate,
                },
            )
            .unwrap();
        runtime.admissions.insert(
            physical_device_id.to_string(),
            DeviceAdmission { envelope, ledger },
        );
        runtime.active_admissions.insert(
            format!("context-active-{suffix}"),
            ActiveAdmission {
                physical_device_id: physical_device_id.to_string(),
                token: active.token,
            },
        );
        runtime.pending_admissions.insert(
            format!("context-pending-{suffix}"),
            PendingAdmission {
                physical_device_id: physical_device_id.to_string(),
                token: pending.token,
                model_key: format!("model-pending-{suffix}"),
                full_estimate: pending_estimate,
                quarantine_key: quarantine_key(quarantine_device, 2),
            },
        );
    }

    fn device_supervisor(
        physical_device_id: &str,
        worker_build_id: &str,
        worker_pid: u32,
        launch_generation: u64,
    ) -> (WorkerSupervisorState, WorkerGenerationIdentity) {
        let policy = WorkerCircuitBreakerPolicy::new(25, 100, 3).unwrap();
        let key = WorkerHealthKey::new(
            physical_device_id,
            format!("driver-{physical_device_id}"),
            worker_build_id,
        )
        .unwrap();
        let mut supervisor =
            WorkerSupervisorState::restore(WorkerHealthState::new(key), policy, 10_000).unwrap();
        let worker =
            WorkerGenerationIdentity::new(worker_pid, launch_generation, worker_build_id).unwrap();
        supervisor.begin_start(worker.clone()).unwrap();
        supervisor.mark_ready(&worker).unwrap();
        (supervisor, worker)
    }

    #[test]
    fn closed_exit_status_maps_device_loss_without_stderr_parsing() {
        let device_lost =
            <std::process::ExitStatus as std::os::unix::process::ExitStatusExt>::from_raw(
                WORKER_DEVICE_LOST_EXIT_STATUS << 8,
            );
        let aborted = <std::process::ExitStatus as std::os::unix::process::ExitStatusExt>::from_raw(
            libc::SIGABRT,
        );
        let ordinary =
            <std::process::ExitStatus as std::os::unix::process::ExitStatusExt>::from_raw(7 << 8);

        assert_eq!(
            exited_worker_failure_kind(device_lost),
            WorkerFailureKind::DeviceLost
        );
        assert_eq!(
            exited_worker_failure_kind(aborted),
            WorkerFailureKind::Signaled
        );
        assert_eq!(
            exited_worker_failure_kind(ordinary),
            WorkerFailureKind::Exited
        );
    }

    #[test]
    fn redacted_stderr_observation_is_private_typed_worker_loss_log() {
        let evidence = "worker_stderr_observation bytes=4096 markers_retained=1 markers_dropped=0 read_failed=false\nobserved_non_authoritative_marker=vk_error_device_lost";
        let failure = typed_worker_loss(
            WorkerFailureKind::Signaled,
            "inference worker exited on a signal",
            worker_loss_log("exact worker generation was reaped", evidence),
        );

        assert_eq!(failure.code(), "inference_worker_signaled");
        assert!(failure.is_backend_lost());
        assert!(!failure.message().contains("vk_error_device_lost"));
        assert!(failure.log().contains(evidence));
        assert_eq!(
            runtime_worker_failure_kind(&failure),
            WorkerFailureKind::Signaled
        );
    }

    #[test]
    fn runtime_options_reject_ambient_or_relative_authority() {
        assert!(WorkerModelRuntime::new(WorkerRuntimeOptions::new("relative")).is_err());

        let mut environment = BTreeMap::new();
        environment.insert("LD_LIBRARY_PATH".to_string(), OsString::from("/host/lib"));
        let options = WorkerRuntimeOptions::new("/tmp/agl-worker").with_environment(environment);
        assert!(WorkerModelRuntime::new(options).is_err());
    }

    #[test]
    fn sandbox_paths_are_exact_and_duplicate_free() {
        let duplicate = WorkerRuntimeOptions::new("/tmp/agl-worker")
            .with_runtime_roots(vec![PathBuf::from("/runtime"), PathBuf::from("/runtime")]);
        assert!(WorkerModelRuntime::new(duplicate).is_err());

        let parent = WorkerRuntimeOptions::new("/tmp/agl-worker")
            .with_gpu_device_paths(vec![PathBuf::from("/dev/dri/../dri/renderD128")]);
        assert!(WorkerModelRuntime::new(parent).is_err());
    }

    #[test]
    fn fresh_runtime_roots_are_created_before_device_and_health_open() {
        let root = test_root("fresh-state");
        let runtime = WorkerModelRuntime::new(
            WorkerRuntimeOptions::new(root.join("home-a/worker-tmp"))
                .with_device_lease_root(root.join("global/device-leases"))
                .with_health_root(root.join("global/health")),
        )
        .unwrap();

        assert!(root.join("home-a/worker-tmp").is_dir());
        assert!(root.join("global/device-leases").is_dir());
        assert!(root.join("global/health").is_dir());
        drop(runtime);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn runtime_root_creation_never_traverses_an_intermediate_symlink() {
        let root = test_root("root-symlink");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let alias_parent = root.join("home-a");
        fs::create_dir_all(&alias_parent).unwrap();
        symlink(&outside, alias_parent.join("redirect")).unwrap();

        let error = WorkerModelRuntime::new(WorkerRuntimeOptions::new(
            alias_parent.join("redirect/worker-tmp"),
        ))
        .unwrap_err();
        assert!(error.to_string().contains("private inference temp root"));
        assert!(!outside.join("worker-tmp").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn two_agl_homes_contend_on_one_uid_global_device_authority() {
        let root = test_root("two-home-authority");
        let home_a = root.join("agl-home-a");
        let home_b = root.join("agl-home-b");
        let global_lease_root = root.join("uid-global/device-leases");
        let global_health_root = root.join("uid-global/health");
        let physical_device_id = "pci:0000:03:00.0";
        let render_node = PathBuf::from("/dev/dri/renderD128");
        let resource = crate::worker_resources::RenderDeviceResource::fixture(
            &render_node,
            physical_device_id,
            "radv:test-driver",
        );
        let admitted_devices = BTreeMap::from([(physical_device_id.to_string(), resource.clone())]);
        let runtime_options = |home: &Path| {
            WorkerRuntimeOptions::new(home.join("worker-tmp"))
                .with_gpu_device_paths(vec![render_node.clone()])
                .with_admitted_devices(admitted_devices.clone())
                .with_device_lease_root(&global_lease_root)
                .with_health_root(&global_health_root)
        };

        let mut first = WorkerModelRuntime::new(runtime_options(&home_a))
            .expect("first AGL home starts without eagerly owning a physical device");
        assert!(first.device_leases.is_empty());
        assert!(first.pending_admissions.is_empty());
        assert!(first.active_admissions.is_empty());
        assert!(first.session.is_none());

        first
            .acquire_device_authority(physical_device_id)
            .expect("first AGL home lazily owns the physical device");
        let mut second = WorkerModelRuntime::new(runtime_options(&home_b)).expect(
            "daemon runtime in a second AGL home remains available while the device is owned",
        );
        assert!(second.device_leases.is_empty());
        let error = second
            .worker_device_snapshot()
            .expect_err("loser must fail before starting its GPU inventory worker");
        assert_eq!(error.code(), "device_authority_busy");
        assert!(second.device_leases.is_empty());
        assert!(second.pending_admissions.is_empty());
        assert!(second.active_admissions.is_empty());
        assert!(second.session.is_none());

        drop(first);
        drop(second);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn production_authority_paths_depend_only_on_effective_uid() {
        let (_, leases, health) = authority_roots_for_uid(1234);
        assert_eq!(
            leases,
            PathBuf::from("/run/user/1234/agentLIBRE/inference/device-leases")
        );
        assert_eq!(
            health,
            PathBuf::from("/run/user/1234/agentLIBRE/inference/health")
        );
        assert!(!leases.to_string_lossy().contains("AGL_HOME"));
    }

    #[test]
    fn job_sandbox_gpu_allowlist_contains_only_the_selected_render_node() {
        let root = test_root("selected-render-node");
        let runtime = WorkerModelRuntime::new(WorkerRuntimeOptions::new(&root)).unwrap();
        let selected = PathBuf::from("/dev/dri/renderD129");
        let configuration = runtime
            .sandbox_configuration(Vec::new(), Vec::new(), std::slice::from_ref(&selected))
            .unwrap();

        assert_eq!(
            configuration.gpu_device_paths(),
            &["/dev/dri/renderD129".to_string()]
        );
        assert!(
            !configuration
                .gpu_device_paths()
                .iter()
                .any(|path| path == "/dev/dri/renderD128")
        );
        drop(runtime);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn start_failure_persists_cooldown_and_status_without_fabricating_pid() {
        let root = test_root("durable-start-failure");
        let policy = WorkerCircuitBreakerPolicy::new(25, 100, 3).unwrap();
        let health_root = root.join("health");
        let mut runtime = WorkerModelRuntime::new(
            WorkerRuntimeOptions::new(root.join("worker-tmp"))
                .with_health_root(&health_root)
                .with_circuit_breaker_policy(policy),
        )
        .unwrap();
        let key =
            WorkerHealthKey::new("pci:0000:03:00.0", "radv:test-driver", WORKER_BUILD_ID).unwrap();
        runtime.supervisors.insert(
            key.physical_device_id().to_string(),
            WorkerSupervisorState::restore(
                WorkerHealthState::new(key.clone()),
                policy,
                unix_time_millis().unwrap(),
            )
            .unwrap(),
        );

        let failure = runtime.record_start_failure(
            key.physical_device_id(),
            WorkerFailureKind::SpawnFailed,
            "injected hostile worker start failure",
        );
        assert_eq!(failure.code(), "inference_worker_spawn_failed");
        let status = runtime.status_handle().snapshot();
        assert_eq!(status.phase(), WorkerLifecyclePhase::CoolingDown);
        assert_eq!(status.worker_pid(), None);
        assert_eq!(status.launch_generation(), None);
        assert_eq!(status.physical_device_id(), Some(key.physical_device_id()));
        let not_before = status.cooldown_not_before_unix_ms().unwrap();
        drop(runtime);

        let store = DurableHealthStore::open(&health_root).unwrap();
        let health = store.load_worker_health(&key, policy).unwrap().unwrap();
        assert_eq!(health.crash_streak(), 1);
        assert_eq!(health.cooldown_not_before_unix_ms(), Some(not_before));
        let restored = WorkerSupervisorState::restore(health, policy, not_before).unwrap();
        assert_eq!(restored.phase(), WorkerLifecyclePhase::Cold);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failure_before_started_reaps_pending_admission_generation() {
        let root = test_root("missing-receipt");
        let mut runtime = WorkerModelRuntime::new(WorkerRuntimeOptions::new(&root)).unwrap();
        let total = crate::admission::mib(32).unwrap();
        let envelope = DeviceMemoryEnvelope {
            physical_device_id: "pci:0000:03:00.0".to_string(),
            minimum_total_bytes: total,
            maximum_total_bytes: total,
        };
        let snapshot = crate::admission::validate_device_snapshot(
            DeviceMemorySnapshot {
                physical_device_id: envelope.physical_device_id.clone(),
                driver_id: "radv:test".to_string(),
                total_bytes: total,
                available_bytes: total,
                observed_at_unix_ms: 10_000,
            },
            &envelope,
            SnapshotPolicy::default(),
            10_000,
        )
        .unwrap();
        let mut ledger = ReservationLedger::new(
            envelope.physical_device_id.clone(),
            AdmissionPolicy { reserve_bytes: 0 },
        )
        .unwrap();
        let estimate = crate::admission::AllocationEstimate {
            model_bytes: crate::admission::mib(8).unwrap(),
            context_bytes: crate::admission::mib(12).unwrap(),
            transient_bytes: crate::admission::mib(2).unwrap(),
            uncertainty_bytes: 0,
        };
        let pending = ledger
            .reserve(
                &snapshot,
                ReservationRequest {
                    model_key: "model".to_string(),
                    context_key: "context".to_string(),
                    estimate,
                },
            )
            .unwrap();
        runtime.admissions.insert(
            envelope.physical_device_id.clone(),
            DeviceAdmission { envelope, ledger },
        );
        runtime.pending_admissions.insert(
            "context".to_string(),
            PendingAdmission {
                physical_device_id: "pci:0000:03:00.0".to_string(),
                token: pending.token,
                model_key: "model".to_string(),
                full_estimate: estimate,
                quarantine_key: ResourceQuarantineKey::new(
                    "a".repeat(64),
                    "b".repeat(64),
                    "c".repeat(64),
                    "d".repeat(64),
                    "e".repeat(64),
                )
                .unwrap(),
            },
        );

        let failure = runtime.missing_receipt_failure(&RuntimeFailure::new(
            "worker exited during allocation",
            "exit_status=86",
        ));

        assert_eq!(failure.code(), "allocation_receipt_missing");
        assert!(failure.is_backend_lost());
        assert!(runtime.pending_admissions.is_empty());
        let status = runtime
            .admissions
            .get_mut("pci:0000:03:00.0")
            .unwrap()
            .ledger
            .status()
            .unwrap();
        assert_eq!(status.pending_reservations, 0);
        assert_eq!(status.active_reservations, 0);
        assert_eq!(
            status.resident,
            crate::admission::ResidentReservations::default()
        );

        drop(runtime);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn device_loss_releases_only_the_affected_device_generation() {
        let root = test_root("device-fence");
        let mut runtime = WorkerModelRuntime::new(WorkerRuntimeOptions::new(&root)).unwrap();
        let device_a = "pci:0000:03:00.0";
        let device_b = "pci:0000:04:00.0";
        seed_device_generation(&mut runtime, device_a, "a", 3);
        seed_device_generation(&mut runtime, device_b, "b", 4);
        let lease_root = root.join("device-leases");
        runtime.device_leases.insert(
            device_a.to_string(),
            DeviceAuthorityLease::acquire(&lease_root, device_a).unwrap(),
        );
        runtime.device_leases.insert(
            device_b.to_string(),
            DeviceAuthorityLease::acquire(&lease_root, device_b).unwrap(),
        );

        let (mut supervisor_a, worker_a) = device_supervisor(device_a, "worker-build", 41, 1);
        supervisor_a
            .begin_attempt(&worker_a, ActiveAttemptIdentity::new(1).unwrap())
            .unwrap();
        let (supervisor_b, worker_b) = device_supervisor(device_b, "worker-build", 42, 2);
        runtime
            .supervisors
            .insert(device_a.to_string(), supervisor_a);
        runtime
            .supervisors
            .insert(device_b.to_string(), supervisor_b);

        let effect = runtime
            .supervisors
            .get_mut(device_a)
            .unwrap()
            .record_worker_failure(&worker_a, WorkerFailureKind::DeviceLost, 10_000)
            .unwrap();
        runtime.release_device_admission_generation(device_a);

        assert!(effect.active_terminal.is_some());
        assert_eq!(
            runtime.supervisors.get(device_a).unwrap().phase(),
            WorkerLifecyclePhase::CoolingDown
        );
        assert_eq!(
            runtime.supervisors.get(device_b).unwrap().phase(),
            WorkerLifecyclePhase::Ready
        );
        assert_eq!(
            runtime.supervisors.get(device_b).unwrap().current_worker(),
            Some(&worker_b)
        );
        let device_a_status = runtime
            .admissions
            .get_mut(device_a)
            .unwrap()
            .ledger
            .status()
            .unwrap();
        assert_eq!(
            device_a_status.resident,
            crate::admission::ResidentReservations::default()
        );
        assert_eq!(device_a_status.pending_reservations, 0);
        assert_eq!(device_a_status.active_reservations, 0);

        let device_b_status = runtime
            .admissions
            .get_mut(device_b)
            .unwrap()
            .ledger
            .status()
            .unwrap();
        assert!(device_b_status.resident.total_bytes().unwrap() > 0);
        assert_eq!(device_b_status.pending_reservations, 1);
        assert_eq!(device_b_status.active_reservations, 1);
        assert!(runtime.pending_admissions.contains_key("context-pending-b"));
        assert!(runtime.active_admissions.contains_key("context-active-b"));
        assert!(!runtime.pending_admissions.contains_key("context-pending-a"));
        assert!(!runtime.active_admissions.contains_key("context-active-a"));
        assert!(!runtime.device_leases.contains_key(device_a));
        assert!(runtime.device_leases.contains_key(device_b));

        runtime.release_admission_generation();
        let device_b_after_host_teardown = runtime
            .admissions
            .get_mut(device_b)
            .unwrap()
            .ledger
            .status()
            .unwrap();
        assert_eq!(
            device_b_after_host_teardown.resident,
            crate::admission::ResidentReservations::default()
        );
        assert_eq!(device_b_after_host_teardown.pending_reservations, 0);
        assert_eq!(device_b_after_host_teardown.active_reservations, 0);
        assert!(runtime.pending_admissions.is_empty());
        assert!(runtime.active_admissions.is_empty());
        assert!(runtime.device_leases.is_empty());

        drop(runtime);
        let _ = fs::remove_dir_all(root);
    }
}
