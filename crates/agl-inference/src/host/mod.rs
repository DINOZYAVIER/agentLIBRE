mod attempt;
mod config;
pub(crate) mod descriptors;
mod media;
mod resource_ledger;

use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agl_model::{CatalogCapability, HostCapabilities, ModelArtifactRole, ModelExecutionPlan};
use tokio::sync::Semaphore;

pub use descriptors::{ArtifactFileHandle, DescriptorSetError};
pub use media::{
    EngineDeviceRuntimeIdentity, EngineExecutable, EngineInventory, InferenceFailure,
    InferenceHostConfig, InferenceHostStartError, InferenceHostStatus, InferenceQueueRejection,
    ResolvedMediaAttachment, VolatileHandles,
};
pub use resource_ledger::{
    LiveAdmissionRejection, ResourcePools, ResourceRequest, ResourceReservation,
};

use self::attempt::{AttemptRecorder, recover_attempt_journals};
#[cfg(test)]
use self::config::sha256_path;
use self::config::{
    acquire_authority_lease, prepare_authority_root, prepare_evidence_root, validate_idle_duration,
    validate_inventory,
};
use self::descriptors::open_verified;
use self::resource_ledger::LiveResourceLedger;

#[derive(Debug)]
struct HostState {
    shutting_down: bool,
    pending: usize,
    active: usize,
    active_cancellation: Option<crate::InferenceCancellation>,
}

#[derive(Debug)]
struct ResidentEngine {
    model_key: String,
    context: Option<ResidentContext>,
    model_idle_deadline: Option<Instant>,
    process: crate::engine::EngineProcess,
    reservation: ResourceReservation,
}

#[derive(Debug)]
struct ResidentContext {
    key: String,
    cache_loaded: bool,
    prefix: Option<agl_oven::RenderedModelRequest>,
    idle_deadline: Instant,
}

#[derive(Clone, Debug)]
pub struct InferenceHost {
    inventory: EngineInventory,
    capabilities: HostCapabilities,
    executable_path: Arc<PathBuf>,
    executable: Arc<File>,
    queue_capacity: usize,
    slot: Arc<Semaphore>,
    state: Arc<Mutex<HostState>>,
    ledger: Arc<Mutex<LiveResourceLedger>>,
    resident: Arc<Mutex<Option<ResidentEngine>>>,
    next_engine_generation: Arc<AtomicU64>,
    journal_root: Option<Arc<PathBuf>>,
    authority_leases: Arc<Vec<crate::DeviceAuthorityLease>>,
    context_idle_duration: Duration,
    model_idle_duration: Duration,
    external_host_reserve_bytes: u64,
    health_store: Arc<crate::durable_health::DurableHealthStore>,
    health_policy: crate::worker_supervisor::WorkerCircuitBreakerPolicy,
    evidence_root: Arc<PathBuf>,
}

impl InferenceHost {
    pub fn start(config: InferenceHostConfig) -> Result<Self, InferenceHostStartError> {
        let discovered = crate::engine::discover(&config.executable)?;
        validate_inventory(&discovered.inventory)?;
        if config.queue_capacity == 0 {
            return Err(InferenceHostStartError::InvalidEngineInventory {
                reason: "queue_capacity must be greater than zero".to_owned(),
            });
        }
        validate_idle_duration("context_idle_duration", config.context_idle_duration)?;
        validate_idle_duration("model_idle_duration", config.model_idle_duration)?;
        prepare_authority_root(&config.authority_root)?;
        prepare_evidence_root(&config.evidence_root)?;
        let lease_root = config.authority_root.join("leases");
        let mut authority_leases = Vec::new();
        authority_leases.push(acquire_authority_lease(&lease_root, "host")?);
        for device in discovered
            .inventory
            .devices
            .iter()
            .filter(|device| device.usable && device.supports_gpu_offload)
        {
            authority_leases.push(acquire_authority_lease(&lease_root, &device.identity)?);
        }
        let health_store =
            crate::durable_health::DurableHealthStore::open(config.authority_root.join("health"))
                .map_err(|error| InferenceHostStartError::LeaseUnavailable {
                reason: format!("durable inference health: {error}"),
            })?;
        let health_policy =
            crate::worker_supervisor::WorkerCircuitBreakerPolicy::new(5_000, 60_000, 5)
                .expect("default inference health policy is valid");
        let available_host_bytes = discovered
            .available
            .host_bytes
            .checked_sub(config.external_host_reserve_bytes)
            .ok_or_else(|| InferenceHostStartError::InvalidEngineInventory {
                reason: "external host reserve exceeds currently available RAM".to_owned(),
            })?;
        let available = ResourcePools {
            host_bytes: available_host_bytes,
            ..discovered.available
        };
        let host = Self {
            inventory: discovered.inventory.clone(),
            capabilities: HostCapabilities {
                physical_host_bytes: discovered.inventory.physical_host_bytes,
                physical_cpu_cores: discovered.inventory.physical_cpu_cores,
                logical_cpu_cores: discovered.inventory.logical_cpu_cores,
                devices: discovered.inventory.devices.clone(),
            },
            executable_path: Arc::new(config.executable.path),
            executable: Arc::new(discovered.executable),
            queue_capacity: config.queue_capacity,
            slot: Arc::new(Semaphore::new(1)),
            state: Arc::new(Mutex::new(HostState {
                shutting_down: false,
                pending: 0,
                active: 0,
                active_cancellation: None,
            })),
            ledger: Arc::new(Mutex::new(LiveResourceLedger::new(available))),
            resident: Arc::new(Mutex::new(None)),
            next_engine_generation: Arc::new(AtomicU64::new(1)),
            journal_root: None,
            authority_leases: Arc::new(authority_leases),
            context_idle_duration: config.context_idle_duration,
            model_idle_duration: config.model_idle_duration,
            external_host_reserve_bytes: config.external_host_reserve_bytes,
            health_store: Arc::new(health_store),
            health_policy,
            evidence_root: Arc::new(config.evidence_root),
        };
        host.spawn_idle_reaper();
        Ok(host)
    }

    pub fn start_with_journal_root(
        config: InferenceHostConfig,
        journal_root: impl Into<PathBuf>,
    ) -> Result<Self, InferenceHostStartError> {
        let journal_root = journal_root.into();
        recover_attempt_journals(&journal_root, &config.evidence_root)?;
        let mut host = Self::start(config)?;
        host.journal_root = Some(Arc::new(journal_root));
        Ok(host)
    }

    pub fn static_capabilities(&self) -> &HostCapabilities {
        &self.capabilities
    }

    pub fn engine_inventory(&self) -> &EngineInventory {
        &self.inventory
    }

    pub async fn submit(
        &self,
        plan: ModelExecutionPlan,
        request: crate::InferenceRequest,
        artifacts: Vec<ArtifactFileHandle>,
        volatile: VolatileHandles,
    ) -> Result<crate::InferenceResponse, InferenceFailure> {
        if let Some(root) = volatile.evidence_root.as_ref() {
            validate_evidence_root(root, self.evidence_root.as_ref())?;
        }
        let mut attempt = AttemptRecorder::begin(
            self.journal_root.as_deref(),
            volatile.evidence_root.as_deref(),
            volatile.product_resolution.clone(),
            &plan,
            &request,
        )?;
        let result = self
            .submit_live(&plan, &request, artifacts, volatile, &mut attempt)
            .await;
        attempt.finish(&result)?;
        result
    }

    pub fn record_plan_rejection(
        &self,
        request: &crate::InferenceRequest,
        rejection: crate::InferencePlanRejectionEvidence,
        evidence_root: Option<&std::path::Path>,
    ) -> Result<(), InferenceFailure> {
        if let Some(root) = evidence_root {
            validate_evidence_root(root, self.evidence_root.as_ref())?;
        }
        AttemptRecorder::reject_plan(
            self.journal_root.as_deref(),
            evidence_root,
            rejection,
            request,
        )
    }

    async fn submit_live(
        &self,
        plan: &ModelExecutionPlan,
        request: &crate::InferenceRequest,
        artifacts: Vec<ArtifactFileHandle>,
        volatile: VolatileHandles,
        attempt: &mut AttemptRecorder,
    ) -> Result<crate::InferenceResponse, InferenceFailure> {
        let media_accounting = volatile.media_accounting(request)?;
        validate_supported_media(plan, &volatile.media)?;
        let volatile_host_bytes = media_accounting.admitted_host_bytes;
        attempt.content_ready(request, media_accounting)?;
        let broker = Arc::new(crate::output::PublicInferenceOutputBroker::new(
            request.attempt_id.clone(),
            Arc::clone(&volatile.output_sink),
        ));
        broker.emit_host_stage(crate::InferenceProductStage::Queued);
        let _active = self
            .acquire_execution(&volatile.cancellation, volatile.deadline)
            .await
            .inspect_err(|error| emit_terminal(&broker, error))?;
        broker.emit_host_stage(crate::InferenceProductStage::Admission);
        check_control(&volatile.cancellation, volatile.deadline)
            .inspect_err(|error| emit_terminal(&broker, error))?;
        let descriptors = open_verified(plan, &artifacts).map_err(|error| match error {
            DescriptorSetError::Changed { basename } => {
                InferenceFailure::DescriptorChanged { basename }
            }
            error => error.into(),
        })?;
        let model_key = plan.model_key().as_str().to_owned();
        let context_key = plan.context_key(
            request
                .session_id
                .as_ref()
                .map(|value| value.as_str())
                .unwrap_or_else(|| request.run_id.as_str()),
        );
        let mut resident = self
            .resident
            .lock()
            .expect("resident inference engine state poisoned");
        if resident
            .as_ref()
            .is_some_and(|current| current.model_key != model_key)
        {
            self.release_resident(resident.take().expect("resident was checked as present"));
        }
        let health_key = self.health_key(plan)?;
        let quarantine_key = self.quarantine_key(plan, &health_key)?;
        self.check_health_authority(&health_key, &quarantine_key)?;
        self.refresh_live_capacity(resident.as_mut())?;
        let (cold_reservation, transient_reservation) = if resident.is_none() {
            let (model, transient) = self.reserve_cold(plan, volatile_host_bytes)?;
            (Some(model), transient)
        } else {
            (None, self.reserve_transient(volatile_host_bytes)?)
        };
        let transient = TransientReservation::new(self.clone(), transient_reservation);
        if resident.is_none() {
            let reservation = cold_reservation.expect("cold reservation was created");
            attempt.admitted(&reservation, &transient.reservation, false)?;
            let generation = self.next_engine_generation.fetch_add(1, Ordering::Relaxed);
            attempt.dispatched(generation, &model_key)?;
            broker.emit_engine_stage(crate::InferenceProductStage::ModelLoad);
            let native_device_id = plan.selected_device().and_then(|selected| {
                self.inventory
                    .runtime_devices
                    .iter()
                    .find(|runtime| runtime.identity == selected.identity)
                    .map(|runtime| runtime.native_device_id.as_str())
            });
            let process = match crate::engine::EngineProcess::start(
                self.executable_path.as_ref(),
                self.executable.as_ref(),
                plan,
                native_device_id,
                descriptors,
                generation,
                reservation.id(),
            ) {
                Ok(process) => process,
                Err(error) => {
                    self.release_reservation(&reservation);
                    let health_result = self
                        .record_unsafe_receipt(&quarantine_key, &error)
                        .and_then(|()| {
                            self.record_engine_failure(&health_key, None, generation, &error)
                        });
                    emit_terminal(&broker, &error);
                    health_result?;
                    return Err(error);
                }
            };
            if let Err(error) = attempt.runtime_started(process.receipt()) {
                let mut process = process;
                process.terminate();
                self.release_reservation(&reservation);
                emit_terminal(&broker, &error);
                return Err(error);
            }
            *resident = Some(ResidentEngine {
                model_key: model_key.clone(),
                context: None,
                model_idle_deadline: None,
                process,
                reservation,
            });
        } else {
            drop(descriptors);
            broker.emit_engine_stage(crate::InferenceProductStage::ModelReuse);
            let engine_reservation = &resident
                .as_ref()
                .expect("resident engine was checked as present")
                .reservation;
            attempt.admitted(engine_reservation, &transient.reservation, true)?;
            let generation = self
                .next_engine_generation
                .load(Ordering::Relaxed)
                .saturating_sub(1);
            attempt.dispatched(generation, &model_key)?;
            let receipt = resident
                .as_ref()
                .expect("resident engine was checked as present")
                .process
                .receipt()
                .clone();
            attempt.runtime_started(&receipt)?;
        }
        let engine = resident.as_mut().expect("resident engine was loaded");
        engine.model_idle_deadline = None;
        if engine.context.as_ref().is_some_and(|context| {
            context.key == context_key.as_str()
                && context.cache_loaded
                && context
                    .prefix
                    .as_ref()
                    .is_some_and(|prefix| is_exact_context_extension(prefix, &request.rendered))
        }) {
            broker.emit_engine_stage(crate::InferenceProductStage::ContextReuse);
        } else {
            if engine
                .context
                .as_ref()
                .is_some_and(|context| context.cache_loaded)
            {
                engine.process.clear_slot()?;
            }
            engine.context = Some(ResidentContext {
                key: context_key.as_str().to_owned(),
                cache_loaded: true,
                prefix: Some(request.rendered.clone()),
                idle_deadline: Instant::now() + self.context_idle_duration,
            });
            broker.emit_engine_stage(crate::InferenceProductStage::ContextRebuild);
            broker.emit_engine_stage(crate::InferenceProductStage::Prefill);
        }
        check_control(&volatile.cancellation, volatile.deadline)
            .inspect_err(|error| emit_terminal(&broker, error))?;
        broker.emit_engine_stage(crate::InferenceProductStage::Generation);
        let response = engine.process.generate(
            plan,
            request,
            &volatile.media,
            &volatile.cancellation,
            volatile.deadline,
            |sequence, content| {
                let _ = broker.emit_text_delta(sequence, content);
            },
        );
        let health_result = match &response {
            Ok(_) => self.record_engine_success(&health_key),
            Err(InferenceFailure::Cancelled | InferenceFailure::DeadlineExceeded) => Ok(()),
            Err(error) => self
                .record_unsafe_receipt(&quarantine_key, error)
                .and_then(|()| {
                    self.record_engine_failure(
                        &health_key,
                        Some(engine.process.pid()),
                        engine.process.receipt().engine_generation,
                        error,
                    )
                }),
        };
        match &response {
            Ok(response) => {
                broker.emit_engine_stage(crate::InferenceProductStage::OutputParse);
                broker.emit_host_stage(match response.finish_reason {
                    crate::InferenceFinishReason::Stop => crate::InferenceProductStage::Completed,
                    crate::InferenceFinishReason::Length
                    | crate::InferenceFinishReason::ContentByteLimit => {
                        crate::InferenceProductStage::Incomplete
                    }
                });
            }
            Err(error) => emit_terminal(&broker, error),
        }
        if let Ok(success) = &response {
            if let Some(context) = &mut engine.context {
                context.prefix = Some(request.rendered.clone());
                context.idle_deadline = Instant::now() + self.context_idle_duration;
            }
            attempt.response_recorded(success)?;
        }
        if response.is_err() || health_result.is_err() {
            let failed = resident.take();
            drop(resident);
            if let Some(failed) = failed {
                self.release_resident(failed);
            }
        }
        drop(transient);
        health_result?;
        response
    }

    async fn acquire_execution(
        &self,
        cancellation: &crate::InferenceCancellation,
        deadline: Option<Instant>,
    ) -> Result<ActiveExecution, InferenceFailure> {
        {
            let mut state = self.state.lock().expect("inference host state poisoned");
            if state.shutting_down {
                return Err(InferenceQueueRejection::ShuttingDown.into());
            }
            if state.pending >= self.queue_capacity {
                return Err(InferenceQueueRejection::Full {
                    capacity: self.queue_capacity,
                    retryable: true,
                }
                .into());
            }
            state.pending += 1;
        }
        let acquire = self.slot.clone().acquire_owned();
        tokio::pin!(acquire);
        let permit = loop {
            if let Err(error) = check_control(cancellation, deadline) {
                self.decrement_pending();
                return Err(error);
            }
            tokio::select! {
                result = &mut acquire => match result {
                    Ok(permit) => break permit,
                    Err(_) => {
                        self.decrement_pending();
                        return Err(InferenceQueueRejection::ShuttingDown.into());
                    }
                },
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
        };
        let mut state = self.state.lock().expect("inference host state poisoned");
        state.pending = state.pending.saturating_sub(1);
        if state.shutting_down {
            return Err(InferenceQueueRejection::ShuttingDown.into());
        }
        state.active = 1;
        state.active_cancellation = Some(cancellation.clone());
        drop(state);
        Ok(ActiveExecution {
            host: self.clone(),
            _permit: permit,
        })
    }

    fn decrement_pending(&self) {
        let mut state = self.state.lock().expect("inference host state poisoned");
        state.pending = state.pending.saturating_sub(1);
    }

    fn model_resource_request(
        &self,
        plan: &ModelExecutionPlan,
    ) -> Result<ResourceRequest, InferenceFailure> {
        let resources = plan.resources();
        Ok(ResourceRequest {
            host_bytes: resources
                .host_private_bytes()
                .checked_add(resources.decoder_scratch_bytes())
                .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?,
            device_bytes: resources.device_private_bytes(),
            shared_bytes: resources.shared_bytes(),
        })
    }

    fn reserve_cold(
        &self,
        plan: &ModelExecutionPlan,
        transient_host_bytes: u64,
    ) -> Result<(ResourceReservation, ResourceReservation), InferenceFailure> {
        let model = self.model_resource_request(plan)?;
        let transient = ResourceRequest {
            host_bytes: transient_host_bytes,
            device_bytes: 0,
            shared_bytes: 0,
        };
        Ok(self
            .ledger
            .lock()
            .expect("inference resource ledger poisoned")
            .reserve_pair(model, transient)?)
    }

    fn reserve_transient(&self, host_bytes: u64) -> Result<ResourceReservation, InferenceFailure> {
        Ok(self
            .ledger
            .lock()
            .expect("inference resource ledger poisoned")
            .reserve(ResourceRequest {
                host_bytes,
                device_bytes: 0,
                shared_bytes: 0,
            })?)
    }

    fn refresh_live_capacity(
        &self,
        resident: Option<&mut ResidentEngine>,
    ) -> Result<(), InferenceFailure> {
        let (observed, persistent) = if let Some(resident) = resident {
            (
                resident.process.available_pools(&self.inventory)?,
                ResourcePools {
                    host_bytes: resident.reservation.host_bytes(),
                    device_bytes: resident.reservation.device_bytes(),
                    shared_bytes: resident.reservation.shared_bytes(),
                },
            )
        } else {
            let discovered =
                crate::engine::discover(&self.inventory.executable).map_err(|error| {
                    InferenceFailure::EngineProtocol {
                        reason: format!("fresh engine inventory failed: {error}"),
                    }
                })?;
            if discovered.inventory != self.inventory {
                return Err(InferenceFailure::EngineProtocol {
                    reason: "fresh engine inventory changed static host or engine identity"
                        .to_owned(),
                });
            }
            (discovered.available, ResourcePools::default())
        };
        let observed_host = observed
            .host_bytes
            .checked_sub(self.external_host_reserve_bytes)
            .ok_or(LiveAdmissionRejection::InsufficientHostMemory {
                requested: self.external_host_reserve_bytes,
                available: observed.host_bytes,
            })?;
        let capacity = ResourcePools {
            host_bytes: observed_host
                .checked_add(persistent.host_bytes)
                .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?,
            device_bytes: observed
                .device_bytes
                .checked_add(persistent.device_bytes)
                .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?,
            shared_bytes: observed
                .shared_bytes
                .checked_add(persistent.shared_bytes)
                .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?,
        };
        self.ledger
            .lock()
            .expect("inference resource ledger poisoned")
            .update_capacity(capacity);
        Ok(())
    }

    fn health_key(
        &self,
        plan: &ModelExecutionPlan,
    ) -> Result<crate::worker_supervisor::WorkerHealthKey, InferenceFailure> {
        let identity = plan
            .selected_device()
            .map(|device| device.identity.as_str())
            .unwrap_or("CPU");
        let runtime = self
            .inventory
            .runtime_devices
            .iter()
            .find(|device| device.identity == identity)
            .ok_or_else(|| InferenceFailure::HealthAuthority {
                reason: format!("runtime device identity `{identity}` is unavailable"),
            })?;
        crate::worker_supervisor::WorkerHealthKey::new(
            identity,
            &runtime.driver_build_id,
            &self.inventory.executable.sha256,
        )
        .map_err(|error| InferenceFailure::HealthAuthority {
            reason: error.to_string(),
        })
    }

    fn quarantine_key(
        &self,
        plan: &ModelExecutionPlan,
        health: &crate::worker_supervisor::WorkerHealthKey,
    ) -> Result<crate::durable_health::ResourceQuarantineKey, InferenceFailure> {
        crate::durable_health::ResourceQuarantineKey::new(
            digest_hex(plan.model_key().as_str())?,
            digest_hex(plan.digest().as_str())?,
            sha256_text(health.physical_device_id()),
            digest_hex(health.driver_build_id())?,
            digest_hex(health.worker_build_id())?,
        )
        .map_err(|error| InferenceFailure::HealthAuthority {
            reason: error.to_string(),
        })
    }

    fn check_health_authority(
        &self,
        health_key: &crate::worker_supervisor::WorkerHealthKey,
        quarantine_key: &crate::durable_health::ResourceQuarantineKey,
    ) -> Result<(), InferenceFailure> {
        if self
            .health_store
            .load_resource_quarantine(quarantine_key)
            .map_err(health_store_failure)?
            .is_some()
        {
            return Err(InferenceFailure::Quarantined {
                identity: plan_safe_quarantine_identity(quarantine_key),
            });
        }
        let Some(health) = self
            .health_store
            .load_worker_health(health_key, self.health_policy)
            .map_err(health_store_failure)?
        else {
            return Ok(());
        };
        let now = unix_time_ms()?;
        let mut supervisor = crate::worker_supervisor::WorkerSupervisorState::restore(
            health,
            self.health_policy,
            now,
        )
        .map_err(supervisor_failure)?;
        if supervisor.phase() == crate::worker_supervisor::WorkerLifecyclePhase::CoolingDown
            && let Err(crate::worker_supervisor::WorkerSupervisorError::CooldownActive {
                not_before_unix_ms,
            }) = supervisor.release_cooldown(now)
        {
            return Err(InferenceFailure::CoolingDown { not_before_unix_ms });
        }
        Ok(())
    }

    fn record_engine_success(
        &self,
        key: &crate::worker_supervisor::WorkerHealthKey,
    ) -> Result<(), InferenceFailure> {
        self.health_store
            .clear_worker_health(key, self.health_policy)
            .map(|_| ())
            .map_err(health_store_failure)
    }

    fn record_engine_failure(
        &self,
        key: &crate::worker_supervisor::WorkerHealthKey,
        pid: Option<u32>,
        generation: u64,
        error: &InferenceFailure,
    ) -> Result<(), InferenceFailure> {
        let now = unix_time_ms()?;
        let health = self
            .health_store
            .load_worker_health(key, self.health_policy)
            .map_err(health_store_failure)?
            .unwrap_or_else(|| crate::worker_supervisor::WorkerHealthState::new(key.clone()));
        let mut supervisor = crate::worker_supervisor::WorkerSupervisorState::restore(
            health,
            self.health_policy,
            now,
        )
        .map_err(supervisor_failure)?;
        if supervisor.phase() == crate::worker_supervisor::WorkerLifecyclePhase::CoolingDown {
            return Ok(());
        }
        if let Some(pid) = pid {
            let worker = crate::worker_supervisor::WorkerGenerationIdentity::new(
                pid,
                generation,
                self.inventory.executable.sha256.clone(),
            )
            .map_err(|error| InferenceFailure::HealthAuthority {
                reason: error.to_string(),
            })?;
            supervisor
                .begin_start(worker.clone())
                .map_err(supervisor_failure)?;
            supervisor.mark_ready(&worker).map_err(supervisor_failure)?;
            let kind = if error
                .to_string()
                .to_ascii_lowercase()
                .contains("device lost")
            {
                crate::worker_supervisor::WorkerFailureKind::DeviceLost
            } else {
                crate::worker_supervisor::WorkerFailureKind::ProtocolViolation
            };
            supervisor
                .record_worker_failure(&worker, kind, now)
                .map_err(supervisor_failure)?;
        } else {
            supervisor
                .record_start_failure(now)
                .map_err(supervisor_failure)?;
        }
        self.health_store
            .store_worker_health(supervisor.health(), self.health_policy)
            .map_err(health_store_failure)
    }

    fn record_unsafe_receipt(
        &self,
        key: &crate::durable_health::ResourceQuarantineKey,
        error: &InferenceFailure,
    ) -> Result<(), InferenceFailure> {
        let InferenceFailure::InvalidAllocationReceipt {
            admitted, reported, ..
        } = error
        else {
            return Ok(());
        };
        let quarantine = crate::durable_health::ResourceEstimateQuarantine::new(
            key.clone(),
            crate::admission::AllocationEstimate {
                model_bytes: admitted.device_bytes,
                context_bytes: admitted.shared_bytes,
                transient_bytes: admitted.host_bytes,
                uncertainty_bytes: 0,
            },
            crate::admission::AllocationReceipt {
                model_bytes: reported.device_bytes,
                context_bytes: reported.shared_bytes,
                transient_bytes: reported.host_bytes,
            },
        )
        .map_err(|error| InferenceFailure::HealthAuthority {
            reason: error.to_string(),
        })?;
        self.health_store
            .store_resource_quarantine(&quarantine)
            .map_err(health_store_failure)
    }

    fn release_reservation(&self, reservation: &ResourceReservation) {
        self.ledger
            .lock()
            .expect("inference resource ledger poisoned")
            .release(reservation);
    }

    fn release_resident(&self, mut resident: ResidentEngine) {
        resident.process.terminate();
        self.release_reservation(&resident.reservation);
    }

    pub fn clear_context(
        &self,
        context: &agl_model::ModelContextKey,
    ) -> Result<bool, InferenceFailure> {
        let mut resident = self
            .resident
            .lock()
            .expect("resident inference engine state poisoned");
        let Some(engine) = resident.as_mut() else {
            return Ok(false);
        };
        if engine.context.as_ref().map(|entry| entry.key.as_str()) != Some(context.as_str()) {
            return Ok(false);
        }
        if engine
            .context
            .as_ref()
            .is_some_and(|context| context.cache_loaded)
        {
            engine.process.clear_slot()?;
        }
        engine.context = Some(ResidentContext {
            key: context.as_str().to_owned(),
            cache_loaded: false,
            prefix: None,
            idle_deadline: Instant::now() + self.context_idle_duration,
        });
        Ok(true)
    }

    pub fn release_context(
        &self,
        context: &agl_model::ModelContextKey,
    ) -> Result<bool, InferenceFailure> {
        let mut resident = self
            .resident
            .lock()
            .expect("resident inference engine state poisoned");
        let Some(engine) = resident.as_mut() else {
            return Ok(false);
        };
        if engine.context.as_ref().map(|entry| entry.key.as_str()) != Some(context.as_str()) {
            return Ok(false);
        }
        if engine
            .context
            .as_ref()
            .is_some_and(|context| context.cache_loaded)
        {
            engine.process.clear_slot()?;
        }
        engine.context = None;
        engine.model_idle_deadline = Some(Instant::now() + self.model_idle_duration);
        Ok(true)
    }

    pub fn unload_all(&self) -> Result<bool, InferenceFailure> {
        if self
            .state
            .lock()
            .expect("inference host state poisoned")
            .active
            > 0
        {
            return Err(InferenceFailure::Busy {
                model_key: "active".to_owned(),
            });
        }
        let resident = self
            .resident
            .lock()
            .expect("resident inference engine state poisoned")
            .take();
        if let Some(resident) = resident {
            self.release_resident(resident);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn unload_model_digest(&self, digest: &str) -> Result<bool, InferenceFailure> {
        if self
            .state
            .lock()
            .expect("inference host state poisoned")
            .active
            > 0
        {
            return Err(InferenceFailure::Busy {
                model_key: digest.to_owned(),
            });
        }
        let mut resident = self
            .resident
            .lock()
            .expect("resident inference engine state poisoned");
        if resident
            .as_ref()
            .is_none_or(|engine| engine.model_key != digest)
        {
            return Ok(false);
        }
        let engine = resident.take().expect("matching resident was checked");
        drop(resident);
        self.release_resident(engine);
        Ok(true)
    }

    pub fn submit_blocking(
        &self,
        plan: ModelExecutionPlan,
        request: crate::InferenceRequest,
        artifacts: Vec<ArtifactFileHandle>,
        volatile: VolatileHandles,
    ) -> Result<crate::InferenceResponse, InferenceFailure> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return std::thread::scope(|scope| {
                scope
                    .spawn(|| blocking_runtime(self, plan, request, artifacts, volatile))
                    .join()
                    .map_err(|_| InferenceFailure::EngineProtocol {
                        reason: "standalone inference runtime panicked".to_owned(),
                    })?
            });
        }
        blocking_runtime(self, plan, request, artifacts, volatile)
    }

    pub fn status(&self) -> InferenceHostStatus {
        let (shutting_down, pending, active) = {
            let state = self.state.lock().expect("inference host state poisoned");
            (state.shutting_down, state.pending, state.active)
        };
        let reserved = self
            .ledger
            .lock()
            .expect("inference resource ledger poisoned")
            .reserved();
        let resident = self
            .resident
            .lock()
            .expect("resident inference engine state poisoned");
        InferenceHostStatus {
            shutting_down,
            pending,
            active,
            resident_models: usize::from(resident.is_some()),
            resident_contexts: usize::from(
                resident
                    .as_ref()
                    .and_then(|engine| engine.context.as_ref())
                    .is_some_and(|context| context.cache_loaded),
            ),
            resident_model_digest: resident.as_ref().map(|engine| engine.model_key.clone()),
            reserved,
            authority_leases: self.authority_leases.len(),
        }
    }

    pub fn shutdown(&self) {
        let mut state = self.state.lock().expect("inference host state poisoned");
        state.shutting_down = true;
        if let Some(cancellation) = &state.active_cancellation {
            cancellation.cancel();
        }
        self.slot.close();
        drop(state);
        if let Some(resident) = self
            .resident
            .lock()
            .expect("resident inference engine state poisoned")
            .take()
        {
            self.release_resident(resident);
        }
    }

    fn spawn_idle_reaper(&self) {
        let state = Arc::downgrade(&self.state);
        let resident = Arc::downgrade(&self.resident);
        let ledger = Arc::downgrade(&self.ledger);
        let context_idle_duration = self.context_idle_duration;
        let model_idle_duration = self.model_idle_duration;
        let _ = std::thread::Builder::new()
            .name("agl-inference-residency".to_owned())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let (Some(state), Some(resident), Some(ledger)) =
                        (state.upgrade(), resident.upgrade(), ledger.upgrade())
                    else {
                        break;
                    };
                    let state = state.lock().expect("inference host state poisoned");
                    if state.shutting_down {
                        break;
                    }
                    if state.active != 0 {
                        continue;
                    }
                    drop(state);
                    let now = Instant::now();
                    let mut resident = resident
                        .lock()
                        .expect("resident inference engine state poisoned");
                    let Some(engine) = resident.as_mut() else {
                        continue;
                    };
                    if engine
                        .context
                        .as_ref()
                        .is_some_and(|context| context.idle_deadline <= now)
                    {
                        let clear = if engine
                            .context
                            .as_ref()
                            .is_some_and(|context| context.cache_loaded)
                        {
                            engine.process.clear_slot()
                        } else {
                            Ok(())
                        };
                        if clear.is_err() {
                            let failed = resident.take().expect("resident engine exists");
                            drop(resident);
                            release_resident_parts(failed, &ledger);
                            continue;
                        }
                        engine.context = None;
                        engine.model_idle_deadline = Some(now + model_idle_duration);
                    }
                    if engine
                        .model_idle_deadline
                        .is_some_and(|deadline| deadline <= now)
                    {
                        let idle = resident.take().expect("resident engine exists");
                        drop(resident);
                        release_resident_parts(idle, &ledger);
                    } else if engine.context.is_none() && engine.model_idle_deadline.is_none() {
                        engine.model_idle_deadline = Some(now + model_idle_duration);
                    } else if let Some(context) = &mut engine.context
                        && context.idle_deadline < now
                    {
                        context.idle_deadline = now + context_idle_duration;
                    }
                }
            });
    }
}

fn validate_supported_media(
    plan: &ModelExecutionPlan,
    media: &[ResolvedMediaAttachment],
) -> Result<(), InferenceFailure> {
    validate_supported_media_shape(
        media.len(),
        plan.supports(CatalogCapability::Vision),
        plan.artifact_role(ModelArtifactRole::Projector).is_some(),
        plan.artifact_role(ModelArtifactRole::Draft).is_some(),
    )
}

fn validate_supported_media_shape(
    media_count: usize,
    supports_vision: bool,
    has_projector: bool,
    has_draft: bool,
) -> Result<(), InferenceFailure> {
    if media_count == 0 {
        return Ok(());
    }
    if !supports_vision {
        return Err(InferenceFailure::InvalidMedia {
            reason: "the selected Model package does not declare vision capability".to_owned(),
        });
    }
    if !has_projector {
        return Err(InferenceFailure::InvalidMedia {
            reason: "the selected vision Model plan has no projector artifact".to_owned(),
        });
    }
    if has_draft {
        return Err(InferenceFailure::InvalidMedia {
            reason: "vision media and speculative draft/MTP generation cannot be combined"
                .to_owned(),
        });
    }
    Ok(())
}

fn is_exact_context_extension(
    cached: &agl_oven::RenderedModelRequest,
    requested: &agl_oven::RenderedModelRequest,
) -> bool {
    cached.dialect == requested.dialect
        && cached.tool_call_format == requested.tool_call_format
        && cached.tools == requested.tools
        && cached.messages.len() <= requested.messages.len()
        && cached
            .messages
            .iter()
            .zip(&requested.messages)
            .all(|(cached, requested)| cached == requested)
}

fn release_resident_parts(mut resident: ResidentEngine, ledger: &Mutex<LiveResourceLedger>) {
    resident.process.terminate();
    ledger
        .lock()
        .expect("inference resource ledger poisoned")
        .release(&resident.reservation);
}

fn check_control(
    cancellation: &crate::InferenceCancellation,
    deadline: Option<Instant>,
) -> Result<(), InferenceFailure> {
    if cancellation.is_cancelled() {
        return Err(InferenceFailure::Cancelled);
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(InferenceFailure::DeadlineExceeded);
    }
    Ok(())
}

fn emit_terminal(broker: &crate::output::PublicInferenceOutputBroker, failure: &InferenceFailure) {
    broker.emit_host_stage(match failure {
        InferenceFailure::Cancelled | InferenceFailure::DeadlineExceeded => {
            crate::InferenceProductStage::Cancelled
        }
        InferenceFailure::EngineProtocol { .. }
            if broker
                .last_public_stage()
                .is_some_and(crate::InferenceProductStage::is_worker_owned) =>
        {
            crate::InferenceProductStage::BackendLost
        }
        _ => crate::InferenceProductStage::Failed,
    });
}

struct ActiveExecution {
    host: InferenceHost,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for ActiveExecution {
    fn drop(&mut self) {
        let mut state = self
            .host
            .state
            .lock()
            .expect("inference host state poisoned");
        state.active = 0;
        state.active_cancellation = None;
    }
}

struct TransientReservation {
    host: InferenceHost,
    reservation: ResourceReservation,
}

impl TransientReservation {
    fn new(host: InferenceHost, reservation: ResourceReservation) -> Self {
        Self { host, reservation }
    }
}

impl Drop for TransientReservation {
    fn drop(&mut self) {
        self.host.release_reservation(&self.reservation);
    }
}

fn health_store_failure(error: crate::durable_health::DurableHealthStoreError) -> InferenceFailure {
    InferenceFailure::HealthAuthority {
        reason: error.to_string(),
    }
}

fn supervisor_failure(error: crate::worker_supervisor::WorkerSupervisorError) -> InferenceFailure {
    InferenceFailure::HealthAuthority {
        reason: error.to_string(),
    }
}

fn unix_time_ms() -> Result<u64, InferenceFailure> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| InferenceFailure::HealthAuthority {
            reason: format!("system clock precedes Unix epoch: {error}"),
        })?
        .as_millis()
        .try_into()
        .map_err(|_| InferenceFailure::HealthAuthority {
            reason: "Unix time does not fit u64 milliseconds".to_owned(),
        })
}

fn digest_hex(value: &str) -> Result<String, InferenceFailure> {
    let value = value.strip_prefix("sha256:").unwrap_or(value);
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err(InferenceFailure::HealthAuthority {
            reason: "health identity is not an exact SHA-256 digest".to_owned(),
        })
    }
}

fn sha256_text(value: &str) -> String {
    use sha2::{Digest as _, Sha256};

    let digest = Sha256::digest(value.as_bytes());
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing a SHA-256 digest cannot fail");
    }
    output
}

fn plan_safe_quarantine_identity(key: &crate::durable_health::ResourceQuarantineKey) -> String {
    key.model_digest().chars().take(16).collect()
}

fn validate_evidence_root(
    root: &std::path::Path,
    allowed: &std::path::Path,
) -> Result<(), InferenceFailure> {
    let root = root
        .canonicalize()
        .map_err(|error| InferenceFailure::EngineProtocol {
            reason: format!("attempt evidence root is unavailable: {error}"),
        })?;
    let allowed = allowed
        .canonicalize()
        .map_err(|error| InferenceFailure::EngineProtocol {
            reason: format!("configured evidence authority is unavailable: {error}"),
        })?;
    if !root.starts_with(&allowed) {
        return Err(InferenceFailure::EngineProtocol {
            reason: "attempt evidence root is outside the configured authority".to_owned(),
        });
    }
    Ok(())
}

pub(super) fn validate_recovered_projection_root(
    root: &std::path::Path,
    allowed: &std::path::Path,
) -> Result<(), InferenceHostStartError> {
    let root = root
        .canonicalize()
        .map_err(|error| InferenceHostStartError::EngineStart {
            reason: format!("attempt projection root is unavailable: {error}"),
        })?;
    let allowed = allowed
        .canonicalize()
        .map_err(|error| InferenceHostStartError::EngineStart {
            reason: format!("configured evidence authority is unavailable: {error}"),
        })?;
    if !root.starts_with(&allowed) {
        return Err(InferenceHostStartError::EngineStart {
            reason: "attempt projection root is outside the configured evidence authority"
                .to_owned(),
        });
    }
    Ok(())
}

fn blocking_runtime(
    host: &InferenceHost,
    plan: ModelExecutionPlan,
    request: crate::InferenceRequest,
    artifacts: Vec<ArtifactFileHandle>,
    volatile: VolatileHandles,
) -> Result<crate::InferenceResponse, InferenceFailure> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| InferenceFailure::EngineProtocol {
            reason: format!("failed to create standalone inference runtime: {error}"),
        })?
        .block_on(host.submit(plan, request, artifacts, volatile))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use agl_config::{ModelDialect, ToolCallFormat};
    use agl_content::{
        ArtifactSensitivity, ArtifactSource, ArtifactSourceKind, BlobDigest, Content,
        ContentAttachmentId, ContentAttachmentRef, ContentPart, ImageDimensions, MediaType,
    };
    use agl_ids::{AttemptId, RunId, TurnId};
    use agl_oven::{RenderedMessage, RenderedMessageRole, RenderedModelRequest};

    use super::{
        EngineExecutable, InferenceFailure, InferenceHost, InferenceHostConfig,
        InferenceQueueRejection, ResolvedMediaAttachment, VolatileHandles,
        is_exact_context_extension, sha256_path, validate_supported_media_shape,
    };
    use crate::InferenceRequest;

    static NEXT_HOST_FIXTURE: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn media_accounting_requires_exact_order_and_recomputes_integrity() {
        let first = attachment(&[1, 2, 3], 2, 3);
        let second = attachment(&[4, 5], 5, 7);
        let request = request_with_attachments([first.reference.clone(), second.reference.clone()]);

        let accounting = VolatileHandles {
            media: vec![first.clone(), second.clone()],
            ..VolatileHandles::default()
        }
        .media_accounting(&request)
        .unwrap();
        assert_eq!(accounting.resolved_bytes, 5);
        assert_eq!(accounting.decoder_allowance_bytes, (2 * 3 + 5 * 7) * 4);
        assert!(accounting.transport_bytes > accounting.resolved_bytes);

        let reordered = VolatileHandles {
            media: vec![second, first.clone()],
            ..VolatileHandles::default()
        }
        .media_accounting(&request);
        assert!(matches!(
            reordered,
            Err(InferenceFailure::InvalidMedia { .. })
        ));

        let mut corrupted = first;
        corrupted.bytes = Arc::from([9_u8, 9, 9]);
        let corrupt = VolatileHandles {
            media: vec![corrupted],
            ..VolatileHandles::default()
        }
        .media_accounting(&request_with_attachments([request_attachment(&request, 0)]));
        assert!(matches!(
            corrupt,
            Err(InferenceFailure::InvalidMedia { .. })
        ));
    }

    // MIW-MEDIA-001. Native multimodal support never widens the package-bound
    // profile matrix, and draft/MTP remains disjoint from vision.
    #[test]
    fn media_requires_vision_projector_and_excludes_mtp() {
        assert!(validate_supported_media_shape(0, false, false, true).is_ok());
        for shape in [
            (1, false, false, false),
            (1, true, false, false),
            (1, true, true, true),
        ] {
            assert!(validate_supported_media_shape(shape.0, shape.1, shape.2, shape.3).is_err());
        }
        assert!(validate_supported_media_shape(1, true, true, false).is_ok());
    }

    // MIW-CACHE-001. ContextKey equality is insufficient: the complete cached
    // message/media prefix and visible Tool set must still be exact.
    // MIW-ENG-006.
    #[test]
    fn context_reuse_requires_an_exact_structural_prefix() {
        let cached = request_with_text("original").rendered;
        assert!(is_exact_context_extension(&cached, &cached));

        let mut extended = cached.clone();
        extended.messages.push(RenderedMessage {
            role: RenderedMessageRole::Assistant,
            content: Some(Content::text("next").unwrap()),
            name: None,
            tool_calls: Vec::new(),
        });
        assert!(is_exact_context_extension(&cached, &extended));

        let mut drifted = extended.clone();
        drifted.messages[0].content = Some(Content::text("changed").unwrap());
        assert!(!is_exact_context_extension(&cached, &drifted));

        let mut tools_changed = extended;
        tools_changed.tools.push(agl_oven::RenderedTool {
            name: "core.repo:read".to_owned(),
            description: "read".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        });
        assert!(!is_exact_context_extension(&cached, &tools_changed));
    }

    // MIW-QUE-001 and MIW-QUE-002. Active work is outside pending capacity;
    // queued cancellation releases its slot and surviving waiters remain FIFO.
    #[tokio::test]
    async fn bounded_queue_cancellation_preserves_survivor_fifo() {
        let host = queue_host(2);
        let active_cancel = crate::InferenceCancellation::new();
        let active = host.acquire_execution(&active_cancel, None).await.unwrap();

        let first_cancel = crate::InferenceCancellation::new();
        let first_task = {
            let host = host.clone();
            let cancellation = first_cancel.clone();
            tokio::spawn(async move { host.acquire_execution(&cancellation, None).await })
        };
        wait_for_pending(&host, 1).await;
        let second_cancel = crate::InferenceCancellation::new();
        let (order_tx, mut order_rx) = tokio::sync::mpsc::unbounded_channel();
        let second_task = {
            let host = host.clone();
            let cancellation = second_cancel.clone();
            let order_tx = order_tx.clone();
            tokio::spawn(async move {
                let guard = host.acquire_execution(&cancellation, None).await.unwrap();
                order_tx.send(2_u8).unwrap();
                drop(guard);
            })
        };
        wait_for_pending(&host, 2).await;

        assert!(matches!(
            host.acquire_execution(&crate::InferenceCancellation::new(), None)
                .await,
            Err(InferenceFailure::Queue(InferenceQueueRejection::Full {
                capacity: 2,
                retryable: true,
            }))
        ));
        first_cancel.cancel();
        assert!(matches!(
            first_task.await.unwrap(),
            Err(InferenceFailure::Cancelled)
        ));
        wait_for_pending(&host, 1).await;

        let third_cancel = crate::InferenceCancellation::new();
        let third_task = {
            let host = host.clone();
            let cancellation = third_cancel.clone();
            let order_tx = order_tx.clone();
            tokio::spawn(async move {
                let guard = host.acquire_execution(&cancellation, None).await.unwrap();
                order_tx.send(3_u8).unwrap();
                drop(guard);
            })
        };
        wait_for_pending(&host, 2).await;
        drop(active);
        assert_eq!(order_rx.recv().await, Some(2));
        assert_eq!(order_rx.recv().await, Some(3));
        second_task.await.unwrap();
        third_task.await.unwrap();
        assert_eq!(host.status().pending, 0);
        assert_eq!(host.status().active, 0);
    }

    // MIW-QUE-002. Shutdown rejects new work, wakes pending work and signals
    // the exact active attempt through its cancellation handle.
    #[tokio::test]
    async fn shutdown_closes_pending_and_cancels_active_authority() {
        let host = queue_host(1);
        let active_cancel = crate::InferenceCancellation::new();
        let active = host.acquire_execution(&active_cancel, None).await.unwrap();
        let queued_cancel = crate::InferenceCancellation::new();
        let queued = {
            let host = host.clone();
            tokio::spawn(async move { host.acquire_execution(&queued_cancel, None).await })
        };
        wait_for_pending(&host, 1).await;
        host.shutdown();
        assert!(active_cancel.is_cancelled());
        assert!(matches!(
            queued.await.unwrap(),
            Err(InferenceFailure::Queue(
                InferenceQueueRejection::ShuttingDown
            ))
        ));
        assert!(matches!(
            host.acquire_execution(&crate::InferenceCancellation::new(), None)
                .await,
            Err(InferenceFailure::Queue(
                InferenceQueueRejection::ShuttingDown
            ))
        ));
        drop(active);
    }

    async fn wait_for_pending(host: &InferenceHost, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while host.status().pending != expected {
            assert!(
                Instant::now() < deadline,
                "pending queue did not reach {expected}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn queue_host(queue_capacity: usize) -> InferenceHost {
        let executable = inventory_fixture();
        InferenceHost::start(InferenceHostConfig {
            executable: EngineExecutable {
                sha256: sha256_path(&executable).unwrap(),
                path: executable.clone(),
            },
            queue_capacity,
            external_host_reserve_bytes: 0,
            authority_root: executable.parent().unwrap().join("authority"),
            context_idle_duration: Duration::from_secs(900),
            model_idle_duration: Duration::from_secs(300),
            evidence_root: executable.parent().unwrap().join("evidence"),
        })
        .unwrap()
    }

    fn inventory_fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agl173-queue-{}-{}",
            std::process::id(),
            NEXT_HOST_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("llama-server");
        fs::write(
            &path,
            "#!/usr/bin/env bash\nprintf '%s\\n' '{\"schema\":\"agentlibre.llama-inventory/v1\",\"llama_cpp_commit\":\"0123456\",\"devices\":[{\"identity\":\"CPU\",\"description\":\"fixture\",\"native_device_id\":\"\",\"kind\":\"cpu\",\"available_pool_bytes\":1073741824,\"physical_pool_bytes\":1073741824}]}' >&$AGL_LLAMA_SERVER_INVENTORY_FD\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn request_attachment(request: &InferenceRequest, index: usize) -> ContentAttachmentRef {
        request.rendered.messages[0]
            .content
            .as_ref()
            .unwrap()
            .attachments()
            .nth(index)
            .unwrap()
            .clone()
    }

    fn attachment(bytes: &[u8], width: u32, height: u32) -> ResolvedMediaAttachment {
        let reference = ContentAttachmentRef::new(
            ContentAttachmentId::generate(),
            BlobDigest::from_bytes(bytes),
            MediaType::ImagePng,
            bytes.len() as u64,
            Some(ImageDimensions::new(width, height).unwrap()),
            ArtifactSensitivity::Private,
            ArtifactSource {
                kind: ArtifactSourceKind::UserProvided,
                extension: Some("png".to_owned()),
            },
        )
        .unwrap();
        ResolvedMediaAttachment::new(reference, Arc::<[u8]>::from(bytes)).unwrap()
    }

    fn request_with_attachments(
        attachments: impl IntoIterator<Item = ContentAttachmentRef>,
    ) -> InferenceRequest {
        let run_id = RunId::generate();
        let turn_id = TurnId::generate();
        InferenceRequest {
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
            attempt_id: AttemptId::generate(),
            session_id: None,
            request_id: None,
            rendered: RenderedModelRequest {
                run_id,
                turn_id,
                request_index: 0,
                dialect: ModelDialect::Generic,
                tool_call_format: ToolCallFormat::StructuredToolCalls,
                messages: vec![RenderedMessage {
                    role: RenderedMessageRole::User,
                    content: Some(
                        Content::new(attachments.into_iter().map(ContentPart::attachment)).unwrap(),
                    ),
                    name: None,
                    tool_calls: Vec::new(),
                }],
                tools: Vec::new(),
            },
        }
    }

    fn request_with_text(text: &str) -> InferenceRequest {
        let run_id = RunId::generate();
        let turn_id = TurnId::generate();
        InferenceRequest {
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
            attempt_id: AttemptId::generate(),
            session_id: None,
            request_id: None,
            rendered: RenderedModelRequest {
                run_id,
                turn_id,
                request_index: 0,
                dialect: ModelDialect::Generic,
                tool_call_format: ToolCallFormat::StructuredToolCalls,
                messages: vec![RenderedMessage {
                    role: RenderedMessageRole::User,
                    content: Some(Content::text(text).unwrap()),
                    name: None,
                    tool_calls: Vec::new(),
                }],
                tools: Vec::new(),
            },
        }
    }
}
