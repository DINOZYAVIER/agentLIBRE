mod config;
mod platform;
mod repository;
mod request;
mod status;
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

pub use agl_exec::{
    CommittedOutputFrame, ExecutionChannel, ExecutionContextSnapshot, ExecutionCursor,
    ExecutionExit, ExecutionId, ExecutionOutputChunk, ExecutionPrivateCommand, ExecutionReadResult,
    ExecutionRequestId, ExecutionState, ExecutionTerminalUpdate, FileOutputSpool, InputLease,
    KillMode, OutputSpool, OutputSpoolRead, ProcessBytes, ProcessBytesEncoding, ProcessError,
    ProcessErrorCode, ProcessSupervisorOptions, Result, WRITABLE_INPUT_LEASE_HEARTBEAT,
    WRITABLE_INPUT_LEASE_TTL, WriterLeaseId, resolve_execution_directory,
};
pub use config::ProcessPlatformDiagnostics;
pub use repository::{ExecutionRepository, InMemoryExecutionRepository};
pub use request::{
    EnvironmentOverride, ExecutionAuthorization, ExecutionGrantLease, ExecutionIo, ExecutionKind,
    ExecutionLeaseOrigin, ExecutionLimits, ExecutionOwner, ExecutionProfile, ExecutionRequest,
    LOCAL_OPERATOR_TERMINAL_LEASE_DURATION, ShellProfileSnapshot, TerminalSize,
};
pub use status::{ExecutionListFilter, ExecutionStatus, ShellIntegrationReadResult};
pub use supervisor::{ProcessHandle, ProcessSupervisor};
pub use terminal::command::{
    AgentTerminalCommandQueue, CommandCardSanitizer, HumanTerminalCommandAdmission,
    QueuedTerminalCommand, SanitizedTerminalOutput, TerminalCommandOutputRange,
    TerminalCommandResult, human_terminal_command_submission, sanitize_terminal_card_output,
};
pub use terminal::environment::{
    RejectTerminalSecrets, ResolvedTerminalEnvironment, TerminalEnvironmentDigest,
    TerminalEnvironmentRequest, TerminalEnvironmentValue, TerminalSecretReference,
    TerminalSecretResolver, TerminalSecretValue,
};
pub use terminal::history::{
    EphemeralTerminalHistory, HumanShellHistoryStore, TerminalHistoryOwner, TerminalHistorySeed,
};
pub use terminal::registry::{
    TerminalEnsureRequest, TerminalOwner, TerminalRecord, TerminalRegistry, TerminalState,
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
pub use platform::launcher_main;

pub fn process_platform_diagnostics(
    launcher_path: impl AsRef<std::path::Path>,
) -> ProcessPlatformDiagnostics {
    platform::diagnostics(launcher_path.as_ref())
}

#[doc(hidden)]
pub fn verify_process_launcher_identity(launcher_path: impl AsRef<std::path::Path>) -> Result<()> {
    platform::verify_launcher_binary_identity(launcher_path.as_ref())
}

/// Returns the existing canonical Linux runtime roots admitted by the
/// workspace sandbox. Aliases such as `/bin` and `/usr/bin` are deduplicated
/// by filesystem identity after canonicalization.
pub fn process_standard_runtime_roots() -> Result<Vec<std::path::PathBuf>> {
    platform::standard_runtime_roots()
}
