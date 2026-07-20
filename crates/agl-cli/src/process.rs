use std::io::{self, IsTerminal as _, Read as _, Write as _};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agl_client::{AgentLibreClient, ExecutionAttachment, ExecutionAttachmentEvent};
use agl_ids::ExecutionId;
use agl_process::ProcessPlatformDiagnostics;
use agl_protocol::{
    ExecutionChannel, ExecutionIo, ExecutionKillRequest, ExecutionListRequest, ExecutionOwner,
    ExecutionReadRequest, ExecutionStatus, ExecutionStatusRequest, KillMode, PROTOCOL_VERSION,
    ProcessBytes,
};
use agl_runtime::AgentLibreRuntimeConfig;
use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::args::{
    ProcessAttachOptions, ProcessCommand, ProcessDoctorOptions, ProcessKillOptions,
    ProcessListOptions, ProcessReadOptions, ProcessStatusOptions,
};
use crate::tui::terminal_filter::TerminalOutputFilter;

const ATTACH_DETACH_BYTE: u8 = 0x1d;
const ATTACH_POLL_INTERVAL: Duration = Duration::from_millis(20);

pub(crate) fn run_process(
    command: ProcessCommand,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    match command {
        ProcessCommand::List(options) => process_list(options, runtime),
        ProcessCommand::Status(options) => process_status(options, runtime),
        ProcessCommand::Read(options) => process_read(options, runtime),
        ProcessCommand::Attach(options) => process_attach(options, runtime),
        ProcessCommand::Kill(options) => process_kill(options, runtime),
        ProcessCommand::Doctor(options) => process_doctor(options, runtime),
    }
}

#[cfg(unix)]
fn process_list(options: ProcessListOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let client = daemon_client(runtime)?;
    let event = client
        .execution_list(ExecutionListRequest {
            session_id: options.session_id,
            root_run_id: options.root_run_id,
            include_finished: options.include_finished,
        })
        .context("failed to list daemon process executions")?;
    crate::print_json_or(options.json, &event.executions, || {
        print_execution_list(&event.executions)
    })
}

#[cfg(not(unix))]
fn process_list(_options: ProcessListOptions, _runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    bail!("daemon process control is unsupported on this platform")
}

#[cfg(unix)]
fn process_status(options: ProcessStatusOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let client = daemon_client(runtime)?;
    let event = client
        .execution_status(ExecutionStatusRequest {
            execution_id: options.execution_id,
            include_private_command: options.private_command,
        })
        .context("failed to inspect daemon process execution")?;
    crate::print_json_or(options.json, &event, || {
        print_execution_detail(&event.status);
        if let Some(command) = &event.private_command {
            println!("private_command={}", command.display);
            println!("private_command_truncated={}", command.truncated);
        }
    })
}

#[cfg(not(unix))]
fn process_status(
    _options: ProcessStatusOptions,
    _runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    bail!("daemon process control is unsupported on this platform")
}

#[cfg(unix)]
fn process_read(options: ProcessReadOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let client = daemon_client(runtime)?;
    let event = client
        .execution_read(ExecutionReadRequest {
            execution_id: options.execution_id,
            after_sequence: options.after_sequence,
            max_bytes: options.max_bytes,
        })
        .context("failed to read retained process output")?;
    if options.json {
        return crate::print_json(&event.output);
    }
    write_output_chunks(&event.output.chunks, options.max_bytes)
}

#[cfg(not(unix))]
fn process_read(_options: ProcessReadOptions, _runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    bail!("daemon process control is unsupported on this platform")
}

#[cfg(unix)]
fn process_kill(options: ProcessKillOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    confirm_kill(&options)?;
    let mode = if options.immediate {
        KillMode::Immediate
    } else {
        KillMode::Graceful
    };
    let client = daemon_client(runtime)?;
    let event = client
        .execution_kill(ExecutionKillRequest {
            execution_id: options.execution_id,
            mode,
        })
        .context("failed to terminate daemon process execution")?;
    crate::print_json_or(options.json, &event, || {
        println!(
            "execution_id={} termination={:?} accepted=true",
            event.execution_id, event.mode
        );
    })
}

#[cfg(not(unix))]
fn process_kill(_options: ProcessKillOptions, _runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    bail!("daemon process control is unsupported on this platform")
}

fn process_doctor(options: ProcessDoctorOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let launcher = process_launcher_path()?;
    let diagnostics = agl_process::process_platform_diagnostics(&launcher);
    let workspace_root = runtime
        .resolve_workspace_root(None)
        .context("failed to resolve the process doctor workspace")?;
    let report = ProcessDoctorReport {
        diagnostics,
        workspace_root,
        runtime_read_only_roots: runtime.execution.runtime_read_only_roots.clone(),
    };
    crate::print_json_or(options.json, &report, || print_doctor_report(&report))?;
    if report.diagnostics.supported {
        Ok(())
    } else {
        bail!(
            "process sandbox preflight failed: {}",
            report
                .diagnostics
                .error_code
                .as_deref()
                .unwrap_or("platform_unsupported")
        )
    }
}

#[cfg(unix)]
fn process_attach(options: ProcessAttachOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("process attach requires local terminal stdin and stdout")
    }
    let client = daemon_client(runtime)?;
    let mut attachment = client
        .attach_execution(
            options.execution_id,
            options.after_sequence,
            !options.read_only,
        )
        .context("failed to attach to daemon process execution")?;
    let terminal = TerminalGuard::enter().context("failed to enter raw terminal mode")?;
    let is_pty = attachment.started().status.io == ExecutionIo::Pty;
    if is_pty && let Some((columns, rows)) = terminal_size() {
        attachment
            .resize(columns, rows)
            .context("failed to send initial terminal size")?;
    }

    let writable = attachment.started().writable;
    let mut output_filter = TerminalOutputFilter::new(true);
    let mut stdin_open = true;
    let mut detaching = false;
    loop {
        if !detaching && terminal_interrupted() {
            attachment
                .detach()
                .context("failed to detach after interrupt")?;
            detaching = true;
        }
        if !detaching
            && is_pty
            && terminal_resized()
            && let Some((columns, rows)) = terminal_size()
        {
            attachment
                .resize(columns, rows)
                .context("failed to resize attached terminal")?;
        }
        if !detaching && stdin_open && stdin_ready()? {
            let mut bytes = [0_u8; 4096];
            let count = io::stdin()
                .read(&mut bytes)
                .context("failed to read attached terminal input")?;
            if count == 0 {
                stdin_open = false;
                if writable {
                    attachment
                        .input(ProcessBytes::from_bytes(&[]), true)
                        .context("failed to close attached process input")?;
                }
            } else if let Some(detach_at) = bytes[..count]
                .iter()
                .position(|byte| *byte == ATTACH_DETACH_BYTE)
            {
                if writable && detach_at > 0 {
                    attachment
                        .input(ProcessBytes::from_bytes(&bytes[..detach_at]), false)
                        .context("failed to forward terminal input before detach")?;
                }
                attachment.detach().context("failed to detach terminal")?;
                detaching = true;
            } else if writable {
                attachment
                    .input(ProcessBytes::from_bytes(&bytes[..count]), false)
                    .context("failed to forward attached terminal input")?;
            }
        }

        if !attachment.wait_for_event(ATTACH_POLL_INTERVAL)? {
            continue;
        }
        match attachment.next_event()? {
            Some(ExecutionAttachmentEvent::Output(event)) => {
                write_attached_chunk(&mut output_filter, &event.chunk)?;
            }
            Some(ExecutionAttachmentEvent::Finished(event)) => {
                let _ = output_filter.finish();
                drop(terminal);
                eprintln!(
                    "attachment_finished execution_id={} state={:?} reason={:?} cursor={} filtered_controls={} malformed_controls={}",
                    event.execution_id,
                    event.state,
                    event.reason,
                    event.last_delivered_sequence,
                    output_filter.blocked_total(),
                    output_filter.malformed_total(),
                );
                return Ok(());
            }
            None => {
                drop(terminal);
                bail!("execution attachment ended without a terminal event")
            }
        }
    }
}

#[cfg(not(unix))]
fn process_attach(
    _options: ProcessAttachOptions,
    _runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    bail!("process attach is unsupported on this platform")
}

#[cfg(unix)]
fn daemon_client(runtime: &AgentLibreRuntimeConfig) -> Result<CliDaemonClient> {
    let socket_path = agl_daemon::default_socket_path(&runtime.paths);
    let async_runtime = std::sync::Arc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build CLI async runtime")?,
    );
    let client = async_runtime
        .block_on(AgentLibreClient::connect(&socket_path))
        .with_context(|| {
            format!(
                "agentLIBRE daemon is unavailable at {}; start it with `agl serve`",
                socket_path.display()
            )
        })?;
    let hello = client
        .hello()
        .context("daemon process protocol handshake failed")?;
    if hello.protocol_version != PROTOCOL_VERSION {
        bail!(
            "daemon protocol {} is incompatible with client {}",
            hello.protocol_version,
            PROTOCOL_VERSION
        );
    }
    Ok(CliDaemonClient {
        async_runtime,
        client,
    })
}

#[cfg(unix)]
struct CliDaemonClient {
    async_runtime: std::sync::Arc<tokio::runtime::Runtime>,
    client: AgentLibreClient,
}

#[cfg(unix)]
impl CliDaemonClient {
    fn execution_list(
        &self,
        request: ExecutionListRequest,
    ) -> Result<agl_protocol::ExecutionListEvent, agl_client::ClientError> {
        self.async_runtime
            .block_on(self.client.execution_list(request))
    }

    fn execution_status(
        &self,
        request: ExecutionStatusRequest,
    ) -> Result<agl_protocol::ExecutionStatusEvent, agl_client::ClientError> {
        self.async_runtime
            .block_on(self.client.execution_status(request))
    }

    fn execution_read(
        &self,
        request: ExecutionReadRequest,
    ) -> Result<agl_protocol::ExecutionReadEvent, agl_client::ClientError> {
        self.async_runtime
            .block_on(self.client.execution_read(request))
    }

    fn execution_kill(
        &self,
        request: ExecutionKillRequest,
    ) -> Result<agl_protocol::ExecutionKillAcceptedEvent, agl_client::ClientError> {
        self.async_runtime
            .block_on(self.client.execution_kill(request))
    }

    fn attach_execution(
        &self,
        execution_id: ExecutionId,
        after_sequence: u64,
        writable: bool,
    ) -> Result<CliExecutionAttachment, agl_client::ClientError> {
        let inner = self.async_runtime.block_on(self.client.attach_execution(
            execution_id,
            after_sequence,
            writable,
        ))?;
        Ok(CliExecutionAttachment {
            async_runtime: std::sync::Arc::clone(&self.async_runtime),
            inner,
            pending: None,
        })
    }
}

#[cfg(unix)]
struct CliExecutionAttachment {
    async_runtime: std::sync::Arc<tokio::runtime::Runtime>,
    inner: ExecutionAttachment,
    pending: Option<ExecutionAttachmentEvent>,
}

#[cfg(unix)]
impl CliExecutionAttachment {
    fn started(&self) -> &agl_protocol::ExecutionAttachmentStartedEvent {
        &self.inner.started
    }

    fn resize(
        &self,
        columns: u16,
        rows: u16,
    ) -> Result<agl_protocol::ExecutionResizeAcceptedEvent, agl_client::ClientError> {
        self.async_runtime
            .block_on(self.inner.resize(columns, rows))
    }

    fn input(
        &self,
        bytes: ProcessBytes,
        eof: bool,
    ) -> Result<agl_protocol::ExecutionInputAcceptedEvent, agl_client::ClientError> {
        self.async_runtime.block_on(self.inner.input(bytes, eof))
    }

    fn detach(
        &mut self,
    ) -> Result<agl_protocol::ExecutionDetachAcceptedEvent, agl_client::ClientError> {
        self.async_runtime.block_on(self.inner.detach())
    }

    fn wait_for_event(&mut self, timeout: Duration) -> Result<bool> {
        if self.pending.is_some() {
            return Ok(true);
        }
        match self
            .async_runtime
            .block_on(async { tokio::time::timeout(timeout, self.inner.next()).await })
        {
            Ok(event) => {
                self.pending = event?;
                Ok(self.pending.is_some())
            }
            Err(_) => Ok(false),
        }
    }

    fn next_event(&mut self) -> Result<Option<ExecutionAttachmentEvent>> {
        Ok(self.pending.take())
    }
}

fn confirm_kill(options: &ProcessKillOptions) -> Result<()> {
    if options.yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!("process kill requires --yes when no interactive terminal is available")
    }
    eprint!(
        "Terminate execution {}{}? [y/N] ",
        options.execution_id,
        if options.immediate {
            " immediately"
        } else {
            ""
        }
    );
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!("process termination was not confirmed")
    }
}

pub(crate) fn print_execution_list(executions: &[ExecutionStatus]) {
    println!("EXECUTION_ID\tOWNER\tSTATE\tPROFILE\tIO\tCWD\tAGE\tEXIT/ERROR\tBYTES");
    for status in executions {
        println!(
            "{}\t{}\t{:?}\t{:?}\t{:?}\t{}\t{}\t{}\t{}",
            status.execution_id,
            owner_label(&status.owner),
            status.state,
            status.profile,
            status.io,
            status.cwd.display(),
            age_label(status),
            exit_label(status),
            status.retained_bytes,
        );
    }
}

fn print_execution_detail(status: &ExecutionStatus) {
    println!("execution_id={}", status.execution_id);
    println!("owner={}", owner_label(&status.owner));
    println!("state={:?}", status.state);
    println!("profile={:?}", status.profile);
    println!("io={:?}", status.io);
    println!("cwd={}", status.cwd.display());
    println!("age={}", age_label(status));
    println!("exit_or_error={}", exit_label(status));
    println!("retained_bytes={}", status.retained_bytes);
    println!("discarded_output_bytes={}", status.discarded_output_bytes);
    println!("last_sequence={}", status.last_sequence);
    println!("output_truncated={}", status.output_truncated);
    println!("output_expired={}", status.output_expired);
}

fn owner_label(owner: &ExecutionOwner) -> String {
    match owner {
        ExecutionOwner::Session {
            session_id,
            root_run_id,
        } => format!("session:{session_id}@{root_run_id}"),
        ExecutionOwner::Run {
            run_id,
            root_run_id,
        } => format!("run:{run_id}@{root_run_id}"),
    }
}

fn age_label(status: &ExecutionStatus) -> String {
    let timestamp = status
        .finished_at_unix_ms
        .or(status.started_at_unix_ms)
        .unwrap_or_default();
    let age = unix_millis().saturating_sub(timestamp).max(0) as u64 / 1000;
    format!("{age}s")
}

fn exit_label(status: &ExecutionStatus) -> String {
    if let Some(error) = &status.error_code {
        return error.clone();
    }
    status
        .exit
        .as_ref()
        .map(|exit| format!("{exit:?}"))
        .unwrap_or_else(|| "-".to_string())
}

fn write_output_chunks(
    chunks: &[agl_protocol::ExecutionOutputChunk],
    maximum_bytes: usize,
) -> Result<()> {
    for chunk in chunks {
        let bytes = chunk
            .bytes
            .decode(maximum_bytes)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        match chunk.channel {
            ExecutionChannel::Stderr => io::stderr().write_all(&bytes)?,
            ExecutionChannel::Stdout | ExecutionChannel::Terminal | ExecutionChannel::Lifecycle => {
                io::stdout().write_all(&bytes)?
            }
        }
    }
    io::stdout().flush()?;
    io::stderr().flush()?;
    Ok(())
}

#[cfg(unix)]
fn write_attached_chunk(
    filter: &mut TerminalOutputFilter,
    chunk: &agl_protocol::ExecutionOutputChunk,
) -> Result<()> {
    let bytes = chunk
        .bytes
        .decode(65_536)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let filtered = filter.filter(&bytes).bytes;
    match chunk.channel {
        ExecutionChannel::Stderr => {
            io::stderr().write_all(&filtered)?;
            io::stderr().flush()?;
        }
        ExecutionChannel::Stdout | ExecutionChannel::Terminal | ExecutionChannel::Lifecycle => {
            io::stdout().write_all(&filtered)?;
            io::stdout().flush()?;
        }
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ProcessDoctorReport {
    diagnostics: ProcessPlatformDiagnostics,
    workspace_root: PathBuf,
    runtime_read_only_roots: Vec<PathBuf>,
}

fn print_doctor_report(report: &ProcessDoctorReport) {
    println!("platform={}", report.diagnostics.platform);
    println!("supported={}", report.diagnostics.supported);
    println!("launcher={}", report.diagnostics.launcher);
    println!("user_namespace={}", report.diagnostics.user_namespace);
    println!("pid_namespace={}", report.diagnostics.pid_namespace);
    println!("mount_namespace={}", report.diagnostics.mount_namespace);
    println!("network_namespace={}", report.diagnostics.network_namespace);
    println!("landlock_abi={:?}", report.diagnostics.landlock_abi);
    println!("seccomp={}", report.diagnostics.seccomp);
    println!("pidfd={}", report.diagnostics.pidfd);
    println!("pty={}", report.diagnostics.pty);
    println!("workspace_root={}", report.workspace_root.display());
    if let Some(code) = &report.diagnostics.error_code {
        println!("error_code={code}");
    }
    if let Some(remediation) = &report.diagnostics.remediation {
        println!("remediation={remediation}");
    }
}

fn process_launcher_path() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("failed to resolve current executable")?;
    let parent = executable
        .parent()
        .context("current executable has no parent directory")?;
    let directory = if parent.file_name().is_some_and(|name| name == "deps") {
        parent
            .parent()
            .context("test executable directory has no target parent")?
    } else {
        parent
    };
    Ok(directory.join("agl-process-launcher"))
}

pub(crate) fn verify_runtime_bundle_identity() -> Result<()> {
    let launcher = process_launcher_path()?;
    agl_process::verify_process_launcher_identity(&launcher)
        .map_err(anyhow::Error::from)
        .context("process launcher identity verification failed")
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(unix)]
static TERMINAL_RESIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(unix)]
static TERMINAL_INTERRUPTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn terminal_signal(signal: libc::c_int) {
    match signal {
        libc::SIGWINCH => TERMINAL_RESIZED.store(true, std::sync::atomic::Ordering::Release),
        libc::SIGINT | libc::SIGTERM | libc::SIGHUP => {
            TERMINAL_INTERRUPTED.store(true, std::sync::atomic::Ordering::Release)
        }
        _ => {}
    }
}

#[cfg(unix)]
struct TerminalGuard {
    original_termios: libc::termios,
    original_handlers: Vec<(libc::c_int, libc::sigaction)>,
}

#[cfg(unix)]
impl TerminalGuard {
    fn enter() -> Result<Self> {
        let mut original_termios = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original_termios) } != 0 {
            return Err(io::Error::last_os_error()).context("tcgetattr failed");
        }
        let mut raw = original_termios;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error()).context("tcsetattr raw mode failed");
        }
        TERMINAL_RESIZED.store(true, std::sync::atomic::Ordering::Release);
        TERMINAL_INTERRUPTED.store(false, std::sync::atomic::Ordering::Release);
        let mut original_handlers = Vec::new();
        for signal in [libc::SIGWINCH, libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
            action.sa_sigaction = terminal_signal as *const () as usize;
            unsafe { libc::sigemptyset(&mut action.sa_mask) };
            let mut original = unsafe { std::mem::zeroed::<libc::sigaction>() };
            if unsafe { libc::sigaction(signal, &action, &mut original) } != 0 {
                let error = io::Error::last_os_error();
                for (installed, previous) in original_handlers.iter().rev() {
                    unsafe { libc::sigaction(*installed, previous, std::ptr::null_mut()) };
                }
                unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original_termios) };
                return Err(error).context("sigaction failed");
            }
            original_handlers.push((signal, original));
        }
        Ok(Self {
            original_termios,
            original_handlers,
        })
    }
}

#[cfg(unix)]
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        for (signal, previous) in self.original_handlers.iter().rev() {
            unsafe { libc::sigaction(*signal, previous, std::ptr::null_mut()) };
        }
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original_termios) };
    }
}

#[cfg(unix)]
fn terminal_resized() -> bool {
    TERMINAL_RESIZED.swap(false, std::sync::atomic::Ordering::AcqRel)
}

#[cfg(unix)]
fn terminal_interrupted() -> bool {
    TERMINAL_INTERRUPTED.swap(false, std::sync::atomic::Ordering::AcqRel)
}

#[cfg(unix)]
fn terminal_size() -> Option<(u16, u16)> {
    let mut size = unsafe { std::mem::zeroed::<libc::winsize>() };
    for descriptor in [libc::STDOUT_FILENO, libc::STDIN_FILENO] {
        if unsafe { libc::ioctl(descriptor, libc::TIOCGWINSZ, &mut size) } == 0
            && size.ws_col > 0
            && size.ws_row > 0
        {
            return Some((size.ws_col, size.ws_row));
        }
    }
    None
}

#[cfg(unix)]
fn stdin_ready() -> Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut descriptor, 1, 0) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error).context("failed to poll terminal input");
    }
    Ok(ready > 0 && descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0)
}
