mod command;
mod command_queue;
pub mod environment;
pub mod history;
mod identity;
mod lifecycle;
mod repository;
mod shell_integration;
mod shell_materialization;
mod shell_profile;

pub use agl_exec::{
    AuthorityFingerprint, CallerNamespace, CallerOwner, CallerOwnerKind, CallerRole, OpaqueOwnerId,
};
pub use identity::{ParseTerminalIdentityError, TerminalId, TerminalRequestId, TerminalStreamId};
pub use lifecycle::{
    TerminalDescriptor, TerminalDomainError, TerminalOperation, TerminalOwner, TerminalRecord,
    TerminalState, TerminalTopologyId, validate_terminal_transition,
};
pub use repository::{
    InMemoryTerminalRepository, StoredTerminalRecord, TerminalRepository, TerminalReservation,
    terminal_slot_key, validate_terminal_replacement, validate_terminal_reservation,
};

pub use command::{
    CommandCardSanitizer, MAX_HUMAN_TERMINAL_COMMAND_BYTES, MAX_TYPED_TERMINAL_COMMAND_BYTES,
    SanitizedTerminalOutput, human_terminal_command_submission, sanitize_terminal_card_output,
};
pub use command_queue::{
    AgentTerminalCommandQueue, DEFAULT_AGENT_TERMINAL_QUEUE_CAPACITY,
    HumanTerminalCommandAdmission, MAX_AGENT_TERMINAL_COMMAND_BYTES, QueuedTerminalCommand,
    TerminalCommandOutputRange, TerminalCommandResult,
};
pub use shell_integration::{
    BoundedShellIntegration, CommandBoundary, IntegrationBatch, MAX_SHELL_INTEGRATION_FRAME_BYTES,
    ShellExit, ShellIntegrationControl, ShellIntegrationEvent, ShellIntegrationHealth,
    ShellIntegrationNotice, ShellIntegrationState, ShellIntegrationToken, TerminalPromptState,
    TypedCommandAbortReason, TypedCommandTransactionId,
};
pub use shell_materialization::{
    ManagedShellIntegrationTransport, ManagedShellStartup, MaterializedManagedShell,
};
pub use shell_profile::{
    AdmittedShellKind, AdmittedShellProfile, HostStartupPolicy, ManagedShellLaunchPlan,
    ShellAdapterCapability, ShellAdapterDescriptor, ShellStartupPaths,
};
