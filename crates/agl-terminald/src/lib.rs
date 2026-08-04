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
pub use supervisor::{ProcessHandle, ProcessSupervisor};
pub use terminal::environment::{
    RejectTerminalSecrets, ResolvedTerminalEnvironment, TerminalEnvironmentDigest,
    TerminalEnvironmentRequest, TerminalEnvironmentValue, TerminalSecretReference,
    TerminalSecretResolver, TerminalSecretValue,
};
pub use terminal::history::{
    EphemeralTerminalHistory, HumanShellHistoryStore, TerminalHistoryOwner, TerminalHistorySeed,
};
pub use terminal::registry::{
    TerminalEnsureRequest, TerminalRecord, TerminalRegistry, TerminalState,
};
pub use terminal::repository::{
    InMemoryTerminalRepository, StoredTerminalRecord, TerminalRepository, TerminalReservation,
    terminal_slot_key, validate_terminal_replacement, validate_terminal_reservation,
};
pub use terminal::shell::{
    AdmittedShellKind, AdmittedShellProfile, BoundedShellIntegration, CommandBoundary,
    HostStartupPolicy, IntegrationBatch, ShellExit, ShellIntegrationControl, ShellIntegrationEvent,
    ShellIntegrationHealth, ShellIntegrationNotice, ShellIntegrationState, TerminalPromptState,
    TypedCommandAbortReason, TypedCommandTransactionId,
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
    Err(ProcessError::new(
        ProcessErrorCode::PlatformUnsupported,
        "agl-terminald transport endpoint is not configured",
    ))
}
