use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agl_cron::{CronJob, CronTargetKind};
use agl_protocol::{DaemonEvent, DaemonEventKind, DaemonRequest, ProtocolError, ProtocolErrorCode};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_store::AglStore;
use anyhow::{Context, Result, bail};

use crate::{
    CronExecution, CronNotification, CronNotifier, CronTargetExecutor, DaemonOptions, DaemonState,
    run_cron_tick,
};

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
        let store = AglStore::open_default(&self.runtime.paths)
            .context("failed to open daemon cron store")?;
        tracing::info!(
            target: "agentlibre::daemon",
            socket_path = %self.options.socket_path.display(),
            "daemon listening"
        );
        let mut state = DaemonState::new(self.runtime, self.options.inference);
        let mut last_cron_tick = None;
        loop {
            let now = unix_now();
            if last_cron_tick
                .is_none_or(|last| now.saturating_sub(last) >= self.options.cron_interval_seconds)
            {
                last_cron_tick = Some(now);
                let mut executor = DaemonCronExecutor;
                let mut notifier = TracingCronNotifier;
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
            }

            match listener.accept() {
                Ok((stream, _addr)) => {
                    if let Err(err) = handle_stream(stream, &mut state) {
                        tracing::warn!(target: "agentlibre::daemon", error = %err, "daemon client failed");
                    }
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

struct DaemonCronExecutor;

impl CronTargetExecutor for DaemonCronExecutor {
    fn execute(&mut self, job: &CronJob) -> CronExecution {
        match (job.target_kind, job.target_ref.as_str()) {
            (CronTargetKind::Builtin, "store-status") => {
                CronExecution::succeeded("builtin:store-status")
            }
            (CronTargetKind::Builtin, target) => {
                CronExecution::failed(format!("unknown builtin cron target: {target}"))
            }
            (CronTargetKind::Skill, target) => CronExecution::failed(format!(
                "daemon cron skill execution is not enabled yet: {target}"
            )),
        }
    }
}

struct TracingCronNotifier;

impl CronNotifier for TracingCronNotifier {
    fn notify(&mut self, notification: CronNotification) -> Result<()> {
        if notification.notify_ref.starts_with("matrix-room:") {
            tracing::info!(
                target: "agentlibre::daemon",
                notify_ref = %notification.notify_ref,
                job_id = %notification.job_id,
                job_name = %notification.job_name,
                status = notification.status.as_str(),
                scheduled_for = %notification.scheduled_for,
                result_ref = notification.result_ref.as_deref(),
                error = notification.error.as_deref(),
                "cron Matrix notification queued at daemon boundary"
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
fn handle_stream(stream: UnixStream, state: &mut DaemonState) -> Result<()> {
    let mut writer = stream
        .try_clone()
        .context("failed to clone daemon client stream")?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.context("failed to read daemon request")?;
        if line.trim().is_empty() {
            continue;
        }
        let events = match serde_json::from_str::<DaemonRequest>(&line) {
            Ok(request) => state.handle_request(request),
            Err(err) => vec![DaemonEvent::new(
                None,
                DaemonEventKind::Error(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    format!("invalid daemon request JSON: {err}"),
                    false,
                )),
            )],
        };
        for event in events {
            write_event(&mut writer, &event)?;
        }
    }
    Ok(())
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
