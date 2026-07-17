mod bytes;
mod config;
mod context;
mod error;
mod platform;
mod repository;
mod request;
mod spool;
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

pub use bytes::{ProcessBytes, ProcessBytesEncoding};
pub use config::{
    ProcessPlatformDiagnostics, ProcessSupervisorOptions, WRITABLE_INPUT_LEASE_HEARTBEAT,
    WRITABLE_INPUT_LEASE_TTL,
};
pub use context::{ExecutionContextSnapshot, resolve_execution_directory};
pub use error::{ProcessError, ProcessErrorCode, Result};
pub use repository::{
    CommittedOutputFrame, ExecutionRepository, ExecutionTerminalUpdate,
    InMemoryExecutionRepository, OutputSpool,
};
pub use request::{
    EnvironmentOverride, ExecutionAuthorization, ExecutionGrantLease, ExecutionIo, ExecutionKind,
    ExecutionLimits, ExecutionOwner, ExecutionProfile, ExecutionRequest, ShellProfileSnapshot,
    TerminalSize,
};
pub use spool::FileOutputSpool;
pub use status::{
    ExecutionChannel, ExecutionCursor, ExecutionExit, ExecutionListFilter, ExecutionOutputChunk,
    ExecutionPrivateCommand, ExecutionReadResult, ExecutionState, ExecutionStatus, InputLease,
    KillMode,
};
pub use supervisor::{ProcessHandle, ProcessSupervisor};

#[doc(hidden)]
pub use platform::launcher_main;

pub fn process_platform_diagnostics(
    launcher_path: impl AsRef<std::path::Path>,
) -> ProcessPlatformDiagnostics {
    platform::diagnostics(launcher_path.as_ref())
}
