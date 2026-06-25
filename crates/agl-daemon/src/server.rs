use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use agl_protocol::{DaemonEvent, DaemonEventKind, DaemonRequest, ProtocolError, ProtocolErrorCode};
use agl_runtime::AgentLibreRuntimeConfig;
use anyhow::{Context, Result, bail};

use crate::{DaemonOptions, DaemonState};

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
        tracing::info!(
            target: "agentlibre::daemon",
            socket_path = %self.options.socket_path.display(),
            "daemon listening"
        );
        let mut state = DaemonState::new(self.runtime, self.options.inference);
        for incoming in listener.incoming() {
            let stream = incoming.context("failed to accept daemon client")?;
            if let Err(err) = handle_stream(stream, &mut state) {
                tracing::warn!(target: "agentlibre::daemon", error = %err, "daemon client failed");
            }
        }
        Ok(())
    }

    #[cfg(not(unix))]
    pub fn run_foreground(self) -> Result<()> {
        bail!("agl daemon is only available on Unix platforms in this alpha")
    }
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
