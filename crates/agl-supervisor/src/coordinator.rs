use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use agl_events::SafeRuntimeEventEnvelope;
use agl_ids::{RunId, StepId};
use agl_store::{
    AglStore, DurableRunDraft, DurableRunRecord, EffectDeliveryClass, RunLease, RunState,
    RunStepDraft, RunStepState, SafeRunStatus,
};

use crate::driver::{
    DriverSnapshot, DurableRunDriverFactory, EffectExecutionContext, RunCancellation,
    SupervisorTerminal,
};
use crate::{Result, SupervisorError, SupervisorOptions};

#[derive(Clone, Debug)]
pub struct IdempotentRunSpec {
    pub namespace: String,
    pub key: String,
    pub fingerprint: String,
}

#[derive(Clone, Debug)]
pub struct RunSpec {
    pub run: DurableRunDraft,
    pub idempotency: Option<IdempotentRunSpec>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunAccepted {
    pub status: SafeRunStatus,
    pub replayed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunOutcome {
    pub status: SafeRunStatus,
    pub terminal_result: Option<serde_json::Value>,
    pub error_message: Option<String>,
}

pub struct RunSubscription {
    pub backlog: Vec<SafeRuntimeEventEnvelope>,
    receiver: mpsc::Receiver<SubscriptionMessage>,
    last_delivered: Arc<AtomicU64>,
    overflowed: Arc<Mutex<bool>>,
}

impl RunSubscription {
    pub fn recv(&self) -> Result<Option<SafeRuntimeEventEnvelope>> {
        match self.receiver.recv() {
            Ok(SubscriptionMessage::Event(event)) => {
                self.last_delivered.store(event.sequence, Ordering::Release);
                Ok(Some(*event))
            }
            Ok(SubscriptionMessage::Complete) => Ok(None),
            Err(_) if *self.overflowed.lock().expect("overflow flag poisoned") => {
                Err(SupervisorError::SubscriberOverflow {
                    last_sequence: self.last_delivered.load(Ordering::Acquire),
                })
            }
            Err(_) => Err(SupervisorError::Unavailable),
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<SafeRuntimeEventEnvelope>> {
        match self.receiver.recv_timeout(timeout) {
            Ok(SubscriptionMessage::Event(event)) => {
                self.last_delivered.store(event.sequence, Ordering::Release);
                Ok(Some(*event))
            }
            Ok(SubscriptionMessage::Complete) => Ok(None),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected)
                if *self.overflowed.lock().expect("overflow flag poisoned") =>
            {
                Err(SupervisorError::SubscriberOverflow {
                    last_sequence: self.last_delivered.load(Ordering::Acquire),
                })
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(SupervisorError::Unavailable),
        }
    }
}

pub struct Supervisor {
    handle: SupervisorHandle,
    join: Option<thread::JoinHandle<()>>,
}

impl Supervisor {
    pub fn spawn(
        store_root: impl AsRef<Path>,
        factory: Arc<dyn DurableRunDriverFactory>,
        options: SupervisorOptions,
    ) -> Result<Self> {
        options.validate()?;
        let store_root = store_root.as_ref().to_path_buf();
        let (sender, receiver) = mpsc::sync_channel(options.command_capacity);
        let (init_sender, init_receiver) = mpsc::sync_channel(1);
        let worker_sender = sender.clone();
        let join = thread::Builder::new()
            .name("agl-supervisor".to_string())
            .spawn(move || {
                let result =
                    Coordinator::open(store_root, factory, options, receiver, worker_sender);
                match result {
                    Ok(mut coordinator) => {
                        let _ = init_sender.send(Ok(()));
                        coordinator.run();
                    }
                    Err(error) => {
                        let _ = init_sender.send(Err(error.to_string()));
                    }
                }
            })
            .map_err(|error| SupervisorError::Driver(error.to_string()))?;
        init_receiver
            .recv()
            .map_err(|_| SupervisorError::Unavailable)?
            .map_err(SupervisorError::Driver)?;
        Ok(Self {
            handle: SupervisorHandle { sender },
            join: Some(join),
        })
    }

    pub fn handle(&self) -> SupervisorHandle {
        self.handle.clone()
    }

    pub fn shutdown(mut self) -> Result<()> {
        self.handle.shutdown_blocking()?;
        if let Some(join) = self.join.take() {
            join.join().map_err(|_| {
                SupervisorError::Driver("supervisor coordinator panicked".to_string())
            })?;
        }
        Ok(())
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.handle.shutdown_blocking();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[derive(Clone)]
pub struct SupervisorHandle {
    sender: mpsc::SyncSender<CoordinatorMessage>,
}

impl SupervisorHandle {
    pub fn submit(&self, spec: RunSpec) -> Result<RunAccepted> {
        self.request_with_reply(|reply| CommandRequest::Submit {
            spec: Box::new(spec),
            reply,
        })
    }

    pub fn status(&self, run_id: RunId) -> Result<Option<SafeRunStatus>> {
        self.request_with_reply(|reply| CommandRequest::Status { run_id, reply })
    }

    pub fn outcome(&self, run_id: RunId) -> Result<Option<RunOutcome>> {
        self.request_with_reply(|reply| CommandRequest::Outcome { run_id, reply })
    }

    pub fn tree(&self, run_id: RunId) -> Result<Vec<SafeRunStatus>> {
        self.request_with_reply(|reply| CommandRequest::Tree { run_id, reply })
    }

    pub fn cancel(&self, run_id: RunId) -> Result<SafeRunStatus> {
        self.request_with_reply(|reply| CommandRequest::Cancel { run_id, reply })
    }

    pub fn events_after(
        &self,
        run_id: RunId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<SafeRuntimeEventEnvelope>> {
        self.request_with_reply(|reply| CommandRequest::EventsAfter {
            run_id,
            after_sequence,
            limit,
            reply,
        })
    }

    pub fn subscribe(&self, run_id: RunId, after_sequence: u64) -> Result<RunSubscription> {
        self.request_with_reply(|reply| CommandRequest::Subscribe {
            run_id,
            after_sequence,
            reply,
        })
    }

    fn request_with_reply<T: Send + 'static>(
        &self,
        build: impl FnOnce(mpsc::SyncSender<Result<T>>) -> CommandRequest,
    ) -> Result<T> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.request(build(reply))?;
        receiver.recv().map_err(|_| SupervisorError::Unavailable)?
    }

    fn request(&self, request: CommandRequest) -> Result<()> {
        self.sender
            .try_send(CoordinatorMessage::Command(Box::new(request)))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => SupervisorError::CommandQueueFull,
                mpsc::TrySendError::Disconnected(_) => SupervisorError::Unavailable,
            })
    }

    fn shutdown_blocking(&self) -> Result<()> {
        self.sender
            .send(CoordinatorMessage::Command(Box::new(
                CommandRequest::Shutdown,
            )))
            .map_err(|_| SupervisorError::Unavailable)
    }
}

enum CoordinatorMessage {
    Command(Box<CommandRequest>),
    Worker(WorkerMessage),
}

enum CommandRequest {
    Submit {
        spec: Box<RunSpec>,
        reply: mpsc::SyncSender<Result<RunAccepted>>,
    },
    Status {
        run_id: RunId,
        reply: mpsc::SyncSender<Result<Option<SafeRunStatus>>>,
    },
    Outcome {
        run_id: RunId,
        reply: mpsc::SyncSender<Result<Option<RunOutcome>>>,
    },
    Tree {
        run_id: RunId,
        reply: mpsc::SyncSender<Result<Vec<SafeRunStatus>>>,
    },
    Cancel {
        run_id: RunId,
        reply: mpsc::SyncSender<Result<SafeRunStatus>>,
    },
    EventsAfter {
        run_id: RunId,
        after_sequence: u64,
        limit: usize,
        reply: mpsc::SyncSender<Result<Vec<SafeRuntimeEventEnvelope>>>,
    },
    Subscribe {
        run_id: RunId,
        after_sequence: u64,
        reply: mpsc::SyncSender<Result<RunSubscription>>,
    },
    Shutdown,
}

enum WorkerMessage {
    EventsCommitted {
        run_id: RunId,
        events: Vec<SafeRuntimeEventEnvelope>,
    },
    Finished {
        run_id: RunId,
    },
}

struct ActiveRun {
    lease: RunLease,
    cancellation: RunCancellation,
}

struct Subscriber {
    sender: mpsc::SyncSender<SubscriptionMessage>,
    last_sequence: u64,
    overflowed: Arc<Mutex<bool>>,
}

enum SubscriptionMessage {
    Event(Box<SafeRuntimeEventEnvelope>),
    Complete,
}

struct Coordinator {
    store_root: PathBuf,
    store: AglStore,
    factory: Arc<dyn DurableRunDriverFactory>,
    options: SupervisorOptions,
    receiver: mpsc::Receiver<CoordinatorMessage>,
    sender: mpsc::SyncSender<CoordinatorMessage>,
    active: BTreeMap<RunId, ActiveRun>,
    subscribers: BTreeMap<RunId, Vec<Subscriber>>,
    last_heartbeat_ms: i64,
}

impl Coordinator {
    fn open(
        store_root: PathBuf,
        factory: Arc<dyn DurableRunDriverFactory>,
        options: SupervisorOptions,
        receiver: mpsc::Receiver<CoordinatorMessage>,
        sender: mpsc::SyncSender<CoordinatorMessage>,
    ) -> Result<Self> {
        let store = AglStore::open_at(&store_root)?;
        let now_ms = options.clock.now_ms();
        store.recover_expired_work(now_ms)?;
        store.expire_delegation_trees(now_ms)?;
        Ok(Self {
            store_root,
            store,
            factory,
            options,
            receiver,
            sender,
            active: BTreeMap::new(),
            subscribers: BTreeMap::new(),
            last_heartbeat_ms: now_ms,
        })
    }

    fn run(&mut self) {
        let mut running = true;
        while running {
            match self.receiver.recv_timeout(self.options.heartbeat_interval) {
                Ok(CoordinatorMessage::Command(command)) => {
                    running = self.handle_command(*command);
                }
                Ok(CoordinatorMessage::Worker(message)) => self.handle_worker(message),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.heartbeat_if_due();
            self.claim_available();
        }
        for active in self.active.values() {
            active.cancellation.cancel();
        }
    }

    fn handle_command(&mut self, command: CommandRequest) -> bool {
        match command {
            CommandRequest::Submit { spec, reply } => {
                let result = self.submit(*spec);
                let _ = reply.send(result);
            }
            CommandRequest::Status { run_id, reply } => {
                let _ = reply.send(self.store.safe_run_status(&run_id).map_err(Into::into));
            }
            CommandRequest::Outcome { run_id, reply } => {
                let result = self.store.run(&run_id).map(|run| {
                    run.map(|run| RunOutcome {
                        status: SafeRunStatus {
                            run_id: run.run_id,
                            session_id: run.session_id,
                            turn_id: run.turn_id,
                            kind: run.kind,
                            state: run.state,
                            priority: run.priority,
                            usage: run.usage,
                            cancellation_requested: run.cancellation_requested_at_ms.is_some(),
                            attempts: run.attempts,
                            created_at_ms: run.created_at_ms,
                            updated_at_ms: run.updated_at_ms,
                            started_at_ms: run.started_at_ms,
                            finished_at_ms: run.finished_at_ms,
                            error_code: run.error_code,
                            parent_run_id: run.parent_run_id,
                            root_run_id: run.root_run_id,
                            depth: run.depth,
                            subagent_id: run.subagent_id,
                            spawned_by_step_id: run.spawned_by_step_id,
                            child_spec_digest: run.child_spec_digest,
                            model_profile_digest: run.model_profile_digest,
                            result_delivered: run.result_delivered_at_ms.is_some(),
                        },
                        terminal_result: run.terminal_result,
                        error_message: run.error_message,
                    })
                });
                let _ = reply.send(result.map_err(Into::into));
            }
            CommandRequest::Tree { run_id, reply } => {
                let _ = reply.send(self.store.run_tree(&run_id).map_err(Into::into));
            }
            CommandRequest::Cancel { run_id, reply } => {
                let now_ms = self.options.clock.now_ms();
                let result = self
                    .store
                    .request_run_tree_cancellation(&run_id, now_ms)
                    .map_err(SupervisorError::from)
                    .and_then(|statuses| {
                        let requested = statuses
                            .iter()
                            .find(|status| status.run_id == run_id)
                            .cloned()
                            .ok_or_else(|| {
                                SupervisorError::Driver(format!(
                                    "cancelled run tree omitted requested run {run_id}"
                                ))
                            })?;
                        for status in statuses {
                            if let Some(active) = self.active.get(&status.run_id) {
                                active.cancellation.cancel();
                            }
                            if status.state == RunState::Cancelled {
                                self.complete_subscribers(&status.run_id);
                            }
                        }
                        Ok(requested)
                    });
                let _ = reply.send(result);
            }
            CommandRequest::EventsAfter {
                run_id,
                after_sequence,
                limit,
                reply,
            } => {
                let _ = reply.send(
                    self.store
                        .run_events_after(&run_id, after_sequence, limit)
                        .map_err(Into::into),
                );
            }
            CommandRequest::Subscribe {
                run_id,
                after_sequence,
                reply,
            } => {
                let _ = reply.send(self.subscribe(run_id, after_sequence));
            }
            CommandRequest::Shutdown => return false,
        }
        true
    }

    fn submit(&self, spec: RunSpec) -> Result<RunAccepted> {
        let now_ms = self.options.clock.now_ms();
        let admission = if let Some(idempotency) = spec.idempotency {
            self.store.admit_idempotent_run(
                &spec.run,
                &idempotency.namespace,
                &idempotency.key,
                &idempotency.fingerprint,
                &self.options.owner_id,
                now_ms.saturating_add(self.options.lease_duration_ms()),
                now_ms,
            )?
        } else {
            agl_store::DurableRunAdmission {
                run: self.store.admit_run_at(&spec.run, now_ms)?,
                replayed: false,
            }
        };
        Ok(RunAccepted {
            status: self
                .store
                .safe_run_status(&admission.run.run_id)?
                .expect("admitted run must be readable"),
            replayed: admission.replayed,
        })
    }

    fn subscribe(&mut self, run_id: RunId, after_sequence: u64) -> Result<RunSubscription> {
        let status = self.store.safe_run_status(&run_id)?.ok_or_else(|| {
            agl_store::StoreError::NotFound {
                resource: format!("run {run_id}"),
            }
        })?;
        let boundary = self.store.latest_run_event_sequence(&run_id)?;
        let backlog = self.store.run_events_after(
            &run_id,
            after_sequence,
            usize::try_from(boundary.saturating_sub(after_sequence))
                .unwrap_or(usize::MAX)
                .max(1),
        )?;
        let (sender, receiver) = mpsc::sync_channel(self.options.subscriber_capacity);
        let last_delivered = Arc::new(AtomicU64::new(
            backlog
                .last()
                .map_or(after_sequence, |event| event.sequence),
        ));
        let overflowed = Arc::new(Mutex::new(false));
        if status.state.is_terminal() {
            let _ = sender.try_send(SubscriptionMessage::Complete);
        } else {
            self.subscribers
                .entry(run_id)
                .or_default()
                .push(Subscriber {
                    sender,
                    last_sequence: boundary,
                    overflowed: overflowed.clone(),
                });
        }
        Ok(RunSubscription {
            backlog,
            receiver,
            last_delivered,
            overflowed,
        })
    }

    fn handle_worker(&mut self, message: WorkerMessage) {
        match message {
            WorkerMessage::EventsCommitted { run_id, events } => {
                if let Some(subscribers) = self.subscribers.get_mut(&run_id) {
                    subscribers.retain_mut(|subscriber| {
                        for event in &events {
                            if event.sequence <= subscriber.last_sequence {
                                continue;
                            }
                            match subscriber
                                .sender
                                .try_send(SubscriptionMessage::Event(Box::new(event.clone())))
                            {
                                Ok(()) => subscriber.last_sequence = event.sequence,
                                Err(mpsc::TrySendError::Full(_)) => {
                                    *subscriber
                                        .overflowed
                                        .lock()
                                        .expect("overflow flag poisoned") = true;
                                    return false;
                                }
                                Err(mpsc::TrySendError::Disconnected(_)) => return false,
                            }
                        }
                        true
                    });
                }
            }
            WorkerMessage::Finished { run_id } => {
                self.active.remove(&run_id);
                if self
                    .store
                    .safe_run_status(&run_id)
                    .ok()
                    .flatten()
                    .is_some_and(|status| status.state.is_terminal())
                {
                    self.complete_subscribers(&run_id);
                }
            }
        }
    }

    fn complete_subscribers(&mut self, run_id: &RunId) {
        if let Some(subscribers) = self.subscribers.remove(run_id) {
            for subscriber in subscribers {
                if matches!(
                    subscriber.sender.try_send(SubscriptionMessage::Complete),
                    Err(mpsc::TrySendError::Full(_))
                ) {
                    *subscriber
                        .overflowed
                        .lock()
                        .expect("overflow flag poisoned") = true;
                }
            }
        }
    }

    fn heartbeat_if_due(&mut self) {
        let now_ms = self.options.clock.now_ms();
        let interval_ms =
            i64::try_from(self.options.heartbeat_interval.as_millis()).unwrap_or(i64::MAX);
        if now_ms.saturating_sub(self.last_heartbeat_ms) < interval_ms {
            return;
        }
        self.last_heartbeat_ms = now_ms;
        if !self.expire_delegation_trees(now_ms) {
            return;
        }
        let expires_at_ms = now_ms.saturating_add(self.options.lease_duration_ms());
        let lost = self
            .active
            .iter()
            .filter_map(|(run_id, active)| {
                self.store
                    .heartbeat_run(&active.lease, expires_at_ms, now_ms)
                    .err()
                    .map(|_| run_id.clone())
            })
            .collect::<Vec<_>>();
        for run_id in lost {
            if let Some(active) = self.active.remove(&run_id) {
                active.cancellation.cancel();
            }
        }
    }

    fn claim_available(&mut self) {
        if !self.expire_delegation_trees(self.options.clock.now_ms()) {
            return;
        }
        while self.active.len() < self.options.worker_limit {
            let now_ms = self.options.clock.now_ms();
            let lease = match self.store.claim_next_run(
                &self.options.owner_id,
                now_ms,
                self.options.lease_duration_ms(),
            ) {
                Ok(Some(lease)) => lease,
                Ok(None) | Err(_) => break,
            };
            let cancellation = RunCancellation::new();
            let run_id = lease.run_id.clone();
            self.active.insert(
                run_id.clone(),
                ActiveRun {
                    lease: lease.clone(),
                    cancellation: cancellation.clone(),
                },
            );
            spawn_worker(WorkerContext {
                store_root: self.store_root.clone(),
                factory: self.factory.clone(),
                options: self.options.clone(),
                sender: self.sender.clone(),
                lease,
                cancellation,
            });
        }
    }

    fn expire_delegation_trees(&mut self, now_ms: i64) -> bool {
        let statuses = match self.store.expire_delegation_trees(now_ms) {
            Ok(statuses) => statuses,
            Err(_) => {
                for active in self.active.values() {
                    active.cancellation.cancel();
                }
                return false;
            }
        };
        for status in statuses {
            if let Some(active) = self.active.get(&status.run_id) {
                active.cancellation.cancel();
            }
            if status.state == RunState::Cancelled {
                self.complete_subscribers(&status.run_id);
            }
        }
        true
    }
}

struct WorkerContext {
    store_root: PathBuf,
    factory: Arc<dyn DurableRunDriverFactory>,
    options: SupervisorOptions,
    sender: mpsc::SyncSender<CoordinatorMessage>,
    lease: RunLease,
    cancellation: RunCancellation,
}

fn spawn_worker(context: WorkerContext) {
    let name = format!("agl-run-{}", context.lease.run_id);
    let fallback_sender = context.sender.clone();
    let fallback_run_id = context.lease.run_id.clone();
    match thread::Builder::new()
        .name(name)
        .spawn(move || run_worker(context))
    {
        Ok(_) => {}
        Err(_) => {
            let _ = fallback_sender.send(CoordinatorMessage::Worker(WorkerMessage::Finished {
                run_id: fallback_run_id,
            }));
        }
    }
}

fn run_worker(context: WorkerContext) {
    let run_id = context.lease.run_id.clone();
    if let Err(error) = run_worker_inner(&context) {
        fail_worker_run(&context, &error);
    }
    let _ = context
        .sender
        .send(CoordinatorMessage::Worker(WorkerMessage::Finished {
            run_id,
        }));
}

fn fail_worker_run(context: &WorkerContext, error: &SupervisorError) {
    let Ok(store) = AglStore::open_current_at(&context.store_root) else {
        return;
    };
    let Ok(Some(run)) = store.run(&context.lease.run_id) else {
        return;
    };
    if run.state != RunState::Running {
        return;
    }
    let checkpoint = run.checkpoint.unwrap_or(serde_json::Value::Null);
    let _ = store.finish_run(
        &context.lease,
        RunState::Failed,
        Some(&checkpoint),
        &run.usage,
        None,
        Some("supervisor_worker_failed"),
        Some(&error.to_string()),
        &[],
        context.options.clock.now_ms(),
    );
}

fn run_worker_inner(context: &WorkerContext) -> Result<()> {
    let store = AglStore::open_current_at(&context.store_root)?;
    let run = store
        .run(&context.lease.run_id)?
        .ok_or_else(|| agl_store::StoreError::NotFound {
            resource: format!("run {}", context.lease.run_id),
        })?;
    let mut driver = context.factory.open(&run, context.cancellation.clone())?;

    loop {
        let mut snapshot = driver.snapshot()?;
        refresh_wall_time(&run, &mut snapshot, context.options.clock.now_ms());
        if budget_exhausted(&run, &snapshot, context.options.clock.now_ms()) {
            finish_terminal(
                &store,
                context,
                &snapshot,
                SupervisorTerminal {
                    state: RunState::Cancelled,
                    result: None,
                    error_code: Some("budget_exhausted".to_string()),
                    error_message: Some("run budget exhausted".to_string()),
                },
            )?;
            return Ok(());
        }
        if let Some(terminal) = snapshot.terminal.clone() {
            finish_terminal(&store, context, &snapshot, terminal)?;
            return Ok(());
        }
        let effect = snapshot.pending_effect.clone().ok_or_else(|| {
            SupervisorError::Driver(
                "driver snapshot has neither a pending effect nor a terminal state".to_string(),
            )
        })?;
        let existing = store.run_step_by_sequence(&run.run_id, effect.sequence)?;
        let step = if let Some(existing) = existing {
            existing
        } else {
            let draft = RunStepDraft {
                step_id: StepId::generate(),
                turn_id: run.turn_id.clone(),
                effect_sequence: effect.sequence,
                effect_kind: effect.kind.clone(),
                delivery_class: effect.delivery_class,
                request: effect.request.clone(),
            };
            let step = store.publish_run_step(
                &context.lease,
                &snapshot.checkpoint,
                &draft,
                &snapshot.events,
                context.options.clock.now_ms(),
            )?;
            notify_events(context, &snapshot.events);
            step
        };
        if step.state != RunStepState::Pending {
            return Err(SupervisorError::Driver(format!(
                "checkpoint pending effect {} has durable step state {:?}",
                effect.sequence, step.state
            )));
        }
        let now_ms = context.options.clock.now_ms();
        let step_lease = store.claim_run_step(
            &context.lease,
            &step.step_id,
            now_ms.saturating_add(context.options.lease_duration_ms()),
            now_ms,
        )?;

        let execution_context = EffectExecutionContext {
            run_id: run.run_id.clone(),
            step_id: step.step_id.clone(),
            attempt: step.attempts.saturating_add(1),
            cancellation: context.cancellation.clone(),
        };
        let effect_result = driver.execute_pending_effect(&execution_context);
        if context.cancellation.is_cancelled() {
            let mut cancelled = driver.snapshot()?;
            refresh_wall_time(&run, &mut cancelled, context.options.clock.now_ms());
            store.complete_run_step(
                &context.lease,
                &step_lease,
                RunStepState::Cancelled,
                effect_result.as_ref().ok(),
                &cancelled.checkpoint,
                &cancelled.usage,
                &cancelled.events,
                Some("run_cancelled"),
                context.options.clock.now_ms(),
            )?;
            notify_events(context, &cancelled.events);
            cancelled.events.clear();
            finish_terminal(
                &store,
                context,
                &cancelled,
                SupervisorTerminal {
                    state: RunState::Cancelled,
                    result: None,
                    error_code: None,
                    error_message: None,
                },
            )?;
            return Ok(());
        }
        match effect_result {
            Ok(result) => {
                let mut next = driver.snapshot()?;
                refresh_wall_time(&run, &mut next, context.options.clock.now_ms());
                let step_state = match next.terminal.as_ref().map(|terminal| terminal.state) {
                    Some(RunState::Cancelled) => RunStepState::Cancelled,
                    Some(RunState::Failed) => RunStepState::Failed,
                    _ => RunStepState::Succeeded,
                };
                store.complete_run_step(
                    &context.lease,
                    &step_lease,
                    step_state,
                    Some(&result),
                    &next.checkpoint,
                    &next.usage,
                    &next.events,
                    next.terminal
                        .as_ref()
                        .and_then(|terminal| terminal.error_code.as_deref()),
                    context.options.clock.now_ms(),
                )?;
                notify_events(context, &next.events);
            }
            Err(error)
                if error.retryable
                    && effect.delivery_class != EffectDeliveryClass::AtMostOnce
                    && (error.retry_limit_exempt
                        || step.attempts.saturating_add(1) < context.options.retry_limit) =>
            {
                let mut failed = driver.snapshot()?;
                refresh_wall_time(&run, &mut failed, context.options.clock.now_ms());
                let not_before_ms = context
                    .options
                    .clock
                    .now_ms()
                    .saturating_add(context.options.retry_delay_ms(step.attempts + 1));
                store.retry_run_step(
                    &context.lease,
                    &step_lease,
                    not_before_ms,
                    &error.code,
                    &failed.checkpoint,
                    &failed.usage,
                    &failed.events,
                    context.options.clock.now_ms(),
                )?;
                notify_events(context, &failed.events);
                return Ok(());
            }
            Err(error) => {
                let mut failed = driver.snapshot()?;
                refresh_wall_time(&run, &mut failed, context.options.clock.now_ms());
                store.complete_run_step(
                    &context.lease,
                    &step_lease,
                    RunStepState::Failed,
                    None,
                    &failed.checkpoint,
                    &failed.usage,
                    &failed.events,
                    Some(&error.code),
                    context.options.clock.now_ms(),
                )?;
                notify_events(context, &failed.events);
                failed.events.clear();
                finish_terminal(
                    &store,
                    context,
                    &failed,
                    SupervisorTerminal {
                        state: RunState::Failed,
                        result: None,
                        error_code: Some(error.code),
                        error_message: Some(error.message),
                    },
                )?;
                return Ok(());
            }
        }
    }
}

fn finish_terminal(
    store: &AglStore,
    context: &WorkerContext,
    snapshot: &DriverSnapshot,
    terminal: SupervisorTerminal,
) -> Result<()> {
    store.finish_run(
        &context.lease,
        terminal.state,
        Some(&snapshot.checkpoint),
        &snapshot.usage,
        terminal.result.as_ref(),
        terminal.error_code.as_deref(),
        terminal.error_message.as_deref(),
        &snapshot.events,
        context.options.clock.now_ms(),
    )?;
    notify_events(context, &snapshot.events);
    Ok(())
}

fn notify_events(context: &WorkerContext, events: &[SafeRuntimeEventEnvelope]) {
    if events.is_empty() {
        return;
    }
    let _ = context
        .sender
        .send(CoordinatorMessage::Worker(WorkerMessage::EventsCommitted {
            run_id: context.lease.run_id.clone(),
            events: events.to_vec(),
        }));
}

fn budget_exhausted(run: &DurableRunRecord, snapshot: &DriverSnapshot, now_ms: i64) -> bool {
    let elapsed = run
        .started_at_ms
        .map_or(0, |started| now_ms.saturating_sub(started).max(0) as u64);
    let usage = &snapshot.usage;
    let budget = &run.budget;
    let aggregate_output_tokens = usage
        .model_output_tokens
        .saturating_add(run.delegation_used_output_tokens)
        .saturating_add(run.delegation_reserved_output_tokens);
    let pending_kind = snapshot
        .pending_effect
        .as_ref()
        .map(|effect| effect.kind.as_str());
    elapsed > budget.wall_time_ms
        || usage.model_input_tokens > budget.model_input_tokens
        || aggregate_output_tokens > budget.model_output_tokens
        || usage.model_attempts > budget.model_attempts
        || usage.capability_calls > budget.capability_calls
        || (snapshot.terminal.is_none()
            && (elapsed >= budget.wall_time_ms
                || (pending_kind == Some("model_generation")
                    && (usage.model_attempts >= budget.model_attempts
                        || usage.model_input_tokens >= budget.model_input_tokens
                        || aggregate_output_tokens >= budget.model_output_tokens))
                || (pending_kind == Some("capability_dispatch")
                    && usage.capability_calls >= budget.capability_calls)))
}

fn refresh_wall_time(run: &DurableRunRecord, snapshot: &mut DriverSnapshot, now_ms: i64) {
    let elapsed = run
        .started_at_ms
        .map_or(0, |started| now_ms.saturating_sub(started).max(0) as u64);
    snapshot.usage.wall_time_ms = snapshot.usage.wall_time_ms.max(elapsed);
}
