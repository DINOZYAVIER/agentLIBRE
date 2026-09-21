use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use agl_execution_api::{
    ExecutionId, ExecutionIo, ExecutionIsolation, ExecutionOutcome, ExecutionOutputStream,
    ExecutionSignal, ExecutionStartRequest, ExecutionStatus, TerminalId, TerminalSize,
};
use anyhow::{Context, Result, ensure};

use crate::store::ExecutionStore;

const REASON_NONE: u8 = 0;
const REASON_TIMEOUT: u8 = 1;
const REASON_TERMINATE: u8 = 2;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ExecutionControlError {
    #[error("execution service is shutting down")]
    Unavailable,
    #[error("terminal is not active")]
    InactiveTerminal,
    #[error("execution is not active")]
    InactiveExecution,
}

struct ActiveExecution {
    pid: i32,
    signal_process_group: bool,
    terminal_id: Option<TerminalId>,
    input: Option<Arc<Mutex<File>>>,
    termination_reason: Arc<AtomicU8>,
    timer_cancels: Vec<Sender<()>>,
}

pub struct ExecutionService {
    store: ExecutionStore,
    active: Arc<Mutex<BTreeMap<ExecutionId, ActiveExecution>>>,
    launcher: PathBuf,
    accepting: Arc<AtomicBool>,
    owns_lifetime: bool,
}

impl Clone for ExecutionService {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            active: Arc::clone(&self.active),
            launcher: self.launcher.clone(),
            accepting: Arc::clone(&self.accepting),
            owns_lifetime: false,
        }
    }
}

impl ExecutionService {
    pub fn start(data_root: &Path, launcher: PathBuf) -> Result<Self> {
        validate_launcher(&launcher)?;
        Ok(Self {
            store: ExecutionStore::open(data_root)?,
            active: Arc::new(Mutex::new(BTreeMap::new())),
            launcher,
            accepting: Arc::new(AtomicBool::new(true)),
            owns_lifetime: true,
        })
    }

    pub fn launch(&self, request: ExecutionStartRequest) -> Result<ExecutionStatus> {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(ExecutionControlError::Unavailable.into());
        }
        request.validate().map_err(anyhow::Error::msg)?;
        let execution_id = ExecutionId::generate();
        let terminal_id = (request.io == ExecutionIo::Pty).then(TerminalId::generate);
        let mut spawned = spawn_launcher(&self.launcher, &request)?;
        let pid = i32::try_from(spawned.child.id()).context("child PID exceeds i32")?;
        if let Err(error) = self.store.insert(
            execution_id,
            terminal_id,
            &request.owner,
            request.max_output_bytes,
        ) {
            let _ = spawned.child.kill();
            let _ = spawned.child.wait();
            return Err(error);
        }

        let termination_reason = Arc::new(AtomicU8::new(REASON_NONE));
        let input = spawned.input.map(|file| Arc::new(Mutex::new(file)));
        let (timeout_cancel, timeout_receiver) = if request.timeout_ms > 0 {
            let (cancel, receiver) = channel();
            (Some(cancel), Some(receiver))
        } else {
            (None, None)
        };
        self.active
            .lock()
            .map_err(|_| anyhow::anyhow!("active execution lock is poisoned"))?
            .insert(
                execution_id,
                ActiveExecution {
                    pid,
                    signal_process_group: spawned.signal_process_group,
                    terminal_id,
                    input,
                    termination_reason: Arc::clone(&termination_reason),
                    timer_cancels: timeout_cancel.into_iter().collect(),
                },
            );

        let mut readers = Vec::new();
        for (stream, mut reader) in spawned.readers {
            let store = self.store.clone();
            readers.push(thread::spawn(move || {
                let mut buffer = [0_u8; 16 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            if store.append(execution_id, stream, &buffer[..read]).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                        Err(_) => break,
                    }
                }
            }));
        }
        self.spawn_waiter(
            execution_id,
            spawned.child,
            readers,
            Arc::clone(&termination_reason),
        );
        if let Some(timeout_receiver) = timeout_receiver {
            self.spawn_timeout(
                execution_id,
                pid,
                spawned.signal_process_group,
                request.timeout_ms,
                termination_reason,
                timeout_receiver,
            );
        }
        self.store.status(execution_id)
    }

    fn spawn_waiter(
        &self,
        execution_id: ExecutionId,
        mut child: Child,
        readers: Vec<thread::JoinHandle<()>>,
        termination_reason: Arc<AtomicU8>,
    ) {
        let store = self.store.clone();
        let active = Arc::clone(&self.active);
        thread::spawn(move || {
            let status = child.wait();
            for reader in readers {
                let _ = reader.join();
            }
            let reason = termination_reason.load(Ordering::Acquire);
            let outcome = match (reason, status) {
                (REASON_TIMEOUT, _) => ExecutionOutcome::TimedOut,
                (REASON_TERMINATE, _) => ExecutionOutcome::Terminated,
                (_, Ok(status)) => match (status.code(), status.signal()) {
                    (Some(code), _) => ExecutionOutcome::Exit { code },
                    (_, Some(signal)) => ExecutionOutcome::Signal { signal },
                    _ => ExecutionOutcome::UnknownAfterServiceRestart,
                },
                (_, Err(_)) => ExecutionOutcome::UnknownAfterServiceRestart,
            };
            let _ = store.finish(execution_id, &outcome);
            if let Ok(mut active) = active.lock() {
                active.remove(&execution_id);
            }
        });
    }

    fn spawn_timeout(
        &self,
        execution_id: ExecutionId,
        pid: i32,
        signal_process_group: bool,
        timeout_ms: u64,
        termination_reason: Arc<AtomicU8>,
        cancel: Receiver<()>,
    ) {
        let active = Arc::clone(&self.active);
        thread::spawn(move || {
            if cancel
                .recv_timeout(Duration::from_millis(timeout_ms))
                .is_ok()
            {
                return;
            }
            let still_active = active
                .lock()
                .is_ok_and(|active| active.contains_key(&execution_id));
            if still_active {
                termination_reason.store(REASON_TIMEOUT, Ordering::Release);
                force_cleanup(pid, signal_process_group);
            }
        });
    }

    pub fn status(&self, execution_id: ExecutionId) -> Result<ExecutionStatus> {
        self.store.status(execution_id)
    }

    pub fn status_by_terminal(&self, terminal_id: TerminalId) -> Result<ExecutionStatus> {
        self.store
            .execution_for_terminal(terminal_id)
            .and_then(|execution_id| self.store.status(execution_id))
    }

    pub fn read(
        &self,
        execution_id: ExecutionId,
        after: u64,
        max_bytes: u32,
    ) -> Result<agl_execution_api::ExecutionOutput> {
        self.store.read(execution_id, after, max_bytes)
    }

    pub fn write(&self, terminal_id: TerminalId, data: &[u8]) -> Result<()> {
        let execution_id = self.store.execution_for_terminal(terminal_id)?;
        let input = self
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("active execution lock is poisoned"))?
            .get(&execution_id)
            .and_then(|active| active.input.clone())
            .ok_or(ExecutionControlError::InactiveTerminal)?;
        input
            .lock()
            .map_err(|_| anyhow::anyhow!("terminal input lock is poisoned"))?
            .write_all(data)?;
        Ok(())
    }

    pub fn resize(&self, terminal_id: TerminalId, size: TerminalSize) -> Result<()> {
        let execution_id = self.store.execution_for_terminal(terminal_id)?;
        let active = self
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("active execution lock is poisoned"))?;
        let active = active
            .get(&execution_id)
            .ok_or(ExecutionControlError::InactiveTerminal)?;
        ensure!(
            active.terminal_id == Some(terminal_id),
            "terminal identity mismatch"
        );
        let input = active.input.as_ref().context("terminal has no PTY")?;
        let descriptor = std::os::fd::AsRawFd::as_raw_fd(
            &*input
                .lock()
                .map_err(|_| anyhow::anyhow!("terminal input lock is poisoned"))?,
        );
        let dimensions = libc::winsize {
            ws_row: size.rows,
            ws_col: size.columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: descriptor is a live PTY master and dimensions points to initialized memory.
        let result = unsafe { libc::ioctl(descriptor, libc::TIOCSWINSZ, &dimensions) };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("failed to resize PTY");
        }
        signal_process(active.pid, active.signal_process_group, libc::SIGWINCH);
        Ok(())
    }

    pub fn signal(&self, execution_id: ExecutionId, signal: ExecutionSignal) -> Result<()> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("active execution lock is poisoned"))?;
        let active = active
            .get_mut(&execution_id)
            .ok_or(ExecutionControlError::InactiveExecution)?;
        let raw = match signal {
            ExecutionSignal::Interrupt => libc::SIGINT,
            ExecutionSignal::Terminate => {
                active
                    .termination_reason
                    .store(REASON_TERMINATE, Ordering::Release);
                libc::SIGTERM
            }
        };
        let pid = active.pid;
        let signal_process_group = active.signal_process_group;
        signal_process(pid, signal_process_group, raw);
        if signal == ExecutionSignal::Terminate {
            let (cancel, receiver) = channel();
            active.timer_cancels.push(cancel);
            let active = Arc::clone(&self.active);
            thread::spawn(move || {
                if receiver.recv_timeout(Duration::from_secs(2)).is_ok() {
                    return;
                }
                if active
                    .lock()
                    .is_ok_and(|active| active.contains_key(&execution_id))
                {
                    force_cleanup(pid, signal_process_group);
                }
            });
        }
        Ok(())
    }
}

fn validate_launcher(launcher: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    ensure!(
        launcher.is_absolute(),
        "execd launcher path must be absolute"
    );
    let metadata = std::fs::symlink_metadata(launcher)
        .with_context(|| format!("failed to inspect execd launcher {}", launcher.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "execd launcher must be one regular non-symlink file"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "execd launcher must be owned by the current user"
    );
    let mode = metadata.permissions().mode();
    ensure!(mode & 0o111 != 0, "execd launcher must be executable");
    ensure!(
        mode & 0o022 == 0,
        "execd launcher must not be group- or world-writable"
    );
    Ok(())
}

struct SpawnedExecution {
    child: Child,
    signal_process_group: bool,
    input: Option<File>,
    readers: Vec<(ExecutionOutputStream, Box<dyn Read + Send>)>,
}

fn spawn_launcher(launcher: &Path, request: &ExecutionStartRequest) -> Result<SpawnedExecution> {
    if let ExecutionIsolation::PrivateInference {
        artifacts,
        listen_socket,
        device_paths,
        address_space_limit_bytes,
    } = &request.isolation
    {
        return spawn_private_inference(
            request,
            artifacts,
            listen_socket,
            device_paths,
            *address_space_limit_bytes,
        );
    }
    let mut command = Command::new(launcher);
    command.current_dir(&request.cwd);
    if request.clear_environment {
        command.env_clear();
    }
    command.envs(&request.environment);
    command.arg(match request.io {
        ExecutionIo::Pipes => "--pipes",
        ExecutionIo::Pty => "--pty",
    });
    command.arg("--");
    command.args(&request.argv);

    match request.io {
        ExecutionIo::Pipes => {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().context("failed to start execd launcher")?;
            let stdout = child
                .stdout
                .take()
                .context("launcher stdout is unavailable")?;
            let stderr = child
                .stderr
                .take()
                .context("launcher stderr is unavailable")?;
            Ok(SpawnedExecution {
                child,
                signal_process_group: false,
                input: None,
                readers: vec![
                    (ExecutionOutputStream::Stdout, Box::new(stdout)),
                    (ExecutionOutputStream::Stderr, Box::new(stderr)),
                ],
            })
        }
        ExecutionIo::Pty => {
            let size = request.terminal_size.context("PTY size is missing")?;
            let (master, slave) = open_pty(size)?;
            let slave_file = File::from(slave);
            command.stdin(Stdio::from(slave_file.try_clone()?));
            command.stdout(Stdio::from(slave_file.try_clone()?));
            command.stderr(Stdio::from(slave_file));
            let child = command.spawn().context("failed to start PTY launcher")?;
            let master = File::from(master);
            Ok(SpawnedExecution {
                child,
                signal_process_group: false,
                input: Some(master.try_clone()?),
                readers: vec![(ExecutionOutputStream::Pty, Box::new(master))],
            })
        }
    }
}

const EXECUTABLE_FD: RawFd = 189;
const LISTEN_FD: RawFd = 190;
const MODEL_FD: RawFd = 200;

fn spawn_private_inference(
    request: &ExecutionStartRequest,
    artifacts: &[String],
    listen_socket: &str,
    device_paths: &[String],
    address_space_limit_bytes: u64,
) -> Result<SpawnedExecution> {
    let executable_path = PathBuf::from(&request.argv[0]);
    let executable = File::open(&executable_path).context("failed to open inference executable")?;
    let artifacts = artifacts
        .iter()
        .map(|path| File::open(path).context("failed to open inference artifact"))
        .collect::<Result<Vec<_>>>()?;
    let socket_path = Path::new(listen_socket);
    let listener = UnixListener::bind(socket_path).context("failed to bind inference socket")?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    let device_paths = device_paths.iter().map(PathBuf::from).collect::<Vec<_>>();
    let model_files = artifacts.iter().collect::<Vec<_>>();
    let sandbox = crate::sandbox::PreparedSandbox::prepare(
        &executable_path,
        &executable,
        &model_files,
        Path::new(&request.cwd),
        &device_paths,
    )?;
    let executable_source = executable.as_raw_fd();
    let listener_source = listener.as_raw_fd();
    let artifact_sources = artifacts
        .iter()
        .enumerate()
        .map(|(index, file)| (file.as_raw_fd(), MODEL_FD + index as RawFd))
        .collect::<Vec<_>>();
    let last_artifact_fd = MODEL_FD + artifact_sources.len() as RawFd - 1;
    let held_resources = (executable, artifacts, listener);
    let mut command = Command::new(format!("/proc/self/fd/{EXECUTABLE_FD}"));
    command.current_dir(&request.cwd);
    if request.clear_environment {
        command.env_clear();
    }
    command.envs(&request.environment);
    command.args(&request.argv[1..]);
    command.env("AGL_LLAMA_SERVER_LISTEN_FD", LISTEN_FD.to_string());
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() == 1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "execd exited while starting private inference",
                ));
            }
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            duplicate(executable_source, EXECUTABLE_FD)?;
            duplicate(listener_source, LISTEN_FD)?;
            for (source, target) in &artifact_sources {
                duplicate(*source, *target)?;
            }
            sandbox.enter()?;
            close_other_descriptors(last_artifact_fd)?;
            set_address_limit(address_space_limit_bytes)?;
            install_network_filter()?;
            let _ = &held_resources;
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .context("failed to start private inference process")?;
    let stdout = child
        .stdout
        .take()
        .context("inference stdout is unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("inference stderr is unavailable")?;
    Ok(SpawnedExecution {
        child,
        signal_process_group: true,
        input: None,
        readers: vec![
            (ExecutionOutputStream::Stdout, Box::new(stdout)),
            (ExecutionOutputStream::Stderr, Box::new(stderr)),
        ],
    })
}

unsafe fn duplicate(source: RawFd, target: RawFd) -> std::io::Result<()> {
    if source != target && unsafe { libc::dup2(source, target) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let flags = unsafe { libc::fcntl(target, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(target, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn close_other_descriptors(last_artifact_fd: RawFd) -> std::io::Result<()> {
    for (start, end) in [
        (3, EXECUTABLE_FD - 1),
        (LISTEN_FD + 1, MODEL_FD - 1),
        (last_artifact_fd + 1, 4096),
    ] {
        for fd in start..=end {
            unsafe {
                libc::close(fd);
            }
        }
    }
    Ok(())
}

fn set_address_limit(bytes: u64) -> std::io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: bytes,
        rlim_max: bytes,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_network_filter() -> std::io::Result<()> {
    const LD_W_ABS: u16 = 0x20;
    const JMP_JEQ: u16 = 0x15;
    const RET: u16 = 0x06;
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xc000_00b7;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "private inference seccomp does not support this architecture",
    ));
    let stmt = |code, k| libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |code, k, jt, jf| libc::sock_filter { code, jt, jf, k };
    let denied = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
    let mut filter = vec![
        stmt(LD_W_ABS, 4),
        jump(JMP_JEQ, AUDIT_ARCH, 1, 0),
        stmt(RET, denied),
        stmt(LD_W_ABS, 0),
    ];
    for syscall in [
        libc::SYS_socket,
        libc::SYS_connect,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_setns,
        libc::SYS_unshare,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_open_by_handle_at,
        libc::SYS_kexec_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_fork,
        libc::SYS_vfork,
    ] {
        filter.push(jump(JMP_JEQ, syscall as u32, 0, 1));
        filter.push(stmt(RET, denied));
    }
    filter.push(stmt(RET, libc::SECCOMP_RET_ALLOW));
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    if unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn install_network_filter() -> std::io::Result<()> {
    Ok(())
}

fn open_pty(size: TerminalSize) -> Result<(OwnedFd, OwnedFd)> {
    let dimensions = libc::winsize {
        ws_row: size.rows,
        ws_col: size.columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: pointers refer to writable descriptors and an initialized winsize.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &dimensions,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to allocate PTY");
    }
    // SAFETY: successful openpty returns two newly owned descriptors.
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

fn signal_process(pid: i32, process_group: bool, signal: i32) {
    // SAFETY: kill has no memory-safety preconditions. Standard executions
    // signal their supervising launcher; isolated runtime services are direct
    // session leaders and use their process group.
    let target = if process_group { -pid } else { pid };
    let _ = unsafe { libc::kill(target, signal) };
}

fn force_cleanup(pid: i32, process_group: bool) {
    if process_group {
        signal_process(pid, true, libc::SIGKILL);
    } else {
        // The launcher owns its target process group. SIGUSR1 asks it to kill
        // that group before it exits, unlike SIGKILL directed at the launcher.
        signal_process(pid, false, libc::SIGUSR1);
    }
}

impl Drop for ExecutionService {
    fn drop(&mut self) {
        if !self.owns_lifetime {
            return;
        }
        self.accepting.store(false, Ordering::Release);
        if let Ok(active) = self.active.lock() {
            for execution in active.values() {
                force_cleanup(execution.pid, execution.signal_process_group);
            }
        }
    }
}
