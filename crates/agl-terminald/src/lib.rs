mod service;
mod storage;
#[cfg(target_os = "linux")]
mod supervisor;
#[cfg(not(target_os = "linux"))]
#[path = "supervisor_unsupported.rs"]
mod supervisor;
#[cfg(all(test, target_os = "linux"))]
#[allow(dead_code)]
#[path = "supervisor_unsupported.rs"]
mod supervisor_unsupported_contract;
pub mod terminal;
#[doc(hidden)]
pub mod test_support;

pub use agl_exec::ShellIntegrationReadResult;
pub use agl_exec::{
    CommittedOutputFrame, ExecutionChannel, ExecutionContextSnapshot, ExecutionCursor,
    ExecutionExit, ExecutionId, ExecutionListFilter, ExecutionOutputChunk, ExecutionPrivateCommand,
    ExecutionReadResult, ExecutionRepository, ExecutionRequest, ExecutionRequestId, ExecutionState,
    ExecutionStatus, ExecutionTerminalUpdate, FileOutputSpool, InMemoryExecutionRepository,
    InputLease, KillMode, OutputSpool, OutputSpoolRead, ProcessBytes, ProcessBytesEncoding,
    ProcessError, ProcessErrorCode, ProcessSupervisorOptions, Result,
    WRITABLE_INPUT_LEASE_HEARTBEAT, WRITABLE_INPUT_LEASE_TTL, WriterLeaseId,
    resolve_execution_directory,
};
pub use agl_exec::{
    EnvironmentOverride, ExecutionAuthorization, ExecutionCorrelation, ExecutionGrantLease,
    ExecutionIo, ExecutionKind, ExecutionLeaseOrigin, ExecutionLimits, ExecutionOwner,
    ExecutionProfile, LOCAL_OPERATOR_TERMINAL_LEASE_DURATION, ShellProfileSnapshot, TerminalSize,
};
pub use agl_pty::ProcessPlatformDiagnostics;
pub use agl_terminal::{
    AgentTerminalCommandQueue, CommandCardSanitizer, HumanTerminalCommandAdmission,
    QueuedTerminalCommand, SanitizedTerminalOutput, TerminalCommandOutputRange,
    TerminalCommandResult, TerminalOwner, TerminalTopologyId, human_terminal_command_submission,
    sanitize_terminal_card_output,
};
pub use service::{TerminalService, serve_unix, serve_unix_listener};
pub use storage::{SqliteExecutionRepository, SqliteTerminalRepository};
pub use supervisor::{ProcessHandle, ProcessSupervisor};
pub use terminal::environment::{
    RejectTerminalSecrets, ResolvedTerminalEnvironment, TerminalEnvironmentDigest,
    TerminalEnvironmentRequest, TerminalEnvironmentValue, TerminalSecretReference,
    TerminalSecretResolver, TerminalSecretValue,
};
pub use terminal::history::{
    EphemeralTerminalHistory, HumanShellHistoryStore, TerminalHistoryOwner, TerminalHistorySeed,
};
pub use terminal::registry::{TerminalEnsureRequest, TerminalRegistry};
pub use terminal::shell::{
    AdmittedShellKind, AdmittedShellProfile, BoundedShellIntegration, CommandBoundary,
    HostStartupPolicy, IntegrationBatch, ShellExit, ShellIntegrationControl, ShellIntegrationEvent,
    ShellIntegrationHealth, ShellIntegrationNotice, ShellIntegrationState, TerminalPromptState,
    TypedCommandAbortReason, TypedCommandTransactionId,
};
pub use terminal::{
    InMemoryTerminalRepository, StoredTerminalRecord, TerminalRecord, TerminalRepository,
    TerminalReservation, TerminalState, terminal_slot_key, validate_terminal_replacement,
    validate_terminal_reservation,
};

#[doc(hidden)]
pub use agl_pty::launcher_main;

pub fn process_platform_diagnostics(
    launcher_path: impl AsRef<std::path::Path>,
) -> ProcessPlatformDiagnostics {
    agl_pty::diagnostics(launcher_path.as_ref())
}

#[doc(hidden)]
pub fn verify_process_launcher_identity(launcher_path: impl AsRef<std::path::Path>) -> Result<()> {
    agl_pty::verify_launcher_binary_identity(launcher_path.as_ref())
}

pub fn run_from_environment() -> Result<()> {
    use fs2::FileExt as _;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use agl_exec::ServiceGenerationId;
    use agl_terminal::environment::RejectTerminalSecrets;
    use agl_terminal_protocol::{
        ServiceIdentity, TerminalGenerationFileRole, VerifiedTerminalGeneration,
    };
    use tokio_util::sync::CancellationToken;

    fn required_path(name: &str) -> Result<PathBuf> {
        let value = std::env::var_os(name).ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                format!("{name} must be configured for agl-terminald"),
            )
        })?;
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                format!("{name} must be an absolute path"),
            ));
        }
        Ok(path)
    }

    let socket_path = required_path("AGL_TERMINALD_SOCKET")?;
    let data_root = required_path("AGL_TERMINALD_DATA_ROOT")?;
    let state_root = required_path("AGL_TERMINALD_STATE_ROOT")?;
    let runtime_root = required_path("AGL_TERMINALD_RUNTIME_ROOT")?;

    let executable = std::env::current_exe().map_err(identity_io_error)?;
    let generation_directory = executable.parent().ok_or_else(|| {
        ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "agl-terminald executable has no generation directory",
        )
    })?;
    let generation =
        VerifiedTerminalGeneration::load_installed(generation_directory).map_err(|error| {
            ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                format!("terminal generation verification failed: {error}"),
            )
        })?;
    let expected_executable = generation.file_path(TerminalGenerationFileRole::Service);
    if executable != expected_executable {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            format!(
                "terminal service must execute its sealed generation binary: expected {}, got {}",
                expected_executable.display(),
                executable.display()
            ),
        ));
    }
    let launcher_path = generation.file_path(TerminalGenerationFileRole::Launcher);
    verify_process_launcher_identity(&launcher_path)?;
    let identity = ServiceIdentity::new(
        generation.identity().clone(),
        ServiceGenerationId::generate(),
    )
    .map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            format!("terminal service identity is invalid: {error}"),
        )
    })?;

    prepare_runtime_root(&runtime_root)?;
    let lifetime_lock_path = runtime_root.join("service.lock");
    let lifetime_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lifetime_lock_path)
        .map_err(identity_io_error)?;
    lifetime_lock.try_lock_exclusive().map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::StateConflict,
            format!("another terminal service owns this runtime root: {error}"),
        )
    })?;
    remove_service_identity(&runtime_root)?;

    enum ReadinessState {
        Installed,
        DependenciesReady,
        ListenerReady,
        IdentityPublished,
    }
    let mut readiness = ReadinessState::Installed;

    let options = ProcessSupervisorOptions {
        launcher_path,
        data_root: data_root.clone(),
        state_root: state_root.clone(),
        max_active: 64,
        command_capacity: 512,
        poll_interval: Duration::from_millis(5),
        setup_timeout: Duration::from_secs(10),
        termination_grace: Duration::from_secs(2),
        max_input_bytes: 4 * 1024 * 1024,
        max_result_bytes: 4 * 1024 * 1024,
        max_spool_bytes: 64 * 1024 * 1024,
        termination_output_headroom_bytes: 64 * 1024,
        finished_retention: Duration::from_secs(24 * 60 * 60),
        runtime_read_only_roots: Vec::new(),
    };
    let execution_repository = Arc::new(SqliteExecutionRepository::open_at(
        &data_root,
        options.finished_retention,
    )?);
    let spool = Arc::new(FileOutputSpool::new(state_root.join("spool"))?);
    let supervisor = ProcessSupervisor::start(options, execution_repository, spool)?;
    let process = supervisor.handle();
    let terminal_repository = Arc::new(SqliteTerminalRepository::open_at(&data_root)?);
    let admission_repository = Arc::new(storage::SqliteAdmissionRepository::open_at(&data_root)?);
    let registry = Arc::new(TerminalRegistry::new(
        process.clone(),
        Arc::new(RejectTerminalSecrets),
        terminal_repository,
    )?);
    let service = Arc::new(
        TerminalService::new(identity.clone(), registry, process)
            .map_err(|error| {
                ProcessError::new(
                    ProcessErrorCode::InvalidRequest,
                    format!("terminal service identity is invalid: {error}"),
                )
            })?
            .with_admission_repository(admission_repository)?,
    );
    debug_assert!(matches!(readiness, ReadinessState::Installed));
    readiness = ReadinessState::DependenciesReady;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            ProcessError::new(
                ProcessErrorCode::Internal,
                format!("failed to start terminal service runtime: {error}"),
            )
        })?;
    let result = runtime.block_on(async {
        let listener = match adopt_systemd_listener(&socket_path)? {
            Some(listener) => listener,
            None => {
                if socket_path.exists() {
                    std::fs::remove_file(&socket_path).map_err(identity_io_error)?;
                }
                tokio::net::UnixListener::bind(&socket_path).map_err(identity_io_error)?
            }
        };
        debug_assert!(matches!(readiness, ReadinessState::DependenciesReady));
        readiness = ReadinessState::ListenerReady;
        write_service_identity(&runtime_root, &identity)?;
        debug_assert!(matches!(readiness, ReadinessState::ListenerReady));
        readiness = ReadinessState::IdentityPublished;
        notify_systemd_ready()?;

        let cancellation = CancellationToken::new();
        let signal = cancellation.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                let mut terminate =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if result.is_ok() { signal.cancel(); }
                    }
                    _ = async {
                        if let Ok(receiver) = terminate.as_mut() {
                            receiver.recv().await;
                        }
                    } => signal.cancel(),
                }
            }
            #[cfg(not(unix))]
            if tokio::signal::ctrl_c().await.is_ok() {
                signal.cancel();
            }
        });
        let served = serve_unix_listener(service, listener, cancellation).await;
        served.map_err(|error| {
            ProcessError::new(
                ProcessErrorCode::Internal,
                format!("terminal service transport failed: {error}"),
            )
        })
    });
    let shutdown = supervisor.shutdown();
    let removed = remove_service_identity(&runtime_root);
    let _ = lifetime_lock.unlock();
    let _final_readiness = readiness;
    result.and(shutdown).and(removed)
}

#[cfg(unix)]
fn adopt_systemd_listener(
    socket_path: &std::path::Path,
) -> Result<Option<tokio::net::UnixListener>> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::net::SocketAddr;

    if std::env::var_os("AGL_TERMINALD_SYSTEMD_ACTIVATION").is_none() {
        return Ok(None);
    }
    let expected_pid = std::process::id().to_string();
    if std::env::var("LISTEN_PID").as_deref() != Ok(expected_pid.as_str())
        || std::env::var("LISTEN_FDS").as_deref() != Ok("1")
        || std::env::var("LISTEN_FDNAMES").as_deref() != Ok("agl-terminal")
    {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "systemd activation requires exactly one agl-terminal named descriptor",
        ));
    }
    // SAFETY: systemd's activation ABI assigns descriptor 3 to the first
    // passed descriptor. The PID/count/name checks admit that descriptor once.
    let listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) };
    let descriptor = listener.as_raw_fd();
    let mut socket_type = 0;
    let mut socket_type_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: pointers refer to initialized storage of the declared length.
    if unsafe {
        libc::getsockopt(
            descriptor,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&raw mut socket_type).cast(),
            &raw mut socket_type_len,
        )
    } != 0
        || socket_type != libc::SOCK_STREAM
    {
        return Err(identity_io_error(std::io::Error::last_os_error()));
    }
    let mut accepting = 0;
    let mut accepting_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: pointers refer to initialized storage of the declared length.
    if unsafe {
        libc::getsockopt(
            descriptor,
            libc::SOL_SOCKET,
            libc::SO_ACCEPTCONN,
            (&raw mut accepting).cast(),
            &raw mut accepting_len,
        )
    } != 0
        || accepting != 1
    {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "activated descriptor is not an accepting Unix stream listener",
        ));
    }
    let mut raw_address = std::mem::MaybeUninit::<libc::sockaddr_un>::zeroed();
    let mut raw_address_len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    // SAFETY: the address buffer is large enough for sockaddr_un and the
    // kernel initializes the returned length.
    if unsafe {
        libc::getsockname(
            descriptor,
            raw_address.as_mut_ptr().cast(),
            &raw mut raw_address_len,
        )
    } != 0
    {
        return Err(identity_io_error(std::io::Error::last_os_error()));
    }
    let local_address: SocketAddr = listener.local_addr().map_err(identity_io_error)?;
    if local_address.as_pathname() != Some(socket_path) {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "activated descriptor address does not match AGL_TERMINALD_SOCKET",
        ));
    }
    // SAFETY: fcntl operates on the owned live listener descriptor.
    let descriptor_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if descriptor_flags < 0
        || unsafe {
            libc::fcntl(
                descriptor,
                libc::F_SETFD,
                descriptor_flags | libc::FD_CLOEXEC,
            )
        } != 0
    {
        return Err(identity_io_error(std::io::Error::last_os_error()));
    }
    listener.set_nonblocking(true).map_err(identity_io_error)?;
    // SAFETY: these process-global activation values are consumed before
    // other service tasks start.
    unsafe {
        std::env::remove_var("LISTEN_PID");
        std::env::remove_var("LISTEN_FDS");
        std::env::remove_var("LISTEN_FDNAMES");
    }
    tokio::net::UnixListener::from_std(listener)
        .map(Some)
        .map_err(identity_io_error)
}

#[cfg(not(unix))]
fn adopt_systemd_listener(
    _socket_path: &std::path::Path,
) -> Result<Option<tokio::net::UnixListener>> {
    if std::env::var_os("AGL_TERMINALD_SYSTEMD_ACTIVATION").is_some() {
        return Err(ProcessError::new(
            ProcessErrorCode::Unsupported,
            "systemd activation is supported only on Unix",
        ));
    }
    Ok(None)
}

const SERVICE_IDENTITY_FILE: &str = "service-identity.json";

fn prepare_runtime_root(runtime_root: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::create_dir_all(runtime_root).map_err(identity_io_error)?;
    #[cfg(unix)]
    std::fs::set_permissions(runtime_root, std::fs::Permissions::from_mode(0o700))
        .map_err(identity_io_error)?;
    Ok(())
}

fn write_service_identity(
    state_root: &std::path::Path,
    identity: &agl_terminal_protocol::ServiceIdentity,
) -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    std::fs::create_dir_all(state_root).map_err(identity_io_error)?;
    #[cfg(unix)]
    std::fs::set_permissions(state_root, std::fs::Permissions::from_mode(0o700))
        .map_err(identity_io_error)?;
    let encoded = serde_json::to_vec(identity).map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::Internal,
            format!("failed to encode terminal service identity: {error}"),
        )
    })?;
    let temporary = state_root.join(format!(
        ".{SERVICE_IDENTITY_FILE}.{}-{}",
        std::process::id(),
        identity.process_generation_id()
    ));
    let destination = state_root.join(SERVICE_IDENTITY_FILE);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let result = (|| {
        use std::io::Write as _;
        let mut file = options.open(&temporary).map_err(identity_io_error)?;
        file.write_all(&encoded).map_err(identity_io_error)?;
        file.sync_all().map_err(identity_io_error)?;
        std::fs::rename(&temporary, &destination).map_err(identity_io_error)?;
        #[cfg(unix)]
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o600))
            .map_err(identity_io_error)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn remove_service_identity(runtime_root: &std::path::Path) -> Result<()> {
    let path = runtime_root.join(SERVICE_IDENTITY_FILE);
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(identity_io_error(error)),
    }
}

#[cfg(unix)]
fn notify_systemd_ready() -> Result<()> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::net::UnixDatagram;

    let Some(address) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    let socket = UnixDatagram::unbound().map_err(identity_io_error)?;

    #[cfg(target_os = "linux")]
    if let Some(name) = address.as_bytes().strip_prefix(b"@") {
        let mut raw = std::mem::MaybeUninit::<libc::sockaddr_un>::zeroed();
        // SAFETY: zeroed sockaddr_un is initialized below before use.
        let raw = unsafe { raw.assume_init_mut() };
        if name.is_empty() || name.len() >= raw.sun_path.len() {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "NOTIFY_SOCKET abstract address is invalid",
            ));
        }
        raw.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (destination, source) in raw.sun_path[1..].iter_mut().zip(name) {
            *destination = *source as libc::c_char;
        }
        let address_length = std::mem::offset_of!(libc::sockaddr_un, sun_path) + 1 + name.len();
        let payload = b"READY=1\nSTATUS=terminal listener and identity ready";
        // SAFETY: the descriptor is live, the payload is valid for its length,
        // and sockaddr_un is initialized through the exact abstract-name length.
        let sent = unsafe {
            libc::sendto(
                socket.as_raw_fd(),
                payload.as_ptr().cast(),
                payload.len(),
                0,
                (raw as *const libc::sockaddr_un).cast(),
                address_length as libc::socklen_t,
            )
        };
        if sent < 0 {
            return Err(identity_io_error(std::io::Error::last_os_error()));
        }
        return Ok(());
    }

    socket
        .send_to(
            b"READY=1\nSTATUS=terminal listener and identity ready",
            std::path::PathBuf::from(address),
        )
        .map_err(identity_io_error)?;
    Ok(())
}

#[cfg(not(unix))]
fn notify_systemd_ready() -> Result<()> {
    Ok(())
}

fn identity_io_error(error: std::io::Error) -> ProcessError {
    ProcessError::new(
        ProcessErrorCode::Internal,
        format!("failed to publish terminal service identity: {error}"),
    )
}

#[cfg(test)]
mod identity_tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use agl_exec::{AuthorityFingerprint, ServiceGenerationId};
    use agl_terminal_protocol::{
        ServiceIdentity, TERMINAL_PROTOCOL_VERSION, TerminalGenerationIdentity,
    };

    use super::*;

    #[test]
    fn service_identity_is_private_atomic_and_exact() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "agl-terminal-identity-{}-{nonce}",
            std::process::id()
        ));
        let identity = ServiceIdentity::new(
            TerminalGenerationIdentity::new(
                AuthorityFingerprint::new(format!("sha256:{}", "b".repeat(64))).unwrap(),
                "c".repeat(40),
                AuthorityFingerprint::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
                TERMINAL_PROTOCOL_VERSION,
            )
            .unwrap(),
            ServiceGenerationId::generate(),
        )
        .unwrap();
        write_service_identity(&root, &identity).unwrap();
        let path = root.join(SERVICE_IDENTITY_FILE);
        assert_eq!(
            serde_json::from_slice::<ServiceIdentity>(&std::fs::read(&path).unwrap()).unwrap(),
            identity
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_readiness_supports_abstract_notify_sockets() {
        use std::os::linux::net::SocketAddrExt as _;
        use std::os::unix::net::{SocketAddr, UnixDatagram};
        use std::sync::Mutex;

        static NOTIFY_ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = NOTIFY_ENV_LOCK.lock().unwrap();
        let name = format!(
            "agl178-notify-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let receiver =
            UnixDatagram::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap())
                .unwrap();
        let previous = std::env::var_os("NOTIFY_SOCKET");
        // SAFETY: the process-global value is serialized for this test and is
        // restored before releasing the lock.
        unsafe { std::env::set_var("NOTIFY_SOCKET", format!("@{name}")) };
        let notified = notify_systemd_ready();
        // SAFETY: see the serialized mutation above.
        unsafe {
            match previous {
                Some(value) => std::env::set_var("NOTIFY_SOCKET", value),
                None => std::env::remove_var("NOTIFY_SOCKET"),
            }
        }
        notified.unwrap();
        let mut payload = [0_u8; 128];
        let size = receiver.recv(&mut payload).unwrap();
        assert!(payload[..size].starts_with(b"READY=1\n"));
    }
}
