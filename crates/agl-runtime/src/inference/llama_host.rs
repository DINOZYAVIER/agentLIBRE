use std::collections::BTreeMap;
use std::fs;
#[cfg(test)]
use std::io::{Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::model::ModelConfig;
use agl_core::Content;
use agl_core::agent::{
    GpuLayerSelection, InferenceRealizationRef, ModelArtifactKind,
    ModelDialect as RuntimeModelDialect, ModelFinishReason, ModelGenerationOutput,
    ModelGenerationResult, ModelOutputDiagnostic, ModelOutputFailureClass, ModelRuntimeSelection,
    ModelUsage, PackageDigest, PhysicalDeviceSelector, ToolCallFormat as RuntimeToolCallFormat,
};
use agl_execution_api::{
    BlockingExecutionClient, ExecutionId, ExecutionIo, ExecutionIsolation, ExecutionOutcome,
    ExecutionOutputStream, ExecutionOwner, ExecutionSignal, ExecutionStartRequest, ExecutionState,
};
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::inference::output_codec::{ParsedModelOutput, ToolJsonRepair, parse_model_output};
use crate::inference::request_codec::{RenderedMessageRole, render_model_request};
use crate::inference::{
    DriverBuildDigest, EngineBuildDigest, InferenceCancellation, InferenceFailureKind,
    InferenceGenerateRequest, InferenceGenerateResult, InferenceGenerator, InferenceHealthUpdate,
    InferenceProgressSink, InferenceServiceError, PhysicalDeviceDigest, RegisteredModelService,
    RestoredInferenceHealth, RuntimeProfileDigest, WorkerHealth,
};

const MODEL_FD: i32 = 200;
const FIRST_ADAPTER_FD: i32 = MODEL_FD + 1;
const MAX_ADAPTERS: usize = 32;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const CANCELLATION_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(30);
const GENERATION_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlamaRuntimeProfile {
    pub digest: RuntimeProfileDigest,
    pub physical_device: PhysicalDeviceDigest,
    pub driver_build: DriverBuildDigest,
    pub required_host_bytes: u64,
    pub required_device_bytes: u64,
    pub required_shared_bytes: u64,
    pub context_tokens: u32,
    pub batch_size: u32,
    pub ubatch_size: u32,
    pub threads: u32,
    pub gpu_layers: i32,
    #[serde(default)]
    pub speculative: bool,
    #[serde(default)]
    pub speculative_max_draft_tokens: u32,
    #[serde(default)]
    pub speculative_type_k: Option<String>,
    #[serde(default)]
    pub speculative_type_v: Option<String>,
    #[serde(default = "one_slot")]
    pub slots: u32,
    #[serde(default)]
    pub continuous_batching: bool,
    pub device: Option<String>,
    pub device_paths: Vec<PathBuf>,
    #[serde(default)]
    pub selectors: Vec<PhysicalDeviceSelector>,
}

const fn one_slot() -> u32 {
    1
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlamaServerConfig {
    pub executable: PathBuf,
}

pub(crate) fn generator(
    config: LlamaServerConfig,
    expected_engine_build: EngineBuildDigest,
    execution: BlockingExecutionClient,
    restored_health: &RestoredInferenceHealth,
) -> Result<Arc<dyn InferenceGenerator>, InferenceServiceError> {
    let executable = verified_regular_file(&config.executable)?;
    let engine_build = crate::inference::private_engine_build_digest(&executable)
        .map_err(|_| InferenceServiceError::InvalidRequest)?;
    if engine_build != expected_engine_build {
        return Err(InferenceServiceError::InvalidRequest);
    }
    Ok(Arc::new(LlamaHost {
        executable,
        execution,
        engine_build,
        health: Mutex::new(restored_health.clone()),
        health_updates: Mutex::new(Vec::new()),
        service_changes: Mutex::new(()),
        services: Mutex::new(BTreeMap::new()),
    }))
}

struct LlamaHost {
    executable: PathBuf,
    execution: BlockingExecutionClient,
    engine_build: EngineBuildDigest,
    health: Mutex<RestoredInferenceHealth>,
    health_updates: Mutex<Vec<InferenceHealthUpdate>>,
    service_changes: Mutex<()>,
    services: Mutex<BTreeMap<PackageDigest, Arc<RegisteredLlamaService>>>,
}

struct RegisteredLlamaService {
    registration: RegisteredModelService,
    profile: LlamaRuntimeProfile,
    gate: ServiceGate,
    engine: Mutex<Option<Arc<ResidentEngine>>>,
}

struct ServiceGate {
    slots: u32,
    queue_capacity: u32,
    state: Mutex<ServiceGateState>,
    ready: Condvar,
}

#[derive(Default)]
struct ServiceGateState {
    active: u32,
    queued: u32,
}

struct ServiceLease<'a> {
    gate: &'a ServiceGate,
}

impl ServiceGate {
    fn new(slots: u32, queue_capacity: u32) -> Self {
        Self {
            slots,
            queue_capacity,
            state: Mutex::new(ServiceGateState::default()),
            ready: Condvar::new(),
        }
    }

    fn acquire(
        &self,
        cancellation: &InferenceCancellation,
        deadline_at_ms: i64,
    ) -> Result<ServiceLease<'_>, InferenceServiceError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| InferenceServiceError::Unavailable)?;
        if state.active < self.slots {
            state.active += 1;
            return Ok(ServiceLease { gate: self });
        }
        if state.queued >= self.queue_capacity {
            return Err(InferenceServiceError::Unavailable);
        }
        state.queued += 1;
        loop {
            if cancellation.is_cancelled() {
                state.queued -= 1;
                self.ready.notify_one();
                return Err(InferenceServiceError::Cancelled);
            }
            if deadline_at_ms <= unix_ms() {
                state.queued -= 1;
                self.ready.notify_one();
                return Err(InferenceServiceError::Deadline);
            }
            if state.active < self.slots {
                state.queued -= 1;
                state.active += 1;
                return Ok(ServiceLease { gate: self });
            }
            let waited = self
                .ready
                .wait_timeout(state, Duration::from_millis(25))
                .map_err(|_| InferenceServiceError::Unavailable)?;
            state = waited.0;
        }
    }
}

impl Drop for ServiceLease<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.gate.state.lock() {
            state.active = state.active.saturating_sub(1);
            self.gate.ready.notify_one();
        }
    }
}

impl InferenceGenerator for LlamaHost {
    fn measure(
        &self,
        request: InferenceGenerateRequest,
    ) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError> {
        let service = self
            .services
            .lock()
            .map_err(|_| InferenceServiceError::Unavailable)?
            .get(&request.runtime.service.key)
            .cloned()
            .filter(|service| same_service_plan(&service.registration.runtime, &request.runtime))
            .ok_or(InferenceServiceError::Unavailable)?;
        let format = model_config(&request.runtime)?;
        let rendered = render_model_request(&request, &format)
            .map_err(|_| InferenceServiceError::InvalidRequest)?;
        let _lease = service
            .gate
            .acquire(&request.cancellation, request.deadline_at_ms)?;
        let engine = self.resident_engine(&service, &request.cancellation)?;
        if engine.execution_status()? != ExecutionState::Running {
            return Err(engine.unavailable_error());
        }
        let attempt = format!(
            "{}:{}:{}",
            request.operation.run_id, request.operation.ordinal, request.delivery_attempt
        );
        let body = native_request(&rendered, &attempt, &request)?;
        measure_context_capacity(
            &engine.socket_path,
            &body,
            rendered.max_output_tokens,
            engine.profile.context_tokens,
        )
    }

    fn generate(
        &self,
        request: InferenceGenerateRequest,
    ) -> Result<InferenceGenerateResult, InferenceServiceError> {
        if request.cancellation.is_cancelled() {
            return Err(InferenceServiceError::Cancelled);
        }
        let service = self
            .services
            .lock()
            .map_err(|_| InferenceServiceError::Unavailable)?
            .get(&request.runtime.service.key)
            .cloned()
            .filter(|service| same_service_plan(&service.registration.runtime, &request.runtime))
            .ok_or(InferenceServiceError::Unavailable)?;
        let format = model_config(&request.runtime)?;
        let rendered = render_model_request(&request, &format)
            .map_err(|_| InferenceServiceError::InvalidRequest)?;
        let _lease = service
            .gate
            .acquire(&request.cancellation, request.deadline_at_ms)?;
        let engine = self.resident_engine(&service, &request.cancellation)?;
        let generated = match engine.generate(&request, &rendered) {
            Ok(result) => result,
            Err(mut error) => {
                if let InferenceServiceError::InvalidModelOutput(output) = &mut error {
                    output.realization = Some(InferenceRealizationRef {
                        runtime_profile_digest:
                            agl_core::agent::InferenceRuntimeProfileDigest::from_bytes(
                                *engine.profile.digest.as_bytes(),
                            ),
                        engine_build_digest:
                            agl_core::agent::InferenceEngineBuildDigest::from_bytes(
                                *self.engine_build.as_bytes(),
                            ),
                        physical_resource_digest:
                            agl_core::agent::PhysicalResourceDigest::from_bytes(
                                *engine.profile.physical_device.as_bytes(),
                            ),
                    });
                }
                if let Some(kind) = worker_failure_kind(
                    &error,
                    engine
                        .execution_record()
                        .ok()
                        .and_then(|status| status.outcome),
                ) {
                    self.record_worker_failure(&engine.profile, kind);
                }
                if error_requires_engine_restart(&error) {
                    engine.terminate()?;
                    if let Ok(mut resident) = service.engine.lock()
                        && resident
                            .as_ref()
                            .is_some_and(|current| Arc::ptr_eq(current, &engine))
                    {
                        *resident = None;
                    }
                }
                return Err(error);
            }
        };
        Ok(InferenceGenerateResult {
            operation: request.operation,
            delivery_attempt: request.delivery_attempt,
            result: ModelGenerationResult {
                output: generated.output,
                private_reasoning: if matches!(
                    request.runtime.reasoning,
                    agl_core::agent::ReasoningSelection::Enabled { preserve: true, .. }
                ) {
                    generated.private_reasoning
                } else {
                    None
                },
                finish_reason: generated.finish_reason,
                usage: generated.usage,
                realization: InferenceRealizationRef {
                    runtime_profile_digest:
                        agl_core::agent::InferenceRuntimeProfileDigest::from_bytes(
                            *engine.profile.digest.as_bytes(),
                        ),
                    engine_build_digest: agl_core::agent::InferenceEngineBuildDigest::from_bytes(
                        *self.engine_build.as_bytes(),
                    ),
                    physical_resource_digest: agl_core::agent::PhysicalResourceDigest::from_bytes(
                        *engine.profile.physical_device.as_bytes(),
                    ),
                },
                correction: None,
            },
        })
    }

    fn take_health_updates(&self) -> Vec<InferenceHealthUpdate> {
        self.health_updates
            .lock()
            .map(|mut updates| std::mem::take(&mut *updates))
            .unwrap_or_default()
    }

    fn register_service(
        &self,
        registration: RegisteredModelService,
    ) -> Result<(), InferenceServiceError> {
        let _change = self
            .service_changes
            .lock()
            .map_err(|_| InferenceServiceError::Unavailable)?;
        validate_registration(&registration, self.engine_build)?;
        model_config(&registration.runtime)?;
        let profile = generated_profile(&registration.runtime, self.engine_build)?;
        let key = registration.runtime.service.key;
        if let Some(existing) = self
            .services
            .lock()
            .map_err(|_| InferenceServiceError::Unavailable)?
            .get(&key)
            .cloned()
        {
            return if existing.registration.artifact == registration.artifact
                && existing.registration.adapters == registration.adapters
                && same_service_plan(&existing.registration.runtime, &registration.runtime)
            {
                Ok(())
            } else {
                Err(InferenceServiceError::InvalidRequest)
            };
        }
        let service = Arc::new(RegisteredLlamaService {
            gate: ServiceGate::new(
                registration.runtime.service.slots,
                registration.runtime.service.queue_capacity,
            ),
            registration,
            profile,
            engine: Mutex::new(None),
        });
        // Registration validates the plan without cold-loading a Model. The
        // first measured request starts it and verifies readiness/capabilities.
        let replaced = self
            .services
            .lock()
            .map_err(|_| InferenceServiceError::Unavailable)?
            .insert(key, service);
        debug_assert!(replaced.is_none());
        Ok(())
    }

    fn unload_service(&self, key: PackageDigest) -> Result<(), InferenceServiceError> {
        let _change = self
            .service_changes
            .lock()
            .map_err(|_| InferenceServiceError::Unavailable)?;
        let service = self
            .services
            .lock()
            .map_err(|_| InferenceServiceError::Unavailable)?
            .get(&key)
            .cloned();
        if let Some(service) = service {
            let mut resident = service
                .engine
                .lock()
                .map_err(|_| InferenceServiceError::Unavailable)?;
            if let Some(engine) = resident.as_ref() {
                engine.terminate()?;
            }
            *resident = None;
            self.services
                .lock()
                .map_err(|_| InferenceServiceError::Unavailable)?
                .remove(&key);
        }
        Ok(())
    }
}

impl LlamaHost {
    fn resident_engine(
        &self,
        service: &RegisteredLlamaService,
        cancellation: &InferenceCancellation,
    ) -> Result<Arc<ResidentEngine>, InferenceServiceError> {
        let mut resident = service
            .engine
            .lock()
            .map_err(|_| InferenceServiceError::Unavailable)?;
        if let Some(engine) = resident.as_ref()
            && engine.retiring.load(Ordering::Acquire)
        {
            engine.terminate()?;
            *resident = None;
        }
        if let Some(engine) = resident.as_ref() {
            return Ok(Arc::clone(engine));
        }
        if cancellation.is_cancelled() {
            return Err(InferenceServiceError::Cancelled);
        }
        let engine = self.start_engine(service, &mut resident)?;
        *resident = Some(Arc::clone(&engine));
        Ok(engine)
    }

    fn start_engine(
        &self,
        service: &RegisteredLlamaService,
        resident: &mut Option<Arc<ResidentEngine>>,
    ) -> Result<Arc<ResidentEngine>, InferenceServiceError> {
        let health = self
            .health
            .lock()
            .map_err(|_| InferenceServiceError::Unavailable)?
            .clone();
        if let Some(reason) = profile_unavailable_reason(
            &service.profile,
            &service.registration.runtime.artifact.digest,
            &self.engine_build,
            &health,
        ) {
            return Err(InferenceServiceError::UnavailableWithReason(reason));
        }
        tracing::info!(
            model_id=%service.registration.model.id,
            model_digest=%service.registration.model.digest,
            runtime_profile=%service.profile.digest,
            physical_device=%service.profile.physical_device,
            engine_build=%self.engine_build,
            "starting private llama-server realization"
        );
        match ResidentEngine::start(
            &self.executable,
            &service.registration,
            service.profile.clone(),
            self.execution.clone(),
        ) {
            Ok(engine) => {
                tracing::info!(
                    runtime_profile=%service.profile.digest,
                    physical_device=%service.profile.physical_device,
                    "private llama-server realization is ready"
                );
                Ok(Arc::new(engine))
            }
            Err(EngineStartError::InvalidAllocation(observed)) => {
                self.record_resource_quarantine(
                    &service.profile,
                    service.registration.runtime.artifact.digest,
                    observed,
                );
                Err(InferenceServiceError::InvalidRequest)
            }
            Err(EngineStartError::Service(error)) => {
                self.record_worker_failure(&service.profile, InferenceFailureKind::Unavailable);
                Err(error)
            }
            Err(EngineStartError::Exited(outcome)) => {
                self.record_worker_failure(
                    &service.profile,
                    worker_failure_kind(&InferenceServiceError::Unavailable, outcome.clone())
                        .expect("unavailable engine exit has a health class"),
                );
                Err(InferenceServiceError::UnavailableWithReason(format!(
                    "private llama-server exited before readiness: {outcome:?}"
                )))
            }
            Err(EngineStartError::Unreleased(engine)) => {
                // Keep ownership and its reservation until execd confirms reap.
                *resident = Some(Arc::new(*engine));
                Err(InferenceServiceError::OutcomeUnknown)
            }
        }
    }

    fn record_worker_failure(&self, profile: &LlamaRuntimeProfile, kind: InferenceFailureKind) {
        let now = unix_ms();
        let Ok(mut health) = self.health.lock() else {
            return;
        };
        let previous = health.workers.iter().find(|worker| {
            worker.physical_device == profile.physical_device
                && worker.driver_build == profile.driver_build
                && worker.engine_build == self.engine_build
        });
        let crash_streak = previous.map_or(1, |worker| worker.crash_streak.saturating_add(1));
        let exponent = crash_streak.saturating_sub(1).min(6);
        let retry_after_ms = now.saturating_add(1_000_i64.saturating_mul(1_i64 << exponent));
        let update = WorkerHealth {
            physical_device: profile.physical_device,
            driver_build: profile.driver_build,
            engine_build: self.engine_build,
            crash_streak,
            retry_after_ms,
            last_failure_kind: kind,
        };
        health.workers.retain(|worker| {
            worker.physical_device != update.physical_device
                || worker.driver_build != update.driver_build
                || worker.engine_build != update.engine_build
        });
        health.workers.push(update.clone());
        drop(health);
        if let Ok(mut updates) = self.health_updates.lock() {
            updates.push(InferenceHealthUpdate::Worker(update));
        }
    }

    fn record_resource_quarantine(
        &self,
        profile: &LlamaRuntimeProfile,
        model: agl_core::agent::PackageDigest,
        observed: ObservedAllocation,
    ) {
        let update = crate::inference::ResourceQuarantine {
            physical_device: profile.physical_device,
            driver_build: profile.driver_build,
            engine_build: self.engine_build,
            model,
            runtime_profile: profile.digest,
            admitted_host_bytes: profile.required_host_bytes,
            observed_host_bytes: observed.host,
            admitted_device_bytes: profile.required_device_bytes,
            observed_device_bytes: observed.device,
            admitted_shared_bytes: profile.required_shared_bytes,
            observed_shared_bytes: observed.shared,
            recorded_at_ms: unix_ms(),
        };
        if let Ok(mut health) = self.health.lock() {
            health.quarantines.retain(|entry| {
                entry.physical_device != update.physical_device
                    || entry.driver_build != update.driver_build
                    || entry.engine_build != update.engine_build
                    || entry.model != update.model
                    || entry.runtime_profile != update.runtime_profile
            });
            health.quarantines.push(update.clone());
        }
        if let Ok(mut updates) = self.health_updates.lock() {
            updates.push(InferenceHealthUpdate::Quarantine(update));
        }
    }
}

impl Drop for LlamaHost {
    fn drop(&mut self) {
        if let Ok(services) = self.services.get_mut() {
            for service in services.values() {
                if let Ok(mut engine) = service.engine.lock()
                    && let Some(engine) = engine.take()
                {
                    let _ = engine.terminate();
                }
            }
        }
    }
}

fn validate_registration(
    registration: &RegisteredModelService,
    engine_build: EngineBuildDigest,
) -> Result<(), InferenceServiceError> {
    let runtime = &registration.runtime;
    if runtime.artifact.kind != ModelArtifactKind::Gguf
        || runtime.artifact.bytes == 0
        || runtime.service.slots == 0
        || runtime.service.queue_capacity == 0
        || runtime.load.context_tokens == 0
        || runtime.load.batch_size == 0
        || runtime.load.ubatch_size == 0
        || runtime.load.ubatch_size > runtime.load.batch_size
        || runtime.load.threads == 0
        || runtime.load.threads_batch == 0
        || runtime.load.engine_build_digest.as_bytes() != engine_build.as_bytes()
        || runtime.adapters.len() != registration.adapters.len()
        || runtime.adapters.len() > MAX_ADAPTERS
        || runtime
            .adapters
            .iter()
            .any(|adapter| adapter.artifact.kind != ModelArtifactKind::LoraAdapter)
    {
        return Err(InferenceServiceError::InvalidRequest);
    }
    if registration.artifact.verify_unchanged().is_err()
        || registration.artifact.digest() != model_artifact_digest(runtime.artifact.digest)?
        || registration.artifact.bytes() != runtime.artifact.bytes
    {
        return Err(InferenceServiceError::InvalidRequest);
    }
    for (adapter, imported) in runtime.adapters.iter().zip(&registration.adapters) {
        if imported.verify_unchanged().is_err()
            || imported.digest() != model_artifact_digest(adapter.artifact.digest)?
            || imported.bytes() != adapter.artifact.bytes
            || !adapter.scale.is_finite()
            || adapter.scale <= 0.0
        {
            return Err(InferenceServiceError::InvalidRequest);
        }
    }
    Ok(())
}

fn model_artifact_digest(
    digest: PackageDigest,
) -> Result<crate::model::ModelArtifactDigest, InferenceServiceError> {
    crate::model::ModelArtifactDigest::parse(digest.to_string())
        .map_err(|_| InferenceServiceError::InvalidRequest)
}

fn model_config(runtime: &ModelRuntimeSelection) -> Result<ModelConfig, InferenceServiceError> {
    let config = ModelConfig {
        dialect: match runtime.dialect {
            RuntimeModelDialect::Generic => crate::model::ModelDialect::Generic,
            RuntimeModelDialect::Qwen3 => crate::model::ModelDialect::Qwen3,
            RuntimeModelDialect::Gemma4 => crate::model::ModelDialect::Gemma4,
        },
        tool_call_format: match runtime.tool_call_format {
            RuntimeToolCallFormat::StructuredToolCalls => {
                crate::model::ToolCallFormat::StructuredToolCalls
            }
            RuntimeToolCallFormat::HermesJson => crate::model::ToolCallFormat::HermesJson,
            RuntimeToolCallFormat::GemmaAgentCall => crate::model::ToolCallFormat::GemmaAgentCall,
        },
    };
    config
        .validate()
        .map_err(|_| InferenceServiceError::InvalidRequest)?;
    Ok(config)
}

fn same_service_plan(left: &ModelRuntimeSelection, right: &ModelRuntimeSelection) -> bool {
    left.shares_service_with(right)
}

fn generated_profile(
    runtime: &ModelRuntimeSelection,
    engine_build: EngineBuildDigest,
) -> Result<LlamaRuntimeProfile, InferenceServiceError> {
    let gpu = !matches!(runtime.load.gpu_layers, GpuLayerSelection::Count(0));
    if runtime.speculative.is_some() && !gpu {
        return Err(InferenceServiceError::InvalidRequest);
    }
    let model_bytes = runtime.artifact.bytes.saturating_add(
        runtime
            .adapters
            .iter()
            .map(|adapter| adapter.artifact.bytes)
            .sum(),
    );
    let context_bytes = u64::from(runtime.load.context_tokens)
        .saturating_mul(u64::from(runtime.service.slots))
        .saturating_mul(if gpu { 128 * 1024 } else { 256 * 1024 });
    let speculative_bytes = runtime
        .speculative
        .map(|selection| match selection {
            agl_core::agent::SpeculativeSelection::Mtp {
                max_draft_tokens,
                kv_cache_type_k,
                kv_cache_type_v,
            } => {
                // MTP keeps a draft KV for the admitted context, not merely
                // for the number of tokens produced in one verification step.
                // Retain max_draft_tokens in the estimate so larger draft
                // windows remain conservatively distinct.
                let width_ratio = kv_cache_bytes_per_token(kv_cache_type_k)
                    .saturating_add(kv_cache_bytes_per_token(kv_cache_type_v));
                context_bytes
                    .saturating_mul(width_ratio)
                    .saturating_div(4)
                    .saturating_add(
                        u64::from(max_draft_tokens)
                            .saturating_mul(u64::from(runtime.service.slots))
                            .saturating_mul(128 * 1024)
                            .saturating_mul(width_ratio)
                            .saturating_div(4),
                    )
            }
        })
        .unwrap_or(0);
    let selectors = serde_json::to_vec(&runtime.load.devices)
        .map_err(|_| InferenceServiceError::InvalidRequest)?;
    let mut physical_identity = b"agentlibre.physical-device.v2\0".to_vec();
    physical_identity.extend_from_slice(&selectors);
    let mut driver_identity = b"agentlibre.driver-build.v2\0".to_vec();
    driver_identity.extend_from_slice(engine_build.as_bytes());
    driver_identity.extend_from_slice(&selectors);
    let mut profile = LlamaRuntimeProfile {
        digest: RuntimeProfileDigest::from_bytes(*runtime.service.key.as_bytes()),
        physical_device: PhysicalDeviceDigest::from_bytes(hash32(&physical_identity)),
        driver_build: DriverBuildDigest::from_bytes(hash32(&driver_identity)),
        required_host_bytes: model_bytes
            .saturating_mul(if gpu { 1 } else { 2 })
            .saturating_add(context_bytes)
            .saturating_add(speculative_bytes)
            .saturating_add(1024 * 1024 * 1024),
        required_device_bytes: if gpu {
            model_bytes
                .saturating_add(context_bytes)
                .saturating_add(speculative_bytes)
        } else {
            0
        },
        required_shared_bytes: 0,
        context_tokens: runtime.load.context_tokens,
        batch_size: runtime.load.batch_size,
        ubatch_size: runtime.load.ubatch_size,
        threads: runtime.load.threads,
        gpu_layers: 0,
        speculative: runtime.speculative.is_some(),
        speculative_max_draft_tokens: runtime
            .speculative
            .map(|selection| match selection {
                agl_core::agent::SpeculativeSelection::Mtp {
                    max_draft_tokens, ..
                } => max_draft_tokens,
            })
            .unwrap_or(0),
        speculative_type_k: runtime.speculative.map(|selection| match selection {
            agl_core::agent::SpeculativeSelection::Mtp {
                kv_cache_type_k, ..
            } => kv_cache_name(kv_cache_type_k).to_owned(),
        }),
        speculative_type_v: runtime.speculative.map(|selection| match selection {
            agl_core::agent::SpeculativeSelection::Mtp {
                kv_cache_type_v, ..
            } => kv_cache_name(kv_cache_type_v).to_owned(),
        }),
        slots: runtime.service.slots,
        continuous_batching: runtime.service.continuous_batching,
        device: gpu.then(|| "Vulkan0".to_owned()),
        device_paths: if gpu { drm_device_paths() } else { Vec::new() },
        selectors: runtime.load.devices.clone(),
    };
    profile.digest = RuntimeProfileDigest::from_bytes(*runtime.service.key.as_bytes());
    profile.context_tokens = runtime.load.context_tokens;
    profile.batch_size = runtime.load.batch_size;
    profile.ubatch_size = runtime.load.ubatch_size;
    profile.threads = runtime.load.threads;
    profile.gpu_layers = match runtime.load.gpu_layers {
        // Current llama.cpp reserves -1 for automatic placement and -2 for
        // the explicit `all` selection.
        GpuLayerSelection::All => -2,
        GpuLayerSelection::Count(value) => value.min(i32::MAX as u32) as i32,
    };
    profile.slots = runtime.service.slots;
    profile.continuous_batching = runtime.service.continuous_batching;
    Ok(profile)
}

fn kv_cache_bytes_per_token(value: agl_core::agent::KvCacheType) -> u64 {
    match value {
        agl_core::agent::KvCacheType::F32 => 4,
        agl_core::agent::KvCacheType::F16 | agl_core::agent::KvCacheType::Bf16 => 2,
        agl_core::agent::KvCacheType::Q8_0 => 1,
        agl_core::agent::KvCacheType::Q5_0 | agl_core::agent::KvCacheType::Q5_1 => 1,
        agl_core::agent::KvCacheType::Q4_0
        | agl_core::agent::KvCacheType::Q4_1
        | agl_core::agent::KvCacheType::Iq4Nl => 1,
    }
}

fn drm_device_paths() -> Vec<PathBuf> {
    use std::os::unix::fs::FileTypeExt as _;

    let mut paths = fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return false;
            };
            let suffix = name
                .strip_prefix("renderD")
                .or_else(|| name.strip_prefix("card"));
            suffix.is_some_and(|suffix| {
                !suffix.is_empty()
                    && suffix.bytes().all(|byte| byte.is_ascii_digit())
                    && fs::symlink_metadata(path)
                        .is_ok_and(|metadata| metadata.file_type().is_char_device())
            })
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn hash32(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[cfg(test)]
fn compatible_profile(
    profile: &LlamaRuntimeProfile,
    model_digest: &agl_core::agent::PackageDigest,
    engine_build: &EngineBuildDigest,
    restored_health: &RestoredInferenceHealth,
) -> bool {
    profile_unavailable_reason(profile, model_digest, engine_build, restored_health).is_none()
}

fn profile_unavailable_reason(
    profile: &LlamaRuntimeProfile,
    model_digest: &agl_core::agent::PackageDigest,
    engine_build: &EngineBuildDigest,
    restored_health: &RestoredInferenceHealth,
) -> Option<String> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let available = system.available_memory();
    if profile.required_host_bytes > available {
        return Some(format!(
            "host-memory admission rejected: required {} bytes, available {} bytes",
            profile.required_host_bytes, available
        ));
    }
    if let Some(worker) = restored_health.workers.iter().find(|worker| {
        worker.physical_device == profile.physical_device
            && worker.driver_build == profile.driver_build
            && worker.engine_build == *engine_build
            && worker.retry_after_ms > unix_ms()
    }) {
        return Some(format!(
            "worker cooldown until {}: {:?}",
            worker.retry_after_ms, worker.last_failure_kind
        ));
    }
    if restored_health.quarantines.iter().any(|entry| {
        entry.physical_device == profile.physical_device
            && entry.driver_build == profile.driver_build
            && entry.engine_build == *engine_build
            && entry.model == *model_digest
            && entry.runtime_profile == profile.digest
    }) {
        return Some("runtime resource allocation is quarantined".to_owned());
    }
    None
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

struct ResidentEngine {
    execution: BlockingExecutionClient,
    execution_id: ExecutionId,
    directory: PathBuf,
    socket_path: PathBuf,
    profile: LlamaRuntimeProfile,
    reasoning_supported: AtomicBool,
    device_lost: Arc<AtomicBool>,
    diagnostic_cursor: Mutex<u64>,
    terminated: AtomicBool,
    retiring: AtomicBool,
}

enum EngineStartError {
    Service(InferenceServiceError),
    Exited(Option<ExecutionOutcome>),
    InvalidAllocation(ObservedAllocation),
    Unreleased(Box<ResidentEngine>),
}

impl From<InferenceServiceError> for EngineStartError {
    fn from(value: InferenceServiceError) -> Self {
        Self::Service(value)
    }
}

impl ResidentEngine {
    fn start(
        executable: &Path,
        registration: &RegisteredModelService,
        profile: LlamaRuntimeProfile,
        execution: BlockingExecutionClient,
    ) -> Result<Self, EngineStartError> {
        registration
            .artifact
            .verify_unchanged()
            .map_err(|_| InferenceServiceError::Unavailable)?;
        for adapter in &registration.adapters {
            adapter
                .verify_unchanged()
                .map_err(|_| InferenceServiceError::Unavailable)?;
        }
        let directory = private_directory()?;
        let socket_path = directory.join("llama.sock");
        let address_limit = address_space_limit(&profile);
        let plan_digest = profile.digest.to_string();
        let vulkan_driver_files =
            std::env::var_os("VK_DRIVER_FILES").or_else(|| std::env::var_os("VK_ICD_FILENAMES"));
        let arguments = launch_arguments(&registration.runtime, &profile)?;
        let mut environment = BTreeMap::from([
            ("AGL_INFERENCE_PLAN_DIGEST".to_owned(), plan_digest),
            ("AGL_INFERENCE_RESERVATION_ID".to_owned(), "1".to_owned()),
            ("AGL_INFERENCE_ENGINE_GENERATION".to_owned(), "1".to_owned()),
        ]);
        if let Some(driver_files) = vulkan_driver_files {
            environment.insert(
                "VK_DRIVER_FILES".to_owned(),
                driver_files.to_string_lossy().into_owned(),
            );
        }
        let mut argv = Vec::with_capacity(arguments.len() + 1);
        argv.push(executable.to_string_lossy().into_owned());
        argv.extend(arguments);
        let artifacts = std::iter::once(registration.artifact.path())
            .chain(registration.adapters.iter().map(|adapter| adapter.path()))
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let status = execution
            .start(ExecutionStartRequest {
                owner: ExecutionOwner::Runtime {
                    component: "inference".to_owned(),
                },
                argv,
                cwd: directory.to_string_lossy().into_owned(),
                environment,
                clear_environment: true,
                io: ExecutionIo::Pipes,
                timeout_ms: 0,
                max_output_bytes: 4 * 1024 * 1024,
                terminal_size: None,
                isolation: ExecutionIsolation::PrivateInference {
                    artifacts,
                    listen_socket: socket_path.to_string_lossy().into_owned(),
                    device_paths: profile
                        .device_paths
                        .iter()
                        .map(|path| path.to_string_lossy().into_owned())
                        .collect(),
                    address_space_limit_bytes: address_limit,
                },
            })
            .map_err(|_| InferenceServiceError::Unavailable)?;
        let device_lost = Arc::new(AtomicBool::new(false));
        let engine = Self {
            execution,
            execution_id: status.execution_id,
            directory,
            socket_path,
            profile,
            reasoning_supported: AtomicBool::new(false),
            device_lost,
            diagnostic_cursor: Mutex::new(0),
            terminated: AtomicBool::new(false),
            retiring: AtomicBool::new(false),
        };
        if let Err(error) = engine.wait_ready() {
            if engine.terminate().is_err() {
                return Err(EngineStartError::Unreleased(Box::new(engine)));
            }
            return Err(error);
        }
        Ok(engine)
    }

    fn wait_ready(&self) -> Result<(), EngineStartError> {
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            let status = self.execution_record()?;
            if status.state != ExecutionState::Running {
                self.drain_diagnostics();
                tracing::warn!(outcome=?status.outcome, "private llama-server exited before readiness");
                return Err(EngineStartError::Exited(status.outcome));
            }
            if let Ok(response) = http_request(&self.socket_path, "GET", "/agl/v1/readiness", None)
                && response.status == 200
            {
                let readiness: Readiness =
                    serde_json::from_slice(&response.body).map_err(|error| {
                        tracing::warn!(%error, "private llama-server returned invalid readiness");
                        InferenceServiceError::UnavailableWithReason(
                            "private llama-server returned invalid readiness JSON".to_owned(),
                        )
                    })?;
                if readiness_matches_profile(&readiness, &self.profile)? {
                    validate_allocation(&self.profile, &readiness.memory)?;
                    self.reasoning_supported
                        .store(readiness.reasoning_supported, Ordering::Release);
                    return Ok(());
                }
                tracing::warn!(
                    schema=%readiness.schema,
                    plan_digest=%readiness.plan_digest,
                    reservation_id=%readiness.reservation_id,
                    engine_generation=%readiness.engine_generation,
                    context_tokens=readiness.context_tokens,
                    batch_size=readiness.batch_size,
                    ubatch_size=readiness.ubatch_size,
                    slot_count=readiness.slot_count,
                    speculative_enabled=readiness.speculative.enabled,
                    speculative_kind=%readiness.speculative.kind,
                    speculative_max_draft_tokens=readiness.speculative.max_draft_tokens,
                    speculative_gpu_layers=readiness.speculative.gpu_layers,
                    speculative_key_cache_type=%readiness.speculative.key_cache_type,
                    speculative_value_cache_type=%readiness.speculative.value_cache_type,
                    "private llama-server readiness does not match the admitted profile"
                );
                return Err(InferenceServiceError::UnavailableWithReason(
                    "private llama-server readiness does not match the admitted profile".to_owned(),
                )
                .into());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err(InferenceServiceError::UnavailableWithReason(
            "private llama-server did not become ready within 120 seconds".to_owned(),
        )
        .into())
    }

    fn generate(
        &self,
        request: &InferenceGenerateRequest,
        rendered: &crate::inference::request_codec::RenderedModelRequest,
    ) -> Result<Generated, InferenceServiceError> {
        if self.retiring.load(Ordering::Acquire) {
            return Err(InferenceServiceError::OutcomeUnknown);
        }
        let reasoning_enabled = validate_reasoning_capability(
            request.runtime.reasoning,
            self.reasoning_supported.load(Ordering::Acquire),
        )?;
        if self.execution_status()? != ExecutionState::Running {
            return Err(self.unavailable_error());
        }
        let attempt = format!(
            "{}:{}:{}",
            request.operation.run_id, request.operation.ordinal, request.delivery_attempt
        );
        let body = native_request(rendered, &attempt, request)?;
        validate_context_capacity(
            &self.socket_path,
            &body,
            rendered.max_output_tokens,
            self.profile.context_tokens,
        )?;
        let response = generation_request(
            &self.socket_path,
            &body,
            &request.cancellation,
            &attempt,
            request.progress.clone(),
            reasoning_enabled,
        )
        .map_err(|error| self.classify_transport_error(error))?;
        if response.status != 200 {
            tracing::warn!(
                attempt_id=%attempt,
                backend_status=response.status,
                backend_diagnostic_bytes=response.diagnostic.as_ref().map(String::len),
                "private llama-server rejected generation"
            );
            return Err(if (400..500).contains(&response.status) {
                InferenceServiceError::InvalidRequest
            } else {
                InferenceServiceError::UnavailableWithReason(format!(
                    "llama-server rejected generation with HTTP status {}",
                    response.status
                ))
            });
        }
        let generated = response
            .generated
            .ok_or(InferenceServiceError::OutcomeUnknown)?;
        tracing::info!(
            attempt_id=%attempt,
            prefill_tokens=generated.timings.prompt_n,
            prefill_cached_tokens=generated.timings.cache_n,
            prefill_ms=generated.timings.prompt_ms,
            prefill_tokens_per_second=generated.timings.prompt_per_second,
            decode_tokens=generated.timings.predicted_n,
            decode_ms=generated.timings.predicted_ms,
            decode_tokens_per_second=generated.timings.predicted_per_second,
            draft_tokens=generated.timings.draft_n,
            draft_tokens_accepted=generated.timings.draft_n_accepted,
            "private llama-server model call timings"
        );
        Ok(generated)
    }

    fn terminate(&self) -> Result<(), InferenceServiceError> {
        if self.terminated.load(Ordering::Acquire) {
            return Ok(());
        }
        self.retiring.store(true, Ordering::Release);
        let _ = self
            .execution
            .signal(self.execution_id, ExecutionSignal::Terminate);
        for _ in 0..200 {
            match self.execution_status() {
                Ok(ExecutionState::Exited) => {
                    self.terminated.store(true, Ordering::Release);
                    self.drain_diagnostics();
                    let _ = fs::remove_dir_all(&self.directory);
                    return Ok(());
                }
                Ok(ExecutionState::Running) => {}
                Ok(ExecutionState::OutcomeUnknown) | Err(_) => {
                    return Err(InferenceServiceError::OutcomeUnknown);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.drain_diagnostics();
        Err(InferenceServiceError::OutcomeUnknown)
    }

    fn unavailable_error(&self) -> InferenceServiceError {
        self.drain_diagnostics();
        if self.device_lost.load(Ordering::Acquire) {
            InferenceServiceError::DeviceLost
        } else {
            InferenceServiceError::UnavailableWithReason(
                "private llama-server execution is no longer running".to_owned(),
            )
        }
    }

    fn classify_transport_error(&self, error: InferenceServiceError) -> InferenceServiceError {
        if matches!(
            error,
            InferenceServiceError::Unavailable
                | InferenceServiceError::UnavailableWithReason(_)
                | InferenceServiceError::OutcomeUnknown
        ) && self.execution_status().ok() != Some(ExecutionState::Running)
        {
            self.unavailable_error()
        } else {
            error
        }
    }

    fn drain_diagnostics(&self) {
        let Ok(mut cursor) = self.diagnostic_cursor.lock() else {
            return;
        };
        loop {
            let Ok(output) = self.execution.read(
                self.execution_id,
                *cursor,
                agl_execution_api::MAX_EXECUTION_OUTPUT_READ_BYTES,
            ) else {
                return;
            };
            for chunk in output
                .chunks
                .iter()
                .filter(|chunk| chunk.stream == ExecutionOutputStream::Stderr)
            {
                for line in String::from_utf8_lossy(&chunk.data).lines() {
                    if engine_diagnostic_is_device_lost(line) {
                        self.device_lost.store(true, Ordering::Release);
                    }
                    tracing::debug!(
                        target: "agl_inference_engine",
                        execution_id = %self.execution_id,
                        diagnostic_bytes = line.len(),
                        "private llama-server diagnostic"
                    );
                }
            }
            *cursor = output.next;
            if output.eof || output.chunks.is_empty() {
                break;
            }
        }
    }

    fn execution_status(&self) -> Result<ExecutionState, InferenceServiceError> {
        Ok(self.execution_record()?.state)
    }

    fn execution_record(
        &self,
    ) -> Result<agl_execution_api::ExecutionStatus, InferenceServiceError> {
        let mut status = self
            .execution
            .inspect(self.execution_id)
            .map_err(|_| InferenceServiceError::OutcomeUnknown)?;
        if matches!(
            status.outcome,
            Some(ExecutionOutcome::UnknownAfterServiceRestart)
        ) {
            status.state = ExecutionState::OutcomeUnknown;
        }
        Ok(status)
    }
}

fn validate_context_capacity(
    socket_path: &Path,
    generation_body: &[u8],
    max_output_tokens: u64,
    context_tokens: u32,
) -> Result<(), InferenceServiceError> {
    let capacity = measure_context_capacity(
        socket_path,
        generation_body,
        max_output_tokens,
        context_tokens,
    )?;
    ensure_context_capacity(
        capacity.prompt_tokens,
        capacity.reserved_output_tokens,
        capacity.context_capacity_tokens,
    )
}

fn measure_context_capacity(
    socket_path: &Path,
    generation_body: &[u8],
    max_output_tokens: u64,
    context_tokens: u32,
) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError> {
    let tokenized = http_request(
        socket_path,
        "POST",
        "/agl/v1/input-tokens",
        Some(generation_body),
    )?;
    if tokenized.status != 200 {
        let (error, code, diagnostic) = classify_tokenizer_error(
            tokenized.status,
            &tokenized.body,
            max_output_tokens,
            context_tokens,
        );
        tracing::warn!(
            endpoint = "/agl/v1/input-tokens",
            status = tokenized.status,
            backend_code = code,
            diagnostic_bytes = diagnostic.len(),
            response_bytes = tokenized.body.len(),
            response_digest = %PackageDigest::from_bytes(Sha256::digest(&tokenized.body).into()),
            "private llama-server context preflight failed"
        );
        return Err(error);
    }
    let tokenized: Value = serde_json::from_slice(&tokenized.body)
        .map_err(|_| InferenceServiceError::InvalidResult)?;
    let prompt_tokens = tokenized
        .get("input_tokens")
        .and_then(Value::as_u64)
        .ok_or(InferenceServiceError::InvalidResult)?;
    Ok(agl_core::agent::ContextCapacity::new(
        prompt_tokens,
        max_output_tokens,
        context_tokens,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BackendErrorEnvelope {
    error: BackendError,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BackendError {
    code: u16,
    message: String,
    #[serde(rename = "type")]
    kind: String,
}

fn classify_tokenizer_error(
    status: u16,
    body: &[u8],
    max_output_tokens: u64,
    context_tokens: u32,
) -> (InferenceServiceError, &'static str, String) {
    const DIAGNOSTIC_CHARS: usize = 512;
    let Ok(envelope) = serde_json::from_slice::<BackendErrorEnvelope>(body) else {
        return (
            InferenceServiceError::InvalidResult,
            "malformed_error_envelope",
            "backend returned a malformed error envelope".to_owned(),
        );
    };
    let diagnostic = envelope
        .error
        .message
        .chars()
        .take(DIAGNOSTIC_CHARS)
        .collect::<String>();
    if envelope.error.code != status {
        return (
            InferenceServiceError::InvalidResult,
            "status_mismatch",
            diagnostic,
        );
    }
    let (error, code) = match envelope.error.kind.as_str() {
        "exceed_context_size_error" => (
            InferenceServiceError::ContextExhausted(agl_core::agent::ContextCapacity::new(
                u64::from(context_tokens).saturating_add(1),
                max_output_tokens,
                context_tokens,
            )),
            "context_exhausted",
        ),
        "invalid_request_error" => (InferenceServiceError::InvalidRequest, "invalid_request"),
        "unavailable_error" | "server_error" => (
            InferenceServiceError::UnavailableWithReason(
                "llama-server reported an unavailable backend".to_owned(),
            ),
            "backend_unavailable",
        ),
        _ => (
            InferenceServiceError::InvalidResult,
            "unknown_backend_error",
        ),
    };
    (error, code, diagnostic)
}

fn ensure_context_capacity(
    prompt_tokens: u64,
    max_output_tokens: u64,
    context_tokens: u32,
) -> Result<(), InferenceServiceError> {
    let capacity =
        agl_core::agent::ContextCapacity::new(prompt_tokens, max_output_tokens, context_tokens);
    if !capacity.fits() {
        return Err(InferenceServiceError::ContextExhausted(capacity));
    }
    Ok(())
}

impl Drop for ResidentEngine {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            tracing::error!(execution_id=%self.execution_id, ?error, "private engine release is unconfirmed");
        }
    }
}

fn worker_failure_kind(
    error: &InferenceServiceError,
    outcome: Option<ExecutionOutcome>,
) -> Option<InferenceFailureKind> {
    match error {
        InferenceServiceError::DeviceLost => Some(InferenceFailureKind::DeviceLost),
        InferenceServiceError::Unavailable | InferenceServiceError::UnavailableWithReason(_) => {
            Some(match outcome {
                Some(ExecutionOutcome::Signal { signal }) => {
                    InferenceFailureKind::UnattributedSignal { signal }
                }
                _ => InferenceFailureKind::EngineCrash,
            })
        }
        // The execution outcome is deliberately unknown after a service
        // restart or an unconfirmed cancellation.  Do not turn that loss of
        // evidence into an engine-crash health record and impose a cooldown
        // on an otherwise usable worker.
        InferenceServiceError::OutcomeUnknown => match outcome {
            Some(ExecutionOutcome::Signal { signal }) => {
                Some(InferenceFailureKind::UnattributedSignal { signal })
            }
            _ => None,
        },
        _ => None,
    }
}

fn error_requires_engine_restart(error: &InferenceServiceError) -> bool {
    matches!(
        error,
        InferenceServiceError::InvalidResult
            | InferenceServiceError::Unavailable
            | InferenceServiceError::UnavailableWithReason(_)
            | InferenceServiceError::DeviceLost
            | InferenceServiceError::OutcomeUnknown
            | InferenceServiceError::IdentityMismatch
    )
}

fn engine_diagnostic_is_device_lost(line: &str) -> bool {
    // These are the stable symbolic forms emitted by Vulkan-Hpp and the C
    // Vulkan API. Matching the exact backend token avoids classifying an
    // arbitrary diagnostic sentence as a device-loss event.
    line.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .any(|token| matches!(token, "ErrorDeviceLost" | "VK_ERROR_DEVICE_LOST"))
}

fn engine_context_tokens(profile: &LlamaRuntimeProfile) -> Result<u32, InferenceServiceError> {
    profile
        .context_tokens
        .checked_mul(profile.slots)
        .ok_or(InferenceServiceError::InvalidRequest)
}

fn launch_arguments(
    runtime: &ModelRuntimeSelection,
    profile: &LlamaRuntimeProfile,
) -> Result<Vec<String>, InferenceServiceError> {
    let mut args = vec![
        "--model".into(),
        format!("/proc/self/fd/{MODEL_FD}"),
        "--ctx-size".into(),
        engine_context_tokens(profile)?.to_string(),
        "--batch-size".into(),
        profile.batch_size.to_string(),
        "--ubatch-size".into(),
        profile.ubatch_size.to_string(),
        "--threads".into(),
        profile.threads.to_string(),
        "--threads-batch".into(),
        runtime.load.threads_batch.to_string(),
        "--gpu-layers".into(),
        profile.gpu_layers.to_string(),
        "--parallel".into(),
        runtime.service.slots.to_string(),
        "--no-context-shift".into(),
        "--no-warmup".into(),
        "--no-ui".into(),
        "--jinja".into(),
        "--reasoning".into(),
        "off".into(),
        "--reasoning-format".into(),
        "none".into(),
        "--log-verbosity".into(),
        "1".into(),
    ];
    if let Some(agl_core::agent::SpeculativeSelection::Mtp {
        max_draft_tokens,
        kv_cache_type_k,
        kv_cache_type_v,
    }) = runtime.speculative
    {
        let device = profile
            .device
            .as_deref()
            .ok_or(InferenceServiceError::InvalidRequest)?;
        args.extend([
            "--spec-type".into(),
            "draft-mtp".into(),
            "--spec-draft-device".into(),
            device.into(),
            "--spec-draft-ngl".into(),
            "all".into(),
            "--spec-draft-n-max".into(),
            max_draft_tokens.to_string(),
            "--spec-draft-type-k".into(),
            kv_cache_name(kv_cache_type_k).into(),
            "--spec-draft-type-v".into(),
            kv_cache_name(kv_cache_type_v).into(),
        ]);
    }
    if !runtime.service.continuous_batching {
        args.push("--no-cont-batching".into());
    }
    if !runtime.load.mmap {
        args.push("--no-mmap".into());
    }
    if runtime.load.mlock {
        args.push("--mlock".into());
    }
    args.extend([
        "--flash-attn".into(),
        if runtime.load.flash_attention {
            "on"
        } else {
            "off"
        }
        .into(),
        "--cache-type-k".into(),
        kv_cache_name(runtime.load.kv_cache_type_k).into(),
        "--cache-type-v".into(),
        kv_cache_name(runtime.load.kv_cache_type_v).into(),
        "--split-mode".into(),
        match runtime.load.split_mode {
            agl_core::agent::SplitMode::None => "none",
            agl_core::agent::SplitMode::Layer => "layer",
            agl_core::agent::SplitMode::Row => "row",
        }
        .into(),
    ]);
    if let Some(main_gpu) = runtime.load.main_gpu {
        args.extend(["--main-gpu".into(), main_gpu.to_string()]);
    }
    if !runtime.load.tensor_split.is_empty() {
        args.extend([
            "--tensor-split".into(),
            runtime
                .load
                .tensor_split
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
        ]);
    }
    for (index, adapter) in runtime.adapters.iter().enumerate() {
        args.extend([
            "--lora-scaled".into(),
            format!("/proc/self/fd/{}", FIRST_ADAPTER_FD + index as i32),
            adapter.scale.to_string(),
        ]);
    }
    if let Some(device) = &profile.device {
        args.extend(["--device".into(), device.clone()]);
    } else {
        args.extend(["--device".into(), "none".into()]);
    }
    Ok(args)
}

fn kv_cache_name(value: agl_core::agent::KvCacheType) -> &'static str {
    match value {
        agl_core::agent::KvCacheType::F32 => "f32",
        agl_core::agent::KvCacheType::F16 => "f16",
        agl_core::agent::KvCacheType::Bf16 => "bf16",
        agl_core::agent::KvCacheType::Q8_0 => "q8_0",
        agl_core::agent::KvCacheType::Q5_0 => "q5_0",
        agl_core::agent::KvCacheType::Q5_1 => "q5_1",
        agl_core::agent::KvCacheType::Q4_0 => "q4_0",
        agl_core::agent::KvCacheType::Q4_1 => "q4_1",
        agl_core::agent::KvCacheType::Iq4Nl => "iq4_nl",
    }
}

fn readiness_matches_profile(
    readiness: &Readiness,
    profile: &LlamaRuntimeProfile,
) -> Result<bool, InferenceServiceError> {
    Ok(readiness.schema == "agentlibre.llama-readiness/v1"
        && readiness.plan_digest == profile.digest.to_string()
        && readiness.reservation_id == "1"
        && readiness.engine_generation == "1"
        && readiness.context_tokens == engine_context_tokens(profile)?
        && readiness.batch_size == profile.batch_size
        && readiness.ubatch_size == profile.ubatch_size
        && readiness.slot_count == profile.slots
        && readiness.speculative.enabled == profile.speculative
        && (!profile.speculative
            || (readiness.speculative.kind == "none,draft-mtp"
                && readiness.speculative.max_draft_tokens == profile.speculative_max_draft_tokens
                && readiness.speculative.gpu_layers == -2
                && profile.speculative_type_k.as_deref()
                    == Some(readiness.speculative.key_cache_type.as_str())
                && profile.speculative_type_v.as_deref()
                    == Some(readiness.speculative.value_cache_type.as_str()))))
}

fn native_request(
    request: &crate::inference::request_codec::RenderedModelRequest,
    attempt: &str,
    source: &InferenceGenerateRequest,
) -> Result<Vec<u8>, InferenceServiceError> {
    let mut messages = request
        .messages
        .iter()
        .map(|message| {
            let role = match message.role {
                RenderedMessageRole::System => "system",
                RenderedMessageRole::User => "user",
                RenderedMessageRole::Assistant => "assistant",
                RenderedMessageRole::Tool => "tool",
            };
            let content = message
                .content
                .as_ref()
                .map(|content| content.as_text().to_owned());
            let mut value = json!({"role": role, "content": content});
            if let Some(name) = &message.name {
                value["name"] = json!(name);
            }
            if let Some(reasoning) = &message.private_reasoning {
                value["reasoning_content"] = json!(reasoning.as_text());
            }
            if let Some(call) = &message.tool_call {
                value["tool_calls"] = json!([{"id":"call_0","type":"function","function":{
                    "name":call.name,"arguments":call.arguments.to_string()
                }}]);
            } else if !message.tool_calls.is_empty() {
                value["tool_calls"] = json!(
                    message
                        .tool_calls
                        .iter()
                        .enumerate()
                        .map(|(index, call)| json!({
                            "id": format!("call_{index}"),
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": call.arguments.to_string()
                            }
                        }))
                        .collect::<Vec<_>>()
                );
            }
            Ok(value)
        })
        .collect::<Result<Vec<_>, InferenceServiceError>>()?;
    let after_compaction = source.context.last().is_some_and(|entry| {
        matches!(
            entry.source_request,
            Some(agl_core::agent::AgentOperationRequest::Compaction(_))
        )
    });
    append_user_turn_after_assistant(&mut messages, after_compaction);
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let parameters = llama_tool_schema(tool.input_schema.clone());
            json!({
                "type":"function","function":{"name":tool.name,"description":tool.description,
                "parameters":parameters}
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "agl_attempt_id": attempt,
        "messages": messages,
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "stream": true,
        "seed": source.runtime.generation.seed,
        "temperature": source.runtime.generation.temperature,
        "top_k": source.runtime.generation.top_k,
        "top_p": source.runtime.generation.top_p,
        "min_p": source.runtime.generation.min_p,
        "typical_p": source.runtime.generation.typical_p,
        "repeat_last_n": source.runtime.generation.repeat_last_n,
        "repeat_penalty": source.runtime.generation.repeat_penalty,
        "presence_penalty": source.runtime.generation.presence_penalty,
        "frequency_penalty": source.runtime.generation.frequency_penalty,
        "stop": source.runtime.generation.stop,
        "max_tokens": request.max_output_tokens.min(source.runtime.generation.max_output_tokens),
        "id_slot": -1,
        "cache_prompt": true
    });
    apply_reasoning_request(&mut body, source.runtime.reasoning);
    if let Some(response_format) = &request.response_format {
        body["response_format"] = response_format.clone();
    }
    serde_json::to_vec(&body).map_err(|_| InferenceServiceError::InvalidRequest)
}

fn append_user_turn_after_assistant(messages: &mut Vec<Value>, after_compaction: bool) {
    if messages
        .last()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        == Some("assistant")
    {
        let content = if after_compaction {
            "Continue the task preserved in the compaction summary, starting from the concrete next step in semantic.next_position. Use exact.inspected_files and semantic.completed as the work ledger; do not repeat completed investigation unless a file digest changed."
        } else {
            ""
        };
        messages.push(json!({"role": "user", "content": content}));
    }
}

fn llama_tool_schema(mut value: Value) -> Value {
    if let Value::Object(object) = &mut value {
        project_root_object_union(object);
    }
    relax_llama_repetition_bounds(&mut value);
    value
}

fn relax_llama_repetition_bounds(value: &mut Value) {
    const MAX_GBNF_REPETITION: u64 = 2_000;

    match value {
        Value::Object(object) => {
            for keyword in ["maxLength", "maxItems"] {
                if object
                    .get(keyword)
                    .and_then(Value::as_u64)
                    .is_some_and(|bound| bound > MAX_GBNF_REPETITION)
                {
                    object.remove(keyword);
                }
            }
            for child in object.values_mut() {
                relax_llama_repetition_bounds(child);
            }
        }
        Value::Array(array) => {
            for child in array {
                relax_llama_repetition_bounds(child);
            }
        }
        _ => {}
    }
}

fn project_root_object_union(object: &mut serde_json::Map<String, Value>) {
    if object.contains_key("properties") {
        return;
    }
    let Some(branches) = object
        .remove("oneOf")
        .and_then(|value| value.as_array().cloned())
    else {
        return;
    };
    if branches.is_empty()
        || branches.iter().any(|branch| {
            branch.get("type").and_then(Value::as_str) != Some("object")
                || !branch.get("properties").is_some_and(Value::is_object)
        })
    {
        object.insert("oneOf".to_owned(), Value::Array(branches));
        return;
    }
    let mut properties = serde_json::Map::new();
    let mut required: Option<std::collections::BTreeSet<String>> = None;
    for branch in &branches {
        for (name, schema) in branch["properties"].as_object().expect("checked above") {
            match properties.get_mut(name) {
                None => {
                    properties.insert(name.clone(), schema.clone());
                }
                Some(existing) if existing == schema => {}
                Some(existing) => {
                    let previous = existing.take();
                    *existing = serde_json::json!({"anyOf": [previous, schema]});
                }
            }
        }
        let branch_required = branch
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>();
        required = Some(match required {
            None => branch_required,
            Some(current) => current.intersection(&branch_required).cloned().collect(),
        });
    }
    object.insert("type".to_owned(), Value::String("object".to_owned()));
    object.insert("additionalProperties".to_owned(), Value::Bool(false));
    object.insert("properties".to_owned(), Value::Object(properties));
    object.insert(
        "required".to_owned(),
        Value::Array(
            required
                .unwrap_or_default()
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
    );
}

fn apply_reasoning_request(body: &mut Value, reasoning: agl_core::agent::ReasoningSelection) {
    match reasoning {
        agl_core::agent::ReasoningSelection::Disabled => {
            body["chat_template_kwargs"] = json!({"enable_thinking": false});
            body["reasoning_format"] = json!("none");
        }
        agl_core::agent::ReasoningSelection::Enabled {
            max_tokens,
            effort,
            preserve,
        } => {
            let mut kwargs = json!({
                "enable_thinking": true,
                "preserve_thinking": preserve,
            });
            if let Some(effort) = effort {
                kwargs["reasoning_effort"] = json!(match effort {
                    agl_core::agent::ReasoningEffort::Low => "low",
                    agl_core::agent::ReasoningEffort::Medium => "medium",
                    agl_core::agent::ReasoningEffort::Xhigh => "xhigh",
                });
            }
            body["chat_template_kwargs"] = kwargs;
            body["reasoning_format"] = json!("deepseek");
            body["thinking_budget_tokens"] = json!(max_tokens);
        }
    }
}

fn validate_reasoning_capability(
    reasoning: agl_core::agent::ReasoningSelection,
    supported: bool,
) -> Result<bool, InferenceServiceError> {
    let enabled = matches!(
        reasoning,
        agl_core::agent::ReasoningSelection::Enabled { .. }
    );
    if enabled && !supported {
        return Err(InferenceServiceError::InvalidRequest);
    }
    Ok(enabled)
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn http_request(
    path: &Path,
    method: &str,
    route: &str,
    body: Option<&[u8]>,
) -> Result<HttpResponse, InferenceServiceError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|_| InferenceServiceError::Unavailable)?;
    runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(5),
            hyper_request(path, method, route, body.unwrap_or_default(), None),
        )
        .await
        .map_err(|_| InferenceServiceError::OutcomeUnknown)?
    })
}

async fn hyper_request(
    path: &Path,
    method: &str,
    route: &str,
    body: &[u8],
    private_attempt: Option<&str>,
) -> Result<HttpResponse, InferenceServiceError> {
    let (mut sender, connection) = hyper_connection(path).await?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::warn!(%error, "private llama-server HTTP connection failed");
        }
    });
    let mut builder = Request::builder()
        .method(
            method
                .parse::<Method>()
                .map_err(|_| InferenceServiceError::InvalidRequest)?,
        )
        .uri(route)
        .header("host", "agentlibre.internal")
        .header("content-type", "application/json");
    if let Some(attempt) = private_attempt {
        builder = builder
            .header("x-agl-protocol", "1")
            .header("x-agl-attempt-id", attempt);
    }
    let request = builder
        .body(Full::new(Bytes::copy_from_slice(body)))
        .map_err(|_| InferenceServiceError::InvalidRequest)?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|_| InferenceServiceError::OutcomeUnknown)?;
    let status = response.status().as_u16();
    let mut incoming = response.into_body();
    let mut body = Vec::new();
    while let Some(frame) = incoming.frame().await {
        let data = frame
            .map_err(|_| InferenceServiceError::OutcomeUnknown)?
            .into_data()
            .map_err(|_| InferenceServiceError::InvalidResult)?;
        if body.len().saturating_add(data.len()) > MAX_RESPONSE_BYTES {
            return Err(InferenceServiceError::InvalidResult);
        }
        body.extend_from_slice(&data);
    }
    Ok(HttpResponse { status, body })
}

async fn hyper_connection(
    path: &Path,
) -> Result<
    (
        hyper::client::conn::http1::SendRequest<Full<Bytes>>,
        hyper::client::conn::http1::Connection<TokioIo<tokio::net::UnixStream>, Full<Bytes>>,
    ),
    InferenceServiceError,
> {
    let stream = tokio::net::UnixStream::connect(path)
        .await
        .map_err(|_| InferenceServiceError::Unavailable)?;
    hyper::client::conn::http1::Builder::new()
        .max_buf_size(MAX_HEADER_BYTES)
        .handshake(TokioIo::new(stream))
        .await
        .map_err(|_| InferenceServiceError::Unavailable)
}

struct HttpGenerationResponse {
    status: u16,
    generated: Option<Generated>,
    diagnostic: Option<String>,
}

fn generation_request(
    path: &Path,
    body: &[u8],
    cancellation: &InferenceCancellation,
    attempt: &str,
    progress: Option<InferenceProgressSink>,
    reasoning_enabled: bool,
) -> Result<HttpGenerationResponse, InferenceServiceError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|_| InferenceServiceError::Unavailable)?;
    runtime.block_on(generation_request_async(
        path,
        body,
        cancellation,
        attempt,
        progress,
        reasoning_enabled,
    ))
}

async fn generation_request_async(
    path: &Path,
    body: &[u8],
    cancellation: &InferenceCancellation,
    attempt: &str,
    progress: Option<InferenceProgressSink>,
    reasoning_enabled: bool,
) -> Result<HttpGenerationResponse, InferenceServiceError> {
    let (mut sender, connection) = hyper_connection(path).await?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::warn!(%error, "private llama-server generation connection failed");
        }
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri("/agl/v1/generate")
        .header("host", "agentlibre.internal")
        .header("content-type", "application/json")
        .header("x-agl-protocol", "1")
        .header("x-agl-attempt-id", attempt)
        .body(Full::new(Bytes::copy_from_slice(body)))
        .map_err(|_| InferenceServiceError::InvalidRequest)?;
    let response = sender.send_request(request);
    tokio::pin!(response);
    let mut deadline = tokio::time::Instant::now() + GENERATION_RESPONSE_TIMEOUT;
    let mut cancellation_requested = false;
    let response = loop {
        tokio::select! {
            result = &mut response => {
                break result.map_err(|error| {
                    tracing::warn!(attempt_id=%attempt, %error, "private llama-server rejected the HTTP exchange");
                    InferenceServiceError::OutcomeUnknown
                })?;
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if !cancellation_requested && cancellation.is_cancelled() {
                    tracing::warn!(attempt_id=%attempt, "sending cancellation to private llama-server before response headers");
                    request_cancel_async(path, attempt).await?;
                    cancellation_requested = true;
                    deadline = tokio::time::Instant::now() + CANCELLATION_CONFIRMATION_TIMEOUT;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(InferenceServiceError::OutcomeUnknown);
                }
            }
        }
    };
    let status = response.status().as_u16();
    let mut incoming = response.into_body();
    if status != 200 {
        let mut body = Vec::new();
        while let Some(frame) = incoming.frame().await {
            let data = frame
                .map_err(|_| InferenceServiceError::OutcomeUnknown)?
                .into_data()
                .map_err(|_| InferenceServiceError::InvalidResult)?;
            if body.len().saturating_add(data.len()) > MAX_HEADER_BYTES {
                return Err(InferenceServiceError::InvalidResult);
            }
            body.extend_from_slice(&data);
        }
        return Ok(HttpGenerationResponse {
            status,
            generated: None,
            diagnostic: Some(bounded_backend_diagnostic(&body)),
        });
    }

    let mut decoder = GenerationStreamDecoder::new(attempt, progress, reasoning_enabled);
    deadline = tokio::time::Instant::now() + GENERATION_RESPONSE_TIMEOUT;
    let mut received = 0_usize;
    loop {
        if !cancellation_requested && cancellation.is_cancelled() {
            tracing::warn!(attempt_id=%attempt, "sending cancellation to private llama-server during response stream");
            request_cancel_async(path, attempt).await?;
            cancellation_requested = true;
            deadline = tokio::time::Instant::now() + CANCELLATION_CONFIRMATION_TIMEOUT;
        }
        let wait = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame =
            tokio::time::timeout(wait.min(Duration::from_millis(50)), incoming.frame()).await;
        match frame {
            Ok(Some(Ok(frame))) => {
                let data = frame
                    .into_data()
                    .map_err(|_| InferenceServiceError::InvalidResult)?;
                received = received.saturating_add(data.len());
                if received > MAX_RESPONSE_BYTES {
                    return Err(InferenceServiceError::InvalidResult);
                }
                decoder.feed(&data)?;
            }
            Ok(Some(Err(error))) => {
                tracing::warn!(attempt_id=%attempt, %error, "private llama-server response stream failed");
                return Err(InferenceServiceError::OutcomeUnknown);
            }
            Ok(None) => break,
            Err(_) if tokio::time::Instant::now() >= deadline => {
                return Err(InferenceServiceError::OutcomeUnknown);
            }
            Err(_) => continue,
        }
    }
    Ok(HttpGenerationResponse {
        status,
        generated: Some(decoder.finish(cancellation_requested)?),
        diagnostic: None,
    })
}

fn bounded_backend_diagnostic(body: &[u8]) -> String {
    String::from_utf8_lossy(body)
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .take(512)
        .collect::<String>()
}

async fn request_cancel_async(path: &Path, attempt: &str) -> Result<(), InferenceServiceError> {
    let body = serde_json::to_vec(&json!({
        "attempt_id": attempt,
        "action": "cancel"
    }))
    .map_err(|_| InferenceServiceError::OutcomeUnknown)?;
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        hyper_request(path, "POST", "/agl/v1/control", &body, None),
    )
    .await
    .map_err(|_| InferenceServiceError::OutcomeUnknown)?
    .map_err(|_| InferenceServiceError::OutcomeUnknown)?;
    if response.status != 200 {
        return Err(InferenceServiceError::OutcomeUnknown);
    }
    let value: Value = serde_json::from_slice(&response.body)
        .map_err(|_| InferenceServiceError::OutcomeUnknown)?;
    if value.get("schema").and_then(Value::as_str) != Some("agentlibre.llama-cancel/v1")
        || value.get("attempt_id").and_then(Value::as_str) != Some(attempt)
        || value.get("acknowledged").and_then(Value::as_bool) != Some(true)
    {
        return Err(InferenceServiceError::OutcomeUnknown);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Readiness {
    schema: String,
    plan_digest: String,
    reservation_id: String,
    engine_generation: String,
    context_tokens: u32,
    batch_size: u32,
    ubatch_size: u32,
    slot_count: u32,
    reasoning_supported: bool,
    speculative: ReadinessSpeculative,
    memory: Vec<MemoryAllocation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadinessSpeculative {
    enabled: bool,
    kind: String,
    max_draft_tokens: u32,
    #[serde(rename = "min_draft_tokens")]
    _min_draft_tokens: u32,
    #[serde(rename = "p_min_millionths")]
    _p_min_millionths: u32,
    gpu_layers: i32,
    key_cache_type: String,
    value_cache_type: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryAllocation {
    pool: String,
    device: String,
    model_bytes: u64,
    context_bytes: u64,
    compute_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ObservedAllocation {
    host: u64,
    device: u64,
    shared: u64,
}

fn validate_allocation(
    profile: &LlamaRuntimeProfile,
    allocations: &[MemoryAllocation],
) -> Result<(), EngineStartError> {
    if allocations.is_empty() {
        return Err(InferenceServiceError::InvalidRequest.into());
    }
    let mut host = 0_u64;
    let mut device = 0_u64;
    let mut shared = 0_u64;
    for allocation in allocations {
        let bytes = allocation
            .model_bytes
            .checked_add(allocation.context_bytes)
            .and_then(|bytes| bytes.checked_add(allocation.compute_bytes))
            .ok_or(InferenceServiceError::InvalidRequest)?;
        match allocation.pool.as_str() {
            "host" => {
                host = host
                    .checked_add(bytes)
                    .ok_or(InferenceServiceError::InvalidRequest)?
            }
            "device"
                if profile
                    .device
                    .as_ref()
                    .is_some_and(|expected| expected == &allocation.device) =>
            {
                device = device
                    .checked_add(bytes)
                    .ok_or(InferenceServiceError::InvalidRequest)?;
            }
            "shared" => {
                shared = shared
                    .checked_add(bytes)
                    .ok_or(InferenceServiceError::InvalidRequest)?;
            }
            _ => return Err(InferenceServiceError::InvalidRequest.into()),
        }
    }
    let observed = ObservedAllocation {
        host,
        device,
        shared,
    };
    if host > profile.required_host_bytes
        || device > profile.required_device_bytes
        || shared > profile.required_shared_bytes
    {
        return Err(EngineStartError::InvalidAllocation(observed));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamFrame {
    schema: String,
    attempt_id: String,
    sequence: u64,
    kind: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    raw_output: Option<String>,
    #[serde(default)]
    message: Option<Value>,
    #[serde(default)]
    usage: Option<StreamUsage>,
    #[serde(default)]
    prefill: Option<Value>,
    #[serde(default)]
    timings: Option<StreamTimings>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamTimings {
    cache_n: u64,
    prompt_n: u64,
    prompt_ms: f64,
    prompt_per_token_ms: f64,
    prompt_per_second: f64,
    predicted_n: u64,
    predicted_ms: f64,
    predicted_per_token_ms: f64,
    predicted_per_second: f64,
    #[serde(default)]
    draft_n: u64,
    #[serde(default)]
    draft_n_accepted: u64,
}

impl StreamTimings {
    fn validate(&self, usage: &StreamUsage) -> Result<(), InferenceServiceError> {
        let finite_non_negative = |value: f64| value.is_finite() && value >= 0.0;
        if self.cache_n.checked_add(self.prompt_n) != Some(usage.prompt_tokens)
            || self.predicted_n != usage.completion_tokens
            || self.draft_n_accepted > self.draft_n
            || !finite_non_negative(self.prompt_ms)
            || !finite_non_negative(self.prompt_per_token_ms)
            || !finite_non_negative(self.prompt_per_second)
            || !finite_non_negative(self.predicted_ms)
            || !finite_non_negative(self.predicted_per_token_ms)
            || !finite_non_negative(self.predicted_per_second)
        {
            return Err(InferenceServiceError::InvalidResult);
        }
        Ok(())
    }
}

struct Generated {
    output: ModelGenerationOutput,
    private_reasoning: Option<Content>,
    finish_reason: ModelFinishReason,
    usage: ModelUsage,
    timings: StreamTimings,
}

#[cfg(test)]
fn decode_generation(
    bytes: &[u8],
    attempt: &str,
    progress: Option<&InferenceProgressSink>,
    cancellation_requested: bool,
    reasoning_enabled: bool,
) -> Result<Generated, InferenceServiceError> {
    let mut decoder = GenerationStreamDecoder::new(attempt, progress.cloned(), reasoning_enabled);
    decoder.feed(bytes)?;
    decoder.finish(cancellation_requested)
}

struct GenerationStreamDecoder<'a> {
    attempt: &'a str,
    progress: Option<InferenceProgressSink>,
    pending: Vec<u8>,
    sequence: u64,
    terminal: Option<StreamFrame>,
    terminal_error: bool,
    raw_deltas: String,
    reasoning_enabled: bool,
}

impl<'a> GenerationStreamDecoder<'a> {
    fn new(
        attempt: &'a str,
        progress: Option<InferenceProgressSink>,
        reasoning_enabled: bool,
    ) -> Self {
        Self {
            attempt,
            progress,
            pending: Vec::new(),
            sequence: 1,
            terminal: None,
            terminal_error: false,
            raw_deltas: String::new(),
            reasoning_enabled,
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Result<(), InferenceServiceError> {
        if self.terminal.is_some() || self.terminal_error {
            return Err(InferenceServiceError::IdentityMismatch);
        }
        if self.pending.len().saturating_add(bytes.len()) > MAX_RESPONSE_BYTES {
            return Err(InferenceServiceError::InvalidResult);
        }
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=end).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            self.accept_line(&line)?;
        }
        Ok(())
    }

    fn accept_line(&mut self, line: &[u8]) -> Result<(), InferenceServiceError> {
        let frame: StreamFrame = serde_json::from_slice(line).map_err(|error| {
            tracing::warn!(
                attempt_id=%self.attempt,
                %error,
                backend_diagnostic_bytes=line.len(),
                "private llama-server returned an invalid stream frame"
            );
            InferenceServiceError::InvalidResult
        })?;
        if frame.schema != "agentlibre.llama-stream/v1"
            || frame.attempt_id != self.attempt
            || frame.sequence != self.sequence
            || self.terminal.is_some()
            || self.terminal_error
        {
            return Err(InferenceServiceError::IdentityMismatch);
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(InferenceServiceError::InvalidRequest)?;
        match frame.kind.as_str() {
            "delta" => {
                if frame.finish_reason.is_some()
                    || frame.raw_output.is_some()
                    || frame.message.is_some()
                    || frame.usage.is_some()
                    || frame.prefill.is_some()
                    || frame.timings.is_some()
                    || frame.error.is_some()
                {
                    return Err(InferenceServiceError::InvalidResult);
                }
                let content = frame.content.ok_or(InferenceServiceError::InvalidResult)?;
                if content.is_empty() {
                    return Err(InferenceServiceError::InvalidResult);
                }
                self.raw_deltas.push_str(&content);
                if !self.reasoning_enabled
                    && let Some(progress) = &self.progress
                {
                    progress.emit(
                        Content::text(content).map_err(|_| InferenceServiceError::InvalidResult)?,
                    );
                }
            }
            "error" => {
                if frame.content.is_some()
                    || frame.finish_reason.is_some()
                    || frame.raw_output.is_some()
                    || frame.message.is_some()
                    || frame.usage.is_some()
                    || frame.prefill.is_some()
                    || frame.timings.is_some()
                    || frame.error.is_none()
                {
                    return Err(InferenceServiceError::InvalidResult);
                }
                let diagnostic = serde_json::to_vec(frame.error.as_ref().expect("checked above"))
                    .map(|value| bounded_backend_diagnostic(&value))
                    .unwrap_or_default();
                tracing::warn!(
                    attempt_id=%self.attempt,
                    backend_diagnostic_bytes=diagnostic.len(),
                    backend_diagnostic=%diagnostic,
                    "private llama-server generation failed"
                );
                self.terminal_error = true;
            }
            "final" => {
                if frame.content.is_some() || frame.error.is_some() {
                    return Err(InferenceServiceError::InvalidResult);
                }
                self.terminal = Some(frame);
            }
            _ => return Err(InferenceServiceError::InvalidRequest),
        }
        Ok(())
    }

    fn finish(self, cancellation_requested: bool) -> Result<Generated, InferenceServiceError> {
        if !self.pending.is_empty() {
            return Err(InferenceServiceError::OutcomeUnknown);
        }
        if self.terminal_error {
            return Err(if cancellation_requested {
                InferenceServiceError::Cancelled
            } else {
                InferenceServiceError::Unavailable
            });
        }
        if cancellation_requested {
            return Err(InferenceServiceError::OutcomeUnknown);
        }
        let frame = self.terminal.ok_or(InferenceServiceError::OutcomeUnknown)?;
        let usage = frame.usage.ok_or(InferenceServiceError::InvalidRequest)?;
        let timings = frame.timings.ok_or(InferenceServiceError::InvalidRequest)?;
        timings.validate(&usage)?;
        let raw = frame
            .raw_output
            .ok_or(InferenceServiceError::InvalidRequest)?;
        if self.raw_deltas != raw {
            return Err(InferenceServiceError::InvalidResult);
        }
        let usage = ModelUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
        };
        let invalid_raw = Content::text(raw.clone()).ok();
        let invalid_output = |class, field: &str| {
            InferenceServiceError::InvalidModelOutput(Box::new(
                crate::inference::InvalidModelOutput {
                    raw_output: invalid_raw.clone(),
                    diagnostic: ModelOutputDiagnostic {
                        class,
                        field: Some(field.to_owned()),
                        finish_reason: match frame.finish_reason.as_deref() {
                            Some("length") => Some(ModelFinishReason::Length),
                            Some("tool_calls") => Some(ModelFinishReason::ToolCall),
                            Some("stop") => Some(ModelFinishReason::Stop),
                            _ => None,
                        },
                        output_bytes: raw.len() as u64,
                        output_digest: PackageDigest::from_bytes(
                            Sha256::digest(raw.as_bytes()).into(),
                        ),
                    },
                    usage,
                    realization: None,
                },
            ))
        };
        let (output, private_reasoning) = if self.reasoning_enabled {
            projected_reasoning_output(frame.message.as_ref()).map_err(|error| {
                invalid_output(
                    ModelOutputFailureClass::ReasoningProjection,
                    error.diagnostic_field(),
                )
            })?
        } else {
            (
                match structured_call(frame.message.as_ref()).map_err(|error| {
                    invalid_output(
                        ModelOutputFailureClass::StructuredToolCall,
                        error.diagnostic_field(),
                    )
                })? {
                    Some(call) => call,
                    None => parsed_output(&raw)
                        .map_err(|class| invalid_output(class, "raw_output.public_action"))?,
                },
                None,
            )
        };
        let finish_reason = match frame.finish_reason.as_deref() {
            Some("length") => ModelFinishReason::Length,
            Some("tool_calls") => ModelFinishReason::ToolCall,
            Some("stop")
                if matches!(
                    output,
                    ModelGenerationOutput::ToolCall(_)
                        | ModelGenerationOutput::ToolCalls(_)
                        | ModelGenerationOutput::AssistantToolCall(_)
                        | ModelGenerationOutput::AssistantToolCalls { .. }
                ) =>
            {
                ModelFinishReason::ToolCall
            }
            Some("stop") => ModelFinishReason::Stop,
            _ => return Err(InferenceServiceError::InvalidRequest),
        };
        Ok(Generated {
            output,
            private_reasoning,
            finish_reason,
            usage,
            timings,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelMessageError(String);

impl ModelMessageError {
    fn field(field: impl Into<String>) -> Self {
        Self(field.into())
    }

    fn diagnostic_field(&self) -> &str {
        &self.0
    }
}

fn projected_reasoning_output(
    message: Option<&Value>,
) -> Result<(ModelGenerationOutput, Option<Content>), ModelMessageError> {
    let message = message.ok_or_else(|| ModelMessageError::field("message.missing"))?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err(ModelMessageError::field("message.role"));
    }
    if message
        .get("reasoning_content")
        .is_some_and(|value| !value.is_string())
    {
        return Err(ModelMessageError::field("message.reasoning_content.type"));
    }
    let content = match message.get("content") {
        None | Some(Value::Null) => None,
        Some(Value::String(content)) if content.is_empty() => None,
        Some(Value::String(content)) => Some(content.as_str()),
        Some(_) => return Err(ModelMessageError::field("message.content.type")),
    };
    let private_reasoning = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(Content::text)
        .transpose()
        .map_err(|_| ModelMessageError::field("message.reasoning_content.value"))?;
    match (structured_call(Some(message))?, content) {
        (Some(call), None) => Ok((call, private_reasoning)),
        (None, Some(content)) => Content::text(content)
            .map(|content| (ModelGenerationOutput::Assistant(content), private_reasoning))
            .map_err(|_| ModelMessageError::field("message.content.value")),
        (None, None) => Err(ModelMessageError::field("message.public_action.none")),
        (Some(ModelGenerationOutput::ToolCall(call)), Some(content)) => Content::text(content)
            .map(|content| {
                (
                    ModelGenerationOutput::AssistantToolCall(agl_core::agent::AssistantToolCall {
                        content,
                        call,
                    }),
                    private_reasoning,
                )
            })
            .map_err(|_| ModelMessageError::field("message.content.value")),
        (Some(ModelGenerationOutput::ToolCalls(calls)), Some(content)) => Content::text(content)
            .map(|content| {
                (
                    ModelGenerationOutput::AssistantToolCalls { content, calls },
                    private_reasoning,
                )
            })
            .map_err(|_| ModelMessageError::field("message.content.value")),
        (Some(_), Some(_)) => unreachable!("structured_call returns only tool calls"),
    }
}

fn structured_call(
    message: Option<&Value>,
) -> Result<Option<ModelGenerationOutput>, ModelMessageError> {
    let Some(message) = message else {
        return Ok(None);
    };
    let Some(calls) = message.get("tool_calls") else {
        return Ok(None);
    };
    let calls = calls
        .as_array()
        .ok_or_else(|| ModelMessageError::field("message.tool_calls.type"))?;
    if calls.is_empty() {
        return Err(ModelMessageError::field(format!(
            "message.tool_calls.count={}",
            calls.len()
        )));
    }
    let parsed = calls
        .iter()
        .enumerate()
        .map(|(index, call)| {
            let function = call
                .get("function")
                .or_else(|| call.get("agent"))
                .ok_or_else(|| {
                    ModelMessageError::field(format!("message.tool_calls[{index}].function"))
                })?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ModelMessageError::field(format!("message.tool_calls[{index}].function.name"))
                })?;
            let arguments = function.get("arguments").ok_or_else(|| {
                ModelMessageError::field(format!("message.tool_calls[{index}].function.arguments"))
            })?;
            let arguments = match arguments {
                Value::String(value) => serde_json::from_str(value).map_err(|_| {
                    ModelMessageError::field(format!(
                        "message.tool_calls[{index}].function.arguments.json"
                    ))
                })?,
                value => value.clone(),
            };
            Ok(agl_core::agent::ToolCall {
                tool_id: agl_core::ToolId::new(name).map_err(|_| {
                    ModelMessageError::field(format!(
                        "message.tool_calls[{index}].function.name.value"
                    ))
                })?,
                input: arguments,
            })
        })
        .collect::<Result<Vec<_>, ModelMessageError>>()?;
    Ok(Some(if parsed.len() == 1 {
        ModelGenerationOutput::ToolCall(parsed.into_iter().next().unwrap())
    } else {
        ModelGenerationOutput::ToolCalls(parsed)
    }))
}

fn parsed_output(raw: &str) -> Result<ModelGenerationOutput, ModelOutputFailureClass> {
    match parse_model_output(raw) {
        ParsedModelOutput::Answer(answer) => Content::text(answer)
            .map(ModelGenerationOutput::Assistant)
            .map_err(|_| ModelOutputFailureClass::InvalidContent),
        ParsedModelOutput::ToolCall(call) => {
            Ok(ModelGenerationOutput::ToolCall(agl_core::agent::ToolCall {
                tool_id: agl_core::ToolId::new(call.name)
                    .map_err(|_| ModelOutputFailureClass::InvalidToolId)?,
                input: call.arguments,
            }))
        }
        ParsedModelOutput::ToolCalls(calls) => calls
            .into_iter()
            .map(|call| {
                Ok(agl_core::agent::ToolCall {
                    tool_id: agl_core::ToolId::new(call.name)
                        .map_err(|_| ModelOutputFailureClass::InvalidToolId)?,
                    input: call.arguments,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(ModelGenerationOutput::ToolCalls),
        ParsedModelOutput::MalformedToolCall(call) => match call.repair {
            Some(ToolJsonRepair::Succeeded { tool_call, .. }) => {
                Ok(ModelGenerationOutput::ToolCall(agl_core::agent::ToolCall {
                    tool_id: agl_core::ToolId::new(tool_call.name)
                        .map_err(|_| ModelOutputFailureClass::InvalidToolId)?,
                    input: tool_call.arguments,
                }))
            }
            _ => Err(match call.classification {
                crate::inference::output_codec::MalformedToolJsonKind::MissingTerminator => {
                    ModelOutputFailureClass::MissingTerminator
                }
                crate::inference::output_codec::MalformedToolJsonKind::Syntax => {
                    ModelOutputFailureClass::Syntax
                }
                crate::inference::output_codec::MalformedToolJsonKind::InvalidShape => {
                    ModelOutputFailureClass::InvalidShape
                }
            }),
        },
    }
}

fn verified_regular_file(path: &Path) -> Result<PathBuf, InferenceServiceError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| InferenceServiceError::InvalidRequest)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(InferenceServiceError::InvalidRequest);
    }
    path.canonicalize()
        .map_err(|_| InferenceServiceError::InvalidRequest)
}

fn private_directory() -> Result<PathBuf, InferenceServiceError> {
    let path = std::env::temp_dir().join(format!(
        "agl-runtime-inference-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .map_err(|_| InferenceServiceError::Unavailable)?;
    Ok(path)
}

fn address_space_limit(profile: &LlamaRuntimeProfile) -> u64 {
    profile
        .required_host_bytes
        .saturating_add(profile.required_device_bytes)
        .saturating_add(profile.required_shared_bytes)
        // llama.cpp temporarily duplicates prompt state while checkpointing a
        // slot. RLIMIT_AS bounds virtual mappings, not the admitted resident
        // memory, so leave room for that bounded transient copy.
        .saturating_mul(2)
        .saturating_add(2 * 1024 * 1024 * 1024)
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};

    use super::*;

    fn digest(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    fn profile() -> LlamaRuntimeProfile {
        LlamaRuntimeProfile {
            digest: RuntimeProfileDigest::parse(digest('1')).unwrap(),
            physical_device: PhysicalDeviceDigest::parse(digest('2')).unwrap(),
            driver_build: DriverBuildDigest::parse(digest('3')).unwrap(),
            required_host_bytes: 1,
            required_device_bytes: 0,
            required_shared_bytes: 0,
            context_tokens: 1024,
            batch_size: 32,
            ubatch_size: 16,
            threads: 1,
            gpu_layers: 0,
            speculative: false,
            speculative_max_draft_tokens: 0,
            speculative_type_k: None,
            speculative_type_v: None,
            slots: 1,
            continuous_batching: false,
            device: None,
            device_paths: vec![],
            selectors: vec![],
        }
    }

    #[test]
    fn cold_registration_never_launches_and_failed_unload_retains_service() {
        let directory = private_directory().unwrap();
        let artifact_path = directory.join("model.gguf");
        fs::write(&artifact_path, b"GGUF-test").unwrap();
        let artifact_digest = PackageDigest::from_bytes(hash32(b"GGUF-test"));
        let artifact = crate::model::import_model(&crate::model::ModelImportRequest {
            path: artifact_path,
            expected_digest: model_artifact_digest(artifact_digest).unwrap(),
        })
        .unwrap();
        let mut runtime = crate::inference::service::test_runtime_selection();
        runtime.artifact.digest = artifact_digest;
        runtime.artifact.bytes = 9;
        let key = runtime.service.key;
        let execution = BlockingExecutionClient::new(directory.join("absent-execd.sock"));
        let host = LlamaHost {
            executable: directory.join("absent-llama-server"),
            execution: execution.clone(),
            engine_build: EngineBuildDigest::from_bytes(
                *runtime.load.engine_build_digest.as_bytes(),
            ),
            health: Mutex::new(RestoredInferenceHealth::default()),
            health_updates: Mutex::new(vec![]),
            service_changes: Mutex::new(()),
            services: Mutex::new(BTreeMap::new()),
        };
        host.register_service(RegisteredModelService {
            model: agl_core::agent::ModelDefinitionRef {
                id: crate::package::PackageId::new("test-model").unwrap(),
                version: crate::package::PackageVersion::new("1.0.0").unwrap(),
                digest: artifact_digest,
            },
            runtime,
            artifact,
            adapters: vec![],
        })
        .unwrap();
        let service = host.services.lock().unwrap().get(&key).unwrap().clone();
        assert!(service.engine.lock().unwrap().is_none());
        let engine_directory = directory.join("engine");
        fs::create_dir(&engine_directory).unwrap();
        *service.engine.lock().unwrap() = Some(Arc::new(ResidentEngine {
            execution,
            execution_id: ExecutionId::generate(),
            directory: engine_directory.clone(),
            socket_path: engine_directory.join("llama.sock"),
            profile: profile(),
            reasoning_supported: AtomicBool::new(false),
            device_lost: Arc::new(AtomicBool::new(false)),
            diagnostic_cursor: Mutex::new(0),
            terminated: AtomicBool::new(false),
            retiring: AtomicBool::new(false),
        }));
        assert!(matches!(
            host.unload_service(key),
            Err(InferenceServiceError::OutcomeUnknown)
        ));
        assert!(host.services.lock().unwrap().contains_key(&key));
        assert!(service.engine.lock().unwrap().is_some());
        assert!(engine_directory.exists());
        drop(service);
        drop(host);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn engine_release_requires_confirmed_reap_and_keeps_unknown_ownership() {
        use agl_execution_api::{
            ExecutionCommand, ExecutionOutput, ExecutionProtocolRequest, ExecutionProtocolResponse,
            ExecutionResponse, ExecutionStatus,
        };
        use std::io::BufRead as _;

        let directory = private_directory().unwrap();
        let socket = directory.join("execd.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let execution_id = ExecutionId::generate();
        let server = std::thread::spawn(move || {
            for index in 0..5 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                std::io::BufReader::new(&mut stream)
                    .read_line(&mut line)
                    .unwrap();
                let request: ExecutionProtocolRequest = serde_json::from_str(&line).unwrap();
                let response = match request.command {
                    ExecutionCommand::Signal {
                        execution_id: id,
                        signal: ExecutionSignal::Terminate,
                    } => {
                        assert_eq!(id, execution_id);
                        assert!(matches!(index, 0 | 2));
                        ExecutionResponse::Acknowledged
                    }
                    ExecutionCommand::Inspect { execution_id: id } => {
                        assert_eq!(id, execution_id);
                        assert!(matches!(index, 1 | 3));
                        ExecutionResponse::Status {
                            status: ExecutionStatus {
                                execution_id,
                                terminal_id: None,
                                owner: ExecutionOwner::Runtime {
                                    component: "inference".into(),
                                },
                                state: if index == 1 {
                                    ExecutionState::OutcomeUnknown
                                } else {
                                    ExecutionState::Exited
                                },
                                outcome: Some(if index == 1 {
                                    ExecutionOutcome::UnknownAfterServiceRestart
                                } else {
                                    ExecutionOutcome::Terminated
                                }),
                                output_bytes: 0,
                                output_truncated: false,
                            },
                        }
                    }
                    ExecutionCommand::Read {
                        execution_id: id,
                        after,
                        ..
                    } => {
                        assert_eq!(id, execution_id);
                        assert_eq!(index, 4);
                        ExecutionResponse::Output {
                            output: ExecutionOutput {
                                execution_id,
                                after,
                                next: after,
                                chunks: vec![],
                                eof: true,
                                truncated: false,
                            },
                        }
                    }
                    other => panic!("unexpected release request: {other:?}"),
                };
                serde_json::to_writer(
                    &mut stream,
                    &ExecutionProtocolResponse::success(request.request_id, response),
                )
                .unwrap();
                stream.write_all(b"\n").unwrap();
            }
        });
        let engine = ResidentEngine {
            execution: BlockingExecutionClient::new(socket),
            execution_id,
            directory: directory.clone(),
            socket_path: directory.join("llama.sock"),
            profile: profile(),
            reasoning_supported: AtomicBool::new(false),
            device_lost: Arc::new(AtomicBool::new(false)),
            diagnostic_cursor: Mutex::new(0),
            terminated: AtomicBool::new(false),
            retiring: AtomicBool::new(false),
        };
        assert!(matches!(
            engine.terminate(),
            Err(InferenceServiceError::OutcomeUnknown)
        ));
        assert!(directory.exists());
        assert!(!engine.terminated.load(Ordering::Acquire));
        assert!(engine.retiring.load(Ordering::Acquire));
        engine.terminate().unwrap();
        assert!(engine.terminated.load(Ordering::Acquire));
        assert!(!directory.exists());
        server.join().unwrap();
        // Confirmed release is idempotent and needs no reachable execd socket.
        engine.terminate().unwrap();
    }

    #[test]
    fn address_limit_covers_all_admitted_memory_domains() {
        let mut profile = profile();
        profile.required_host_bytes = 10;
        profile.required_device_bytes = 20;
        profile.required_shared_bytes = 30;
        assert_eq!(address_space_limit(&profile), 2 * 1024 * 1024 * 1024 + 120);

        profile.required_host_bytes = u64::MAX;
        assert_eq!(address_space_limit(&profile), u64::MAX);
    }

    #[test]
    fn service_gate_enforces_slots_queue_and_request_local_cancellation() {
        let gate = Arc::new(ServiceGate::new(2, 1));
        let first = gate
            .acquire(&InferenceCancellation::new(), unix_ms() + 5_000)
            .unwrap();
        let second = gate
            .acquire(&InferenceCancellation::new(), unix_ms() + 5_000)
            .unwrap();
        let queued_cancellation = InferenceCancellation::new();
        let worker_cancellation = queued_cancellation.clone();
        let queued_gate = Arc::clone(&gate);
        let queued = std::thread::spawn(move || {
            queued_gate
                .acquire(&worker_cancellation, unix_ms() + 5_000)
                .map(|_| ())
        });
        for _ in 0..1_000 {
            if gate.state.lock().unwrap().queued == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(gate.state.lock().unwrap().queued, 1);
        assert!(matches!(
            gate.acquire(&InferenceCancellation::new(), unix_ms() + 5_000),
            Err(InferenceServiceError::Unavailable)
        ));

        queued_cancellation.cancel();
        assert_eq!(
            queued.join().unwrap(),
            Err(InferenceServiceError::Cancelled)
        );
        drop(first);
        drop(second);
        assert_eq!(gate.state.lock().unwrap().active, 0);
    }

    #[test]
    fn launch_arguments_expose_selected_slots_and_continuous_batching() {
        let mut runtime = crate::inference::service::test_runtime_selection();
        runtime.service.slots = 2;
        runtime.service.continuous_batching = true;
        let mut profile = profile();
        profile.slots = 2;
        profile.continuous_batching = true;

        let arguments = launch_arguments(&runtime, &profile).unwrap();
        let parallel = arguments
            .iter()
            .position(|value| value == "--parallel")
            .unwrap();
        assert_eq!(arguments[parallel + 1], "2");
        let context = arguments
            .iter()
            .position(|value| value == "--ctx-size")
            .unwrap();
        assert_eq!(arguments[context + 1], "2048");
        assert!(!arguments.iter().any(|value| value == "--no-cont-batching"));

        runtime.service.continuous_batching = false;
        assert!(
            launch_arguments(&runtime, &profile)
                .unwrap()
                .iter()
                .any(|value| value == "--no-cont-batching")
        );
    }

    #[test]
    fn launch_arguments_emit_exact_mtp_pairs() {
        let mut runtime = crate::inference::service::test_runtime_selection();
        runtime.load.gpu_layers = GpuLayerSelection::All;
        runtime.speculative = Some(agl_core::agent::SpeculativeSelection::Mtp {
            max_draft_tokens: 3,
            kv_cache_type_k: agl_core::agent::KvCacheType::Q4_0,
            kv_cache_type_v: agl_core::agent::KvCacheType::Q4_0,
        });
        let mut profile = profile();
        profile.gpu_layers = -2;
        profile.device = Some("Vulkan0".into());
        profile.speculative = true;
        let arguments = launch_arguments(&runtime, &profile).unwrap();
        let gpu_layers = arguments
            .iter()
            .position(|value| value == "--gpu-layers")
            .unwrap();
        assert_eq!(arguments[gpu_layers + 1], "-2");
        for pair in [
            ("--spec-type", "draft-mtp"),
            ("--spec-draft-device", "Vulkan0"),
            ("--spec-draft-ngl", "all"),
            ("--spec-draft-n-max", "3"),
            ("--spec-draft-type-k", "q4_0"),
            ("--spec-draft-type-v", "q4_0"),
        ] {
            let index = arguments.iter().position(|value| value == pair.0).unwrap();
            assert_eq!(arguments[index + 1], pair.1);
        }
    }

    #[test]
    fn readiness_rejects_missing_or_mismatched_speculative_activation() {
        let mut profile = profile();
        profile.speculative = true;
        profile.speculative_max_draft_tokens = 3;
        profile.speculative_type_k = Some("q4_0".into());
        profile.speculative_type_v = Some("q4_0".into());
        let readiness = |enabled| Readiness {
            schema: "agentlibre.llama-readiness/v1".into(),
            plan_digest: profile.digest.to_string(),
            reservation_id: "1".into(),
            engine_generation: "1".into(),
            context_tokens: profile.context_tokens,
            batch_size: profile.batch_size,
            ubatch_size: profile.ubatch_size,
            slot_count: profile.slots,
            reasoning_supported: false,
            speculative: ReadinessSpeculative {
                enabled,
                kind: if enabled { "none,draft-mtp" } else { "none" }.into(),
                max_draft_tokens: if enabled { 3 } else { 0 },
                _min_draft_tokens: 0,
                _p_min_millionths: 0,
                gpu_layers: if enabled { -2 } else { 0 },
                key_cache_type: if enabled { "q4_0" } else { "f16" }.into(),
                value_cache_type: if enabled { "q4_0" } else { "f16" }.into(),
            },
            memory: vec![],
        };
        assert!(readiness_matches_profile(&readiness(true), &profile).unwrap());
        assert!(!readiness_matches_profile(&readiness(false), &profile).unwrap());
        profile.speculative = false;
        assert!(readiness_matches_profile(&readiness(false), &profile).unwrap());
    }

    #[test]
    fn readiness_accepts_the_engine_speculative_object_shape() {
        let value = serde_json::json!({
            "enabled": true,
            "kind": "none,draft-mtp",
            "max_draft_tokens": 3,
            "min_draft_tokens": 0,
            "p_min_millionths": 0,
            "gpu_layers": -2,
            "key_cache_type": "q4_0",
            "value_cache_type": "q4_0"
        });
        let parsed: ReadinessSpeculative = serde_json::from_value(value).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.gpu_layers, -2);
        assert_eq!(parsed.max_draft_tokens, 3);
    }

    #[test]
    fn mtp_reservation_scales_with_the_admitted_context() {
        let mut runtime = crate::inference::service::test_runtime_selection();
        runtime.load.gpu_layers = GpuLayerSelection::All;
        runtime.speculative = Some(agl_core::agent::SpeculativeSelection::Mtp {
            max_draft_tokens: 3,
            kv_cache_type_k: agl_core::agent::KvCacheType::Q4_0,
            kv_cache_type_v: agl_core::agent::KvCacheType::Q4_0,
        });
        runtime.load.context_tokens = 1024;
        let engine = EngineBuildDigest::parse(digest('4')).unwrap();
        let short = generated_profile(&runtime, engine).unwrap();
        runtime.load.context_tokens = 2048;
        let long = generated_profile(&runtime, engine).unwrap();
        assert!(long.required_device_bytes > short.required_device_bytes);
        assert!(long.required_host_bytes > short.required_host_bytes);
    }

    #[test]
    fn reasoning_request_is_explicit_per_attempt() {
        let mut disabled = json!({});
        apply_reasoning_request(&mut disabled, agl_core::agent::ReasoningSelection::Disabled);
        assert_eq!(
            disabled,
            json!({
                "chat_template_kwargs": {"enable_thinking": false},
                "reasoning_format": "none"
            })
        );

        let mut enabled = json!({});
        apply_reasoning_request(
            &mut enabled,
            agl_core::agent::ReasoningSelection::Enabled {
                max_tokens: 4096,
                effort: Some(agl_core::agent::ReasoningEffort::Xhigh),
                preserve: true,
            },
        );
        assert_eq!(
            enabled,
            json!({
                "chat_template_kwargs": {
                    "enable_thinking": true,
                    "preserve_thinking": true,
                    "reasoning_effort": "xhigh"
                },
                "reasoning_format": "deepseek",
                "thinking_budget_tokens": 4096
            })
        );
    }

    #[test]
    fn oversized_schema_repetitions_are_relaxed_only_for_llama_grammar() {
        let admitted = json!({
            "type": "object",
            "properties": {
                "argv": {
                    "type": "array",
                    "maxItems": 256,
                    "items": {"type": "string", "maxLength": 32768}
                },
                "input": {"type": "string", "maxLength": 1048576}
            }
        });

        assert_eq!(
            llama_tool_schema(admitted.clone()),
            json!({
                "type": "object",
                "properties": {
                    "argv": {
                        "type": "array",
                        "maxItems": 256,
                        "items": {"type": "string"}
                    },
                    "input": {"type": "string"}
                }
            })
        );
        assert_eq!(admitted["properties"]["input"]["maxLength"], 1048576);
    }

    #[test]
    fn root_object_union_is_projected_for_llama_tool_parameters() {
        let admitted = json!({
            "oneOf": [
                {
                    "type": "object",
                    "required": ["action", "argv"],
                    "properties": {
                        "action": {"const": "open"},
                        "argv": {"type": "array"}
                    }
                },
                {
                    "type": "object",
                    "required": ["action", "terminal_id"],
                    "properties": {
                        "action": {"const": "read"},
                        "terminal_id": {"type": "string"}
                    }
                }
            ]
        });

        let projected = llama_tool_schema(admitted.clone());
        assert_eq!(projected["type"], "object");
        assert_eq!(projected["required"], json!(["action"]));
        assert_eq!(
            projected["properties"]["action"],
            json!({"anyOf": [{"const": "open"}, {"const": "read"}]})
        );
        assert!(projected["properties"].get("argv").is_some());
        assert!(projected["properties"].get("terminal_id").is_some());
        assert_eq!(admitted.get("type"), None);
        assert!(admitted.get("oneOf").is_some());
    }

    #[test]
    fn required_reasoning_fails_closed_without_template_support() {
        assert_eq!(
            validate_reasoning_capability(
                agl_core::agent::ReasoningSelection::Enabled {
                    max_tokens: 1,
                    effort: None,
                    preserve: false,
                },
                false,
            ),
            Err(InferenceServiceError::InvalidRequest)
        );
        assert_eq!(
            validate_reasoning_capability(agl_core::agent::ReasoningSelection::Disabled, false,),
            Ok(false)
        );
    }

    #[test]
    fn complete_prompt_reserves_the_full_generation_budget() {
        assert_eq!(ensure_context_capacity(48, 16, 64), Ok(()));
        assert_eq!(
            ensure_context_capacity(49, 16, 64),
            Err(InferenceServiceError::ContextExhausted(
                agl_core::agent::ContextCapacity::new(49, 16, 64)
            ))
        );
        assert_eq!(
            ensure_context_capacity(u64::MAX, 1, 64),
            Err(InferenceServiceError::ContextExhausted(
                agl_core::agent::ContextCapacity::new(u64::MAX, 1, 64)
            ))
        );
    }

    #[test]
    fn prompt_capacity_is_per_slot_not_the_engines_total_context() {
        let mut profile = profile();
        profile.context_tokens = 64;
        profile.slots = 2;
        assert_eq!(engine_context_tokens(&profile).unwrap(), 128);
        assert!(
            matches!(ensure_context_capacity(49, 16, profile.context_tokens),
            Err(InferenceServiceError::ContextExhausted(capacity))
                if capacity.context_capacity_tokens == 64 && capacity.prompt_tokens == 49)
        );
    }

    #[test]
    fn engine_context_rejects_slot_multiplication_overflow() {
        let mut profile = profile();
        profile.context_tokens = u32::MAX;
        profile.slots = 2;

        assert_eq!(
            engine_context_tokens(&profile),
            Err(InferenceServiceError::InvalidRequest)
        );
    }

    #[test]
    fn generator_requires_the_exact_private_engine_bundle_digest() {
        let directory = private_directory().unwrap();
        let executable = directory.join("llama-server");
        let implementation = directory.join("libllama-server-impl.so");
        fs::write(&executable, b"launcher\n").unwrap();
        fs::write(&implementation, b"implementation\n").unwrap();
        let engine_build = crate::inference::private_engine_build_digest(&executable).unwrap();

        assert!(
            generator(
                LlamaServerConfig {
                    executable: executable.clone(),
                },
                engine_build,
                BlockingExecutionClient::new("/test-does-not-use-execd"),
                &RestoredInferenceHealth::default(),
            )
            .is_ok()
        );

        fs::write(&implementation, b"changed implementation\n").unwrap();
        assert!(matches!(
            generator(
                LlamaServerConfig { executable },
                engine_build,
                BlockingExecutionClient::new("/test-does-not-use-execd"),
                &RestoredInferenceHealth::default(),
            ),
            Err(InferenceServiceError::InvalidRequest)
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn request_appends_a_user_turn_after_an_assistant_summary() {
        let mut messages = vec![json!({"role": "assistant", "content": "summary"})];
        append_user_turn_after_assistant(&mut messages, false);
        assert_eq!(
            messages,
            vec![
                json!({"role": "assistant", "content": "summary"}),
                json!({"role": "user", "content": ""}),
            ]
        );

        let mut messages = vec![json!({"role": "user", "content": "tail"})];
        append_user_turn_after_assistant(&mut messages, true);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn request_instructs_continuation_after_compaction() {
        let mut messages = vec![json!({"role": "assistant", "content": "summary"})];
        append_user_turn_after_assistant(&mut messages, true);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(
            messages[1]["content"],
            "Continue the task preserved in the compaction summary, starting from the concrete next step in semantic.next_position. Use exact.inspected_files and semantic.completed as the work ledger; do not repeat completed investigation unless a file digest changed."
        );
    }

    #[test]
    fn measurement_only_posts_the_exact_body_to_tokenizer_and_reports_oversize() {
        use std::io::BufRead as _;
        let directory = private_directory().unwrap();
        let socket = directory.join("measure.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let body = br#"{"messages":[{"role":"user","content":"fixture"}]}"#;
        let server = std::thread::spawn(move || {
            let (connection, _) = listener.accept().unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = std::io::BufReader::new(connection);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, "POST /agl/v1/input-tokens HTTP/1.1\r\n");
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                assert!(!line.is_empty());
            }
            let mut received = vec![0; body.len()];
            reader.read_exact(&mut received).unwrap();
            assert_eq!(received, body);
            let response = br#"{"input_tokens":257}"#;
            write!(
                reader.get_mut(),
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            )
            .unwrap();
            reader.get_mut().write_all(response).unwrap();
        });
        let capacity = measure_context_capacity(&socket, body, 32, 256).unwrap();
        assert_eq!(capacity.prompt_tokens, 257);
        assert!(!capacity.fits());
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn tokenizer_error_envelopes_have_typed_fail_closed_mapping() {
        let envelope = |status: u16, kind: &str, message: &str| {
            serde_json::to_vec(&json!({
                "error": {"code": status, "message": message, "type": kind}
            }))
            .unwrap()
        };

        let (error, code, _) = classify_tokenizer_error(
            400,
            &envelope(400, "exceed_context_size_error", "too large"),
            32,
            256,
        );
        assert_eq!(code, "context_exhausted");
        assert!(matches!(error, InferenceServiceError::ContextExhausted(_)));

        let (error, code, _) = classify_tokenizer_error(
            400,
            &envelope(400, "invalid_request_error", "bad template"),
            32,
            256,
        );
        assert_eq!(code, "invalid_request");
        assert_eq!(error, InferenceServiceError::InvalidRequest);

        let (error, code, diagnostic) = classify_tokenizer_error(
            503,
            &envelope(503, "unavailable_error", &"x".repeat(600)),
            32,
            256,
        );
        assert_eq!(code, "backend_unavailable");
        assert_eq!(
            error,
            InferenceServiceError::UnavailableWithReason(
                "llama-server reported an unavailable backend".to_owned()
            )
        );
        assert_eq!(diagnostic.chars().count(), 512);

        let (error, code, _) = classify_tokenizer_error(418, br#"{"error":"unknown"}"#, 32, 256);
        assert_eq!(code, "malformed_error_envelope");
        assert_eq!(error, InferenceServiceError::InvalidResult);
    }

    #[test]
    fn private_http_stream_emits_progress_before_terminal_and_sends_identity_headers() {
        let directory = private_directory().unwrap();
        let socket = directory.join("stream.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (progress_sent, progress_seen) = std::sync::mpsc::sync_channel(1);
        let observed_before_terminal = Arc::new(AtomicBool::new(false));
        let server_observed = observed_before_terminal.clone();
        let server = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = connection.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| position + 4)
                else {
                    continue;
                };
                let header = std::str::from_utf8(&request[..header_end]).unwrap();
                let normalized_header = header.to_ascii_lowercase();
                let length = header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap();
                if request.len() >= header_end + length {
                    assert!(header.starts_with("POST /agl/v1/generate HTTP/1.1\r\n"));
                    assert!(normalized_header.contains("\r\nx-agl-protocol: 1\r\n"));
                    assert!(normalized_header.contains("\r\nx-agl-attempt-id: run:1:1\r\n"));
                    break;
                }
            }
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .unwrap();
            let delta = frame_lines(&[json!({
                "schema": "agentlibre.llama-stream/v1",
                "attempt_id": "run:1:1",
                "sequence": 1,
                "kind": "delta",
                "content": "live"
            })]);
            write!(connection, "{:x}\r\n", delta.len()).unwrap();
            connection.write_all(&delta).unwrap();
            connection.write_all(b"\r\n").unwrap();
            connection.flush().unwrap();
            if progress_seen.recv_timeout(Duration::from_secs(1)).is_ok() {
                server_observed.store(true, Ordering::Release);
            }
            let terminal = frame_lines(&[final_frame(
                2,
                "live",
                json!({"role": "assistant", "content": "live"}),
            )]);
            write!(connection, "{:x}\r\n", terminal.len()).unwrap();
            connection.write_all(&terminal).unwrap();
            connection.write_all(b"\r\n0\r\n\r\n").unwrap();
        });
        let sink = InferenceProgressSink::new(move |_| {
            let _ = progress_sent.send(());
        });

        let response = generation_request(
            &socket,
            b"{}",
            &InferenceCancellation::new(),
            "run:1:1",
            Some(sink),
            false,
        )
        .unwrap();

        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
        assert!(observed_before_terminal.load(Ordering::Acquire));
        assert_eq!(response.status, 200);
        assert!(response.generated.is_some());
    }

    #[test]
    fn restored_cooldown_and_quarantine_remove_unsafe_profiles() {
        let profile = profile();
        let engine = EngineBuildDigest::parse(digest('4')).unwrap();
        let model = digest('5').parse().unwrap();
        let mut health = RestoredInferenceHealth {
            workers: vec![crate::inference::WorkerHealth {
                physical_device: profile.physical_device,
                driver_build: profile.driver_build,
                engine_build: engine,
                crash_streak: 1,
                retry_after_ms: i64::MAX,
                last_failure_kind: crate::inference::InferenceFailureKind::EngineCrash,
            }],
            quarantines: vec![],
        };
        assert!(!compatible_profile(&profile, &model, &engine, &health));
        health.workers.clear();
        health
            .quarantines
            .push(crate::inference::ResourceQuarantine {
                physical_device: profile.physical_device,
                driver_build: profile.driver_build,
                engine_build: engine,
                model,
                runtime_profile: profile.digest,
                admitted_host_bytes: 1,
                observed_host_bytes: 2,
                admitted_device_bytes: 0,
                observed_device_bytes: 0,
                admitted_shared_bytes: 0,
                observed_shared_bytes: 0,
                recorded_at_ms: 1,
            });
        assert!(!compatible_profile(
            &profile,
            &health.quarantines[0].model,
            &engine,
            &health
        ));
    }

    #[test]
    fn allocation_overage_returns_exact_quarantine_evidence() {
        let profile = profile();
        let allocations = vec![MemoryAllocation {
            pool: "host".into(),
            device: "cpu".into(),
            model_bytes: 2,
            context_bytes: 3,
            compute_bytes: 4,
        }];
        let Err(EngineStartError::InvalidAllocation(observed)) =
            validate_allocation(&profile, &allocations)
        else {
            panic!("allocation above the admitted profile must be quarantined")
        };
        assert_eq!(
            observed,
            ObservedAllocation {
                host: 9,
                device: 0,
                shared: 0,
            }
        );
    }

    #[test]
    fn device_loss_requires_an_exact_backend_symbol() {
        assert!(engine_diagnostic_is_device_lost(
            "ggml_vulkan: waitForFences error ErrorDeviceLost at backend.cpp:1"
        ));
        assert!(engine_diagnostic_is_device_lost(
            "vulkan returned VK_ERROR_DEVICE_LOST"
        ));
        assert!(!engine_diagnostic_is_device_lost(
            "the device lost contact with a remote worker"
        ));
        assert_eq!(
            worker_failure_kind(
                &InferenceServiceError::DeviceLost,
                Some(ExecutionOutcome::Signal {
                    signal: libc::SIGKILL
                }),
            ),
            Some(InferenceFailureKind::DeviceLost)
        );
        assert_eq!(
            worker_failure_kind(
                &InferenceServiceError::Unavailable,
                Some(ExecutionOutcome::Signal {
                    signal: libc::SIGKILL
                }),
            ),
            Some(InferenceFailureKind::UnattributedSignal {
                signal: libc::SIGKILL
            })
        );
        assert_eq!(
            worker_failure_kind(
                &InferenceServiceError::OutcomeUnknown,
                Some(ExecutionOutcome::UnknownAfterServiceRestart),
            ),
            None
        );
        assert_eq!(
            worker_failure_kind(
                &InferenceServiceError::OutcomeUnknown,
                Some(ExecutionOutcome::Signal {
                    signal: libc::SIGTERM
                }),
            ),
            Some(InferenceFailureKind::UnattributedSignal {
                signal: libc::SIGTERM
            })
        );
        assert_ne!(
            worker_failure_kind(
                &InferenceServiceError::Unavailable,
                Some(ExecutionOutcome::Signal {
                    signal: libc::SIGKILL
                }),
            ),
            Some(InferenceFailureKind::InvalidAllocation)
        );
    }

    fn frame_lines(frames: &[Value]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for frame in frames {
            serde_json::to_writer(&mut bytes, frame).unwrap();
            bytes.push(b'\n');
        }
        bytes
    }

    fn final_frame(sequence: u64, raw_output: &str, message: Value) -> Value {
        json!({
            "schema": "agentlibre.llama-stream/v1",
            "attempt_id": "run:1:1",
            "sequence": sequence,
            "kind": "final",
            "finish_reason": "stop",
            "raw_output": raw_output,
            "message": message,
            "usage": {"prompt_tokens": 3, "completion_tokens": 2},
            "prefill": {"tokens": 3, "cached_tokens": 0, "chunks": 1},
            "timings": {
                "cache_n": 1,
                "prompt_n": 2,
                "prompt_ms": 4.0,
                "prompt_per_token_ms": 2.0,
                "prompt_per_second": 500.0,
                "predicted_n": 2,
                "predicted_ms": 10.0,
                "predicted_per_token_ms": 5.0,
                "predicted_per_second": 200.0
            }
        })
    }

    #[test]
    fn private_stream_preserves_exact_deltas_and_terminal_output() {
        let bytes = frame_lines(&[
            json!({
                "schema": "agentlibre.llama-stream/v1",
                "attempt_id": "run:1:1",
                "sequence": 1,
                "kind": "delta",
                "content": "hel"
            }),
            json!({
                "schema": "agentlibre.llama-stream/v1",
                "attempt_id": "run:1:1",
                "sequence": 2,
                "kind": "delta",
                "content": "lo"
            }),
            final_frame(3, "hello", json!({"role": "assistant", "content": "hello"})),
        ]);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink_observed = observed.clone();
        let sink = InferenceProgressSink::new(move |content| {
            sink_observed.lock().unwrap().push(content);
        });

        let generated = decode_generation(&bytes, "run:1:1", Some(&sink), false, false).unwrap();

        assert_eq!(
            generated.output,
            ModelGenerationOutput::Assistant(Content::text("hello").unwrap())
        );
        assert_eq!(
            *observed.lock().unwrap(),
            vec![Content::text("hel").unwrap(), Content::text("lo").unwrap()]
        );
        assert_eq!(generated.timings.prompt_per_second, 500.0);
        assert_eq!(generated.timings.predicted_per_second, 200.0);
    }

    #[test]
    fn private_stream_requires_valid_native_timings() {
        let mut missing = final_frame(1, "hello", json!({"role": "assistant", "content": "hello"}));
        missing.as_object_mut().unwrap().remove("timings");
        assert!(matches!(
            decode_generation(&frame_lines(&[missing]), "run:1:1", None, false, false),
            Err(InferenceServiceError::InvalidRequest)
        ));

        let mut invalid = final_frame(1, "hello", json!({"role": "assistant", "content": "hello"}));
        invalid["timings"]["predicted_per_second"] = json!(-1.0);
        assert!(matches!(
            decode_generation(&frame_lines(&[invalid]), "run:1:1", None, false, false),
            Err(InferenceServiceError::InvalidResult)
        ));
    }

    #[test]
    fn private_stream_strips_gemma_channel_wrappers_from_raw_output() {
        let raw = "<|channel>thought\n<channel|>visible answer";
        let bytes = frame_lines(&[
            json!({
                "schema": "agentlibre.llama-stream/v1",
                "attempt_id": "run:1:1",
                "sequence": 1,
                "kind": "delta",
                "content": raw
            }),
            final_frame(
                2,
                raw,
                json!({"role": "assistant", "content": "visible answer"}),
            ),
        ]);

        let generated = decode_generation(&bytes, "run:1:1", None, false, false).unwrap();

        assert_eq!(
            generated.output,
            ModelGenerationOutput::Assistant(Content::text("visible answer").unwrap())
        );
    }

    #[test]
    fn reasoning_stream_buffers_private_content_for_internal_persistence() {
        let raw = "<think>private scratch</think>public answer";
        let bytes = frame_lines(&[
            json!({
                "schema": "agentlibre.llama-stream/v1",
                "attempt_id": "run:1:1",
                "sequence": 1,
                "kind": "delta",
                "content": raw
            }),
            final_frame(
                2,
                raw,
                json!({
                    "role": "assistant",
                    "reasoning_content": "private scratch",
                    "content": "public answer"
                }),
            ),
        ]);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink_observed = observed.clone();
        let sink = InferenceProgressSink::new(move |content| {
            sink_observed.lock().unwrap().push(content);
        });

        let generated = decode_generation(&bytes, "run:1:1", Some(&sink), false, true).unwrap();

        assert_eq!(
            generated.output,
            ModelGenerationOutput::Assistant(Content::text("public answer").unwrap())
        );
        assert_eq!(
            generated.private_reasoning,
            Some(Content::text("private scratch").unwrap())
        );
        assert!(observed.lock().unwrap().is_empty());
    }

    #[test]
    fn reasoning_projection_preserves_text_next_to_a_tool_call() {
        let message = json!({
            "role": "assistant",
            "reasoning_content": "private scratch",
            "content": "answer",
            "tool_calls": [{"function": {"name": "agentlibre.builtins:fs_read", "arguments": "{}"}}]
        });
        let (output, reasoning) = projected_reasoning_output(Some(&message)).unwrap();
        let ModelGenerationOutput::AssistantToolCall(mixed) = output else {
            panic!("expected mixed assistant and tool output")
        };
        assert_eq!(mixed.content.as_text(), "answer");
        assert_eq!(mixed.call.tool_id.as_str(), "agentlibre.builtins:fs_read");
        assert_eq!(reasoning.unwrap().as_text(), "private scratch");
        assert_eq!(
            projected_reasoning_output(Some(&json!({
                "role": "assistant",
                "reasoning_content": "private scratch",
                "content": ""
            })))
            .unwrap_err(),
            ModelMessageError::field("message.public_action.none")
        );
    }

    #[test]
    fn reasoning_projection_reports_safe_message_shape_diagnostics() {
        let cases = [
            (
                json!({"role":"assistant", "content":null, "tool_calls":[]}),
                "message.tool_calls.count=0",
            ),
            (
                json!({"role":"assistant", "content":null, "tool_calls":[{}, {}]}),
                "message.tool_calls[0].function",
            ),
            (
                json!({"role":"assistant", "content":7}),
                "message.content.type",
            ),
            (
                json!({"role":"assistant", "content":null, "tool_calls":[{"function":{"name":"read","arguments":"{"}}]}),
                "message.tool_calls[0].function.arguments.json",
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(
                projected_reasoning_output(Some(&message)).unwrap_err(),
                ModelMessageError::field(expected)
            );
        }
    }

    #[test]
    fn private_stream_rejects_sequence_and_raw_output_mismatches() {
        let sequence_mismatch = frame_lines(&[final_frame(
            2,
            "hello",
            json!({"role": "assistant", "content": "hello"}),
        )]);
        assert!(matches!(
            decode_generation(&sequence_mismatch, "run:1:1", None, false, false),
            Err(InferenceServiceError::IdentityMismatch)
        ));

        let raw_mismatch = frame_lines(&[
            json!({
                "schema": "agentlibre.llama-stream/v1",
                "attempt_id": "run:1:1",
                "sequence": 1,
                "kind": "delta",
                "content": "hello"
            }),
            final_frame(
                2,
                "different",
                json!({"role": "assistant", "content": "different"}),
            ),
        ]);
        assert!(matches!(
            decode_generation(&raw_mismatch, "run:1:1", None, false, false),
            Err(InferenceServiceError::InvalidResult)
        ));
    }

    #[test]
    fn private_stream_accepts_multiple_structured_tool_calls() {
        let raw = r#"{"tool":"agentlibre.builtins:fs_read","arguments":{"path":"a"}}"#;
        let bytes = frame_lines(&[
            json!({
                "schema": "agentlibre.llama-stream/v1",
                "attempt_id": "run:1:1",
                "sequence": 1,
                "kind": "delta",
                "content": raw
            }),
            final_frame(
                2,
                raw,
                json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {"function": {"name": "agentlibre.builtins:fs_read", "arguments": "{\"path\":\"a\"}"}},
                        {"function": {"name": "agentlibre.builtins:fs_read", "arguments": "{\"path\":\"b\"}"}}
                    ]
                }),
            ),
        ]);

        let generated = decode_generation(&bytes, "run:1:1", None, false, false).unwrap();
        let ModelGenerationOutput::ToolCalls(calls) = generated.output else {
            panic!("expected a structured tool call batch")
        };
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tool_id.as_str(), "agentlibre.builtins:fs_read");
        assert_eq!(calls[1].input["path"], "b");
    }

    #[test]
    fn invalid_empty_and_oversized_answers_keep_diagnostics_without_raw_content() {
        for raw in [String::new(), "x".repeat(1_048_577)] {
            let mut frames = Vec::new();
            if !raw.is_empty() {
                frames.push(json!({"schema":"agentlibre.llama-stream/v1", "attempt_id":"run:1:1", "sequence":1, "kind":"delta", "content":raw}));
            }
            frames.push(final_frame(
                frames.len() as u64 + 1,
                &raw,
                json!({"role":"assistant", "content":raw}),
            ));
            let bytes = frame_lines(&frames);
            let Err(InferenceServiceError::InvalidModelOutput(output)) =
                decode_generation(&bytes, "run:1:1", None, false, false)
            else {
                panic!("expected invalid answer diagnostic")
            };
            assert!(output.raw_output.is_none());
            assert_eq!(
                output.diagnostic.class,
                ModelOutputFailureClass::InvalidContent
            );
            assert_eq!(output.diagnostic.output_bytes, raw.len() as u64);
            assert_eq!(
                output.diagnostic.output_digest,
                PackageDigest::from_bytes(Sha256::digest(raw.as_bytes()).into())
            );
        }
    }

    #[test]
    fn cancellation_requires_a_terminal_error_frame() {
        let terminal_error = frame_lines(&[json!({
            "schema": "agentlibre.llama-stream/v1",
            "attempt_id": "run:1:1",
            "sequence": 1,
            "kind": "error",
            "error": {"message": "cancelled"}
        })]);
        assert!(matches!(
            decode_generation(&terminal_error, "run:1:1", None, true, false),
            Err(InferenceServiceError::Cancelled)
        ));
        assert!(matches!(
            decode_generation(&[], "run:1:1", None, true, false),
            Err(InferenceServiceError::OutcomeUnknown)
        ));
        let completed_after_cancellation = frame_lines(&[
            json!({
                "schema": "agentlibre.llama-stream/v1",
                "attempt_id": "run:1:1",
                "sequence": 1,
                "kind": "delta",
                "content": "too late"
            }),
            final_frame(
                2,
                "too late",
                json!({"role": "assistant", "content": "too late"}),
            ),
        ]);
        assert!(matches!(
            decode_generation(&completed_after_cancellation, "run:1:1", None, true, false),
            Err(InferenceServiceError::OutcomeUnknown)
        ));
        assert!(!error_requires_engine_restart(
            &InferenceServiceError::Cancelled
        ));
        assert!(!error_requires_engine_restart(
            &InferenceServiceError::InvalidRequest
        ));
        assert!(error_requires_engine_restart(
            &InferenceServiceError::OutcomeUnknown
        ));
    }
}
