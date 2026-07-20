use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use agl_app::{
    ApplicationCallContext, ApplicationService, SessionHeader, SessionPresentationEvent, Severity,
    TerminalSessionView,
};
use agl_ids::{SessionId, TerminalSessionId};
use agl_process::{
    HumanShellHistoryStore, ProcessErrorCode, ShellExit, ShellIntegrationEvent,
    ShellIntegrationNotice, TerminalRegistry,
};

use crate::state::{DaemonStateCallError, DaemonStateExecutor};

const CWD_RETRY_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Default)]
pub(crate) struct ShellMonitorConnector {
    connection: Arc<OnceLock<ShellMonitorConnection>>,
}

#[derive(Clone)]
struct ShellMonitorConnection {
    state: Weak<DaemonStateExecutor>,
    application: ApplicationService,
}

pub(crate) struct ShellMonitorSpec {
    pub terminal_id: TerminalSessionId,
    pub session_id: SessionId,
    pub workspace_root: PathBuf,
    pub initial_command_sequence: u64,
    pub registry: Arc<TerminalRegistry>,
    pub history: HumanShellHistoryStore,
    pub maximum_read_bytes: usize,
    pub poll_interval: Duration,
}

pub(crate) struct TerminalMonitorProjection {
    pub terminal: Option<TerminalSessionView>,
    pub header: Option<SessionHeader>,
    pub cwd_consumed: bool,
}

impl ShellMonitorConnector {
    pub fn connect(
        &self,
        state: Weak<DaemonStateExecutor>,
        application: ApplicationService,
    ) -> Result<(), &'static str> {
        self.connection
            .set(ShellMonitorConnection { state, application })
            .map_err(|_| "shell monitor connector is already connected")
    }

    /// Returns false only for direct, unshared DaemonState fixtures. Production
    /// SharedDaemonState connects the monitor owner before application calls.
    pub fn spawn(&self, spec: ShellMonitorSpec) -> Result<bool, std::io::Error> {
        let Some(connection) = self.connection.get().cloned() else {
            return Ok(false);
        };
        std::thread::Builder::new()
            .name("agl-shell-monitor".to_owned())
            .spawn(move || monitor_terminal(connection, spec))?;
        Ok(true)
    }
}

fn monitor_terminal(connection: ShellMonitorConnection, spec: ShellMonitorSpec) {
    let mut tracker = TrustedCommandTracker::new(spec.initial_command_sequence);
    let mut pending_cwd = None;
    let mut next_cwd_attempt = Instant::now();

    loop {
        if connection.state.upgrade().is_none() {
            return;
        }

        let batch = match spec
            .registry
            .poll_private_integration(&spec.terminal_id, spec.maximum_read_bytes)
        {
            Ok(batch) => batch,
            Err(error) if error.code() == ProcessErrorCode::InputBackpressure => {
                std::thread::sleep(spec.poll_interval);
                continue;
            }
            Err(_) => {
                close_integration(&connection, &spec, &mut tracker, None);
                return;
            }
        };

        let mut presentation = Vec::new();
        let mut terminal_changed = false;
        let integration_notice = batch.notice;
        if !batch.events.is_empty() {
            let record = match spec.registry.record(&spec.terminal_id) {
                Ok(record) => record,
                Err(_) => return,
            };
            match tracker.accept(&spec.terminal_id, &batch.events, record.command_sequence) {
                Ok(projected) => {
                    terminal_changed = true;
                    if let Some(cwd) = projected.cwd {
                        pending_cwd = Some(cwd);
                        next_cwd_attempt = Instant::now();
                    }
                    presentation.extend(projected.presentation);
                    for command in projected.completed_commands {
                        if spec.history.append(&spec.workspace_root, &command).is_err() {
                            presentation.push(notice(
                                "shell_history_write_failed",
                                "private Human shell history could not be updated",
                            ));
                        }
                    }
                }
                Err(()) => {
                    close_integration(
                        &connection,
                        &spec,
                        &mut tracker,
                        Some(ShellIntegrationNotice {
                            code: "shell_integration_degraded",
                            message: "trusted command boundary tracking diverged".to_owned(),
                        }),
                    );
                    return;
                }
            }
        }

        let attempt_cwd = pending_cwd.is_some() && Instant::now() >= next_cwd_attempt;
        if terminal_changed || attempt_cwd {
            let requested_cwd = if attempt_cwd {
                pending_cwd.as_ref()
            } else {
                None
            };
            match project_terminal(
                &connection,
                &spec.terminal_id,
                requested_cwd,
                terminal_changed,
            ) {
                Some(projection) => {
                    if attempt_cwd {
                        if projection.cwd_consumed {
                            pending_cwd = None;
                        } else {
                            next_cwd_attempt = Instant::now() + CWD_RETRY_INTERVAL;
                        }
                    }
                    if let Some(terminal) = projection.terminal {
                        presentation.push(SessionPresentationEvent::TerminalChanged { terminal });
                    }
                    if let Some(header) = projection.header {
                        presentation.push(SessionPresentationEvent::HeaderChanged { header });
                    }
                }
                None => return,
            }
        }
        publish_all(&connection.application, &spec.session_id, presentation);

        if integration_notice.is_some() {
            close_integration(&connection, &spec, &mut tracker, integration_notice);
            return;
        }
        if batch.events.is_empty() {
            std::thread::sleep(spec.poll_interval);
        }
    }
}

fn close_integration(
    connection: &ShellMonitorConnection,
    spec: &ShellMonitorSpec,
    tracker: &mut TrustedCommandTracker,
    fallback_notice: Option<ShellIntegrationNotice>,
) {
    tracker.clear_pending();
    let closed_notice = spec
        .registry
        .integration_closed(&spec.terminal_id)
        .ok()
        .flatten();
    let integration_notice = fallback_notice.or(closed_notice);
    let mut presentation = Vec::new();
    if let Some(projection) = project_terminal(connection, &spec.terminal_id, None, true)
        && let Some(terminal) = projection.terminal
    {
        presentation.push(SessionPresentationEvent::TerminalChanged { terminal });
    }
    if let Some(notice) = integration_notice {
        presentation.push(integration_notice_event(notice));
    }
    publish_all(&connection.application, &spec.session_id, presentation);
}

fn project_terminal(
    connection: &ShellMonitorConnection,
    terminal_id: &TerminalSessionId,
    cwd: Option<&PathBuf>,
    include_terminal: bool,
) -> Option<TerminalMonitorProjection> {
    let (canonical_cwd, rejected_cwd) = match cwd {
        Some(cwd) => match cwd.canonicalize() {
            Ok(canonical) if canonical == *cwd => (Some(canonical), false),
            Ok(_) | Err(_) => (None, true),
        },
        None => (None, false),
    };
    let state = connection.state.upgrade()?;
    let projection = loop {
        let terminal_id = terminal_id.clone();
        let canonical_cwd = canonical_cwd.clone();
        match state.call(ApplicationCallContext::new(), move |state, _| {
            state.terminal_monitor_projection(
                &terminal_id,
                canonical_cwd.as_deref(),
                include_terminal,
            )
        }) {
            Ok(projection) => break projection,
            Err(DaemonStateCallError::Full) => std::thread::sleep(CWD_RETRY_INTERVAL),
            Err(DaemonStateCallError::Cancelled | DaemonStateCallError::Closed) => return None,
        }
    };
    let mut projection = projection.unwrap_or(TerminalMonitorProjection {
        terminal: None,
        header: None,
        cwd_consumed: cwd.is_none(),
    });
    if rejected_cwd {
        projection.cwd_consumed = true;
    }
    Some(projection)
}

fn publish_all(
    application: &ApplicationService,
    session_id: &SessionId,
    events: Vec<SessionPresentationEvent>,
) {
    for event in events {
        let _ = application.publish(session_id, event);
    }
}

fn integration_notice_event(
    integration_notice: ShellIntegrationNotice,
) -> SessionPresentationEvent {
    notice(integration_notice.code, &integration_notice.message)
}

fn notice(code: &str, message: &str) -> SessionPresentationEvent {
    SessionPresentationEvent::Notice {
        severity: Severity::Warning,
        code: bounded_text(code, 8 * 1024),
        message: bounded_text(message, 8 * 1024),
    }
}

fn bounded_text(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_owned();
    }
    let mut end = maximum_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn presentation_exit(exit: &ShellExit) -> i32 {
    match exit {
        ShellExit::Code { code } => *code,
        ShellExit::Signal { signal } => signal.saturating_add(128),
    }
}

struct TrustedCommandTracker {
    command_sequence: u64,
    pending: Option<PendingCommand>,
}

struct PendingCommand {
    command_sequence: u64,
    command: String,
}

struct ProjectedIntegrationEvents {
    presentation: Vec<SessionPresentationEvent>,
    completed_commands: Vec<String>,
    cwd: Option<PathBuf>,
}

impl TrustedCommandTracker {
    fn new(command_sequence: u64) -> Self {
        Self {
            command_sequence,
            pending: None,
        }
    }

    fn accept(
        &mut self,
        terminal_id: &TerminalSessionId,
        events: &[ShellIntegrationEvent],
        accepted_command_sequence: u64,
    ) -> Result<ProjectedIntegrationEvents, ()> {
        let mut next_command_sequence = self.command_sequence;
        let mut pending = self.pending.take();
        let mut presentation = Vec::new();
        let mut completed_commands = Vec::new();
        let mut cwd = None;

        for event in events {
            match event {
                ShellIntegrationEvent::PromptReady {
                    cwd: prompt_cwd, ..
                } => cwd = Some(prompt_cwd.clone()),
                ShellIntegrationEvent::CommandStarted { command, .. } => {
                    if pending.is_some() {
                        return Err(());
                    }
                    next_command_sequence = next_command_sequence.checked_add(1).ok_or(())?;
                    pending = Some(PendingCommand {
                        command_sequence: next_command_sequence,
                        command: command.clone(),
                    });
                    presentation.push(SessionPresentationEvent::TerminalCommandStarted {
                        terminal_id: terminal_id.clone(),
                        sequence: next_command_sequence,
                    });
                }
                ShellIntegrationEvent::CommandFinished {
                    exit,
                    cwd: finished_cwd,
                    ..
                } => {
                    let command = pending.take().ok_or(())?;
                    presentation.push(SessionPresentationEvent::TerminalCommandFinished {
                        terminal_id: terminal_id.clone(),
                        sequence: command.command_sequence,
                        exit_status: presentation_exit(exit),
                        cwd: finished_cwd.to_string_lossy().into_owned(),
                    });
                    completed_commands.push(command.command);
                    cwd = Some(finished_cwd.clone());
                }
                ShellIntegrationEvent::ForegroundChanged { .. } => {}
            }
        }
        if next_command_sequence != accepted_command_sequence {
            return Err(());
        }
        self.command_sequence = next_command_sequence;
        self.pending = pending;
        Ok(ProjectedIntegrationEvents {
            presentation,
            completed_commands,
            cwd,
        })
    }

    fn clear_pending(&mut self) {
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> PathBuf {
        std::env::current_dir().unwrap().canonicalize().unwrap()
    }

    #[test]
    fn trusted_pair_yields_only_private_history_and_metadata() {
        let terminal_id = TerminalSessionId::generate();
        let directory = cwd();
        let command = "printf 'private-token'";
        let mut tracker = TrustedCommandTracker::new(0);
        let projected = tracker
            .accept(
                &terminal_id,
                &[
                    ShellIntegrationEvent::PromptReady {
                        sequence: 1,
                        cwd: directory.clone(),
                        last_exit: None,
                    },
                    ShellIntegrationEvent::CommandStarted {
                        sequence: 2,
                        command: command.to_owned(),
                        cwd: directory.clone(),
                    },
                    ShellIntegrationEvent::CommandFinished {
                        sequence: 3,
                        exit: ShellExit::Code { code: 0 },
                        cwd: directory.clone(),
                    },
                ],
                1,
            )
            .unwrap();

        assert_eq!(projected.completed_commands, [command]);
        assert_eq!(projected.cwd, Some(directory));
        let encoded = serde_json::to_string(&projected.presentation).unwrap();
        assert!(!encoded.contains("private-token"));
        assert!(encoded.contains(terminal_id.as_str()));
    }

    #[test]
    fn unfinished_or_mismatched_boundaries_never_complete_history() {
        let directory = cwd();
        let mut tracker = TrustedCommandTracker::new(7);
        let terminal_id = TerminalSessionId::generate();
        let started = tracker
            .accept(
                &terminal_id,
                &[ShellIntegrationEvent::CommandStarted {
                    sequence: 1,
                    command: "unfinished-secret".to_owned(),
                    cwd: directory.clone(),
                }],
                8,
            )
            .unwrap();
        assert!(started.completed_commands.is_empty());
        tracker.clear_pending();
        assert!(
            tracker
                .accept(
                    &terminal_id,
                    &[ShellIntegrationEvent::CommandFinished {
                        sequence: 2,
                        exit: ShellExit::Code { code: 1 },
                        cwd: directory,
                    }],
                    8,
                )
                .is_err()
        );
    }
}
