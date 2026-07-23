use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::super::{
    Handshake, HostCommand, Ready, Result, Shutdown, ShutdownComplete, ShutdownReason,
    WORKER_BINARY_NAME, WorkerEvent, WorkerIdentity, WorkerProtocolError, WorkerProtocolErrorCode,
};
use super::socket::{HostControlChannel, launch_channel_pair};
use super::{INHERITED_CONTROL_FD_ENV, INHERITED_PARENT_PID_ENV};

const DEFAULT_FORCE_REAP_TIMEOUT: Duration = Duration::from_secs(2);
const WORKER_STDERR_FINISH_TIMEOUT: Duration = Duration::from_millis(100);
const WORKER_STDERR_READ_BUFFER_BYTES: usize = 4 * 1024;
const MAX_WORKER_STDERR_MARKERS: usize = 32;

/// Maximum serialized size of the private, redacted stderr observation.
///
/// The observation contains only byte counts and closed marker identifiers.
/// No worker-provided text is retained or returned to the caller.
pub const MAX_WORKER_STDERR_EVIDENCE_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerStderrMarker {
    VulkanDeviceLost,
    VulkanDeviceOutOfMemory,
    VulkanHostOutOfMemory,
    GgmlAbort,
    SignalAbort,
    SignalSegmentationFault,
    SignalBus,
    SignalIllegalInstruction,
    SignalKill,
}

impl WorkerStderrMarker {
    const fn as_str(self) -> &'static str {
        match self {
            Self::VulkanDeviceLost => "vk_error_device_lost",
            Self::VulkanDeviceOutOfMemory => "vk_error_out_of_device_memory",
            Self::VulkanHostOutOfMemory => "vk_error_out_of_host_memory",
            Self::GgmlAbort => "ggml_abort",
            Self::SignalAbort => "sigabrt",
            Self::SignalSegmentationFault => "sigsegv",
            Self::SignalBus => "sigbus",
            Self::SignalIllegalInstruction => "sigill",
            Self::SignalKill => "sigkill",
        }
    }
}

const WORKER_STDERR_MARKERS: [(&[u8], WorkerStderrMarker); 9] = [
    (
        b"VK_ERROR_DEVICE_LOST",
        WorkerStderrMarker::VulkanDeviceLost,
    ),
    (
        b"VK_ERROR_OUT_OF_DEVICE_MEMORY",
        WorkerStderrMarker::VulkanDeviceOutOfMemory,
    ),
    (
        b"VK_ERROR_OUT_OF_HOST_MEMORY",
        WorkerStderrMarker::VulkanHostOutOfMemory,
    ),
    (b"GGML_ABORT", WorkerStderrMarker::GgmlAbort),
    (b"SIGABRT", WorkerStderrMarker::SignalAbort),
    (b"SIGSEGV", WorkerStderrMarker::SignalSegmentationFault),
    (b"SIGBUS", WorkerStderrMarker::SignalBus),
    (b"SIGILL", WorkerStderrMarker::SignalIllegalInstruction),
    (b"SIGKILL", WorkerStderrMarker::SignalKill),
];

#[derive(Debug, Default)]
struct WorkerStderrState {
    total_bytes: u64,
    markers: VecDeque<WorkerStderrMarker>,
    dropped_markers: u64,
    read_failed: bool,
}

impl WorkerStderrState {
    fn record_bytes(&mut self, count: usize) {
        self.total_bytes = self
            .total_bytes
            .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
    }

    fn record_marker(&mut self, marker: WorkerStderrMarker) {
        if self.markers.len() == MAX_WORKER_STDERR_MARKERS {
            self.markers.pop_front();
            self.dropped_markers = self.dropped_markers.saturating_add(1);
        }
        self.markers.push_back(marker);
    }

    fn render_private_evidence(&self) -> Option<String> {
        if self.total_bytes == 0 && !self.read_failed {
            return None;
        }
        let mut evidence = format!(
            "worker_stderr_observation bytes={} markers_retained={} markers_dropped={} read_failed={}",
            self.total_bytes,
            self.markers.len(),
            self.dropped_markers,
            self.read_failed
        );
        for marker in &self.markers {
            evidence.push_str("\nobserved_non_authoritative_marker=");
            evidence.push_str(marker.as_str());
        }
        debug_assert!(evidence.len() <= MAX_WORKER_STDERR_EVIDENCE_BYTES);
        if evidence.len() > MAX_WORKER_STDERR_EVIDENCE_BYTES {
            evidence.truncate(MAX_WORKER_STDERR_EVIDENCE_BYTES);
        }
        Some(evidence)
    }
}

#[derive(Debug, Default)]
struct WorkerStderrScanner {
    tail: Vec<u8>,
}

impl WorkerStderrScanner {
    fn scan(&mut self, bytes: &[u8], state: &mut WorkerStderrState) {
        let prior_tail_len = self.tail.len();
        let mut window = Vec::with_capacity(prior_tail_len.saturating_add(bytes.len()));
        window.extend_from_slice(&self.tail);
        window.extend_from_slice(bytes);

        let mut observed = Vec::new();
        for (needle, marker) in WORKER_STDERR_MARKERS {
            let mut offset = 0;
            while let Some(relative) = find_bytes(&window[offset..], needle) {
                let start = offset + relative;
                let end = start + needle.len();
                if end > prior_tail_len {
                    observed.push((start, marker));
                }
                offset = end;
            }
        }
        observed.sort_by_key(|(offset, _)| *offset);
        for (_, marker) in observed {
            state.record_marker(marker);
        }

        let maximum_marker_bytes = WORKER_STDERR_MARKERS
            .iter()
            .map(|(marker, _)| marker.len())
            .max()
            .unwrap_or(1);
        let retained = maximum_marker_bytes.saturating_sub(1).min(window.len());
        self.tail.clear();
        self.tail
            .extend_from_slice(&window[window.len().saturating_sub(retained)..]);
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[derive(Debug)]
struct WorkerStderrCapture {
    state: Arc<Mutex<WorkerStderrState>>,
    finished: mpsc::Receiver<()>,
    reader: Option<JoinHandle<()>>,
}

impl WorkerStderrCapture {
    fn start(stderr: ChildStderr) -> Result<Self> {
        Self::start_reader(stderr)
    }

    fn start_reader<R>(mut reader: R) -> Result<Self>
    where
        R: std::io::Read + Send + 'static,
    {
        let state = Arc::new(Mutex::new(WorkerStderrState::default()));
        let reader_state = Arc::clone(&state);
        let (finished_sender, finished) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("agl-inference-worker-stderr".to_string())
            .spawn(move || {
                let mut scanner = WorkerStderrScanner::default();
                let mut buffer = [0_u8; WORKER_STDERR_READ_BUFFER_BYTES];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            let mut state = lock_stderr_state(&reader_state);
                            state.record_bytes(read);
                            scanner.scan(&buffer[..read], &mut state);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            lock_stderr_state(&reader_state).read_failed = true;
                            break;
                        }
                    }
                }
                let _ = finished_sender.send(());
            })
            .map_err(|error| {
                WorkerProtocolError::new(
                    WorkerProtocolErrorCode::SpawnFailed,
                    format!("failed to start bounded inference worker stderr drain: {error}"),
                )
            })?;
        Ok(Self {
            state,
            finished,
            reader: Some(reader),
        })
    }

    fn finish(&mut self, timeout: Duration) {
        if self.reader.is_none() {
            return;
        }
        let completed = match self.finished.recv_timeout(timeout) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => true,
            Err(mpsc::RecvTimeoutError::Timeout) => false,
        };
        let Some(reader) = self.reader.take() else {
            return;
        };
        if completed && reader.join().is_err() {
            lock_stderr_state(&self.state).read_failed = true;
        }
        // Dropping a still-running JoinHandle deliberately detaches the
        // bounded drain instead of blocking a daemon application thread.
    }

    fn private_evidence(&self) -> Option<String> {
        lock_stderr_state(&self.state).render_private_evidence()
    }
}

fn lock_stderr_state(
    state: &Arc<Mutex<WorkerStderrState>>,
) -> std::sync::MutexGuard<'_, WorkerStderrState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug)]
pub struct WorkerExecutable {
    path: PathBuf,
    origin_directory: PathBuf,
    file: File,
}

impl WorkerExecutable {
    pub fn sibling_of_current_executable() -> Result<Self> {
        let host = std::env::current_exe().map_err(|error| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUnavailable,
                format!("failed to resolve the current AGL executable: {error}"),
            )
        })?;
        Self::sibling_of(host)
    }

    pub fn sibling_of(host_executable: impl AsRef<Path>) -> Result<Self> {
        let host_executable = host_executable.as_ref();
        let directory = host_executable.parent().ok_or_else(|| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUnavailable,
                format!(
                    "host executable {} has no sibling directory",
                    host_executable.display()
                ),
            )
        })?;
        Self::open_exact(directory.join(WORKER_BINARY_NAME))
    }

    pub fn open_exact(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let file = options.open(path).map_err(|error| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUnavailable,
                format!(
                    "inference worker {} is unavailable: {error}",
                    path.display()
                ),
            )
        })?;
        let proc_fd_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        let pinned_path = fs::read_link(&proc_fd_path).map_err(|error| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUntrusted,
                format!(
                    "failed to resolve pinned inference worker {}: {error}",
                    path.display()
                ),
            )
        })?;
        let origin_directory = pinned_path.parent().ok_or_else(|| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUntrusted,
                format!(
                    "pinned inference worker {} has no origin directory",
                    path.display()
                ),
            )
        })?;
        let origin_directory = fs::canonicalize(origin_directory).map_err(|error| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUntrusted,
                format!(
                    "failed to resolve inference worker origin {}: {error}",
                    path.display()
                ),
            )
        })?;
        let metadata = file.metadata().map_err(|error| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUntrusted,
                format!(
                    "failed to inspect inference worker {}: {error}",
                    path.display()
                ),
            )
        })?;
        let mode = metadata.permissions().mode();
        if !metadata.is_file() || mode & 0o111 == 0 {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUntrusted,
                format!(
                    "inference worker {} is not a regular executable",
                    path.display()
                ),
            ));
        }
        if mode & 0o6022 != 0 {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUntrusted,
                format!(
                    "inference worker {} has set-id or group/other-writable mode bits",
                    path.display()
                ),
            ));
        }
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid && metadata.uid() != 0 {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUntrusted,
                format!(
                    "inference worker {} is owned by an unexpected user",
                    path.display()
                ),
            ));
        }
        Ok(Self {
            path: path.to_owned(),
            origin_directory,
            file,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn proc_fd_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
    }

    fn origin_directory(&self) -> &Path {
        &self.origin_directory
    }
}

#[derive(Debug)]
pub struct WorkerProcess {
    child: Option<Child>,
    channel: HostControlChannel,
    stderr: WorkerStderrCapture,
    identity: WorkerIdentity,
    executable_path: PathBuf,
    native_bundle_id: String,
}

impl WorkerProcess {
    /// Constructs an in-memory host-side process fixture for runtime protocol
    /// tests. It deliberately has no child: containment tests exercise the
    /// same control transport and teardown path without granting a fixture an
    /// ambient executable or process authority.
    #[cfg(test)]
    pub(crate) fn test_fixture(channel: HostControlChannel) -> Self {
        Self {
            child: None,
            channel,
            stderr: WorkerStderrCapture::start_reader(std::io::empty())
                .expect("empty test stderr capture must start"),
            identity: WorkerIdentity::current(),
            executable_path: PathBuf::from("/test/agl-inference-worker"),
            native_bundle_id: "test-native-bundle".to_string(),
        }
    }

    pub fn spawn(executable: &WorkerExecutable, handshake_timeout: Duration) -> Result<Self> {
        Self::spawn_with_environment(executable, handshake_timeout, &BTreeMap::new())
    }

    /// Starts the exact worker with a deliberately small structured environment.
    ///
    /// The worker never inherits the host environment. In particular, `PATH`,
    /// proxy credentials and `LD_LIBRARY_PATH` cannot cross this boundary.
    /// Callers may provide only backend-discovery paths and private cache/temp
    /// roots which are independently admitted by the sandbox configuration.
    pub fn spawn_with_environment(
        executable: &WorkerExecutable,
        handshake_timeout: Duration,
        environment: &BTreeMap<String, OsString>,
    ) -> Result<Self> {
        validate_launch_environment(environment)?;
        let native_bundle = crate::worker_resources::discover_native_bundle_for_pinned_worker(
            &executable.proc_fd_path(),
            executable.origin_directory(),
            executable.path(),
            false,
        )
        .map_err(|error| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUntrusted,
                format!("failed to validate exact native worker bundle: {error}"),
            )
        })?;
        let expected_native_bundle_id = native_bundle.identity().to_owned();
        let (channel, child_socket) = launch_channel_pair()?;
        clear_close_on_exec(child_socket.as_raw_fd())?;
        let child_socket_descriptor = child_socket.as_raw_fd();
        let expected_parent = unsafe { libc::getpid() };
        let mut command = Command::new(executable.proc_fd_path());
        command
            .env_clear()
            .env(
                INHERITED_CONTROL_FD_ENV,
                child_socket.as_raw_fd().to_string(),
            )
            .env(INHERITED_PARENT_PID_ENV, expected_parent.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (name, value) in environment {
            command.env(name, value);
        }
        unsafe {
            command.pre_exec(move || {
                prepare_child_before_exec(expected_parent, child_socket_descriptor)
            });
        }
        let mut child = command.spawn().map_err(|error| {
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::SpawnFailed,
                format!(
                    "failed to start exact inference worker {}: {error}",
                    executable.path.display()
                ),
            )
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            let _ = child.kill();
            let _ = child.wait();
            WorkerProtocolError::new(
                WorkerProtocolErrorCode::SpawnFailed,
                "inference worker did not expose its bounded stderr pipe",
            )
        })?;
        let stderr = match WorkerStderrCapture::start(stderr) {
            Ok(stderr) => stderr,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        drop(child_socket);

        let mut process = Self {
            child: Some(child),
            channel,
            stderr,
            identity: WorkerIdentity::current(),
            executable_path: executable.path.clone(),
            native_bundle_id: expected_native_bundle_id.clone(),
        };
        let handshake = (|| {
            process
                .channel
                .send(HostCommand::Handshake(Handshake::current()))?;
            match process.channel.receive_timeout(handshake_timeout)? {
                WorkerEvent::Ready(ready) => {
                    validate_ready(&ready, &expected_native_bundle_id)?;
                    process.identity = ready.identity().clone();
                    Ok(())
                }
                WorkerEvent::HandshakeRejected(rejection) => Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::IdentityMismatch,
                    format!(
                        "inference worker rejected the exact-generation handshake: {}",
                        rejection.code().as_str()
                    ),
                )),
                WorkerEvent::ShutdownComplete(_) => Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::UnexpectedMessage,
                    "inference worker replied to handshake with shutdown_complete",
                )),
                _ => Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::UnexpectedMessage,
                    "inference worker sent an operation event before handshake completed",
                )),
            }
        })();
        if let Err(error) = handshake {
            process.terminate_and_reap();
            let error = match process.private_stderr_evidence() {
                Some(evidence) => error.with_private_log(evidence),
                None => error,
            };
            return Err(error);
        }
        Ok(process)
    }

    pub fn identity(&self) -> &WorkerIdentity {
        &self.identity
    }

    pub fn executable_path(&self) -> &Path {
        &self.executable_path
    }

    pub fn native_bundle_id(&self) -> &str {
        &self.native_bundle_id
    }

    pub fn generation_build_id(&self) -> String {
        format!("{}+{}", self.identity.build_id(), self.native_bundle_id)
    }

    pub fn child_id(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    pub fn channel_mut(&mut self) -> &mut HostControlChannel {
        &mut self.channel
    }

    /// Returns a bounded private observation of worker stderr without any raw
    /// worker-provided text. Marker names are diagnostic hints only and must
    /// never be used as authoritative device-loss classification.
    pub(crate) fn private_stderr_evidence(&self) -> Option<String> {
        self.stderr.private_evidence()
    }

    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        let status = self
            .child
            .as_mut()
            .map(|child| {
                child.try_wait().map_err(|error| {
                    WorkerProtocolError::new(
                        WorkerProtocolErrorCode::Io,
                        format!("failed to inspect inference worker exit: {error}"),
                    )
                })
            })
            .transpose()
            .map(Option::flatten)?;
        if status.is_some() {
            self.stderr.finish(WORKER_STDERR_FINISH_TIMEOUT);
        }
        Ok(status)
    }

    pub fn shutdown(self, reason: ShutdownReason, timeout: Duration) -> Result<()> {
        self.shutdown_with_reap_status(reason, timeout).0
    }

    pub(crate) fn shutdown_with_reap_status(
        mut self,
        reason: ShutdownReason,
        timeout: Duration,
    ) -> (Result<()>, bool) {
        let orderly = (|| {
            self.channel
                .send(HostCommand::Shutdown(Shutdown::new(reason)))?;
            match self.channel.receive_timeout(timeout)? {
                WorkerEvent::ShutdownComplete(ShutdownComplete {}) => {}
                _ => {
                    return Err(WorkerProtocolError::new(
                        WorkerProtocolErrorCode::UnexpectedMessage,
                        "inference worker sent an invalid response to shutdown",
                    ));
                }
            }
            let status = self.wait_for_exit(timeout)?;
            if !status.success() {
                return Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::SpawnFailed,
                    format!("inference worker failed during orderly shutdown: {status}"),
                ));
            }
            Ok(())
        })();
        match orderly {
            Ok(()) => (Ok(()), true),
            Err(orderly_error) => match self.terminate_and_reap_with_timeout(timeout) {
                Ok(()) => (Err(orderly_error), true),
                Err(reap_error) => (
                    Err(WorkerProtocolError::new(
                        WorkerProtocolErrorCode::TimedOut,
                        format!(
                            "inference worker shutdown failed ({orderly_error}); forced reap also failed ({reap_error})"
                        ),
                    )),
                    false,
                ),
            },
        }
    }

    pub fn terminate_and_reap(&mut self) {
        let _ = self.terminate_and_reap_with_timeout(DEFAULT_FORCE_REAP_TIMEOUT);
    }

    /// Sends SIGKILL and bounds the caller's reap wait. A child which remains
    /// unreapable is handed to a detached reaper thread so the daemon's
    /// application thread never blocks indefinitely.
    pub fn terminate_and_reap_with_timeout(&mut self, timeout: Duration) -> Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        let _ = child.kill();
        match self.wait_for_exit(timeout) {
            Ok(_) => Ok(()),
            Err(error) => {
                if let Some(mut child) = self.child.take() {
                    let _ = std::thread::Builder::new()
                        .name("agl-inference-worker-reaper".to_string())
                        .spawn(move || {
                            let _ = child.wait();
                        });
                }
                Err(error)
            }
        }
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Result<std::process::ExitStatus> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            let Some(child) = self.child.as_mut() else {
                return Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::Io,
                    "inference worker was already reaped",
                ));
            };
            if let Some(status) = child.try_wait().map_err(|error| {
                WorkerProtocolError::new(
                    WorkerProtocolErrorCode::Io,
                    format!("failed to reap inference worker: {error}"),
                )
            })? {
                self.child.take();
                self.stderr.finish(WORKER_STDERR_FINISH_TIMEOUT);
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(WorkerProtocolError::new(
                    WorkerProtocolErrorCode::TimedOut,
                    "timed out waiting for inference worker exit",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn validate_launch_environment(environment: &BTreeMap<String, OsString>) -> Result<()> {
    const ALLOWED: [&str; 3] = ["TMPDIR", "VK_DRIVER_FILES", "XDG_CACHE_HOME"];
    for (name, value) in environment {
        if !ALLOWED.contains(&name.as_str()) {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUntrusted,
                format!("inference worker launch environment key is not allowed: {name}"),
            ));
        }
        if value.is_empty() {
            return Err(WorkerProtocolError::new(
                WorkerProtocolErrorCode::WorkerUntrusted,
                format!("inference worker launch environment value is empty: {name}"),
            ));
        }
    }
    Ok(())
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        self.terminate_and_reap();
    }
}

fn validate_ready(ready: &Ready, expected_native_bundle_id: &str) -> Result<()> {
    ready.validate_exact()?;
    if ready.native_bundle_id() != expected_native_bundle_id {
        return Err(WorkerProtocolError::new(
            WorkerProtocolErrorCode::IdentityMismatch,
            "inference worker native bundle identity does not match its exact sibling generation",
        ));
    }
    Ok(())
}

fn clear_close_on_exec(descriptor: libc::c_int) -> Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0
    {
        return Err(WorkerProtocolError::last_os_error(
            WorkerProtocolErrorCode::Io,
            "failed to admit the inference worker control descriptor",
        ));
    }
    Ok(())
}

fn prepare_child_before_exec(
    expected_parent: libc::pid_t,
    control_descriptor: libc::c_int,
) -> std::io::Result<()> {
    let core_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &core_limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::getppid() } != expected_parent {
        return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
    }
    if unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3_u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let flags = unsafe { libc::fcntl(control_descriptor, libc::F_GETFD) };
    if flags < 0
        || unsafe { libc::fcntl(control_descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn sibling_resolution_never_searches_path() {
        let error = WorkerExecutable::sibling_of("agl")
            .expect_err("relative executable without parent must fail");
        assert_eq!(error.code(), WorkerProtocolErrorCode::WorkerUnavailable);
    }

    #[test]
    fn launch_environment_is_an_exact_allowlist() {
        let mut accepted = BTreeMap::new();
        accepted.insert(
            "VK_DRIVER_FILES".to_string(),
            OsString::from("/nix/store/example/share/vulkan/icd.d/radeon.json"),
        );
        accepted.insert("TMPDIR".to_string(), OsString::from("/tmp/agl-worker"));
        accepted.insert(
            "XDG_CACHE_HOME".to_string(),
            OsString::from("/tmp/agl-worker"),
        );
        validate_launch_environment(&accepted).expect("structured paths are allowed");

        for rejected in ["HOME", "LD_LIBRARY_PATH", "PATH", "HTTPS_PROXY"] {
            let mut environment = BTreeMap::new();
            environment.insert(rejected.to_string(), OsString::from("/host/authority"));
            let error = validate_launch_environment(&environment)
                .expect_err("ambient host authority must be rejected");
            assert_eq!(error.code(), WorkerProtocolErrorCode::WorkerUntrusted);
        }

        let mut empty = BTreeMap::new();
        empty.insert("TMPDIR".to_string(), OsString::new());
        assert_eq!(
            validate_launch_environment(&empty)
                .expect_err("empty values are ambiguous")
                .code(),
            WorkerProtocolErrorCode::WorkerUntrusted
        );
    }

    #[test]
    fn hostile_stderr_is_continuously_drained_redacted_and_bounded() {
        let secret = "PROMPT_SECRET_must_never_reach_worker_loss_evidence";
        let mut hostile = vec![b'x'; WORKER_STDERR_READ_BUFFER_BYTES - 7];
        for _ in 0..(MAX_WORKER_STDERR_MARKERS + 80) {
            hostile.extend_from_slice(b" VK_ERROR_DEVICE_LOST ");
            hostile.extend_from_slice(secret.as_bytes());
            hostile.push(b'\n');
        }
        let expected_bytes = hostile.len();
        let mut capture = WorkerStderrCapture::start_reader(Cursor::new(hostile))
            .expect("start bounded stderr capture");
        capture.finish(Duration::from_secs(1));
        let evidence = capture
            .private_evidence()
            .expect("nonempty stderr produces private evidence");

        assert!(evidence.len() <= MAX_WORKER_STDERR_EVIDENCE_BYTES);
        assert!(evidence.contains(&format!("bytes={expected_bytes}")));
        assert!(evidence.contains("markers_retained=32"));
        assert!(evidence.contains("observed_non_authoritative_marker=vk_error_device_lost"));
        assert!(!evidence.contains("markers_dropped=0"));
        assert!(!evidence.contains(secret));
        assert!(!evidence.contains("xxxxxxxx"));
    }
}
