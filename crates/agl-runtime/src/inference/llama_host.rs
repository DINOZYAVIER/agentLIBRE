mod output;
mod wire;

pub(crate) use output::*;
pub(crate) use wire::*;

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

pub(crate) struct ResidentEngine {
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

pub(crate) enum EngineStartError {
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

#[cfg(test)]
mod tests;
