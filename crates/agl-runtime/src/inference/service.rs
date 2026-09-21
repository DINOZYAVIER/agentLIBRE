use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::Duration;

use agl_core::Content;
use agl_core::agent::{
    AdmittedTool, AgentContextEntry, AgentOperationKey, InferenceRealizationRef, InstructionSet,
    ModelDefinitionRef, ModelGenerationRequest, ModelGenerationResult, ModelUsage,
};

use crate::inference::{InferenceHealthUpdate, RestoredInferenceHealth};

const INFERENCE_QUEUE_CAPACITY: usize = 1_024;
const MAX_INFERENCE_WORKERS: usize = 128;

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

#[derive(Clone)]
pub struct InferenceProgressSink {
    emit: Arc<dyn Fn(Content) + Send + Sync>,
}

impl InferenceProgressSink {
    pub fn new(emit: impl Fn(Content) + Send + Sync + 'static) -> Self {
        Self {
            emit: Arc::new(emit),
        }
    }

    pub fn emit(&self, content: Content) {
        (self.emit)(content);
    }
}

impl fmt::Debug for InferenceProgressSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("InferenceProgressSink")
    }
}

#[derive(Clone)]
pub struct InferenceHealthSink {
    persist: Arc<dyn Fn(Vec<InferenceHealthUpdate>) -> Result<(), ()> + Send + Sync>,
}

impl InferenceHealthSink {
    pub fn new(
        persist: impl Fn(Vec<InferenceHealthUpdate>) -> Result<(), ()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            persist: Arc::new(persist),
        }
    }

    fn persist(&self, updates: Vec<InferenceHealthUpdate>) -> Result<(), ()> {
        (self.persist)(updates)
    }
}

impl fmt::Debug for InferenceHealthSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("InferenceHealthSink")
    }
}

#[derive(Clone, Debug)]
pub struct InferenceGenerateRequest {
    pub operation: AgentOperationKey,
    pub delivery_attempt: NonZeroU32,
    pub model: ModelDefinitionRef,
    pub runtime: agl_core::agent::ModelRuntimeSelection,
    pub generation: ModelGenerationRequest,
    pub instructions: InstructionSet,
    pub context: Vec<AgentContextEntry>,
    pub tools: Vec<AdmittedTool>,
    /// Optional server-side structured-output contract for this generation.
    pub response_format: Option<serde_json::Value>,
    pub deadline_at_ms: i64,
    pub cancellation: InferenceCancellation,
    pub progress: Option<InferenceProgressSink>,
    pub health: Option<InferenceHealthSink>,
}

#[derive(Clone, Debug)]
pub struct InferenceGenerateResult {
    pub operation: AgentOperationKey,
    pub delivery_attempt: NonZeroU32,
    pub result: ModelGenerationResult,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RegisteredModelService {
    pub model: ModelDefinitionRef,
    pub runtime: agl_core::agent::ModelRuntimeSelection,
    pub artifact: crate::model::ImportedModel,
    pub adapters: Vec<crate::model::ImportedModel>,
}

pub trait InferenceGenerator: Send + Sync + 'static {
    fn measure(
        &self,
        request: InferenceGenerateRequest,
    ) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError>;

    fn generate(
        &self,
        request: InferenceGenerateRequest,
    ) -> Result<InferenceGenerateResult, InferenceServiceError>;

    fn take_health_updates(&self) -> Vec<InferenceHealthUpdate> {
        Vec::new()
    }

    fn register_service(
        &self,
        _service: RegisteredModelService,
    ) -> Result<(), InferenceServiceError> {
        Ok(())
    }

    fn unload_service(
        &self,
        _key: agl_core::agent::PackageDigest,
    ) -> Result<(), InferenceServiceError> {
        Ok(())
    }
}

pub trait InferenceRoute: Send + Sync + 'static {
    fn measure(
        &self,
        request: InferenceGenerateRequest,
    ) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError>;

    fn generate(
        &self,
        request: InferenceGenerateRequest,
    ) -> Result<ModelGenerationResult, InferenceServiceError>;
}

#[derive(Clone)]
pub struct InferenceConfig(InferenceBackendConfig);

#[derive(Clone)]
enum InferenceBackendConfig {
    Custom(Arc<dyn InferenceGenerator>),
    LlamaServer {
        config: crate::inference::LlamaServerConfig,
        engine_build: crate::inference::EngineBuildDigest,
        execution: agl_execution_api::BlockingExecutionClient,
    },
}

impl InferenceConfig {
    pub fn custom(generator: Arc<dyn InferenceGenerator>) -> Self {
        Self(InferenceBackendConfig::Custom(generator))
    }

    pub fn llama_server(
        config: crate::inference::LlamaServerConfig,
        execution: agl_execution_api::BlockingExecutionClient,
    ) -> Result<Self, InferenceServiceError> {
        let engine_build = crate::inference::private_engine_build_digest(&config.executable)
            .map_err(|_| InferenceServiceError::InvalidRequest)?;
        Ok(Self(InferenceBackendConfig::LlamaServer {
            config,
            engine_build,
            execution,
        }))
    }

    pub fn engine_build_digest(&self) -> crate::inference::EngineBuildDigest {
        match &self.0 {
            InferenceBackendConfig::Custom(_) => {
                crate::inference::EngineBuildDigest::from_bytes([0; 32])
            }
            InferenceBackendConfig::LlamaServer { engine_build, .. } => *engine_build,
        }
    }
}

#[derive(Clone)]
pub struct InferenceHandle {
    sender: mpsc::SyncSender<Command>,
    stopped: Arc<AtomicBool>,
}

impl InferenceHandle {
    pub fn register_service(
        &self,
        service: RegisteredModelService,
        health: InferenceHealthSink,
    ) -> Result<(), InferenceServiceError> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(InferenceServiceError::Stopped);
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        self.sender
            .try_send(Command::Register {
                service: Box::new(service),
                health,
                reply,
            })
            .map_err(map_send_error)?;
        receiver
            .recv()
            .map_err(|_| InferenceServiceError::Stopped)?
    }

    pub fn unload_service(
        &self,
        key: agl_core::agent::PackageDigest,
        health: InferenceHealthSink,
    ) -> Result<(), InferenceServiceError> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(InferenceServiceError::Stopped);
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        self.sender
            .try_send(Command::Unload { key, health, reply })
            .map_err(map_send_error)?;
        receiver
            .recv()
            .map_err(|_| InferenceServiceError::Stopped)?
    }

    pub fn generate(
        &self,
        request: InferenceGenerateRequest,
    ) -> Result<ModelGenerationResult, InferenceServiceError> {
        self.request(request, |request, reply| Command::Generate {
            request,
            reply,
        })
    }

    pub fn measure(
        &self,
        request: InferenceGenerateRequest,
    ) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError> {
        self.request(request, |request, reply| Command::Measure {
            request,
            reply,
        })
    }

    fn request<T>(
        &self,
        request: InferenceGenerateRequest,
        command: impl FnOnce(
            Box<InferenceGenerateRequest>,
            mpsc::SyncSender<Result<T, InferenceServiceError>>,
        ) -> Command,
    ) -> Result<T, InferenceServiceError> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(InferenceServiceError::Stopped);
        }
        if request.cancellation.is_cancelled() {
            return Err(InferenceServiceError::Cancelled);
        }
        if request.deadline_at_ms <= unix_ms() {
            return Err(InferenceServiceError::Deadline);
        }
        let cancellation = request.cancellation.clone();
        let deadline_at_ms = request.deadline_at_ms;
        let (reply, receiver) = mpsc::sync_channel(1);
        self.sender
            .try_send(command(Box::new(request), reply))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => InferenceServiceError::Unavailable,
                mpsc::TrySendError::Disconnected(_) => InferenceServiceError::Stopped,
            })?;
        let remaining_ms = deadline_at_ms.saturating_sub(unix_ms()).max(1) as u64;
        match receiver.recv_timeout(Duration::from_millis(remaining_ms)) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(InferenceServiceError::Stopped),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                cancellation.cancel();
                match receiver.recv_timeout(Duration::from_secs(2)) {
                    Ok(Err(InferenceServiceError::OutcomeUnknown)) => {
                        Err(InferenceServiceError::OutcomeUnknown)
                    }
                    Ok(_) => Err(InferenceServiceError::Deadline),
                    Err(_) => Err(InferenceServiceError::OutcomeUnknown),
                }
            }
        }
    }
}

impl InferenceRoute for InferenceHandle {
    fn measure(
        &self,
        request: InferenceGenerateRequest,
    ) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError> {
        InferenceHandle::measure(self, request)
    }

    fn generate(
        &self,
        request: InferenceGenerateRequest,
    ) -> Result<ModelGenerationResult, InferenceServiceError> {
        InferenceHandle::generate(self, request)
    }
}

pub struct InferenceService {
    stopped: Arc<AtomicBool>,
    sender: mpsc::SyncSender<Command>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

impl InferenceService {
    pub fn start(
        config: InferenceConfig,
        restored_health: RestoredInferenceHealth,
    ) -> Result<(Self, InferenceHandle), InferenceServiceError> {
        validate_restored_health(&restored_health)?;
        let generator = match config.0 {
            InferenceBackendConfig::Custom(generator) => generator,
            InferenceBackendConfig::LlamaServer {
                config,
                engine_build,
                execution,
            } => crate::inference::llama_host::generator(
                config,
                engine_build,
                execution,
                &restored_health,
            )?,
        };
        let stopped = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel(INFERENCE_QUEUE_CAPACITY);
        let worker_generator = generator.clone();
        let worker = thread::Builder::new()
            .name("agl-runtime-inference".into())
            .spawn(move || inference_loop(receiver, worker_generator))
            .map_err(|_| InferenceServiceError::Unavailable)?;
        Ok((
            Self {
                stopped: stopped.clone(),
                sender: sender.clone(),
                worker: Mutex::new(Some(worker)),
            },
            InferenceHandle { sender, stopped },
        ))
    }

    pub fn shutdown(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            let _ = self.sender.send(Command::Shutdown);
            if let Ok(mut worker) = self.worker.lock()
                && let Some(worker) = worker.take()
            {
                let _ = worker.join();
            }
        }
    }
}

impl Drop for InferenceService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum Command {
    Measure {
        request: Box<InferenceGenerateRequest>,
        reply: mpsc::SyncSender<Result<agl_core::agent::ContextCapacity, InferenceServiceError>>,
    },
    Register {
        service: Box<RegisteredModelService>,
        health: InferenceHealthSink,
        reply: mpsc::SyncSender<Result<(), InferenceServiceError>>,
    },
    Unload {
        key: agl_core::agent::PackageDigest,
        health: InferenceHealthSink,
        reply: mpsc::SyncSender<Result<(), InferenceServiceError>>,
    },
    Generate {
        request: Box<InferenceGenerateRequest>,
        reply: mpsc::SyncSender<Result<ModelGenerationResult, InferenceServiceError>>,
    },
    Shutdown,
}

fn inference_loop(receiver: mpsc::Receiver<Command>, generator: Arc<dyn InferenceGenerator>) {
    let mut workers = Vec::<thread::JoinHandle<()>>::new();
    while let Ok(command) = receiver.recv() {
        reap_finished(&mut workers);
        match command {
            Command::Measure { request, reply } => {
                wait_for_worker_capacity(&mut workers);
                let generator = Arc::clone(&generator);
                if let Ok(worker) = thread::Builder::new()
                    .name("agl-runtime-inference-measure".into())
                    .spawn(move || {
                        let health = request.health.clone();
                        let measured = generator.measure(*request);
                        let persisted = persist_health(&*generator, health);
                        let _ = reply.send(persisted.and(measured));
                    })
                {
                    workers.push(worker);
                }
            }
            Command::Register {
                service,
                health,
                reply,
            } => {
                wait_for_worker_capacity(&mut workers);
                let generator = Arc::clone(&generator);
                if let Ok(worker) = thread::Builder::new()
                    .name("agl-runtime-inference-register".into())
                    .spawn(move || {
                        let lifecycle = generator.register_service(*service);
                        let persisted = persist_health(&*generator, Some(health));
                        let _ = reply.send(persisted.and(lifecycle));
                    })
                {
                    workers.push(worker);
                }
            }
            Command::Unload { key, health, reply } => {
                wait_for_worker_capacity(&mut workers);
                let generator = Arc::clone(&generator);
                if let Ok(worker) = thread::Builder::new()
                    .name("agl-runtime-inference-unload".into())
                    .spawn(move || {
                        let lifecycle = generator.unload_service(key);
                        let persisted = persist_health(&*generator, Some(health));
                        let _ = reply.send(persisted.and(lifecycle));
                    })
                {
                    workers.push(worker);
                }
            }
            Command::Generate { request, reply } => {
                wait_for_worker_capacity(&mut workers);
                let generator = Arc::clone(&generator);
                match thread::Builder::new()
                    .name(format!(
                        "agl-runtime-inference-{}",
                        request.operation.run_id
                    ))
                    .spawn(move || generate_one(generator, *request, reply))
                {
                    Ok(worker) => workers.push(worker),
                    Err(_) => {
                        // The receiver was moved only when the worker was created.
                        // A spawn failure is therefore represented by a disconnected
                        // reply and maps to Stopped at the caller boundary.
                    }
                }
            }
            Command::Shutdown => break,
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
}

fn wait_for_worker_capacity(workers: &mut Vec<thread::JoinHandle<()>>) {
    while workers.len() >= MAX_INFERENCE_WORKERS {
        let worker = workers.swap_remove(0);
        let _ = worker.join();
        reap_finished(workers);
    }
}

fn generate_one(
    generator: Arc<dyn InferenceGenerator>,
    request: InferenceGenerateRequest,
    reply: mpsc::SyncSender<Result<ModelGenerationResult, InferenceServiceError>>,
) {
    let operation = request.operation.clone();
    let delivery_attempt = request.delivery_attempt;
    let health = request.health.clone();
    let generated = generator.generate(request);
    let health_result = persist_health(&*generator, health);
    let result = health_result.and(generated).and_then(|generated| {
        if generated.operation != operation || generated.delivery_attempt != delivery_attempt {
            Err(InferenceServiceError::IdentityMismatch)
        } else {
            Ok(generated.result)
        }
    });
    let _ = reply.send(result);
}

fn persist_health(
    generator: &dyn InferenceGenerator,
    health: Option<InferenceHealthSink>,
) -> Result<(), InferenceServiceError> {
    let updates = generator.take_health_updates();
    if updates.is_empty() {
        Ok(())
    } else {
        health
            .ok_or(InferenceServiceError::Unavailable)
            .and_then(|sink| {
                sink.persist(updates)
                    .map_err(|()| InferenceServiceError::Unavailable)
            })
    }
}

fn reap_finished(workers: &mut Vec<thread::JoinHandle<()>>) {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].is_finished() {
            let worker = workers.swap_remove(index);
            let _ = worker.join();
        } else {
            index += 1;
        }
    }
}

fn map_send_error(error: mpsc::TrySendError<Command>) -> InferenceServiceError {
    match error {
        mpsc::TrySendError::Full(_) => InferenceServiceError::Unavailable,
        mpsc::TrySendError::Disconnected(_) => InferenceServiceError::Stopped,
    }
}

fn validate_restored_health(health: &RestoredInferenceHealth) -> Result<(), InferenceServiceError> {
    if health.workers.iter().any(|worker| {
        worker.retry_after_ms < 0
            || health
                .workers
                .iter()
                .filter(|other| {
                    other.physical_device == worker.physical_device
                        && other.driver_build == worker.driver_build
                        && other.engine_build == worker.engine_build
                })
                .count()
                != 1
    }) || health
        .quarantines
        .iter()
        .any(|entry| entry.recorded_at_ms < 0)
    {
        return Err(InferenceServiceError::InvalidRequest);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidModelOutput {
    pub raw_output: Option<Content>,
    pub diagnostic: agl_core::agent::ModelOutputDiagnostic,
    pub usage: ModelUsage,
    pub realization: Option<InferenceRealizationRef>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InferenceServiceError {
    Stopped,
    InvalidRequest,
    InvalidResult,
    InvalidModelOutput(Box<InvalidModelOutput>),
    ContextExhausted(agl_core::agent::ContextCapacity),
    Unavailable,
    UnavailableWithReason(String),
    DeviceLost,
    Cancelled,
    Deadline,
    OutcomeUnknown,
    IdentityMismatch,
}

impl fmt::Display for InferenceServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Stopped => "inference service is stopped",
            Self::InvalidRequest => "invalid inference request",
            Self::InvalidResult => "invalid inference result",
            Self::InvalidModelOutput(_) => "invalid model output",
            Self::ContextExhausted(_) => "inference context is exhausted",
            Self::Unavailable => "inference is unavailable",
            Self::UnavailableWithReason(reason) => {
                return write!(formatter, "inference is unavailable: {reason}");
            }
            Self::DeviceLost => "inference device was lost",
            Self::Cancelled => "inference was cancelled",
            Self::Deadline => "inference deadline elapsed",
            Self::OutcomeUnknown => "inference outcome is unknown",
            Self::IdentityMismatch => "inference result identity does not match its request",
        })
    }
}

impl std::error::Error for InferenceServiceError {}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) fn test_runtime_selection() -> agl_core::agent::ModelRuntimeSelection {
    use agl_core::agent::{
        GenerationSettings, GpuLayerSelection, KvCacheType, ModelArtifactKind, ModelArtifactRef,
        ModelLoadSelection, ModelRuntimeSelection, ModelServiceSelection, PackageDigest, SplitMode,
    };

    ModelRuntimeSelection {
        artifact: ModelArtifactRef {
            kind: ModelArtifactKind::Gguf,
            url: "https://example.invalid/model.gguf".to_owned(),
            digest: PackageDigest::from_bytes([8; 32]),
            bytes: 4,
        },
        dialect: agl_core::agent::ModelDialect::Generic,
        tool_call_format: agl_core::agent::ToolCallFormat::HermesJson,
        generation: GenerationSettings {
            max_output_tokens: 32,
            seed: 1,
            temperature: 0.0,
            top_k: 1,
            top_p: 1.0,
            min_p: 0.0,
            typical_p: 1.0,
            repeat_last_n: 64,
            repeat_penalty: 1.0,
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            stop: vec![],
        },
        reasoning: agl_core::agent::ReasoningSelection::Disabled,
        speculative: None,
        load: ModelLoadSelection {
            context_tokens: 128,
            batch_size: 8,
            ubatch_size: 8,
            threads: 1,
            threads_batch: 1,
            gpu_layers: GpuLayerSelection::Count(0),
            devices: vec![],
            split_mode: SplitMode::None,
            main_gpu: None,
            tensor_split: vec![],
            mmap: true,
            mlock: false,
            flash_attention: false,
            kv_cache_type_k: KvCacheType::F16,
            kv_cache_type_v: KvCacheType::F16,
            engine_build_digest: PackageDigest::from_bytes([9; 32]),
        },
        adapters: vec![],
        service: ModelServiceSelection {
            key: PackageDigest::from_bytes([10; 32]),
            slots: 1,
            queue_capacity: 32,
            continuous_batching: true,
            idle_timeout_ms: 900_000,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::package::{PackageId, PackageVersion};
    use agl_core::AgentRunId;
    use agl_core::Content;
    use agl_core::agent::{
        AgentOperationKey, InferenceEngineBuildDigest, InferenceRealizationRef,
        InferenceRuntimeProfileDigest, InstructionSet, ModelDefinitionRef, ModelFinishReason,
        ModelGenerationOutput, ModelGenerationRequest, ModelGenerationResult, ModelUsage,
        PackageDigest, PhysicalResourceDigest,
    };
    use sha2::{Digest as _, Sha256};

    use super::*;

    struct ExactGenerator {
        calls: AtomicUsize,
        mismatch_operation: bool,
        mismatch_attempt: bool,
    }

    struct HealthGenerator {
        updates: Mutex<Vec<InferenceHealthUpdate>>,
    }

    struct BlockingRegistrationGenerator {
        inner: ExactGenerator,
        started: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    struct BlockingGenerationGenerator {
        inner: ExactGenerator,
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl InferenceGenerator for HealthGenerator {
        fn measure(
            &self,
            request: InferenceGenerateRequest,
        ) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError> {
            // This fake model has an empty token stream; no production tokenizer is substituted.
            Ok(agl_core::agent::ContextCapacity::new(
                0,
                request.generation.max_output_tokens,
                request.runtime.load.context_tokens,
            ))
        }

        fn generate(
            &self,
            request: InferenceGenerateRequest,
        ) -> Result<InferenceGenerateResult, InferenceServiceError> {
            Ok(InferenceGenerateResult {
                operation: request.operation,
                delivery_attempt: request.delivery_attempt,
                result: ModelGenerationResult {
                    private_reasoning: None,
                    output: ModelGenerationOutput::Assistant(Content::text("ok").unwrap()),
                    finish_reason: ModelFinishReason::Stop,
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    realization: InferenceRealizationRef {
                        runtime_profile_digest: InferenceRuntimeProfileDigest::from_bytes([1; 32]),
                        engine_build_digest: InferenceEngineBuildDigest::from_bytes([2; 32]),
                        physical_resource_digest: PhysicalResourceDigest::from_bytes([3; 32]),
                    },
                    correction: None,
                },
            })
        }

        fn take_health_updates(&self) -> Vec<InferenceHealthUpdate> {
            std::mem::take(&mut *self.updates.lock().unwrap())
        }
    }

    impl InferenceGenerator for ExactGenerator {
        fn measure(
            &self,
            request: InferenceGenerateRequest,
        ) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError> {
            // This fake model has an empty token stream; no production tokenizer is substituted.
            Ok(agl_core::agent::ContextCapacity::new(
                0,
                request.generation.max_output_tokens,
                request.runtime.load.context_tokens,
            ))
        }

        fn generate(
            &self,
            request: InferenceGenerateRequest,
        ) -> Result<InferenceGenerateResult, InferenceServiceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(InferenceGenerateResult {
                operation: if self.mismatch_operation {
                    AgentOperationKey {
                        run_id: AgentRunId::generate(),
                        ordinal: NonZeroU32::MIN,
                    }
                } else {
                    request.operation
                },
                delivery_attempt: if self.mismatch_attempt {
                    NonZeroU32::new(request.delivery_attempt.get() + 1).unwrap()
                } else {
                    request.delivery_attempt
                },
                result: ModelGenerationResult {
                    private_reasoning: None,
                    output: ModelGenerationOutput::Assistant(Content::text("ok").unwrap()),
                    finish_reason: ModelFinishReason::Stop,
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    realization: InferenceRealizationRef {
                        runtime_profile_digest: InferenceRuntimeProfileDigest::from_bytes([1; 32]),
                        engine_build_digest: InferenceEngineBuildDigest::from_bytes([2; 32]),
                        physical_resource_digest: PhysicalResourceDigest::from_bytes([3; 32]),
                    },
                    correction: None,
                },
            })
        }
    }

    impl InferenceGenerator for BlockingRegistrationGenerator {
        fn measure(
            &self,
            request: InferenceGenerateRequest,
        ) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError> {
            // This fake model has an empty token stream; no production tokenizer is substituted.
            Ok(agl_core::agent::ContextCapacity::new(
                0,
                request.generation.max_output_tokens,
                request.runtime.load.context_tokens,
            ))
        }

        fn generate(
            &self,
            request: InferenceGenerateRequest,
        ) -> Result<InferenceGenerateResult, InferenceServiceError> {
            self.inner.generate(request)
        }

        fn register_service(
            &self,
            _service: RegisteredModelService,
        ) -> Result<(), InferenceServiceError> {
            self.started
                .send(())
                .map_err(|_| InferenceServiceError::Stopped)?;
            self.release
                .lock()
                .map_err(|_| InferenceServiceError::Unavailable)?
                .recv()
                .map_err(|_| InferenceServiceError::Stopped)
        }
    }

    impl InferenceGenerator for BlockingGenerationGenerator {
        fn measure(
            &self,
            request: InferenceGenerateRequest,
        ) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError> {
            // This fake model has an empty token stream; no production tokenizer is substituted.
            Ok(agl_core::agent::ContextCapacity::new(
                0,
                request.generation.max_output_tokens,
                request.runtime.load.context_tokens,
            ))
        }

        fn generate(
            &self,
            request: InferenceGenerateRequest,
        ) -> Result<InferenceGenerateResult, InferenceServiceError> {
            self.entered
                .send(())
                .map_err(|_| InferenceServiceError::Stopped)?;
            self.release
                .lock()
                .map_err(|_| InferenceServiceError::Unavailable)?
                .recv()
                .map_err(|_| InferenceServiceError::Stopped)?;
            self.inner.generate(request)
        }
    }

    fn request(cancellation: InferenceCancellation) -> InferenceGenerateRequest {
        InferenceGenerateRequest {
            operation: AgentOperationKey {
                run_id: AgentRunId::generate(),
                ordinal: NonZeroU32::MIN,
            },
            delivery_attempt: NonZeroU32::MIN,
            model: ModelDefinitionRef {
                id: PackageId::new("test-model").unwrap(),
                version: PackageVersion::new("1.0.0").unwrap(),
                digest: PackageDigest::from_bytes([7; 32]),
            },
            runtime: test_runtime_selection(),
            generation: ModelGenerationRequest {
                context: vec![],
                max_output_tokens: 16,
            },
            instructions: InstructionSet::new(vec![]).unwrap(),
            context: vec![],
            tools: vec![],
            response_format: None,
            deadline_at_ms: unix_ms() + 10_000,
            cancellation,
            progress: None,
            health: None,
        }
    }

    fn imported_model() -> crate::model::ImportedModel {
        let bytes = b"GGUF-service-test";
        let digest = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = std::env::temp_dir().join(format!(
            "agl-runtime-inference-service-{}-{}.gguf",
            std::process::id(),
            AgentRunId::generate()
        ));
        std::fs::write(&path, bytes).unwrap();
        let imported = crate::model::import_model(&crate::model::ModelImportRequest {
            path: path.clone(),
            expected_digest: crate::model::ModelArtifactDigest::parse(format!("sha256:{digest}"))
                .unwrap(),
        })
        .unwrap();
        std::fs::remove_file(path).unwrap();
        imported
    }

    #[test]
    fn measurement_does_not_invoke_generation_and_obeys_request_lifecycle() {
        let generator = Arc::new(ExactGenerator {
            calls: AtomicUsize::new(0),
            mismatch_operation: false,
            mismatch_attempt: false,
        });
        let (service, handle) = InferenceService::start(
            InferenceConfig::custom(generator.clone()),
            RestoredInferenceHealth::default(),
        )
        .unwrap();
        let sample = request(InferenceCancellation::new());
        let measured = handle.measure(sample.clone()).unwrap();
        assert_eq!(measured.prompt_tokens, 0); // the fixture model's defined empty token stream
        assert_eq!(
            measured.reserved_output_tokens,
            sample.generation.max_output_tokens
        );
        assert_eq!(
            measured.context_capacity_tokens,
            sample.runtime.load.context_tokens
        );
        let cancelled = request(InferenceCancellation::new());
        cancelled.cancellation.cancel();
        assert_eq!(
            handle.measure(cancelled),
            Err(InferenceServiceError::Cancelled)
        );
        let mut expired = sample.clone();
        expired.deadline_at_ms = unix_ms();
        assert_eq!(
            handle.measure(expired),
            Err(InferenceServiceError::Deadline)
        );
        assert_eq!(generator.calls.load(Ordering::SeqCst), 0);
        service.shutdown();
        assert_eq!(handle.measure(sample), Err(InferenceServiceError::Stopped));
    }

    #[test]
    fn rejects_cancelled_and_expired_requests_before_dispatch() {
        let generator = Arc::new(ExactGenerator {
            calls: AtomicUsize::new(0),
            mismatch_operation: false,
            mismatch_attempt: false,
        });
        let (service, handle) = InferenceService::start(
            InferenceConfig::custom(generator.clone()),
            RestoredInferenceHealth::default(),
        )
        .unwrap();

        let cancellation = InferenceCancellation::new();
        cancellation.cancel();
        assert_eq!(
            handle.generate(request(cancellation)).unwrap_err(),
            InferenceServiceError::Cancelled
        );
        let mut expired = request(InferenceCancellation::new());
        expired.deadline_at_ms = unix_ms();
        assert_eq!(
            handle.generate(expired).unwrap_err(),
            InferenceServiceError::Deadline
        );
        assert_eq!(generator.calls.load(Ordering::SeqCst), 0);
        service.shutdown();
    }

    #[test]
    fn service_registration_does_not_block_generation_dispatch() {
        let (started, observed_start) = mpsc::sync_channel(1);
        let (release, wait_for_release) = mpsc::sync_channel(1);
        let generator = Arc::new(BlockingRegistrationGenerator {
            inner: ExactGenerator {
                calls: AtomicUsize::new(0),
                mismatch_operation: false,
                mismatch_attempt: false,
            },
            started,
            release: Mutex::new(wait_for_release),
        });
        let (service, handle) = InferenceService::start(
            InferenceConfig::custom(generator),
            RestoredInferenceHealth::default(),
        )
        .unwrap();
        let sample = request(InferenceCancellation::new());
        let registration = RegisteredModelService {
            model: sample.model.clone(),
            runtime: sample.runtime.clone(),
            artifact: imported_model(),
            adapters: vec![],
        };
        let register_handle = handle.clone();
        let registration_worker = thread::spawn(move || {
            register_handle.register_service(registration, InferenceHealthSink::new(|_| Ok(())))
        });
        observed_start.recv_timeout(Duration::from_secs(1)).unwrap();

        let generate_handle = handle.clone();
        let (generated, observed_generation) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = generated.send(generate_handle.generate(sample));
        });
        assert!(
            observed_generation
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_ok()
        );

        release.send(()).unwrap();
        registration_worker.join().unwrap().unwrap();
        service.shutdown();
    }

    #[test]
    fn compatible_generation_requests_are_dispatched_concurrently() {
        let (entered, observed_entry) = mpsc::sync_channel(2);
        let (release, wait_for_release) = mpsc::sync_channel(2);
        let generator = Arc::new(BlockingGenerationGenerator {
            inner: ExactGenerator {
                calls: AtomicUsize::new(0),
                mismatch_operation: false,
                mismatch_attempt: false,
            },
            entered,
            release: Mutex::new(wait_for_release),
        });
        let (service, handle) = InferenceService::start(
            InferenceConfig::custom(generator),
            RestoredInferenceHealth::default(),
        )
        .unwrap();
        let workers = (0..2)
            .map(|_| {
                let handle = handle.clone();
                thread::spawn(move || handle.generate(request(InferenceCancellation::new())))
            })
            .collect::<Vec<_>>();

        observed_entry.recv_timeout(Duration::from_secs(1)).unwrap();
        observed_entry.recv_timeout(Duration::from_secs(1)).unwrap();
        release.send(()).unwrap();
        release.send(()).unwrap();
        for worker in workers {
            assert!(worker.join().unwrap().is_ok());
        }
        service.shutdown();
    }

    #[test]
    fn rejects_a_result_for_another_operation_or_delivery_attempt() {
        for (mismatch_operation, mismatch_attempt) in [(true, false), (false, true)] {
            let generator = Arc::new(ExactGenerator {
                calls: AtomicUsize::new(0),
                mismatch_operation,
                mismatch_attempt,
            });
            let (service, handle) = InferenceService::start(
                InferenceConfig::custom(generator),
                RestoredInferenceHealth::default(),
            )
            .unwrap();
            assert_eq!(
                handle
                    .generate(request(InferenceCancellation::new()))
                    .unwrap_err(),
                InferenceServiceError::IdentityMismatch
            );
            service.shutdown();
        }
    }

    #[test]
    fn restored_health_rejects_duplicate_worker_identity() {
        let worker = crate::inference::WorkerHealth {
            physical_device: crate::inference::PhysicalDeviceDigest::from_bytes([1; 32]),
            driver_build: crate::inference::DriverBuildDigest::from_bytes([2; 32]),
            engine_build: crate::inference::EngineBuildDigest::from_bytes([3; 32]),
            crash_streak: 1,
            retry_after_ms: 1,
            last_failure_kind: crate::inference::InferenceFailureKind::Unavailable,
        };
        let generator = Arc::new(ExactGenerator {
            calls: AtomicUsize::new(0),
            mismatch_operation: false,
            mismatch_attempt: false,
        });
        assert!(matches!(
            InferenceService::start(
                InferenceConfig::custom(generator),
                RestoredInferenceHealth {
                    workers: vec![worker.clone(), worker],
                    quarantines: vec![],
                },
            ),
            Err(InferenceServiceError::InvalidRequest)
        ));
    }

    #[test]
    fn health_updates_are_persisted_before_generation_returns() {
        let update = InferenceHealthUpdate::Worker(crate::inference::WorkerHealth {
            physical_device: crate::inference::PhysicalDeviceDigest::from_bytes([1; 32]),
            driver_build: crate::inference::DriverBuildDigest::from_bytes([2; 32]),
            engine_build: crate::inference::EngineBuildDigest::from_bytes([3; 32]),
            crash_streak: 1,
            retry_after_ms: 1,
            last_failure_kind: crate::inference::InferenceFailureKind::EngineCrash,
        });
        let (service, handle) = InferenceService::start(
            InferenceConfig::custom(Arc::new(HealthGenerator {
                updates: Mutex::new(vec![update.clone()]),
            })),
            RestoredInferenceHealth::default(),
        )
        .unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink_observed = observed.clone();
        let mut request = request(InferenceCancellation::new());
        request.health = Some(InferenceHealthSink::new(move |updates| {
            *sink_observed.lock().unwrap() = updates;
            Ok(())
        }));

        handle.generate(request).unwrap();

        assert_eq!(*observed.lock().unwrap(), vec![update]);
        service.shutdown();
    }

    #[test]
    fn lifecycle_health_persistence_failure_reaches_the_caller() {
        let update = InferenceHealthUpdate::Worker(crate::inference::WorkerHealth {
            physical_device: crate::inference::PhysicalDeviceDigest::from_bytes([1; 32]),
            driver_build: crate::inference::DriverBuildDigest::from_bytes([2; 32]),
            engine_build: crate::inference::EngineBuildDigest::from_bytes([3; 32]),
            crash_streak: 1,
            retry_after_ms: 1,
            last_failure_kind: crate::inference::InferenceFailureKind::EngineCrash,
        });
        let (service, handle) = InferenceService::start(
            InferenceConfig::custom(Arc::new(HealthGenerator {
                updates: Mutex::new(vec![update]),
            })),
            RestoredInferenceHealth::default(),
        )
        .unwrap();
        let sample = request(InferenceCancellation::new());
        let result = handle.register_service(
            RegisteredModelService {
                model: sample.model,
                runtime: sample.runtime,
                artifact: imported_model(),
                adapters: vec![],
            },
            InferenceHealthSink::new(|_| Err(())),
        );

        assert_eq!(result, Err(InferenceServiceError::Unavailable));
        service.shutdown();
    }
}
