use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agl_chat::InferenceClientHandle;
use agl_cron::{CronJob, CronRepository, CronRunStatus};
use agl_inference::{LlamaCppModelRuntime, ModelManager, ModelManagerOptions};
use agl_protocol::{
    DaemonEvent, DaemonEventKind, DaemonRequest, DaemonRequestKind, ProtocolError,
    ProtocolErrorCode, RunSubscriptionFinishedEvent, RunSubscriptionStartedEvent,
};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_store::{AglStore, MatrixNotificationOutboxDraft, RunState};
use anyhow::{Context, Result, bail};

use crate::state::protocol_run_state;
use crate::{
    CronExecution, CronNotification, CronNotifier, CronTargetExecutor, DaemonOptions,
    SharedDaemonState, render_cron_notification_body, run_cron_tick,
};

const CONNECTION_WRITER_CAPACITY: usize = 128;

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

pub struct DaemonServer {
    runtime: AgentLibreRuntimeConfig,
    options: DaemonOptions,
}

impl DaemonServer {
    pub fn new(runtime: AgentLibreRuntimeConfig, options: DaemonOptions) -> Self {
        Self { runtime, options }
    }

    pub fn socket_path(&self) -> &Path {
        &self.options.socket_path
    }

    #[cfg(unix)]
    pub fn run_foreground(self) -> Result<()> {
        let listener = bind_listener(&self.options.socket_path)?;
        listener
            .set_nonblocking(true)
            .context("failed to set daemon socket nonblocking")?;
        let store = AglStore::open_at(self.runtime.paths.store_root())
            .context("failed to open daemon cron store")?;
        let model_manager =
            ModelManager::spawn(ModelManagerOptions::default(), LlamaCppModelRuntime::new())
                .context("failed to start daemon model manager")?;
        let inference_client = InferenceClientHandle::from(model_manager.handle());
        tracing::info!(
            target: "agentlibre::daemon",
            socket_path = %self.options.socket_path.display(),
            "daemon listening"
        );
        let state = SharedDaemonState::open(
            self.runtime.clone(),
            self.options.inference.clone(),
            inference_client.clone(),
        )?;
        let mut last_cron_tick = None;
        let mut linked_cron_runs = BTreeSet::new();
        loop {
            let now = unix_now();
            if last_cron_tick
                .is_none_or(|last| now.saturating_sub(last) >= self.options.cron_interval_seconds)
            {
                last_cron_tick = Some(now);
                let mut executor = DaemonCronExecutor {
                    state: state.clone(),
                };
                let mut notifier = StoreCronNotifier { store: &store };
                match run_cron_tick(&store, now, &mut executor, &mut notifier) {
                    Ok(report) if report.due_jobs > 0 => tracing::info!(
                        target: "agentlibre::daemon",
                        due_jobs = report.due_jobs,
                        recorded_runs = report.recorded_runs.len(),
                        notifications = report.notifications,
                        "cron scheduler tick completed"
                    ),
                    Ok(_) => {}
                    Err(err) => tracing::warn!(
                        target: "agentlibre::daemon",
                        error = %err,
                        "cron scheduler tick failed"
                    ),
                }
                spawn_cron_run_linkers(
                    &self.runtime.paths.store_root(),
                    &store,
                    &state,
                    &mut linked_cron_runs,
                );
                trace_model_manager_status(&state);
            }

            match listener.accept() {
                Ok((stream, _addr)) => {
                    let state = state.clone();
                    thread::Builder::new()
                        .name("agl-daemon-client".to_string())
                        .spawn(move || {
                            if let Err(err) = handle_stream(stream, &state) {
                                tracing::warn!(target: "agentlibre::daemon", error = %err, "daemon client failed");
                            }
                        })
                        .context("failed to spawn daemon client thread")?;
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(250));
                }
                Err(err) => return Err(err).context("failed to accept daemon client"),
            }
        }
    }

    #[cfg(not(unix))]
    pub fn run_foreground(self) -> Result<()> {
        bail!("agl daemon is only available on Unix platforms in this alpha")
    }
}

fn trace_model_manager_status(state: &SharedDaemonState) {
    match state.model_manager_status() {
        Ok(status) => tracing::debug!(
            target: "agentlibre::daemon",
            queue_depth = status.queue_depth,
            loaded_model_digests = ?status.loaded_model_digests,
            active = status.active_scope.is_some(),
            cached_contexts = status.cached_contexts,
            model_loads = status.model_loads,
            context_loads = status.context_loads,
            model_evictions = status.model_evictions,
            context_evictions = status.context_evictions,
            completed_jobs = status.completed_jobs,
            cancellations = status.cancellations,
            deadline_exceeded = status.deadline_exceeded,
            failures = status.failures,
            "model manager status"
        ),
        Err(error) => tracing::warn!(
            target: "agentlibre::daemon",
            error = %error,
            "failed to inspect model manager status"
        ),
    }
}

struct DaemonCronExecutor {
    state: SharedDaemonState,
}

impl CronTargetExecutor for DaemonCronExecutor {
    fn execute(&mut self, job: &CronJob, scheduled_for: &str) -> CronExecution {
        match self.state.submit_cron_job(job, scheduled_for) {
            Ok(accepted) => CronExecution::queued(accepted.status.run_id),
            Err(error) => CronExecution::failed(error.message),
        }
    }
}

fn spawn_cron_run_linkers(
    store_root: &Path,
    store: &AglStore,
    state: &SharedDaemonState,
    linked: &mut BTreeSet<String>,
) {
    let repository = CronRepository::new(store);
    let Ok(runs) = repository.active_supervisor_runs() else {
        return;
    };
    for cron_run in runs {
        if !linked.insert(cron_run.id.clone()) {
            continue;
        }
        let Ok(Some(job)) = repository.job(&cron_run.job_id) else {
            continue;
        };
        let store_root = store_root.to_path_buf();
        let state = state.clone();
        if let Err(error) = thread::Builder::new()
            .name(format!("agl-cron-link-{}", cron_run.id))
            .spawn(move || {
                if let Err(error) = link_cron_run(&store_root, &state, cron_run, job) {
                    tracing::warn!(
                        target: "agentlibre::daemon",
                        error = %error,
                        "failed to link cron run terminal state"
                    );
                }
            })
        {
            tracing::warn!(
                target: "agentlibre::daemon",
                error = %error,
                "failed to spawn cron terminal linker"
            );
        }
    }
}

pub(crate) fn link_cron_run(
    store_root: &Path,
    state: &SharedDaemonState,
    cron_run: agl_cron::CronRun,
    job: CronJob,
) -> Result<()> {
    let supervisor_run_id = cron_run
        .supervisor_run_id
        .clone()
        .context("queued cron run has no supervisor run ID")?;
    if let Ok(subscription) = state.subscribe_run(supervisor_run_id.clone(), 0) {
        while subscription.recv()?.is_some() {}
    }
    let outcome = loop {
        let outcome = state
            .run_outcome(supervisor_run_id.clone())
            .map_err(|error| anyhow::anyhow!(error.message))?;
        if outcome.status.state.is_terminal() {
            break outcome;
        }
        thread::sleep(Duration::from_millis(25));
    };
    let result_ref = format!("run:{supervisor_run_id}");
    let (status, error) = match outcome.status.state {
        RunState::Succeeded => (CronRunStatus::Succeeded, None),
        RunState::Failed => (
            CronRunStatus::Failed,
            outcome
                .error_message
                .or(outcome.status.error_code)
                .or_else(|| Some("scheduled run failed".to_string())),
        ),
        RunState::Cancelled => (
            CronRunStatus::Failed,
            Some("scheduled run was cancelled".to_string()),
        ),
        RunState::Queued | RunState::Running | RunState::Waiting => unreachable!(),
    };
    let store = AglStore::open_current_at(store_root)?;
    let repository = CronRepository::new(&store);
    let run = repository.finish_supervisor_run(
        &supervisor_run_id,
        status,
        Some(&result_ref),
        error.as_deref(),
    )?;
    if let Some(notify_ref) = job.notify_ref {
        let mut notifier = StoreCronNotifier { store: &store };
        notifier.notify(CronNotification {
            notify_ref,
            run_id: run.id,
            job_id: job.id,
            job_name: job.name,
            scheduled_for: run.scheduled_for,
            status: run.status,
            result_ref: run.result_ref,
            error: run.error,
        })?;
    }
    Ok(())
}

struct StoreCronNotifier<'a> {
    store: &'a AglStore,
}

impl CronNotifier for StoreCronNotifier<'_> {
    fn notify(&mut self, notification: CronNotification) -> Result<()> {
        if notification.notify_ref.starts_with("matrix-room:") {
            let body = render_cron_notification_body(&notification);
            let dedupe_key = format!("cron:{}:{}", notification.run_id, notification.notify_ref);
            let item =
                self.store
                    .enqueue_matrix_notification(MatrixNotificationOutboxDraft::new(
                        notification.notify_ref.clone(),
                        "cron",
                        notification.run_id.clone(),
                        dedupe_key,
                        body,
                    ))?;
            tracing::info!(
                target: "agentlibre::daemon",
                notify_ref = %notification.notify_ref,
                outbox_id = %item.id,
                job_id = %notification.job_id,
                job_name = %notification.job_name,
                status = notification.status.as_str(),
                scheduled_for = %notification.scheduled_for,
                result_ref = notification.result_ref.as_deref(),
                error = notification.error.as_deref(),
                "cron Matrix notification queued in store outbox"
            );
        } else {
            tracing::warn!(
                target: "agentlibre::daemon",
                notify_ref = %notification.notify_ref,
                job_id = %notification.job_id,
                "unsupported cron notification target"
            );
        }
        Ok(())
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(unix)]
fn bind_listener(socket_path: &Path) -> Result<UnixListener> {
    let parent = socket_path
        .parent()
        .context("daemon socket path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create daemon socket dir {}", parent.display()))?;

    if socket_path.exists() {
        match UnixStream::connect(socket_path) {
            Ok(_) => bail!(
                "daemon socket is already owned by a live process: {}",
                socket_path.display()
            ),
            Err(_) => std::fs::remove_file(socket_path).with_context(|| {
                format!(
                    "failed to remove stale daemon socket {}",
                    socket_path.display()
                )
            })?,
        }
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind daemon socket {}", socket_path.display()))?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600)).with_context(
        || {
            format!(
                "failed to restrict daemon socket permissions {}",
                socket_path.display()
            )
        },
    )?;
    Ok(listener)
}

#[cfg(unix)]
fn handle_stream(stream: UnixStream, state: &SharedDaemonState) -> Result<()> {
    let writer = stream
        .try_clone()
        .context("failed to clone daemon client stream")?;
    let (event_sender, event_receiver) = mpsc::sync_channel(CONNECTION_WRITER_CAPACITY);
    thread::Builder::new()
        .name("agl-daemon-writer".to_string())
        .spawn(move || run_connection_writer(writer, event_receiver))
        .context("failed to spawn daemon connection writer")?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.context("failed to read daemon request")?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<DaemonRequest>(&line) {
            Ok(DaemonRequest {
                schema,
                request_id,
                kind: DaemonRequestKind::RunSubscribe(request),
            }) => {
                let _ = schema;
                let state = state.clone();
                let sender = event_sender.clone();
                thread::Builder::new()
                    .name(format!("agl-daemon-subscribe-{}", request.run_id))
                    .spawn(move || {
                        if let Err(error) =
                            stream_run_subscription(&sender, &state, request_id, request)
                        {
                            tracing::debug!(
                                target: "agentlibre::daemon",
                                error = %error,
                                "daemon run subscription ended"
                            );
                        }
                    })
                    .context("failed to spawn daemon subscription")?;
            }
            Ok(request) => {
                queue_event(&event_sender, state.handle_request(request))?;
            }
            Err(err) => {
                queue_event(
                    &event_sender,
                    DaemonEvent::new(
                        None,
                        DaemonEventKind::Error(ProtocolError::new(
                            ProtocolErrorCode::InvalidRequest,
                            format!("invalid daemon request JSON: {err}"),
                            false,
                        )),
                    ),
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn stream_run_subscription(
    sender: &mpsc::SyncSender<DaemonEvent>,
    state: &SharedDaemonState,
    request_id: agl_ids::RequestId,
    request: agl_protocol::RunSubscribeRequest,
) -> Result<()> {
    let subscription = match state.subscribe_run(request.run_id.clone(), request.after_sequence) {
        Ok(subscription) => subscription,
        Err(error) => {
            return queue_event(
                sender,
                DaemonEvent::new(Some(request_id), DaemonEventKind::Error(error)),
            );
        }
    };
    let replay_boundary = subscription
        .backlog
        .last()
        .map_or(request.after_sequence, |event| event.sequence);
    queue_event(
        sender,
        DaemonEvent::new(
            Some(request_id.clone()),
            DaemonEventKind::RunSubscriptionStarted(RunSubscriptionStartedEvent {
                run_id: request.run_id.clone(),
                after_sequence: request.after_sequence,
                replay_boundary,
            }),
        ),
    )?;
    let mut last_sequence = request.after_sequence;
    for event in &subscription.backlog {
        last_sequence = event.sequence;
        queue_event(
            sender,
            DaemonEvent::new(
                Some(request_id.clone()),
                DaemonEventKind::RunEvent(Box::new(event.clone())),
            ),
        )?;
    }
    loop {
        match subscription.recv() {
            Ok(Some(event)) => {
                last_sequence = event.sequence;
                queue_event(
                    sender,
                    DaemonEvent::new(
                        Some(request_id.clone()),
                        DaemonEventKind::RunEvent(Box::new(event)),
                    ),
                )?;
            }
            Ok(None) => {
                let outcome = state
                    .run_outcome(request.run_id.clone())
                    .map_err(|error| anyhow::anyhow!(error.message))?;
                return queue_event(
                    sender,
                    DaemonEvent::new(
                        Some(request_id),
                        DaemonEventKind::RunSubscriptionFinished(RunSubscriptionFinishedEvent {
                            run_id: request.run_id,
                            state: protocol_run_state(outcome.status.state),
                            last_sequence,
                            terminal_result: outcome.terminal_result,
                            error_code: outcome.status.error_code,
                            error_message: outcome.error_message,
                        }),
                    ),
                );
            }
            Err(error) => {
                let mut protocol =
                    ProtocolError::new(ProtocolErrorCode::Busy, error.to_string(), true);
                protocol
                    .safe_metadata
                    .insert("last_sequence".to_string(), last_sequence.to_string());
                return queue_event(
                    sender,
                    DaemonEvent::new(Some(request_id), DaemonEventKind::Error(protocol)),
                );
            }
        }
    }
}

#[cfg(unix)]
fn queue_event(sender: &mpsc::SyncSender<DaemonEvent>, event: DaemonEvent) -> Result<()> {
    sender.try_send(event).map_err(|error| match error {
        mpsc::TrySendError::Full(_) => anyhow::anyhow!("daemon connection writer queue is full"),
        mpsc::TrySendError::Disconnected(_) => {
            anyhow::anyhow!("daemon connection writer is disconnected")
        }
    })
}

#[cfg(unix)]
fn run_connection_writer(mut writer: UnixStream, events: mpsc::Receiver<DaemonEvent>) {
    for event in events {
        if let Err(error) = write_event(&mut writer, &event) {
            tracing::debug!(
                target: "agentlibre::daemon",
                error = %error,
                "daemon connection writer stopped"
            );
            break;
        }
    }
}

#[cfg(unix)]
fn write_event(writer: &mut impl Write, event: &DaemonEvent) -> Result<()> {
    serde_json::to_writer(&mut *writer, event).context("failed to serialize daemon event")?;
    writer
        .write_all(b"\n")
        .context("failed to write daemon event newline")?;
    writer.flush().context("failed to flush daemon event")
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
