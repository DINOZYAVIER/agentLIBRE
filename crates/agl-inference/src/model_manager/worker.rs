use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::output::PublicInferenceOutputBroker;
use crate::{
    InferenceDeviceInfo, InferenceFinishReason, InferenceOutputSink, InferenceProductStage,
    InferenceResponse, InferenceResponseMetadata,
};
use agl_config::ResolvedInferenceConfig;
use serde::Serialize;

use super::evidence::AttemptEvidence;
use super::queue::{
    ActiveModelTarget, EnqueueResult, ManagerClock, PendingQueue, PendingWaitGuard, QueueCommand,
    QueueWake, SystemManagerClock, WaitAbandonReason,
};
use super::types::SharedManagerStatus;
use super::{
    ContextKey, InferenceJob, MAX_STATUS_MODEL_DIGESTS, ModelGeneration, ModelKey,
    ModelManagerError, ModelManagerOptions, ModelManagerStatus, ModelManagerStatusDetail,
    ModelReleaseOutcome, ModelReleaseReason, ModelUnloadOutcome, ModelUnloadResult,
    ModelUnloadTarget, RuntimeFailure, RuntimeOperation,
};

const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(10);
static NEXT_MODEL_LEASE: AtomicU64 = AtomicU64::new(1);

pub trait ModelRuntime: Send + 'static {
    type Model: 'static;
    type Context: 'static;

    fn device_inventory(&mut self) -> Result<Vec<InferenceDeviceInfo>, RuntimeFailure>;

    fn load_model(
        &mut self,
        job: &InferenceJob,
    ) -> Result<RuntimeOperation<Self::Model>, RuntimeFailure>;

    fn create_context(
        &mut self,
        model: &mut Self::Model,
        job: &InferenceJob,
    ) -> Result<RuntimeOperation<Self::Context>, RuntimeFailure>;

    fn generate(
        &mut self,
        model: &mut Self::Model,
        context: &mut Self::Context,
        job: &InferenceJob,
    ) -> Result<RuntimeOperation<ModelGeneration>, RuntimeFailure>;

    fn clear_context(
        &mut self,
        model: &mut Self::Model,
        context: &mut Self::Context,
    ) -> Result<RuntimeOperation<()>, RuntimeFailure>;

    fn release_context(
        &mut self,
        model: &mut Self::Model,
        context: &mut Self::Context,
    ) -> Result<RuntimeOperation<()>, RuntimeFailure>;

    fn release_model(
        &mut self,
        model: &mut Self::Model,
    ) -> Result<RuntimeOperation<()>, RuntimeFailure>;
}

pub struct ModelManager {
    handle: ModelManagerHandle,
    worker: Option<JoinHandle<()>>,
}

impl ModelManager {
    pub fn spawn<R>(options: ModelManagerOptions, runtime: R) -> Result<Self, ModelManagerError>
    where
        R: ModelRuntime,
    {
        Self::spawn_with_clock(options, runtime, Arc::new(SystemManagerClock))
    }

    fn spawn_with_clock<R>(
        options: ModelManagerOptions,
        runtime: R,
        clock: Arc<dyn ManagerClock>,
    ) -> Result<Self, ModelManagerError>
    where
        R: ModelRuntime,
    {
        options.validate()?;
        let status = Arc::new(Mutex::new(SharedManagerStatus::default()));
        let queue = Arc::new(PendingQueue::with_clock(
            options.queue_capacity,
            Arc::clone(&status),
            Arc::clone(&clock),
        ));
        let worker_status = Arc::clone(&status);
        let worker_queue = Arc::clone(&queue);
        let worker_options = options.clone();
        let worker_clock = Arc::clone(&clock);
        let worker = thread::Builder::new()
            .name("agl-model-manager".to_string())
            .spawn(move || {
                let mut availability = AvailabilityGuard::new(Arc::clone(&worker_queue));
                if Worker::new(runtime, worker_options, worker_status, worker_clock)
                    .run(worker_queue)
                    .is_ok()
                {
                    availability.finish();
                }
            })
            .map_err(|_| ModelManagerError::ManagerUnavailable)?;
        Ok(Self {
            handle: ModelManagerHandle {
                inner: Arc::new(HandleInner {
                    queue,
                    status,
                    clock,
                }),
            },
            worker: Some(worker),
        })
    }

    #[cfg(test)]
    pub(super) fn spawn_for_test<R>(
        options: ModelManagerOptions,
        runtime: R,
        clock: Arc<dyn ManagerClock>,
    ) -> Result<Self, ModelManagerError>
    where
        R: ModelRuntime,
    {
        Self::spawn_with_clock(options, runtime, clock)
    }

    pub fn handle(&self) -> ModelManagerHandle {
        self.handle.clone()
    }

    pub fn shutdown(&mut self) -> Result<(), ModelManagerError> {
        let result = self.handle.shutdown();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            return Err(ModelManagerError::ManagerUnavailable);
        }
        result
    }
}

impl Drop for ModelManager {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[derive(Clone)]
pub struct ModelManagerHandle {
    inner: Arc<HandleInner>,
}

struct HandleInner {
    queue: Arc<PendingQueue<Command>>,
    status: Arc<Mutex<SharedManagerStatus>>,
    clock: Arc<dyn ManagerClock>,
}

impl ModelManagerHandle {
    pub fn device_inventory(&self) -> Result<Vec<InferenceDeviceInfo>, ModelManagerError> {
        let (reply, receiver) = mpsc::channel();
        let id = self
            .inner
            .queue
            .enqueue(Command::DeviceInventory { reply })?;
        let mut guard = PendingWaitGuard::new(Arc::clone(&self.inner.queue), id);
        let result = receiver
            .recv()
            .map_err(|_| ModelManagerError::ManagerUnavailable)?;
        guard.disarm();
        result
    }

    pub fn generate(&self, mut job: InferenceJob) -> Result<InferenceResponse, ModelManagerError> {
        check_job_gate(&job)?;
        let cancellation = job.cancellation().clone();
        let deadline = job.deadline();
        let stages = Arc::new(PublicInferenceOutputBroker::new(
            job.request().attempt_id.clone(),
            job.output_sink_handle(),
        ));
        let broker_sink: Arc<dyn InferenceOutputSink> = stages.clone();
        job.replace_output_sink(broker_sink);
        let (reply, receiver) = mpsc::channel();
        let id = self.inner.queue.enqueue(Command::Generate {
            job: Box::new(job),
            stages,
            reply,
        })?;
        let mut guard = PendingWaitGuard::new(Arc::clone(&self.inner.queue), id);
        let mut gate_requested = false;

        loop {
            match receiver.try_recv() {
                Ok(result) => {
                    guard.disarm();
                    return result;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    guard.disarm();
                    return Err(ModelManagerError::ManagerUnavailable);
                }
            }
            if !gate_requested && cancellation.is_cancelled() {
                guard.abandon(WaitAbandonReason::Cancelled);
                gate_requested = true;
                continue;
            }
            if !gate_requested && deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                guard.abandon(WaitAbandonReason::DeadlineExceeded);
                gate_requested = true;
                continue;
            }
            let wait = if gate_requested {
                CANCELLATION_POLL_INTERVAL
            } else {
                deadline
                    .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                    .map_or(CANCELLATION_POLL_INTERVAL, |remaining| {
                        remaining.min(CANCELLATION_POLL_INTERVAL)
                    })
            };
            match receiver.recv_timeout(wait) {
                Ok(result) => {
                    guard.disarm();
                    return result;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    guard.disarm();
                    return Err(ModelManagerError::ManagerUnavailable);
                }
            }
        }
    }

    pub fn clear_context(&self, key: &ContextKey) -> Result<(), ModelManagerError> {
        let (reply, receiver) = mpsc::channel();
        let id = self.inner.queue.enqueue(Command::ClearContext {
            key: key.clone(),
            reply,
        })?;
        let mut guard = PendingWaitGuard::new(Arc::clone(&self.inner.queue), id);
        let result = receiver
            .recv()
            .map_err(|_| ModelManagerError::ManagerUnavailable)?;
        guard.disarm();
        result
    }

    pub fn release_context(&self, key: &ContextKey) -> Result<(), ModelManagerError> {
        let (reply, receiver) = mpsc::channel();
        let id = self.inner.queue.enqueue(Command::ReleaseContext {
            key: key.clone(),
            reply,
        })?;
        let mut guard = PendingWaitGuard::new(Arc::clone(&self.inner.queue), id);
        let result = receiver
            .recv()
            .map_err(|_| ModelManagerError::ManagerUnavailable)?;
        guard.disarm();
        result
    }

    pub fn unload(
        &self,
        target: ModelUnloadTarget,
    ) -> Result<ModelUnloadResult, ModelManagerError> {
        target.validate()?;
        let conflict_target = target.clone();
        let (reply, receiver) = mpsc::sync_channel(1);
        let command = Command::Unload { target, reply };
        let id = match self
            .inner
            .queue
            .enqueue_with_active_check(command, |active| {
                active.is_some_and(|active| match active {
                    ActiveModelTarget::All => true,
                    ActiveModelTarget::Digest(digest) => conflict_target.matches_digest(digest),
                })
            })? {
            EnqueueResult::Queued(id) => id,
            EnqueueResult::ActiveConflict(Command::Unload { reply, .. }) => {
                let _ = reply.send(Ok(ModelUnloadResult::busy()));
                return receiver
                    .recv()
                    .map_err(|_| ModelManagerError::ManagerUnavailable)?;
            }
            EnqueueResult::ActiveConflict(_) => unreachable!("only unload was admitted"),
        };
        let mut guard = PendingWaitGuard::new(Arc::clone(&self.inner.queue), id);
        let result = receiver
            .recv()
            .map_err(|_| ModelManagerError::ManagerUnavailable)?;
        guard.disarm();
        result
    }

    pub fn status(&self) -> Result<ModelManagerStatus, ModelManagerError> {
        self.status_with_detail(ModelManagerStatusDetail::Aggregate)
    }

    pub fn status_with_detail(
        &self,
        detail: ModelManagerStatusDetail,
    ) -> Result<ModelManagerStatus, ModelManagerError> {
        let queue = self.inner.queue.snapshot()?;
        let shared = lock_status(&self.inner.status);
        let mut status = shared.status.clone();
        status.queue_depth = queue.depth;
        status.active_scope = queue.active_scope;
        status.next_residency_deadline_after_ms = shared.next_residency_deadline.map(|deadline| {
            u64::try_from(
                deadline
                    .saturating_duration_since(self.inner.clock.now())
                    .as_millis(),
            )
            .unwrap_or(u64::MAX)
        });
        if detail == ModelManagerStatusDetail::ModelDigests {
            status.resident_model_digests = shared
                .resident_model_digests
                .iter()
                .take(MAX_STATUS_MODEL_DIGESTS)
                .cloned()
                .collect();
            status.resident_model_digests_truncated =
                shared.resident_model_digests.len() > MAX_STATUS_MODEL_DIGESTS;
        } else {
            status.resident_model_digests.clear();
            status.resident_model_digests_truncated = false;
        }
        Ok(status)
    }

    #[cfg(test)]
    pub(super) fn wake_for_test(&self) {
        self.inner.queue.wake();
    }

    pub fn shutdown(&self) -> Result<(), ModelManagerError> {
        self.inner.queue.close_for_shutdown();
        self.inner.queue.wait_for_worker()
    }
}

struct AvailabilityGuard {
    queue: Arc<PendingQueue<Command>>,
    finished: bool,
}

impl AvailabilityGuard {
    fn new(queue: Arc<PendingQueue<Command>>) -> Self {
        Self {
            queue,
            finished: false,
        }
    }

    fn finish(&mut self) {
        self.queue.worker_stopped(true);
        self.finished = true;
    }
}

impl Drop for AvailabilityGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.queue.worker_stopped(false);
        }
    }
}

enum Command {
    DeviceInventory {
        reply: mpsc::Sender<Result<Vec<InferenceDeviceInfo>, ModelManagerError>>,
    },
    Generate {
        job: Box<InferenceJob>,
        stages: Arc<PublicInferenceOutputBroker>,
        reply: mpsc::Sender<Result<InferenceResponse, ModelManagerError>>,
    },
    ClearContext {
        key: ContextKey,
        reply: mpsc::Sender<Result<(), ModelManagerError>>,
    },
    ReleaseContext {
        key: ContextKey,
        reply: mpsc::Sender<Result<(), ModelManagerError>>,
    },
    Unload {
        target: ModelUnloadTarget,
        reply: mpsc::SyncSender<Result<ModelUnloadResult, ModelManagerError>>,
    },
}

impl QueueCommand for Command {
    fn is_generation(&self) -> bool {
        matches!(self, Self::Generate { .. })
    }

    fn cancellation(&self) -> Option<&super::InferenceCancellation> {
        match self {
            Self::Generate { job, .. } => Some(job.cancellation()),
            Self::DeviceInventory { .. }
            | Self::ClearContext { .. }
            | Self::ReleaseContext { .. }
            | Self::Unload { .. } => None,
        }
    }

    fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Generate { job, .. } => job.deadline(),
            Self::DeviceInventory { .. }
            | Self::ClearContext { .. }
            | Self::ReleaseContext { .. }
            | Self::Unload { .. } => None,
        }
    }

    fn active_scope(&self) -> Option<super::InferenceJobScope> {
        match self {
            Self::Generate { job, .. } => Some(job.scope()),
            Self::DeviceInventory { .. }
            | Self::ClearContext { .. }
            | Self::ReleaseContext { .. }
            | Self::Unload { .. } => None,
        }
    }

    fn active_model_target(&self) -> Option<ActiveModelTarget<'_>> {
        match self {
            Self::Generate { job, .. } => Some(ActiveModelTarget::Digest(job.model_key().digest())),
            Self::ClearContext { key, .. } | Self::ReleaseContext { key, .. } => {
                Some(ActiveModelTarget::Digest(key.model_key().digest()))
            }
            Self::Unload {
                target: ModelUnloadTarget::All,
                ..
            } => Some(ActiveModelTarget::All),
            Self::Unload {
                target: ModelUnloadTarget::Digest(digest),
                ..
            } => Some(ActiveModelTarget::Digest(digest)),
            Self::DeviceInventory { .. } => None,
        }
    }

    fn on_queued(&self) {
        if let Self::Generate { stages, .. } = self {
            stages.emit_host_stage(InferenceProductStage::Queued);
        }
    }

    fn on_active(&self) {
        if let Self::Generate { stages, .. } = self {
            stages.emit_host_stage(InferenceProductStage::Admission);
        }
    }

    fn complete(self, error: ModelManagerError) {
        match self {
            Self::DeviceInventory { reply } => {
                let _ = reply.send(Err(error));
            }
            Self::Generate { stages, reply, .. } => {
                emit_terminal_stage(&stages, &Err(error.clone()));
                let _ = reply.send(Err(error));
            }
            Self::ClearContext { reply, .. } | Self::ReleaseContext { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Unload { reply, .. } => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

fn emit_terminal_stage(
    stages: &PublicInferenceOutputBroker,
    result: &Result<InferenceResponse, ModelManagerError>,
) {
    let stage = match result {
        Ok(response) => match response.finish_reason {
            InferenceFinishReason::Stop => InferenceProductStage::Completed,
            InferenceFinishReason::Length | InferenceFinishReason::ContentByteLimit => {
                InferenceProductStage::Incomplete
            }
        },
        Err(ModelManagerError::Cancelled) => InferenceProductStage::Cancelled,
        Err(ModelManagerError::BackendLost { .. })
            if stages
                .last_public_stage()
                .is_some_and(InferenceProductStage::is_worker_owned) =>
        {
            InferenceProductStage::BackendLost
        }
        Err(_) => InferenceProductStage::Failed,
    };
    stages.emit_host_stage(stage);
}

struct Worker<R: ModelRuntime> {
    runtime: R,
    options: ModelManagerOptions,
    status: Arc<Mutex<SharedManagerStatus>>,
    clock: Arc<dyn ManagerClock>,
    models: BTreeMap<ModelKey, ModelEntry<R::Model, R::Context>>,
    lru_clock: u64,
}

struct ModelEntry<M, C> {
    // Contexts precede the model so normal and unwind drops preserve native ownership order.
    contexts: BTreeMap<ContextKey, ContextEntry<C>>,
    model: M,
    // The cache lease outlives the native model and every native context.
    _lease: Option<ModelLease>,
    last_used: u64,
    idle: Option<IdleState>,
}

struct ContextEntry<C> {
    context: C,
    last_used: u64,
    idle: Option<IdleState>,
}

#[derive(Clone, Copy)]
struct IdleState {
    _since: Instant,
    deadline: Instant,
    automatic_attempt_failed: bool,
}

#[derive(Clone, Copy)]
enum ReleaseAccounting {
    None,
    Capacity,
    IdleContext,
    IdleModel,
    Manual,
    Shutdown,
}

impl<R: ModelRuntime> Worker<R> {
    fn new(
        runtime: R,
        options: ModelManagerOptions,
        status: Arc<Mutex<SharedManagerStatus>>,
        clock: Arc<dyn ManagerClock>,
    ) -> Self {
        Self {
            runtime,
            options,
            status,
            clock,
            models: BTreeMap::new(),
            lru_clock: 0,
        }
    }

    fn run(mut self, queue: Arc<PendingQueue<Command>>) -> Result<(), ModelManagerError> {
        loop {
            self.refresh_resource_status();
            match queue.pop_until(self.earliest_deadline()) {
                QueueWake::Command(active) => {
                    let id = active.id;
                    active.command.on_active();
                    match active.command {
                        Command::DeviceInventory { reply } => {
                            let result = self.device_inventory();
                            queue.complete_active(id);
                            let _ = reply.send(result);
                        }
                        Command::Generate { job, stages, reply } => {
                            let result = self.process_job(*job);
                            emit_terminal_stage(&stages, &result);
                            queue.complete_active(id);
                            let _ = reply.send(result);
                        }
                        Command::ClearContext { key, reply } => {
                            let result = self.clear_context(&key);
                            queue.complete_active(id);
                            let _ = reply.send(result);
                        }
                        Command::ReleaseContext { key, reply } => {
                            let result = self.release_context(&key);
                            queue.complete_active(id);
                            let _ = reply.send(result);
                        }
                        Command::Unload { target, reply } => {
                            let result = self.unload(&target);
                            queue.complete_active(id);
                            let _ = reply.send(result);
                        }
                    }
                }
                QueueWake::Deadline => self.release_due_resources(),
                QueueWake::Shutdown => break,
            }
        }
        self.release_all(ReleaseAccounting::Shutdown)?;
        self.refresh_resource_status();
        Ok(())
    }

    fn device_inventory(&mut self) -> Result<Vec<InferenceDeviceInfo>, ModelManagerError> {
        match self.runtime.device_inventory() {
            Ok(devices) => Ok(devices),
            Err(error) if error.is_backend_lost() => {
                let outcome = resource_admission_error(&error).unwrap_or_else(|| {
                    ModelManagerError::BackendLost {
                        message: error.message().to_string(),
                    }
                });
                self.discard_generation();
                Err(outcome)
            }
            Err(error) if error.is_resource_admission() => {
                Err(resource_admission_error(&error).expect("resource failure was classified"))
            }
            Err(error) => Err(ModelManagerError::DeviceInventoryFailed {
                message: error.message().to_string(),
            }),
        }
    }

    fn process_job(
        &mut self,
        mut job: InferenceJob,
    ) -> Result<InferenceResponse, ModelManagerError> {
        let started = Instant::now();
        let mut log = format!(
            "model_key_digest = {}\ncontext_key_digest = {}\n",
            job.model_key().digest(),
            job.context_key().digest()
        );
        let result = match AttemptEvidence::start(&job) {
            Ok(evidence) => {
                let active = job.resolve_content().and_then(|(images, bytes)| {
                    log.push_str(&format!(
                        "resolved_images = {images}\nresolved_media_bytes = {bytes}\n"
                    ));
                    self.process_active_job(&job, &mut log)
                });
                match active {
                    Ok((generation, model_loaded)) => {
                        let response = InferenceResponse {
                            attempt_id: job.request().attempt_id.clone(),
                            content: generation.content,
                            finish_reason: generation.finish_reason,
                            metadata: InferenceResponseMetadata {
                                model_state: Some(if model_loaded {
                                    "loaded".to_string()
                                } else {
                                    "reused".to_string()
                                }),
                                selected_device: generation.selected_device,
                                duration_ms: elapsed_millis(started),
                                input_tokens: generation.input_tokens,
                                output_tokens: generation.output_tokens,
                                resource_admission: generation.resource_admission,
                            },
                        };
                        match evidence.succeed(&response, log) {
                            Ok(()) => Ok(response),
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => match evidence.fail(&error, log) {
                        Ok(()) => Err(error),
                        Err(evidence_error) => Err(evidence_error),
                    },
                }
            }
            Err(error) => Err(error),
        };
        {
            let mut shared = lock_status(&self.status);
            let status = &mut shared.status;
            match &result {
                Ok(response) => match response.finish_reason {
                    crate::InferenceFinishReason::Stop => {
                        status.completed_jobs = status.completed_jobs.saturating_add(1);
                    }
                    crate::InferenceFinishReason::Length
                    | crate::InferenceFinishReason::ContentByteLimit => {
                        status.incomplete_jobs = status.incomplete_jobs.saturating_add(1);
                    }
                },
                Err(ModelManagerError::Cancelled) => {
                    status.cancellations = status.cancellations.saturating_add(1);
                }
                Err(ModelManagerError::DeadlineExceeded) => {
                    status.deadline_exceeded = status.deadline_exceeded.saturating_add(1);
                }
                Err(_) => status.failures = status.failures.saturating_add(1),
            }
        }
        self.refresh_resource_status();
        result
    }

    fn process_active_job(
        &mut self,
        job: &InferenceJob,
        log: &mut String,
    ) -> Result<(ModelGeneration, bool), ModelManagerError> {
        check_job_gate(job)?;
        let model_loaded = self.ensure_model(job, log)?;
        if let Err(error) = check_job_gate(job) {
            if model_loaded {
                self.release_model_resource(job.model_key(), Some(log), ReleaseAccounting::None)?;
            }
            return Err(error);
        }
        let context_loaded = match self.ensure_context(job, log) {
            Ok(context_loaded) => context_loaded,
            Err(error) => {
                if model_loaded && matches!(&error, ModelManagerError::InvalidOptions { .. }) {
                    self.release_model_resource(
                        job.model_key(),
                        Some(log),
                        ReleaseAccounting::None,
                    )?;
                }
                return Err(error);
            }
        };
        if let Err(error) = check_job_gate(job) {
            if context_loaded {
                self.release_context_resource(
                    job.context_key(),
                    Some(log),
                    ReleaseAccounting::None,
                )?;
            }
            if model_loaded {
                self.release_model_resource(job.model_key(), Some(log), ReleaseAccounting::None)?;
            }
            return Err(error);
        }

        let model_key = job.model_key().clone();
        let context_key = job.context_key().clone();
        if let Some(context) = self
            .models
            .get_mut(&model_key)
            .and_then(|entry| entry.contexts.get_mut(&context_key))
        {
            context.idle = None;
        }
        self.refresh_resource_status();
        let generation = {
            let entry = self
                .models
                .get_mut(&model_key)
                .expect("model was inserted before generation");
            let context = entry
                .contexts
                .get_mut(&context_key)
                .expect("context was inserted before generation");
            self.runtime
                .generate(&mut entry.model, &mut context.context, job)
        };

        if let Err(error) = &generation
            && error.is_backend_lost()
        {
            append_operation_log(log, "generation", error.log());
            let outcome =
                resource_admission_error(error).unwrap_or_else(|| ModelManagerError::BackendLost {
                    message: error.message().to_string(),
                });
            self.discard_generation();
            return Err(outcome);
        }

        if job.cancellation().is_cancelled() {
            append_generation_log(log, &generation);
            self.invalidate_context(&model_key, &context_key, log)?;
            return Err(ModelManagerError::Cancelled);
        }
        if job.deadline_exceeded() {
            append_generation_log(log, &generation);
            self.invalidate_context(&model_key, &context_key, log)?;
            return Err(ModelManagerError::DeadlineExceeded);
        }
        let generation = match generation {
            Ok(operation) => {
                append_operation_log(log, "generation", &operation.log);
                operation.value
            }
            Err(error) => {
                append_operation_log(log, "generation", error.log());
                self.invalidate_context(&model_key, &context_key, log)?;
                return Err(if error.is_resource_admission() {
                    resource_admission_error(&error).expect("resource failure was classified")
                } else if error.is_multimodal_encode() {
                    ModelManagerError::MultimodalEncodeFailed {
                        message: error.message().to_string(),
                    }
                } else {
                    ModelManagerError::GenerationFailed {
                        message: error.message().to_string(),
                    }
                });
            }
        };

        let tick = self.next_tick();
        let idle = self.new_idle_state_after_native_commit(self.options.context_idle_duration)?;
        if let Some(entry) = self.models.get_mut(&model_key) {
            entry.last_used = tick;
            if let Some(context) = entry.contexts.get_mut(&context_key) {
                context.last_used = tick;
                context.idle = Some(idle);
            }
        }
        if context_loaded {
            log.push_str("context_state = loaded\n");
        } else {
            log.push_str("context_state = reused\n");
        }
        Ok((generation, model_loaded))
    }

    fn ensure_model(
        &mut self,
        job: &InferenceJob,
        log: &mut String,
    ) -> Result<bool, ModelManagerError> {
        if self.models.contains_key(job.model_key()) {
            return Ok(false);
        }
        while self.models.len() >= self.options.max_loaded_models {
            self.evict_lru_model(log)?;
        }
        let lease = self
            .options
            .model_lease_root
            .as_deref()
            .map(|root| ModelLease::acquire(root, job.model_key(), job.config()))
            .transpose()
            .map_err(|message| ModelManagerError::LoadFailed {
                model_digest: job.model_key().digest().to_string(),
                message,
            })?;
        let model = match self.runtime.load_model(job) {
            Ok(operation) => {
                append_operation_log(log, "model_load", &operation.log);
                operation.value
            }
            Err(error) => {
                append_operation_log(log, "model_load", error.log());
                check_job_gate(job)?;
                if error.is_backend_lost() {
                    let outcome = resource_admission_error(&error).unwrap_or_else(|| {
                        ModelManagerError::BackendLost {
                            message: error.message().to_string(),
                        }
                    });
                    self.discard_generation();
                    return Err(outcome);
                }
                if error.is_resource_admission() {
                    return Err(
                        resource_admission_error(&error).expect("resource failure was classified")
                    );
                }
                return Err(ModelManagerError::LoadFailed {
                    model_digest: job.model_key().digest().to_string(),
                    message: error.message().to_string(),
                });
            }
        };
        let tick = self.next_tick();
        self.models.insert(
            job.model_key().clone(),
            ModelEntry {
                contexts: BTreeMap::new(),
                model,
                _lease: lease,
                last_used: tick,
                idle: None,
            },
        );
        {
            let mut shared = lock_status(&self.status);
            let status = &mut shared.status;
            status.model_loads = status.model_loads.saturating_add(1);
        }
        self.refresh_resource_status();
        Ok(true)
    }

    fn ensure_context(
        &mut self,
        job: &InferenceJob,
        log: &mut String,
    ) -> Result<bool, ModelManagerError> {
        let model_key = job.model_key().clone();
        if self
            .models
            .get(&model_key)
            .is_some_and(|entry| entry.contexts.contains_key(job.context_key()))
        {
            return Ok(false);
        }
        while self
            .models
            .get(&model_key)
            .is_some_and(|entry| entry.contexts.len() >= self.options.max_contexts_per_model)
        {
            self.evict_lru_context(&model_key, log)?;
        }
        // Reject an unrepresentable deadline before creating a native context. The actual
        // idle state is still sampled after the acknowledgement below, when its timer begins.
        let _ = self.new_idle_state(self.options.context_idle_duration)?;
        if let Some(entry) = self.models.get_mut(&model_key) {
            entry.idle = None;
        }
        let context = {
            let entry = self
                .models
                .get_mut(&model_key)
                .expect("model was inserted before context creation");
            match self.runtime.create_context(&mut entry.model, job) {
                Ok(operation) => {
                    append_operation_log(log, "context_create", &operation.log);
                    operation.value
                }
                Err(error) => {
                    append_operation_log(log, "context_create", error.log());
                    self.mark_model_idle_if_empty(&model_key)?;
                    check_job_gate(job)?;
                    if error.is_backend_lost() {
                        let outcome = resource_admission_error(&error).unwrap_or_else(|| {
                            ModelManagerError::BackendLost {
                                message: error.message().to_string(),
                            }
                        });
                        self.discard_generation();
                        return Err(outcome);
                    }
                    if error.is_resource_admission() {
                        return Err(resource_admission_error(&error)
                            .expect("resource failure was classified"));
                    }
                    return Err(ModelManagerError::ContextFailed {
                        context_digest: job.context_key().digest().to_string(),
                        message: error.message().to_string(),
                    });
                }
            }
        };
        let tick = self.next_tick();
        let idle = self.new_idle_state_after_native_commit(self.options.context_idle_duration)?;
        self.models
            .get_mut(&model_key)
            .expect("model remains present after context creation")
            .contexts
            .insert(
                job.context_key().clone(),
                ContextEntry {
                    context,
                    last_used: tick,
                    idle: Some(idle),
                },
            );
        {
            let mut shared = lock_status(&self.status);
            let status = &mut shared.status;
            status.context_loads = status.context_loads.saturating_add(1);
        }
        self.refresh_resource_status();
        Ok(true)
    }

    fn clear_context(&mut self, key: &ContextKey) -> Result<(), ModelManagerError> {
        let previous_idle = self
            .models
            .get_mut(key.model_key())
            .and_then(|entry| entry.contexts.get_mut(key))
            .map(|context| context.idle.take());
        self.refresh_resource_status();
        let outcome = {
            let Some(entry) = self.models.get_mut(key.model_key()) else {
                return Ok(());
            };
            let Some(context) = entry.contexts.get_mut(key) else {
                return Ok(());
            };
            self.runtime
                .clear_context(&mut entry.model, &mut context.context)
        };
        match outcome {
            Ok(_) => {
                let idle =
                    self.new_idle_state_after_native_commit(self.options.context_idle_duration)?;
                self.models
                    .get_mut(key.model_key())
                    .and_then(|entry| entry.contexts.get_mut(key))
                    .expect("cleared context remains present")
                    .idle = Some(idle);
                self.refresh_resource_status();
                Ok(())
            }
            Err(error) if error.is_backend_lost() => {
                let outcome = resource_admission_error(&error).unwrap_or_else(|| {
                    ModelManagerError::BackendLost {
                        message: error.message().to_string(),
                    }
                });
                self.discard_generation();
                Err(outcome)
            }
            Err(error) if error.is_resource_admission() => {
                Err(resource_admission_error(&error).expect("resource failure was classified"))
            }
            Err(error) => {
                if let Some(previous_idle) = previous_idle
                    && let Some(context) = self
                        .models
                        .get_mut(key.model_key())
                        .and_then(|entry| entry.contexts.get_mut(key))
                {
                    context.idle = previous_idle;
                }
                self.refresh_resource_status();
                Err(ModelManagerError::ContextFailed {
                    context_digest: key.digest().to_string(),
                    message: error.message().to_string(),
                })
            }
        }
    }

    fn release_context(&mut self, key: &ContextKey) -> Result<(), ModelManagerError> {
        self.release_context_resource(key, None, ReleaseAccounting::None)
            .map(|_| ())
    }

    fn invalidate_context(
        &mut self,
        _model_key: &ModelKey,
        context_key: &ContextKey,
        log: &mut String,
    ) -> Result<(), ModelManagerError> {
        self.release_context_resource(context_key, Some(log), ReleaseAccounting::None)
            .map(|_| ())
    }

    fn release_context_resource(
        &mut self,
        key: &ContextKey,
        mut log: Option<&mut String>,
        accounting: ReleaseAccounting,
    ) -> Result<bool, ModelManagerError> {
        let outcome = {
            let Some(entry) = self.models.get_mut(key.model_key()) else {
                return Ok(false);
            };
            let Some(context) = entry.contexts.get_mut(key) else {
                return Ok(false);
            };
            self.runtime
                .release_context(&mut entry.model, &mut context.context)
        };
        let operation = match outcome {
            Ok(operation) => operation,
            Err(error) => {
                if let Some(log) = log.as_deref_mut() {
                    append_operation_log(log, "context_release", error.log());
                }
                if error.is_backend_lost() {
                    let outcome = resource_admission_error(&error).unwrap_or_else(|| {
                        ModelManagerError::BackendLost {
                            message: error.message().to_string(),
                        }
                    });
                    self.discard_generation();
                    return Err(outcome);
                }
                if error.is_resource_admission() {
                    return Err(
                        resource_admission_error(&error).expect("resource failure was classified")
                    );
                }
                return Err(ModelManagerError::ContextFailed {
                    context_digest: key.digest().to_string(),
                    message: error.message().to_string(),
                });
            }
        };
        if let Some(log) = log {
            append_operation_log(log, "context_release", &operation.log);
        }
        self.models
            .get_mut(key.model_key())
            .expect("acknowledged context model remains present")
            .contexts
            .remove(key)
            .expect("acknowledged context remains present until removal");
        self.mark_model_idle_if_empty(key.model_key())?;
        {
            let mut shared = lock_status(&self.status);
            let status = &mut shared.status;
            match accounting {
                ReleaseAccounting::Capacity => {
                    status.context_evictions = status.context_evictions.saturating_add(1);
                }
                ReleaseAccounting::IdleContext => {
                    status.automatic_context_unloads =
                        status.automatic_context_unloads.saturating_add(1);
                }
                ReleaseAccounting::None
                | ReleaseAccounting::IdleModel
                | ReleaseAccounting::Manual
                | ReleaseAccounting::Shutdown => {}
            }
            if let Some(reason) = release_reason(accounting) {
                status.last_release_reason = Some(reason);
                status.last_release_outcome = Some(ModelReleaseOutcome::Released);
            }
        }
        self.refresh_resource_status();
        Ok(true)
    }

    fn new_idle_state(&self, duration: Duration) -> Result<IdleState, ModelManagerError> {
        let now = self.clock.now();
        let deadline =
            now.checked_add(duration)
                .ok_or_else(|| ModelManagerError::InvalidOptions {
                    message: "manager residency deadline overflowed the monotonic clock"
                        .to_string(),
                })?;
        Ok(IdleState {
            _since: now,
            deadline,
            automatic_attempt_failed: false,
        })
    }

    fn new_idle_state_after_native_commit(
        &mut self,
        duration: Duration,
    ) -> Result<IdleState, ModelManagerError> {
        match self.new_idle_state(duration) {
            Ok(idle) => Ok(idle),
            Err(error) => {
                self.discard_generation();
                Err(error)
            }
        }
    }

    fn mark_model_idle_if_empty(&mut self, key: &ModelKey) -> Result<(), ModelManagerError> {
        let should_mark = self
            .models
            .get(key)
            .is_some_and(|entry| entry.contexts.is_empty() && entry.idle.is_none());
        if should_mark {
            let idle = self.new_idle_state_after_native_commit(self.options.model_idle_duration)?;
            self.models
                .get_mut(key)
                .expect("empty model remains present")
                .idle = Some(idle);
        }
        Ok(())
    }

    fn earliest_deadline(&self) -> Option<Instant> {
        self.models
            .values()
            .flat_map(|entry| {
                entry
                    .contexts
                    .values()
                    .filter_map(|context| context.idle)
                    .chain(entry.idle)
            })
            .filter(|idle| !idle.automatic_attempt_failed)
            .map(|idle| idle.deadline)
            .min()
    }

    fn release_due_resources(&mut self) {
        let now = self.clock.now();
        let mut contexts = self
            .models
            .values()
            .flat_map(|entry| entry.contexts.iter())
            .filter_map(|(key, context)| {
                context
                    .idle
                    .filter(|idle| !idle.automatic_attempt_failed && idle.deadline <= now)
                    .map(|idle| (idle.deadline, key.digest().to_string(), key.clone()))
            })
            .collect::<Vec<_>>();
        contexts.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
        for (_, _, key) in contexts {
            if let Err(error) =
                self.release_context_resource(&key, None, ReleaseAccounting::IdleContext)
            {
                if let Some(context) = self
                    .models
                    .get_mut(key.model_key())
                    .and_then(|entry| entry.contexts.get_mut(&key))
                    && let Some(idle) = context.idle.as_mut()
                {
                    idle.automatic_attempt_failed = true;
                }
                self.record_release_failure(ModelReleaseReason::IdleContext, &error);
            }
        }

        let now = self.clock.now();
        let mut models = self
            .models
            .iter()
            .filter_map(|(key, entry)| {
                entry
                    .idle
                    .filter(|idle| {
                        entry.contexts.is_empty()
                            && !idle.automatic_attempt_failed
                            && idle.deadline <= now
                    })
                    .map(|idle| (idle.deadline, key.digest().to_string(), key.clone()))
            })
            .collect::<Vec<_>>();
        models.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
        for (_, _, key) in models {
            if let Err(error) =
                self.release_model_resource(&key, None, ReleaseAccounting::IdleModel)
            {
                if let Some(idle) = self
                    .models
                    .get_mut(&key)
                    .and_then(|entry| entry.idle.as_mut())
                {
                    idle.automatic_attempt_failed = true;
                }
                self.record_release_failure(ModelReleaseReason::IdleModel, &error);
            }
        }
        self.refresh_resource_status();
    }

    fn record_release_failure(&self, reason: ModelReleaseReason, error: &ModelManagerError) {
        let mut shared = lock_status(&self.status);
        shared.status.last_release_reason = Some(reason);
        shared.status.last_release_outcome =
            Some(if matches!(error, ModelManagerError::BackendLost { .. }) {
                ModelReleaseOutcome::BackendLost
            } else {
                ModelReleaseOutcome::Failed
            });
        shared.status.unload_failures = shared.status.unload_failures.saturating_add(1);
    }

    fn unload(
        &mut self,
        target: &ModelUnloadTarget,
    ) -> Result<ModelUnloadResult, ModelManagerError> {
        let model_keys = self
            .models
            .keys()
            .filter(|key| target.matches_digest(key.digest()))
            .cloned()
            .collect::<Vec<_>>();
        let mut model_keys = model_keys;
        model_keys.sort_by(|left, right| left.digest().cmp(right.digest()));
        if model_keys.is_empty() {
            return Ok(ModelUnloadResult::not_resident());
        }
        let matched_models = u32::try_from(model_keys.len()).unwrap_or(u32::MAX);
        let mut released_contexts = 0_u32;
        let mut released_models = 0_u32;
        for model_key in model_keys {
            let context_keys = self
                .models
                .get(&model_key)
                .into_iter()
                .flat_map(|entry| entry.contexts.keys().cloned())
                .collect::<Vec<_>>();
            let mut context_keys = context_keys;
            context_keys.sort_by(|left, right| left.digest().cmp(right.digest()));
            for context_key in context_keys {
                match self.release_context_resource(&context_key, None, ReleaseAccounting::Manual) {
                    Ok(true) => released_contexts = released_contexts.saturating_add(1),
                    Ok(false) => {}
                    Err(error) => {
                        self.record_release_failure(ModelReleaseReason::Manual, &error);
                        return Err(error);
                    }
                }
            }
            match self.release_model_resource(&model_key, None, ReleaseAccounting::Manual) {
                Ok(true) => released_models = released_models.saturating_add(1),
                Ok(false) => {}
                Err(error) => {
                    self.record_release_failure(ModelReleaseReason::Manual, &error);
                    return Err(error);
                }
            }
        }
        let mut shared = lock_status(&self.status);
        shared.status.manual_unloads = shared.status.manual_unloads.saturating_add(1);
        Ok(ModelUnloadResult {
            matched_models,
            released_models,
            released_contexts,
            outcome: ModelUnloadOutcome::Released,
        })
    }

    fn evict_lru_model(&mut self, log: &mut String) -> Result<(), ModelManagerError> {
        let key = self
            .models
            .iter()
            .min_by_key(|(key, entry)| (entry.last_used, *key))
            .map(|(key, _)| key.clone())
            .expect("model limit requires an eviction candidate");
        let contexts = self
            .models
            .get(&key)
            .expect("LRU model candidate remains present")
            .contexts
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for context_key in contexts {
            self.release_context_resource(&context_key, Some(log), ReleaseAccounting::Capacity)?;
        }
        self.release_model_resource(&key, Some(log), ReleaseAccounting::Capacity)?;
        Ok(())
    }

    fn evict_lru_context(
        &mut self,
        model_key: &ModelKey,
        log: &mut String,
    ) -> Result<(), ModelManagerError> {
        let entry = self
            .models
            .get_mut(model_key)
            .expect("context limit requires a loaded model");
        let key = entry
            .contexts
            .iter()
            .min_by_key(|(key, context)| (context.last_used, *key))
            .map(|(key, _)| key.clone())
            .expect("context limit requires an eviction candidate");
        self.release_context_resource(&key, Some(log), ReleaseAccounting::Capacity)?;
        Ok(())
    }

    fn release_model_resource(
        &mut self,
        key: &ModelKey,
        mut log: Option<&mut String>,
        accounting: ReleaseAccounting,
    ) -> Result<bool, ModelManagerError> {
        let outcome = {
            let Some(entry) = self.models.get_mut(key) else {
                return Ok(false);
            };
            debug_assert!(entry.contexts.is_empty());
            self.runtime.release_model(&mut entry.model)
        };
        let operation = match outcome {
            Ok(operation) => operation,
            Err(error) => {
                if let Some(log) = log.as_deref_mut() {
                    append_operation_log(log, "model_release", error.log());
                }
                if error.is_backend_lost() {
                    let outcome = resource_admission_error(&error).unwrap_or_else(|| {
                        ModelManagerError::BackendLost {
                            message: error.message().to_string(),
                        }
                    });
                    self.discard_generation();
                    return Err(outcome);
                }
                if error.is_resource_admission() {
                    return Err(
                        resource_admission_error(&error).expect("resource failure was classified")
                    );
                }
                return Err(ModelManagerError::LoadFailed {
                    model_digest: key.digest().to_string(),
                    message: error.message().to_string(),
                });
            }
        };
        if let Some(log) = log {
            append_operation_log(log, "model_release", &operation.log);
        }
        self.models
            .remove(key)
            .expect("acknowledged model remains present until removal");
        {
            let mut shared = lock_status(&self.status);
            let status = &mut shared.status;
            match accounting {
                ReleaseAccounting::Capacity => {
                    status.model_evictions = status.model_evictions.saturating_add(1);
                }
                ReleaseAccounting::IdleModel => {
                    status.automatic_model_unloads =
                        status.automatic_model_unloads.saturating_add(1);
                }
                ReleaseAccounting::None
                | ReleaseAccounting::IdleContext
                | ReleaseAccounting::Manual
                | ReleaseAccounting::Shutdown => {}
            }
            if let Some(reason) = release_reason(accounting) {
                status.last_release_reason = Some(reason);
                status.last_release_outcome = Some(ModelReleaseOutcome::Released);
            }
        }
        self.refresh_resource_status();
        Ok(true)
    }

    fn release_all(&mut self, accounting: ReleaseAccounting) -> Result<(), ModelManagerError> {
        let model_keys = self.models.keys().cloned().collect::<Vec<_>>();
        for model_key in model_keys {
            let context_keys = self
                .models
                .get(&model_key)
                .into_iter()
                .flat_map(|entry| entry.contexts.keys().cloned())
                .collect::<Vec<_>>();
            for context_key in context_keys {
                self.release_context_resource(&context_key, None, accounting)?;
            }
            self.release_model_resource(&model_key, None, accounting)?;
        }
        Ok(())
    }

    fn discard_generation(&mut self) {
        let discarded = std::mem::take(&mut self.models);
        self.refresh_resource_status();
        drop(discarded);
    }

    fn refresh_resource_status(&self) {
        let mut shared = lock_status(&self.status);
        shared.resident_model_digests = self
            .models
            .keys()
            .map(|key| key.digest().to_string())
            .collect();
        shared.resident_model_digests.sort();
        shared.status.resident_models = self.models.len();
        shared.status.resident_contexts =
            self.models.values().map(|entry| entry.contexts.len()).sum();
        shared.next_residency_deadline = self.earliest_deadline();
    }

    fn next_tick(&mut self) -> u64 {
        self.lru_clock = self.lru_clock.saturating_add(1);
        self.lru_clock
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ModelLeaseRecord<'a> {
    version: u32,
    model_key: &'a str,
    pid: u32,
    paths: Vec<PathBuf>,
}

struct ModelLease {
    path: PathBuf,
    _file: File,
}

impl ModelLease {
    fn acquire(
        root: &Path,
        key: &ModelKey,
        config: &ResolvedInferenceConfig,
    ) -> Result<Self, String> {
        std::fs::create_dir_all(root)
            .map_err(|error| format!("failed to create model lease directory: {error}"))?;
        let sequence = NEXT_MODEL_LEASE.fetch_add(1, Ordering::Relaxed);
        let name = format!("{}-{sequence}-{}.json", std::process::id(), key.digest());
        let path = root.join(&name);
        let temporary = root.join(format!(".{name}.tmp"));
        let current_dir = std::env::current_dir()
            .map_err(|error| format!("failed to resolve model lease paths: {error}"))?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| format!("failed to create model lease: {error}"))?;
        if let Err(error) = file.lock() {
            let _ = std::fs::remove_file(&temporary);
            return Err(format!("failed to lock model lease: {error}"));
        }
        let absolute = |path: &Path| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                current_dir.join(path)
            }
        };
        let mut paths = BTreeSet::new();
        paths.insert(absolute(&config.backend.model));
        paths.extend(
            config
                .backend
                .multimodal_projector
                .iter()
                .map(|path| absolute(path)),
        );
        if config.runtime.mtp.enabled {
            paths.extend(
                config
                    .runtime
                    .mtp
                    .draft_model
                    .iter()
                    .map(|path| absolute(path)),
            );
        }
        let record = ModelLeaseRecord {
            version: 1,
            model_key: key.digest(),
            pid: std::process::id(),
            paths: paths.into_iter().collect(),
        };
        let bytes = match serde_json::to_vec_pretty(&record) {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = std::fs::remove_file(&temporary);
                return Err(format!("failed to encode model lease: {error}"));
            }
        };
        if let Err(error) = file
            .write_all(&bytes)
            .and_then(|()| file.sync_all())
            .and_then(|()| std::fs::rename(&temporary, &path))
        {
            let _ = std::fs::remove_file(&temporary);
            return Err(format!("failed to publish model lease: {error}"));
        }
        Ok(Self { path, _file: file })
    }
}

impl Drop for ModelLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn check_job_gate(job: &InferenceJob) -> Result<(), ModelManagerError> {
    if job.cancellation().is_cancelled() {
        Err(ModelManagerError::Cancelled)
    } else if job.deadline_exceeded() {
        Err(ModelManagerError::DeadlineExceeded)
    } else {
        Ok(())
    }
}

fn append_operation_log(log: &mut String, operation: &str, operation_log: &str) {
    if operation_log.is_empty() {
        return;
    }
    log.push_str(operation);
    log.push_str(":\n");
    log.push_str(operation_log);
    if !operation_log.ends_with('\n') {
        log.push('\n');
    }
}

fn append_generation_log(
    log: &mut String,
    generation: &Result<RuntimeOperation<ModelGeneration>, RuntimeFailure>,
) {
    match generation {
        Ok(operation) => append_operation_log(log, "generation", &operation.log),
        Err(failure) => append_operation_log(log, "generation", failure.log()),
    }
}

fn resource_admission_error(error: &RuntimeFailure) -> Option<ModelManagerError> {
    error
        .is_resource_admission()
        .then(|| ModelManagerError::ResourceAdmission {
            code: error.code().to_string(),
            message: error.message().to_string(),
            details: error.resource_admission_details().cloned().map(Box::new),
        })
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn lock_status(
    status: &Mutex<SharedManagerStatus>,
) -> std::sync::MutexGuard<'_, SharedManagerStatus> {
    status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn release_reason(accounting: ReleaseAccounting) -> Option<ModelReleaseReason> {
    match accounting {
        ReleaseAccounting::None => None,
        ReleaseAccounting::Capacity => Some(ModelReleaseReason::Capacity),
        ReleaseAccounting::IdleContext => Some(ModelReleaseReason::IdleContext),
        ReleaseAccounting::IdleModel => Some(ModelReleaseReason::IdleModel),
        ReleaseAccounting::Manual => Some(ModelReleaseReason::Manual),
        ReleaseAccounting::Shutdown => Some(ModelReleaseReason::Shutdown),
    }
}
