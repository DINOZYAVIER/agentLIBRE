use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agl_chat::InferenceClientHandle;
use agl_cron::{
    CronJob, CronRepository, CronRunStatus, CronTargetKind, STORE_STATUS_BUILTIN_CRON_TARGET,
    unsupported_builtin_cron_target_message,
};
use agl_inference::{EngineRuntimeStatusHandle, InferenceHost, InferenceHostConfig};
use agl_protocol::{
    DaemonEvent, DaemonEventKind, DaemonRequest, DaemonRequestKind,
    PresentationSubscriptionFinishReason, ProtocolError, ProtocolErrorCode,
    RunSubscriptionFinishedEvent, RunSubscriptionStartedEvent, SessionPresentationSnapshotTransfer,
    SessionPresentationSnapshotTransferPurpose, SessionPresentationSubscriptionFinishedEvent,
    SubscriptionCancelledEvent,
};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_store::{AglStore, MatrixNotificationOutboxDraft, RunState};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt as _;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{UnixListener as TokioUnixListener, UnixStream as TokioUnixStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::codec::{FramedRead, LinesCodec};

use crate::state::protocol_run_state;
use crate::{
    CronExecution, CronNotification, CronNotifier, CronTargetExecutor, DaemonOptions,
    ListenerSource, SharedDaemonState, default_socket_path, render_cron_notification_body,
    run_cron_tick,
};

const CONNECTION_WRITER_CAPACITY: usize = 128;
const CONNECTION_TASK_CAPACITY: usize = 128;
const CONNECTION_SUBSCRIPTION_CAPACITY: usize = 128;
const RUN_SUBSCRIPTION_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[cfg(unix)]
use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};

pub struct DaemonServer {
    runtime: AgentLibreRuntimeConfig,
    options: DaemonOptions,
}

impl DaemonServer {
    pub fn new(runtime: AgentLibreRuntimeConfig, options: DaemonOptions) -> Self {
        Self { runtime, options }
    }

    pub fn listener_source(&self) -> &ListenerSource {
        &self.options.listener_source
    }

    #[cfg(unix)]
    pub fn run_foreground(self) -> Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to build daemon Tokio runtime")?
            .block_on(self.run_async())
    }

    #[cfg(unix)]
    async fn run_async(self) -> Result<()> {
        let listener = match &self.options.listener_source {
            ListenerSource::Bind(path) => {
                bind_listener(path, path == &default_socket_path(&self.runtime.paths))?
            }
            ListenerSource::Systemd => crate::activation::claim_systemd_listener()?,
        };
        let store = AglStore::open_at(self.runtime.paths.store_root())
            .context("failed to open daemon cron store")?;
        crate::state::verify_terminal_handshake(&self.runtime).await?;
        let inference_host = InferenceHost::start_with_journal_root(
            InferenceHostConfig::development_default(
                self.runtime.paths.inference_state_root().join("authority"),
                self.runtime.paths.default_artifact_root(),
                std::time::Duration::from_secs(
                    self.runtime.inference.residency.context_idle_seconds,
                ),
                std::time::Duration::from_secs(self.runtime.inference.residency.model_idle_seconds),
            )?,
            self.runtime.paths.inference_state_root().join("attempts"),
        )
        .context("failed to start daemon inference host")?;
        let inference_status = EngineRuntimeStatusHandle::default();
        let inference_client = InferenceClientHandle::from(inference_host);
        tracing::info!(
            target: "agentlibre::daemon",
            listener = %self.options.listener_source,
            "daemon listening"
        );
        let state = SharedDaemonState::open(
            self.runtime.clone(),
            self.options.inference.clone(),
            inference_client.clone(),
            inference_status,
        )?;
        let mut cron_tick = tokio::time::interval(Duration::from_secs(
            self.options.cron_interval_seconds.max(1),
        ));
        cron_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut linked_cron_runs = BTreeSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _addr) = accepted.context("failed to accept daemon client")?;
                    let state = state.clone();
                    tokio::spawn(async move {
                        if let Err(err) = serve_connection(stream, &state).await {
                            tracing::warn!(target: "agentlibre::daemon", error = %err, "daemon client failed");
                        }
                    });
                }
                _ = cron_tick.tick() => {
                    let now = unix_now();
                    let mut executor = DaemonCronExecutor { state: state.clone(), store: &store };
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
                        Err(err) => tracing::warn!(target: "agentlibre::daemon", error = %err, "cron scheduler tick failed"),
                    }
                    spawn_cron_run_linkers(
                        &self.runtime.paths.store_root(),
                        &store,
                        &state,
                        &mut linked_cron_runs,
                    );
                    trace_model_manager_status(&state);
                }
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
            active = status.active_scope.is_some(),
            resident_models = status.resident_models,
            resident_contexts = status.resident_contexts,
            next_residency_deadline_after_ms = status.next_residency_deadline_after_ms,
            automatic_context_unloads = status.automatic_context_unloads,
            automatic_model_unloads = status.automatic_model_unloads,
            manual_unloads = status.manual_unloads,
            unload_failures = status.unload_failures,
            model_loads = status.model_loads,
            context_loads = status.context_loads,
            model_evictions = status.model_evictions,
            context_evictions = status.context_evictions,
            completed_jobs = status.completed_jobs,
            incomplete_jobs = status.incomplete_jobs,
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

struct DaemonCronExecutor<'a> {
    state: SharedDaemonState,
    store: &'a AglStore,
}

impl CronTargetExecutor for DaemonCronExecutor<'_> {
    fn execute(&mut self, job: &CronJob, scheduled_for: &str) -> CronExecution {
        if let Some(execution) = execute_builtin_cron_target(job, self.store) {
            return execution;
        }
        match self.state.submit_cron_job(job, scheduled_for) {
            Ok(accepted) => CronExecution::queued(accepted.status.run_id),
            Err(error) => CronExecution::failed(error.message),
        }
    }
}

fn execute_builtin_cron_target(job: &CronJob, store: &AglStore) -> Option<CronExecution> {
    if job.target_kind != CronTargetKind::Builtin {
        return None;
    }
    Some(match job.target_ref.as_str() {
        STORE_STATUS_BUILTIN_CRON_TARGET => store
            .health()
            .map(|health| {
                CronExecution::succeeded(format!(
                    "builtin:store-status:schema:{}",
                    health.migration_version
                ))
            })
            .unwrap_or_else(|error| CronExecution::failed(error.to_string())),
        _ => CronExecution::failed(unsupported_builtin_cron_target_message(&job.target_ref)),
    })
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
        RunState::Incomplete => (
            CronRunStatus::Failed,
            Some("scheduled run produced incomplete output".to_string()),
        ),
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
fn bind_listener(socket_path: &Path, tighten_owned_parent: bool) -> Result<TokioUnixListener> {
    if !socket_path.is_absolute() || socket_path.file_name().is_none() {
        bail!("daemon socket path must be one absolute file path");
    }
    let parent = socket_path
        .parent()
        .context("daemon socket path has no parent directory")?;
    ensure_private_socket_parent(parent, tighten_owned_parent)?;

    match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket()
                || metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.nlink() != 1
            {
                bail!(
                    "daemon socket target must be one owned Unix socket: {}",
                    socket_path.display()
                );
            }
            match StdUnixStream::connect(socket_path) {
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
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect daemon socket target {}",
                    socket_path.display()
                )
            });
        }
    }

    let listener = StdUnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind daemon socket {}", socket_path.display()))?;
    listener
        .set_nonblocking(true)
        .context("failed to set daemon socket nonblocking")?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600)).with_context(
        || {
            format!(
                "failed to restrict daemon socket permissions {}",
                socket_path.display()
            )
        },
    )?;
    let metadata = std::fs::symlink_metadata(socket_path).with_context(|| {
        format!(
            "failed to verify daemon socket permissions {}",
            socket_path.display()
        )
    })?;
    if !metadata.file_type().is_socket()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        drop(listener);
        let _ = std::fs::remove_file(socket_path);
        bail!(
            "daemon socket must be one owned private Unix socket: {}",
            socket_path.display()
        );
    }
    TokioUnixListener::from_std(listener).context("failed to adopt daemon socket into Tokio")
}

#[cfg(unix)]
fn ensure_private_socket_parent(parent: &Path, tighten_owned_mode: bool) -> Result<()> {
    if !parent.is_absolute() {
        bail!("daemon socket parent must be absolute");
    }
    let created = match std::fs::symlink_metadata(parent) {
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(parent).with_context(|| {
                format!("failed to create daemon socket dir {}", parent.display())
            })?;
            true
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to inspect daemon socket dir {}", parent.display())
            });
        }
    };
    let metadata = std::fs::symlink_metadata(parent)
        .with_context(|| format!("failed to verify daemon socket dir {}", parent.display()))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        bail!(
            "daemon socket dir must be owned by the daemon UID and contain no symlink: {}",
            parent.display()
        );
    }
    let canonical = parent.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize daemon socket dir {}",
            parent.display()
        )
    })?;
    if canonical != parent {
        bail!(
            "daemon socket dir must be canonical and contain no symlink components: {}",
            parent.display()
        );
    }
    if metadata.mode() & 0o777 != 0o700 {
        if !created && !tighten_owned_mode {
            bail!(
                "custom daemon socket dir must already have mode 0700: {}",
                parent.display()
            );
        }
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).with_context(
            || format!("failed to restrict daemon socket dir {}", parent.display()),
        )?;
    }
    let metadata = std::fs::symlink_metadata(parent).with_context(|| {
        format!(
            "failed to re-verify daemon socket dir after restricting it {}",
            parent.display()
        )
    })?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        bail!(
            "daemon socket dir must be owned by the daemon UID with mode 0700 and no symlink: {}",
            parent.display()
        );
    }
    if parent.canonicalize().with_context(|| {
        format!(
            "failed to re-canonicalize daemon socket dir {}",
            parent.display()
        )
    })? != parent
    {
        bail!(
            "daemon socket dir changed while its permissions were being restricted: {}",
            parent.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
#[doc(hidden)]
pub async fn serve_connection(stream: TokioUnixStream, state: &SharedDaemonState) -> Result<()> {
    let credentials = stream
        .peer_cred()
        .context("failed to read daemon peer credentials")?;
    let expected_uid = unsafe { libc::geteuid() };
    let operator_uid = credentials.uid();
    if operator_uid != expected_uid {
        bail!("private daemon connection peer UID does not match daemon UID");
    }
    serve_authenticated_connection(stream, state, operator_uid).await
}

#[cfg(all(unix, test))]
pub(crate) async fn serve_authenticated_test_connection<S>(
    stream: S,
    state: &SharedDaemonState,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    serve_authenticated_connection(stream, state, unsafe { libc::geteuid() }).await
}

#[cfg(unix)]
async fn serve_authenticated_connection<S>(
    stream: S,
    state: &SharedDaemonState,
    operator_uid: u32,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    let (raw_event_sender, event_receiver) = mpsc::channel(CONNECTION_WRITER_CAPACITY);
    let (shutdown, mut shutdown_receiver) = watch::channel(false);
    let event_sender = ConnectionEventSender {
        events: raw_event_sender,
        shutdown: shutdown.clone(),
    };
    let writer_task = tokio::spawn(run_connection_writer(writer, event_receiver, shutdown));
    let subscriptions = ConnectionSubscriptions::default();
    let mut reader = FramedRead::new(
        reader,
        LinesCodec::new_with_max_length(agl_protocol::MAX_JSONL_FRAME_BYTES),
    );
    let mut tasks = JoinSet::new();
    let result = loop {
        tokio::select! {
            _ = shutdown_receiver.changed() => break Ok(()),
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::debug!(target: "agentlibre::daemon", error = %error, "daemon connection task ended");
                }
            }
            line = reader.next() => {
            let Some(line) = line else { break Ok(()); };
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
                    if reject_connection_task_overflow(&tasks, &event_sender, &request_id)? {
                        continue;
                    }
                    if let Err(error) = subscriptions.reserve(&request_id) {
                        queue_event(
                            &event_sender,
                            DaemonEvent::new(Some(request_id), DaemonEventKind::Error(error)),
                        )?;
                        continue;
                    }
                    let state = state.clone();
                    let sender = event_sender.clone();
                    let subscription_id = request_id.clone();
                    let task_request_id = request_id.clone();
                    let subscriptions_for_task = subscriptions.clone();
                    let cancellation = Arc::new(AtomicBool::new(false));
                    let task_cancellation = Arc::clone(&cancellation);
                    let application = state.application();
                    let refresh_state = state.clone();
                    let refresh_run_id = request.run_id.clone();
                    let handle = tasks.spawn(async move {
                        tokio::task::spawn_blocking(move || {
                            if let Err(error) = stream_run_subscription(
                                &sender,
                                &state,
                                task_request_id,
                                request,
                                &task_cancellation,
                            )
                            {
                                tracing::debug!(
                                    target: "agentlibre::daemon",
                                    error = %error,
                                    "daemon run subscription ended"
                                );
                            }
                        }).await.ok();
                        if let Ok(outcome) = refresh_state.run_outcome(refresh_run_id)
                            && let Some(session_id) = outcome.status.session_id
                        {
                            let _ = application.refresh(&session_id).await;
                        }
                        subscriptions_for_task.remove(&subscription_id);
                    });
                    subscriptions.install(&request_id, handle, Some(cancellation));
                }
                Ok(DaemonRequest {
                    request_id,
                    kind: DaemonRequestKind::RunSubmit(request),
                    ..
                }) => {
                    if reject_connection_task_overflow(&tasks, &event_sender, &request_id)? {
                        continue;
                    }
                    let application = state.application();
                    let sender = event_sender.clone();
                    tasks.spawn(async move {
                        let event = crate::surface::handle_prompt_submit_request(
                            &application,
                            request_id,
                            request,
                        )
                        .await;
                        let _ = queue_event(&sender, event);
                    });
                }
                Ok(DaemonRequest {
                    request_id,
                    kind: DaemonRequestKind::SessionPresentation(request),
                    ..
                }) => {
                    if reject_connection_task_overflow(&tasks, &event_sender, &request_id)? {
                        continue;
                    }
                    if let Err(error) = subscriptions.reserve(&request_id) {
                        queue_event(
                            &event_sender,
                            DaemonEvent::new(Some(request_id), DaemonEventKind::Error(error)),
                        )?;
                        continue;
                    }
                    let application = state.application();
                    let sender = event_sender.clone();
                    let subscription_id = request_id.clone();
                    let task_request_id = request_id.clone();
                    let subscriptions_for_task = subscriptions.clone();
                    let handle = tasks.spawn(async move {
                        if let Err(error) = stream_presentation_page(
                            &sender,
                            &application,
                            task_request_id,
                            request,
                        )
                        .await
                        {
                            tracing::debug!(target: "agentlibre::daemon", error = %error, "presentation page transfer ended");
                        }
                        subscriptions_for_task.remove(&subscription_id);
                    });
                    subscriptions.install(&request_id, handle, None);
                }
                Ok(DaemonRequest {
                    request_id,
                    kind: DaemonRequestKind::SessionPresentationSubscribe(request),
                    ..
                }) => {
                    if reject_connection_task_overflow(&tasks, &event_sender, &request_id)? {
                        continue;
                    }
                    if let Err(error) = subscriptions.reserve(&request_id) {
                        queue_event(
                            &event_sender,
                            DaemonEvent::new(Some(request_id), DaemonEventKind::Error(error)),
                        )?;
                        continue;
                    }
                    let application = state.application();
                    let sender = event_sender.clone();
                    let subscription_id = request_id.clone();
                    let task_request_id = request_id.clone();
                    let subscriptions_for_task = subscriptions.clone();
                    let handle = tasks.spawn(async move {
                        if let Err(error) = stream_presentation_subscription(
                            &sender,
                            &application,
                            task_request_id,
                            request,
                        ).await {
                            tracing::debug!(target: "agentlibre::daemon", error = %error, "presentation subscription ended");
                        }
                        subscriptions_for_task.remove(&subscription_id);
                    });
                    subscriptions.install(&request_id, handle, None);
                }
                Ok(DaemonRequest {
                    request_id,
                    kind: DaemonRequestKind::SubscriptionCancel(request),
                    ..
                }) => {
                    if subscriptions.cancel(&request.subscription_request_id) {
                        queue_event(
                            &event_sender,
                            DaemonEvent::new(
                                Some(request_id),
                                DaemonEventKind::SubscriptionCancelled(
                                    SubscriptionCancelledEvent {
                                        subscription_request_id: request.subscription_request_id,
                                    },
                                ),
                            ),
                        )?;
                    } else {
                        queue_event(
                            &event_sender,
                            DaemonEvent::new(
                                Some(request_id),
                                DaemonEventKind::Error(ProtocolError::new(
                                    ProtocolErrorCode::NotFound,
                                    "subscription is not active on this connection",
                                    false,
                                )),
                            ),
                        )?;
                    }
                }
                Ok(DaemonRequest {
                    request_id,
                    kind: DaemonRequestKind::HumanHostTerminalEnsure(request),
                    ..
                }) => {
                    if reject_connection_task_overflow(&tasks, &event_sender, &request_id)? {
                        continue;
                    }
                    let state = state.clone();
                    let application = state.application();
                    let sender = event_sender.clone();
                    tasks.spawn(async move {
                        let event = crate::surface::handle_human_host_terminal_request(
                            &state,
                            &application,
                            request_id,
                            request,
                            operator_uid,
                        )
                        .await;
                        let _ = queue_event(&sender, event);
                    });
                }
                Ok(DaemonRequest {
                    request_id,
                    kind: kind @ (DaemonRequestKind::CommandCatalog(_)
                    | DaemonRequestKind::CommandSuggestions(_)
                    | DaemonRequestKind::ApplicationAction(_)
                    | DaemonRequestKind::HumanTerminalEnsure(_)),
                    ..
                }) => {
                    if reject_connection_task_overflow(&tasks, &event_sender, &request_id)? {
                        continue;
                    }
                    let application = state.application();
                    let sender = event_sender.clone();
                    tasks.spawn(async move {
                        let event = crate::surface::handle_finite_request(
                            &application,
                            request_id,
                            kind,
                            operator_uid,
                        ).await;
                        let _ = queue_event(&sender, event);
                    });
                }
                Ok(request) => {
                    if reject_connection_task_overflow(
                        &tasks,
                        &event_sender,
                        &request.request_id,
                    )? {
                        continue;
                    }
                    let state = state.clone();
                    let application = state.application();
                    let sender = event_sender.clone();
                    tasks.spawn(async move {
                        let event = state.handle_request_async(request).await;
                        if let DaemonEventKind::SessionFinished(finished) = &event.kind {
                            let _ = application
                                .finish_session_projection(&finished.session_id)
                                .await;
                        }
                        let _ = queue_event(&sender, event);
                    });
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
        }
    };
    subscriptions.cancel_all();
    tasks.abort_all();
    drop(event_sender);
    let _ = writer_task.await;
    result
}

#[cfg(unix)]
#[derive(Clone, Default)]
struct ConnectionSubscriptions {
    inner: Arc<Mutex<BTreeMap<agl_ids::RequestId, Option<ConnectionSubscriptionTask>>>>,
}

#[cfg(unix)]
#[derive(Clone)]
struct ConnectionSubscriptionTask {
    handle: tokio::task::AbortHandle,
    cancellation: Option<Arc<AtomicBool>>,
}

#[cfg(unix)]
impl ConnectionSubscriptionTask {
    fn cancel(self) {
        if let Some(cancellation) = self.cancellation {
            cancellation.store(true, Ordering::Release);
        }
        self.handle.abort();
    }
}

#[cfg(unix)]
impl ConnectionSubscriptions {
    fn reserve(&self, request_id: &agl_ids::RequestId) -> std::result::Result<(), ProtocolError> {
        let mut subscriptions = self.inner.lock().map_err(|_| {
            ProtocolError::new(
                ProtocolErrorCode::Internal,
                "connection subscription registry is poisoned",
                false,
            )
        })?;
        if subscriptions.contains_key(request_id) {
            return Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "duplicate subscription request ID on one connection",
                false,
            ));
        }
        if subscriptions.len() >= CONNECTION_SUBSCRIPTION_CAPACITY {
            return Err(ProtocolError::new(
                ProtocolErrorCode::InputBackpressure,
                "connection reached its bounded subscription limit",
                true,
            ));
        }
        subscriptions.insert(request_id.clone(), None);
        Ok(())
    }

    fn install(
        &self,
        request_id: &agl_ids::RequestId,
        handle: tokio::task::AbortHandle,
        cancellation: Option<Arc<AtomicBool>>,
    ) {
        let task = ConnectionSubscriptionTask {
            handle,
            cancellation,
        };
        let installed = self
            .inner
            .lock()
            .ok()
            .and_then(|mut subscriptions| {
                subscriptions
                    .get_mut(request_id)
                    .map(|slot| *slot = Some(task.clone()))
            })
            .is_some();
        if !installed {
            task.cancel();
        }
    }

    fn remove(&self, request_id: &agl_ids::RequestId) {
        if let Ok(mut subscriptions) = self.inner.lock() {
            subscriptions.remove(request_id);
        }
    }

    fn cancel(&self, request_id: &agl_ids::RequestId) -> bool {
        let Some(handle) = self
            .inner
            .lock()
            .ok()
            .and_then(|mut subscriptions| subscriptions.remove(request_id))
        else {
            return false;
        };
        if let Some(task) = handle {
            task.cancel();
        }
        true
    }

    fn cancel_all(&self) {
        let subscriptions = match self.inner.lock() {
            Ok(mut subscriptions) => std::mem::take(&mut *subscriptions),
            Err(_) => return,
        };
        for task in subscriptions.into_values().flatten() {
            task.cancel();
        }
    }
}

#[cfg(unix)]
async fn stream_presentation_page(
    sender: &ConnectionEventSender,
    application: &agl_app::ApplicationService,
    request_id: agl_ids::RequestId,
    request: agl_protocol::SessionPresentationRequest,
) -> Result<()> {
    let page = match application
        .snapshot_page(&request.session_id, request.page_cursor)
        .await
    {
        Ok(page) => page,
        Err(error) => return queue_application_error(sender, request_id, error),
    };
    let snapshot =
        match crate::surface::presentation_snapshot(page.snapshot, page.older_page_cursor) {
            Ok(snapshot) => snapshot,
            Err(error) => return queue_application_error(sender, request_id, error),
        };
    queue_presentation_snapshot_transfer(
        sender,
        request_id,
        SessionPresentationSnapshotTransferPurpose::Requested,
        &snapshot,
    )
}

#[cfg(unix)]
async fn stream_presentation_subscription(
    sender: &ConnectionEventSender,
    application: &agl_app::ApplicationService,
    request_id: agl_ids::RequestId,
    request: agl_protocol::SessionPresentationSubscribeRequest,
) -> Result<()> {
    let mut subscription = match application
        .subscribe(crate::surface::presentation_subscribe(
            request.session_id.clone(),
        ))
        .await
    {
        Ok(subscription) => subscription,
        Err(error) => {
            return queue_event(
                sender,
                DaemonEvent::new(
                    Some(request_id),
                    DaemonEventKind::Error(crate::surface::protocol_error(error)),
                ),
            );
        }
    };
    let snapshot = match crate::surface::presentation_snapshot(
        subscription.snapshot.clone(),
        subscription.older_page_cursor.clone(),
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => return queue_application_error(sender, request_id, error),
    };
    let already_finished =
        snapshot.header.status == agl_protocol::SessionPresentationStatus::Finished;
    let mut last_cursor = snapshot.cursor.clone();
    queue_presentation_snapshot_transfer(
        sender,
        request_id.clone(),
        SessionPresentationSnapshotTransferPurpose::SubscriptionInitial,
        &snapshot,
    )?;
    if already_finished {
        return queue_event(
            sender,
            DaemonEvent::new(
                Some(request_id),
                DaemonEventKind::SessionPresentationSubscriptionFinished(
                    SessionPresentationSubscriptionFinishedEvent {
                        session_id: request.session_id,
                        last_delivered_cursor: last_cursor,
                        reason: PresentationSubscriptionFinishReason::SessionFinished,
                    },
                ),
            ),
        );
    }
    loop {
        match subscription.next().await {
            Ok(event) => {
                let agl_app::SessionPresentationEventEnvelope {
                    event_id,
                    session_id,
                    cursor,
                    event,
                } = event;
                let event = match event {
                    agl_app::SessionPresentationEvent::SnapshotReplaced {
                        snapshot,
                        older_page_cursor,
                    } => {
                        let snapshot = match crate::surface::presentation_snapshot(
                            *snapshot,
                            older_page_cursor,
                        ) {
                            Ok(snapshot) => snapshot,
                            Err(error) => {
                                return queue_application_error(sender, request_id, error);
                            }
                        };
                        if snapshot.session_id != session_id
                            || snapshot.cursor.daemon_instance_id != cursor.daemon_instance_id
                            || snapshot.cursor.revision != cursor.revision
                        {
                            return queue_application_error(
                                sender,
                                request_id,
                                agl_app::ApplicationError::new(
                                    agl_app::ApplicationErrorCode::Internal,
                                    "replacement snapshot identity does not match its live event",
                                ),
                            );
                        }
                        last_cursor = snapshot.cursor.clone();
                        queue_presentation_snapshot_transfer(
                            sender,
                            request_id.clone(),
                            SessionPresentationSnapshotTransferPurpose::Replacement { event_id },
                            &snapshot,
                        )?;
                        continue;
                    }
                    event => event,
                };
                let session_finished =
                    matches!(&event, agl_app::SessionPresentationEvent::SessionFinished);
                let event = match crate::surface::presentation_event(
                    agl_app::SessionPresentationEventEnvelope {
                        event_id,
                        session_id,
                        cursor,
                        event,
                    },
                ) {
                    Ok(event) => event,
                    Err(error) => return queue_application_error(sender, request_id, error),
                };
                last_cursor = event.cursor.clone();
                queue_event(
                    sender,
                    DaemonEvent::new(
                        Some(request_id.clone()),
                        DaemonEventKind::SessionPresentationEvent(Box::new(event)),
                    ),
                )?;
                if session_finished {
                    return queue_event(
                        sender,
                        DaemonEvent::new(
                            Some(request_id),
                            DaemonEventKind::SessionPresentationSubscriptionFinished(
                                SessionPresentationSubscriptionFinishedEvent {
                                    session_id: request.session_id,
                                    last_delivered_cursor: last_cursor,
                                    reason: PresentationSubscriptionFinishReason::SessionFinished,
                                },
                            ),
                        ),
                    );
                }
            }
            Err(error) => {
                let reason = if error.code == agl_app::ApplicationErrorCode::ResyncRequired {
                    PresentationSubscriptionFinishReason::ResyncRequired
                } else {
                    PresentationSubscriptionFinishReason::DaemonShutdown
                };
                return queue_event(
                    sender,
                    DaemonEvent::new(
                        Some(request_id),
                        DaemonEventKind::SessionPresentationSubscriptionFinished(
                            SessionPresentationSubscriptionFinishedEvent {
                                session_id: request.session_id,
                                last_delivered_cursor: last_cursor,
                                reason,
                            },
                        ),
                    ),
                );
            }
        }
    }
}

#[cfg(unix)]
fn queue_presentation_snapshot_transfer(
    sender: &ConnectionEventSender,
    request_id: agl_ids::RequestId,
    purpose: SessionPresentationSnapshotTransferPurpose,
    snapshot: &agl_protocol::SessionPresentationSnapshot,
) -> Result<()> {
    let transfer = match SessionPresentationSnapshotTransfer::encode(
        agl_ids::RequestId::generate(),
        purpose,
        snapshot,
    ) {
        Ok(transfer) => transfer,
        Err(_) => {
            return queue_event(
                sender,
                DaemonEvent::new(
                    Some(request_id),
                    DaemonEventKind::Error(ProtocolError::new(
                        ProtocolErrorCode::Internal,
                        "session presentation snapshot could not be transferred safely",
                        false,
                    )),
                ),
            );
        }
    };
    queue_event(
        sender,
        DaemonEvent::new(
            Some(request_id.clone()),
            DaemonEventKind::SessionPresentationSnapshotManifest(transfer.manifest),
        ),
    )?;
    for chunk in transfer.chunks {
        queue_event(
            sender,
            DaemonEvent::new(
                Some(request_id.clone()),
                DaemonEventKind::SessionPresentationSnapshotChunk(chunk),
            ),
        )?;
    }
    queue_event(
        sender,
        DaemonEvent::new(
            Some(request_id),
            DaemonEventKind::SessionPresentationSnapshotFinished(transfer.finished),
        ),
    )
}

#[cfg(unix)]
fn queue_application_error(
    sender: &ConnectionEventSender,
    request_id: agl_ids::RequestId,
    error: agl_app::ApplicationError,
) -> Result<()> {
    queue_event(
        sender,
        DaemonEvent::new(
            Some(request_id),
            DaemonEventKind::Error(crate::surface::protocol_error(error)),
        ),
    )
}

#[cfg(unix)]
fn stream_run_subscription(
    sender: &ConnectionEventSender,
    state: &SharedDaemonState,
    request_id: agl_ids::RequestId,
    request: agl_protocol::RunSubscribeRequest,
    cancellation: &AtomicBool,
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
        if cancellation.load(Ordering::Acquire) {
            return Ok(());
        }
        match subscription.recv_timeout(RUN_SUBSCRIPTION_CANCEL_POLL_INTERVAL) {
            Ok(agl_supervisor::RunSubscriptionPoll::Event(event)) => {
                last_sequence = event.sequence;
                queue_event(
                    sender,
                    DaemonEvent::new(Some(request_id.clone()), DaemonEventKind::RunEvent(event)),
                )?;
            }
            Ok(agl_supervisor::RunSubscriptionPoll::Complete) => {
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
            Ok(agl_supervisor::RunSubscriptionPoll::Pending) => {}
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
#[derive(Clone)]
struct ConnectionEventSender {
    events: mpsc::Sender<DaemonEvent>,
    shutdown: watch::Sender<bool>,
}

#[cfg(unix)]
fn reject_connection_task_overflow(
    tasks: &JoinSet<()>,
    sender: &ConnectionEventSender,
    request_id: &agl_ids::RequestId,
) -> Result<bool> {
    if tasks.len() < CONNECTION_TASK_CAPACITY {
        return Ok(false);
    }
    queue_event(
        sender,
        DaemonEvent::new(
            Some(request_id.clone()),
            DaemonEventKind::Error(ProtocolError::new(
                ProtocolErrorCode::InputBackpressure,
                "connection reached its bounded concurrent request limit",
                true,
            )),
        ),
    )?;
    Ok(true)
}

#[cfg(unix)]
fn queue_event(sender: &ConnectionEventSender, event: DaemonEvent) -> Result<()> {
    let event = match event.validate() {
        Ok(()) => event,
        Err(error) if !matches!(&event.kind, DaemonEventKind::Error(_)) => {
            tracing::warn!(
                target: "agentlibre::daemon",
                validation_error = %error,
                "daemon response was replaced because it exceeded protocol bounds"
            );
            DaemonEvent::new(
                event.request_id,
                DaemonEventKind::Error(ProtocolError::new(
                    ProtocolErrorCode::Internal,
                    "daemon response exceeded its bounded wire representation",
                    false,
                )),
            )
        }
        Err(error) => {
            return Err(anyhow::anyhow!(
                "daemon protocol error response is not wire-safe: {error}"
            ));
        }
    };
    sender.events.try_send(event).map_err(|error| {
        // A full bounded queue means the peer is no longer consuming events at
        // the rate required by the live protocol. Close every clone of this
        // socket so the client observes EOF and can resume from its last
        // delivered durable cursor instead of waiting forever on an attachment
        // whose producer has already stopped.
        let _ = sender.shutdown.send(true);
        match error {
            mpsc::error::TrySendError::Full(_) => {
                anyhow::anyhow!("daemon connection writer queue is full; slow peer disconnected")
            }
            mpsc::error::TrySendError::Closed(_) => {
                anyhow::anyhow!("daemon connection writer is disconnected")
            }
        }
    })
}

#[cfg(unix)]
async fn run_connection_writer<W>(
    mut writer: W,
    mut events: mpsc::Receiver<DaemonEvent>,
    shutdown: watch::Sender<bool>,
) where
    W: AsyncWrite + Unpin,
{
    while let Some(event) = events.recv().await {
        let line = match serde_json::to_vec(&event) {
            Ok(line) if line.len() <= agl_protocol::MAX_JSONL_FRAME_BYTES => line,
            Ok(_) => break,
            Err(error) => {
                tracing::debug!(target: "agentlibre::daemon", error = %error, "daemon event serialization failed");
                break;
            }
        };
        if let Err(error) = writer.write_all(&line).await {
            tracing::debug!(
                target: "agentlibre::daemon",
                error = %error,
                "daemon connection writer stopped"
            );
            break;
        }
        if writer.write_all(b"\n").await.is_err() || writer.flush().await.is_err() {
            break;
        }
    }
    let _ = shutdown.send(true);
}

#[cfg(test)]
mod cron_authority_tests {
    use super::*;

    #[test]
    fn builtin_store_status_executes_without_a_durable_agent_run() {
        let root = std::env::temp_dir().join(format!(
            "agentlibre-agl174-cron-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let store = AglStore::open_at(&root).unwrap();
        let job = CronJob {
            id: "job-agl174".to_owned(),
            name: "Store status".to_owned(),
            enabled: true,
            target_kind: CronTargetKind::Builtin,
            target_ref: STORE_STATUS_BUILTIN_CRON_TARGET.to_owned(),
            schedule_expr: "* * * * *".to_owned(),
            timezone: "UTC".to_owned(),
            notify_ref: None,
            prompt: None,
            input: None,
            created_at: "2026-08-14T00:00:00Z".to_owned(),
            updated_at: "2026-08-14T00:00:00Z".to_owned(),
            deleted_at: None,
        };

        let execution = execute_builtin_cron_target(&job, &store).unwrap();
        assert_eq!(execution.status, CronRunStatus::Succeeded);
        let durable_runs: u64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
            .unwrap();
        let durable_steps: u64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM run_steps", [], |row| row.get(0))
            .unwrap();
        assert_eq!((durable_runs, durable_steps), (0, 0));
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(unix)]
use std::os::unix::fs::{
    DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, PermissionsExt as _,
};

#[cfg(all(test, unix))]
mod socket_bind_security_tests {
    use super::*;

    fn private_test_root(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agl-daemon-{label}-{}-{}",
            std::process::id(),
            agl_ids::RequestId::generate()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        root.canonicalize().unwrap()
    }

    #[test]
    fn manual_bind_parent_is_created_owned_and_mode_0700() {
        let root = private_test_root("socket-parent-created");
        let parent = root.join("daemon");

        ensure_private_socket_parent(&parent, false).unwrap();

        let metadata = std::fs::symlink_metadata(&parent).unwrap();
        assert!(metadata.is_dir());
        assert!(!metadata.file_type().is_symlink());
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.mode() & 0o777, 0o700);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manual_bind_rejects_public_symlinked_and_non_directory_parents() {
        let root = private_test_root("socket-parent-rejected");
        let public = root.join("public");
        std::fs::create_dir(&public).unwrap();
        std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_socket_parent(&public, false).is_err());

        let private = root.join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&private, &alias).unwrap();
        assert!(ensure_private_socket_parent(&alias, true).is_err());

        let file = root.join("file");
        std::fs::write(&file, b"not a directory").unwrap();
        assert!(ensure_private_socket_parent(&file, true).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn app_owned_bind_tightens_existing_owned_canonical_parent_to_0700() {
        let root = private_test_root("socket-parent-tightened");
        let parent = root.join("daemon");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        ensure_private_socket_parent(&parent, true).unwrap();

        let metadata = std::fs::symlink_metadata(&parent).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o700);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manual_bind_never_removes_a_non_socket_stale_target() {
        let root = private_test_root("socket-target-rejected");
        let parent = root.join("daemon");
        ensure_private_socket_parent(&parent, false).unwrap();
        let target = parent.join("agl.sock");
        std::fs::write(&target, b"operator data").unwrap();

        let error = match bind_listener(&target, false) {
            Ok(_) => panic!("regular target must fail"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("owned Unix socket"));
        assert_eq!(std::fs::read(&target).unwrap(), b"operator data");
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(all(test, unix))]
mod connection_writer_tests {
    use super::*;

    fn test_event() -> DaemonEvent {
        DaemonEvent::new(
            None,
            DaemonEventKind::SessionList(agl_protocol::SessionListEvent {
                sessions: Vec::new(),
            }),
        )
    }

    #[tokio::test]
    async fn full_bounded_writer_queue_disconnects_slow_peer() {
        let (events, _receiver) = mpsc::channel(1);
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let sender = ConnectionEventSender { events, shutdown };

        queue_event(&sender, test_event()).unwrap();
        let error = queue_event(&sender, test_event()).unwrap_err();

        assert!(error.to_string().contains("slow peer disconnected"));
        assert!(*shutdown_receiver.borrow());
    }

    #[tokio::test]
    async fn oversized_response_becomes_typed_error_without_closing_connection() {
        let (events, mut receiver) = mpsc::channel(2);
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let sender = ConnectionEventSender { events, shutdown };
        let request_id = agl_ids::RequestId::generate();
        let summaries = (0..2_000)
            .map(|_| agl_protocol::SessionSummary {
                session_id: agl_ids::SessionId::generate(),
                title: Some("x".repeat(1_024)),
                status: agl_protocol::SessionStatus::Open,
                updated_at_unix_ms: 0,
            })
            .collect();

        queue_event(
            &sender,
            DaemonEvent::new(
                Some(request_id.clone()),
                DaemonEventKind::SessionList(agl_protocol::SessionListEvent {
                    sessions: summaries,
                }),
            ),
        )
        .unwrap();
        queue_event(&sender, test_event()).unwrap();

        let oversized = receiver.recv().await.unwrap();
        assert_eq!(oversized.request_id.as_ref(), Some(&request_id));
        let DaemonEventKind::Error(error) = oversized.kind else {
            panic!("expected typed protocol error");
        };
        assert_eq!(error.code, ProtocolErrorCode::Internal);
        assert!(!error.retryable);
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            DaemonEventKind::SessionList(_)
        ));
        assert!(!*shutdown_receiver.borrow());
    }

    #[tokio::test]
    async fn bounded_connection_tasks_reject_excess_work_without_spawning_it() {
        let mut tasks = JoinSet::new();
        for _ in 0..CONNECTION_TASK_CAPACITY {
            tasks.spawn(std::future::pending::<()>());
        }
        let (events, mut receiver) = mpsc::channel(1);
        let (shutdown, _shutdown_receiver) = watch::channel(false);
        let sender = ConnectionEventSender { events, shutdown };
        let request_id = agl_ids::RequestId::generate();

        assert!(reject_connection_task_overflow(&tasks, &sender, &request_id).unwrap());
        let event = receiver.recv().await.unwrap();
        assert_eq!(event.request_id.as_ref(), Some(&request_id));
        let DaemonEventKind::Error(error) = event.kind else {
            panic!("expected bounded task rejection");
        };
        assert_eq!(error.code, ProtocolErrorCode::InputBackpressure);
        assert!(error.retryable);
        assert_eq!(tasks.len(), CONNECTION_TASK_CAPACITY);
        tasks.abort_all();
    }

    #[tokio::test]
    async fn subscriptions_are_reserved_before_spawn_and_bounded_per_connection() {
        let subscriptions = ConnectionSubscriptions::default();
        let first = agl_ids::RequestId::generate();
        subscriptions.reserve(&first).unwrap();

        let duplicate = subscriptions.reserve(&first).unwrap_err();
        assert_eq!(duplicate.code, ProtocolErrorCode::InvalidRequest);

        for _ in 1..CONNECTION_SUBSCRIPTION_CAPACITY {
            subscriptions
                .reserve(&agl_ids::RequestId::generate())
                .unwrap();
        }
        let overflow = subscriptions
            .reserve(&agl_ids::RequestId::generate())
            .unwrap_err();
        assert_eq!(overflow.code, ProtocolErrorCode::InputBackpressure);
        assert!(overflow.retryable);

        assert!(subscriptions.cancel(&first));
        assert!(!subscriptions.cancel(&first));

        let cancellable = agl_ids::RequestId::generate();
        subscriptions.reserve(&cancellable).unwrap();
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut tasks = JoinSet::new();
        let handle = tasks.spawn(std::future::pending::<()>());
        subscriptions.install(&cancellable, handle, Some(Arc::clone(&cancellation)));
        assert!(subscriptions.cancel(&cancellable));
        assert!(cancellation.load(Ordering::Acquire));
        assert!(tasks.join_next().await.unwrap().unwrap_err().is_cancelled());
    }
}
