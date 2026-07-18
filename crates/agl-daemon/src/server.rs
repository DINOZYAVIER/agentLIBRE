use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agl_chat::InferenceClientHandle;
use agl_cron::{CronJob, CronRepository, CronRunStatus};
use agl_inference::{LlamaCppModelRuntime, ModelManager, ModelManagerOptions};
use agl_process::{
    ExecutionCursor, ExecutionState, InputLease, ProcessErrorCode, TerminalSize,
    WRITABLE_INPUT_LEASE_HEARTBEAT, WRITABLE_INPUT_LEASE_TTL,
};
use agl_protocol::{
    DaemonEvent, DaemonEventKind, DaemonRequest, DaemonRequestKind,
    ExecutionAttachmentFinishReason, ExecutionAttachmentFinishedEvent,
    ExecutionAttachmentStartedEvent, ExecutionDetachAcceptedEvent, ExecutionInputAcceptedEvent,
    ExecutionLeaseRenewedEvent, ExecutionOutputEvent, ExecutionResizeAcceptedEvent,
    PresentationSubscriptionFinishReason, ProtocolError, ProtocolErrorCode,
    RunSubscriptionFinishedEvent, RunSubscriptionStartedEvent,
    SessionPresentationSubscriptionFinishedEvent, SessionPresentationSubscriptionStartedEvent,
    SubscriptionCancelledEvent,
};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_store::{AglStore, MatrixNotificationOutboxDraft, RunState};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt as _;
use tokio::io::{AsyncWriteExt as _, WriteHalf};
use tokio::net::{UnixListener as TokioUnixListener, UnixStream as TokioUnixStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::codec::{FramedRead, LinesCodec};

use crate::state::{process_error, protocol_run_state};
use crate::{
    CronExecution, CronNotification, CronNotifier, CronTargetExecutor, DaemonOptions,
    ListenerSource, SharedDaemonState, render_cron_notification_body, run_cron_tick,
};

const CONNECTION_WRITER_CAPACITY: usize = 128;
const EXECUTION_ATTACH_POLL_INTERVAL: Duration = Duration::from_millis(20);

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
            ListenerSource::Bind(path) => bind_listener(path)?,
            ListenerSource::Systemd => crate::activation::claim_systemd_listener()?,
        };
        let store = AglStore::open_at(self.runtime.paths.store_root())
            .context("failed to open daemon cron store")?;
        let model_manager = ModelManager::spawn(
            ModelManagerOptions::default()
                .with_model_lease_root(self.runtime.paths.model_lease_root()),
            LlamaCppModelRuntime::new(),
        )
        .context("failed to start daemon model manager")?;
        let inference_client = InferenceClientHandle::from(model_manager.handle());
        tracing::info!(
            target: "agentlibre::daemon",
            listener = %self.options.listener_source,
            "daemon listening"
        );
        let state = SharedDaemonState::open(
            self.runtime.clone(),
            self.options.inference.clone(),
            inference_client.clone(),
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
                    let mut executor = DaemonCronExecutor { state: state.clone() };
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
fn bind_listener(socket_path: &Path) -> Result<TokioUnixListener> {
    let parent = socket_path
        .parent()
        .context("daemon socket path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create daemon socket dir {}", parent.display()))?;

    if socket_path.exists() {
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
    TokioUnixListener::from_std(listener).context("failed to adopt daemon socket into Tokio")
}

#[cfg(unix)]
#[doc(hidden)]
pub async fn serve_connection(stream: TokioUnixStream, state: &SharedDaemonState) -> Result<()> {
    let credentials = stream
        .peer_cred()
        .context("failed to read daemon peer credentials")?;
    let expected_uid = unsafe { libc::geteuid() };
    if credentials.uid() != expected_uid {
        bail!("private daemon connection peer UID does not match daemon UID");
    }
    let (reader, writer) = tokio::io::split(stream);
    let (raw_event_sender, event_receiver) = mpsc::channel(CONNECTION_WRITER_CAPACITY);
    let (shutdown, mut shutdown_receiver) = watch::channel(false);
    let event_sender = ConnectionEventSender {
        events: raw_event_sender,
        shutdown: shutdown.clone(),
    };
    let writer_task = tokio::spawn(run_connection_writer(writer, event_receiver, shutdown));
    let attachments = ConnectionAttachments::default();
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
                    let state = state.clone();
                    let sender = event_sender.clone();
                    let subscription_id = request_id.clone();
                    let registry_id = request_id.clone();
                    let subscriptions_for_task = subscriptions.clone();
                    let application = state.application();
                    let refresh_state = state.clone();
                    let refresh_run_id = request.run_id.clone();
                    let handle = tasks.spawn(async move {
                        tokio::task::spawn_blocking(move || {
                            if let Err(error) =
                                stream_run_subscription(&sender, &state, request_id, request)
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
                    subscriptions.insert(registry_id, handle)?;
                }
                Ok(DaemonRequest {
                    request_id,
                    kind: DaemonRequestKind::SessionPresentationSubscribe(request),
                    ..
                }) => {
                    let application = state.application();
                    let sender = event_sender.clone();
                    let subscription_id = request_id.clone();
                    let registry_id = request_id.clone();
                    let subscriptions_for_task = subscriptions.clone();
                    let handle = tasks.spawn(async move {
                        if let Err(error) = stream_presentation_subscription(
                            &sender,
                            &application,
                            request_id,
                            request,
                        ).await {
                            tracing::debug!(target: "agentlibre::daemon", error = %error, "presentation subscription ended");
                        }
                        subscriptions_for_task.remove(&subscription_id);
                    });
                    subscriptions.insert(registry_id, handle)?;
                }
                Ok(DaemonRequest {
                    schema,
                    request_id,
                    kind: DaemonRequestKind::ExecutionAttach(request),
                }) => {
                    let _ = schema;
                    let state = state.clone();
                    let sender = event_sender.clone();
                    let attachments = attachments.clone();
                    tasks.spawn(async move {
                        tokio::task::spawn_blocking(move || {
                            if let Err(error) = stream_execution_attachment(
                                &sender,
                                &state,
                                &attachments,
                                request_id,
                                request,
                            ) {
                                tracing::debug!(
                                    target: "agentlibre::daemon",
                                    error = %error,
                                    "daemon execution attachment ended"
                                );
                            }
                        }).await.ok();
                    });
                }
                Ok(DaemonRequest {
                    request_id,
                    kind: DaemonRequestKind::ExecutionLeaseRenew(request),
                    ..
                }) => queue_event(
                    &event_sender,
                    execution_lease_renew_event(state, &attachments, request_id, request),
                )?,
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
                    kind: kind @ (DaemonRequestKind::CommandCatalog(_)
                    | DaemonRequestKind::CommandSuggestions(_)
                    | DaemonRequestKind::ApplicationAction(_)
                    | DaemonRequestKind::SessionPresentation(_)
                    | DaemonRequestKind::UserShellStart(_)),
                    ..
                }) => {
                    let application = state.application();
                    let sender = event_sender.clone();
                    tasks.spawn(async move {
                        let event = crate::surface::handle_finite_request(
                            &application,
                            request_id,
                            kind,
                            expected_uid,
                        ).await;
                        let _ = queue_event(&sender, event);
                    });
                }
                Ok(DaemonRequest {
                    request_id,
                    kind: DaemonRequestKind::ExecutionInput(request),
                    ..
                }) => queue_event(
                    &event_sender,
                    execution_input_event(state, &attachments, request_id, request),
                )?,
                Ok(DaemonRequest {
                    request_id,
                    kind: DaemonRequestKind::ExecutionResize(request),
                    ..
                }) => queue_event(
                    &event_sender,
                    execution_resize_event(state, &attachments, request_id, request),
                )?,
                Ok(DaemonRequest {
                    request_id,
                    kind: DaemonRequestKind::ExecutionDetach(request),
                    ..
                }) => queue_event(
                    &event_sender,
                    execution_detach_event(state, &attachments, request_id, request),
                )?,
                Ok(request) => {
                    let state = state.clone();
                    let sender = event_sender.clone();
                    tasks.spawn(async move {
                        let event = tokio::task::spawn_blocking(move || state.handle_request(request)).await;
                        if let Ok(event) = event {
                            let _ = queue_event(&sender, event);
                        }
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
    tasks.abort_all();
    attachments.release_all(state);
    drop(event_sender);
    let _ = writer_task.await;
    result
}

#[cfg(unix)]
#[derive(Clone, Default)]
struct ConnectionAttachments {
    inner: Arc<Mutex<BTreeMap<agl_ids::RequestId, ConnectionAttachment>>>,
}

#[cfg(unix)]
#[derive(Clone)]
struct ConnectionAttachment {
    execution_id: agl_ids::ExecutionId,
    lease: InputLease,
    cursor: u64,
}

#[cfg(unix)]
impl ConnectionAttachments {
    fn get(
        &self,
        attachment_id: &agl_ids::RequestId,
    ) -> std::result::Result<ConnectionAttachment, ProtocolError> {
        self.inner
            .lock()
            .map_err(attachment_lock_error)?
            .get(attachment_id)
            .cloned()
            .ok_or_else(|| attachment_not_found(attachment_id))
    }

    fn insert(
        &self,
        attachment_id: agl_ids::RequestId,
        attachment: ConnectionAttachment,
    ) -> std::result::Result<(), ProtocolError> {
        let replaced = self
            .inner
            .lock()
            .map_err(attachment_lock_error)?
            .insert(attachment_id, attachment);
        if replaced.is_some() {
            return Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "duplicate execution attachment request ID",
                false,
            ));
        }
        Ok(())
    }

    fn remove(
        &self,
        attachment_id: &agl_ids::RequestId,
    ) -> std::result::Result<Option<ConnectionAttachment>, ProtocolError> {
        Ok(self
            .inner
            .lock()
            .map_err(attachment_lock_error)?
            .remove(attachment_id))
    }

    fn release_all(&self, state: &SharedDaemonState) {
        let attachments = match self.inner.lock() {
            Ok(mut attachments) => std::mem::take(&mut *attachments),
            Err(_) => return,
        };
        let Ok(process) = state.process_handle() else {
            return;
        };
        for attachment in attachments.into_values() {
            let _ = process.operator_detach(&attachment.execution_id, attachment.lease);
        }
    }
}

#[cfg(unix)]
#[derive(Clone, Default)]
struct ConnectionSubscriptions {
    inner: Arc<Mutex<BTreeMap<agl_ids::RequestId, tokio::task::AbortHandle>>>,
}

#[cfg(unix)]
impl ConnectionSubscriptions {
    fn insert(
        &self,
        request_id: agl_ids::RequestId,
        handle: tokio::task::AbortHandle,
    ) -> Result<()> {
        let mut subscriptions = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("connection subscription registry is poisoned"))?;
        if subscriptions.insert(request_id, handle).is_some() {
            bail!("duplicate subscription request ID on one connection");
        }
        Ok(())
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
        handle.abort();
        true
    }
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
    let snapshot = crate::surface::presentation_snapshot(subscription.snapshot.clone())
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let mut last_cursor = snapshot.cursor.clone();
    queue_event(
        sender,
        DaemonEvent::new(
            Some(request_id.clone()),
            DaemonEventKind::SessionPresentationSubscriptionStarted(
                SessionPresentationSubscriptionStartedEvent { snapshot },
            ),
        ),
    )?;
    loop {
        match subscription.next().await {
            Ok(event) => {
                let event = crate::surface::presentation_event(event)
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                last_cursor = event.cursor.clone();
                queue_event(
                    sender,
                    DaemonEvent::new(
                        Some(request_id.clone()),
                        DaemonEventKind::SessionPresentationEvent(Box::new(event)),
                    ),
                )?;
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
fn stream_execution_attachment(
    sender: &ConnectionEventSender,
    state: &SharedDaemonState,
    attachments: &ConnectionAttachments,
    attachment_id: agl_ids::RequestId,
    request: agl_protocol::ExecutionAttachRequest,
) -> Result<()> {
    let process = state.process_handle()?;
    let maximum_bytes = state.process_read_limit()?;
    let lease = match process.operator_attach(
        &request.execution_id,
        attachment_id.clone(),
        request.writable,
    ) {
        Ok(lease) => lease,
        Err(error) => {
            return queue_event(
                sender,
                DaemonEvent::new(
                    Some(attachment_id),
                    DaemonEventKind::Error(process_error(error)),
                ),
            );
        }
    };
    let status = match process.operator_status(&request.execution_id) {
        Ok(status) => status,
        Err(error) => {
            let _ = process.operator_detach(&request.execution_id, lease);
            return queue_event(
                sender,
                DaemonEvent::new(
                    Some(attachment_id),
                    DaemonEventKind::Error(process_error(error)),
                ),
            );
        }
    };
    let attachment = ConnectionAttachment {
        execution_id: request.execution_id.clone(),
        lease: lease.clone(),
        cursor: request.after_sequence,
    };
    if let Err(error) = attachments.insert(attachment_id.clone(), attachment) {
        let _ = process.operator_detach(&request.execution_id, lease);
        return queue_event(
            sender,
            DaemonEvent::new(Some(attachment_id), DaemonEventKind::Error(error)),
        );
    }
    if let Err(error) = queue_event(
        sender,
        DaemonEvent::new(
            Some(attachment_id.clone()),
            DaemonEventKind::ExecutionAttachmentStarted(ExecutionAttachmentStartedEvent {
                attachment_id: attachment_id.clone(),
                status,
                writable: request.writable,
                next_sequence: request.after_sequence,
                lease_ttl_ms: request
                    .writable
                    .then_some(WRITABLE_INPUT_LEASE_TTL.as_millis() as u64),
                heartbeat_interval_ms: request
                    .writable
                    .then_some(WRITABLE_INPUT_LEASE_HEARTBEAT.as_millis() as u64),
            }),
        ),
    ) {
        release_attachment(attachments, &process, &attachment_id);
        return Err(error);
    }

    let mut last_cursor = request.after_sequence;
    loop {
        let attachment = match attachments.get(&attachment_id) {
            Ok(attachment) => attachment,
            Err(error) if error.code == ProtocolErrorCode::NotFound => {
                let state = process
                    .operator_status(&request.execution_id)
                    .map_or(ExecutionState::OutcomeUnknown, |status| status.state);
                return queue_event(
                    sender,
                    DaemonEvent::new(
                        Some(attachment_id.clone()),
                        DaemonEventKind::ExecutionAttachmentFinished(
                            ExecutionAttachmentFinishedEvent {
                                attachment_id,
                                execution_id: request.execution_id,
                                state,
                                last_delivered_sequence: last_cursor,
                                reason: ExecutionAttachmentFinishReason::Detached,
                            },
                        ),
                    ),
                );
            }
            Err(error) => {
                release_attachment(attachments, &process, &attachment_id);
                return queue_event(
                    sender,
                    DaemonEvent::new(Some(attachment_id), DaemonEventKind::Error(error)),
                );
            }
        };
        if attachment.lease.writable {
            match process
                .operator_input_lease_active(&attachment.execution_id, attachment.lease.clone())
            {
                Ok(true) => {}
                Ok(false) => {
                    let _ = attachments.remove(&attachment_id);
                    let state = process
                        .operator_status(&request.execution_id)
                        .map_or(ExecutionState::OutcomeUnknown, |status| status.state);
                    return queue_event(
                        sender,
                        DaemonEvent::new(
                            Some(attachment_id.clone()),
                            DaemonEventKind::ExecutionAttachmentFinished(
                                ExecutionAttachmentFinishedEvent {
                                    attachment_id,
                                    execution_id: request.execution_id,
                                    state,
                                    last_delivered_sequence: last_cursor,
                                    reason: ExecutionAttachmentFinishReason::InputLeaseExpired,
                                },
                            ),
                        ),
                    );
                }
                Err(error) if error.code() == ProcessErrorCode::ExecutionNotLive => {}
                Err(error) => {
                    release_attachment(attachments, &process, &attachment_id);
                    return queue_event(
                        sender,
                        DaemonEvent::new(
                            Some(attachment_id),
                            DaemonEventKind::Error(process_error(error)),
                        ),
                    );
                }
            }
        }
        let output = match process.operator_read(
            &attachment.execution_id,
            ExecutionCursor {
                after_sequence: attachment.cursor,
            },
            maximum_bytes,
        ) {
            Ok(output) => output,
            Err(error) => {
                release_attachment(attachments, &process, &attachment_id);
                let mut error = process_error(error);
                error.safe_metadata.insert(
                    "last_delivered_sequence".to_string(),
                    attachment.cursor.to_string(),
                );
                return queue_event(
                    sender,
                    DaemonEvent::new(Some(attachment_id), DaemonEventKind::Error(error)),
                );
            }
        };

        let mut detached = false;
        for chunk in &output.chunks {
            let mut active = attachments
                .inner
                .lock()
                .map_err(|error| anyhow::anyhow!(attachment_lock_error(error).message))?;
            let Some(active_attachment) = active.get_mut(&attachment_id) else {
                detached = true;
                break;
            };
            if let Err(error) = queue_event(
                sender,
                DaemonEvent::new(
                    Some(attachment_id.clone()),
                    DaemonEventKind::ExecutionOutput(ExecutionOutputEvent {
                        attachment_id: attachment_id.clone(),
                        execution_id: request.execution_id.clone(),
                        chunk: chunk.clone(),
                        state: output.state,
                    }),
                ),
            ) {
                drop(active);
                release_attachment(attachments, &process, &attachment_id);
                return Err(error);
            }
            active_attachment.cursor = chunk.sequence;
            last_cursor = chunk.sequence;
        }
        if detached {
            continue;
        }

        let cursor = {
            let mut active = attachments
                .inner
                .lock()
                .map_err(|error| anyhow::anyhow!(attachment_lock_error(error).message))?;
            let Some(active_attachment) = active.get_mut(&attachment_id) else {
                continue;
            };
            active_attachment.cursor = output.next_sequence;
            active_attachment.cursor
        };
        last_cursor = cursor;
        if output.state.is_terminal() && output.chunks.is_empty() {
            release_attachment(attachments, &process, &attachment_id);
            return queue_event(
                sender,
                DaemonEvent::new(
                    Some(attachment_id.clone()),
                    DaemonEventKind::ExecutionAttachmentFinished(
                        ExecutionAttachmentFinishedEvent {
                            attachment_id,
                            execution_id: request.execution_id,
                            state: output.state,
                            last_delivered_sequence: cursor,
                            reason: ExecutionAttachmentFinishReason::TargetTerminal,
                        },
                    ),
                ),
            );
        }
        if output.chunks.is_empty() {
            thread::sleep(EXECUTION_ATTACH_POLL_INTERVAL);
        }
    }
}

#[cfg(unix)]
fn execution_lease_renew_event(
    state: &SharedDaemonState,
    attachments: &ConnectionAttachments,
    request_id: agl_ids::RequestId,
    request: agl_protocol::ExecutionLeaseRenewRequest,
) -> DaemonEvent {
    let result = (|| -> std::result::Result<DaemonEventKind, ProtocolError> {
        let attachment = attachments.get(&request.attachment_id)?;
        state
            .process_handle()
            .map_err(daemon_runtime_error)?
            .operator_renew_input_lease(&attachment.execution_id, attachment.lease)
            .map_err(process_error)?;
        Ok(DaemonEventKind::ExecutionLeaseRenewed(
            ExecutionLeaseRenewedEvent {
                attachment_id: request.attachment_id,
                lease_ttl_ms: WRITABLE_INPUT_LEASE_TTL.as_millis() as u64,
            },
        ))
    })();
    DaemonEvent::new(
        Some(request_id),
        result.unwrap_or_else(DaemonEventKind::Error),
    )
}

#[cfg(unix)]
fn execution_input_event(
    state: &SharedDaemonState,
    attachments: &ConnectionAttachments,
    request_id: agl_ids::RequestId,
    request: agl_protocol::ExecutionInputRequest,
) -> DaemonEvent {
    let result = (|| -> std::result::Result<DaemonEventKind, ProtocolError> {
        let attachment = attachments.get(&request.attachment_id)?;
        request
            .bytes
            .decode(state.process_input_limit().map_err(daemon_runtime_error)?)
            .map_err(process_error)?;
        state
            .process_handle()
            .map_err(daemon_runtime_error)?
            .operator_write(
                &attachment.execution_id,
                attachment.lease,
                request.bytes,
                request.eof,
            )
            .map_err(process_error)?;
        Ok(DaemonEventKind::ExecutionInputAccepted(
            ExecutionInputAcceptedEvent {
                attachment_id: request.attachment_id,
                eof: request.eof,
            },
        ))
    })();
    DaemonEvent::new(
        Some(request_id),
        result.unwrap_or_else(DaemonEventKind::Error),
    )
}

#[cfg(unix)]
fn execution_resize_event(
    state: &SharedDaemonState,
    attachments: &ConnectionAttachments,
    request_id: agl_ids::RequestId,
    request: agl_protocol::ExecutionResizeRequest,
) -> DaemonEvent {
    let result = (|| -> std::result::Result<DaemonEventKind, ProtocolError> {
        let attachment = attachments.get(&request.attachment_id)?;
        let terminal_size = TerminalSize {
            columns: request.columns,
            rows: request.rows,
        }
        .validate()
        .map_err(process_error)?;
        state
            .process_handle()
            .map_err(daemon_runtime_error)?
            .operator_resize(&attachment.execution_id, terminal_size)
            .map_err(process_error)?;
        Ok(DaemonEventKind::ExecutionResizeAccepted(
            ExecutionResizeAcceptedEvent {
                attachment_id: request.attachment_id,
                columns: request.columns,
                rows: request.rows,
            },
        ))
    })();
    DaemonEvent::new(
        Some(request_id),
        result.unwrap_or_else(DaemonEventKind::Error),
    )
}

#[cfg(unix)]
fn execution_detach_event(
    state: &SharedDaemonState,
    attachments: &ConnectionAttachments,
    request_id: agl_ids::RequestId,
    request: agl_protocol::ExecutionDetachRequest,
) -> DaemonEvent {
    let result = (|| -> std::result::Result<DaemonEventKind, ProtocolError> {
        let process = state.process_handle().map_err(daemon_runtime_error)?;
        let mut active = attachments.inner.lock().map_err(attachment_lock_error)?;
        let attachment = active
            .get(&request.attachment_id)
            .cloned()
            .ok_or_else(|| attachment_not_found(&request.attachment_id))?;
        match process.operator_detach(&attachment.execution_id, attachment.lease) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.code(),
                    ProcessErrorCode::ExecutionNotLive | ProcessErrorCode::InputLeaseExpired
                ) => {}
            Err(error) => return Err(process_error(error)),
        }
        active.remove(&request.attachment_id);
        Ok(DaemonEventKind::ExecutionDetachAccepted(
            ExecutionDetachAcceptedEvent {
                attachment_id: request.attachment_id,
            },
        ))
    })();
    DaemonEvent::new(
        Some(request_id),
        result.unwrap_or_else(DaemonEventKind::Error),
    )
}

#[cfg(unix)]
fn release_attachment(
    attachments: &ConnectionAttachments,
    process: &agl_process::ProcessHandle,
    attachment_id: &agl_ids::RequestId,
) {
    let Ok(Some(attachment)) = attachments.remove(attachment_id) else {
        return;
    };
    let _ = process.operator_detach(&attachment.execution_id, attachment.lease);
}

#[cfg(unix)]
fn attachment_not_found(attachment_id: &agl_ids::RequestId) -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::NotFound,
        format!("execution attachment {attachment_id} not found on this connection"),
        false,
    )
}

#[cfg(unix)]
fn attachment_lock_error<T>(error: std::sync::PoisonError<T>) -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::RuntimeFailure,
        format!("execution attachment registry is poisoned: {error}"),
        false,
    )
}

#[cfg(unix)]
fn daemon_runtime_error(error: anyhow::Error) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::RuntimeFailure, error.to_string(), false)
}

#[cfg(unix)]
fn stream_run_subscription(
    sender: &ConnectionEventSender,
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
#[derive(Clone)]
struct ConnectionEventSender {
    events: mpsc::Sender<DaemonEvent>,
    shutdown: watch::Sender<bool>,
}

#[cfg(unix)]
fn queue_event(sender: &ConnectionEventSender, event: DaemonEvent) -> Result<()> {
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
async fn run_connection_writer(
    mut writer: WriteHalf<TokioUnixStream>,
    mut events: mpsc::Receiver<DaemonEvent>,
    shutdown: watch::Sender<bool>,
) {
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

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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
}
