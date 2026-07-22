use std::collections::BTreeMap;
use std::os::fd::{OwnedFd, RawFd};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Instant;

use agl_ids::AttemptId;
use agl_inference::worker_protocol::{
    AllocationReceipt, ContextResourceId, DescriptorSet, DeviceSnapshot, HandshakeRejected,
    HostCommand, InventorySnapshot, LiveContextInventoryEntry, LoadedModelInventoryEntry,
    MAX_WORKER_CONTEXTS, MAX_WORKER_MODELS, ModelResourceId, OperationId, Ready, Result,
    SandboxConfiguration, SealedPayload, SealedPayloadTransfer, ShutdownComplete,
    WORKER_DEVICE_LOST_EXIT_STATUS, WorkerCommandReceiver, WorkerControlChannel, WorkerEvent,
    WorkerEventSender, WorkerFailure, WorkerFailureCode, WorkerHealth, WorkerOperationKind,
    WorkerProtocolError, WorkerProtocolErrorCode, WorkerStatusSnapshot,
};
use agl_inference::{
    InferenceCancellation, InferenceJob, InferenceOutputEvent, InferenceOutputSink,
    InferenceProductStage, InferenceStageEvent, InferenceStageValidator, ModelRuntime,
    NoopInferenceOutputSink, OutputDelivery, RuntimeFailure,
};

const MAX_RUNTIME_LOG_BYTES: usize = 4 * 1024 * 1024;

type FatalExit = Arc<dyn Fn(i32) + Send + Sync + 'static>;

pub trait WorkerServiceRuntime: ModelRuntime {
    fn allocation_receipt(
        &self,
        model: &Self::Model,
        context: &Self::Context,
        job: &InferenceJob,
    ) -> std::result::Result<AllocationReceipt, RuntimeFailure>;

    fn failure_code(&self, _failure: &RuntimeFailure) -> WorkerFailureCode {
        WorkerFailureCode::RuntimeFailure
    }
}

pub fn serve_with_runtime<R, I, F, S>(
    channel: WorkerControlChannel,
    runtime_factory: F,
    inventory: I,
    enter_sandbox: S,
) -> Result<()>
where
    R: WorkerServiceRuntime,
    I: Fn() -> Result<DeviceSnapshot> + Send + Sync + 'static,
    F: FnOnce() -> Result<R>,
    S: FnOnce(&SandboxConfiguration, RawFd) -> Result<()>,
{
    serve_with_runtime_and_fatal_exit(
        channel,
        runtime_factory,
        inventory,
        enter_sandbox,
        immediate_process_exit,
    )
}

fn serve_with_runtime_and_fatal_exit<R, I, F, S, X>(
    mut channel: WorkerControlChannel,
    runtime_factory: F,
    inventory: I,
    enter_sandbox: S,
    fatal_exit: X,
) -> Result<()>
where
    R: WorkerServiceRuntime,
    I: Fn() -> Result<DeviceSnapshot> + Send + Sync + 'static,
    F: FnOnce() -> Result<R>,
    S: FnOnce(&SandboxConfiguration, RawFd) -> Result<()>,
    X: Fn(i32) + Send + Sync + 'static,
{
    match channel.receive_with_descriptors()?.into_parts() {
        (HostCommand::Handshake(handshake), descriptors) => {
            descriptors.ensure_empty()?;
            if let Err(code) = handshake.validate_exact() {
                channel.send(WorkerEvent::HandshakeRejected(HandshakeRejected::new(code)))?;
                return Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::IdentityMismatch,
                    format!(
                        "host rejected by exact inference worker handshake: {}",
                        code.as_str()
                    ),
                ));
            }
        }
        (_, _) => {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::UnexpectedMessage,
                "inference worker requires handshake as the first command",
            ));
        }
    }

    // Ready is deliberately non-native. No runtime construction, ggml inventory, Vulkan access,
    // or helper thread may happen before the one-shot sandbox admission below.
    channel.send(WorkerEvent::Ready(Ready::with_native_bundle_id(
        crate::native_bundle::expected_identity(),
    )?))?;
    let registry = Arc::new(Mutex::new(OperationRegistry::default()));
    if !configure_before_threads(&mut channel, &registry, enter_sandbox)? {
        return Ok(());
    }
    let runtime = runtime_factory()?;
    let (receiver, sender) = channel.into_split()?;
    let sender = Arc::new(Mutex::new(sender));
    let (inbound_sender, inbound_receiver) = mpsc::channel();
    let receiver_registry = Arc::clone(&registry);
    let receiver_events = Arc::clone(&sender);
    let receiver_thread = thread::Builder::new()
        .name("agl-inference-worker-control".to_string())
        .spawn(move || {
            receive_commands(receiver, inbound_sender, receiver_registry, receiver_events)
        })
        .map_err(|error| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::Io,
                format!("failed to start inference worker command receiver: {error}"),
            )
        })?;

    let mut service = WorkerService::new(
        runtime,
        inventory,
        Arc::clone(&sender),
        registry,
        Arc::new(fatal_exit),
    );
    let result = service.run(inbound_receiver);
    if result.is_err() {
        service.cancel_all();
        let _ = lock_sender(&sender).and_then(|sender| sender.shutdown());
    }
    let receiver_result = receiver_thread.join().map_err(|_| {
        WorkerProtocolError::new(
            WorkerProtocolErrorCode::Io,
            "inference worker command receiver panicked",
        )
    })?;
    result.and(receiver_result)
}

fn immediate_process_exit(status: i32) {
    // Device loss may leave native destructors unsafe. `_exit` terminates every
    // worker thread without Rust unwinding or C/C++ cleanup; the OS owns native
    // resource reclamation from this point.
    unsafe { libc::_exit(status) }
}

fn configure_before_threads<S>(
    channel: &mut WorkerControlChannel,
    registry: &Arc<Mutex<OperationRegistry>>,
    enter_sandbox: S,
) -> Result<bool>
where
    S: FnOnce(&SandboxConfiguration, RawFd) -> Result<()>,
{
    let mut enter_sandbox = Some(enter_sandbox);
    loop {
        let (command, descriptors) = channel.receive_with_descriptors()?.into_parts();
        match command {
            HostCommand::Handshake(_) => {
                return Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::UnexpectedMessage,
                    "inference worker received a duplicate handshake",
                ));
            }
            HostCommand::Shutdown(_) => {
                descriptors.ensure_empty()?;
                channel.send(WorkerEvent::ShutdownComplete(ShutdownComplete::default()))?;
                return Ok(false);
            }
            HostCommand::ConfigureSandbox {
                operation_id,
                configuration,
            } => {
                descriptors.ensure_empty()?;
                let command = HostCommand::ConfigureSandbox {
                    operation_id,
                    configuration: configuration.clone(),
                };
                {
                    let mut registry = lock_registry(registry)?;
                    registry.register(&command)?;
                    registry.begin(operation_id)?;
                }
                enter_sandbox.take().expect("sandbox admission is one-shot")(
                    &configuration,
                    channel.control_descriptor(),
                )?;
                channel.send(WorkerEvent::SandboxReady { operation_id })?;
                lock_registry(registry)?.finish(operation_id, true)?;
                return Ok(true);
            }
            command => {
                let operation_id = command.operation_id().ok_or_else(|| {
                    WorkerProtocolError::new(
                        WorkerProtocolErrorCode::UnexpectedMessage,
                        "pre-sandbox command has no operation ID",
                    )
                })?;
                {
                    let mut registry = lock_registry(registry)?;
                    registry.register(&command)?;
                    registry.begin(operation_id)?;
                }
                drop(descriptors);
                channel.send(WorkerEvent::Failed {
                    operation_id,
                    failure: WorkerFailure::bounded(
                        WorkerFailureCode::SandboxNotConfigured,
                        "worker sandbox must be configured before native commands",
                    ),
                    log: None,
                })?;
                lock_registry(registry)?.finish(operation_id, false)?;
            }
        }
    }
}

enum Inbound {
    Command(HostCommand, DescriptorSet),
    ProtocolError(WorkerProtocolError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationState {
    Queued,
    Active,
    Terminal,
}

struct OperationRecord {
    kind: WorkerOperationKind,
    state: OperationState,
    cancellation: Option<InferenceCancellation>,
}

#[derive(Default)]
struct OperationRegistry {
    operations: BTreeMap<OperationId, OperationRecord>,
    active: Option<(OperationId, WorkerOperationKind)>,
    queued_commands: usize,
    completed_operations: u64,
    failed_operations: u64,
    cancellation_requests: u64,
}

impl OperationRegistry {
    fn register(&mut self, command: &HostCommand) -> Result<()> {
        let Some(operation_id) = command.operation_id() else {
            return Ok(());
        };
        let kind = command.operation_kind().ok_or_else(|| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::UnexpectedMessage,
                "operation-bearing worker command has no operation kind",
            )
        })?;
        if self.operations.contains_key(&operation_id) {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::SequenceViolation,
                format!(
                    "duplicate inference worker operation ID {}",
                    operation_id.get()
                ),
            ));
        }
        let cancellation = (kind == WorkerOperationKind::Generate).then(InferenceCancellation::new);
        self.operations.insert(
            operation_id,
            OperationRecord {
                kind,
                state: OperationState::Queued,
                cancellation,
            },
        );
        if kind != WorkerOperationKind::Cancel {
            self.queued_commands = self.queued_commands.saturating_add(1);
        }
        Ok(())
    }

    fn begin(&mut self, operation_id: OperationId) -> Result<()> {
        let record = self.operations.get_mut(&operation_id).ok_or_else(|| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::SequenceViolation,
                "worker began an unregistered operation",
            )
        })?;
        if record.state != OperationState::Queued || self.active.is_some() {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::SequenceViolation,
                "worker operation lifecycle is invalid",
            ));
        }
        record.state = OperationState::Active;
        self.queued_commands = self.queued_commands.saturating_sub(1);
        self.active = Some((operation_id, record.kind));
        Ok(())
    }

    fn finish(&mut self, operation_id: OperationId, success: bool) -> Result<()> {
        let record = self.operations.get_mut(&operation_id).ok_or_else(|| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::SequenceViolation,
                "worker completed an unregistered operation",
            )
        })?;
        if record.state == OperationState::Terminal {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::SequenceViolation,
                "worker operation emitted a duplicate terminal event",
            ));
        }
        if self
            .active
            .is_some_and(|(active_operation, _)| active_operation == operation_id)
        {
            self.active = None;
        }
        record.state = OperationState::Terminal;
        if success {
            self.completed_operations = self.completed_operations.saturating_add(1);
        } else {
            self.failed_operations = self.failed_operations.saturating_add(1);
        }
        Ok(())
    }

    fn cancellation(&self, operation_id: OperationId) -> InferenceCancellation {
        self.operations
            .get(&operation_id)
            .and_then(|record| record.cancellation.clone())
            .unwrap_or_default()
    }

    fn cancel(&mut self, target: OperationId) -> bool {
        self.cancellation_requests = self.cancellation_requests.saturating_add(1);
        let Some(record) = self.operations.get_mut(&target) else {
            return false;
        };
        if record.kind != WorkerOperationKind::Generate || record.state == OperationState::Terminal
        {
            return false;
        }
        if let Some(cancellation) = &record.cancellation {
            cancellation.cancel();
            true
        } else {
            false
        }
    }

    fn cancel_all(&self) {
        for record in self.operations.values() {
            if let Some(cancellation) = &record.cancellation {
                cancellation.cancel();
            }
        }
    }
}

fn receive_commands(
    mut receiver: WorkerCommandReceiver,
    inbound: mpsc::Sender<Inbound>,
    registry: Arc<Mutex<OperationRegistry>>,
    events: Arc<Mutex<WorkerEventSender>>,
) -> Result<()> {
    loop {
        let received = match receiver.receive_with_descriptors() {
            Ok(received) => received,
            Err(error) => {
                if let Ok(registry) = registry.lock() {
                    registry.cancel_all();
                }
                let _ = inbound.send(Inbound::ProtocolError(error));
                return Ok(());
            }
        };
        let (command, descriptors) = received.into_parts();
        if matches!(command, HostCommand::Handshake(_)) {
            let error = WorkerProtocolError::new(
                WorkerProtocolErrorCode::UnexpectedMessage,
                "inference worker received a duplicate handshake",
            );
            let _ = inbound.send(Inbound::ProtocolError(error));
            return Ok(());
        }
        if matches!(command, HostCommand::Shutdown(_)) {
            lock_registry(&registry)?.cancel_all();
            inbound
                .send(Inbound::Command(command, descriptors))
                .map_err(|_| service_unavailable("worker service stopped before shutdown"))?;
            return Ok(());
        }

        {
            let mut registry = lock_registry(&registry)?;
            if let Err(error) = registry.register(&command) {
                registry.cancel_all();
                let _ = inbound.send(Inbound::ProtocolError(error));
                return Ok(());
            }
        }

        if let HostCommand::Cancel {
            operation_id,
            target_operation_id,
        } = command
        {
            descriptors.ensure_empty()?;
            let accepted = lock_registry(&registry)?.cancel(target_operation_id);
            let event = if accepted {
                WorkerEvent::CancelAccepted {
                    operation_id,
                    target_operation_id,
                }
            } else {
                WorkerEvent::Failed {
                    operation_id,
                    failure: WorkerFailure::bounded(
                        WorkerFailureCode::CancelTargetNotActive,
                        "cancel target is not an active or queued generation",
                    ),
                    log: None,
                }
            };
            lock_sender(&events)?.send(event)?;
            lock_registry(&registry)?.finish(operation_id, accepted)?;
            continue;
        }

        inbound
            .send(Inbound::Command(command, descriptors))
            .map_err(|_| service_unavailable("inference worker service receiver stopped"))?;
    }
}

struct ModelEntry<M> {
    key_digest: String,
    allocation_reported: bool,
    value: M,
}

struct ContextEntry<C> {
    model_resource_id: ModelResourceId,
    key_digest: String,
    allocation_reported: bool,
    value: C,
}

struct AttemptStream {
    validator: InferenceStageValidator,
    next_delta_sequence: u64,
}

struct WorkerService<R, I>
where
    R: WorkerServiceRuntime,
    I: Fn() -> Result<DeviceSnapshot>,
{
    runtime: R,
    inventory: I,
    events: Arc<Mutex<WorkerEventSender>>,
    registry: Arc<Mutex<OperationRegistry>>,
    attempts: Arc<Mutex<BTreeMap<AttemptId, AttemptStream>>>,
    fatal_exit: FatalExit,
    sandbox_configuration_seen: bool,
    contexts: BTreeMap<ContextResourceId, ContextEntry<R::Context>>,
    models: BTreeMap<ModelResourceId, ModelEntry<R::Model>>,
}

impl<R, I> WorkerService<R, I>
where
    R: WorkerServiceRuntime,
    I: Fn() -> Result<DeviceSnapshot>,
{
    fn new(
        runtime: R,
        inventory: I,
        events: Arc<Mutex<WorkerEventSender>>,
        registry: Arc<Mutex<OperationRegistry>>,
        fatal_exit: FatalExit,
    ) -> Self {
        Self {
            runtime,
            inventory,
            events,
            registry,
            attempts: Arc::new(Mutex::new(BTreeMap::new())),
            fatal_exit,
            sandbox_configuration_seen: true,
            contexts: BTreeMap::new(),
            models: BTreeMap::new(),
        }
    }

    fn run(&mut self, inbound: mpsc::Receiver<Inbound>) -> Result<()> {
        loop {
            match inbound.recv() {
                Ok(Inbound::ProtocolError(error)) => return Err(error),
                Ok(Inbound::Command(command, descriptors)) => {
                    if self.handle(command, descriptors)? {
                        self.contexts.clear();
                        self.models.clear();
                        return Ok(());
                    }
                }
                Err(_) => {
                    return Err(service_unavailable(
                        "inference worker command receiver exited unexpectedly",
                    ));
                }
            }
        }
    }

    fn handle(&mut self, command: HostCommand, descriptors: DescriptorSet) -> Result<bool> {
        if let HostCommand::Shutdown(_) = command {
            descriptors.ensure_empty()?;
            lock_sender(&self.events)?
                .send(WorkerEvent::ShutdownComplete(ShutdownComplete::default()))?;
            return Ok(true);
        }
        let operation_id = command.operation_id().ok_or_else(|| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::UnexpectedMessage,
                "post-handshake command has no operation ID",
            )
        })?;
        lock_registry(&self.registry)?.begin(operation_id)?;

        if let HostCommand::ConfigureSandbox {
            operation_id,
            configuration: _,
        } = command
        {
            descriptors.ensure_empty()?;
            if self.sandbox_configuration_seen {
                self.fail(
                    operation_id,
                    WorkerFailureCode::SandboxAlreadyConfigured,
                    "sandbox configuration is one-shot",
                    "",
                )?;
            } else {
                // The state gate is production protocol behavior. Landlock/seccomp application is
                // injected at this exact point by the dedicated sandbox implementation slice.
                self.sandbox_configuration_seen = true;
                lock_sender(&self.events)?.send(WorkerEvent::SandboxReady { operation_id })?;
                lock_registry(&self.registry)?.finish(operation_id, true)?;
            }
            return Ok(false);
        }

        if !self.sandbox_configuration_seen {
            self.fail(
                operation_id,
                WorkerFailureCode::SandboxNotConfigured,
                "worker sandbox must be configured before native commands",
                "",
            )?;
            return Ok(false);
        }

        match command {
            HostCommand::Inventory { operation_id } => {
                descriptors.ensure_empty()?;
                self.handle_inventory(operation_id)?;
            }
            HostCommand::Status { operation_id } => {
                descriptors.ensure_empty()?;
                self.handle_status(operation_id)?;
            }
            HostCommand::LoadModel {
                operation_id,
                model_resource_id,
                job,
            } => self.handle_load(operation_id, model_resource_id, job, descriptors)?,
            HostCommand::CreateContext {
                operation_id,
                model_resource_id,
                context_resource_id,
                job,
            } => self.handle_create_context(
                operation_id,
                model_resource_id,
                context_resource_id,
                job,
                descriptors,
            )?,
            HostCommand::Generate {
                operation_id,
                model_resource_id,
                context_resource_id,
                job,
            } => self.handle_generate(
                operation_id,
                model_resource_id,
                context_resource_id,
                job,
                descriptors,
            )?,
            HostCommand::ClearContext {
                operation_id,
                context_resource_id,
            } => {
                descriptors.ensure_empty()?;
                self.handle_clear(operation_id, context_resource_id)?;
            }
            HostCommand::ReleaseContext {
                operation_id,
                context_resource_id,
            } => {
                descriptors.ensure_empty()?;
                self.handle_release_context(operation_id, context_resource_id)?;
            }
            HostCommand::ReleaseModel {
                operation_id,
                model_resource_id,
            } => {
                descriptors.ensure_empty()?;
                self.handle_release_model(operation_id, model_resource_id)?;
            }
            HostCommand::Handshake(_)
            | HostCommand::ConfigureSandbox { .. }
            | HostCommand::Cancel { .. }
            | HostCommand::Shutdown(_) => {
                return Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::UnexpectedMessage,
                    "worker service received a command in the wrong control path",
                ));
            }
        }
        Ok(false)
    }

    fn handle_inventory(&mut self, operation_id: OperationId) -> Result<()> {
        let loaded_models = self
            .models
            .iter()
            .map(|(id, model)| LoadedModelInventoryEntry::new(*id, &model.key_digest))
            .collect::<Result<Vec<_>>>()?;
        let live_contexts = self
            .contexts
            .iter()
            .map(|(id, context)| {
                LiveContextInventoryEntry::new(*id, context.model_resource_id, &context.key_digest)
            })
            .collect::<Result<Vec<_>>>()?;
        let snapshot = InventorySnapshot::new((self.inventory)()?, loaded_models, live_contexts)?;
        lock_sender(&self.events)?.send(WorkerEvent::Inventory {
            operation_id,
            snapshot,
        })?;
        lock_registry(&self.registry)?.finish(operation_id, true)
    }

    fn handle_status(&mut self, operation_id: OperationId) -> Result<()> {
        let registry = lock_registry(&self.registry)?;
        let active = registry
            .active
            .map(|(id, kind)| agl_inference::worker_protocol::ActiveOperationStatus::new(id, kind));
        let snapshot = WorkerStatusSnapshot::new(
            if active.is_some() {
                WorkerHealth::Busy
            } else {
                WorkerHealth::Ready
            },
            self.models.len(),
            self.contexts.len(),
            registry.queued_commands,
            active,
            registry.completed_operations,
            registry.failed_operations,
            registry.cancellation_requests,
        )?;
        drop(registry);
        lock_sender(&self.events)?.send(WorkerEvent::Status {
            operation_id,
            snapshot,
        })?;
        lock_registry(&self.registry)?.finish(operation_id, true)
    }

    fn handle_load(
        &mut self,
        operation_id: OperationId,
        model_resource_id: ModelResourceId,
        payload: SealedPayload,
        mut descriptors: DescriptorSet,
    ) -> Result<()> {
        if self.models.len() >= MAX_WORKER_MODELS {
            return self.fail(
                operation_id,
                WorkerFailureCode::ResourceLimit,
                "worker model resource bound reached",
                "",
            );
        }
        if self.models.contains_key(&model_resource_id) {
            return self.fail(
                operation_id,
                WorkerFailureCode::ResourceConflict,
                "model resource ID already exists",
                "",
            );
        }
        let Some(job) = self.decode_job(
            operation_id,
            &payload,
            &mut descriptors,
            Arc::new(NoopInferenceOutputSink),
        )?
        else {
            return Ok(());
        };
        self.emit_stage(
            operation_id,
            job.request().attempt_id.clone(),
            InferenceProductStage::ModelLoad,
        )?;
        match self.runtime.load_model(&job) {
            Ok(operation) => {
                self.models.insert(
                    model_resource_id,
                    ModelEntry {
                        key_digest: job.model_key().digest().to_string(),
                        allocation_reported: false,
                        value: operation.value,
                    },
                );
                let (log, descriptors) = optional_payload(&operation.log, 0)?;
                lock_sender(&self.events)?.send_with_descriptors(
                    WorkerEvent::ModelLoaded {
                        operation_id,
                        model_resource_id,
                        log,
                    },
                    descriptors,
                )?;
                lock_registry(&self.registry)?.finish(operation_id, true)
            }
            Err(failure) => self.runtime_failure(operation_id, failure),
        }
    }

    fn handle_create_context(
        &mut self,
        operation_id: OperationId,
        model_resource_id: ModelResourceId,
        context_resource_id: ContextResourceId,
        payload: SealedPayload,
        mut descriptors: DescriptorSet,
    ) -> Result<()> {
        if self.contexts.len() >= MAX_WORKER_CONTEXTS {
            return self.fail(
                operation_id,
                WorkerFailureCode::ResourceLimit,
                "worker context resource bound reached",
                "",
            );
        }
        if self.contexts.contains_key(&context_resource_id) {
            return self.fail(
                operation_id,
                WorkerFailureCode::ResourceConflict,
                "context resource ID already exists",
                "",
            );
        }
        let Some(job) = self.decode_job(
            operation_id,
            &payload,
            &mut descriptors,
            Arc::new(NoopInferenceOutputSink),
        )?
        else {
            return Ok(());
        };
        let Some(model) = self.models.get(&model_resource_id) else {
            return self.fail(
                operation_id,
                WorkerFailureCode::ResourceNotFound,
                "model resource does not exist",
                "",
            );
        };
        if model.key_digest != job.model_key().digest() {
            return self.fail(
                operation_id,
                WorkerFailureCode::ResourceMismatch,
                "model resource does not match worker job",
                "",
            );
        }
        let attempt_id = job.request().attempt_id.clone();
        self.ensure_model_stage(operation_id, attempt_id.clone())?;
        self.emit_stage(
            operation_id,
            attempt_id,
            InferenceProductStage::ContextRebuild,
        )?;
        let model = self
            .models
            .get_mut(&model_resource_id)
            .expect("model existence was checked above");
        match self.runtime.create_context(&mut model.value, &job) {
            Ok(operation) => {
                self.contexts.insert(
                    context_resource_id,
                    ContextEntry {
                        model_resource_id,
                        key_digest: job.context_key().digest().to_string(),
                        allocation_reported: false,
                        value: operation.value,
                    },
                );
                let (log, descriptors) = optional_payload(&operation.log, 0)?;
                lock_sender(&self.events)?.send_with_descriptors(
                    WorkerEvent::ContextCreated {
                        operation_id,
                        model_resource_id,
                        context_resource_id,
                        log,
                    },
                    descriptors,
                )?;
                lock_registry(&self.registry)?.finish(operation_id, true)
            }
            Err(failure) => self.runtime_failure(operation_id, failure),
        }
    }

    fn handle_generate(
        &mut self,
        operation_id: OperationId,
        model_resource_id: ModelResourceId,
        context_resource_id: ContextResourceId,
        payload: SealedPayload,
        mut descriptors: DescriptorSet,
    ) -> Result<()> {
        let cancellation = lock_registry(&self.registry)?.cancellation(operation_id);
        let output_error = Arc::new(Mutex::new(None));
        let bytes = payload.read_from(&mut descriptors)?;
        descriptors.ensure_empty()?;
        let attempt_id = extract_attempt_id(&bytes)?;
        let output_sink: Arc<dyn InferenceOutputSink> = Arc::new(ProtocolOutputSink {
            operation_id,
            attempt_id: attempt_id.clone(),
            events: Arc::clone(&self.events),
            attempts: Arc::clone(&self.attempts),
            error: Arc::clone(&output_error),
        });
        let job = match InferenceJob::decode_worker_payload(
            &bytes,
            cancellation,
            output_sink,
            Instant::now(),
        ) {
            Ok(job) => job,
            Err(error) => {
                self.fail(
                    operation_id,
                    WorkerFailureCode::InvalidRequest,
                    error.to_string(),
                    "",
                )?;
                return Ok(());
            }
        };
        if job.request().attempt_id != attempt_id {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::MalformedFrame,
                "worker job attempt identity changed during decoding",
            ));
        }
        let Some(context) = self.contexts.get(&context_resource_id) else {
            return self.fail(
                operation_id,
                WorkerFailureCode::ResourceNotFound,
                "context resource does not exist",
                "",
            );
        };
        if context.model_resource_id != model_resource_id
            || context.key_digest != job.context_key().digest()
        {
            return self.fail(
                operation_id,
                WorkerFailureCode::ResourceMismatch,
                "context resource does not match worker job",
                "",
            );
        }
        let Some(model) = self.models.get(&model_resource_id) else {
            return self.fail(
                operation_id,
                WorkerFailureCode::ResourceNotFound,
                "model resource does not exist",
                "",
            );
        };
        if model.key_digest != job.model_key().digest() {
            return self.fail(
                operation_id,
                WorkerFailureCode::ResourceMismatch,
                "model resource does not match worker job",
                "",
            );
        }

        self.ensure_context_stage(operation_id, attempt_id.clone())?;
        let full_receipt = match self
            .runtime
            .allocation_receipt(&model.value, &context.value, &job)
        {
            Ok(receipt) => receipt,
            Err(failure) => return self.runtime_failure(operation_id, failure),
        };
        let receipt = AllocationReceipt::new(
            if model.allocation_reported {
                0
            } else {
                full_receipt.model_bytes()
            },
            if context.allocation_reported {
                0
            } else {
                full_receipt.context_bytes()
            },
            full_receipt.transient_bytes(),
            full_receipt.device_id().map(str::to_string),
        )?;
        lock_sender(&self.events)?.send(WorkerEvent::Started {
            operation_id,
            allocation_receipt: receipt,
        })?;
        self.models
            .get_mut(&model_resource_id)
            .expect("model existence was checked above")
            .allocation_reported = true;
        self.contexts
            .get_mut(&context_resource_id)
            .expect("context existence was checked above")
            .allocation_reported = true;
        self.ensure_prefill_stage(operation_id, attempt_id.clone())?;
        self.emit_stage(
            operation_id,
            attempt_id.clone(),
            InferenceProductStage::Generation,
        )?;

        let outcome = {
            let model = self
                .models
                .get_mut(&model_resource_id)
                .expect("model existence was checked above");
            let context = self
                .contexts
                .get_mut(&context_resource_id)
                .expect("context existence was checked above");
            self.runtime
                .generate(&mut model.value, &mut context.value, &job)
        };
        if let Some(error) = output_error.lock().map_err(|_| lock_poisoned())?.take() {
            return Err(error);
        }
        match outcome {
            Ok(operation) => {
                self.emit_stage(
                    operation_id,
                    attempt_id.clone(),
                    InferenceProductStage::OutputParse,
                )?;
                let result_bytes = serde_json::to_vec(&operation.value).map_err(|error| {
                    WorkerProtocolError::new(
                        WorkerProtocolErrorCode::InvalidPayload,
                        format!("failed to encode worker generation result: {error}"),
                    )
                })?;
                let (result, result_descriptor) =
                    SealedPayloadTransfer::new(&result_bytes, 0)?.into_parts();
                let (log, mut descriptors) = optional_payload(&operation.log, 1)?;
                let mut outbound = vec![result_descriptor];
                outbound.append(&mut descriptors);
                lock_sender(&self.events)?.send_with_descriptors(
                    WorkerEvent::Completed {
                        operation_id,
                        result,
                        log,
                    },
                    outbound,
                )?;
                self.attempts
                    .lock()
                    .map_err(|_| lock_poisoned())?
                    .remove(&attempt_id);
                lock_registry(&self.registry)?.finish(operation_id, true)
            }
            Err(failure) if job.cancellation().is_cancelled() => {
                self.attempts
                    .lock()
                    .map_err(|_| lock_poisoned())?
                    .remove(&attempt_id);
                self.fail(
                    operation_id,
                    WorkerFailureCode::Cancelled,
                    "generation was cancelled",
                    failure.log(),
                )
            }
            Err(failure) if job.deadline_exceeded() => {
                self.attempts
                    .lock()
                    .map_err(|_| lock_poisoned())?
                    .remove(&attempt_id);
                self.fail(
                    operation_id,
                    WorkerFailureCode::DeadlineExceeded,
                    "generation deadline expired",
                    failure.log(),
                )
            }
            Err(failure) => {
                self.attempts
                    .lock()
                    .map_err(|_| lock_poisoned())?
                    .remove(&attempt_id);
                self.runtime_failure(operation_id, failure)
            }
        }
    }

    fn handle_clear(
        &mut self,
        operation_id: OperationId,
        context_resource_id: ContextResourceId,
    ) -> Result<()> {
        let Some(context) = self.contexts.get_mut(&context_resource_id) else {
            return self.fail(
                operation_id,
                WorkerFailureCode::ResourceNotFound,
                "context resource does not exist",
                "",
            );
        };
        let Some(model) = self.models.get_mut(&context.model_resource_id) else {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::SequenceViolation,
                "worker context references an absent model",
            ));
        };
        match self
            .runtime
            .clear_context(&mut model.value, &mut context.value)
        {
            Ok(operation) => {
                let (log, descriptors) = optional_payload(&operation.log, 0)?;
                lock_sender(&self.events)?.send_with_descriptors(
                    WorkerEvent::ContextCleared {
                        operation_id,
                        context_resource_id,
                        log,
                    },
                    descriptors,
                )?;
                lock_registry(&self.registry)?.finish(operation_id, true)
            }
            Err(failure) => self.runtime_failure(operation_id, failure),
        }
    }

    fn handle_release_context(
        &mut self,
        operation_id: OperationId,
        context_resource_id: ContextResourceId,
    ) -> Result<()> {
        let Some(context) = self.contexts.get(&context_resource_id) else {
            return self.fail(
                operation_id,
                WorkerFailureCode::ResourceNotFound,
                "context resource does not exist",
                "",
            );
        };
        let model_resource_id = context.model_resource_id;
        let outcome = {
            let Some(model) = self.models.get_mut(&model_resource_id) else {
                return Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::SequenceViolation,
                    "worker context references an absent model",
                ));
            };
            let context = self
                .contexts
                .get_mut(&context_resource_id)
                .expect("context existence was checked above");
            self.runtime
                .release_context(&mut model.value, &mut context.value)
        };
        if let Err(failure) = outcome {
            return self.runtime_failure(operation_id, failure);
        }
        self.contexts
            .remove(&context_resource_id)
            .expect("acknowledged context remains present until removal");
        lock_sender(&self.events)?.send(WorkerEvent::ContextReleased {
            operation_id,
            context_resource_id,
        })?;
        lock_registry(&self.registry)?.finish(operation_id, true)
    }

    fn handle_release_model(
        &mut self,
        operation_id: OperationId,
        model_resource_id: ModelResourceId,
    ) -> Result<()> {
        if self
            .contexts
            .values()
            .any(|context| context.model_resource_id == model_resource_id)
        {
            return self.fail(
                operation_id,
                WorkerFailureCode::ResourceBusy,
                "model resource still owns live contexts",
                "",
            );
        }
        let Some(model) = self.models.get_mut(&model_resource_id) else {
            return self.fail(
                operation_id,
                WorkerFailureCode::ResourceNotFound,
                "model resource does not exist",
                "",
            );
        };
        if let Err(failure) = self.runtime.release_model(&mut model.value) {
            return self.runtime_failure(operation_id, failure);
        }
        self.models
            .remove(&model_resource_id)
            .expect("acknowledged model remains present until removal");
        lock_sender(&self.events)?.send(WorkerEvent::ModelReleased {
            operation_id,
            model_resource_id,
        })?;
        lock_registry(&self.registry)?.finish(operation_id, true)
    }

    fn decode_job(
        &self,
        operation_id: OperationId,
        payload: &SealedPayload,
        descriptors: &mut DescriptorSet,
        output_sink: Arc<dyn InferenceOutputSink>,
    ) -> Result<Option<InferenceJob>> {
        let bytes = payload.read_from(descriptors)?;
        descriptors.ensure_empty()?;
        let cancellation = lock_registry(&self.registry)?.cancellation(operation_id);
        match InferenceJob::decode_worker_payload(&bytes, cancellation, output_sink, Instant::now())
        {
            Ok(job) => Ok(Some(job)),
            Err(error) => {
                self.fail(
                    operation_id,
                    WorkerFailureCode::InvalidRequest,
                    error.to_string(),
                    "",
                )?;
                Ok(None)
            }
        }
    }

    fn ensure_model_stage(&self, operation_id: OperationId, attempt_id: AttemptId) -> Result<()> {
        let absent = !self
            .attempts
            .lock()
            .map_err(|_| lock_poisoned())?
            .contains_key(&attempt_id);
        if absent {
            self.emit_stage(operation_id, attempt_id, InferenceProductStage::ModelReuse)?;
        }
        Ok(())
    }

    fn ensure_context_stage(&self, operation_id: OperationId, attempt_id: AttemptId) -> Result<()> {
        self.ensure_model_stage(operation_id, attempt_id.clone())?;
        let last = self
            .attempts
            .lock()
            .map_err(|_| lock_poisoned())?
            .get(&attempt_id)
            .and_then(|stream| stream.validator.last_stage());
        if matches!(
            last,
            Some(InferenceProductStage::ModelLoad | InferenceProductStage::ModelReuse)
        ) {
            self.emit_stage(
                operation_id,
                attempt_id,
                InferenceProductStage::ContextReuse,
            )?;
        }
        Ok(())
    }

    fn ensure_prefill_stage(&self, operation_id: OperationId, attempt_id: AttemptId) -> Result<()> {
        let last = self
            .attempts
            .lock()
            .map_err(|_| lock_poisoned())?
            .get(&attempt_id)
            .and_then(|stream| stream.validator.last_stage());
        if last == Some(InferenceProductStage::ContextRebuild) {
            self.emit_stage(operation_id, attempt_id, InferenceProductStage::Prefill)?;
        }
        Ok(())
    }

    fn emit_stage(
        &self,
        operation_id: OperationId,
        attempt_id: AttemptId,
        stage: InferenceProductStage,
    ) -> Result<()> {
        let event = {
            let mut attempts = self.attempts.lock().map_err(|_| lock_poisoned())?;
            let stream = attempts
                .entry(attempt_id.clone())
                .or_insert_with(|| AttemptStream {
                    validator: InferenceStageValidator::worker(attempt_id.clone()),
                    next_delta_sequence: 1,
                });
            let sequence = stream
                .validator
                .last_sequence()
                .checked_add(1)
                .ok_or_else(|| {
                    WorkerProtocolError::new(
                        WorkerProtocolErrorCode::SequenceViolation,
                        "worker stage sequence exhausted",
                    )
                })?;
            let event = InferenceStageEvent {
                attempt_id,
                stage_sequence: sequence,
                stage,
                completed: None,
                total: None,
                unit: None,
            };
            stream.validator.accept(&event).map_err(|error| {
                WorkerProtocolError::new(
                    WorkerProtocolErrorCode::SequenceViolation,
                    error.to_string(),
                )
            })?;
            event
        };
        lock_sender(&self.events)?.send(WorkerEvent::Output {
            operation_id,
            event: InferenceOutputEvent::Stage(event),
        })
    }

    fn runtime_failure(&self, operation_id: OperationId, failure: RuntimeFailure) -> Result<()> {
        let code = self.runtime.failure_code(&failure);
        if code == WorkerFailureCode::DeviceLost {
            // The terminal receipt is deliberately best effort. A broken
            // channel, memfd failure, or poisoned registry must not keep a
            // device-lost native process alive long enough to unwind unsafe
            // backend state.
            let _ = self.fail(operation_id, code, failure.message(), failure.log());
            (self.fatal_exit)(WORKER_DEVICE_LOST_EXIT_STATUS);
            return Err(service_unavailable(
                "fatal inference-worker exit callback returned after device loss",
            ));
        }
        self.fail(operation_id, code, failure.message(), failure.log())
    }

    fn fail(
        &self,
        operation_id: OperationId,
        code: WorkerFailureCode,
        message: impl AsRef<str>,
        log: &str,
    ) -> Result<()> {
        let (log, descriptors) = optional_payload(log, 0)?;
        lock_sender(&self.events)?.send_with_descriptors(
            WorkerEvent::Failed {
                operation_id,
                failure: WorkerFailure::bounded(code, message),
                log,
            },
            descriptors,
        )?;
        lock_registry(&self.registry)?.finish(operation_id, false)
    }

    fn cancel_all(&self) {
        if let Ok(registry) = self.registry.lock() {
            registry.cancel_all();
        }
    }
}

struct ProtocolOutputSink {
    operation_id: OperationId,
    attempt_id: AttemptId,
    events: Arc<Mutex<WorkerEventSender>>,
    attempts: Arc<Mutex<BTreeMap<AttemptId, AttemptStream>>>,
    error: Arc<Mutex<Option<WorkerProtocolError>>>,
}

impl InferenceOutputSink for ProtocolOutputSink {
    fn try_emit(&self, event: InferenceOutputEvent) -> OutputDelivery {
        let validation = (|| -> Result<()> {
            {
                let mut attempts = self.attempts.lock().map_err(|_| lock_poisoned())?;
                let stream = attempts.get_mut(&self.attempt_id).ok_or_else(|| {
                    WorkerProtocolError::new(
                        WorkerProtocolErrorCode::SequenceViolation,
                        "native output arrived before worker attempt stages",
                    )
                })?;
                match &event {
                    InferenceOutputEvent::TextDelta {
                        attempt_id,
                        sequence,
                        ..
                    } => {
                        if attempt_id != &self.attempt_id || *sequence != stream.next_delta_sequence
                        {
                            return Err(WorkerProtocolError::new(
                                WorkerProtocolErrorCode::SequenceViolation,
                                "native output delta identity or sequence is invalid",
                            ));
                        }
                        stream.next_delta_sequence =
                            stream.next_delta_sequence.checked_add(1).ok_or_else(|| {
                                WorkerProtocolError::new(
                                    WorkerProtocolErrorCode::SequenceViolation,
                                    "worker delta sequence exhausted",
                                )
                            })?;
                    }
                    InferenceOutputEvent::Stage(stage) => {
                        stream.validator.accept(stage).map_err(|error| {
                            WorkerProtocolError::new(
                                WorkerProtocolErrorCode::SequenceViolation,
                                error.to_string(),
                            )
                        })?;
                    }
                }
            }
            lock_sender(&self.events)?.send(WorkerEvent::Output {
                operation_id: self.operation_id,
                event,
            })
        })();
        match validation {
            Ok(()) => OutputDelivery::Delivered,
            Err(error) => {
                if let Ok(mut slot) = self.error.lock()
                    && slot.is_none()
                {
                    *slot = Some(error);
                }
                OutputDelivery::Closed
            }
        }
    }
}

fn extract_attempt_id(bytes: &[u8]) -> Result<AttemptId> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
        WorkerProtocolError::new(
            WorkerProtocolErrorCode::MalformedFrame,
            format!("failed to inspect worker job attempt identity: {error}"),
        )
    })?;
    let attempt = value
        .get("request")
        .and_then(|request| request.get("attempt_id"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::MalformedFrame,
                "worker job has no attempt identity",
            )
        })?;
    AttemptId::parse(attempt).map_err(|error| {
        WorkerProtocolError::new(
            WorkerProtocolErrorCode::MalformedFrame,
            format!("worker job attempt identity is invalid: {error}"),
        )
    })
}

fn optional_payload(
    log: &str,
    descriptor_index: u16,
) -> Result<(Option<SealedPayload>, Vec<OwnedFd>)> {
    if log.is_empty() {
        return Ok((None, Vec::new()));
    }
    let bounded = truncate_utf8(log, MAX_RUNTIME_LOG_BYTES);
    let (manifest, descriptor) =
        SealedPayloadTransfer::new(bounded.as_bytes(), descriptor_index)?.into_parts();
    Ok((Some(manifest), vec![descriptor]))
}

fn truncate_utf8(value: &str, maximum: usize) -> &str {
    if value.len() <= maximum {
        return value;
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn lock_sender(
    sender: &Arc<Mutex<WorkerEventSender>>,
) -> Result<std::sync::MutexGuard<'_, WorkerEventSender>> {
    sender.lock().map_err(|_| lock_poisoned())
}

fn lock_registry(
    registry: &Arc<Mutex<OperationRegistry>>,
) -> Result<std::sync::MutexGuard<'_, OperationRegistry>> {
    registry.lock().map_err(|_| lock_poisoned())
}

fn lock_poisoned() -> WorkerProtocolError {
    WorkerProtocolError::new(
        WorkerProtocolErrorCode::Io,
        "inference worker synchronization state was poisoned",
    )
}

fn service_unavailable(message: &str) -> WorkerProtocolError {
    WorkerProtocolError::new(WorkerProtocolErrorCode::WorkerUnavailable, message)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use agl_config::{
        BackendKind, InferenceBackendConfig, InferenceRuntimeConfig, ModelConfig, ModelDialect,
        MtpRuntimeConfig, PromptConfig, ResolvedInferenceConfig, ToolCallFormat,
    };
    use agl_ids::{RunId, TurnId};
    use agl_inference::evidence::InferenceArtifactRoot;
    use agl_inference::worker_protocol::{
        DeviceKind, DeviceSnapshotEntry, Handshake, HostControlChannel, SandboxConfiguration,
        Shutdown, ShutdownReason, control_channel_pair,
    };
    use agl_inference::{
        ContextKey, InferenceFinishReason, InferenceRequest, ModelGeneration, RuntimeOperation,
    };
    use agl_oven::{RenderedMessage, RenderedMessageRole, RenderedModelRequest};

    use super::*;

    const RUN_ID: &str = "run_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b31";
    const TURN_ID: &str = "turn_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b32";
    const ATTEMPT_ID: &str = "attempt_01890f3b-6d7a-7c1f-b4b5-000000000001";

    struct FakeRuntime {
        block_generation: Arc<AtomicBool>,
        device_lost: Arc<AtomicBool>,
    }

    struct FakeModel(String);
    struct FakeContext(String);

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum DeviceLossPhase {
        ModelLoad,
        ContextCreate,
        Generation,
        Cleanup,
    }

    struct ScriptedDeviceLossRuntime {
        phase: DeviceLossPhase,
    }

    impl ModelRuntime for FakeRuntime {
        type Model = FakeModel;
        type Context = FakeContext;

        fn device_inventory(
            &mut self,
        ) -> std::result::Result<Vec<agl_inference::InferenceDeviceInfo>, RuntimeFailure> {
            Ok(Vec::new())
        }

        fn load_model(
            &mut self,
            job: &InferenceJob,
        ) -> std::result::Result<RuntimeOperation<Self::Model>, RuntimeFailure> {
            let key = job.model_key();
            Ok(RuntimeOperation::new(
                FakeModel(key.digest().to_string()),
                "fake model log",
            ))
        }

        fn create_context(
            &mut self,
            model: &mut Self::Model,
            job: &InferenceJob,
        ) -> std::result::Result<RuntimeOperation<Self::Context>, RuntimeFailure> {
            if model.0 != job.model_key().digest() {
                return Err(RuntimeFailure::new("model mismatch", ""));
            }
            Ok(RuntimeOperation::new(
                FakeContext(job.context_key().digest().to_string()),
                "fake context log",
            ))
        }

        fn generate(
            &mut self,
            model: &mut Self::Model,
            context: &mut Self::Context,
            job: &InferenceJob,
        ) -> std::result::Result<RuntimeOperation<ModelGeneration>, RuntimeFailure> {
            if model.0 != job.model_key().digest() || context.0 != job.context_key().digest() {
                return Err(RuntimeFailure::new("resource mismatch", ""));
            }
            while self.block_generation.load(Ordering::Acquire) && !job.should_abort() {
                thread::sleep(Duration::from_millis(2));
            }
            if job.should_abort() {
                return Err(RuntimeFailure::new(
                    "fake generation cancelled",
                    "fake cancellation log",
                ));
            }
            if job.output_sink().try_emit(InferenceOutputEvent::TextDelta {
                attempt_id: job.request().attempt_id.clone(),
                sequence: 1,
                text: "fake".to_string(),
            }) != OutputDelivery::Delivered
            {
                return Err(RuntimeFailure::new("output closed", ""));
            }
            Ok(RuntimeOperation::new(
                ModelGeneration {
                    content: "fake answer".to_string(),
                    finish_reason: InferenceFinishReason::Stop,
                    selected_device: Some("fake-device".to_string()),
                    input_tokens: 4,
                    output_tokens: 1,
                },
                "fake generation log",
            ))
        }

        fn clear_context(
            &mut self,
            _model: &mut Self::Model,
            _context: &mut Self::Context,
        ) -> std::result::Result<RuntimeOperation<()>, RuntimeFailure> {
            Ok(RuntimeOperation::new((), "fake clear log"))
        }

        fn release_context(
            &mut self,
            _model: &mut Self::Model,
            _context: &mut Self::Context,
        ) -> std::result::Result<RuntimeOperation<()>, RuntimeFailure> {
            if self.device_lost.load(Ordering::Acquire) {
                return Err(RuntimeFailure::backend_lost(
                    "fake device lost",
                    "fake context release loss log",
                ));
            }
            Ok(RuntimeOperation::new((), "fake context release log"))
        }

        fn release_model(
            &mut self,
            _model: &mut Self::Model,
        ) -> std::result::Result<RuntimeOperation<()>, RuntimeFailure> {
            if self.device_lost.load(Ordering::Acquire) {
                return Err(RuntimeFailure::backend_lost(
                    "fake device lost",
                    "fake model release loss log",
                ));
            }
            Ok(RuntimeOperation::new((), "fake model release log"))
        }
    }

    impl WorkerServiceRuntime for FakeRuntime {
        fn allocation_receipt(
            &self,
            _model: &Self::Model,
            _context: &Self::Context,
            _job: &InferenceJob,
        ) -> std::result::Result<AllocationReceipt, RuntimeFailure> {
            if self.device_lost.load(Ordering::Acquire) {
                return Err(RuntimeFailure::backend_lost(
                    "fake device lost",
                    "fake device loss log",
                ));
            }
            AllocationReceipt::new(10, 20, 3, Some("fake-device".to_string()))
                .map_err(|error| RuntimeFailure::new(error.to_string(), ""))
        }

        fn failure_code(&self, failure: &RuntimeFailure) -> WorkerFailureCode {
            if failure.is_backend_lost() {
                WorkerFailureCode::DeviceLost
            } else {
                WorkerFailureCode::RuntimeFailure
            }
        }
    }

    impl ModelRuntime for ScriptedDeviceLossRuntime {
        type Model = FakeModel;
        type Context = FakeContext;

        fn device_inventory(
            &mut self,
        ) -> std::result::Result<Vec<agl_inference::InferenceDeviceInfo>, RuntimeFailure> {
            Ok(Vec::new())
        }

        fn load_model(
            &mut self,
            job: &InferenceJob,
        ) -> std::result::Result<RuntimeOperation<Self::Model>, RuntimeFailure> {
            if self.phase == DeviceLossPhase::ModelLoad {
                return Err(RuntimeFailure::backend_lost(
                    "scripted device loss during model load",
                    "scripted model-load loss log",
                ));
            }
            Ok(RuntimeOperation::new(
                FakeModel(job.model_key().digest().to_string()),
                "scripted model log",
            ))
        }

        fn create_context(
            &mut self,
            model: &mut Self::Model,
            job: &InferenceJob,
        ) -> std::result::Result<RuntimeOperation<Self::Context>, RuntimeFailure> {
            if model.0 != job.model_key().digest() {
                return Err(RuntimeFailure::new("model mismatch", ""));
            }
            if self.phase == DeviceLossPhase::ContextCreate {
                return Err(RuntimeFailure::backend_lost(
                    "scripted device loss during context creation",
                    "scripted context-create loss log",
                ));
            }
            Ok(RuntimeOperation::new(
                FakeContext(job.context_key().digest().to_string()),
                "scripted context log",
            ))
        }

        fn generate(
            &mut self,
            model: &mut Self::Model,
            context: &mut Self::Context,
            job: &InferenceJob,
        ) -> std::result::Result<RuntimeOperation<ModelGeneration>, RuntimeFailure> {
            if model.0 != job.model_key().digest() || context.0 != job.context_key().digest() {
                return Err(RuntimeFailure::new("resource mismatch", ""));
            }
            if self.phase == DeviceLossPhase::Generation {
                return Err(RuntimeFailure::backend_lost(
                    "scripted device loss during generation",
                    "scripted generation loss log",
                ));
            }
            Ok(RuntimeOperation::new(
                ModelGeneration {
                    content: "scripted answer".to_string(),
                    finish_reason: InferenceFinishReason::Stop,
                    selected_device: Some("fake-device".to_string()),
                    input_tokens: 4,
                    output_tokens: 1,
                },
                "scripted generation log",
            ))
        }

        fn clear_context(
            &mut self,
            _model: &mut Self::Model,
            _context: &mut Self::Context,
        ) -> std::result::Result<RuntimeOperation<()>, RuntimeFailure> {
            Ok(RuntimeOperation::new((), "scripted clear log"))
        }

        fn release_context(
            &mut self,
            _model: &mut Self::Model,
            _context: &mut Self::Context,
        ) -> std::result::Result<RuntimeOperation<()>, RuntimeFailure> {
            if self.phase == DeviceLossPhase::Cleanup {
                return Err(RuntimeFailure::backend_lost(
                    "scripted device loss during cleanup",
                    "scripted cleanup loss log",
                ));
            }
            Ok(RuntimeOperation::new((), "scripted context release log"))
        }

        fn release_model(
            &mut self,
            _model: &mut Self::Model,
        ) -> std::result::Result<RuntimeOperation<()>, RuntimeFailure> {
            Ok(RuntimeOperation::new((), "scripted model release log"))
        }
    }

    impl WorkerServiceRuntime for ScriptedDeviceLossRuntime {
        fn allocation_receipt(
            &self,
            _model: &Self::Model,
            _context: &Self::Context,
            _job: &InferenceJob,
        ) -> std::result::Result<AllocationReceipt, RuntimeFailure> {
            AllocationReceipt::new(10, 20, 3, Some("fake-device".to_string()))
                .map_err(|error| RuntimeFailure::new(error.to_string(), ""))
        }

        fn failure_code(&self, failure: &RuntimeFailure) -> WorkerFailureCode {
            if failure.is_backend_lost() {
                WorkerFailureCode::DeviceLost
            } else {
                WorkerFailureCode::RuntimeFailure
            }
        }
    }

    #[test]
    fn fake_service_enforces_bootstrap_resources_streams_and_sealed_terminals() {
        let block = Arc::new(AtomicBool::new(false));
        let device_lost = Arc::new(AtomicBool::new(false));
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let inventory_calls = Arc::new(AtomicUsize::new(0));
        let sandbox_calls = Arc::new(AtomicUsize::new(0));
        let (mut host, server, fatal_exit_calls) = start_fake(
            Arc::clone(&block),
            device_lost,
            Arc::clone(&factory_calls),
            Arc::clone(&inventory_calls),
            Arc::clone(&sandbox_calls),
        );

        assert!(
            matches!(host.receive().unwrap(), WorkerEvent::Ready(ready) if ready.device_snapshot().devices().is_empty())
        );
        assert_eq!(factory_calls.load(Ordering::Acquire), 0);
        assert_eq!(inventory_calls.load(Ordering::Acquire), 0);

        host.send(HostCommand::Inventory {
            operation_id: operation(1),
        })
        .unwrap();
        assert!(matches!(
            host.receive().unwrap(),
            WorkerEvent::Failed { operation_id, failure, .. }
                if operation_id == operation(1)
                    && failure.code() == WorkerFailureCode::SandboxNotConfigured
        ));
        assert_eq!(inventory_calls.load(Ordering::Acquire), 0);

        configure(&mut host, operation(2));
        assert_eq!(sandbox_calls.load(Ordering::Acquire), 1);
        wait_for_count(&factory_calls, 1);

        host.send(HostCommand::Inventory {
            operation_id: operation(3),
        })
        .unwrap();
        assert!(matches!(
            host.receive().unwrap(),
            WorkerEvent::Inventory { operation_id, snapshot }
                if operation_id == operation(3)
                    && snapshot.devices().devices().len() == 1
                    && snapshot.loaded_models().is_empty()
        ));
        assert_eq!(inventory_calls.load(Ordering::Acquire), 1);

        let job_bytes = worker_job_bytes();
        send_job(&mut host, &job_bytes, |job| HostCommand::LoadModel {
            operation_id: operation(4),
            model_resource_id: model(1),
            job,
        });
        assert_stage(&mut host, operation(4), InferenceProductStage::ModelLoad);
        assert_log_terminal(&mut host, |event| {
            matches!(event, WorkerEvent::ModelLoaded { operation_id, model_resource_id, .. }
                if *operation_id == operation(4) && *model_resource_id == model(1))
        });

        send_job(&mut host, &job_bytes, |job| HostCommand::CreateContext {
            operation_id: operation(5),
            model_resource_id: model(1),
            context_resource_id: context(1),
            job,
        });
        assert_stage(
            &mut host,
            operation(5),
            InferenceProductStage::ContextRebuild,
        );
        assert_log_terminal(&mut host, |event| {
            matches!(event, WorkerEvent::ContextCreated { operation_id, context_resource_id, .. }
                if *operation_id == operation(5) && *context_resource_id == context(1))
        });

        send_job(&mut host, &job_bytes, |job| HostCommand::Generate {
            operation_id: operation(6),
            model_resource_id: model(1),
            context_resource_id: context(1),
            job,
        });
        assert!(matches!(
            host.receive().unwrap(),
            WorkerEvent::Started { operation_id, allocation_receipt }
                if operation_id == operation(6) && allocation_receipt.total_bytes().unwrap() == 33
        ));
        assert_stage(&mut host, operation(6), InferenceProductStage::Prefill);
        assert_stage(&mut host, operation(6), InferenceProductStage::Generation);
        assert!(matches!(
            host.receive().unwrap(),
            WorkerEvent::Output { operation_id, event: InferenceOutputEvent::TextDelta { sequence: 1, .. } }
                if operation_id == operation(6)
        ));
        assert_stage(&mut host, operation(6), InferenceProductStage::OutputParse);
        let mut completed = host.receive_with_descriptors().unwrap();
        let (result_manifest, log_manifest) = match completed.message() {
            WorkerEvent::Completed {
                operation_id,
                result,
                log,
            } if *operation_id == operation(6) => (result.clone(), log.clone().unwrap()),
            event => panic!("unexpected generation terminal: {event:?}"),
        };
        let result: ModelGeneration = serde_json::from_slice(
            &result_manifest
                .read_from(completed.descriptors_mut())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result.content, "fake answer");
        assert_eq!(
            log_manifest.read_from(completed.descriptors_mut()).unwrap(),
            b"fake generation log"
        );
        completed.descriptors().ensure_empty().unwrap();

        host.send(HostCommand::Status {
            operation_id: operation(7),
        })
        .unwrap();
        assert!(matches!(
            host.receive().unwrap(),
            WorkerEvent::Status { snapshot, .. }
                if snapshot.loaded_models() == 1 && snapshot.live_contexts() == 1
        ));

        assert_eq!(fatal_exit_calls.load(Ordering::Acquire), 0);

        shutdown(host, server);
    }

    #[test]
    fn receiver_half_cancels_blocking_generation_without_waiting_for_runtime_return() {
        let block = Arc::new(AtomicBool::new(true));
        let device_lost = Arc::new(AtomicBool::new(false));
        let (mut host, server, fatal_exit_calls) = start_fake(
            Arc::clone(&block),
            device_lost,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        );
        assert!(matches!(host.receive().unwrap(), WorkerEvent::Ready(_)));
        configure(&mut host, operation(1));
        let bytes = worker_job_bytes();
        send_job(&mut host, &bytes, |job| HostCommand::LoadModel {
            operation_id: operation(2),
            model_resource_id: model(1),
            job,
        });
        assert_stage(&mut host, operation(2), InferenceProductStage::ModelLoad);
        assert_log_terminal(&mut host, |_| true);
        send_job(&mut host, &bytes, |job| HostCommand::CreateContext {
            operation_id: operation(3),
            model_resource_id: model(1),
            context_resource_id: context(1),
            job,
        });
        assert_stage(
            &mut host,
            operation(3),
            InferenceProductStage::ContextRebuild,
        );
        assert_log_terminal(&mut host, |_| true);
        send_job(&mut host, &bytes, |job| HostCommand::Generate {
            operation_id: operation(4),
            model_resource_id: model(1),
            context_resource_id: context(1),
            job,
        });
        assert!(matches!(
            host.receive().unwrap(),
            WorkerEvent::Started { .. }
        ));
        assert_stage(&mut host, operation(4), InferenceProductStage::Prefill);
        assert_stage(&mut host, operation(4), InferenceProductStage::Generation);

        host.send(HostCommand::Cancel {
            operation_id: operation(5),
            target_operation_id: operation(4),
        })
        .unwrap();
        assert!(matches!(
            host.receive().unwrap(),
            WorkerEvent::CancelAccepted { operation_id, target_operation_id }
                if operation_id == operation(5) && target_operation_id == operation(4)
        ));
        let mut failed = host.receive_with_descriptors().unwrap();
        let log = match failed.message() {
            WorkerEvent::Failed {
                operation_id,
                failure,
                log,
            } if *operation_id == operation(4)
                && failure.code() == WorkerFailureCode::Cancelled =>
            {
                log.clone().unwrap()
            }
            event => panic!("unexpected cancelled terminal: {event:?}"),
        };
        assert_eq!(
            log.read_from(failed.descriptors_mut()).unwrap(),
            b"fake cancellation log"
        );
        failed.descriptors().ensure_empty().unwrap();
        block.store(false, Ordering::Release);
        assert_eq!(fatal_exit_calls.load(Ordering::Acquire), 0);
        shutdown(host, server);
    }

    #[test]
    fn typed_device_loss_on_release_requests_fatal_exit_after_terminal_receipt() {
        let device_lost = Arc::new(AtomicBool::new(false));
        let (mut host, server, fatal_exit_calls) = start_fake(
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&device_lost),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        );
        assert!(matches!(host.receive().unwrap(), WorkerEvent::Ready(_)));
        configure(&mut host, operation(1));
        let bytes = worker_job_bytes();
        send_job(&mut host, &bytes, |job| HostCommand::LoadModel {
            operation_id: operation(2),
            model_resource_id: model(1),
            job,
        });
        assert_stage(&mut host, operation(2), InferenceProductStage::ModelLoad);
        assert_log_terminal(&mut host, |_| true);
        send_job(&mut host, &bytes, |job| HostCommand::CreateContext {
            operation_id: operation(3),
            model_resource_id: model(1),
            context_resource_id: context(1),
            job,
        });
        assert_stage(
            &mut host,
            operation(3),
            InferenceProductStage::ContextRebuild,
        );
        assert_log_terminal(&mut host, |_| true);

        device_lost.store(true, Ordering::Release);
        host.send(HostCommand::ReleaseContext {
            operation_id: operation(4),
            context_resource_id: context(1),
        })
        .unwrap();
        assert_failed_with_code_and_log(
            &mut host,
            operation(4),
            WorkerFailureCode::DeviceLost,
            b"fake context release loss log",
        );
        finish_after_injected_fatal_exit(host, server, &fatal_exit_calls);
    }

    #[test]
    fn runtime_classifier_emits_typed_device_loss_before_started_receipt() {
        let device_lost = Arc::new(AtomicBool::new(false));
        let (mut host, server, fatal_exit_calls) = start_fake(
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&device_lost),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        );
        assert!(matches!(host.receive().unwrap(), WorkerEvent::Ready(_)));
        configure(&mut host, operation(1));
        let bytes = worker_job_bytes();
        send_job(&mut host, &bytes, |job| HostCommand::LoadModel {
            operation_id: operation(2),
            model_resource_id: model(1),
            job,
        });
        assert_stage(&mut host, operation(2), InferenceProductStage::ModelLoad);
        assert_log_terminal(&mut host, |_| true);
        send_job(&mut host, &bytes, |job| HostCommand::CreateContext {
            operation_id: operation(3),
            model_resource_id: model(1),
            context_resource_id: context(1),
            job,
        });
        assert_stage(
            &mut host,
            operation(3),
            InferenceProductStage::ContextRebuild,
        );
        assert_log_terminal(&mut host, |_| true);
        device_lost.store(true, Ordering::Release);
        send_job(&mut host, &bytes, |job| HostCommand::Generate {
            operation_id: operation(4),
            model_resource_id: model(1),
            context_resource_id: context(1),
            job,
        });
        let mut failed = host.receive_with_descriptors().unwrap();
        let log = match failed.message() {
            WorkerEvent::Failed {
                operation_id,
                failure,
                log,
            } if *operation_id == operation(4)
                && failure.code() == WorkerFailureCode::DeviceLost =>
            {
                log.clone().unwrap()
            }
            event => panic!("unexpected device-loss terminal: {event:?}"),
        };
        assert_eq!(
            log.read_from(failed.descriptors_mut()).unwrap(),
            b"fake device loss log"
        );
        failed.descriptors().ensure_empty().unwrap();
        finish_after_injected_fatal_exit(host, server, &fatal_exit_calls);
    }

    #[test]
    fn production_service_emits_typed_device_loss_at_every_native_phase() {
        for phase in [
            DeviceLossPhase::ModelLoad,
            DeviceLossPhase::ContextCreate,
            DeviceLossPhase::Generation,
            DeviceLossPhase::Cleanup,
        ] {
            let (mut host, server, fatal_exit_calls) = start_scripted_device_loss(phase);
            assert!(matches!(host.receive().unwrap(), WorkerEvent::Ready(_)));
            configure(&mut host, operation(1));
            let bytes = worker_job_bytes();

            send_job(&mut host, &bytes, |job| HostCommand::LoadModel {
                operation_id: operation(2),
                model_resource_id: model(1),
                job,
            });
            assert_stage(&mut host, operation(2), InferenceProductStage::ModelLoad);
            if phase == DeviceLossPhase::ModelLoad {
                assert_failed_with_code_and_log(
                    &mut host,
                    operation(2),
                    WorkerFailureCode::DeviceLost,
                    b"scripted model-load loss log",
                );
                finish_after_injected_fatal_exit(host, server, &fatal_exit_calls);
                continue;
            }
            assert_log_terminal(&mut host, |_| true);

            send_job(&mut host, &bytes, |job| HostCommand::CreateContext {
                operation_id: operation(3),
                model_resource_id: model(1),
                context_resource_id: context(1),
                job,
            });
            assert_stage(
                &mut host,
                operation(3),
                InferenceProductStage::ContextRebuild,
            );
            if phase == DeviceLossPhase::ContextCreate {
                assert_failed_with_code_and_log(
                    &mut host,
                    operation(3),
                    WorkerFailureCode::DeviceLost,
                    b"scripted context-create loss log",
                );
                finish_after_injected_fatal_exit(host, server, &fatal_exit_calls);
                continue;
            }
            assert_log_terminal(&mut host, |_| true);

            send_job(&mut host, &bytes, |job| HostCommand::Generate {
                operation_id: operation(4),
                model_resource_id: model(1),
                context_resource_id: context(1),
                job,
            });
            assert!(matches!(
                host.receive().unwrap(),
                WorkerEvent::Started { operation_id, .. } if operation_id == operation(4)
            ));
            assert_stage(&mut host, operation(4), InferenceProductStage::Prefill);
            assert_stage(&mut host, operation(4), InferenceProductStage::Generation);
            if phase == DeviceLossPhase::Generation {
                assert_failed_with_code_and_log(
                    &mut host,
                    operation(4),
                    WorkerFailureCode::DeviceLost,
                    b"scripted generation loss log",
                );
                finish_after_injected_fatal_exit(host, server, &fatal_exit_calls);
                continue;
            }
            assert_stage(&mut host, operation(4), InferenceProductStage::OutputParse);
            drain_completed(&mut host, operation(4));

            host.send(HostCommand::ReleaseContext {
                operation_id: operation(5),
                context_resource_id: context(1),
            })
            .unwrap();
            assert_failed_with_code_and_log(
                &mut host,
                operation(5),
                WorkerFailureCode::DeviceLost,
                b"scripted cleanup loss log",
            );
            finish_after_injected_fatal_exit(host, server, &fatal_exit_calls);
        }
    }

    fn start_scripted_device_loss(
        phase: DeviceLossPhase,
    ) -> (
        HostControlChannel,
        thread::JoinHandle<Result<()>>,
        Arc<AtomicUsize>,
    ) {
        let (mut host, worker) = control_channel_pair().unwrap();
        let fatal_exit_calls = Arc::new(AtomicUsize::new(0));
        let server_fatal_exit_calls = Arc::clone(&fatal_exit_calls);
        let server = thread::spawn(move || {
            serve_with_runtime_and_fatal_exit(
                worker,
                move || Ok(ScriptedDeviceLossRuntime { phase }),
                || Ok(DeviceSnapshot::empty()),
                |_configuration, _control_fd| Ok(()),
                move |status| {
                    assert_eq!(status, WORKER_DEVICE_LOST_EXIT_STATUS);
                    server_fatal_exit_calls.fetch_add(1, Ordering::AcqRel);
                },
            )
        });
        host.send(HostCommand::Handshake(Handshake::current()))
            .unwrap();
        (host, server, fatal_exit_calls)
    }

    fn drain_completed(host: &mut HostControlChannel, expected_operation: OperationId) {
        let mut received = host.receive_with_descriptors().unwrap();
        let (result, log) = match received.message() {
            WorkerEvent::Completed {
                operation_id,
                result,
                log,
            } if *operation_id == expected_operation => (result.clone(), log.clone()),
            event => panic!("unexpected generation terminal: {event:?}"),
        };
        assert!(
            !result
                .read_from(received.descriptors_mut())
                .unwrap()
                .is_empty()
        );
        if let Some(log) = log {
            assert!(
                !log.read_from(received.descriptors_mut())
                    .unwrap()
                    .is_empty()
            );
        }
        received.descriptors().ensure_empty().unwrap();
    }

    fn start_fake(
        block_generation: Arc<AtomicBool>,
        device_lost: Arc<AtomicBool>,
        factory_calls: Arc<AtomicUsize>,
        inventory_calls: Arc<AtomicUsize>,
        sandbox_calls: Arc<AtomicUsize>,
    ) -> (
        HostControlChannel,
        thread::JoinHandle<Result<()>>,
        Arc<AtomicUsize>,
    ) {
        let (mut host, worker) = control_channel_pair().unwrap();
        let sandbox_factory_calls = Arc::clone(&factory_calls);
        let sandbox_inventory_calls = Arc::clone(&inventory_calls);
        let fatal_exit_calls = Arc::new(AtomicUsize::new(0));
        let server_fatal_exit_calls = Arc::clone(&fatal_exit_calls);
        let server = thread::spawn(move || {
            serve_with_runtime_and_fatal_exit(
                worker,
                move || {
                    factory_calls.fetch_add(1, Ordering::AcqRel);
                    Ok(FakeRuntime {
                        block_generation,
                        device_lost,
                    })
                },
                move || {
                    inventory_calls.fetch_add(1, Ordering::AcqRel);
                    DeviceSnapshot::new(vec![DeviceSnapshotEntry::new(
                        "physical-fake-0",
                        "fake-driver-build",
                        "Vulkan0",
                        "Fake GPU",
                        DeviceKind::DiscreteGpu,
                        900,
                        1000,
                        true,
                        true,
                    )?])
                },
                move |configuration, control_fd| {
                    assert!(control_fd > libc::STDERR_FILENO);
                    assert_eq!(sandbox_factory_calls.load(Ordering::Acquire), 0);
                    assert_eq!(sandbox_inventory_calls.load(Ordering::Acquire), 0);
                    assert_eq!(configuration.model_roots(), &["/models".to_string()]);
                    sandbox_calls.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                },
                move |status| {
                    assert_eq!(status, WORKER_DEVICE_LOST_EXIT_STATUS);
                    server_fatal_exit_calls.fetch_add(1, Ordering::AcqRel);
                },
            )
        });
        host.send(HostCommand::Handshake(Handshake::current()))
            .unwrap();
        (host, server, fatal_exit_calls)
    }

    fn configure(host: &mut HostControlChannel, operation_id: OperationId) {
        host.send(HostCommand::ConfigureSandbox {
            operation_id,
            configuration: SandboxConfiguration::new(
                vec!["/models".to_string()],
                Vec::new(),
                vec!["/runtime".to_string()],
                vec!["/dev/dri/renderD128".to_string()],
                "/tmp/agl-worker",
            )
            .unwrap(),
        })
        .unwrap();
        assert!(matches!(
            host.receive().unwrap(),
            WorkerEvent::SandboxReady { operation_id: received } if received == operation_id
        ));
    }

    fn send_job(
        host: &mut HostControlChannel,
        bytes: &[u8],
        command: impl FnOnce(SealedPayload) -> HostCommand,
    ) {
        let (manifest, descriptor) = SealedPayloadTransfer::new(bytes, 0).unwrap().into_parts();
        host.send_with_descriptors(command(manifest), vec![descriptor])
            .unwrap();
    }

    fn assert_stage(
        host: &mut HostControlChannel,
        operation_id: OperationId,
        expected: InferenceProductStage,
    ) {
        assert!(matches!(
            host.receive().unwrap(),
            WorkerEvent::Output {
                operation_id: received,
                event: InferenceOutputEvent::Stage(InferenceStageEvent { stage, .. }),
            } if received == operation_id && stage == expected
        ));
    }

    fn assert_log_terminal(
        host: &mut HostControlChannel,
        matches_event: impl FnOnce(&WorkerEvent) -> bool,
    ) {
        let mut received = host.receive_with_descriptors().unwrap();
        assert!(matches_event(received.message()));
        let log = match received.message() {
            WorkerEvent::ModelLoaded { log, .. }
            | WorkerEvent::ContextCreated { log, .. }
            | WorkerEvent::ContextCleared { log, .. } => log.clone().unwrap(),
            event => panic!("event has no expected log: {event:?}"),
        };
        assert!(
            !log.read_from(received.descriptors_mut())
                .unwrap()
                .is_empty()
        );
        received.descriptors().ensure_empty().unwrap();
    }

    fn assert_failed_with_code_and_log(
        host: &mut HostControlChannel,
        expected_operation: OperationId,
        expected_code: WorkerFailureCode,
        expected_log: &[u8],
    ) {
        let mut received = host.receive_with_descriptors().unwrap();
        let log = match received.message() {
            WorkerEvent::Failed {
                operation_id,
                failure,
                log,
            } if *operation_id == expected_operation && failure.code() == expected_code => {
                log.clone().expect("failed release carries runtime log")
            }
            event => panic!("unexpected failed release terminal: {event:?}"),
        };
        assert_eq!(
            log.read_from(received.descriptors_mut()).unwrap(),
            expected_log
        );
        received.descriptors().ensure_empty().unwrap();
    }

    fn shutdown(host: HostControlChannel, server: thread::JoinHandle<Result<()>>) {
        let mut host = host;
        host.send(HostCommand::Shutdown(Shutdown::new(
            ShutdownReason::Requested,
        )))
        .unwrap();
        assert!(matches!(
            host.receive().unwrap(),
            WorkerEvent::ShutdownComplete(_)
        ));
        server.join().unwrap().unwrap();
    }

    fn finish_after_injected_fatal_exit(
        host: HostControlChannel,
        server: thread::JoinHandle<Result<()>>,
        fatal_exit_calls: &AtomicUsize,
    ) {
        wait_for_count(fatal_exit_calls, 1);
        drop(host);
        let error = server
            .join()
            .expect("injected fatal-exit server thread")
            .expect_err("returning fatal-exit test hook must stop the service");
        assert_eq!(error.code(), WorkerProtocolErrorCode::WorkerUnavailable);
    }

    fn worker_job_bytes() -> Vec<u8> {
        let root =
            std::env::temp_dir().join(format!("agl-worker-service-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let config = config();
        let request = InferenceRequest {
            run_id: RunId::parse(RUN_ID).unwrap(),
            turn_id: TurnId::parse(TURN_ID).unwrap(),
            attempt_id: AttemptId::parse(ATTEMPT_ID).unwrap(),
            session_id: None,
            request_id: None,
            rendered: RenderedModelRequest {
                run_id: RunId::parse(RUN_ID).unwrap(),
                turn_id: TurnId::parse(TURN_ID).unwrap(),
                request_index: 1,
                dialect: ModelDialect::Qwen3,
                tool_call_format: ToolCallFormat::HermesJson,
                messages: vec![RenderedMessage {
                    role: RenderedMessageRole::User,
                    content: Some(agl_content::Content::text("hello").unwrap()),
                    name: None,
                    tool_calls: Vec::new(),
                }],
                tools: Vec::new(),
            },
        };
        let mut job = InferenceJob::new(
            config.clone(),
            request,
            ContextKey::for_conversation(&config, "fake-session").unwrap(),
            InferenceArtifactRoot::new(&root),
            root,
            16,
            Arc::new(NoopInferenceOutputSink),
        )
        .unwrap();
        job.resolve_content_for_worker_dispatch().unwrap();
        job.encode_worker_payload(Instant::now()).unwrap()
    }

    fn config() -> ResolvedInferenceConfig {
        ResolvedInferenceConfig {
            backend: InferenceBackendConfig {
                kind: BackendKind::LlamaCpp,
                model: PathBuf::from("/models/fake.gguf"),
                multimodal_projector: None,
            },
            runtime: InferenceRuntimeConfig {
                gpu_layers: 0,
                context_tokens: 4096,
                threads: 2,
                device: None,
                batch_size: None,
                ubatch_size: None,
                flash_attention: None,
                cache_type_k: None,
                cache_type_v: None,
                mmap: Some(true),
                kv_unified: None,
                mtp: MtpRuntimeConfig::default(),
            },
            model: ModelConfig {
                dialect: ModelDialect::Qwen3,
                tool_call_format: ToolCallFormat::HermesJson,
            },
            prompt: PromptConfig::default(),
        }
    }

    fn wait_for_count(counter: &AtomicUsize, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while counter.load(Ordering::Acquire) < expected {
            assert!(
                Instant::now() < deadline,
                "counter did not reach {expected}"
            );
            thread::yield_now();
        }
    }

    fn operation(value: u64) -> OperationId {
        OperationId::new(value).unwrap()
    }

    fn model(value: u64) -> ModelResourceId {
        ModelResourceId::new(value).unwrap()
    }

    fn context(value: u64) -> ContextResourceId {
        ContextResourceId::new(value).unwrap()
    }
}
