mod service;
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
#[cfg(test)]
mod test_support;

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
pub use service::{TerminalService, serve_unix};
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
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use agl_exec::{AuthorityFingerprint, ServiceGenerationId};
    use agl_terminal::environment::RejectTerminalSecrets;
    use agl_terminal_protocol::{ServiceIdentity, TERMINAL_PROTOCOL_VERSION};
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
    let launcher_path = required_path("AGL_TERMINALD_LAUNCHER")?;
    let data_root = required_path("AGL_TERMINALD_DATA_ROOT")?;
    let state_root = required_path("AGL_TERMINALD_STATE_ROOT")?;
    let build_id = std::env::var("AGL_TERMINALD_BUILD_ID").map_err(|_| {
        ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "AGL_TERMINALD_BUILD_ID must be configured",
        )
    })?;
    let identity = ServiceIdentity {
        protocol_version: TERMINAL_PROTOCOL_VERSION,
        crate_version: env!("CARGO_PKG_VERSION").to_owned(),
        build_id: AuthorityFingerprint::new(build_id).map_err(|error| {
            ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                format!("AGL_TERMINALD_BUILD_ID is invalid: {error}"),
            )
        })?,
        generation_id: ServiceGenerationId::generate(),
    };
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
    let execution_repository = Arc::new(InMemoryExecutionRepository::new());
    let spool = Arc::new(FileOutputSpool::new(state_root.join("spool"))?);
    let supervisor = ProcessSupervisor::start(options, execution_repository, spool)?;
    let process = supervisor.handle();
    let terminal_repository = Arc::new(InMemoryTerminalRepository::new());
    let registry = Arc::new(TerminalRegistry::new(
        process.clone(),
        Arc::new(RejectTerminalSecrets),
        terminal_repository,
    )?);
    let service = Arc::new(
        TerminalService::new(identity, registry, process).map_err(|error| {
            ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                format!("terminal service identity is invalid: {error}"),
            )
        })?,
    );
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
        let cancellation = CancellationToken::new();
        let signal = cancellation.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signal.cancel();
            }
        });
        serve_unix(service, &socket_path, cancellation)
            .await
            .map_err(|error| {
                ProcessError::new(
                    ProcessErrorCode::Internal,
                    format!("terminal service transport failed: {error}"),
                )
            })
    });
    let shutdown = supervisor.shutdown();
    result.and(shutdown)
}
