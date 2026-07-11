use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::{InferenceResponse, InferenceResponseMetadata};
use agl_config::LocalInferenceConfig;

use super::evidence::AttemptEvidence;
use super::{
    ContextKey, InferenceJob, ModelGeneration, ModelKey, ModelManagerError, ModelManagerOptions,
    ModelManagerStatus, RuntimeFailure, RuntimeOperation,
};

const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub trait ModelRuntime: Send + 'static {
    type Model: 'static;
    type Context: 'static;

    fn load_model(
        &mut self,
        key: &ModelKey,
        config: &LocalInferenceConfig,
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
        options.validate()?;
        let (sender, receiver) = mpsc::sync_channel(options.queue_capacity);
        let status = Arc::new(Mutex::new(ModelManagerStatus::default()));
        let queue_depth = Arc::new(AtomicUsize::new(0));
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let worker_available = Arc::new(AtomicBool::new(true));
        let worker_status = Arc::clone(&status);
        let worker_depth = Arc::clone(&queue_depth);
        let worker_options = options.clone();
        let worker_availability = Arc::clone(&worker_available);
        let worker = thread::Builder::new()
            .name("agl-model-manager".to_string())
            .spawn(move || {
                let _availability = AvailabilityGuard(worker_availability);
                Worker::new(runtime, worker_options, worker_status).run(receiver, &worker_depth);
            })
            .map_err(|_| ModelManagerError::ManagerUnavailable)?;
        Ok(Self {
            handle: ModelManagerHandle {
                inner: Arc::new(HandleInner {
                    sender,
                    status,
                    queue_depth,
                    shutdown_requested,
                    worker_available,
                    queue_capacity: options.queue_capacity,
                }),
            },
            worker: Some(worker),
        })
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
    sender: SyncSender<Command>,
    status: Arc<Mutex<ModelManagerStatus>>,
    queue_depth: Arc<AtomicUsize>,
    shutdown_requested: Arc<AtomicBool>,
    worker_available: Arc<AtomicBool>,
    queue_capacity: usize,
}

impl ModelManagerHandle {
    pub fn generate(&self, job: InferenceJob) -> Result<InferenceResponse, ModelManagerError> {
        check_job_gate(&job)?;
        let cancellation = job.cancellation().clone();
        let deadline = job.deadline();
        let (reply, receiver) = mpsc::channel();
        self.try_send(Command::Generate {
            job: Box::new(job),
            reply,
        })?;

        loop {
            if cancellation.is_cancelled() {
                return Err(ModelManagerError::Cancelled);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(ModelManagerError::DeadlineExceeded);
            }
            let wait = deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .map_or(CANCELLATION_POLL_INTERVAL, |remaining| {
                    remaining.min(CANCELLATION_POLL_INTERVAL)
                });
            match receiver.recv_timeout(wait) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ModelManagerError::ManagerUnavailable);
                }
            }
        }
    }

    pub fn clear_context(&self, key: &ContextKey) -> Result<(), ModelManagerError> {
        let (reply, receiver) = mpsc::channel();
        self.try_send(Command::ClearContext {
            key: key.clone(),
            reply,
        })?;
        receiver
            .recv()
            .map_err(|_| ModelManagerError::ManagerUnavailable)?
    }

    pub fn release_context(&self, key: &ContextKey) -> Result<(), ModelManagerError> {
        let (reply, receiver) = mpsc::channel();
        self.try_send(Command::ReleaseContext {
            key: key.clone(),
            reply,
        })?;
        receiver
            .recv()
            .map_err(|_| ModelManagerError::ManagerUnavailable)?
    }

    pub fn status(&self) -> Result<ModelManagerStatus, ModelManagerError> {
        if self.inner.shutdown_requested.load(Ordering::Acquire)
            || !self.inner.worker_available.load(Ordering::Acquire)
        {
            return Err(ModelManagerError::ManagerUnavailable);
        }
        let mut status = lock_status(&self.inner.status).clone();
        status.queue_depth = self.inner.queue_depth.load(Ordering::Acquire);
        Ok(status)
    }

    pub fn shutdown(&self) -> Result<(), ModelManagerError> {
        if self.inner.shutdown_requested.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let (reply, receiver) = mpsc::channel();
        self.inner.queue_depth.fetch_add(1, Ordering::AcqRel);
        if self.inner.sender.send(Command::Shutdown { reply }).is_err() {
            self.inner.queue_depth.fetch_sub(1, Ordering::AcqRel);
            return Err(ModelManagerError::ManagerUnavailable);
        }
        receiver
            .recv()
            .map_err(|_| ModelManagerError::ManagerUnavailable)
    }

    fn try_send(&self, command: Command) -> Result<(), ModelManagerError> {
        if self.inner.shutdown_requested.load(Ordering::Acquire)
            || !self.inner.worker_available.load(Ordering::Acquire)
        {
            return Err(ModelManagerError::ManagerUnavailable);
        }
        self.inner.queue_depth.fetch_add(1, Ordering::AcqRel);
        match self.inner.sender.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.inner.queue_depth.fetch_sub(1, Ordering::AcqRel);
                Err(ModelManagerError::QueueFull {
                    capacity: self.inner.queue_capacity,
                })
            }
            Err(TrySendError::Disconnected(_)) => {
                self.inner.queue_depth.fetch_sub(1, Ordering::AcqRel);
                Err(ModelManagerError::ManagerUnavailable)
            }
        }
    }
}

struct AvailabilityGuard(Arc<AtomicBool>);

impl Drop for AvailabilityGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

enum Command {
    Generate {
        job: Box<InferenceJob>,
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
    Shutdown {
        reply: mpsc::Sender<()>,
    },
}

struct Worker<R: ModelRuntime> {
    runtime: R,
    options: ModelManagerOptions,
    status: Arc<Mutex<ModelManagerStatus>>,
    models: BTreeMap<ModelKey, ModelEntry<R::Model, R::Context>>,
    lru_clock: u64,
}

struct ModelEntry<M, C> {
    // Contexts precede the model so normal and unwind drops preserve native ownership order.
    contexts: BTreeMap<ContextKey, ContextEntry<C>>,
    model: M,
    last_used: u64,
}

struct ContextEntry<C> {
    context: C,
    last_used: u64,
    idle_since: Instant,
}

impl<R: ModelRuntime> Worker<R> {
    fn new(
        runtime: R,
        options: ModelManagerOptions,
        status: Arc<Mutex<ModelManagerStatus>>,
    ) -> Self {
        Self {
            runtime,
            options,
            status,
            models: BTreeMap::new(),
            lru_clock: 0,
        }
    }

    fn run(mut self, receiver: mpsc::Receiver<Command>, queue_depth: &AtomicUsize) {
        while let Ok(command) = receiver.recv() {
            queue_depth.fetch_sub(1, Ordering::AcqRel);
            self.prune_expired_contexts();
            match command {
                Command::Generate { job, reply } => {
                    let _ = reply.send(self.process_job(*job));
                }
                Command::ClearContext { key, reply } => {
                    let _ = reply.send(self.clear_context(&key));
                }
                Command::ReleaseContext { key, reply } => {
                    self.release_context(&key);
                    let _ = reply.send(Ok(()));
                }
                Command::Shutdown { reply } => {
                    self.models.clear();
                    self.refresh_resource_status();
                    let _ = reply.send(());
                    break;
                }
            }
        }
        self.models.clear();
        self.refresh_resource_status();
    }

    fn process_job(&mut self, job: InferenceJob) -> Result<InferenceResponse, ModelManagerError> {
        {
            let mut status = lock_status(&self.status);
            status.active_scope = Some(job.scope());
        }
        let started = Instant::now();
        let mut log = format!(
            "model_key_digest = {}\ncontext_key_digest = {}\n",
            job.model_key().digest(),
            job.context_key().digest()
        );
        let result = match AttemptEvidence::start(&job) {
            Ok(evidence) => match self.process_active_job(&job, &mut log) {
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
            },
            Err(error) => Err(error),
        };
        {
            let mut status = lock_status(&self.status);
            status.active_scope = None;
            match &result {
                Ok(_) => status.completed_jobs = status.completed_jobs.saturating_add(1),
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
        check_job_gate(job)?;
        let context_loaded = self.ensure_context(job, log)?;
        check_job_gate(job)?;

        let model_key = job.model_key().clone();
        let context_key = job.context_key().clone();
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

        if job.cancellation().is_cancelled() {
            append_generation_log(log, &generation);
            self.invalidate_context(&model_key, &context_key);
            return Err(ModelManagerError::Cancelled);
        }
        if job.deadline_exceeded() {
            append_generation_log(log, &generation);
            self.invalidate_context(&model_key, &context_key);
            return Err(ModelManagerError::DeadlineExceeded);
        }
        let generation = match generation {
            Ok(operation) => {
                append_operation_log(log, "generation", &operation.log);
                operation.value
            }
            Err(error) => {
                append_operation_log(log, "generation", error.log());
                self.invalidate_context(&model_key, &context_key);
                return Err(ModelManagerError::GenerationFailed {
                    message: error.message().to_string(),
                });
            }
        };

        let tick = self.next_tick();
        if let Some(entry) = self.models.get_mut(&model_key) {
            entry.last_used = tick;
            if let Some(context) = entry.contexts.get_mut(&context_key) {
                context.last_used = tick;
                context.idle_since = Instant::now();
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
            self.evict_lru_model();
        }
        let model = match self.runtime.load_model(job.model_key(), job.config()) {
            Ok(operation) => {
                append_operation_log(log, "model_load", &operation.log);
                operation.value
            }
            Err(error) => {
                append_operation_log(log, "model_load", error.log());
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
                last_used: tick,
            },
        );
        {
            let mut status = lock_status(&self.status);
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
            self.evict_lru_context(&model_key);
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
                    return Err(ModelManagerError::ContextFailed {
                        context_digest: job.context_key().digest().to_string(),
                        message: error.message().to_string(),
                    });
                }
            }
        };
        let tick = self.next_tick();
        self.models
            .get_mut(&model_key)
            .expect("model remains present after context creation")
            .contexts
            .insert(
                job.context_key().clone(),
                ContextEntry {
                    context,
                    last_used: tick,
                    idle_since: Instant::now(),
                },
            );
        {
            let mut status = lock_status(&self.status);
            status.context_loads = status.context_loads.saturating_add(1);
        }
        self.refresh_resource_status();
        Ok(true)
    }

    fn clear_context(&mut self, key: &ContextKey) -> Result<(), ModelManagerError> {
        let Some(entry) = self.models.get_mut(key.model_key()) else {
            return Ok(());
        };
        let Some(context) = entry.contexts.get_mut(key) else {
            return Ok(());
        };
        self.runtime
            .clear_context(&mut entry.model, &mut context.context)
            .map_err(|error| ModelManagerError::ContextFailed {
                context_digest: key.digest().to_string(),
                message: error.message().to_string(),
            })?;
        context.idle_since = Instant::now();
        Ok(())
    }

    fn release_context(&mut self, key: &ContextKey) {
        if let Some(entry) = self.models.get_mut(key.model_key()) {
            entry.contexts.remove(key);
        }
        self.refresh_resource_status();
    }

    fn invalidate_context(&mut self, model_key: &ModelKey, context_key: &ContextKey) {
        if let Some(entry) = self.models.get_mut(model_key)
            && entry.contexts.remove(context_key).is_some()
        {
            let mut status = lock_status(&self.status);
            status.context_evictions = status.context_evictions.saturating_add(1);
        }
    }

    fn prune_expired_contexts(&mut self) {
        let now = Instant::now();
        let retention = self.options.idle_context_retention;
        let mut evicted = 0u64;
        for entry in self.models.values_mut() {
            let before = entry.contexts.len();
            entry
                .contexts
                .retain(|_, context| now.duration_since(context.idle_since) < retention);
            evicted = evicted.saturating_add(
                u64::try_from(before.saturating_sub(entry.contexts.len())).unwrap_or(u64::MAX),
            );
        }
        if evicted > 0 {
            let mut status = lock_status(&self.status);
            status.context_evictions = status.context_evictions.saturating_add(evicted);
            drop(status);
            self.refresh_resource_status();
        }
    }

    fn evict_lru_model(&mut self) {
        let key = self
            .models
            .iter()
            .min_by_key(|(key, entry)| (entry.last_used, *key))
            .map(|(key, _)| key.clone())
            .expect("model limit requires an eviction candidate");
        let mut entry = self
            .models
            .remove(&key)
            .expect("LRU model candidate remains present");
        let contexts = u64::try_from(entry.contexts.len()).unwrap_or(u64::MAX);
        entry.contexts.clear();
        drop(entry.model);
        let mut status = lock_status(&self.status);
        status.model_evictions = status.model_evictions.saturating_add(1);
        status.context_evictions = status.context_evictions.saturating_add(contexts);
    }

    fn evict_lru_context(&mut self, model_key: &ModelKey) {
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
        entry.contexts.remove(&key);
        let mut status = lock_status(&self.status);
        status.context_evictions = status.context_evictions.saturating_add(1);
    }

    fn refresh_resource_status(&self) {
        let mut status = lock_status(&self.status);
        status.loaded_model_digests = self
            .models
            .keys()
            .map(|key| key.digest().to_string())
            .collect();
        status.cached_contexts = self.models.values().map(|entry| entry.contexts.len()).sum();
    }

    fn next_tick(&mut self) -> u64 {
        self.lru_clock = self.lru_clock.saturating_add(1);
        self.lru_clock
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

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn lock_status(
    status: &Mutex<ModelManagerStatus>,
) -> std::sync::MutexGuard<'_, ModelManagerStatus> {
    status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
