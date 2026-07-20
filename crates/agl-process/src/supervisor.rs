use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agl_ids::{ExecutionId, RequestId};

use crate::platform::{self, LaunchDirectories, LaunchedProcess};
use crate::terminal::shell::{ManagedShellIntegrationTransport, ManagedShellStartup};
use crate::{
    CommittedOutputFrame, ExecutionChannel, ExecutionCursor, ExecutionExit, ExecutionListFilter,
    ExecutionOutputChunk, ExecutionOwner, ExecutionPrivateCommand, ExecutionReadResult,
    ExecutionRepository, ExecutionRequest, ExecutionState, ExecutionStatus,
    ExecutionTerminalUpdate, InputLease, KillMode, OutputSpool, ProcessBytes, ProcessError,
    ProcessErrorCode, ProcessSupervisorOptions, Result, ShellIntegrationReadResult, TerminalSize,
    WRITABLE_INPUT_LEASE_TTL,
};

const MAX_BUFFERED_SHELL_INTEGRATION_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub struct ProcessHandle {
    inner: Arc<SupervisorInner>,
}

pub struct ProcessSupervisor {
    inner: Arc<SupervisorInner>,
}

struct SupervisorInner {
    sender: Mutex<Option<SyncSender<Command>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

enum Command {
    Start {
        reserved_execution_id: Option<ExecutionId>,
        request: Box<ExecutionRequest>,
        managed_startup: Option<Box<ManagedShellStartup>>,
        cancelled: Arc<AtomicBool>,
        timed_out: Arc<AtomicBool>,
        reply: Reply<ExecutionStatus>,
    },
    Status {
        execution_id: ExecutionId,
        owner: Option<ExecutionOwner>,
        reply: Reply<ExecutionStatus>,
    },
    PrivateInvocation {
        execution_id: ExecutionId,
        maximum_bytes: usize,
        reply: Reply<ExecutionPrivateCommand>,
    },
    List {
        filter: ExecutionListFilter,
        owner: Option<ExecutionOwner>,
        reply: Reply<Vec<ExecutionStatus>>,
    },
    Read {
        execution_id: ExecutionId,
        owner: Option<ExecutionOwner>,
        cursor: ExecutionCursor,
        maximum_bytes: usize,
        reply: Reply<ExecutionReadResult>,
    },
    ReadShellIntegration {
        execution_id: ExecutionId,
        owner: Option<ExecutionOwner>,
        maximum_bytes: usize,
        reply: Reply<ShellIntegrationReadResult>,
    },
    Attach {
        execution_id: ExecutionId,
        owner: Option<ExecutionOwner>,
        attachment_id: RequestId,
        writable: bool,
        reply: Reply<InputLease>,
    },
    Detach {
        execution_id: ExecutionId,
        owner: Option<ExecutionOwner>,
        lease: InputLease,
        reply: Reply<()>,
    },
    RenewInputLease {
        execution_id: ExecutionId,
        owner: Option<ExecutionOwner>,
        lease: InputLease,
        reply: Reply<()>,
    },
    InputLeaseActive {
        execution_id: ExecutionId,
        owner: Option<ExecutionOwner>,
        lease: InputLease,
        reply: Reply<bool>,
    },
    Write {
        execution_id: ExecutionId,
        owner: Option<ExecutionOwner>,
        lease: InputLease,
        bytes: ProcessBytes,
        eof: bool,
        reply: Reply<()>,
    },
    Resize {
        execution_id: ExecutionId,
        owner: Option<ExecutionOwner>,
        terminal_size: TerminalSize,
        reply: Reply<()>,
    },
    InterruptForeground {
        execution_id: ExecutionId,
        owner: Option<ExecutionOwner>,
        reply: Reply<()>,
    },
    HandoffManagedTerminal {
        execution_id: ExecutionId,
        owner: ExecutionOwner,
        interrupt_foreground: bool,
        reply: Reply<()>,
    },
    Kill {
        execution_id: ExecutionId,
        owner: Option<ExecutionOwner>,
        mode: KillMode,
        reason: TerminationReason,
        reply: Reply<()>,
    },
    TerminateOwner {
        owner: ExecutionOwner,
        reason: TerminationReason,
        reply: Reply<usize>,
    },
    TerminateGrant {
        grant_id: String,
        reason: TerminationReason,
        reply: Reply<usize>,
    },
    TerminateRunGrants {
        creating_run_id: agl_ids::RunId,
        duration: String,
        reason: TerminationReason,
        reply: Reply<usize>,
    },
    ReconcileGrants {
        live_grant_ids: BTreeSet<String>,
        reason: TerminationReason,
        reply: Reply<usize>,
    },
    Shutdown {
        reply: Reply<()>,
    },
}

type Reply<T> = mpsc::Sender<Result<T>>;

#[derive(Clone, Copy, Eq, PartialEq)]
enum TerminationReason {
    Cancelled,
    TimedOut,
    OutputLimit,
    SupervisorShutdown,
    GrantRevoked,
    GrantExpired,
    RuntimeFailure(ProcessErrorCode),
}

impl TerminationReason {
    fn state(self) -> ExecutionState {
        match self {
            Self::Cancelled | Self::GrantRevoked | Self::GrantExpired => ExecutionState::Cancelled,
            Self::TimedOut => ExecutionState::TimedOut,
            Self::OutputLimit | Self::SupervisorShutdown | Self::RuntimeFailure(_) => {
                ExecutionState::Failed
            }
        }
    }

    fn error_code(self) -> Option<String> {
        match self {
            Self::Cancelled => None,
            Self::TimedOut => None,
            Self::OutputLimit => Some(ProcessErrorCode::OutputLimitExceeded.as_str().to_owned()),
            Self::SupervisorShutdown => {
                Some(ProcessErrorCode::SupervisorShutdown.as_str().to_owned())
            }
            Self::GrantRevoked => Some(ProcessErrorCode::GrantRevoked.as_str().to_owned()),
            Self::GrantExpired => Some(ProcessErrorCode::GrantExpired.as_str().to_owned()),
            Self::RuntimeFailure(code) => Some(code.as_str().to_owned()),
        }
    }
}

struct Reactor {
    options: ProcessSupervisorOptions,
    repository: Arc<dyn ExecutionRepository>,
    spool: Arc<dyn OutputSpool>,
    supervisor_id: String,
    active: BTreeMap<ExecutionId, ActiveExecution>,
    shell_integrations: BTreeMap<ExecutionId, ActiveShellIntegration>,
    shutting_down: bool,
    shutdown_replies: Vec<Reply<()>>,
    next_retention_scan: Instant,
}

struct ActiveExecution {
    request: ExecutionRequest,
    child: std::process::Child,
    stdin: Option<OwnedFd>,
    stdout: Option<OwnedFd>,
    stderr: Option<OwnedFd>,
    terminal: Option<OwnedFd>,
    next_sequence: u64,
    output_bytes: u64,
    discarded_output_bytes: u64,
    output_truncated: bool,
    accepted_input_bytes: u64,
    input: VecDeque<Vec<u8>>,
    input_offset: usize,
    close_stdin_when_drained: bool,
    writable_lease: Option<WritableInputLease>,
    deadline: Option<Instant>,
    termination: Option<Termination>,
}

struct ManagedLaunchedProcess {
    process: LaunchedProcess,
    shell_integration: Option<ManagedShellIntegrationTransport>,
}

struct ActiveShellIntegration {
    reader: Option<OwnedFd>,
    fifo_path: std::path::PathBuf,
    buffered: VecDeque<u8>,
    closed: bool,
    degraded: bool,
    output_through_sequence: u64,
}

#[derive(Clone)]
struct WritableInputLease {
    attachment_id: RequestId,
    expires_at: Instant,
}

impl WritableInputLease {
    fn new(attachment_id: RequestId, now: Instant) -> Self {
        Self {
            attachment_id,
            expires_at: now + WRITABLE_INPUT_LEASE_TTL,
        }
    }

    fn renew(&mut self, now: Instant) {
        self.expires_at = now + WRITABLE_INPUT_LEASE_TTL;
    }

    fn is_expired(&self, now: Instant) -> bool {
        now >= self.expires_at
    }

    fn as_input_lease(&self) -> InputLease {
        InputLease {
            attachment_id: self.attachment_id.clone(),
            writable: true,
        }
    }
}

#[derive(Clone, Copy)]
struct Termination {
    reason: TerminationReason,
    force_at: Instant,
    forced: bool,
}

impl ProcessSupervisor {
    pub fn start(
        options: ProcessSupervisorOptions,
        repository: Arc<dyn ExecutionRepository>,
        spool: Arc<dyn OutputSpool>,
    ) -> Result<Self> {
        options.validate()?;
        ensure_private_directory(&options.data_root)?;
        ensure_private_directory(&options.state_root)?;
        let supervisor_id = format!("supervisor-{}", ExecutionId::generate());
        let recovered = repository.recover_prior_owners(&supervisor_id, unix_millis())?;
        for execution_id in recovered {
            let committed = repository.committed_output_frames(&execution_id)?;
            spool.recover(&execution_id, &committed)?;
        }

        let (sender, receiver) = mpsc::sync_channel(options.command_capacity);
        let reactor = Reactor {
            options,
            repository,
            spool,
            supervisor_id,
            active: BTreeMap::new(),
            shell_integrations: BTreeMap::new(),
            shutting_down: false,
            shutdown_replies: Vec::new(),
            next_retention_scan: Instant::now(),
        };
        let thread = thread::Builder::new()
            .name("agl-process-reactor".to_owned())
            .spawn(move || reactor.run(receiver))
            .map_err(|error| {
                ProcessError::new(
                    ProcessErrorCode::SupervisorShutdown,
                    format!("failed to start process reactor: {error}"),
                )
            })?;
        Ok(Self {
            inner: Arc::new(SupervisorInner {
                sender: Mutex::new(Some(sender)),
                thread: Mutex::new(Some(thread)),
            }),
        })
    }

    pub fn handle(&self) -> ProcessHandle {
        ProcessHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    pub fn shutdown(&self) -> Result<()> {
        self.handle().request(|reply| Command::Shutdown { reply })?;
        self.inner.join()
    }
}

impl ProcessHandle {
    pub fn start(&self, request: ExecutionRequest) -> Result<ExecutionStatus> {
        self.start_cancellable(request, None, || false)
    }

    pub fn start_cancellable(
        &self,
        request: ExecutionRequest,
        deadline: Option<Instant>,
        cancelled: impl Fn() -> bool,
    ) -> Result<ExecutionStatus> {
        self.start_cancellable_with_startup(request, None, None, deadline, cancelled)
    }

    pub(crate) fn start_reserved_managed_terminal(
        &self,
        execution_id: ExecutionId,
        request: ExecutionRequest,
        managed_startup: ManagedShellStartup,
    ) -> Result<ExecutionStatus> {
        self.start_cancellable_with_startup(
            request,
            Some(execution_id),
            Some(managed_startup),
            None,
            || false,
        )
    }

    fn start_cancellable_with_startup(
        &self,
        request: ExecutionRequest,
        reserved_execution_id: Option<ExecutionId>,
        managed_startup: Option<ManagedShellStartup>,
        deadline: Option<Instant>,
        cancelled: impl Fn() -> bool,
    ) -> Result<ExecutionStatus> {
        let (reply, response) = mpsc::channel();
        let admission_cancelled = Arc::new(AtomicBool::new(false));
        let admission_timed_out = Arc::new(AtomicBool::new(false));
        let sender = self
            .inner
            .sender
            .lock()
            .map_err(|_| supervisor_shutdown())?
            .clone()
            .ok_or_else(supervisor_shutdown)?;
        match sender.try_send(Command::Start {
            reserved_execution_id,
            request: Box::new(request),
            managed_startup: managed_startup.map(Box::new),
            cancelled: Arc::clone(&admission_cancelled),
            timed_out: Arc::clone(&admission_timed_out),
            reply,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                return Err(ProcessError::new(
                    ProcessErrorCode::InputBackpressure,
                    "process supervisor command queue is full",
                ));
            }
            Err(TrySendError::Disconnected(_)) => return Err(supervisor_shutdown()),
        }
        loop {
            if cancelled() {
                admission_cancelled.store(true, Ordering::Release);
            } else if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                admission_timed_out.store(true, Ordering::Release);
                admission_cancelled.store(true, Ordering::Release);
            }
            match response.recv_timeout(Duration::from_millis(10)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(supervisor_shutdown()),
            }
        }
    }

    pub fn status(
        &self,
        execution_id: &ExecutionId,
        owner: &ExecutionOwner,
    ) -> Result<ExecutionStatus> {
        self.request(|reply| Command::Status {
            execution_id: execution_id.clone(),
            owner: Some(owner.clone()),
            reply,
        })
    }

    pub fn operator_status(&self, execution_id: &ExecutionId) -> Result<ExecutionStatus> {
        self.request(|reply| Command::Status {
            execution_id: execution_id.clone(),
            owner: None,
            reply,
        })
    }

    pub fn operator_private_command(
        &self,
        execution_id: &ExecutionId,
        maximum_bytes: usize,
    ) -> Result<ExecutionPrivateCommand> {
        self.request(|reply| Command::PrivateInvocation {
            execution_id: execution_id.clone(),
            maximum_bytes,
            reply,
        })
    }

    pub fn list_owned(
        &self,
        filter: ExecutionListFilter,
        owner: &ExecutionOwner,
    ) -> Result<Vec<ExecutionStatus>> {
        self.request(|reply| Command::List {
            filter,
            owner: Some(owner.clone()),
            reply,
        })
    }

    pub fn operator_list(&self, filter: ExecutionListFilter) -> Result<Vec<ExecutionStatus>> {
        self.request(|reply| Command::List {
            filter,
            owner: None,
            reply,
        })
    }

    pub fn read(
        &self,
        execution_id: &ExecutionId,
        owner: &ExecutionOwner,
        cursor: ExecutionCursor,
        maximum_bytes: usize,
    ) -> Result<ExecutionReadResult> {
        self.read_with_owner(execution_id, Some(owner.clone()), cursor, maximum_bytes)
    }

    pub fn operator_read(
        &self,
        execution_id: &ExecutionId,
        cursor: ExecutionCursor,
        maximum_bytes: usize,
    ) -> Result<ExecutionReadResult> {
        self.read_with_owner(execution_id, None, cursor, maximum_bytes)
    }

    pub fn read_shell_integration(
        &self,
        execution_id: &ExecutionId,
        owner: &ExecutionOwner,
        maximum_bytes: usize,
    ) -> Result<ShellIntegrationReadResult> {
        self.read_shell_integration_with_owner(execution_id, Some(owner.clone()), maximum_bytes)
    }

    pub fn operator_read_shell_integration(
        &self,
        execution_id: &ExecutionId,
        maximum_bytes: usize,
    ) -> Result<ShellIntegrationReadResult> {
        self.read_shell_integration_with_owner(execution_id, None, maximum_bytes)
    }

    pub fn attach(
        &self,
        execution_id: &ExecutionId,
        owner: &ExecutionOwner,
        attachment_id: RequestId,
        writable: bool,
    ) -> Result<InputLease> {
        self.attach_with_owner(execution_id, Some(owner.clone()), attachment_id, writable)
    }

    pub fn operator_attach(
        &self,
        execution_id: &ExecutionId,
        attachment_id: RequestId,
        writable: bool,
    ) -> Result<InputLease> {
        self.attach_with_owner(execution_id, None, attachment_id, writable)
    }

    pub fn detach(
        &self,
        execution_id: &ExecutionId,
        owner: &ExecutionOwner,
        lease: InputLease,
    ) -> Result<()> {
        self.detach_with_owner(execution_id, Some(owner.clone()), lease)
    }

    pub fn operator_detach(&self, execution_id: &ExecutionId, lease: InputLease) -> Result<()> {
        self.detach_with_owner(execution_id, None, lease)
    }

    pub fn renew_input_lease(
        &self,
        execution_id: &ExecutionId,
        owner: &ExecutionOwner,
        lease: InputLease,
    ) -> Result<()> {
        self.renew_input_lease_with_owner(execution_id, Some(owner.clone()), lease)
    }

    pub fn operator_renew_input_lease(
        &self,
        execution_id: &ExecutionId,
        lease: InputLease,
    ) -> Result<()> {
        self.renew_input_lease_with_owner(execution_id, None, lease)
    }

    pub fn input_lease_active(
        &self,
        execution_id: &ExecutionId,
        owner: &ExecutionOwner,
        lease: InputLease,
    ) -> Result<bool> {
        self.input_lease_active_with_owner(execution_id, Some(owner.clone()), lease)
    }

    pub fn operator_input_lease_active(
        &self,
        execution_id: &ExecutionId,
        lease: InputLease,
    ) -> Result<bool> {
        self.input_lease_active_with_owner(execution_id, None, lease)
    }

    pub fn write(
        &self,
        execution_id: &ExecutionId,
        owner: &ExecutionOwner,
        lease: InputLease,
        bytes: ProcessBytes,
        eof: bool,
    ) -> Result<()> {
        self.write_with_owner(execution_id, Some(owner.clone()), lease, bytes, eof)
    }

    pub fn operator_write(
        &self,
        execution_id: &ExecutionId,
        lease: InputLease,
        bytes: ProcessBytes,
        eof: bool,
    ) -> Result<()> {
        self.write_with_owner(execution_id, None, lease, bytes, eof)
    }

    pub fn resize(
        &self,
        execution_id: &ExecutionId,
        owner: &ExecutionOwner,
        terminal_size: TerminalSize,
    ) -> Result<()> {
        self.resize_with_owner(execution_id, Some(owner.clone()), terminal_size)
    }

    pub fn operator_resize(
        &self,
        execution_id: &ExecutionId,
        terminal_size: TerminalSize,
    ) -> Result<()> {
        self.resize_with_owner(execution_id, None, terminal_size)
    }

    pub fn interrupt_foreground(
        &self,
        execution_id: &ExecutionId,
        owner: &ExecutionOwner,
    ) -> Result<()> {
        self.interrupt_foreground_with_owner(execution_id, Some(owner.clone()))
    }

    pub fn operator_interrupt_foreground(&self, execution_id: &ExecutionId) -> Result<()> {
        self.interrupt_foreground_with_owner(execution_id, None)
    }

    pub(crate) fn operator_handoff_managed_terminal(
        &self,
        execution_id: &ExecutionId,
        owner: ExecutionOwner,
        interrupt_foreground: bool,
    ) -> Result<()> {
        self.request(|reply| Command::HandoffManagedTerminal {
            execution_id: execution_id.clone(),
            owner,
            interrupt_foreground,
            reply,
        })
    }

    pub fn kill(
        &self,
        execution_id: &ExecutionId,
        owner: &ExecutionOwner,
        mode: KillMode,
    ) -> Result<()> {
        self.kill_with_owner(
            execution_id,
            Some(owner.clone()),
            mode,
            TerminationReason::Cancelled,
        )
    }

    pub fn operator_kill(&self, execution_id: &ExecutionId, mode: KillMode) -> Result<()> {
        self.kill_with_owner(execution_id, None, mode, TerminationReason::Cancelled)
    }

    pub fn terminate_owner(&self, owner: &ExecutionOwner) -> Result<usize> {
        self.request(|reply| Command::TerminateOwner {
            owner: owner.clone(),
            reason: TerminationReason::Cancelled,
            reply,
        })
    }

    pub fn terminate_grant(&self, grant_id: &str) -> Result<usize> {
        if grant_id.trim().is_empty() {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "grant termination requires a nonempty grant identity",
            ));
        }
        self.request(|reply| Command::TerminateGrant {
            grant_id: grant_id.to_owned(),
            reason: TerminationReason::GrantRevoked,
            reply,
        })
    }

    pub fn expire_run_grants(
        &self,
        creating_run_id: &agl_ids::RunId,
        duration: &str,
    ) -> Result<usize> {
        if duration.trim().is_empty() {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "grant expiry requires a nonempty duration",
            ));
        }
        self.request(|reply| Command::TerminateRunGrants {
            creating_run_id: creating_run_id.clone(),
            duration: duration.to_owned(),
            reason: TerminationReason::GrantExpired,
            reply,
        })
    }

    pub fn terminate_inactive_grants(&self, live_grant_ids: BTreeSet<String>) -> Result<usize> {
        self.request(|reply| Command::ReconcileGrants {
            live_grant_ids,
            reason: TerminationReason::GrantExpired,
            reply,
        })
    }

    pub fn wait(
        &self,
        execution_id: &ExecutionId,
        owner: &ExecutionOwner,
        deadline: Option<Instant>,
        cancelled: impl Fn() -> bool,
    ) -> Result<ExecutionStatus> {
        loop {
            let status = self.status(execution_id, owner)?;
            if status.state.is_terminal() {
                return Ok(status);
            }
            if cancelled() {
                self.kill_with_owner(
                    execution_id,
                    Some(owner.clone()),
                    KillMode::Graceful,
                    TerminationReason::Cancelled,
                )?;
            } else if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                self.kill_with_owner(
                    execution_id,
                    Some(owner.clone()),
                    KillMode::Graceful,
                    TerminationReason::TimedOut,
                )?;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn read_with_owner(
        &self,
        execution_id: &ExecutionId,
        owner: Option<ExecutionOwner>,
        cursor: ExecutionCursor,
        maximum_bytes: usize,
    ) -> Result<ExecutionReadResult> {
        self.request(|reply| Command::Read {
            execution_id: execution_id.clone(),
            owner,
            cursor,
            maximum_bytes,
            reply,
        })
    }

    fn read_shell_integration_with_owner(
        &self,
        execution_id: &ExecutionId,
        owner: Option<ExecutionOwner>,
        maximum_bytes: usize,
    ) -> Result<ShellIntegrationReadResult> {
        self.request(|reply| Command::ReadShellIntegration {
            execution_id: execution_id.clone(),
            owner,
            maximum_bytes,
            reply,
        })
    }

    fn attach_with_owner(
        &self,
        execution_id: &ExecutionId,
        owner: Option<ExecutionOwner>,
        attachment_id: RequestId,
        writable: bool,
    ) -> Result<InputLease> {
        self.request(|reply| Command::Attach {
            execution_id: execution_id.clone(),
            owner,
            attachment_id,
            writable,
            reply,
        })
    }

    fn detach_with_owner(
        &self,
        execution_id: &ExecutionId,
        owner: Option<ExecutionOwner>,
        lease: InputLease,
    ) -> Result<()> {
        self.request(|reply| Command::Detach {
            execution_id: execution_id.clone(),
            owner,
            lease,
            reply,
        })
    }

    fn write_with_owner(
        &self,
        execution_id: &ExecutionId,
        owner: Option<ExecutionOwner>,
        lease: InputLease,
        bytes: ProcessBytes,
        eof: bool,
    ) -> Result<()> {
        self.request(|reply| Command::Write {
            execution_id: execution_id.clone(),
            owner,
            lease,
            bytes,
            eof,
            reply,
        })
    }

    fn renew_input_lease_with_owner(
        &self,
        execution_id: &ExecutionId,
        owner: Option<ExecutionOwner>,
        lease: InputLease,
    ) -> Result<()> {
        self.request(|reply| Command::RenewInputLease {
            execution_id: execution_id.clone(),
            owner,
            lease,
            reply,
        })
    }

    fn input_lease_active_with_owner(
        &self,
        execution_id: &ExecutionId,
        owner: Option<ExecutionOwner>,
        lease: InputLease,
    ) -> Result<bool> {
        self.request(|reply| Command::InputLeaseActive {
            execution_id: execution_id.clone(),
            owner,
            lease,
            reply,
        })
    }

    fn resize_with_owner(
        &self,
        execution_id: &ExecutionId,
        owner: Option<ExecutionOwner>,
        terminal_size: TerminalSize,
    ) -> Result<()> {
        self.request(|reply| Command::Resize {
            execution_id: execution_id.clone(),
            owner,
            terminal_size,
            reply,
        })
    }

    fn interrupt_foreground_with_owner(
        &self,
        execution_id: &ExecutionId,
        owner: Option<ExecutionOwner>,
    ) -> Result<()> {
        self.request(|reply| Command::InterruptForeground {
            execution_id: execution_id.clone(),
            owner,
            reply,
        })
    }

    fn kill_with_owner(
        &self,
        execution_id: &ExecutionId,
        owner: Option<ExecutionOwner>,
        mode: KillMode,
        reason: TerminationReason,
    ) -> Result<()> {
        self.request(|reply| Command::Kill {
            execution_id: execution_id.clone(),
            owner,
            mode,
            reason,
            reply,
        })
    }

    fn request<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (reply, response) = mpsc::channel();
        let sender = self
            .inner
            .sender
            .lock()
            .map_err(|_| supervisor_shutdown())?
            .clone()
            .ok_or_else(supervisor_shutdown)?;
        match sender.try_send(make(reply)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                return Err(ProcessError::new(
                    ProcessErrorCode::InputBackpressure,
                    "process supervisor command queue is full",
                ));
            }
            Err(TrySendError::Disconnected(_)) => return Err(supervisor_shutdown()),
        }
        response.recv().map_err(|_| supervisor_shutdown())?
    }
}

impl SupervisorInner {
    fn join(&self) -> Result<()> {
        let thread = self
            .thread
            .lock()
            .map_err(|_| supervisor_shutdown())?
            .take();
        if let Some(thread) = thread {
            thread.join().map_err(|_| {
                ProcessError::new(
                    ProcessErrorCode::SupervisorShutdown,
                    "process reactor panicked during shutdown",
                )
            })?;
        }
        self.sender
            .lock()
            .map_err(|_| supervisor_shutdown())?
            .take();
        Ok(())
    }
}

impl Drop for SupervisorInner {
    fn drop(&mut self) {
        if let Ok(sender) = self.sender.get_mut() {
            sender.take();
        }
        if let Ok(thread) = self.thread.get_mut()
            && let Some(thread) = thread.take()
        {
            let _ = thread.join();
        }
    }
}

impl Reactor {
    fn run(mut self, receiver: Receiver<Command>) {
        loop {
            match receiver.recv_timeout(self.options.poll_interval) {
                Ok(command) => self.handle(command),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => self.begin_shutdown(None),
            }
            loop {
                match receiver.try_recv() {
                    Ok(command) => self.handle(command),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.begin_shutdown(None);
                        break;
                    }
                }
            }
            self.pump();
            if self.shutting_down && self.active.is_empty() {
                for reply in self.shutdown_replies.drain(..) {
                    let _ = reply.send(Ok(()));
                }
                break;
            }
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Start {
                reserved_execution_id,
                request,
                managed_startup,
                cancelled,
                timed_out,
                reply,
            } => {
                let _ = reply.send(self.start_execution(
                    *request,
                    reserved_execution_id,
                    managed_startup.map(|startup| *startup),
                    &cancelled,
                    &timed_out,
                ));
            }
            Command::Status {
                execution_id,
                owner,
                reply,
            } => {
                let _ = reply.send(self.checked_status(&execution_id, owner.as_ref()));
            }
            Command::PrivateInvocation {
                execution_id,
                maximum_bytes,
                reply,
            } => {
                let _ = reply.send(
                    self.repository
                        .private_command(&execution_id, maximum_bytes),
                );
            }
            Command::List {
                filter,
                owner,
                reply,
            } => {
                let result = self.repository.list(&filter).map(|statuses| {
                    statuses
                        .into_iter()
                        .filter(|status| {
                            owner
                                .as_ref()
                                .is_none_or(|owner| status.owner.may_access(owner))
                        })
                        .collect()
                });
                let _ = reply.send(result);
            }
            Command::Read {
                execution_id,
                owner,
                cursor,
                maximum_bytes,
                reply,
            } => {
                let _ = reply.send(self.read_output(
                    &execution_id,
                    owner.as_ref(),
                    cursor,
                    maximum_bytes,
                ));
            }
            Command::ReadShellIntegration {
                execution_id,
                owner,
                maximum_bytes,
                reply,
            } => {
                let _ = reply.send(self.read_shell_integration(
                    &execution_id,
                    owner.as_ref(),
                    maximum_bytes,
                ));
            }
            Command::Attach {
                execution_id,
                owner,
                attachment_id,
                writable,
                reply,
            } => {
                let _ = reply.send(self.attach_input(
                    &execution_id,
                    owner.as_ref(),
                    attachment_id,
                    writable,
                ));
            }
            Command::Detach {
                execution_id,
                owner,
                lease,
                reply,
            } => {
                let _ = reply.send(self.detach_input(&execution_id, owner.as_ref(), &lease));
            }
            Command::RenewInputLease {
                execution_id,
                owner,
                lease,
                reply,
            } => {
                let _ = reply.send(self.renew_input_lease(&execution_id, owner.as_ref(), &lease));
            }
            Command::InputLeaseActive {
                execution_id,
                owner,
                lease,
                reply,
            } => {
                let _ =
                    reply.send(self.input_lease_is_active(&execution_id, owner.as_ref(), &lease));
            }
            Command::Write {
                execution_id,
                owner,
                lease,
                bytes,
                eof,
                reply,
            } => {
                let _ =
                    reply.send(self.queue_input(&execution_id, owner.as_ref(), &lease, bytes, eof));
            }
            Command::Resize {
                execution_id,
                owner,
                terminal_size,
                reply,
            } => {
                let _ =
                    reply.send(self.resize_terminal(&execution_id, owner.as_ref(), terminal_size));
            }
            Command::InterruptForeground {
                execution_id,
                owner,
                reply,
            } => {
                let _ = reply
                    .send(self.interrupt_foreground_process_group(&execution_id, owner.as_ref()));
            }
            Command::HandoffManagedTerminal {
                execution_id,
                owner,
                interrupt_foreground,
                reply,
            } => {
                let _ = reply.send(self.handoff_managed_terminal_owner(
                    &execution_id,
                    owner,
                    interrupt_foreground,
                ));
            }
            Command::Kill {
                execution_id,
                owner,
                mode,
                reason,
                reply,
            } => {
                let _ =
                    reply.send(self.kill_execution(&execution_id, owner.as_ref(), mode, reason));
            }
            Command::TerminateOwner {
                owner,
                reason,
                reply,
            } => {
                let ids = self.active_execution_ids_owned_by(&owner);
                let mut terminated = 0;
                let mut error = None;
                for id in ids {
                    match self.begin_termination(&id, KillMode::Graceful, reason) {
                        Ok(()) => terminated += 1,
                        Err(found) => {
                            error.get_or_insert(found);
                        }
                    };
                }
                let _ = reply.send(error.map_or(Ok(terminated), Err));
            }
            Command::TerminateGrant {
                grant_id,
                reason,
                reply,
            } => {
                let ids = self
                    .active
                    .iter()
                    .filter(|(_, execution)| {
                        execution.request.grant_lease.as_ref().is_some_and(|lease| {
                            lease.is_capability_grant() && lease.grant_id == grant_id
                        })
                    })
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                let _ = reply.send(self.terminate_ids(ids, reason));
            }
            Command::TerminateRunGrants {
                creating_run_id,
                duration,
                reason,
                reply,
            } => {
                let ids = self
                    .active
                    .iter()
                    .filter(|(_, execution)| {
                        execution.request.creating_run_id == creating_run_id
                            && execution.request.grant_lease.as_ref().is_some_and(|lease| {
                                lease.is_capability_grant() && lease.duration == duration
                            })
                    })
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                let _ = reply.send(self.terminate_ids(ids, reason));
            }
            Command::ReconcileGrants {
                live_grant_ids,
                reason,
                reply,
            } => {
                let ids = self
                    .active
                    .iter()
                    .filter(|(_, execution)| {
                        grant_lease_is_inactive(
                            execution.request.grant_lease.as_ref(),
                            &live_grant_ids,
                        )
                    })
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                let _ = reply.send(self.terminate_ids(ids, reason));
            }
            Command::Shutdown { reply } => self.begin_shutdown(Some(reply)),
        }
    }

    fn start_execution(
        &mut self,
        mut request: ExecutionRequest,
        reserved_execution_id: Option<ExecutionId>,
        managed_startup: Option<ManagedShellStartup>,
        cancelled: &AtomicBool,
        timed_out: &AtomicBool,
    ) -> Result<ExecutionStatus> {
        if self.shutting_down {
            return Err(supervisor_shutdown());
        }
        if reserved_execution_id.is_some() && managed_startup.is_none() {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "a reserved execution identity is valid only for a managed terminal",
            ));
        }
        if let Some(execution_id) = reserved_execution_id.as_ref() {
            self.require_reserved_execution_id_available(execution_id)?;
        }
        request.validate()?;
        if let Some(root) = request
            .read_only_roots
            .iter()
            .find(|root| !self.options.admits_runtime_read_only_root(root))
        {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                format!(
                    "requested runtime read-only root is outside the configured supervisor roots: {}",
                    root.display()
                ),
            ));
        }
        if request.limits.max_input_bytes > self.options.max_input_bytes as u64
            || request.limits.max_output_bytes > self.options.max_spool_bytes
        {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "execution limits exceed the configured supervisor ceilings",
            ));
        }
        if self.active.len() >= self.options.max_active {
            return Err(ProcessError::new(
                ProcessErrorCode::ActiveLimitReached,
                "process supervisor active execution limit was reached",
            ));
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(admission_interrupted(timed_out));
        }
        let explicitly_reserved = reserved_execution_id.is_some();
        let execution_id = reserved_execution_id.unwrap_or_else(ExecutionId::generate);
        let (directories, shell_integration_reader, private_environment) = if let Some(startup) =
            managed_startup
        {
            let directories = self.execution_directories(&execution_id)?;
            let (reader, private_environment) = startup.materialize(&mut request, &directories)?;
            (Some(directories), Some(reader), Some(private_environment))
        } else {
            (None, None, None)
        };
        let now = unix_millis();
        let status = ExecutionStatus {
            execution_id: execution_id.clone(),
            owner: request.owner.clone(),
            state: ExecutionState::Starting,
            profile: request.profile,
            io: request.io,
            cwd: request.cwd.clone(),
            terminal_size: request.terminal_size,
            exit: None,
            first_retained_sequence: None,
            last_sequence: 0,
            retained_bytes: 0,
            discarded_output_bytes: 0,
            output_truncated: false,
            output_expired: false,
            started_at_unix_ms: None,
            finished_at_unix_ms: None,
            error_code: None,
        };
        self.spool.prepare(&execution_id)?;
        if let Err(error) = self
            .repository
            .admit(&status, &request, &self.supervisor_id)
        {
            if !explicitly_reserved {
                let _ = self.spool.remove(&execution_id);
            }
            return Err(error);
        }
        if let Err(error) = commit_lifecycle_with_confirmation(
            &self.repository,
            &execution_id,
            &self.supervisor_id,
            1,
            "admitted",
            now,
        ) {
            return self.fail_unspawned_execution(&execution_id, error);
        }
        if cancelled.load(Ordering::Acquire) {
            return self.fail_unspawned_execution(&execution_id, admission_interrupted(timed_out));
        }

        let directories = match directories {
            Some(directories) => directories,
            None => match self.execution_directories(&execution_id) {
                Ok(directories) => directories,
                Err(error) => return self.fail_unspawned_execution(&execution_id, error),
            },
        };
        let launched = platform::launch(
            &self.options.launcher_path,
            &execution_id,
            &request,
            &directories,
            private_environment,
            self.options.setup_timeout,
            cancelled,
        );
        let launched = match launched {
            Ok(launched) => launched,
            Err(error) => {
                let error = if error.code() == ProcessErrorCode::Cancelled
                    && timed_out.load(Ordering::Acquire)
                {
                    admission_timed_out()
                } else {
                    error
                };
                return self.fail_unspawned_execution(&execution_id, error);
            }
        };
        let deadline = request
            .limits
            .timeout_ms
            .map(Duration::from_millis)
            .and_then(|duration| Instant::now().checked_add(duration));
        let launched = ManagedLaunchedProcess {
            process: launched,
            shell_integration: shell_integration_reader,
        };
        if let Err(error) = commit_running_with_confirmation(
            &self.repository,
            &execution_id,
            &self.supervisor_id,
            unix_millis(),
        ) {
            return self.adopt_failed_spawn(execution_id, request, launched, deadline, 2, error);
        }
        if let Err(error) = commit_lifecycle_with_confirmation(
            &self.repository,
            &execution_id,
            &self.supervisor_id,
            2,
            "running",
            unix_millis(),
        ) {
            return self.adopt_failed_spawn(execution_id, request, launched, deadline, 2, error);
        }
        if let Some(reader) = launched.shell_integration {
            self.shell_integrations
                .insert(execution_id.clone(), ActiveShellIntegration::new(reader));
        }
        self.active.insert(
            execution_id.clone(),
            ActiveExecution::from_launch(request, launched.process, deadline),
        );
        if cancelled.load(Ordering::Acquire) {
            let error = admission_interrupted(timed_out);
            let reason = if error.code() == ProcessErrorCode::TimedOut {
                TerminationReason::TimedOut
            } else {
                TerminationReason::Cancelled
            };
            self.begin_termination(&execution_id, KillMode::Graceful, reason)?;
            return Err(error);
        }
        self.repository.status(&execution_id)
    }

    fn require_reserved_execution_id_available(&self, execution_id: &ExecutionId) -> Result<()> {
        if self.active.contains_key(execution_id) {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                format!("reserved execution identity `{execution_id}` is already active"),
            ));
        }
        match self.repository.status(execution_id) {
            Ok(_) => Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                format!("reserved execution identity `{execution_id}` is already admitted"),
            )),
            Err(error) if error.code() == ProcessErrorCode::ExecutionNotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn fail_unspawned_execution(
        &self,
        execution_id: &ExecutionId,
        error: ProcessError,
    ) -> Result<ExecutionStatus> {
        let status = self.repository.status(execution_id)?;
        if !status.state.is_terminal() {
            let sequence = status.last_sequence.checked_add(1).ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "execution sequence overflowed while recording failed admission",
                )
            })?;
            let update = ExecutionTerminalUpdate {
                state: match error.code() {
                    ProcessErrorCode::Cancelled => ExecutionState::Cancelled,
                    ProcessErrorCode::TimedOut => ExecutionState::TimedOut,
                    _ => ExecutionState::Failed,
                },
                exit: Some(ExecutionExit::Error {
                    code: error.code().as_str().to_owned(),
                }),
                error_code: Some(error.code().as_str().to_owned()),
                finished_at_unix_ms: unix_millis(),
                output_truncated: false,
                discarded_output_bytes: 0,
            };
            commit_terminal_with_confirmation(
                &self.repository,
                execution_id,
                &self.supervisor_id,
                sequence,
                &update,
            )?;
        }
        Err(error)
    }

    fn adopt_failed_spawn(
        &mut self,
        execution_id: ExecutionId,
        request: ExecutionRequest,
        launched: ManagedLaunchedProcess,
        deadline: Option<Instant>,
        next_sequence: u64,
        error: ProcessError,
    ) -> Result<ExecutionStatus> {
        let mut execution = ActiveExecution::from_launch(request, launched.process, deadline);
        execution.next_sequence = next_sequence;
        if let Some(reader) = launched.shell_integration {
            self.shell_integrations
                .insert(execution_id.clone(), ActiveShellIntegration::new(reader));
        }
        self.active.insert(execution_id.clone(), execution);
        let _ = self.begin_termination(
            &execution_id,
            KillMode::Immediate,
            TerminationReason::RuntimeFailure(error.code()),
        );
        Err(ProcessError::new(
            error.code(),
            format!(
                "execution {execution_id} spawned but durable admission failed: {}",
                error.message()
            ),
        ))
    }

    fn checked_status(
        &self,
        execution_id: &ExecutionId,
        owner: Option<&ExecutionOwner>,
    ) -> Result<ExecutionStatus> {
        let status = self.repository.status(execution_id)?;
        if owner.is_some_and(|owner| !status.owner.may_access(owner)) {
            return Err(ProcessError::new(
                ProcessErrorCode::ExecutionNotOwned,
                "execution is not owned by the requesting process context",
            ));
        }
        Ok(status)
    }

    fn require_managed_terminal_operator(
        &self,
        execution_id: &ExecutionId,
        owner: Option<&ExecutionOwner>,
    ) -> Result<()> {
        if owner.is_some() && self.shell_integrations.contains_key(execution_id) {
            return Err(ProcessError::new(
                ProcessErrorCode::ExecutionNotOwned,
                "managed terminal control is available only through its terminal registry operator",
            ));
        }
        Ok(())
    }

    fn read_output(
        &self,
        execution_id: &ExecutionId,
        owner: Option<&ExecutionOwner>,
        cursor: ExecutionCursor,
        maximum_bytes: usize,
    ) -> Result<ExecutionReadResult> {
        if maximum_bytes == 0 || maximum_bytes > self.options.max_result_bytes {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "output read bound is zero or exceeds the supervisor result limit",
            ));
        }
        let status = self.checked_status(execution_id, owner)?;
        if status.output_expired {
            return Err(ProcessError::new(
                ProcessErrorCode::OutputExpired,
                "execution output has expired",
            ));
        }
        let chunks = self.spool.read(
            execution_id,
            cursor.after_sequence,
            status.last_sequence,
            maximum_bytes,
        )?;
        let next_sequence = chunks
            .last()
            .map_or(status.last_sequence, |chunk| chunk.sequence);
        Ok(ExecutionReadResult {
            execution_id: execution_id.clone(),
            chunks,
            next_sequence,
            state: status.state,
            output_truncated: status.output_truncated,
            output_expired: status.output_expired,
        })
    }

    fn read_shell_integration(
        &mut self,
        execution_id: &ExecutionId,
        owner: Option<&ExecutionOwner>,
        maximum_bytes: usize,
    ) -> Result<ShellIntegrationReadResult> {
        if maximum_bytes == 0 || maximum_bytes > self.options.max_result_bytes {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "shell integration read bound is zero or exceeds the supervisor result limit",
            ));
        }
        let status = self.checked_status(execution_id, owner)?;
        self.require_managed_terminal_operator(execution_id, owner)?;
        if status.output_expired {
            return Err(ProcessError::new(
                ProcessErrorCode::OutputExpired,
                "private shell integration has expired",
            ));
        }
        let observe_foreground = self
            .shell_integrations
            .get(execution_id)
            .is_some_and(|integration| integration.reader.is_some());
        let foreground_process_group = if observe_foreground {
            let execution = self.active.get(execution_id).ok_or_else(not_live)?;
            let terminal = execution.terminal.as_ref().ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::IoModeMismatch,
                    "managed shell integration requires one live PTY execution",
                )
            })?;
            let shell_process_group = i32::try_from(execution.child.id()).map_err(|_| {
                ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "managed shell process identity exceeds Linux pid_t",
                )
            })?;
            platform::terminal_foreground_process_group(terminal, shell_process_group)?
        } else {
            None
        };
        let integration = self
            .shell_integrations
            .get_mut(execution_id)
            .ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "execution has no managed shell integration channel",
                )
            })?;
        let byte_count = maximum_bytes.min(integration.buffered.len());
        let bytes = integration.buffered.drain(..byte_count).collect::<Vec<_>>();
        Ok(ShellIntegrationReadResult {
            execution_id: execution_id.clone(),
            bytes: ProcessBytes::from_bytes(&bytes),
            output_through_sequence: integration.output_through_sequence,
            foreground_process_group,
            channel_closed: integration.closed,
            degraded: integration.degraded,
        })
    }

    fn attach_input(
        &mut self,
        execution_id: &ExecutionId,
        owner: Option<&ExecutionOwner>,
        attachment_id: RequestId,
        writable: bool,
    ) -> Result<InputLease> {
        self.checked_status(execution_id, owner)?;
        self.require_managed_terminal_operator(execution_id, owner)?;
        if !writable {
            return Ok(InputLease {
                attachment_id,
                writable: false,
            });
        }
        self.expire_input_lease_if_needed(execution_id, Instant::now())?;
        let execution = self.active.get_mut(execution_id).ok_or_else(not_live)?;
        if execution.termination.is_some() {
            return Err(not_live());
        }
        if execution.writable_lease.is_some() {
            return Err(ProcessError::new(
                ProcessErrorCode::InputLeaseBusy,
                "execution already has a writable input attachment",
            ));
        }
        let lease = InputLease {
            attachment_id: attachment_id.clone(),
            writable,
        };
        self.repository.bind_input_lease(
            execution_id,
            &self.supervisor_id,
            &lease,
            unix_millis(),
        )?;
        execution.writable_lease = Some(WritableInputLease::new(
            attachment_id.clone(),
            Instant::now(),
        ));
        Ok(InputLease {
            attachment_id,
            writable,
        })
    }

    fn detach_input(
        &mut self,
        execution_id: &ExecutionId,
        owner: Option<&ExecutionOwner>,
        lease: &InputLease,
    ) -> Result<()> {
        self.checked_status(execution_id, owner)?;
        self.require_managed_terminal_operator(execution_id, owner)?;
        if !lease.writable {
            return Ok(());
        }
        if self.expire_input_lease_if_needed(execution_id, Instant::now())? {
            return Err(input_lease_expired());
        }
        let execution = self.active.get_mut(execution_id).ok_or_else(not_live)?;
        match execution.writable_lease.as_ref() {
            Some(current) if current.attachment_id == lease.attachment_id => {
                self.repository.release_input_lease(
                    execution_id,
                    &self.supervisor_id,
                    lease,
                    unix_millis(),
                )?;
                execution.writable_lease = None;
            }
            _ => {
                return Err(ProcessError::new(
                    ProcessErrorCode::InputLeaseBusy,
                    "input attachment does not own the writable lease",
                ));
            }
        }
        Ok(())
    }

    fn renew_input_lease(
        &mut self,
        execution_id: &ExecutionId,
        owner: Option<&ExecutionOwner>,
        lease: &InputLease,
    ) -> Result<()> {
        self.checked_status(execution_id, owner)?;
        self.require_managed_terminal_operator(execution_id, owner)?;
        if !lease.writable {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "read-only attachments do not own a renewable input lease",
            ));
        }
        let now = Instant::now();
        if self.expire_input_lease_if_needed(execution_id, now)? {
            return Err(input_lease_expired());
        }
        let execution = self.active.get_mut(execution_id).ok_or_else(not_live)?;
        let current = execution
            .writable_lease
            .as_mut()
            .filter(|current| current.attachment_id == lease.attachment_id)
            .ok_or_else(input_lease_expired)?;
        self.repository.renew_input_lease(
            execution_id,
            &self.supervisor_id,
            lease,
            unix_millis(),
        )?;
        current.renew(now);
        Ok(())
    }

    fn input_lease_is_active(
        &mut self,
        execution_id: &ExecutionId,
        owner: Option<&ExecutionOwner>,
        lease: &InputLease,
    ) -> Result<bool> {
        self.checked_status(execution_id, owner)?;
        self.require_managed_terminal_operator(execution_id, owner)?;
        if !lease.writable {
            return Ok(true);
        }
        if self.expire_input_lease_if_needed(execution_id, Instant::now())? {
            return Ok(false);
        }
        Ok(self
            .active
            .get(execution_id)
            .and_then(|execution| execution.writable_lease.as_ref())
            .is_some_and(|current| current.attachment_id == lease.attachment_id))
    }

    fn expire_input_lease_if_needed(
        &mut self,
        execution_id: &ExecutionId,
        now: Instant,
    ) -> Result<bool> {
        let expired = self
            .active
            .get(execution_id)
            .and_then(|execution| execution.writable_lease.as_ref())
            .filter(|lease| lease.is_expired(now))
            .cloned();
        let Some(expired) = expired else {
            return Ok(false);
        };
        let lease = expired.as_input_lease();
        let release = self.repository.release_input_lease(
            execution_id,
            &self.supervisor_id,
            &lease,
            unix_millis(),
        );
        if let Some(execution) = self.active.get_mut(execution_id)
            && execution
                .writable_lease
                .as_ref()
                .is_some_and(|current| current.attachment_id == expired.attachment_id)
        {
            execution.writable_lease = None;
        }
        release?;
        Ok(true)
    }

    fn queue_input(
        &mut self,
        execution_id: &ExecutionId,
        owner: Option<&ExecutionOwner>,
        lease: &InputLease,
        bytes: ProcessBytes,
        eof: bool,
    ) -> Result<()> {
        self.checked_status(execution_id, owner)?;
        self.require_managed_terminal_operator(execution_id, owner)?;
        let managed_terminal = self.shell_integrations.contains_key(execution_id);
        if self.expire_input_lease_if_needed(execution_id, Instant::now())? {
            return Err(input_lease_expired());
        }
        let execution = self.active.get_mut(execution_id).ok_or_else(not_live)?;
        if execution.termination.is_some() {
            return Err(not_live());
        }
        if !lease.writable
            || !execution
                .writable_lease
                .as_ref()
                .is_some_and(|current| current.attachment_id == lease.attachment_id)
        {
            return Err(ProcessError::new(
                ProcessErrorCode::InputLeaseBusy,
                "input write requires the current writable attachment lease",
            ));
        }
        let admitted_input_bytes = usize::try_from(execution.request.limits.max_input_bytes)
            .unwrap_or(usize::MAX)
            .min(self.options.max_input_bytes);
        let mut decoded = bytes.decode(admitted_input_bytes)?;
        if eof && execution.terminal.is_some() {
            decoded.push(0x04);
        }
        let new_total = execution
            .accepted_input_bytes
            .saturating_add(decoded.len() as u64);
        if (!managed_terminal && new_total > execution.request.limits.max_input_bytes)
            || queued_bytes(execution).saturating_add(decoded.len()) > admitted_input_bytes
        {
            return Err(ProcessError::new(
                ProcessErrorCode::InputBackpressure,
                "execution input exceeds its admitted lifetime or pending byte limit",
            ));
        }
        self.repository.accept_input(
            execution_id,
            &self.supervisor_id,
            lease,
            decoded.len() as u64,
            eof,
            unix_millis(),
        )?;
        execution.accepted_input_bytes = new_total;
        if !decoded.is_empty() {
            execution.input.push_back(decoded);
        }
        execution.close_stdin_when_drained |= eof && execution.stdin.is_some();
        Ok(())
    }

    fn resize_terminal(
        &mut self,
        execution_id: &ExecutionId,
        owner: Option<&ExecutionOwner>,
        terminal_size: TerminalSize,
    ) -> Result<()> {
        self.checked_status(execution_id, owner)?;
        self.require_managed_terminal_operator(execution_id, owner)?;
        let terminal_size = terminal_size.validate()?;
        let execution = self.active.get_mut(execution_id).ok_or_else(not_live)?;
        if execution.termination.is_some() {
            return Err(not_live());
        }
        let terminal = execution.terminal.as_ref().ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::IoModeMismatch,
                "only a PTY execution can be resized",
            )
        })?;
        let dimensions = libc::winsize {
            ws_row: terminal_size.rows,
            ws_col: terminal_size.columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(terminal.as_raw_fd(), libc::TIOCSWINSZ, &dimensions) } != 0 {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidTerminalSize,
                format!(
                    "failed to resize terminal: {}",
                    std::io::Error::last_os_error()
                ),
            ));
        }
        self.repository.update_terminal_size(
            execution_id,
            &self.supervisor_id,
            execution.next_sequence,
            terminal_size,
            unix_millis(),
        )?;
        execution.next_sequence += 1;
        Ok(())
    }

    fn interrupt_foreground_process_group(
        &self,
        execution_id: &ExecutionId,
        owner: Option<&ExecutionOwner>,
    ) -> Result<()> {
        self.checked_status(execution_id, owner)?;
        self.require_managed_terminal_operator(execution_id, owner)?;
        let terminal = self
            .active
            .get(execution_id)
            .and_then(|execution| execution.terminal.as_ref())
            .ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::IoModeMismatch,
                    "foreground interruption requires one live PTY execution",
                )
            })?;
        platform::interrupt_terminal_foreground(terminal)
    }

    fn handoff_managed_terminal_owner(
        &mut self,
        execution_id: &ExecutionId,
        owner: ExecutionOwner,
        interrupt_foreground: bool,
    ) -> Result<()> {
        self.checked_status(execution_id, None)?;
        if !self.shell_integrations.contains_key(execution_id) {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "only a live managed terminal can change runtime owner",
            ));
        }
        let ExecutionOwner::Session {
            root_run_id: next_root_run_id,
            ..
        } = &owner
        else {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "managed terminal promotion requires a session runtime owner",
            ));
        };
        let execution = self.active.get(execution_id).ok_or_else(not_live)?;
        let ExecutionOwner::Run {
            root_run_id: current_root_run_id,
            ..
        } = &execution.request.owner
        else {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "only a run-owned managed terminal can be promoted",
            ));
        };
        if execution.termination.is_some() {
            return Err(ProcessError::new(
                ProcessErrorCode::ExecutionNotLive,
                "managed terminal teardown won the promotion race",
            ));
        }
        if current_root_run_id != next_root_run_id {
            return Err(ProcessError::new(
                ProcessErrorCode::ExecutionNotOwned,
                "managed terminal promotion cannot cross its root run boundary",
            ));
        }
        let writable_lease = execution
            .writable_lease
            .as_ref()
            .map(WritableInputLease::as_input_lease);
        if let Some(lease) = &writable_lease {
            self.repository.release_input_lease(
                execution_id,
                &self.supervisor_id,
                lease,
                unix_millis(),
            )?;
        }

        let execution = self.active.get_mut(execution_id).ok_or_else(not_live)?;
        execution.writable_lease = None;
        execution.input.clear();
        execution.input_offset = 0;
        execution.close_stdin_when_drained = false;
        if interrupt_foreground {
            let terminal = execution.terminal.as_ref().ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::IoModeMismatch,
                    "managed terminal promotion requires one live PTY execution",
                )
            })?;
            platform::interrupt_terminal_foreground(terminal)?;
        }
        // Durable admission keeps its original owner in this bounded slice.
        // Runtime teardown and control authorization use this private owner;
        // every owner-facing managed-terminal control path is fenced above.
        execution.request.owner = owner;
        Ok(())
    }

    fn kill_execution(
        &mut self,
        execution_id: &ExecutionId,
        owner: Option<&ExecutionOwner>,
        mode: KillMode,
        reason: TerminationReason,
    ) -> Result<()> {
        let status = self.checked_status(execution_id, owner)?;
        self.require_managed_terminal_operator(execution_id, owner)?;
        if !status.state.is_live() {
            return Err(ProcessError::new(
                ProcessErrorCode::ExecutionNotLive,
                "execution is not live",
            ));
        }
        self.begin_termination(execution_id, mode, reason)
    }

    fn active_execution_ids_owned_by(&self, owner: &ExecutionOwner) -> Vec<ExecutionId> {
        self.active
            .iter()
            .filter(|(_, execution)| execution.request.owner.may_access(owner))
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn begin_termination(
        &mut self,
        execution_id: &ExecutionId,
        mode: KillMode,
        reason: TerminationReason,
    ) -> Result<()> {
        let execution = self.active.get_mut(execution_id).ok_or_else(not_live)?;
        if execution.termination.is_some() {
            return Ok(());
        }
        let pid = i32::try_from(execution.child.id()).map_err(|_| {
            ProcessError::new(ProcessErrorCode::Internal, "launcher PID is out of range")
        })?;
        let (signal, grace) = match mode {
            KillMode::Graceful => (libc::SIGTERM, self.options.termination_grace),
            KillMode::Immediate => (libc::SIGKILL, Duration::ZERO),
        };
        if unsafe { libc::kill(pid, signal) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(ProcessError::new(
                    ProcessErrorCode::Internal,
                    format!("failed to signal process launcher: {error}"),
                ));
            }
        }
        execution.input.clear();
        execution.input_offset = 0;
        execution.close_stdin_when_drained = false;
        execution.stdin = None;
        execution.termination = Some(Termination {
            reason,
            force_at: Instant::now() + grace,
            forced: mode == KillMode::Immediate,
        });
        if mode == KillMode::Immediate {
            self.record_lifecycle(execution_id, "forced_termination")?;
        }
        Ok(())
    }

    fn terminate_ids(&mut self, ids: Vec<ExecutionId>, reason: TerminationReason) -> Result<usize> {
        let mut terminated = 0;
        let mut error = None;
        for id in ids {
            match self.begin_termination(&id, KillMode::Graceful, reason) {
                Ok(()) => terminated += 1,
                Err(found) => {
                    error.get_or_insert(found);
                }
            }
        }
        error.map_or(Ok(terminated), Err)
    }

    fn begin_shutdown(&mut self, reply: Option<Reply<()>>) {
        if let Some(reply) = reply {
            self.shutdown_replies.push(reply);
        }
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        let ids = self.active.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            let _ = self.begin_termination(
                &id,
                KillMode::Graceful,
                TerminationReason::SupervisorShutdown,
            );
        }
    }

    fn execution_directories(&self, execution_id: &ExecutionId) -> Result<LaunchDirectories> {
        let root = self
            .options
            .state_root
            .join("executions")
            .join(execution_id.as_str());
        let private_home = root.join("home");
        let private_tmp = root.join("tmp");
        for directory in [&root, &private_home, &private_tmp] {
            ensure_private_directory(directory)?;
        }
        Ok(LaunchDirectories {
            execution_root: root,
            private_home,
            private_tmp,
        })
    }

    fn pump(&mut self) {
        let ids = self.active.keys().cloned().collect::<Vec<_>>();
        let mut finished = Vec::new();
        for id in ids {
            let _ = self.expire_input_lease_if_needed(&id, Instant::now());
            match self.pump_one(&id) {
                Ok(true) => self.pump_shell_integration(&id),
                Ok(false) => {}
                Err(error) => {
                    let reason = if error.code() == ProcessErrorCode::OutputLimitExceeded {
                        TerminationReason::OutputLimit
                    } else {
                        TerminationReason::RuntimeFailure(error.code())
                    };
                    if reason == TerminationReason::OutputLimit
                        && let Some(execution) = self.active.get_mut(&id)
                    {
                        execution.output_truncated = true;
                    }
                    let _ = self.begin_termination(&id, KillMode::Immediate, reason);
                }
            }
            if let Some(execution) = self.active.get_mut(&id) {
                match execution.child.try_wait() {
                    Ok(Some(status)) => finished.push((id.clone(), status)),
                    Ok(None) => {}
                    Err(_) => finished.push((
                        id.clone(),
                        std::process::ExitStatus::from_raw(exit_status(125)),
                    )),
                }
            }
        }
        for (id, status) in finished {
            let _ = self.finish_execution(&id, status);
        }
        self.collect_expired_output();
    }

    fn collect_expired_output(&mut self) {
        if Instant::now() < self.next_retention_scan {
            return;
        }
        self.next_retention_scan = Instant::now() + Duration::from_secs(1);
        let now = unix_millis();
        let Ok(candidates) = self.repository.output_retention_candidates(now, 32) else {
            return;
        };
        for execution_id in candidates {
            if self
                .repository
                .tombstone_output(&execution_id, now)
                .is_err()
            {
                continue;
            }
            if self.spool.remove(&execution_id).is_err() {
                continue;
            }
            if let Some(mut integration) = self.shell_integrations.remove(&execution_id) {
                integration.close(false);
            }
            if remove_private_directory(
                &self
                    .options
                    .state_root
                    .join("executions")
                    .join(execution_id.as_str()),
            )
            .is_err()
            {
                continue;
            }
            let _ = self.repository.mark_output_expired(&execution_id, now);
        }
    }

    fn pump_shell_integration(&mut self, execution_id: &ExecutionId) {
        let output_through_sequence = self
            .active
            .get(execution_id)
            .map_or(0, |execution| execution.next_sequence.saturating_sub(1));
        self.pump_shell_integration_at(execution_id, output_through_sequence);
    }

    fn pump_shell_integration_at(
        &mut self,
        execution_id: &ExecutionId,
        output_through_sequence: u64,
    ) {
        let Some(integration) = self.shell_integrations.get_mut(execution_id) else {
            return;
        };
        if integration.reader.as_ref().is_some_and(|reader| {
            !platform::shell_integration_path_is_intact(&integration.fifo_path, reader)
        }) {
            integration.close(true);
            return;
        }
        for _ in 0..32 {
            let Some(reader) = integration.reader.as_ref() else {
                break;
            };
            let mut bytes = [0u8; 16 * 1024];
            let read =
                unsafe { libc::read(reader.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
            if read > 0 {
                integration.ingest(&bytes[..read as usize], output_through_sequence);
                continue;
            }
            if read == 0 {
                integration.close(true);
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() == std::io::ErrorKind::WouldBlock {
                break;
            }
            integration.close(true);
            break;
        }
    }

    fn pump_one(&mut self, execution_id: &ExecutionId) -> Result<bool> {
        let now = Instant::now();
        let timeout = self
            .active
            .get(execution_id)
            .is_some_and(|execution| execution.deadline.is_some_and(|deadline| now >= deadline));
        if timeout {
            self.begin_termination(
                execution_id,
                KillMode::Graceful,
                TerminationReason::TimedOut,
            )?;
        }
        let mut forced = false;
        if let Some(execution) = self.active.get_mut(execution_id)
            && let Some(termination) = execution.termination.as_mut()
            && !termination.forced
            && now >= termination.force_at
        {
            let pid = i32::try_from(execution.child.id()).unwrap_or(i32::MAX);
            unsafe { libc::kill(pid, libc::SIGKILL) };
            termination.forced = true;
            forced = true;
        }
        if forced {
            self.record_lifecycle(execution_id, "forced_termination")?;
        }

        self.flush_input(execution_id)?;
        let descriptors = self
            .active
            .get(execution_id)
            .map(|execution| {
                [
                    execution
                        .stdout
                        .as_ref()
                        .map(|fd| (fd.as_raw_fd(), ExecutionChannel::Stdout)),
                    execution
                        .stderr
                        .as_ref()
                        .map(|fd| (fd.as_raw_fd(), ExecutionChannel::Stderr)),
                    execution
                        .terminal
                        .as_ref()
                        .map(|fd| (fd.as_raw_fd(), ExecutionChannel::Terminal)),
                ]
            })
            .unwrap_or([None, None, None]);
        let mut output_quiescent = true;
        for (descriptor, channel) in descriptors.into_iter().flatten() {
            let mut descriptor_quiescent = false;
            for _ in 0..8 {
                let mut buffer = [0u8; 16 * 1024];
                let read_bound = self
                    .active
                    .get(execution_id)
                    .map(|execution| {
                        let retained = execution
                            .request
                            .limits
                            .max_output_bytes
                            .min(self.options.max_spool_bytes)
                            .saturating_sub(execution.output_bytes);
                        let headroom = self
                            .options
                            .termination_output_headroom_bytes
                            .saturating_sub(execution.discarded_output_bytes);
                        usize::try_from(retained.saturating_add(headroom))
                            .unwrap_or(usize::MAX)
                            .min(buffer.len())
                            .min(self.options.max_result_bytes)
                    })
                    .unwrap_or(0);
                if read_bound == 0 {
                    break;
                }
                let read =
                    unsafe { libc::read(descriptor, buffer.as_mut_ptr().cast(), read_bound) };
                if read > 0 {
                    self.record_output(execution_id, channel, &buffer[..read as usize])?;
                    continue;
                }
                if read == 0 {
                    self.close_output_descriptor(execution_id, channel);
                    descriptor_quiescent = true;
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    descriptor_quiescent = true;
                    break;
                }
                if channel == ExecutionChannel::Terminal && error.raw_os_error() == Some(libc::EIO)
                {
                    self.close_output_descriptor(execution_id, channel);
                    descriptor_quiescent = true;
                    break;
                }
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(ProcessError::new(
                    ProcessErrorCode::Internal,
                    format!("failed to drain process output: {error}"),
                ));
            }
            output_quiescent &= descriptor_quiescent;
        }
        Ok(output_quiescent)
    }

    fn record_lifecycle(&mut self, execution_id: &ExecutionId, kind: &str) -> Result<()> {
        let sequence = self
            .active
            .get(execution_id)
            .ok_or_else(not_live)?
            .next_sequence;
        let next_sequence = sequence.checked_add(1).ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "execution sequence overflowed while recording lifecycle metadata",
            )
        })?;
        commit_lifecycle_with_confirmation(
            &self.repository,
            execution_id,
            &self.supervisor_id,
            sequence,
            kind,
            unix_millis(),
        )?;
        self.active
            .get_mut(execution_id)
            .ok_or_else(not_live)?
            .next_sequence = next_sequence;
        Ok(())
    }

    fn flush_input(&mut self, execution_id: &ExecutionId) -> Result<()> {
        let Some(execution) = self.active.get_mut(execution_id) else {
            return Ok(());
        };
        if execution.termination.is_some() {
            execution.input.clear();
            execution.input_offset = 0;
            execution.close_stdin_when_drained = false;
            execution.stdin = None;
            return Ok(());
        }
        let descriptor = execution
            .stdin
            .as_ref()
            .or(execution.terminal.as_ref())
            .map(AsRawFd::as_raw_fd);
        let Some(descriptor) = descriptor else {
            return Ok(());
        };
        for _ in 0..8 {
            let Some(front) = execution.input.front() else {
                if execution.close_stdin_when_drained {
                    execution.stdin = None;
                    execution.close_stdin_when_drained = false;
                }
                break;
            };
            let remaining = &front[execution.input_offset..];
            let written =
                unsafe { libc::write(descriptor, remaining.as_ptr().cast(), remaining.len()) };
            if written > 0 {
                execution.input_offset += written as usize;
                if execution.input_offset == front.len() {
                    execution.input.pop_front();
                    execution.input_offset = 0;
                }
                continue;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                break;
            }
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if error.raw_os_error() == Some(libc::EPIPE) {
                execution.stdin = None;
                execution.input.clear();
                break;
            }
            return Err(ProcessError::new(
                ProcessErrorCode::Internal,
                format!("failed to write process input: {error}"),
            ));
        }
        Ok(())
    }

    fn record_output(
        &mut self,
        execution_id: &ExecutionId,
        channel: ExecutionChannel,
        bytes: &[u8],
    ) -> Result<()> {
        let execution = self.active.get_mut(execution_id).ok_or_else(not_live)?;
        let limit = execution
            .request
            .limits
            .max_output_bytes
            .min(self.options.max_spool_bytes);
        let remaining = limit.saturating_sub(execution.output_bytes) as usize;
        let retained = &bytes[..bytes.len().min(remaining)];
        if !retained.is_empty() {
            let chunk = ExecutionOutputChunk {
                sequence: execution.next_sequence,
                channel,
                bytes: ProcessBytes::from_bytes(retained),
            };
            if let Err(error) = append_committed_output(
                &self.spool,
                &self.repository,
                execution_id,
                &self.supervisor_id,
                &chunk,
                unix_millis(),
            ) {
                execution.discarded_output_bytes = execution
                    .discarded_output_bytes
                    .saturating_add(retained.len() as u64);
                execution.output_truncated = true;
                return Err(error);
            }
            execution.next_sequence += 1;
            execution.output_bytes += retained.len() as u64;
        }
        if retained.len() != bytes.len() {
            execution.discarded_output_bytes = execution
                .discarded_output_bytes
                .saturating_add((bytes.len() - retained.len()) as u64);
            execution.output_truncated = true;
            return Err(ProcessError::new(
                ProcessErrorCode::OutputLimitExceeded,
                "execution exceeded its admitted output bound",
            ));
        }
        Ok(())
    }

    fn close_output_descriptor(&mut self, execution_id: &ExecutionId, channel: ExecutionChannel) {
        if let Some(execution) = self.active.get_mut(execution_id) {
            match channel {
                ExecutionChannel::Stdout => execution.stdout = None,
                ExecutionChannel::Stderr => execution.stderr = None,
                ExecutionChannel::Terminal => execution.terminal = None,
                ExecutionChannel::Lifecycle => {}
            }
        }
    }

    fn finish_execution(
        &mut self,
        execution_id: &ExecutionId,
        status: std::process::ExitStatus,
    ) -> Result<()> {
        let Some(mut execution) = self.active.remove(execution_id) else {
            return Ok(());
        };
        let result = (|| {
            let drain = OutputDrainContext {
                spool: &self.spool,
                repository: &self.repository,
                supervisor_id: &self.supervisor_id,
                termination_output_headroom_bytes: self.options.termination_output_headroom_bytes,
                max_result_bytes: self.options.max_result_bytes,
            };
            for channel in [
                ExecutionChannel::Stdout,
                ExecutionChannel::Stderr,
                ExecutionChannel::Terminal,
            ] {
                drain_descriptor(&drain, execution_id, &mut execution, channel)?;
            }
            self.pump_shell_integration_at(execution_id, execution.next_sequence.saturating_sub(1));
            if let Some(integration) = self.shell_integrations.get_mut(execution_id) {
                integration.close(false);
            }

            let (state, exit, error_code) = if let Some(termination) = execution.termination {
                (
                    termination.reason.state(),
                    status_to_exit(status),
                    termination.reason.error_code(),
                )
            } else if let Some(code) = status.code() {
                (
                    ExecutionState::Exited,
                    Some(ExecutionExit::Code { code }),
                    None,
                )
            } else if let Some(signal) = status.signal() {
                (
                    ExecutionState::Signalled,
                    Some(ExecutionExit::Signal { signal }),
                    None,
                )
            } else {
                (
                    ExecutionState::Failed,
                    Some(ExecutionExit::Error {
                        code: ProcessErrorCode::Internal.as_str().to_owned(),
                    }),
                    Some(ProcessErrorCode::Internal.as_str().to_owned()),
                )
            };
            let update = ExecutionTerminalUpdate {
                state,
                exit,
                error_code,
                finished_at_unix_ms: unix_millis(),
                output_truncated: execution.output_truncated,
                discarded_output_bytes: execution.discarded_output_bytes,
            };
            commit_terminal_with_confirmation(
                &self.repository,
                execution_id,
                &self.supervisor_id,
                execution.next_sequence,
                &update,
            )
        })();
        if let Err(error) = result {
            execution.termination = Some(Termination {
                reason: TerminationReason::RuntimeFailure(error.code()),
                force_at: Instant::now(),
                forced: true,
            });
            self.active.insert(execution_id.clone(), execution);
            return Err(error);
        }
        Ok(())
    }
}

impl ActiveExecution {
    fn from_launch(
        request: ExecutionRequest,
        launched: LaunchedProcess,
        deadline: Option<Instant>,
    ) -> Self {
        let mut input = VecDeque::new();
        let initial_input = request
            .stdin
            .as_ref()
            .and_then(|bytes| bytes.decode(request.limits.max_input_bytes as usize).ok());
        if let Some(bytes) = initial_input.as_ref()
            && !bytes.is_empty()
        {
            input.push_back(bytes.clone());
        }
        let close_stdin_when_drained =
            request.close_stdin_after_initial && request.io == crate::ExecutionIo::Pipes;
        if request.close_stdin_after_initial && request.io == crate::ExecutionIo::Pty {
            input.push_back(vec![0x04]);
        }
        Self {
            request,
            child: launched.child,
            stdin: launched.stdin,
            stdout: launched.stdout,
            stderr: launched.stderr,
            terminal: launched.terminal,
            next_sequence: 3,
            output_bytes: 0,
            discarded_output_bytes: 0,
            output_truncated: false,
            accepted_input_bytes: initial_input.as_ref().map_or(0, |bytes| bytes.len() as u64),
            input,
            input_offset: 0,
            close_stdin_when_drained,
            writable_lease: None,
            deadline,
            termination: None,
        }
    }
}

impl ActiveShellIntegration {
    fn new(transport: ManagedShellIntegrationTransport) -> Self {
        Self {
            reader: Some(transport.reader),
            fifo_path: transport.fifo_path,
            buffered: VecDeque::new(),
            closed: false,
            degraded: false,
            output_through_sequence: 0,
        }
    }

    fn ingest(&mut self, bytes: &[u8], output_through_sequence: u64) {
        if self.degraded {
            return;
        }
        let remaining = MAX_BUFFERED_SHELL_INTEGRATION_BYTES.saturating_sub(self.buffered.len());
        let retained = remaining.min(bytes.len());
        self.buffered.extend(&bytes[..retained]);
        if retained != 0 {
            self.output_through_sequence = output_through_sequence;
        }
        if retained != bytes.len() {
            self.degraded = true;
        }
    }

    /// Unlink before closing the supervisor's O_RDWR endpoint. A shell hook
    /// that races channel loss then fails its open instead of blocking forever
    /// on a FIFO path with no reader.
    fn close(&mut self, degraded: bool) {
        self.degraded |= degraded;
        let reader = self.reader.take();
        match fs::remove_file(&self.fifo_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => self.degraded = true,
        }
        drop(reader);
        self.closed = true;
    }
}

struct OutputDrainContext<'a> {
    spool: &'a Arc<dyn OutputSpool>,
    repository: &'a Arc<dyn ExecutionRepository>,
    supervisor_id: &'a str,
    termination_output_headroom_bytes: u64,
    max_result_bytes: usize,
}

fn drain_descriptor(
    context: &OutputDrainContext<'_>,
    execution_id: &ExecutionId,
    execution: &mut ActiveExecution,
    channel: ExecutionChannel,
) -> Result<()> {
    let descriptor = match channel {
        ExecutionChannel::Stdout => execution.stdout.as_ref(),
        ExecutionChannel::Stderr => execution.stderr.as_ref(),
        ExecutionChannel::Terminal => execution.terminal.as_ref(),
        ExecutionChannel::Lifecycle => None,
    };
    let Some(descriptor) = descriptor else {
        return Ok(());
    };
    loop {
        let retained_remaining = execution
            .request
            .limits
            .max_output_bytes
            .saturating_sub(execution.output_bytes);
        let headroom_remaining = context
            .termination_output_headroom_bytes
            .saturating_sub(execution.discarded_output_bytes);
        if retained_remaining == 0 && headroom_remaining == 0 {
            execution.output_truncated = true;
            break;
        }
        let mut buffer = [0u8; 16 * 1024];
        let read_bound = buffer
            .len()
            .min(
                usize::try_from(retained_remaining.saturating_add(headroom_remaining))
                    .unwrap_or(usize::MAX),
            )
            .min(context.max_result_bytes);
        let read = unsafe {
            libc::read(
                descriptor.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                read_bound,
            )
        };
        if read <= 0 {
            break;
        }
        let limit = execution.request.limits.max_output_bytes;
        let remaining = limit.saturating_sub(execution.output_bytes) as usize;
        let retained = &buffer[..(read as usize).min(remaining)];
        if !retained.is_empty() {
            let chunk = ExecutionOutputChunk {
                sequence: execution.next_sequence,
                channel,
                bytes: ProcessBytes::from_bytes(retained),
            };
            if let Err(error) = append_committed_output(
                context.spool,
                context.repository,
                execution_id,
                context.supervisor_id,
                &chunk,
                unix_millis(),
            ) {
                execution.discarded_output_bytes = execution
                    .discarded_output_bytes
                    .saturating_add(retained.len() as u64);
                execution.output_truncated = true;
                return Err(error);
            }
            execution.next_sequence += 1;
            execution.output_bytes += retained.len() as u64;
        }
        if retained.len() != read as usize {
            execution.discarded_output_bytes = execution
                .discarded_output_bytes
                .saturating_add(read as u64 - retained.len() as u64);
            execution.output_truncated = true;
            execution.termination = Some(Termination {
                reason: TerminationReason::OutputLimit,
                force_at: Instant::now(),
                forced: true,
            });
        }
    }
    Ok(())
}

fn append_committed_output(
    spool: &Arc<dyn OutputSpool>,
    repository: &Arc<dyn ExecutionRepository>,
    execution_id: &ExecutionId,
    supervisor_id: &str,
    chunk: &ExecutionOutputChunk,
    occurred_at_unix_ms: i64,
) -> Result<()> {
    let offset = match spool.append(execution_id, chunk) {
        Ok(offset) => offset,
        Err(error) => return rollback_uncommitted_output(spool, repository, execution_id, error),
    };
    if let Err(error) = spool.sync(execution_id) {
        return rollback_uncommitted_output(spool, repository, execution_id, error);
    }
    let expected = CommittedOutputFrame::from_chunk(chunk, offset)?;
    match repository.append_indexed_chunk(
        execution_id,
        supervisor_id,
        chunk,
        offset,
        expected.byte_length,
        occurred_at_unix_ms,
    ) {
        Ok(()) => Ok(()),
        Err(error) => {
            let committed = repository.committed_output_frames(execution_id)?;
            if committed.last() == Some(&expected) {
                return Ok(());
            }
            spool.recover(execution_id, &committed)?;
            Err(error)
        }
    }
}

fn rollback_uncommitted_output(
    spool: &Arc<dyn OutputSpool>,
    repository: &Arc<dyn ExecutionRepository>,
    execution_id: &ExecutionId,
    original: ProcessError,
) -> Result<()> {
    let committed = repository.committed_output_frames(execution_id)?;
    spool.recover(execution_id, &committed)?;
    Err(original)
}

fn commit_running_with_confirmation(
    repository: &Arc<dyn ExecutionRepository>,
    execution_id: &ExecutionId,
    supervisor_id: &str,
    started_at_unix_ms: i64,
) -> Result<()> {
    match repository.mark_running(execution_id, supervisor_id, started_at_unix_ms) {
        Ok(()) => Ok(()),
        Err(error) => match repository.status(execution_id) {
            Ok(status) if status.state == ExecutionState::Running => Ok(()),
            _ => Err(error),
        },
    }
}

fn commit_lifecycle_with_confirmation(
    repository: &Arc<dyn ExecutionRepository>,
    execution_id: &ExecutionId,
    supervisor_id: &str,
    sequence: u64,
    kind: &str,
    occurred_at_unix_ms: i64,
) -> Result<()> {
    match repository.append_lifecycle(
        execution_id,
        supervisor_id,
        sequence,
        kind,
        occurred_at_unix_ms,
    ) {
        Ok(()) => Ok(()),
        Err(error) => match repository.status(execution_id) {
            Ok(status) if status.last_sequence >= sequence => Ok(()),
            _ => Err(error),
        },
    }
}

fn commit_terminal_with_confirmation(
    repository: &Arc<dyn ExecutionRepository>,
    execution_id: &ExecutionId,
    supervisor_id: &str,
    sequence: u64,
    update: &ExecutionTerminalUpdate,
) -> Result<()> {
    match repository.mark_terminal(execution_id, supervisor_id, sequence, update) {
        Ok(()) => Ok(()),
        Err(error) => match repository.status(execution_id) {
            Ok(status) if status.state.is_terminal() && status.last_sequence == sequence => Ok(()),
            _ => Err(error),
        },
    }
}

fn status_to_exit(status: std::process::ExitStatus) -> Option<ExecutionExit> {
    status
        .code()
        .map(|code| ExecutionExit::Code { code })
        .or_else(|| {
            status
                .signal()
                .map(|signal| ExecutionExit::Signal { signal })
        })
}

fn queued_bytes(execution: &ActiveExecution) -> usize {
    execution
        .input
        .iter()
        .enumerate()
        .map(|(index, bytes)| {
            if index == 0 {
                bytes.len().saturating_sub(execution.input_offset)
            } else {
                bytes.len()
            }
        })
        .sum()
}

fn grant_lease_is_inactive(
    lease: Option<&crate::ExecutionGrantLease>,
    live_grant_ids: &BTreeSet<String>,
) -> bool {
    lease.is_some_and(|lease| {
        lease.is_capability_grant() && !live_grant_ids.contains(&lease.grant_id)
    })
}

fn ensure_private_directory(path: &std::path::Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                "private execution path is not a real directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|error| {
                ProcessError::new(
                    ProcessErrorCode::StoreCorrupt,
                    format!("failed to create private execution directory: {error}"),
                )
            })?;
        }
        Err(error) => {
            return Err(ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                format!("failed to inspect private execution directory: {error}"),
            ));
        }
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            format!("failed to protect private execution directory: {error}"),
        )
    })?;
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            format!("failed to verify private execution directory: {error}"),
        )
    })?;
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "private execution directory owner or permissions are invalid",
        ));
    }
    Ok(())
}

fn remove_private_directory(path: &std::path::Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                format!("failed to inspect private execution directory: {error}"),
            ));
        }
    };
    use std::os::unix::fs::MetadataExt;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "private execution directory cannot be removed safely",
        ));
    }
    fs::remove_dir_all(path).map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            format!("failed to remove private execution directory: {error}"),
        )
    })
}

fn not_live() -> ProcessError {
    ProcessError::new(ProcessErrorCode::ExecutionNotLive, "execution is not live")
}

fn input_lease_expired() -> ProcessError {
    ProcessError::new(
        ProcessErrorCode::InputLeaseExpired,
        "writable input lease expired or is no longer owned by this attachment",
    )
}

fn supervisor_shutdown() -> ProcessError {
    ProcessError::new(
        ProcessErrorCode::SupervisorShutdown,
        "process supervisor is shut down",
    )
}

fn admission_cancelled() -> ProcessError {
    ProcessError::new(
        ProcessErrorCode::Cancelled,
        "process admission was cancelled before the target became running",
    )
}

fn admission_timed_out() -> ProcessError {
    ProcessError::new(
        ProcessErrorCode::TimedOut,
        "process admission exceeded its durable deadline before the target became running",
    )
}

fn admission_interrupted(timed_out: &AtomicBool) -> ProcessError {
    if timed_out.load(Ordering::Acquire) {
        admission_timed_out()
    } else {
        admission_cancelled()
    }
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn exit_status(code: i32) -> i32 {
    code << 8
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ManagedReactorFixture {
        reactor: Reactor,
        execution_id: ExecutionId,
        run_owner: ExecutionOwner,
        session_owner: ExecutionOwner,
        root: std::path::PathBuf,
    }

    impl Drop for ManagedReactorFixture {
        fn drop(&mut self) {
            if let Some(mut integration) =
                self.reactor.shell_integrations.remove(&self.execution_id)
            {
                integration.close(false);
            }
            if let Some(mut execution) = self.reactor.active.remove(&self.execution_id) {
                let _ = execution.child.kill();
                let _ = execution.child.wait();
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn managed_reactor_fixture(max_input_bytes: u64) -> ManagedReactorFixture {
        let root = std::env::temp_dir().join(format!(
            "agl-managed-reactor-{}-{}",
            std::process::id(),
            ExecutionId::generate()
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let repository: Arc<dyn ExecutionRepository> =
            Arc::new(crate::InMemoryExecutionRepository::new());
        let spool: Arc<dyn OutputSpool> =
            Arc::new(crate::FileOutputSpool::new(root.join("spool")).unwrap());
        let root_run_id = agl_ids::RunId::generate();
        let run_owner = ExecutionOwner::Run {
            run_id: agl_ids::RunId::generate(),
            root_run_id: root_run_id.clone(),
        };
        let session_owner = ExecutionOwner::Session {
            session_id: agl_ids::SessionId::generate(),
            root_run_id,
        };
        let request = ExecutionRequest {
            owner: run_owner.clone(),
            creating_run_id: agl_ids::RunId::generate(),
            creating_step_id: agl_ids::StepId::generate(),
            kind: crate::ExecutionKind::Shell,
            program: std::path::PathBuf::from("/bin/sh"),
            program_digest: None,
            args: Vec::new(),
            workspace_root: workspace.clone(),
            cwd: workspace.clone(),
            read_only_roots: Vec::new(),
            environment: crate::EnvironmentOverride {
                values: BTreeMap::new(),
            },
            stdin: None,
            close_stdin_after_initial: false,
            io: crate::ExecutionIo::Pty,
            terminal_size: Some(TerminalSize::default()),
            profile: crate::ExecutionProfile::Workspace,
            authorization: crate::ExecutionAuthorization::default(),
            grant_lease: None,
            limits: crate::ExecutionLimits {
                timeout_ms: None,
                max_input_bytes,
                max_output_bytes: 4096,
            },
        };
        let execution_id = ExecutionId::generate();
        let status = ExecutionStatus {
            execution_id: execution_id.clone(),
            owner: run_owner.clone(),
            state: ExecutionState::Running,
            profile: crate::ExecutionProfile::Workspace,
            io: crate::ExecutionIo::Pty,
            cwd: workspace,
            terminal_size: Some(TerminalSize::default()),
            exit: None,
            first_retained_sequence: None,
            last_sequence: 0,
            retained_bytes: 0,
            discarded_output_bytes: 0,
            output_truncated: false,
            output_expired: false,
            started_at_unix_ms: Some(1),
            finished_at_unix_ms: None,
            error_code: None,
        };
        repository.admit(&status, &request, "test-owner").unwrap();
        spool.prepare(&execution_id).unwrap();
        let child = std::process::Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let terminal = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap()
            .into();
        let execution = ActiveExecution::from_launch(
            request,
            LaunchedProcess {
                child,
                stdin: None,
                stdout: None,
                stderr: None,
                terminal: Some(terminal),
            },
            None,
        );
        let options = ProcessSupervisorOptions {
            launcher_path: root.join("unused-launcher"),
            data_root: root.join("data"),
            state_root: root.join("state"),
            max_active: 2,
            command_capacity: 8,
            poll_interval: Duration::from_millis(1),
            setup_timeout: Duration::from_millis(100),
            termination_grace: Duration::from_millis(10),
            max_input_bytes: usize::try_from(max_input_bytes).unwrap(),
            max_result_bytes: 1024,
            max_spool_bytes: 4096,
            termination_output_headroom_bytes: 1024,
            finished_retention: Duration::from_secs(60),
            runtime_read_only_roots: Vec::new(),
        };
        let mut reactor = Reactor {
            options,
            repository,
            spool,
            supervisor_id: "test-owner".to_owned(),
            active: BTreeMap::from([(execution_id.clone(), execution)]),
            shell_integrations: BTreeMap::new(),
            shutting_down: false,
            shutdown_replies: Vec::new(),
            next_retention_scan: Instant::now() + Duration::from_secs(60),
        };
        reactor.shell_integrations.insert(
            execution_id.clone(),
            ActiveShellIntegration {
                reader: None,
                fifo_path: root.join("absent-integration.fifo"),
                buffered: VecDeque::new(),
                closed: false,
                degraded: false,
                output_through_sequence: 0,
            },
        );
        ManagedReactorFixture {
            reactor,
            execution_id,
            run_owner,
            session_owner,
            root,
        }
    }

    fn managed_shell_fixture(
        workspace: &std::path::Path,
    ) -> (ExecutionRequest, ManagedShellStartup) {
        let program = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join("bash"))
            .find(|candidate| candidate.is_file())
            .expect("supervisor tests require Bash")
            .canonicalize()
            .unwrap();
        let shell = crate::terminal::shell::AdmittedShellProfile {
            kind: crate::terminal::shell::AdmittedShellKind::Bash,
            snapshot: crate::ShellProfileSnapshot {
                program: program.clone(),
                command_args: vec!["-c".to_owned()],
                login_command_args: None,
                environment_names: vec!["PATH".to_owned()],
                executable_digest:
                    "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                        .to_owned(),
                config_digest: "sha256:test-bash".to_owned(),
            },
        };
        let root_run_id = agl_ids::RunId::generate();
        let request = ExecutionRequest {
            owner: ExecutionOwner::Session {
                session_id: agl_ids::SessionId::generate(),
                root_run_id: root_run_id.clone(),
            },
            creating_run_id: root_run_id,
            creating_step_id: agl_ids::StepId::generate(),
            kind: crate::ExecutionKind::Shell,
            program,
            program_digest: Some(shell.snapshot.executable_digest.clone()),
            args: Vec::new(),
            workspace_root: workspace.to_path_buf(),
            cwd: workspace.to_path_buf(),
            read_only_roots: Vec::new(),
            environment: crate::EnvironmentOverride {
                values: BTreeMap::new(),
            },
            stdin: None,
            close_stdin_after_initial: false,
            io: crate::ExecutionIo::Pty,
            terminal_size: Some(TerminalSize::default()),
            profile: crate::ExecutionProfile::Workspace,
            authorization: crate::ExecutionAuthorization::default(),
            grant_lease: None,
            limits: crate::ExecutionLimits {
                timeout_ms: None,
                max_input_bytes: 1024,
                max_output_bytes: 4096,
            },
        };
        let startup = ManagedShellStartup {
            shell,
            host_startup: crate::terminal::shell::HostStartupPolicy::ManagedOnly,
            history_seed: crate::terminal::history::TerminalHistorySeed::empty(),
            integration_token: crate::terminal::shell::ShellIntegrationToken::generate().unwrap(),
            private_environment: crate::terminal::environment::PrivateTerminalEnvironment::default(
            ),
        };
        (request, startup)
    }

    #[test]
    fn termination_reasons_have_distinct_durable_states() {
        assert_eq!(
            TerminationReason::Cancelled.state(),
            ExecutionState::Cancelled
        );
        assert_eq!(
            TerminationReason::TimedOut.state(),
            ExecutionState::TimedOut
        );
        assert_eq!(
            TerminationReason::OutputLimit.error_code().as_deref(),
            Some("output_limit_exceeded")
        );
    }

    #[test]
    fn writable_input_lease_deadline_is_exact_and_renewal_replaces_it() {
        let started = Instant::now();
        let mut lease = WritableInputLease::new(RequestId::generate(), started);
        assert!(!lease.is_expired(started + WRITABLE_INPUT_LEASE_TTL - Duration::from_nanos(1)));
        assert!(lease.is_expired(started + WRITABLE_INPUT_LEASE_TTL));

        let renewed = started + crate::WRITABLE_INPUT_LEASE_HEARTBEAT;
        lease.renew(renewed);
        assert!(!lease.is_expired(started + WRITABLE_INPUT_LEASE_TTL));
        assert!(lease.is_expired(renewed + WRITABLE_INPUT_LEASE_TTL));
        assert!(lease.as_input_lease().writable);
    }

    #[test]
    fn active_reserved_execution_identity_is_rejected_before_private_materialization() {
        let mut fixture = managed_reactor_fixture(65_536);
        let execution_id = fixture.execution_id.clone();
        let workspace = fixture.root.join("workspace").canonicalize().unwrap();
        let (request, startup) = managed_shell_fixture(&workspace);
        let execution_root = fixture
            .reactor
            .options
            .state_root
            .join("executions")
            .join(execution_id.as_str());
        let spool_path = fixture
            .root
            .join("spool")
            .join(execution_id.as_str())
            .join("stream.aglspool");
        let spool_before = fs::read(&spool_path).unwrap();
        let cancelled = AtomicBool::new(false);
        let timed_out = AtomicBool::new(false);

        let error = fixture
            .reactor
            .start_execution(
                request,
                Some(execution_id.clone()),
                Some(startup),
                &cancelled,
                &timed_out,
            )
            .unwrap_err();

        assert_eq!(error.code(), ProcessErrorCode::StateConflict);
        assert!(!execution_root.exists());
        assert_eq!(fs::read(spool_path).unwrap(), spool_before);
        assert!(
            fixture
                .reactor
                .active
                .get_mut(&execution_id)
                .unwrap()
                .child
                .try_wait()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn managed_terminal_control_is_operator_only_and_promotion_handoffs_runtime_owner() {
        let mut fixture = managed_reactor_fixture(65_536);
        let execution_id = fixture.execution_id.clone();
        let run_owner = fixture.run_owner.clone();
        let session_owner = fixture.session_owner.clone();

        assert_eq!(
            fixture
                .reactor
                .attach_input(&execution_id, Some(&run_owner), RequestId::generate(), true,)
                .unwrap_err()
                .code(),
            ProcessErrorCode::ExecutionNotOwned
        );
        fixture
            .reactor
            .read_output(
                &execution_id,
                Some(&run_owner),
                ExecutionCursor { after_sequence: 0 },
                1,
            )
            .unwrap();
        fixture
            .reactor
            .read_shell_integration(&execution_id, None, 1)
            .unwrap();
        assert_eq!(
            fixture
                .reactor
                .read_shell_integration(&execution_id, Some(&run_owner), 1)
                .unwrap_err()
                .code(),
            ProcessErrorCode::ExecutionNotOwned
        );
        assert_eq!(
            fixture
                .reactor
                .resize_terminal(&execution_id, Some(&run_owner), TerminalSize::default(),)
                .unwrap_err()
                .code(),
            ProcessErrorCode::ExecutionNotOwned
        );
        assert_eq!(
            fixture
                .reactor
                .interrupt_foreground_process_group(&execution_id, Some(&run_owner))
                .unwrap_err()
                .code(),
            ProcessErrorCode::ExecutionNotOwned
        );
        assert_eq!(
            fixture
                .reactor
                .kill_execution(
                    &execution_id,
                    Some(&run_owner),
                    KillMode::Immediate,
                    TerminationReason::Cancelled,
                )
                .unwrap_err()
                .code(),
            ProcessErrorCode::ExecutionNotOwned
        );
        let run_owned = fixture.reactor.active_execution_ids_owned_by(&run_owner);
        assert_eq!(run_owned.as_slice(), std::slice::from_ref(&execution_id));

        let lease = fixture
            .reactor
            .attach_input(&execution_id, None, RequestId::generate(), true)
            .unwrap();
        assert_eq!(
            fixture
                .reactor
                .queue_input(
                    &execution_id,
                    Some(&run_owner),
                    &lease,
                    ProcessBytes::from_bytes(b"forged"),
                    false,
                )
                .unwrap_err()
                .code(),
            ProcessErrorCode::ExecutionNotOwned
        );
        fixture
            .reactor
            .queue_input(
                &execution_id,
                None,
                &lease,
                ProcessBytes::from_bytes(b"operator"),
                false,
            )
            .unwrap();
        fixture
            .reactor
            .handoff_managed_terminal_owner(&execution_id, session_owner.clone(), false)
            .unwrap();

        assert!(
            !fixture
                .reactor
                .input_lease_is_active(&execution_id, None, &lease)
                .unwrap()
        );
        assert!(fixture.reactor.active[&execution_id].input.is_empty());
        assert_eq!(
            fixture.reactor.active[&execution_id].request.owner,
            session_owner
        );
        assert!(
            fixture
                .reactor
                .active_execution_ids_owned_by(&run_owner)
                .is_empty()
        );
        assert_eq!(
            fixture
                .reactor
                .terminate_ids(
                    fixture.reactor.active_execution_ids_owned_by(&run_owner),
                    TerminationReason::Cancelled,
                )
                .unwrap(),
            0
        );
        assert!(fixture.reactor.active[&execution_id].termination.is_none());
        assert_eq!(
            fixture
                .reactor
                .kill_execution(
                    &execution_id,
                    Some(&run_owner),
                    KillMode::Immediate,
                    TerminationReason::Cancelled,
                )
                .unwrap_err()
                .code(),
            ProcessErrorCode::ExecutionNotOwned
        );
        fixture
            .reactor
            .kill_execution(
                &execution_id,
                None,
                KillMode::Graceful,
                TerminationReason::Cancelled,
            )
            .unwrap();
        assert!(fixture.reactor.active[&execution_id].termination.is_some());

        let mut unpromoted = managed_reactor_fixture(65_536);
        let unpromoted_execution_id = unpromoted.execution_id.clone();
        let ids = unpromoted
            .reactor
            .active_execution_ids_owned_by(&unpromoted.run_owner);
        assert_eq!(
            ids.as_slice(),
            std::slice::from_ref(&unpromoted_execution_id)
        );
        assert_eq!(
            unpromoted
                .reactor
                .terminate_ids(ids, TerminationReason::Cancelled)
                .unwrap(),
            1
        );
        assert!(
            unpromoted.reactor.active[&unpromoted_execution_id]
                .termination
                .is_some()
        );
        assert_eq!(
            unpromoted
                .reactor
                .handoff_managed_terminal_owner(
                    &unpromoted_execution_id,
                    unpromoted.session_owner.clone(),
                    false,
                )
                .unwrap_err()
                .code(),
            ProcessErrorCode::ExecutionNotLive
        );
    }

    #[test]
    fn managed_terminal_input_is_pending_bounded_not_lifetime_bounded() {
        const LIMIT: usize = 65_536;
        let mut fixture = managed_reactor_fixture(LIMIT as u64);
        let execution_id = fixture.execution_id.clone();
        let lease = fixture
            .reactor
            .attach_input(&execution_id, None, RequestId::generate(), true)
            .unwrap();
        let chunk = vec![b'x'; LIMIT / 2];
        for _ in 0..3 {
            fixture
                .reactor
                .queue_input(
                    &execution_id,
                    None,
                    &lease,
                    ProcessBytes::from_bytes(&chunk),
                    false,
                )
                .unwrap();
            let execution = fixture.reactor.active.get_mut(&execution_id).unwrap();
            execution.input.clear();
            execution.input_offset = 0;
        }
        assert_eq!(
            fixture.reactor.active[&execution_id].accepted_input_bytes,
            (LIMIT + LIMIT / 2) as u64
        );
        assert_eq!(
            fixture
                .reactor
                .queue_input(
                    &execution_id,
                    None,
                    &lease,
                    ProcessBytes::from_bytes(&vec![b'x'; LIMIT + 1]),
                    false,
                )
                .unwrap_err()
                .code(),
            ProcessErrorCode::InputTooLarge
        );

        fixture.reactor.shell_integrations.remove(&execution_id);
        let execution = fixture.reactor.active.get_mut(&execution_id).unwrap();
        execution.accepted_input_bytes = 0;
        execution.input.clear();
        fixture
            .reactor
            .queue_input(
                &execution_id,
                None,
                &lease,
                ProcessBytes::from_bytes(&chunk),
                false,
            )
            .unwrap();
        let execution = fixture.reactor.active.get_mut(&execution_id).unwrap();
        execution.input.clear();
        execution.input_offset = 0;
        assert_eq!(
            fixture
                .reactor
                .queue_input(
                    &execution_id,
                    None,
                    &lease,
                    ProcessBytes::from_bytes(&vec![b'y'; LIMIT / 2 + 1]),
                    false,
                )
                .unwrap_err()
                .code(),
            ProcessErrorCode::InputBackpressure
        );
    }

    #[test]
    fn private_shell_integration_buffer_is_bounded_and_unlinks_before_close() {
        let root = std::env::temp_dir().join(format!(
            "agl-supervisor-shell-integration-{}-{}",
            std::process::id(),
            ExecutionId::generate()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let fifo_path = root.join("integration.fifo");
        let reader = platform::create_shell_integration_reader(&fifo_path).unwrap();
        let mut integration = ActiveShellIntegration::new(ManagedShellIntegrationTransport {
            reader,
            fifo_path: fifo_path.clone(),
        });

        integration.ingest(&vec![0x41; MAX_BUFFERED_SHELL_INTEGRATION_BYTES + 1], 17);

        assert_eq!(
            integration.buffered.len(),
            MAX_BUFFERED_SHELL_INTEGRATION_BYTES
        );
        assert!(integration.degraded);
        assert_eq!(integration.output_through_sequence, 17);
        integration.close(false);
        assert!(integration.closed);
        assert!(integration.reader.is_none());
        assert!(!fifo_path.exists());
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn grant_reconciliation_ignores_workspace_processes_and_fences_missing_leases() {
        let live = BTreeSet::from(["grant-live".to_owned()]);
        let live_lease = crate::ExecutionGrantLease {
            origin: crate::ExecutionLeaseOrigin::CapabilityGrant,
            grant_id: "grant-live".to_owned(),
            duration: "session".to_owned(),
            scope_digest: "sha256:live".to_owned(),
        };
        let stale_lease = crate::ExecutionGrantLease {
            origin: crate::ExecutionLeaseOrigin::CapabilityGrant,
            grant_id: "grant-stale".to_owned(),
            duration: "one_turn".to_owned(),
            scope_digest: "sha256:stale".to_owned(),
        };
        let local_operator_lease = crate::ExecutionGrantLease {
            origin: crate::ExecutionLeaseOrigin::LocalOperatorTerminal,
            grant_id: "private-operator-authority".to_owned(),
            duration: crate::LOCAL_OPERATOR_TERMINAL_LEASE_DURATION.to_owned(),
            scope_digest: "sha256:terminal".to_owned(),
        };

        assert!(!grant_lease_is_inactive(None, &live));
        assert!(!grant_lease_is_inactive(Some(&live_lease), &live));
        assert!(grant_lease_is_inactive(Some(&stale_lease), &live));
        assert!(!grant_lease_is_inactive(Some(&local_operator_lease), &live));
    }

    #[test]
    fn output_and_terminal_commits_reconcile_post_commit_faults_and_rollback_orphans() {
        let root = std::env::temp_dir().join(format!(
            "agl-process-commit-recovery-{}-{}",
            std::process::id(),
            ExecutionId::generate()
        ));
        let workspace = std::env::temp_dir().canonicalize().unwrap();
        let spool_impl = Arc::new(crate::FileOutputSpool::new(&root).unwrap());
        let spool: Arc<dyn OutputSpool> = spool_impl.clone();
        let repository_impl = Arc::new(crate::InMemoryExecutionRepository::new());
        let repository: Arc<dyn ExecutionRepository> = repository_impl.clone();
        let execution_id = ExecutionId::generate();
        let run_id = agl_ids::RunId::generate();
        let owner = crate::ExecutionOwner::Run {
            run_id: run_id.clone(),
            root_run_id: run_id.clone(),
        };
        let status = ExecutionStatus {
            execution_id: execution_id.clone(),
            owner: owner.clone(),
            state: ExecutionState::Starting,
            profile: crate::ExecutionProfile::Workspace,
            io: crate::ExecutionIo::Pipes,
            cwd: workspace.clone(),
            terminal_size: None,
            exit: None,
            first_retained_sequence: None,
            last_sequence: 0,
            retained_bytes: 0,
            discarded_output_bytes: 0,
            output_truncated: false,
            output_expired: false,
            started_at_unix_ms: None,
            finished_at_unix_ms: None,
            error_code: None,
        };
        let request = ExecutionRequest {
            owner,
            creating_run_id: run_id,
            creating_step_id: agl_ids::StepId::generate(),
            kind: crate::ExecutionKind::Argv,
            program: std::path::PathBuf::from("/bin/echo"),
            program_digest: None,
            args: Vec::new(),
            workspace_root: workspace.clone(),
            cwd: workspace,
            read_only_roots: Vec::new(),
            environment: crate::EnvironmentOverride {
                values: BTreeMap::new(),
            },
            stdin: None,
            close_stdin_after_initial: true,
            io: crate::ExecutionIo::Pipes,
            terminal_size: None,
            profile: crate::ExecutionProfile::Workspace,
            authorization: crate::ExecutionAuthorization::default(),
            grant_lease: None,
            limits: crate::ExecutionLimits {
                timeout_ms: None,
                max_input_bytes: 1024,
                max_output_bytes: 4096,
            },
        };
        spool.prepare(&execution_id).unwrap();
        repository.admit(&status, &request, "owner").unwrap();
        repository_impl.fail_next_lifecycle_after_commit();
        commit_lifecycle_with_confirmation(&repository, &execution_id, "owner", 1, "admitted", 1)
            .unwrap();
        repository_impl.fail_next_running_after_commit();
        commit_running_with_confirmation(&repository, &execution_id, "owner", 2).unwrap();
        repository_impl.fail_next_lifecycle_after_commit();
        commit_lifecycle_with_confirmation(&repository, &execution_id, "owner", 2, "running", 2)
            .unwrap();

        let committed = ExecutionOutputChunk {
            sequence: 3,
            channel: ExecutionChannel::Stdout,
            bytes: ProcessBytes::from_bytes(b"committed"),
        };
        repository_impl.fail_next_output_after_commit();
        append_committed_output(&spool, &repository, &execution_id, "owner", &committed, 3)
            .unwrap();
        assert_eq!(
            repository
                .committed_output_frames(&execution_id)
                .unwrap()
                .len(),
            1
        );

        let orphan = ExecutionOutputChunk {
            sequence: 4,
            channel: ExecutionChannel::Stderr,
            bytes: ProcessBytes::from_bytes(b"orphan"),
        };
        assert_eq!(
            append_committed_output(
                &spool,
                &repository,
                &execution_id,
                "stale-owner",
                &orphan,
                4,
            )
            .unwrap_err()
            .code(),
            ProcessErrorCode::StateConflict
        );
        assert_eq!(spool.read(&execution_id, 0, 4, 64).unwrap(), [committed]);

        let terminal = ExecutionTerminalUpdate {
            state: ExecutionState::Exited,
            exit: Some(ExecutionExit::Code { code: 0 }),
            error_code: None,
            finished_at_unix_ms: 5,
            output_truncated: false,
            discarded_output_bytes: 0,
        };
        repository_impl.fail_next_terminal_after_commit();
        commit_terminal_with_confirmation(&repository, &execution_id, "owner", 4, &terminal)
            .unwrap();
        assert_eq!(
            repository.status(&execution_id).unwrap().state,
            ExecutionState::Exited
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn post_spawn_admission_failure_keeps_child_owned_until_terminal_commit() {
        struct KillOnDrop(u32);

        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                if let Ok(pid) = i32::try_from(self.0) {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }

        let root = std::env::temp_dir().join(format!(
            "agl-process-post-spawn-failure-{}-{}",
            std::process::id(),
            ExecutionId::generate()
        ));
        let workspace = std::env::temp_dir().canonicalize().unwrap();
        let spool_impl = Arc::new(crate::FileOutputSpool::new(root.join("spool")).unwrap());
        let spool: Arc<dyn OutputSpool> = spool_impl.clone();
        let repository_impl = Arc::new(crate::InMemoryExecutionRepository::new());
        let repository: Arc<dyn ExecutionRepository> = repository_impl.clone();
        let execution_id = ExecutionId::generate();
        let run_id = agl_ids::RunId::generate();
        let owner = crate::ExecutionOwner::Run {
            run_id: run_id.clone(),
            root_run_id: run_id.clone(),
        };
        let status = ExecutionStatus {
            execution_id: execution_id.clone(),
            owner: owner.clone(),
            state: ExecutionState::Starting,
            profile: crate::ExecutionProfile::Workspace,
            io: crate::ExecutionIo::Pipes,
            cwd: workspace.clone(),
            terminal_size: None,
            exit: None,
            first_retained_sequence: None,
            last_sequence: 0,
            retained_bytes: 0,
            discarded_output_bytes: 0,
            output_truncated: false,
            output_expired: false,
            started_at_unix_ms: None,
            finished_at_unix_ms: None,
            error_code: None,
        };
        let request = ExecutionRequest {
            owner,
            creating_run_id: run_id,
            creating_step_id: agl_ids::StepId::generate(),
            kind: crate::ExecutionKind::Argv,
            program: std::path::PathBuf::from("/bin/sleep"),
            program_digest: None,
            args: vec!["30".to_owned()],
            workspace_root: workspace.clone(),
            cwd: workspace,
            read_only_roots: Vec::new(),
            environment: crate::EnvironmentOverride {
                values: BTreeMap::new(),
            },
            stdin: None,
            close_stdin_after_initial: true,
            io: crate::ExecutionIo::Pipes,
            terminal_size: None,
            profile: crate::ExecutionProfile::Workspace,
            authorization: crate::ExecutionAuthorization::default(),
            grant_lease: None,
            limits: crate::ExecutionLimits {
                timeout_ms: None,
                max_input_bytes: 1024,
                max_output_bytes: 4096,
            },
        };
        spool.prepare(&execution_id).unwrap();
        repository.admit(&status, &request, "owner").unwrap();
        repository
            .append_lifecycle(&execution_id, "owner", 1, "admitted", 1)
            .unwrap();

        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let child_guard = KillOnDrop(child.id());
        let launched = LaunchedProcess {
            child,
            stdin: None,
            stdout: None,
            stderr: None,
            terminal: None,
        };
        let options = ProcessSupervisorOptions {
            launcher_path: root.join("unused-launcher"),
            data_root: root.join("spool"),
            state_root: root.join("state"),
            max_active: 2,
            command_capacity: 8,
            poll_interval: Duration::from_millis(1),
            setup_timeout: Duration::from_millis(100),
            termination_grace: Duration::from_millis(10),
            max_input_bytes: 1024,
            max_result_bytes: 1024,
            max_spool_bytes: 4096,
            termination_output_headroom_bytes: 1024,
            finished_retention: Duration::from_secs(60),
            runtime_read_only_roots: Vec::new(),
        };
        let mut reactor = Reactor {
            options,
            repository: repository.clone(),
            spool,
            supervisor_id: "owner".to_owned(),
            active: BTreeMap::new(),
            shell_integrations: BTreeMap::new(),
            shutting_down: false,
            shutdown_replies: Vec::new(),
            next_retention_scan: Instant::now() + Duration::from_secs(60),
        };
        let error = reactor
            .adopt_failed_spawn(
                execution_id.clone(),
                request,
                ManagedLaunchedProcess {
                    process: launched,
                    shell_integration: None,
                },
                None,
                2,
                ProcessError::new(
                    ProcessErrorCode::Internal,
                    "injected running commit failure",
                ),
            )
            .unwrap_err();
        assert!(error.message().contains(execution_id.as_str()));
        assert!(reactor.active.contains_key(&execution_id));

        for _ in 0..100 {
            reactor.pump();
            if !reactor.active.contains_key(&execution_id) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!reactor.active.contains_key(&execution_id));
        let terminal = repository.status(&execution_id).unwrap();
        assert_eq!(terminal.state, ExecutionState::Failed);
        assert_eq!(terminal.error_code.as_deref(), Some("internal"));
        drop(child_guard);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reserved_managed_terminal_identity_is_durable_and_cannot_be_reused() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "agl-process-reserved-terminal-{}-{}",
            std::process::id(),
            ExecutionId::generate()
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let marker = root.join("launcher-starts");
        let launcher = root.join("failing-launcher");
        fs::write(
            &launcher,
            format!("#!/bin/sh\nprintf x >> '{}'\nexit 1\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();

        let repository_impl = Arc::new(crate::InMemoryExecutionRepository::new());
        let repository: Arc<dyn ExecutionRepository> = repository_impl.clone();
        let spool_impl = Arc::new(crate::FileOutputSpool::new(root.join("spool")).unwrap());
        let spool: Arc<dyn OutputSpool> = spool_impl.clone();
        let (mut request, startup) = managed_shell_fixture(&workspace);
        let runtime_root = request.program.parent().unwrap().to_path_buf();
        request.read_only_roots.push(runtime_root.clone());
        let supervisor = ProcessSupervisor::start(
            ProcessSupervisorOptions {
                launcher_path: launcher,
                data_root: root.join("data"),
                state_root: root.join("state"),
                max_active: 1,
                command_capacity: 8,
                poll_interval: Duration::from_millis(1),
                setup_timeout: Duration::from_secs(1),
                termination_grace: Duration::from_millis(10),
                max_input_bytes: 1024,
                max_result_bytes: 1024,
                max_spool_bytes: 4096,
                termination_output_headroom_bytes: 1024,
                finished_retention: Duration::from_secs(60),
                runtime_read_only_roots: vec![runtime_root],
            },
            repository,
            spool,
        )
        .unwrap();
        let execution_id = ExecutionId::generate();

        supervisor
            .handle()
            .start_reserved_managed_terminal(execution_id.clone(), request.clone(), startup)
            .unwrap_err();

        let status = repository_impl.status(&execution_id).unwrap();
        assert_eq!(status.execution_id, execution_id);
        assert!(status.state.is_terminal());
        assert_eq!(fs::read(&marker).unwrap(), b"x");
        let execution_root = root
            .join("state")
            .join("executions")
            .join(execution_id.as_str());
        let startup_path = execution_root.join("home/agl-terminal/bashrc");
        assert!(execution_root.join("home").is_dir());
        assert!(execution_root.join("tmp").is_dir());
        let startup_before = fs::read(&startup_path).unwrap();

        spool_impl
            .append(
                &execution_id,
                &ExecutionOutputChunk {
                    sequence: 99,
                    channel: ExecutionChannel::Stdout,
                    bytes: ProcessBytes::from_bytes(b"reserved-spool-sentinel"),
                },
            )
            .unwrap();
        spool_impl.sync(&execution_id).unwrap();
        let spool_path = spool_impl
            .root()
            .join(execution_id.as_str())
            .join("stream.aglspool");
        let spool_before = fs::read(&spool_path).unwrap();
        let (_, second_startup) = managed_shell_fixture(&workspace);

        let error = supervisor
            .handle()
            .start_reserved_managed_terminal(execution_id, request, second_startup)
            .unwrap_err();

        assert_eq!(error.code(), ProcessErrorCode::StateConflict);
        assert_eq!(fs::read(marker).unwrap(), b"x");
        assert_eq!(fs::read(startup_path).unwrap(), startup_before);
        assert_eq!(fs::read(spool_path).unwrap(), spool_before);

        supervisor.shutdown().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn private_terminal_environment_bypasses_repository_and_launcher_environment() {
        use std::os::unix::fs::PermissionsExt as _;

        const SECRET_NAME: &str = "AGL_TEST_PRIVATE_REPOSITORY_VALUE";
        const SENTINEL: &str = "repository-private-sentinel-1c043e";

        struct Secret;
        impl crate::TerminalSecretResolver for Secret {
            fn resolve(
                &self,
                _reference: &crate::TerminalSecretReference,
            ) -> Result<crate::TerminalSecretValue> {
                crate::TerminalSecretValue::new(SENTINEL)
            }
        }

        let root = std::env::temp_dir().join(format!(
            "agl-process-private-environment-{}-{}",
            std::process::id(),
            ExecutionId::generate()
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let launcher_environment = root.join("launcher-environment");
        let launcher = root.join("failing-launcher");
        fs::write(
            &launcher,
            format!(
                "#!/bin/sh\nprintf '%s' \"${{{SECRET_NAME}-unset}}\" > '{}'\nexit 1\n",
                launcher_environment.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();

        let repository_impl = Arc::new(crate::InMemoryExecutionRepository::new());
        let repository: Arc<dyn ExecutionRepository> = repository_impl.clone();
        let spool: Arc<dyn OutputSpool> =
            Arc::new(crate::FileOutputSpool::new(root.join("spool")).unwrap());
        let (mut request, mut startup) = managed_shell_fixture(&workspace);
        let runtime_root = request.program.parent().unwrap().to_path_buf();
        request.read_only_roots.push(runtime_root.clone());
        let mut environment = crate::TerminalEnvironmentRequest::default();
        environment.agl_env.insert(
            SECRET_NAME.to_owned(),
            crate::TerminalEnvironmentValue::Secret(
                crate::TerminalSecretReference::new("test:private-repository").unwrap(),
            ),
        );
        let (public_environment, private_environment) =
            environment.resolve(&Secret).unwrap().into_launch_parts();
        request.environment = public_environment;
        startup.private_environment = private_environment;
        assert!(!format!("{request:?} {startup:?}").contains(SENTINEL));
        assert!(!serde_json::to_string(&request).unwrap().contains(SENTINEL));

        let supervisor = ProcessSupervisor::start(
            ProcessSupervisorOptions {
                launcher_path: launcher,
                data_root: root.join("data"),
                state_root: root.join("state"),
                max_active: 1,
                command_capacity: 8,
                poll_interval: Duration::from_millis(1),
                setup_timeout: Duration::from_secs(1),
                termination_grace: Duration::from_millis(10),
                max_input_bytes: 1024,
                max_result_bytes: 1024,
                max_spool_bytes: 4096,
                termination_output_headroom_bytes: 1024,
                finished_retention: Duration::from_secs(60),
                runtime_read_only_roots: vec![runtime_root],
            },
            repository,
            spool,
        )
        .unwrap();
        let execution_id = ExecutionId::generate();

        let error = supervisor
            .handle()
            .start_reserved_managed_terminal(execution_id.clone(), request, startup)
            .unwrap_err();

        assert!(!error.message().contains(SENTINEL));
        assert!(!format!("{error:?}").contains(SENTINEL));
        assert_eq!(fs::read(&launcher_environment).unwrap(), b"unset");
        let stored_request = repository_impl.admitted_request(&execution_id).unwrap();
        assert!(!stored_request.environment.values.contains_key(SECRET_NAME));
        assert!(!format!("{stored_request:?}").contains(SENTINEL));
        assert!(
            !serde_json::to_string(&stored_request)
                .unwrap()
                .contains(SENTINEL)
        );
        let private_home = root
            .join("state")
            .join("executions")
            .join(execution_id.as_str())
            .join("home/agl-terminal");
        for name in ["bashrc", "history.seed"] {
            assert!(
                !fs::read(private_home.join(name))
                    .unwrap()
                    .windows(SENTINEL.len())
                    .any(|window| window == SENTINEL.as_bytes())
            );
        }

        supervisor.shutdown().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_and_deadline_interrupt_launcher_admission_with_distinct_states() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "agl-process-admission-cancel-{}-{}",
            std::process::id(),
            ExecutionId::generate()
        ));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let sleep = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|root| root.join("sleep"))
            .find(|path| path.is_file())
            .unwrap();
        let launcher = root.join("blocking-launcher");
        std::fs::write(
            &launcher,
            format!("#!/bin/sh\nexec {} 30\n", sleep.display()),
        )
        .unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
        let repository_impl = Arc::new(crate::InMemoryExecutionRepository::new());
        let repository: Arc<dyn ExecutionRepository> = repository_impl.clone();
        let spool = Arc::new(crate::FileOutputSpool::new(root.join("spool")).unwrap());
        let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
        let executable_root = executable.parent().unwrap().to_path_buf();
        let supervisor = ProcessSupervisor::start(
            ProcessSupervisorOptions {
                launcher_path: launcher,
                data_root: root.join("data"),
                state_root: root.join("state"),
                max_active: 1,
                command_capacity: 8,
                poll_interval: Duration::from_millis(1),
                setup_timeout: Duration::from_secs(30),
                termination_grace: Duration::from_millis(10),
                max_input_bytes: 1024,
                max_result_bytes: 1024,
                max_spool_bytes: 4096,
                termination_output_headroom_bytes: 1024,
                finished_retention: Duration::from_secs(60),
                runtime_read_only_roots: vec![executable_root.clone()],
            },
            repository,
            spool,
        )
        .unwrap();
        let run_id = agl_ids::RunId::generate();
        let owner = crate::ExecutionOwner::Run {
            run_id: run_id.clone(),
            root_run_id: run_id.clone(),
        };
        let request = ExecutionRequest {
            owner,
            creating_run_id: run_id,
            creating_step_id: agl_ids::StepId::generate(),
            kind: crate::ExecutionKind::Argv,
            program: executable.clone(),
            program_digest: None,
            args: Vec::new(),
            workspace_root: workspace.clone(),
            cwd: workspace,
            read_only_roots: vec![executable_root],
            environment: crate::EnvironmentOverride {
                values: BTreeMap::new(),
            },
            stdin: None,
            close_stdin_after_initial: true,
            io: crate::ExecutionIo::Pipes,
            terminal_size: None,
            profile: crate::ExecutionProfile::Workspace,
            authorization: crate::ExecutionAuthorization::default(),
            grant_lease: None,
            limits: crate::ExecutionLimits {
                timeout_ms: None,
                max_input_bytes: 1024,
                max_output_bytes: 4096,
            },
        };
        let forged_runtime_root = root.join("forged-runtime-root");
        std::fs::create_dir_all(&forged_runtime_root).unwrap();
        let mut forged_request = request.clone();
        forged_request.read_only_roots = vec![forged_runtime_root.canonicalize().unwrap()];
        let error = supervisor.handle().start(forged_request).unwrap_err();
        assert_eq!(error.code(), ProcessErrorCode::InvalidRequest);
        assert!(
            error
                .message()
                .contains("outside the configured supervisor roots")
        );
        assert!(
            repository_impl
                .list(&ExecutionListFilter {
                    session_id: None,
                    root_run_id: None,
                    include_finished: true,
                })
                .unwrap()
                .is_empty(),
            "forged runtime root must be rejected before durable admission"
        );

        let started = Instant::now();
        let cancellation_repository = Arc::clone(&repository_impl);
        let error = supervisor
            .handle()
            .start_cancellable(request.clone(), None, || {
                cancellation_repository
                    .list(&ExecutionListFilter {
                        session_id: None,
                        root_run_id: None,
                        include_finished: true,
                    })
                    .is_ok_and(|statuses| !statuses.is_empty())
                    || started.elapsed() >= Duration::from_secs(1)
            })
            .unwrap_err();
        assert_eq!(error.code(), ProcessErrorCode::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(2));
        let statuses = repository_impl
            .list(&ExecutionListFilter {
                session_id: None,
                root_run_id: None,
                include_finished: true,
            })
            .unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].state, ExecutionState::Cancelled);
        assert_eq!(
            statuses[0].error_code.as_deref(),
            Some(ProcessErrorCode::Cancelled.as_str())
        );

        let deadline_started = Instant::now();
        let error = supervisor
            .handle()
            .start_cancellable(
                request,
                Some(Instant::now() + Duration::from_millis(500)),
                || false,
            )
            .unwrap_err();
        assert_eq!(error.code(), ProcessErrorCode::TimedOut);
        assert!(deadline_started.elapsed() < Duration::from_secs(2));
        let statuses = repository_impl
            .list(&ExecutionListFilter {
                session_id: None,
                root_run_id: None,
                include_finished: true,
            })
            .unwrap();
        assert_eq!(statuses.len(), 2);
        assert_ne!(statuses[0].execution_id, statuses[1].execution_id);
        assert!(statuses.iter().any(|status| {
            status.state == ExecutionState::TimedOut
                && status.error_code.as_deref() == Some(ProcessErrorCode::TimedOut.as_str())
        }));

        supervisor.shutdown().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
