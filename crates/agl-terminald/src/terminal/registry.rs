use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Debug, Formatter};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use agl_exec::{
    CallerOwnerId, ExecutionCorrelation, ExecutionId, ExecutionRequestId, LifecycleScopeId,
};
pub use agl_terminal::TerminalOwner;
use agl_terminal::{
    AgentTerminalCommandQueue, HumanTerminalCommandAdmission, TerminalCommandResult, TerminalId,
    TerminalRecord, TerminalState, TerminalTopologyId, human_terminal_command_submission,
};
use sha2::{Digest as _, Sha256};

use crate::terminal::environment::{
    TerminalEnvironmentDigest, TerminalEnvironmentRequest, TerminalSecretResolver,
};
use crate::terminal::history::TerminalHistorySeed;
use crate::terminal::shell::{
    AdmittedShellKind, AdmittedShellProfile, BoundedShellIntegration, CommandBoundary,
    HostStartupPolicy, IntegrationBatch, MAX_SHELL_INTEGRATION_FRAME_BYTES, ManagedShellStartup,
    ShellIntegrationControl, ShellIntegrationEvent, ShellIntegrationHealth, ShellIntegrationNotice,
    ShellIntegrationToken, TerminalPromptState, TypedCommandAbortReason, TypedCommandTransactionId,
};
use crate::{
    ExecutionAuthorization, ExecutionContextSnapshot, ExecutionGrantLease, ExecutionIo,
    ExecutionKind, ExecutionLimits, ExecutionOwner, ExecutionProfile, ExecutionRequest,
    ExecutionState, ExecutionStatus, InputLease, KillMode, ProcessBytes, ProcessError,
    ProcessErrorCode, ProcessHandle, Result, ShellIntegrationReadResult, TerminalSize,
    WRITABLE_INPUT_LEASE_HEARTBEAT,
};
use agl_terminal::{
    StoredTerminalRecord, TerminalRepository, TerminalReservation, terminal_slot_key,
};

const AGENT_TERMINAL_DRIVE_INTERVAL: Duration = Duration::from_millis(5);
const AGENT_TERMINAL_PROMPT_RECOVERY_TIMEOUT: Duration = Duration::from_secs(2);
const AGENT_TERMINAL_PROMOTION_QUIESCE_TIMEOUT: Duration = Duration::from_secs(2);
const AGENT_TERMINAL_INTEGRATION_READ_BYTES: usize = MAX_SHELL_INTEGRATION_FRAME_BYTES;

pub struct TerminalEnsureRequest {
    pub topology_id: TerminalTopologyId,
    pub owner: TerminalOwner,
    pub lifecycle_scope_id: LifecycleScopeId,
    pub correlation: ExecutionCorrelation,
    pub context: ExecutionContextSnapshot,
    pub profile: ExecutionProfile,
    pub shell: AdmittedShellProfile,
    pub environment: TerminalEnvironmentRequest,
    pub runtime_read_only_roots: Vec<PathBuf>,
    pub host_startup: HostStartupPolicy,
    pub authorization: ExecutionAuthorization,
    pub grant_lease: Option<ExecutionGrantLease>,
    pub terminal_size: TerminalSize,
    pub limits: ExecutionLimits,
    pub history_seed: TerminalHistorySeed,
}

impl Debug for TerminalEnsureRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TerminalEnsureRequest")
            .field("topology_id", &self.topology_id)
            .field("owner", &self.owner)
            .field("lifecycle_scope_id", &self.lifecycle_scope_id)
            .field("correlation", &self.correlation)
            .field("context_revision", &self.context.revision)
            .field("profile", &self.profile)
            .field("shell", &self.shell)
            .field("environment", &self.environment)
            .field("runtime_read_only_roots", &self.runtime_read_only_roots)
            .field("host_startup", &self.host_startup)
            .field("authorization", &self.authorization)
            .field("grant_lease", &self.grant_lease)
            .field("terminal_size", &self.terminal_size)
            .field("limits", &self.limits)
            .field("history_seed", &self.history_seed)
            .finish()
    }
}

/// Sole-owner terminal lifecycle registry used by `agl-terminald`.
pub struct TerminalRegistry {
    starter: Arc<dyn TerminalExecutionStarter>,
    secrets: Arc<dyn TerminalSecretResolver>,
    repository: Arc<dyn TerminalRepository>,
    inner: Mutex<RegistryState>,
}

#[derive(Default)]
struct RegistryState {
    slots: BTreeMap<TerminalSlot, TerminalId>,
    terminals: BTreeMap<TerminalId, TerminalEntry>,
    retired_owners: BTreeSet<CallerOwnerId>,
}

struct TerminalEntry {
    slot: TerminalSlot,
    fingerprint: String,
    active_slot: bool,
    record: TerminalRecord,
    integration: Option<BoundedShellIntegration>,
    commands: AgentTerminalCommandQueue,
    completed_commands: BTreeMap<u64, Result<TerminalCommandResult>>,
    output_sequence: u64,
    agent_input_lease: Option<InputLease>,
    renew_input_lease_at: Option<Instant>,
    command_timeout: Option<AgentCommandTimeout>,
    pending_prompt_result: Option<PendingAgentCommandResult>,
    command_driver_busy: bool,
    pending_human_command: Option<PendingHumanCommand>,
    pending_agent_transaction: Option<PendingAgentTransaction>,
    active_typed_command: Option<ActiveTypedCommand>,
    raw_human_input_pending: bool,
}

struct PendingHumanCommand {
    command_sequence: u64,
    transaction_id: TypedCommandTransactionId,
    canonical_command: String,
}

struct PendingAgentTransaction {
    command_sequence: u64,
    transaction_id: TypedCommandTransactionId,
    canonical_command: String,
}

struct ActiveTypedCommand {
    command_sequence: u64,
    transaction_id: TypedCommandTransactionId,
    owner: TypedCommandOwner,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TypedCommandOwner {
    Human,
    Agent,
}

struct AgentCommandTimeout {
    command_sequence: u64,
    outcome: ProcessErrorCode,
    recover_by: Instant,
    interrupt_sent: bool,
}

struct PendingAgentCommandResult {
    command_sequence: u64,
    outcome: ProcessError,
    recover_by: Instant,
}

enum AgentDriveAction {
    Submit {
        execution_id: ExecutionId,
        command_sequence: u64,
        submission: ProcessBytes,
    },
    RenewLease {
        execution_id: ExecutionId,
        command_sequence: u64,
        lease: InputLease,
    },
    Interrupt {
        execution_id: ExecutionId,
        command_sequence: u64,
    },
    Terminate {
        execution_id: ExecutionId,
        lease: Option<InputLease>,
    },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum TerminalSlot {
    HumanWorkspace(TerminalTopologyId),
    HumanHost(TerminalTopologyId),
    PersistentAgent(TerminalTopologyId),
    EphemeralAgent(CallerOwnerId),
    Promoted(TerminalId),
}

trait TerminalExecutionStarter: Send + Sync {
    fn start(
        &self,
        execution_id: ExecutionId,
        request: ExecutionRequest,
        startup: ManagedShellStartup,
    ) -> Result<ExecutionStatus>;

    fn status(&self, execution_id: &ExecutionId) -> Result<ExecutionStatus>;

    fn kill(&self, execution_id: &ExecutionId, mode: KillMode) -> Result<()>;

    fn read_shell_integration(
        &self,
        execution_id: &ExecutionId,
        maximum_bytes: usize,
    ) -> Result<ShellIntegrationReadResult>;

    fn send_shell_integration_control(
        &self,
        execution_id: &ExecutionId,
        frame: ProcessBytes,
    ) -> Result<u64>;

    fn attach(
        &self,
        execution_id: &ExecutionId,
        attachment_id: ExecutionRequestId,
    ) -> Result<InputLease>;

    fn detach(&self, execution_id: &ExecutionId, lease: InputLease) -> Result<()>;

    fn renew_input_lease(&self, execution_id: &ExecutionId, lease: InputLease) -> Result<()>;

    fn write(
        &self,
        execution_id: &ExecutionId,
        lease: InputLease,
        bytes: ProcessBytes,
        eof: bool,
    ) -> Result<()>;

    fn interrupt_foreground(&self, execution_id: &ExecutionId) -> Result<()>;

    fn handoff_managed_terminal(
        &self,
        execution_id: &ExecutionId,
        owner: ExecutionOwner,
        interrupt_foreground: bool,
    ) -> Result<()>;
}

struct SupervisorTerminalStarter(ProcessHandle);

impl TerminalExecutionStarter for SupervisorTerminalStarter {
    fn start(
        &self,
        execution_id: ExecutionId,
        request: ExecutionRequest,
        startup: ManagedShellStartup,
    ) -> Result<ExecutionStatus> {
        self.0
            .start_reserved_managed_terminal(execution_id, request, startup)
    }

    fn status(&self, execution_id: &ExecutionId) -> Result<ExecutionStatus> {
        self.0.operator_status(execution_id)
    }

    fn kill(&self, execution_id: &ExecutionId, mode: KillMode) -> Result<()> {
        self.0.operator_kill(execution_id, mode)
    }

    fn read_shell_integration(
        &self,
        execution_id: &ExecutionId,
        maximum_bytes: usize,
    ) -> Result<ShellIntegrationReadResult> {
        self.0
            .operator_read_shell_integration(execution_id, maximum_bytes)
    }

    fn send_shell_integration_control(
        &self,
        execution_id: &ExecutionId,
        frame: ProcessBytes,
    ) -> Result<u64> {
        self.0
            .operator_send_shell_integration_control(execution_id, frame)
    }

    fn attach(
        &self,
        execution_id: &ExecutionId,
        attachment_id: ExecutionRequestId,
    ) -> Result<InputLease> {
        self.0.operator_attach(execution_id, attachment_id, true)
    }

    fn detach(&self, execution_id: &ExecutionId, lease: InputLease) -> Result<()> {
        self.0.operator_detach(execution_id, lease)
    }

    fn renew_input_lease(&self, execution_id: &ExecutionId, lease: InputLease) -> Result<()> {
        self.0.operator_renew_input_lease(execution_id, lease)
    }

    fn write(
        &self,
        execution_id: &ExecutionId,
        lease: InputLease,
        bytes: ProcessBytes,
        eof: bool,
    ) -> Result<()> {
        self.0.operator_write(execution_id, lease, bytes, eof)
    }

    fn interrupt_foreground(&self, execution_id: &ExecutionId) -> Result<()> {
        self.0.operator_interrupt_foreground(execution_id)
    }

    fn handoff_managed_terminal(
        &self,
        execution_id: &ExecutionId,
        owner: ExecutionOwner,
        interrupt_foreground: bool,
    ) -> Result<()> {
        self.0
            .operator_handoff_managed_terminal(execution_id, owner, interrupt_foreground)
    }
}

impl TerminalRegistry {
    pub fn new(
        process: ProcessHandle,
        secrets: Arc<dyn TerminalSecretResolver>,
        repository: Arc<dyn TerminalRepository>,
    ) -> Result<Self> {
        Self::from_parts(
            Arc::new(SupervisorTerminalStarter(process)),
            secrets,
            repository,
        )
    }

    #[cfg(test)]
    fn with_starter(
        starter: Arc<dyn TerminalExecutionStarter>,
        secrets: Arc<dyn TerminalSecretResolver>,
        repository: Arc<dyn TerminalRepository>,
    ) -> Result<Self> {
        Self::from_parts(starter, secrets, repository)
    }

    fn from_parts(
        starter: Arc<dyn TerminalExecutionStarter>,
        secrets: Arc<dyn TerminalSecretResolver>,
        repository: Arc<dyn TerminalRepository>,
    ) -> Result<Self> {
        let recovered = repository.recover_for_new_owner()?;
        let state = recover_registry_state(recovered)?;
        Ok(Self {
            starter,
            secrets,
            repository,
            inner: Mutex::new(state),
        })
    }

    /// Idempotently admits the one topology slot named by `request`. The
    /// registry lock is deliberately held through the bounded supervisor
    /// admission so concurrent retries cannot launch two shells for one slot.
    pub fn ensure_terminal(&self, request: TerminalEnsureRequest) -> Result<TerminalRecord> {
        validate_ensure_request(&request)?;
        let environment_admission = request.environment.admit()?;
        if !environment_admission.names().any(|name| name == "PATH") {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "persistent terminal requires an explicitly admitted PATH",
            ));
        }
        let environment_digest = environment_admission.digest().clone();
        let slot = terminal_slot(&request)?;
        let fingerprint = terminal_fingerprint(&request, &environment_digest);
        let mut state = self.lock()?;
        if request.owner.is_ephemeral()
            && state
                .retired_owners
                .contains(request.owner.caller().owner_id())
        {
            return Err(ProcessError::new(
                ProcessErrorCode::ExecutionNotOwned,
                "subagent terminal ownership has ended or was promoted",
            ));
        }
        if let Some(existing_id) = state.slots.get(&slot) {
            let existing = state.terminals.get(existing_id).ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "terminal topology index refers to a missing record",
                )
            })?;
            if existing.fingerprint != fingerprint {
                return Err(ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "terminal topology slot already has different immutable admission metadata",
                ));
            }
            return Ok(existing.record.clone());
        }

        let terminal_id = TerminalId::generate();
        let execution_id = ExecutionId::generate();
        let record = TerminalRecord {
            terminal_id: terminal_id.clone(),
            execution_id: execution_id.clone(),
            topology_id: request.topology_id.clone(),
            owner: request.owner.clone(),
            lifecycle_scope_id: request.lifecycle_scope_id.clone(),
            profile: request.profile,
            workspace_root: request.context.workspace_root.clone(),
            shell_profile: request.shell.clone(),
            environment_digest,
            command_sequence: 0,
            prompt_state: TerminalPromptState::Unknown,
            integration_health: ShellIntegrationHealth::AwaitingFirstPrompt,
            cwd: request.context.working_directory.clone(),
            state: TerminalState::Starting,
        };
        let reserved = StoredTerminalRecord {
            record: record.clone(),
            slot_key: terminal_slot_key(&record)?,
            fingerprint: fingerprint.clone(),
            active_slot: true,
        };
        match self.repository.reserve(&reserved)? {
            TerminalReservation::Created => {
                insert_stored_terminal(&mut state, reserved)?;
            }
            TerminalReservation::Existing(existing) => {
                let existing = *existing;
                if terminal_slot_for_record(&existing.record)? != slot {
                    return Err(ProcessError::new(
                        ProcessErrorCode::StoreCorrupt,
                        "durable terminal retry resolved to a different topology slot",
                    ));
                }
                let record = existing.record.clone();
                insert_stored_terminal(&mut state, existing)?;
                return Ok(record);
            }
        }

        let environment = match environment_admission.resolve(self.secrets.as_ref()) {
            Ok(environment) => environment,
            Err(error) => {
                return self.fail_reserved_admission(&mut state, &terminal_id, error);
            }
        };
        let (public_environment, private_environment) = environment.into_launch_parts();
        let execution_owner = execution_owner(&request);
        let execution_request = ExecutionRequest {
            owner: execution_owner,
            correlation: request.correlation.clone(),
            kind: ExecutionKind::Shell,
            program: request.shell.snapshot.program.clone(),
            argv0: request.shell.snapshot.program.display().to_string(),
            program_digest: Some(request.shell.snapshot.executable_digest.clone()),
            args: Vec::new(),
            workspace_root: request.context.workspace_root.clone(),
            cwd: request.context.working_directory.clone(),
            read_only_roots: request.runtime_read_only_roots.clone(),
            environment: public_environment,
            stdin: None,
            close_stdin_after_initial: false,
            io: ExecutionIo::Pty,
            terminal_size: Some(request.terminal_size),
            profile: request.profile,
            authorization: request.authorization,
            grant_lease: request.grant_lease.clone(),
            limits: request.limits.clone(),
        };
        let integration_token = match ShellIntegrationToken::generate() {
            Ok(token) => token,
            Err(error) => {
                return self.fail_reserved_admission(&mut state, &terminal_id, error);
            }
        };
        let integration = BoundedShellIntegration::new(integration_token.clone());
        let startup = ManagedShellStartup {
            shell: request.shell.clone(),
            host_startup: request.host_startup.clone(),
            history_seed: request.history_seed,
            integration_token,
            private_environment,
        };
        let status = match self
            .starter
            .start(execution_id.clone(), execution_request, startup)
        {
            Ok(status) => status,
            Err(error) => {
                return self.fail_reserved_admission(&mut state, &terminal_id, error);
            }
        };
        if status.execution_id != execution_id {
            let _ = self.starter.kill(&execution_id, KillMode::Immediate);
            return self.fail_reserved_admission(
                &mut state,
                &terminal_id,
                ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "managed terminal starter returned a different reserved execution identity",
                ),
            );
        }
        let mut running = record;
        running.cwd = status.cwd;
        running.state = terminal_state(status.state);
        let active_slot = running.state.is_live() || running.state == TerminalState::OutcomeUnknown;
        let persisted = replace_terminal_record(
            self.repository.as_ref(),
            &mut state,
            &terminal_id,
            running,
            slot,
            active_slot,
        );
        let record = match persisted {
            Ok(record) => record,
            Err(error) => {
                let _ = self.starter.kill(&execution_id, KillMode::Immediate);
                mark_terminal_outcome_unknown(&mut state, &terminal_id, None)?;
                return Err(error);
            }
        };
        let entry = state
            .terminals
            .get_mut(&terminal_id)
            .expect("reserved terminal was inserted before spawn");
        entry.output_sequence = status.last_sequence;
        if record.state.is_live() {
            entry.integration = Some(integration);
        }
        Ok(record)
    }

    fn fail_reserved_admission(
        &self,
        state: &mut RegistryState,
        terminal_id: &TerminalId,
        launch_error: ProcessError,
    ) -> Result<TerminalRecord> {
        let (mut failed, slot) = {
            let entry = state
                .terminals
                .get(terminal_id)
                .ok_or_else(|| terminal_not_found(terminal_id))?;
            (entry.record.clone(), entry.slot.clone())
        };
        failed.state = TerminalState::Failed;
        match replace_terminal_record(
            self.repository.as_ref(),
            state,
            terminal_id,
            failed,
            slot,
            false,
        ) {
            Ok(_) => Err(launch_error),
            Err(persistence_error) => {
                mark_terminal_outcome_unknown(state, terminal_id, None)?;
                Err(persistence_error)
            }
        }
    }

    pub fn execute_agent_command(
        &self,
        request: TerminalEnsureRequest,
        command: String,
        deadline: Option<Instant>,
    ) -> Result<TerminalCommandResult> {
        self.execute_agent_command_cancellable(request, command, deadline, || false)
    }

    /// Atomically admits one explicit Human command only at the trusted prompt
    /// generation observed by the client. The caller must write `submission`
    /// through the already-owned Human input lease and call
    /// `cancel_human_command_admission` if that write fails.
    pub fn admit_human_command(
        &self,
        topology_id: &TerminalTopologyId,
        terminal_id: &TerminalId,
        expected_command_sequence: u64,
        expected_prompt_generation: u64,
        command: &str,
    ) -> Result<HumanTerminalCommandAdmission> {
        let submission = human_terminal_command_submission(command)?;
        let transaction_id = TypedCommandTransactionId::generate()?;
        let mut state = self.lock()?;
        let entry = state
            .terminals
            .get_mut(terminal_id)
            .ok_or_else(|| terminal_not_found(terminal_id))?;
        if &entry.record.topology_id != topology_id {
            return Err(ProcessError::new(
                ProcessErrorCode::ExecutionNotOwned,
                "Human terminal command requires the terminal's owning session",
            ));
        }
        if !matches!(
            entry.record.state,
            TerminalState::Starting | TerminalState::Running
        ) {
            return Err(ProcessError::new(
                ProcessErrorCode::ExecutionNotLive,
                "Human terminal command requires a live terminal",
            ));
        }
        if entry.raw_human_input_pending {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "Human terminal has pending raw input; attach Terminal instead",
            ));
        }
        if entry.pending_human_command.is_some()
            || entry.pending_agent_transaction.is_some()
            || entry.active_typed_command.is_some()
        {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "Human terminal is busy; attach Terminal instead",
            ));
        }
        if entry.record.command_sequence != expected_command_sequence {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "Human terminal command sequence changed; refresh before submitting",
            ));
        }
        let TerminalPromptState::Ready { sequence, .. } = entry.record.prompt_state else {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "Human terminal is not at a trusted fresh prompt; attach Terminal instead",
            ));
        };
        if sequence != expected_prompt_generation {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "Human terminal prompt generation changed; refresh before submitting",
            ));
        }
        let command_sequence = entry
            .record
            .command_sequence
            .checked_add(1)
            .ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "Human terminal command sequence overflowed",
                )
            })?;
        let execution_id = entry.record.execution_id.clone();
        let integration = entry.integration.as_ref().ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "Human terminal has no private shell integration driver",
            )
        })?;
        let arm = integration.encode_control(&ShellIntegrationControl::ArmTypedCommand {
            transaction_id: transaction_id.clone(),
            expected_command_sequence: command_sequence,
        })?;
        let output_after_sequence = match self
            .starter
            .send_shell_integration_control(&execution_id, ProcessBytes::from_bytes(&arm))
        {
            Ok(sequence) => sequence,
            Err(error) => {
                if let Some(integration) = entry.integration.as_mut() {
                    integration.mark_unavailable();
                    sync_integration_projection(&mut entry.record, integration);
                }
                drop(state);
                let _ = self.starter.kill(&execution_id, KillMode::Immediate);
                return Err(error);
            }
        };
        entry.record.prompt_state = TerminalPromptState::Unknown;
        entry.pending_human_command = Some(PendingHumanCommand {
            command_sequence,
            transaction_id,
            canonical_command: command.to_owned(),
        });
        Ok(HumanTerminalCommandAdmission {
            terminal_id: terminal_id.clone(),
            execution_id,
            command_sequence,
            output_after_sequence,
            submission,
        })
    }

    /// Writes the exact transaction returned by `admit_human_command` through
    /// the same per-terminal transition gate used by raw Human input. A raw
    /// writer cannot enter while this admission is pending.
    pub fn write_admitted_human_command(
        &self,
        terminal_id: &TerminalId,
        execution_id: &ExecutionId,
        command_sequence: u64,
        lease: InputLease,
        submission: ProcessBytes,
    ) -> Result<()> {
        let canonical_submission = {
            let state = self.lock()?;
            let entry = state
                .terminals
                .get(terminal_id)
                .ok_or_else(|| terminal_not_found(terminal_id))?;
            if entry.record.execution_id != *execution_id
                || !entry.record.owner.accepts_human_control()
            {
                return Err(ProcessError::new(
                    ProcessErrorCode::ExecutionNotOwned,
                    "Human command admission does not own this terminal execution",
                ));
            }
            let pending = entry
                .pending_human_command
                .as_ref()
                .filter(|pending| pending.command_sequence == command_sequence)
                .ok_or_else(|| {
                    ProcessError::new(
                        ProcessErrorCode::StateConflict,
                        "Human command input transition is no longer pending",
                    )
                })?;
            if pending.command_sequence != command_sequence {
                return Err(ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "Human command input transition is no longer pending",
                ));
            }
            if entry.raw_human_input_pending {
                return Err(ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "Human terminal raw input already owns the input transition",
                ));
            }
            human_terminal_command_submission(&pending.canonical_command)?
        };
        if submission != canonical_submission {
            let _ = self.cancel_human_command_admission(terminal_id, command_sequence);
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "Human command submission bytes differ from the armed canonical command",
            ));
        }

        let result = self.starter.write(execution_id, lease, submission, false);
        if result.is_err() {
            let _ = self.cancel_human_command_admission(terminal_id, command_sequence);
        }
        result
    }

    /// Routes raw input for a managed Human terminal through the shared input
    /// transition gate. Returns `Ok(false)` when `execution_id` is not a
    /// managed Human terminal, allowing the caller to use the generic process
    /// input path. Once any raw input wins, typed command admission remains
    /// closed until private shell integration observes a newer trusted prompt.
    pub fn write_raw_human_input_if_managed(
        &self,
        execution_id: &ExecutionId,
        lease: InputLease,
        bytes: ProcessBytes,
        eof: bool,
    ) -> Result<bool> {
        {
            let mut state = self.lock()?;
            let terminal_id = state.terminals.iter().find_map(|(terminal_id, entry)| {
                (entry.record.execution_id == *execution_id
                    && entry.record.owner.accepts_human_control())
                .then(|| terminal_id.clone())
            });
            let Some(terminal_id) = terminal_id else {
                return Ok(false);
            };
            let (mut record, slot, active_slot, pending_human_command) = {
                let entry = state
                    .terminals
                    .get(&terminal_id)
                    .expect("Human terminal execution was selected above");
                (
                    entry.record.clone(),
                    entry.slot.clone(),
                    entry.active_slot,
                    entry.pending_human_command.is_some(),
                )
            };
            if !record.state.is_live() {
                return Err(ProcessError::new(
                    ProcessErrorCode::ExecutionNotLive,
                    "raw Human terminal input requires a live terminal",
                ));
            }
            if pending_human_command {
                return Err(ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "Human terminal input transition is busy with a typed command",
                ));
            }
            if eof || !bytes.data.is_empty() {
                if matches!(record.prompt_state, TerminalPromptState::Ready { .. }) {
                    record.prompt_state = TerminalPromptState::Unknown;
                    if let Err(error) = replace_terminal_record(
                        self.repository.as_ref(),
                        &mut state,
                        &terminal_id,
                        record,
                        slot,
                        active_slot,
                    ) {
                        mark_terminal_outcome_unknown(&mut state, &terminal_id, None)?;
                        return Err(error);
                    }
                }
                state
                    .terminals
                    .get_mut(&terminal_id)
                    .expect("Human terminal was checked before its raw-input transition")
                    .raw_human_input_pending = true;
            }
        }

        // Fail closed on write error: the raw-input latch intentionally stays
        // armed because the physical delivery outcome may be unknown.
        self.starter
            .write(execution_id, lease, bytes, eof)
            .map(|()| true)
    }

    pub fn cancel_human_command_admission(
        &self,
        terminal_id: &TerminalId,
        command_sequence: u64,
    ) -> Result<()> {
        let mut state = self.lock()?;
        let entry = state
            .terminals
            .get_mut(terminal_id)
            .ok_or_else(|| terminal_not_found(terminal_id))?;
        let pending = entry
            .pending_human_command
            .as_ref()
            .filter(|pending| pending.command_sequence == command_sequence)
            .map(|pending| {
                (
                    pending.transaction_id.clone(),
                    entry.record.execution_id.clone(),
                )
            });
        let Some((transaction_id, execution_id)) = pending else {
            return Ok(());
        };
        let integration = entry.integration.as_ref().ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "Human terminal has no private shell integration driver",
            )
        })?;
        let disarm = integration.encode_control(&ShellIntegrationControl::DisarmTypedCommand {
            transaction_id,
            reason: TypedCommandAbortReason::Cancelled,
        })?;
        let disarmed = self
            .starter
            .send_shell_integration_control(&execution_id, ProcessBytes::from_bytes(&disarm));
        entry.pending_human_command = None;
        if let Err(error) = disarmed {
            if let Some(integration) = entry.integration.as_mut() {
                integration.mark_unavailable();
                sync_integration_projection(&mut entry.record, integration);
            }
            drop(state);
            let _ = self.starter.kill(&execution_id, KillMode::Immediate);
            return Err(error);
        }
        Ok(())
    }

    pub fn execute_agent_command_cancellable(
        &self,
        request: TerminalEnsureRequest,
        command: String,
        deadline: Option<Instant>,
        cancelled: impl Fn() -> bool,
    ) -> Result<TerminalCommandResult> {
        if !request.owner.is_agent() || request.profile != ExecutionProfile::Workspace {
            return Err(ProcessError::new(
                ProcessErrorCode::HostAuthorityRequired,
                "persistent agent commands require a workspace MainAgent or Subagent terminal",
            ));
        }
        let expected_owner = request.owner.clone();
        let terminal = self.ensure_terminal(request)?;
        let command_sequence = {
            let mut state = self.lock()?;
            let entry = state
                .terminals
                .get_mut(&terminal.terminal_id)
                .ok_or_else(|| terminal_not_found(&terminal.terminal_id))?;
            entry.commands.enqueue(command, deadline)?
        };

        loop {
            match self.poll_private_integration(
                &terminal.terminal_id,
                AGENT_TERMINAL_INTEGRATION_READ_BYTES,
            ) {
                Ok(_) => {}
                Err(error) if error.code() == ProcessErrorCode::InputBackpressure => {}
                Err(error) => {
                    self.fail_agent_command(
                        &terminal.terminal_id,
                        command_sequence,
                        error.clone(),
                    )?;
                    return Err(error);
                }
            }
            match self.refresh(&terminal.terminal_id) {
                Ok(_) => {}
                Err(error) if error.code() == ProcessErrorCode::InputBackpressure => {}
                Err(error) => {
                    self.fail_agent_command(
                        &terminal.terminal_id,
                        command_sequence,
                        error.clone(),
                    )?;
                    return Err(error);
                }
            }

            let now = Instant::now();
            let requested_outcome = if cancelled() {
                Some(ProcessErrorCode::Cancelled)
            } else if deadline.is_some_and(|deadline| now >= deadline) {
                Some(ProcessErrorCode::TimedOut)
            } else {
                None
            };
            let action = {
                let mut state = self.lock()?;
                let entry = state
                    .terminals
                    .get_mut(&terminal.terminal_id)
                    .ok_or_else(|| terminal_not_found(&terminal.terminal_id))?;
                if let Some(result) = entry.completed_commands.remove(&command_sequence) {
                    return result;
                }
                if entry.record.owner != expected_owner {
                    entry.commands.cancel_queued(command_sequence);
                    if entry.commands.active_sequence() == Some(command_sequence) {
                        entry.commands.cancel_active();
                    }
                    return Err(ProcessError::new(
                        ProcessErrorCode::ExecutionNotOwned,
                        "persistent terminal agent ownership was revoked",
                    ));
                }
                if !matches!(
                    entry.record.state,
                    TerminalState::Starting | TerminalState::Running
                ) {
                    fail_all_agent_commands(
                        entry,
                        ProcessError::new(
                            ProcessErrorCode::ExecutionNotLive,
                            "persistent agent terminal is not live",
                        ),
                    );
                    if let Some(result) = entry.completed_commands.remove(&command_sequence) {
                        return result;
                    }
                }
                if let Some(outcome) = requested_outcome {
                    if entry.commands.cancel_queued(command_sequence) {
                        return Err(agent_command_outcome_error(outcome));
                    }
                    if entry.commands.active_sequence() == Some(command_sequence)
                        && !entry.commands.active_is_submitted()
                    {
                        entry.commands.cancel_active();
                        entry.command_driver_busy = false;
                        return Err(agent_command_outcome_error(outcome));
                    }
                    if entry.commands.active_sequence() == Some(command_sequence)
                        && entry.command_timeout.is_none()
                    {
                        entry.command_timeout = Some(AgentCommandTimeout {
                            command_sequence,
                            outcome,
                            recover_by: now + AGENT_TERMINAL_PROMPT_RECOVERY_TIMEOUT,
                            interrupt_sent: false,
                        });
                    }
                }
                plan_agent_drive(entry, now)?
            };
            if let Some(action) = action {
                self.execute_agent_drive_action(&terminal.terminal_id, action)?;
            } else {
                thread::sleep(AGENT_TERMINAL_DRIVE_INTERVAL);
            }
        }
    }

    fn fail_agent_command(
        &self,
        terminal_id: &TerminalId,
        command_sequence: u64,
        error: ProcessError,
    ) -> Result<()> {
        let mut state = self.lock()?;
        let entry = state
            .terminals
            .get_mut(terminal_id)
            .ok_or_else(|| terminal_not_found(terminal_id))?;
        if entry.commands.cancel_queued(command_sequence)
            || entry.commands.active_sequence() == Some(command_sequence)
        {
            if entry.commands.active_sequence() == Some(command_sequence) {
                entry.commands.cancel_active();
            }
            entry
                .completed_commands
                .insert(command_sequence, Err(error));
        }
        entry.command_driver_busy = false;
        Ok(())
    }

    fn execute_agent_drive_action(
        &self,
        terminal_id: &TerminalId,
        action: AgentDriveAction,
    ) -> Result<()> {
        match action {
            AgentDriveAction::Submit {
                execution_id,
                command_sequence,
                submission,
            } => {
                self.submit_agent_command(terminal_id, &execution_id, command_sequence, submission)
            }
            AgentDriveAction::RenewLease {
                execution_id,
                command_sequence,
                lease,
            } => {
                let renewed = self.starter.renew_input_lease(&execution_id, lease.clone());
                let mut state = self.lock()?;
                let entry = state
                    .terminals
                    .get_mut(terminal_id)
                    .ok_or_else(|| terminal_not_found(terminal_id))?;
                entry.command_driver_busy = false;
                let terminate = match renewed {
                    Ok(()) if entry.commands.active_sequence() == Some(command_sequence) => {
                        entry.renew_input_lease_at =
                            Some(Instant::now() + WRITABLE_INPUT_LEASE_HEARTBEAT);
                        false
                    }
                    Ok(()) => false,
                    Err(error) => {
                        fail_all_agent_commands(entry, error);
                        entry.agent_input_lease = None;
                        entry.renew_input_lease_at = None;
                        true
                    }
                };
                drop(state);
                if terminate {
                    self.terminate_terminal(terminal_id, KillMode::Immediate)?;
                }
                Ok(())
            }
            AgentDriveAction::Interrupt {
                execution_id,
                command_sequence,
            } => {
                let interrupted = self.starter.interrupt_foreground(&execution_id);
                let mut state = self.lock()?;
                let entry = state
                    .terminals
                    .get_mut(terminal_id)
                    .ok_or_else(|| terminal_not_found(terminal_id))?;
                entry.command_driver_busy = false;
                if let Some(timeout) = entry
                    .command_timeout
                    .as_mut()
                    .filter(|timeout| timeout.command_sequence == command_sequence)
                    && match &interrupted {
                        Ok(()) => true,
                        Err(error) => error.code() != ProcessErrorCode::InputBackpressure,
                    }
                {
                    timeout.interrupt_sent = true;
                }
                Ok(())
            }
            AgentDriveAction::Terminate {
                execution_id,
                lease,
            } => {
                if let Some(lease) = lease {
                    let _ = self.starter.detach(&execution_id, lease);
                }
                let terminated = self.terminate_terminal(terminal_id, KillMode::Immediate);
                if let Ok(mut state) = self.lock()
                    && let Some(entry) = state.terminals.get_mut(terminal_id)
                {
                    entry.command_driver_busy = false;
                }
                terminated
            }
        }
    }

    fn submit_agent_command(
        &self,
        terminal_id: &TerminalId,
        execution_id: &ExecutionId,
        command_sequence: u64,
        submission: ProcessBytes,
    ) -> Result<()> {
        let transaction_id = TypedCommandTransactionId::generate()?;
        let previous_prompt = {
            let mut state = self.lock()?;
            let entry = state
                .terminals
                .get_mut(terminal_id)
                .ok_or_else(|| terminal_not_found(terminal_id))?;
            if entry.commands.active_sequence() != Some(command_sequence)
                || entry.pending_human_command.is_some()
                || entry.pending_agent_transaction.is_some()
                || entry.active_typed_command.is_some()
            {
                entry.command_driver_busy = false;
                return Err(ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "agent terminal typed-input transition is no longer available",
                ));
            }
            let canonical_command = entry
                .commands
                .active_command()
                .ok_or_else(|| {
                    ProcessError::new(
                        ProcessErrorCode::StateConflict,
                        "agent terminal has no canonical active command",
                    )
                })?
                .to_owned();
            let integration = entry.integration.as_ref().ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "agent terminal has no private shell integration driver",
                )
            })?;
            let arm = integration.encode_control(&ShellIntegrationControl::ArmTypedCommand {
                transaction_id: transaction_id.clone(),
                expected_command_sequence: command_sequence,
            })?;
            if let Err(error) = self
                .starter
                .send_shell_integration_control(execution_id, ProcessBytes::from_bytes(&arm))
            {
                if let Some(integration) = entry.integration.as_mut() {
                    integration.mark_unavailable();
                    entry.record.prompt_state = integration.state().prompt().clone();
                    entry.record.integration_health = integration.state().health();
                }
                entry.command_driver_busy = false;
                drop(state);
                let _ = self.starter.kill(execution_id, KillMode::Immediate);
                return Err(error);
            }
            let previous_prompt = entry.record.prompt_state.clone();
            entry.record.prompt_state = TerminalPromptState::Unknown;
            entry.pending_agent_transaction = Some(PendingAgentTransaction {
                command_sequence,
                transaction_id: transaction_id.clone(),
                canonical_command,
            });
            previous_prompt
        };

        let attachment_id = ExecutionRequestId::generate();
        let attached = self.starter.attach(execution_id, attachment_id);
        let (lease, submitted) = match attached {
            Ok(lease) => {
                let submitted = self
                    .starter
                    .write(execution_id, lease.clone(), submission, false);
                (Some(lease), submitted)
            }
            Err(error) => (None, Err(error)),
        };
        let disarm_failed = if submitted.is_err() {
            let mut state = self.lock()?;
            let entry = state
                .terminals
                .get_mut(terminal_id)
                .ok_or_else(|| terminal_not_found(terminal_id))?;
            let disarm = entry
                .integration
                .as_ref()
                .ok_or_else(|| {
                    ProcessError::new(
                        ProcessErrorCode::StateConflict,
                        "agent terminal lost its private shell integration driver",
                    )
                })?
                .encode_control(&ShellIntegrationControl::DisarmTypedCommand {
                    transaction_id: transaction_id.clone(),
                    reason: TypedCommandAbortReason::InputWriteFailed,
                })?;
            let disarmed = self
                .starter
                .send_shell_integration_control(execution_id, ProcessBytes::from_bytes(&disarm));
            entry.pending_agent_transaction = None;
            if disarmed.is_ok() {
                entry.record.prompt_state = previous_prompt;
                false
            } else {
                if let Some(integration) = entry.integration.as_mut() {
                    integration.mark_unavailable();
                    entry.record.prompt_state = integration.state().prompt().clone();
                    entry.record.integration_health = integration.state().health();
                }
                true
            }
        } else {
            false
        };
        let mut detach = None;
        let mut terminate = disarm_failed;
        {
            let mut state = self.lock()?;
            let entry = state
                .terminals
                .get_mut(terminal_id)
                .ok_or_else(|| terminal_not_found(terminal_id))?;
            entry.command_driver_busy = false;
            if entry.commands.active_sequence() != Some(command_sequence) {
                detach = lease;
            } else {
                match submitted {
                    Ok(()) => {
                        entry.commands.complete_submission()?;
                        entry.agent_input_lease = lease;
                        entry.renew_input_lease_at =
                            Some(Instant::now() + WRITABLE_INPUT_LEASE_HEARTBEAT);
                    }
                    Err(error)
                        if matches!(
                            error.code(),
                            ProcessErrorCode::InputLeaseBusy | ProcessErrorCode::InputBackpressure
                        ) =>
                    {
                        entry.commands.abandon_submission();
                        detach = lease;
                    }
                    Err(error) => {
                        entry.commands.cancel_active();
                        entry
                            .completed_commands
                            .insert(command_sequence, Err(error));
                        entry.command_timeout = None;
                        entry.commands.abandon_submission();
                        detach = lease;
                        terminate = true;
                    }
                }
            }
        }
        if let Some(lease) = detach {
            let _ = self.starter.detach(execution_id, lease);
        }
        if terminate {
            self.terminate_terminal(terminal_id, KillMode::Immediate)?;
        }
        Ok(())
    }

    pub fn record(&self, terminal_id: &TerminalId) -> Result<TerminalRecord> {
        self.lock()?
            .terminals
            .get(terminal_id)
            .map(|entry| entry.record.clone())
            .ok_or_else(|| terminal_not_found(terminal_id))
    }

    pub fn list_topology(&self, topology_id: &TerminalTopologyId) -> Result<Vec<TerminalRecord>> {
        Ok(self
            .lock()?
            .terminals
            .values()
            .filter(|entry| &entry.record.topology_id == topology_id)
            .map(|entry| entry.record.clone())
            .collect())
    }

    pub fn refresh(&self, terminal_id: &TerminalId) -> Result<TerminalRecord> {
        let (execution_id, current, owns_driver) = {
            let state = self.lock()?;
            let entry = state
                .terminals
                .get(terminal_id)
                .ok_or_else(|| terminal_not_found(terminal_id))?;
            (
                entry.record.execution_id.clone(),
                entry.record.clone(),
                entry.integration.is_some(),
            )
        };
        if !current.state.is_live() || !owns_driver {
            return Ok(current);
        }
        let status = self.starter.status(&execution_id)?;
        self.persist_execution_status(terminal_id, status)
    }

    fn persist_execution_status(
        &self,
        terminal_id: &TerminalId,
        status: ExecutionStatus,
    ) -> Result<TerminalRecord> {
        let mut state = self.lock()?;
        let (mut record, slot, mut integration) = {
            let entry = state
                .terminals
                .get(terminal_id)
                .ok_or_else(|| terminal_not_found(terminal_id))?;
            if entry.record.execution_id != status.execution_id {
                return Err(ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "process status does not belong to the requested terminal",
                ));
            }
            if !entry.record.state.is_live() {
                return Ok(entry.record.clone());
            }
            (
                entry.record.clone(),
                entry.slot.clone(),
                entry.integration.clone(),
            )
        };
        record.state = merge_terminal_state(record.state, terminal_state(status.state));
        if record.integration_health != ShellIntegrationHealth::Trusted {
            record.cwd = status.cwd;
        }
        if !record.state.is_live() {
            if let Some(integration) = integration.as_mut() {
                integration.channel_closed();
                sync_integration_projection(&mut record, integration);
            } else {
                record.prompt_state = TerminalPromptState::Degraded;
                record.integration_health = ShellIntegrationHealth::Degraded;
            }
        }
        let active_slot = record.state.is_live() || record.state == TerminalState::OutcomeUnknown;
        let record = replace_terminal_record(
            self.repository.as_ref(),
            &mut state,
            terminal_id,
            record,
            slot,
            active_slot,
        )?;
        let entry = state
            .terminals
            .get_mut(terminal_id)
            .expect("terminal was durably replaced above");
        entry.output_sequence = status.last_sequence;
        if !record.state.is_live() {
            entry.integration = integration;
        }
        Ok(record)
    }

    /// Removes a completed terminal from its active topology slot while
    /// retaining the immutable record for finished-process presentation.
    ///
    /// Retirement is idempotent and requires a confirmed known terminal
    /// execution outcome. Live, stopping, and `outcome_unknown` terminals keep
    /// their slot so `/workspace` cannot accidentally admit a second shell
    /// before the old authority boundary is known to be gone.
    pub fn retire_terminal_slot(&self, terminal_id: &TerminalId) -> Result<TerminalRecord> {
        let current = self.record(terminal_id)?;
        match current.state {
            TerminalState::Exited | TerminalState::Failed => return Ok(current),
            TerminalState::OutcomeUnknown => {
                return Err(ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "terminal with outcome_unknown cannot release its topology slot",
                ));
            }
            TerminalState::Reserved
            | TerminalState::Starting
            | TerminalState::Running
            | TerminalState::Stopping => {}
        }
        let owns_driver = self
            .lock()?
            .terminals
            .get(terminal_id)
            .is_some_and(|entry| entry.integration.is_some());
        if !owns_driver {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "terminal without current runtime ownership cannot confirm retirement",
            ));
        }
        let status = self.starter.status(&current.execution_id)?;
        let refreshed = self.persist_execution_status(terminal_id, status)?;
        match refreshed.state {
            TerminalState::Exited | TerminalState::Failed => Ok(refreshed),
            TerminalState::OutcomeUnknown => Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "terminal with outcome_unknown cannot release its topology slot",
            )),
            TerminalState::Reserved
            | TerminalState::Starting
            | TerminalState::Running
            | TerminalState::Stopping => Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "live terminal cannot release its topology slot",
            )),
        }
    }

    /// Accepts bytes only from the private integration transport owned by the
    /// daemon. PTY output has no call path to this method.
    pub fn accept_private_integration(
        &self,
        terminal_id: &TerminalId,
        bytes: &[u8],
    ) -> Result<IntegrationBatch> {
        self.accept_private_integration_sample(terminal_id, bytes, None)
    }

    fn accept_private_integration_sample(
        &self,
        terminal_id: &TerminalId,
        bytes: &[u8],
        foreground_sample: Option<Option<i32>>,
    ) -> Result<IntegrationBatch> {
        let mut state = self.lock()?;
        let (mut integration, mut record, slot, active_slot, raw_human_input_pending) = {
            let entry = state
                .terminals
                .get(terminal_id)
                .ok_or_else(|| terminal_not_found(terminal_id))?;
            if !entry.record.state.is_live() {
                return Err(ProcessError::new(
                    ProcessErrorCode::ExecutionNotLive,
                    "private shell integration belongs to a non-live terminal",
                ));
            }
            let integration = entry.integration.clone().ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "terminal recovered without private shell integration ownership",
                )
            })?;
            (
                integration,
                entry.record.clone(),
                entry.slot.clone(),
                entry.active_slot,
                entry.raw_human_input_pending,
            )
        };
        let previous = record.clone();
        let mut batch = IntegrationBatch::default();
        if let Some(process_group) = foreground_sample {
            merge_integration_batch(&mut batch, integration.observe_foreground(process_group));
        }
        if batch.notice.is_none() {
            merge_integration_batch(&mut batch, integration.push(bytes));
        }
        // Sampling on both sides of the drained shell frames gives the
        // combined sequence the right ordering whether this poll first sees a
        // command start or a command finish.
        if batch.notice.is_none()
            && let Some(process_group) = foreground_sample
        {
            merge_integration_batch(&mut batch, integration.observe_foreground(process_group));
        }
        if batch.notice.is_none() {
            for event in &batch.events {
                if let Err(error) = apply_event_to_record(&mut record, event) {
                    record = previous.clone();
                    let degraded = integration.mark_unavailable();
                    batch.events.clear();
                    batch.notice = degraded.notice.or_else(|| {
                        Some(ShellIntegrationNotice {
                            code: "shell_integration_degraded",
                            message: error.message().to_owned(),
                        })
                    });
                    break;
                }
            }
        }
        let trusted_prompt_after_raw_input = raw_human_input_pending
            && matches!(
                batch.events.last(),
                Some(ShellIntegrationEvent::PromptReady { .. })
            );
        sync_integration_projection(&mut record, &integration);
        if raw_human_input_pending
            && !trusted_prompt_after_raw_input
            && matches!(record.prompt_state, TerminalPromptState::Ready { .. })
        {
            record.prompt_state = TerminalPromptState::Unknown;
        }
        if record != previous
            && let Err(error) = replace_terminal_record(
                self.repository.as_ref(),
                &mut state,
                terminal_id,
                record,
                slot,
                active_slot,
            )
        {
            mark_terminal_outcome_unknown(&mut state, terminal_id, None)?;
            return Err(error);
        }
        let entry = state
            .terminals
            .get_mut(terminal_id)
            .expect("terminal was checked before integration persistence");
        entry.integration = Some(integration);
        if trusted_prompt_after_raw_input {
            entry.raw_human_input_pending = false;
        }
        Ok(batch)
    }

    fn update_private_integration(
        &self,
        terminal_id: &TerminalId,
        update: impl FnOnce(&mut BoundedShellIntegration) -> IntegrationBatch,
    ) -> Result<IntegrationBatch> {
        let mut state = self.lock()?;
        let (mut integration, mut record, slot, active_slot, raw_human_input_pending) = {
            let entry = state
                .terminals
                .get(terminal_id)
                .ok_or_else(|| terminal_not_found(terminal_id))?;
            let integration = entry.integration.clone().ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "terminal has no current private shell integration ownership",
                )
            })?;
            (
                integration,
                entry.record.clone(),
                entry.slot.clone(),
                entry.active_slot,
                entry.raw_human_input_pending,
            )
        };
        let previous = record.clone();
        let batch = update(&mut integration);
        sync_integration_projection(&mut record, &integration);
        if raw_human_input_pending
            && matches!(record.prompt_state, TerminalPromptState::Ready { .. })
        {
            record.prompt_state = TerminalPromptState::Unknown;
        }
        if record != previous
            && let Err(error) = replace_terminal_record(
                self.repository.as_ref(),
                &mut state,
                terminal_id,
                record,
                slot,
                active_slot,
            )
        {
            mark_terminal_outcome_unknown(&mut state, terminal_id, None)?;
            return Err(error);
        }
        state
            .terminals
            .get_mut(terminal_id)
            .expect("terminal was checked before integration persistence")
            .integration = Some(integration);
        Ok(batch)
    }

    /// Drains and applies only the supervisor-owned private shell integration
    /// channel. This is the shared monitor entrypoint for daemon projection
    /// and persistent agent commands; callers must never feed PTY bytes here.
    pub fn poll_private_integration(
        &self,
        terminal_id: &TerminalId,
        maximum_bytes: usize,
    ) -> Result<IntegrationBatch> {
        if maximum_bytes == 0 {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "private shell integration poll bound must be nonzero",
            ));
        }
        let execution_id = {
            let state = self.lock()?;
            let entry = state
                .terminals
                .get(terminal_id)
                .ok_or_else(|| terminal_not_found(terminal_id))?;
            if !entry.record.state.is_live() {
                return Err(ProcessError::new(
                    ProcessErrorCode::ExecutionNotLive,
                    "private shell integration belongs to a non-live terminal",
                ));
            }
            if entry.integration.is_none() {
                return Err(ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "recovered terminal has no private shell integration driver",
                ));
            }
            entry.record.execution_id.clone()
        };
        let read = self
            .starter
            .read_shell_integration(&execution_id, maximum_bytes)?;
        let bytes = read.bytes.decode(maximum_bytes)?;
        let status = self.starter.status(&execution_id)?;
        let (batch, transaction_must_terminate) = self.accept_acknowledged_private_packet(
            terminal_id,
            &bytes,
            read.foreground_process_group,
            read.degraded,
            read.channel_closed,
        )?;
        self.persist_execution_status(terminal_id, status)?;
        let mut detach = None;
        let mut terminate = transaction_must_terminate;
        {
            let mut state = self.lock()?;
            let entry = state
                .terminals
                .get_mut(terminal_id)
                .ok_or_else(|| terminal_not_found(terminal_id))?;
            if batch.notice.is_some() {
                fail_all_agent_commands(
                    entry,
                    ProcessError::new(
                        ProcessErrorCode::StateConflict,
                        "agent terminal shell integration is degraded",
                    ),
                );
                detach = entry.agent_input_lease.take();
                entry.renew_input_lease_at = None;
                terminate |= entry.record.owner.is_agent() && entry.record.state.is_live();
            } else if entry.commands.active_sequence().is_none() {
                detach = entry.agent_input_lease.take();
                entry.renew_input_lease_at = None;
            }
        }
        if let Some(lease) = detach {
            let _ = self.starter.detach(&execution_id, lease);
        }
        if terminate {
            self.terminate_terminal(terminal_id, KillMode::Immediate)?;
        }
        Ok(batch)
    }

    fn accept_acknowledged_private_packet(
        &self,
        terminal_id: &TerminalId,
        bytes: &[u8],
        foreground_sample: Option<i32>,
        transport_degraded: bool,
        channel_closed: bool,
    ) -> Result<(IntegrationBatch, bool)> {
        let mut state = self.lock()?;
        let entry = state
            .terminals
            .get_mut(terminal_id)
            .ok_or_else(|| terminal_not_found(terminal_id))?;
        if !entry.record.state.is_live() {
            return Err(ProcessError::new(
                ProcessErrorCode::ExecutionNotLive,
                "private shell integration belongs to a non-live terminal",
            ));
        }
        let mut integration = entry.integration.take().ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "terminal recovered without private shell integration ownership",
            )
        })?;
        let previous_record = entry.record.clone();
        let mut record = previous_record.clone();
        let slot = entry.slot.clone();
        let active_slot = entry.active_slot;
        let execution_id = entry.record.execution_id.clone();
        let mut force_prompt_unknown = false;
        let mut transaction_must_terminate = false;
        let mut batch = if bytes.is_empty() {
            integration.observe_foreground(foreground_sample)
        } else {
            integration.push_packet(bytes)
        };

        if batch.notice.is_none() {
            let events = batch.events.clone();
            for event in events {
                let result = (|| -> Result<()> {
                    match &event {
                        ShellIntegrationEvent::PromptReady {
                            sequence,
                            input_pending,
                            ..
                        } => {
                            let mut next_record = record.clone();
                            apply_event_to_record(&mut next_record, &event)?;
                            if entry.pending_human_command.is_some()
                                || entry.pending_agent_transaction.is_some()
                                || entry.active_typed_command.is_some()
                            {
                                Err(ProcessError::new(
                                    ProcessErrorCode::StateConflict,
                                    "prompt_ready arrived while a typed command transaction was active",
                                ))
                            } else {
                                let prompt_generation = (!*input_pending
                                    && !entry.raw_human_input_pending)
                                    .then_some(*sequence);
                                let shell_sequence =
                                    integration.last_shell_sequence().ok_or_else(|| {
                                        ProcessError::new(
                                            ProcessErrorCode::StateConflict,
                                            "prompt_ready omitted its shell event sequence",
                                        )
                                    })?;
                                let acknowledgement = integration.encode_control(
                                    &ShellIntegrationControl::PromptReadyAck {
                                        event_sequence: shell_sequence,
                                        prompt_generation,
                                    },
                                )?;
                                self.starter.send_shell_integration_control(
                                    &execution_id,
                                    ProcessBytes::from_bytes(&acknowledgement),
                                )?;
                                // A previously latched raw write has now made this
                                // prompt dirty. Consume that latch, while a fresh
                                // kernel probe keeps the next transition dirty.
                                entry.raw_human_input_pending = *input_pending;
                                force_prompt_unknown = prompt_generation.is_none();
                                if prompt_generation.is_some()
                                    && let Some(pending) = entry.pending_prompt_result.take()
                                {
                                    entry
                                        .completed_commands
                                        .insert(pending.command_sequence, Err(pending.outcome));
                                }
                                record = next_record;
                                Ok(())
                            }
                        }
                        ShellIntegrationEvent::CommandStarted {
                            sequence,
                            transaction_id,
                            command,
                            ..
                        } => {
                            let mut next_record = record.clone();
                            apply_event_to_record(&mut next_record, &event)?;
                            let next_command_sequence =
                                record.command_sequence.checked_add(1).ok_or_else(|| {
                                    ProcessError::new(
                                        ProcessErrorCode::StateConflict,
                                        "terminal command sequence overflowed",
                                    )
                                })?;
                            match transaction_id {
                                None => {
                                    if entry.pending_human_command.is_some()
                                        || entry.pending_agent_transaction.is_some()
                                        || entry.active_typed_command.is_some()
                                    {
                                        Err(ProcessError::new(
                                            ProcessErrorCode::StateConflict,
                                            "unarmed command_started raced an armed typed transaction",
                                        ))
                                    } else {
                                        entry.raw_human_input_pending = false;
                                        record = next_record;
                                        Ok(())
                                    }
                                }
                                Some(transaction_id) => {
                                    let expected = entry
                                    .pending_human_command
                                    .as_ref()
                                    .map(|pending| {
                                        (
                                            TypedCommandOwner::Human,
                                            pending.command_sequence,
                                            pending.transaction_id.clone(),
                                            pending.canonical_command.clone(),
                                        )
                                    })
                                    .or_else(|| {
                                        entry.pending_agent_transaction.as_ref().map(|pending| {
                                            (
                                                TypedCommandOwner::Agent,
                                                pending.command_sequence,
                                                pending.transaction_id.clone(),
                                                pending.canonical_command.clone(),
                                            )
                                        })
                                    })
                                    .ok_or_else(|| {
                                        ProcessError::new(
                                            ProcessErrorCode::StateConflict,
                                            "typed command_started had no armed registry transaction",
                                        )
                                    })?;
                                    if entry.active_typed_command.is_some()
                                        || expected.1 != next_command_sequence
                                        || expected.2 != *transaction_id
                                        || expected.3 != *command
                                        || (expected.0 == TypedCommandOwner::Agent
                                            && entry.commands.active_sequence() != Some(expected.1))
                                    {
                                        Err(ProcessError::new(
                                            ProcessErrorCode::StateConflict,
                                            "typed command_started did not match the armed identity, sequence, and canonical command",
                                        ))
                                    } else {
                                        let acknowledgement = integration.encode_control(
                                            &ShellIntegrationControl::CommandBoundaryAck {
                                                transaction_id: transaction_id.clone(),
                                                boundary: CommandBoundary::Started,
                                            },
                                        )?;
                                        let output_after_sequence =
                                            self.starter.send_shell_integration_control(
                                                &execution_id,
                                                ProcessBytes::from_bytes(&acknowledgement),
                                            )?;
                                        if expected.0 == TypedCommandOwner::Agent {
                                            entry
                                                .commands
                                                .mark_started(*sequence, output_after_sequence)?;
                                            entry.pending_agent_transaction = None;
                                        } else {
                                            entry.pending_human_command = None;
                                        }
                                        entry.active_typed_command = Some(ActiveTypedCommand {
                                            command_sequence: expected.1,
                                            transaction_id: transaction_id.clone(),
                                            owner: expected.0,
                                        });
                                        record = next_record;
                                        Ok(())
                                    }
                                }
                            }
                        }
                        ShellIntegrationEvent::CommandFinished {
                            sequence,
                            transaction_id,
                            exit,
                            cwd,
                        } => {
                            let mut next_record = record.clone();
                            apply_event_to_record(&mut next_record, &event)?;
                            match transaction_id {
                                None => {
                                    if entry.pending_human_command.is_some()
                                        || entry.pending_agent_transaction.is_some()
                                        || entry.active_typed_command.is_some()
                                    {
                                        Err(ProcessError::new(
                                            ProcessErrorCode::StateConflict,
                                            "unarmed command_finished raced a typed transaction",
                                        ))
                                    } else {
                                        record = next_record;
                                        Ok(())
                                    }
                                }
                                Some(transaction_id) => {
                                    let active = entry.active_typed_command.as_ref().ok_or_else(|| {
                                    ProcessError::new(
                                        ProcessErrorCode::StateConflict,
                                        "typed command_finished had no acknowledged start transaction",
                                    )
                                })?;
                                    if active.transaction_id != *transaction_id
                                        || active.command_sequence != record.command_sequence
                                    {
                                        Err(ProcessError::new(
                                            ProcessErrorCode::StateConflict,
                                            "typed command_finished did not match the acknowledged transaction",
                                        ))
                                    } else {
                                        let active_owner = active.owner;
                                        let command_sequence = active.command_sequence;
                                        let acknowledgement = integration.encode_control(
                                            &ShellIntegrationControl::CommandBoundaryAck {
                                                transaction_id: transaction_id.clone(),
                                                boundary: CommandBoundary::Finished,
                                            },
                                        )?;
                                        let output_through_sequence =
                                            self.starter.send_shell_integration_control(
                                                &execution_id,
                                                ProcessBytes::from_bytes(&acknowledgement),
                                            )?;
                                        if active_owner == TypedCommandOwner::Agent {
                                            let result = entry.commands.finish(
                                                terminal_id.clone(),
                                                execution_id.clone(),
                                                *sequence,
                                                exit.clone(),
                                                cwd.clone(),
                                                output_through_sequence,
                                            )?;
                                            if let Some(timeout) =
                                                entry.command_timeout.take().filter(|timeout| {
                                                    timeout.command_sequence == command_sequence
                                                })
                                            {
                                                entry.pending_prompt_result =
                                                    Some(PendingAgentCommandResult {
                                                        command_sequence,
                                                        outcome: agent_command_outcome_error(
                                                            timeout.outcome,
                                                        ),
                                                        recover_by: timeout.recover_by,
                                                    });
                                            } else {
                                                entry
                                                    .completed_commands
                                                    .insert(command_sequence, Ok(result));
                                            }
                                            entry.command_driver_busy = false;
                                        }
                                        entry.active_typed_command = None;
                                        record = next_record;
                                        Ok(())
                                    }
                                }
                            }
                        }
                        ShellIntegrationEvent::ForegroundChanged { .. } => Ok(()),
                    }
                })();
                if let Err(error) = result {
                    transaction_must_terminate = entry.pending_human_command.is_some()
                        || entry.pending_agent_transaction.is_some()
                        || entry.active_typed_command.is_some()
                        || matches!(
                            event,
                            ShellIntegrationEvent::CommandStarted {
                                transaction_id: Some(_),
                                ..
                            } | ShellIntegrationEvent::CommandFinished {
                                transaction_id: Some(_),
                                ..
                            }
                        );
                    integration.mark_unavailable();
                    batch.events.clear();
                    batch.notice = Some(ShellIntegrationNotice {
                        code: "shell_integration_degraded",
                        message: error.message().to_owned(),
                    });
                    break;
                }
            }
        }
        if batch.notice.is_none() && !bytes.is_empty() {
            merge_integration_batch(
                &mut batch,
                integration.observe_foreground(foreground_sample),
            );
        }

        if batch.notice.is_none() && transport_degraded {
            batch = integration.mark_unavailable();
        }
        if batch.notice.is_none() && channel_closed {
            batch = integration.channel_closed();
        }
        if batch.notice.is_some() {
            transaction_must_terminate |= entry.pending_human_command.is_some()
                || entry.pending_agent_transaction.is_some()
                || entry.active_typed_command.is_some();
        }
        sync_integration_projection(&mut record, &integration);
        if force_prompt_unknown && matches!(record.prompt_state, TerminalPromptState::Ready { .. })
        {
            record.prompt_state = TerminalPromptState::Unknown;
        }
        entry.integration = Some(integration);
        let record_changed = record != previous_record;
        if record_changed
            && let Err(error) = replace_terminal_record(
                self.repository.as_ref(),
                &mut state,
                terminal_id,
                record,
                slot,
                active_slot,
            )
        {
            mark_terminal_outcome_unknown(&mut state, terminal_id, None)?;
            return Err(error);
        }
        Ok((batch, transaction_must_terminate))
    }

    pub fn integration_closed(
        &self,
        terminal_id: &TerminalId,
    ) -> Result<Option<ShellIntegrationNotice>> {
        Ok(self
            .update_private_integration(terminal_id, |integration| integration.channel_closed())?
            .notice)
    }

    pub fn promote_ephemeral_owner(
        &self,
        terminal_id: &TerminalId,
        topology_id: &TerminalTopologyId,
        promoted_caller: agl_exec::CallerOwner,
    ) -> Result<TerminalRecord> {
        let quiesce_deadline = Instant::now() + AGENT_TERMINAL_PROMOTION_QUIESCE_TIMEOUT;
        loop {
            let mut state = self.lock()?;
            let (
                previous_owner,
                lifecycle_scope_id,
                execution_id,
                interrupt_foreground,
                driver_busy,
            ) = {
                let entry = state
                    .terminals
                    .get(terminal_id)
                    .ok_or_else(|| terminal_not_found(terminal_id))?;
                if &entry.record.topology_id != topology_id || !entry.record.state.is_live() {
                    return Err(ProcessError::new(
                        ProcessErrorCode::StateConflict,
                        "only a live subagent terminal in this session can be promoted",
                    ));
                }
                if !entry.record.owner.is_ephemeral() {
                    return Err(ProcessError::new(
                        ProcessErrorCode::StateConflict,
                        "only an ephemeral terminal owner can be promoted",
                    ));
                }
                if entry.integration.is_none() {
                    return Err(ProcessError::new(
                        ProcessErrorCode::StateConflict,
                        "recovered terminal cannot reacquire promotion driver ownership",
                    ));
                }
                (
                    entry.record.owner.caller().clone(),
                    entry.record.lifecycle_scope_id.clone(),
                    entry.record.execution_id.clone(),
                    entry.commands.active_is_submitted(),
                    entry.command_driver_busy,
                )
            };
            if driver_busy {
                drop(state);
                if Instant::now() >= quiesce_deadline {
                    return Err(ProcessError::new(
                        ProcessErrorCode::InputBackpressure,
                        "managed terminal writer did not quiesce before promotion",
                    ));
                }
                thread::sleep(AGENT_TERMINAL_DRIVE_INTERVAL);
                continue;
            }

            self.starter.handoff_managed_terminal(
                &execution_id,
                ExecutionOwner::new(promoted_caller.clone(), lifecycle_scope_id),
                interrupt_foreground,
            )?;

            let promoted_owner =
                TerminalOwner::promoted(promoted_caller.clone(), previous_owner.clone());
            let promoted_slot = TerminalSlot::Promoted(terminal_id.clone());
            let mut promoted_record = state
                .terminals
                .get(terminal_id)
                .expect("terminal was checked above")
                .record
                .clone();
            promoted_record.owner = promoted_owner.clone();
            if let Err(error) = replace_terminal_record(
                self.repository.as_ref(),
                &mut state,
                terminal_id,
                promoted_record,
                promoted_slot.clone(),
                true,
            ) {
                state
                    .retired_owners
                    .insert(previous_owner.owner_id().clone());
                mark_terminal_outcome_unknown(
                    &mut state,
                    terminal_id,
                    Some((promoted_owner, promoted_slot)),
                )?;
                return Err(error);
            }
            state
                .retired_owners
                .insert(previous_owner.owner_id().clone());
            let entry = state
                .terminals
                .get_mut(terminal_id)
                .expect("terminal was checked above");
            fail_all_agent_commands(
                entry,
                ProcessError::new(
                    ProcessErrorCode::ExecutionNotOwned,
                    "subagent terminal command ownership ended at promotion",
                ),
            );
            entry.agent_input_lease = None;
            entry.renew_input_lease_at = None;
            return Ok(entry.record.clone());
        }
    }

    pub fn terminate_terminal(&self, terminal_id: &TerminalId, mode: KillMode) -> Result<()> {
        let mut state = self.lock()?;
        let (record, slot, owns_driver) = {
            let entry = state
                .terminals
                .get(terminal_id)
                .ok_or_else(|| terminal_not_found(terminal_id))?;
            (
                entry.record.clone(),
                entry.slot.clone(),
                entry.integration.is_some(),
            )
        };
        if !record.state.is_live() {
            return Ok(());
        }
        if !owns_driver {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "terminal recovered without process control ownership",
            ));
        }
        self.starter.kill(&record.execution_id, mode)?;
        let mut stopping = record;
        stopping.state = TerminalState::Stopping;
        if let Err(error) = replace_terminal_record(
            self.repository.as_ref(),
            &mut state,
            terminal_id,
            stopping,
            slot,
            true,
        ) {
            mark_terminal_outcome_unknown(&mut state, terminal_id, None)?;
            return Err(error);
        }
        let entry = state
            .terminals
            .get_mut(terminal_id)
            .expect("terminal was durably marked stopping");
        fail_all_agent_commands(
            entry,
            ProcessError::new(
                ProcessErrorCode::ExecutionNotLive,
                "persistent agent terminal is stopping",
            ),
        );
        entry.agent_input_lease = None;
        entry.renew_input_lease_at = None;
        Ok(())
    }

    pub fn terminate_ephemeral_owner(&self, owner_id: &CallerOwnerId) -> Result<usize> {
        let ids = {
            let mut state = self.lock()?;
            state.retired_owners.insert(owner_id.clone());
            state
                .terminals
                .iter()
                .filter(|(_, entry)| {
                    entry.record.owner.is_ephemeral()
                        && entry.record.owner.caller().owner_id() == owner_id
                        && entry.record.state.is_live()
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        };
        for id in &ids {
            self.terminate_terminal(id, KillMode::Graceful)?;
        }
        Ok(ids.len())
    }

    pub fn terminate_topology(&self, topology_id: &TerminalTopologyId) -> Result<usize> {
        let ids = self
            .lock()?
            .terminals
            .iter()
            .filter(|(_, entry)| {
                &entry.record.topology_id == topology_id && entry.record.state.is_live()
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in &ids {
            self.terminate_terminal(id, KillMode::Graceful)?;
        }
        Ok(ids.len())
    }

    fn lock(&self) -> Result<MutexGuard<'_, RegistryState>> {
        self.inner.lock().map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::Internal,
                "terminal registry lock is poisoned",
            )
        })
    }
}

fn recover_registry_state(recovered: Vec<StoredTerminalRecord>) -> Result<RegistryState> {
    let mut state = RegistryState::default();
    for stored in recovered {
        if stored.record.state.is_live()
            || (stored.active_slot
                && (stored.record.state != TerminalState::OutcomeUnknown
                    || stored.record.prompt_state != TerminalPromptState::Degraded
                    || stored.record.integration_health != ShellIntegrationHealth::Degraded))
        {
            return Err(ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                "terminal recovery returned a live or non-degraded active record",
            ));
        }
        insert_stored_terminal(&mut state, stored)?;
    }
    Ok(state)
}

fn insert_stored_terminal(state: &mut RegistryState, stored: StoredTerminalRecord) -> Result<()> {
    stored.validate()?;
    let terminal_id = stored.record.terminal_id.clone();
    let execution_id = stored.record.execution_id.clone();
    if state.terminals.contains_key(&terminal_id)
        || state
            .terminals
            .values()
            .any(|entry| entry.record.execution_id == execution_id)
    {
        return Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "terminal recovery contains duplicate terminal or execution identities",
        ));
    }
    let slot = terminal_slot_for_record(&stored.record)?;
    if stored.active_slot
        && let Some(existing) = state.slots.get(&slot)
    {
        return Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            format!("terminal topology slot is occupied by both `{existing}` and `{terminal_id}`"),
        ));
    }
    if let Some(previous_owner) = stored.record.owner.previous_owner() {
        state
            .retired_owners
            .insert(previous_owner.owner_id().clone());
    }
    if stored.active_slot {
        state.slots.insert(slot.clone(), terminal_id.clone());
    }
    state.terminals.insert(
        terminal_id,
        TerminalEntry {
            slot,
            fingerprint: stored.fingerprint,
            active_slot: stored.active_slot,
            record: stored.record,
            integration: None,
            commands: AgentTerminalCommandQueue::default(),
            completed_commands: BTreeMap::new(),
            output_sequence: 0,
            agent_input_lease: None,
            renew_input_lease_at: None,
            command_timeout: None,
            pending_prompt_result: None,
            command_driver_busy: false,
            pending_human_command: None,
            pending_agent_transaction: None,
            active_typed_command: None,
            raw_human_input_pending: false,
        },
    );
    Ok(())
}

fn replace_terminal_record(
    repository: &dyn TerminalRepository,
    state: &mut RegistryState,
    terminal_id: &TerminalId,
    record: TerminalRecord,
    slot: TerminalSlot,
    active_slot: bool,
) -> Result<TerminalRecord> {
    if terminal_slot_for_record(&record)? != slot {
        return Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "terminal runtime slot does not match its durable record",
        ));
    }
    let (old_slot, old_active_slot, fingerprint) = {
        let entry = state
            .terminals
            .get(terminal_id)
            .ok_or_else(|| terminal_not_found(terminal_id))?;
        (
            entry.slot.clone(),
            entry.active_slot,
            entry.fingerprint.clone(),
        )
    };
    if old_active_slot && state.slots.get(&old_slot) != Some(terminal_id) {
        return Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "active terminal record is missing its runtime topology index",
        ));
    }
    if active_slot
        && let Some(existing) = state.slots.get(&slot)
        && existing != terminal_id
    {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "terminal transition conflicts with an occupied runtime topology slot",
        ));
    }
    let stored = StoredTerminalRecord {
        record: record.clone(),
        slot_key: terminal_slot_key(&record)?,
        fingerprint,
        active_slot,
    };
    repository.replace(&stored)?;

    if old_active_slot {
        state.slots.remove(&old_slot);
    }
    if active_slot {
        state.slots.insert(slot.clone(), terminal_id.clone());
    }
    let entry = state
        .terminals
        .get_mut(terminal_id)
        .expect("terminal was checked before its durable replacement");
    entry.slot = slot;
    entry.active_slot = active_slot;
    entry.record = record.clone();
    if !record.state.is_live() {
        entry.integration = None;
        fail_all_agent_commands(
            entry,
            ProcessError::new(
                ProcessErrorCode::ExecutionNotLive,
                "persistent agent terminal no longer has a live execution outcome",
            ),
        );
        entry.agent_input_lease = None;
        entry.renew_input_lease_at = None;
    }
    Ok(record)
}

fn mark_terminal_outcome_unknown(
    state: &mut RegistryState,
    terminal_id: &TerminalId,
    handoff: Option<(TerminalOwner, TerminalSlot)>,
) -> Result<()> {
    let (old_slot, old_active_slot) = {
        let entry = state
            .terminals
            .get(terminal_id)
            .ok_or_else(|| terminal_not_found(terminal_id))?;
        (entry.slot.clone(), entry.active_slot)
    };
    let next_slot = handoff
        .as_ref()
        .map(|(_, slot)| slot.clone())
        .unwrap_or_else(|| old_slot.clone());
    if let Some(existing) = state.slots.get(&next_slot)
        && existing != terminal_id
    {
        return Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "conservative terminal fencing conflicts with an occupied runtime slot",
        ));
    }
    if old_active_slot {
        state.slots.remove(&old_slot);
    }
    state.slots.insert(next_slot.clone(), terminal_id.clone());
    let entry = state
        .terminals
        .get_mut(terminal_id)
        .expect("terminal was checked before conservative fencing");
    if let Some((owner, _)) = handoff {
        entry.record.owner = owner;
    }
    entry.slot = next_slot;
    entry.active_slot = true;
    entry.record.state = TerminalState::OutcomeUnknown;
    entry.record.prompt_state = TerminalPromptState::Degraded;
    entry.record.integration_health = ShellIntegrationHealth::Degraded;
    entry.integration = None;
    fail_all_agent_commands(
        entry,
        ProcessError::new(
            ProcessErrorCode::StateConflict,
            "terminal ownership became uncertain after a durable persistence failure",
        ),
    );
    entry.agent_input_lease = None;
    entry.renew_input_lease_at = None;
    Ok(())
}

fn terminal_slot_for_record(record: &TerminalRecord) -> Result<TerminalSlot> {
    if record.owner.previous_owner().is_some() && record.profile == ExecutionProfile::Workspace {
        return Ok(TerminalSlot::Promoted(record.terminal_id.clone()));
    }
    match (
        record.owner.caller().owner_kind(),
        record.owner.caller().role(),
        record.profile,
    ) {
        (
            agl_exec::CallerOwnerKind::Persistent,
            agl_exec::CallerRole::Human,
            ExecutionProfile::Workspace,
        ) => Ok(TerminalSlot::HumanWorkspace(record.topology_id.clone())),
        (
            agl_exec::CallerOwnerKind::Persistent,
            agl_exec::CallerRole::Human,
            ExecutionProfile::Host,
        ) => Ok(TerminalSlot::HumanHost(record.topology_id.clone())),
        (
            agl_exec::CallerOwnerKind::Persistent,
            agl_exec::CallerRole::Agent,
            ExecutionProfile::Workspace,
        ) => Ok(TerminalSlot::PersistentAgent(record.topology_id.clone())),
        (
            agl_exec::CallerOwnerKind::Ephemeral,
            agl_exec::CallerRole::Agent,
            ExecutionProfile::Workspace,
        ) => Ok(TerminalSlot::EphemeralAgent(
            record.owner.caller().owner_id().clone(),
        )),
        _ => Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "stored terminal owner and profile do not map to a runtime topology slot",
        )),
    }
}

fn validate_ensure_request(request: &TerminalEnsureRequest) -> Result<()> {
    request.context.validate()?;
    request.shell.validate()?;
    request.host_startup.validate(request.profile)?;
    request.terminal_size.validate()?;
    request.limits.validate()?;
    if request.limits.timeout_ms.is_some() {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "persistent terminal process must not have a wall-clock timeout",
        ));
    }
    if request.shell.snapshot != request.context.shell {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "terminal shell admission must match the immutable execution context snapshot",
        ));
    }
    if request.profile == ExecutionProfile::Workspace
        && !request
            .context
            .working_directory
            .starts_with(&request.context.workspace_root)
    {
        return Err(ProcessError::new(
            ProcessErrorCode::HostAuthorityRequired,
            "workspace terminal cwd must remain inside its immutable workspace root",
        ));
    }
    let admitted_parent_names = request
        .shell
        .snapshot
        .environment_names
        .iter()
        .collect::<BTreeSet<_>>();
    if request
        .environment
        .selected_parent
        .keys()
        .any(|name| !admitted_parent_names.contains(name))
    {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "selected parent terminal environment name was not admitted by the shell snapshot",
        ));
    }

    if request.owner.previous_owner().is_some() {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "promoted terminals are created only by promotion, never ensure",
        ));
    }
    let caller = request.owner.caller();
    match (caller.owner_kind(), caller.role(), request.profile) {
        (agl_exec::CallerOwnerKind::Persistent, agl_exec::CallerRole::Human, _)
        | (
            agl_exec::CallerOwnerKind::Persistent,
            agl_exec::CallerRole::Agent,
            ExecutionProfile::Workspace,
        ) if caller.owner_id().as_str() == request.topology_id.as_str() => {}
        (
            agl_exec::CallerOwnerKind::Persistent,
            agl_exec::CallerRole::Agent,
            ExecutionProfile::Host,
        )
        | (
            agl_exec::CallerOwnerKind::Ephemeral,
            agl_exec::CallerRole::Agent,
            ExecutionProfile::Host,
        ) => {
            return Err(ProcessError::new(
                ProcessErrorCode::HostAuthorityRequired,
                "agent terminal owners are workspace-confined",
            ));
        }
        (
            agl_exec::CallerOwnerKind::Ephemeral,
            agl_exec::CallerRole::Agent,
            ExecutionProfile::Workspace,
        ) => {}
        _ => {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "terminal owner does not belong to the requested topology",
            ));
        }
    }

    let sources_user_rc = matches!(request.host_startup, HostStartupPolicy::SourceUserRc { .. });
    match request.profile {
        ExecutionProfile::Workspace => {
            if request.authorization.host_process_execution
                || request.authorization.shell_login_startup
                || request.grant_lease.is_some()
                || sources_user_rc
            {
                return Err(ProcessError::new(
                    ProcessErrorCode::InvalidRequest,
                    "workspace terminal must not carry Host or login authority",
                ));
            }
        }
        ExecutionProfile::Host => {
            if !request.owner.is_human()
                || !request.owner.is_persistent()
                || !request.authorization.host_process_execution
                || request.authorization.shell_login_startup != sources_user_rc
                || !request.grant_lease.as_ref().is_some_and(|lease| {
                    lease.origin == crate::ExecutionLeaseOrigin::LocalOperatorTerminal
                })
            {
                return Err(ProcessError::new(
                    ProcessErrorCode::HostAuthorityRequired,
                    "Human Host terminal requires explicit local-operator lifetime authority and matching startup authority",
                ));
            }
        }
    }
    Ok(())
}

fn terminal_slot(request: &TerminalEnsureRequest) -> Result<TerminalSlot> {
    match (
        request.owner.caller().owner_kind(),
        request.owner.caller().role(),
        request.profile,
    ) {
        (
            agl_exec::CallerOwnerKind::Persistent,
            agl_exec::CallerRole::Human,
            ExecutionProfile::Workspace,
        ) => Ok(TerminalSlot::HumanWorkspace(request.topology_id.clone())),
        (
            agl_exec::CallerOwnerKind::Persistent,
            agl_exec::CallerRole::Human,
            ExecutionProfile::Host,
        ) => Ok(TerminalSlot::HumanHost(request.topology_id.clone())),
        (
            agl_exec::CallerOwnerKind::Persistent,
            agl_exec::CallerRole::Agent,
            ExecutionProfile::Workspace,
        ) => Ok(TerminalSlot::PersistentAgent(request.topology_id.clone())),
        (
            agl_exec::CallerOwnerKind::Ephemeral,
            agl_exec::CallerRole::Agent,
            ExecutionProfile::Workspace,
        ) => Ok(TerminalSlot::EphemeralAgent(
            request.owner.caller().owner_id().clone(),
        )),
        _ => Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "terminal request does not map to an admitted topology slot",
        )),
    }
}

fn execution_owner(request: &TerminalEnsureRequest) -> ExecutionOwner {
    ExecutionOwner::new(
        request.owner.caller().clone(),
        request.lifecycle_scope_id.clone(),
    )
}

fn terminal_fingerprint(
    request: &TerminalEnsureRequest,
    environment_digest: &TerminalEnvironmentDigest,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"agentlibre.terminal-admission.v1\0");
    update_field(&mut digest, request.topology_id.as_str());
    update_field(&mut digest, &format!("{:?}", request.owner));
    update_field(&mut digest, &format!("{:?}", request.profile));
    update_path(&mut digest, &request.context.workspace_root);
    update_field(
        &mut digest,
        match request.shell.kind {
            AdmittedShellKind::Bash => "bash",
            AdmittedShellKind::Zsh => "zsh",
        },
    );
    update_path(&mut digest, &request.shell.snapshot.program);
    update_field(&mut digest, &request.shell.snapshot.executable_digest);
    update_field(&mut digest, &request.shell.snapshot.config_digest);
    update_field(&mut digest, environment_digest.as_str());
    update_field(&mut digest, &format!("{:?}", request.host_startup));
    update_field(&mut digest, &format!("{:?}", request.grant_lease));
    update_field(&mut digest, &request.limits.max_input_bytes.to_string());
    update_field(&mut digest, &request.limits.max_output_bytes.to_string());
    let bytes = digest.finalize();
    let mut rendered = String::with_capacity(7 + bytes.len() * 2);
    rendered.push_str("sha256:");
    use std::fmt::Write as _;
    for byte in bytes {
        write!(&mut rendered, "{byte:02x}").expect("writing to a String cannot fail");
    }
    rendered
}

fn update_field(digest: &mut Sha256, value: &str) {
    digest.update(value.as_bytes());
    digest.update([0]);
}

fn update_path(digest: &mut Sha256, value: &Path) {
    digest.update(value.as_os_str().as_bytes());
    digest.update([0]);
}

fn apply_event_to_record(record: &mut TerminalRecord, event: &ShellIntegrationEvent) -> Result<()> {
    match event {
        ShellIntegrationEvent::PromptReady { cwd, .. }
        | ShellIntegrationEvent::CommandStarted { cwd, .. }
        | ShellIntegrationEvent::CommandFinished { cwd, .. } => {
            let canonical = cwd.canonicalize().map_err(|error| {
                ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    format!("trusted shell cwd cannot be canonicalized: {error}"),
                )
            })?;
            if canonical != *cwd {
                return Err(ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "trusted shell cwd must already be canonical",
                ));
            }
            if record.profile == ExecutionProfile::Workspace
                && !canonical.starts_with(&record.workspace_root)
            {
                return Err(ProcessError::new(
                    ProcessErrorCode::HostAuthorityRequired,
                    "workspace terminal cwd escaped its immutable root",
                ));
            }
            record.cwd = canonical;
        }
        ShellIntegrationEvent::ForegroundChanged { .. } => {}
    }
    if matches!(event, ShellIntegrationEvent::CommandStarted { .. }) {
        record.command_sequence = record.command_sequence.checked_add(1).ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "terminal command sequence overflowed",
            )
        })?;
    }
    Ok(())
}

fn merge_integration_batch(target: &mut IntegrationBatch, mut source: IntegrationBatch) {
    target.events.append(&mut source.events);
    if target.notice.is_none() {
        target.notice = source.notice;
    }
}

fn sync_integration_projection(record: &mut TerminalRecord, integration: &BoundedShellIntegration) {
    record.prompt_state = integration.state().prompt().clone();
    record.integration_health = integration.state().health();
}

fn fail_all_agent_commands(entry: &mut TerminalEntry, error: ProcessError) {
    if let Some(pending) = entry.pending_prompt_result.take() {
        entry
            .completed_commands
            .entry(pending.command_sequence)
            .or_insert(Err(pending.outcome));
    }
    for command_sequence in entry.commands.cancel_all() {
        entry
            .completed_commands
            .entry(command_sequence)
            .or_insert_with(|| Err(error.clone()));
    }
    entry.command_timeout = None;
    entry.command_driver_busy = false;
}

fn plan_agent_drive(entry: &mut TerminalEntry, now: Instant) -> Result<Option<AgentDriveAction>> {
    if entry.command_driver_busy {
        return Ok(None);
    }

    if let Some(pending) = entry.pending_prompt_result.as_ref() {
        if now < pending.recover_by {
            return Ok(None);
        }
        let pending = entry
            .pending_prompt_result
            .take()
            .expect("pending prompt result was checked above");
        entry
            .completed_commands
            .entry(pending.command_sequence)
            .or_insert(Err(pending.outcome));
        fail_all_agent_commands(
            entry,
            ProcessError::new(
                ProcessErrorCode::ExecutionNotLive,
                "persistent agent terminal did not recover a trusted prompt",
            ),
        );
        let lease = entry.agent_input_lease.take();
        entry.renew_input_lease_at = None;
        entry.command_driver_busy = true;
        return Ok(Some(AgentDriveAction::Terminate {
            execution_id: entry.record.execution_id.clone(),
            lease,
        }));
    }

    if entry.command_timeout.is_none()
        && entry
            .commands
            .active_deadline()
            .is_some_and(|deadline| now >= deadline)
    {
        let command_sequence = entry
            .commands
            .active_sequence()
            .expect("an active deadline belongs to an active command");
        if !entry.commands.active_is_submitted() {
            entry.commands.cancel_active();
            entry.completed_commands.insert(
                command_sequence,
                Err(agent_command_outcome_error(ProcessErrorCode::TimedOut)),
            );
            return Ok(None);
        }
        entry.command_timeout = Some(AgentCommandTimeout {
            command_sequence,
            outcome: ProcessErrorCode::TimedOut,
            recover_by: now + AGENT_TERMINAL_PROMPT_RECOVERY_TIMEOUT,
            interrupt_sent: false,
        });
    }

    if let Some(timeout) = entry.command_timeout.as_ref() {
        if now >= timeout.recover_by {
            let timeout = entry
                .command_timeout
                .take()
                .expect("command timeout was checked above");
            if entry.commands.active_sequence() == Some(timeout.command_sequence) {
                entry.commands.cancel_active();
            }
            entry.completed_commands.insert(
                timeout.command_sequence,
                Err(agent_command_outcome_error(timeout.outcome)),
            );
            fail_all_agent_commands(
                entry,
                ProcessError::new(
                    ProcessErrorCode::ExecutionNotLive,
                    "persistent agent terminal did not recover after interrupt",
                ),
            );
            let lease = entry.agent_input_lease.take();
            entry.renew_input_lease_at = None;
            entry.command_driver_busy = true;
            return Ok(Some(AgentDriveAction::Terminate {
                execution_id: entry.record.execution_id.clone(),
                lease,
            }));
        }
        if !timeout.interrupt_sent {
            entry.command_driver_busy = true;
            return Ok(Some(AgentDriveAction::Interrupt {
                execution_id: entry.record.execution_id.clone(),
                command_sequence: timeout.command_sequence,
            }));
        }
        return Ok(None);
    }

    if entry.commands.active_sequence().is_none() {
        if !entry.record.prompt_state.is_trusted_ready() {
            return Ok(None);
        }
        if entry
            .commands
            .begin_next(&entry.record.prompt_state, entry.output_sequence)?
            .is_none()
        {
            return Ok(None);
        }
    }

    let command_sequence = entry
        .commands
        .active_sequence()
        .expect("begin_next established an active command");
    if !entry.commands.active_is_submitted() {
        let submission = entry.commands.reserve_submission()?;
        entry.command_driver_busy = true;
        return Ok(Some(AgentDriveAction::Submit {
            execution_id: entry.record.execution_id.clone(),
            command_sequence,
            submission,
        }));
    }
    if entry
        .renew_input_lease_at
        .is_some_and(|renew_at| now >= renew_at)
        && let Some(lease) = entry.agent_input_lease.clone()
    {
        entry.command_driver_busy = true;
        return Ok(Some(AgentDriveAction::RenewLease {
            execution_id: entry.record.execution_id.clone(),
            command_sequence,
            lease,
        }));
    }
    Ok(None)
}

fn agent_command_outcome_error(outcome: ProcessErrorCode) -> ProcessError {
    ProcessError::new(
        outcome,
        if outcome == ProcessErrorCode::TimedOut {
            "persistent agent terminal command timed out"
        } else {
            "persistent agent terminal command was cancelled"
        },
    )
}

fn terminal_state(state: ExecutionState) -> TerminalState {
    match state {
        ExecutionState::Admitting | ExecutionState::Starting => TerminalState::Starting,
        ExecutionState::Running => TerminalState::Running,
        ExecutionState::Exited | ExecutionState::Signalled => TerminalState::Exited,
        ExecutionState::OutcomeUnknown => TerminalState::OutcomeUnknown,
        ExecutionState::Cancelled | ExecutionState::TimedOut | ExecutionState::Failed => {
            TerminalState::Failed
        }
    }
}

fn merge_terminal_state(current: TerminalState, observed: TerminalState) -> TerminalState {
    match (current, observed) {
        (TerminalState::Running, TerminalState::Starting) => TerminalState::Running,
        (
            TerminalState::Stopping,
            TerminalState::Starting | TerminalState::Running | TerminalState::Stopping,
        ) => TerminalState::Stopping,
        (_, observed) => observed,
    }
}

fn terminal_not_found(terminal_id: &TerminalId) -> ProcessError {
    ProcessError::new(
        ProcessErrorCode::ExecutionNotFound,
        format!("terminal `{terminal_id}` was not found"),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crate::test_support::{RunId, SessionId, StepId};
    use agl_exec::{CallerNamespace, CallerOwner, CallerOwnerKind, CallerRole, WriterLeaseId};

    use super::*;
    use crate::terminal::environment::TerminalSecretValue;
    use agl_terminal::MAX_AGENT_TERMINAL_COMMAND_BYTES;

    fn caller_id(value: &str) -> CallerOwnerId {
        CallerOwnerId::new(value).unwrap()
    }

    fn topology(session_id: &SessionId) -> TerminalTopologyId {
        TerminalTopologyId::new(caller_id(session_id.as_str()))
    }

    fn caller(value: &str, kind: CallerOwnerKind, role: CallerRole) -> CallerOwner {
        CallerOwner::new(
            CallerNamespace::new("agentlibre", 1).unwrap(),
            caller_id(value),
            kind,
            role,
        )
    }

    fn human_owner(session_id: &SessionId) -> TerminalOwner {
        TerminalOwner::new(caller(
            session_id.as_str(),
            CallerOwnerKind::Persistent,
            CallerRole::Human,
        ))
    }

    fn persistent_agent_owner(session_id: &SessionId) -> TerminalOwner {
        TerminalOwner::new(caller(
            session_id.as_str(),
            CallerOwnerKind::Persistent,
            CallerRole::Agent,
        ))
    }

    fn ephemeral_agent_owner(run_id: &RunId) -> TerminalOwner {
        TerminalOwner::new(caller(
            run_id.as_str(),
            CallerOwnerKind::Ephemeral,
            CallerRole::Agent,
        ))
    }

    struct NoSecrets;

    impl TerminalSecretResolver for NoSecrets {
        fn resolve(
            &self,
            _reference: &crate::terminal::environment::TerminalSecretReference,
        ) -> Result<TerminalSecretValue> {
            Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "test has no admitted secrets",
            ))
        }
    }

    const FAKE_COMMAND_IDLE: usize = 0;
    const FAKE_COMMAND_COMPLETE: usize = 1;
    const FAKE_COMMAND_FINISH_ON_INTERRUPT: usize = 2;
    const FAKE_COMMAND_NEVER_RECOVERS: usize = 3;

    type FakeStartProbe =
        dyn Fn(&ExecutionId, &ExecutionRequest, &ManagedShellStartup) -> Result<()> + Send + Sync;

    #[derive(Default)]
    struct FakeIntegration {
        packets: VecDeque<(Vec<u8>, u64)>,
        output_through_sequence: u64,
        last_read_output_sequence: u64,
        next_event_sequence: u64,
        foreground_process_group: Option<i32>,
        armed_transaction: Option<String>,
        active_transaction: Option<String>,
        channel_closed: bool,
        degraded: bool,
    }

    #[derive(Default)]
    struct FakeStarter {
        starts: AtomicUsize,
        kills: AtomicUsize,
        interrupts: AtomicUsize,
        integration_reads: AtomicUsize,
        command_behavior: AtomicUsize,
        start_probe: Mutex<Option<Arc<FakeStartProbe>>>,
        start_error: Mutex<Option<ProcessError>>,
        records: Mutex<BTreeMap<ExecutionId, ExecutionStatus>>,
        tokens: Mutex<BTreeMap<ExecutionId, ShellIntegrationToken>>,
        integrations: Mutex<BTreeMap<ExecutionId, FakeIntegration>>,
        writable_leases: Mutex<BTreeMap<ExecutionId, (ExecutionRequestId, WriterLeaseId)>>,
        writes: Mutex<Vec<(ExecutionId, Vec<u8>)>>,
        controls: Mutex<Vec<(ExecutionId, Vec<String>)>>,
    }

    impl TerminalExecutionStarter for FakeStarter {
        fn start(
            &self,
            execution_id: ExecutionId,
            request: ExecutionRequest,
            startup: ManagedShellStartup,
        ) -> Result<ExecutionStatus> {
            self.starts.fetch_add(1, Ordering::Relaxed);
            if let Some(probe) = self.start_probe.lock().unwrap().clone() {
                probe(&execution_id, &request, &startup)?;
            }
            if let Some(error) = self.start_error.lock().unwrap().take() {
                return Err(error);
            }
            let status = ExecutionStatus {
                execution_id,
                owner: request.owner,
                state: ExecutionState::Running,
                profile: request.profile,
                io: request.io,
                cwd: request.cwd,
                terminal_size: request.terminal_size,
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
            if self
                .records
                .lock()
                .unwrap()
                .insert(status.execution_id.clone(), status.clone())
                .is_some()
            {
                return Err(ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "fake starter received a duplicate reserved execution identity",
                ));
            }
            self.tokens
                .lock()
                .unwrap()
                .insert(status.execution_id.clone(), startup.integration_token);
            self.integrations
                .lock()
                .unwrap()
                .insert(status.execution_id.clone(), FakeIntegration::default());
            self.push_event(
                &status.execution_id,
                "prompt_ready",
                &[
                    status.cwd.to_string_lossy().into_owned(),
                    "-".to_owned(),
                    "0".to_owned(),
                ],
                2,
            );
            Ok(status)
        }

        fn status(&self, execution_id: &ExecutionId) -> Result<ExecutionStatus> {
            self.records
                .lock()
                .unwrap()
                .get(execution_id)
                .cloned()
                .ok_or_else(|| terminal_not_found(&TerminalId::generate()))
        }

        fn kill(&self, execution_id: &ExecutionId, _mode: KillMode) -> Result<()> {
            self.kills.fetch_add(1, Ordering::Relaxed);
            self.records
                .lock()
                .unwrap()
                .get_mut(execution_id)
                .ok_or_else(|| terminal_not_found(&TerminalId::generate()))?
                .state = ExecutionState::Cancelled;
            Ok(())
        }

        fn read_shell_integration(
            &self,
            execution_id: &ExecutionId,
            maximum_bytes: usize,
        ) -> Result<ShellIntegrationReadResult> {
            self.integration_reads.fetch_add(1, Ordering::Relaxed);
            self.status(execution_id)?;
            let mut integrations = self.integrations.lock().unwrap();
            let integration = integrations.get_mut(execution_id).unwrap();
            let (bytes, output_through_sequence) = integration
                .packets
                .pop_front()
                .unwrap_or_else(|| (Vec::new(), 0));
            if !bytes.is_empty() {
                integration.last_read_output_sequence = output_through_sequence;
            }
            if bytes.len() > maximum_bytes {
                return Err(ProcessError::new(
                    ProcessErrorCode::InvalidRequest,
                    "fake integration packet exceeds read bound",
                ));
            }
            Ok(ShellIntegrationReadResult {
                execution_id: execution_id.clone(),
                bytes: ProcessBytes::from_bytes(&bytes),
                output_through_sequence,
                foreground_process_group: integration.foreground_process_group,
                channel_closed: integration.channel_closed,
                degraded: integration.degraded,
            })
        }

        fn send_shell_integration_control(
            &self,
            execution_id: &ExecutionId,
            frame: ProcessBytes,
        ) -> Result<u64> {
            let frame = frame.decode(MAX_SHELL_INTEGRATION_FRAME_BYTES)?;
            let fields = frame
                .split(|byte| *byte == 0)
                .filter(|field| !field.is_empty())
                .map(|field| std::str::from_utf8(field).map(str::to_owned))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|_| ProcessError::new(ProcessErrorCode::InvalidBytes, "fake control"))?;
            let token = self.token(execution_id);
            if fields.first().map(String::as_str) != Some("AGL2")
                || fields.get(1).map(String::as_str) != Some(token.expose_to_managed_startup())
            {
                return Err(ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "fake control authentication failed",
                ));
            }
            let mut integrations = self.integrations.lock().unwrap();
            let integration = integrations.get_mut(execution_id).unwrap();
            self.controls
                .lock()
                .unwrap()
                .push((execution_id.clone(), fields.clone()));
            match fields.get(2).map(String::as_str) {
                Some("arm_typed_command") => {
                    integration.armed_transaction = fields.get(3).cloned();
                }
                Some("disarm_typed_command") => {
                    if integration.armed_transaction.as_ref() == fields.get(3) {
                        integration.armed_transaction = None;
                    }
                }
                Some("command_boundary_ack") | Some("prompt_ready_ack") => {}
                _ => {
                    return Err(ProcessError::new(
                        ProcessErrorCode::StateConflict,
                        "fake control kind is unsupported",
                    ));
                }
            }
            let output_through_sequence = integration.last_read_output_sequence;
            drop(integrations);
            Ok(output_through_sequence)
        }

        fn attach(
            &self,
            execution_id: &ExecutionId,
            attachment_id: ExecutionRequestId,
        ) -> Result<InputLease> {
            self.status(execution_id)?;
            let mut leases = self.writable_leases.lock().unwrap();
            if leases.contains_key(execution_id) {
                return Err(ProcessError::new(
                    ProcessErrorCode::InputLeaseBusy,
                    "fake terminal already has a writable lease",
                ));
            }
            let writer_lease_id = WriterLeaseId::generate();
            leases.insert(
                execution_id.clone(),
                (attachment_id.clone(), writer_lease_id.clone()),
            );
            Ok(InputLease {
                attachment_id,
                writer_lease_id: Some(writer_lease_id),
            })
        }

        fn detach(&self, execution_id: &ExecutionId, lease: InputLease) -> Result<()> {
            self.status(execution_id)?;
            let mut leases = self.writable_leases.lock().unwrap();
            if leases
                .get(execution_id)
                .is_some_and(|(attachment_id, writer_lease_id)| {
                    attachment_id == &lease.attachment_id
                        && lease.writer_lease_id.as_ref() == Some(writer_lease_id)
                })
            {
                leases.remove(execution_id);
                Ok(())
            } else {
                Err(ProcessError::new(
                    ProcessErrorCode::InputLeaseBusy,
                    "fake terminal lease does not match",
                ))
            }
        }

        fn renew_input_lease(&self, execution_id: &ExecutionId, lease: InputLease) -> Result<()> {
            self.status(execution_id)?;
            if self
                .writable_leases
                .lock()
                .unwrap()
                .get(execution_id)
                .is_some_and(|(attachment_id, writer_lease_id)| {
                    attachment_id == &lease.attachment_id
                        && lease.writer_lease_id.as_ref() == Some(writer_lease_id)
                })
            {
                Ok(())
            } else {
                Err(ProcessError::new(
                    ProcessErrorCode::InputLeaseExpired,
                    "fake terminal lease expired",
                ))
            }
        }

        fn write(
            &self,
            execution_id: &ExecutionId,
            lease: InputLease,
            bytes: ProcessBytes,
            eof: bool,
        ) -> Result<()> {
            if eof {
                return Err(ProcessError::new(
                    ProcessErrorCode::InvalidRequest,
                    "fake managed terminal does not accept an EOF write",
                ));
            }
            self.renew_input_lease(execution_id, lease)?;
            let bytes = bytes.decode(MAX_AGENT_TERMINAL_COMMAND_BYTES + 1)?;
            self.writes
                .lock()
                .unwrap()
                .push((execution_id.clone(), bytes.clone()));
            if self
                .integrations
                .lock()
                .unwrap()
                .get(execution_id)
                .is_none_or(|integration| integration.armed_transaction.is_none())
            {
                return Ok(());
            }
            let command = std::str::from_utf8(&bytes)
                .map_err(|_| ProcessError::new(ProcessErrorCode::InvalidBytes, "fake input"))?
                .strip_prefix("\u{1b}[200~")
                .and_then(|command| command.strip_suffix("\u{1b}[201~\n"))
                .ok_or_else(|| {
                    ProcessError::new(
                        ProcessErrorCode::InvalidRequest,
                        "fake missing typed bracketed-paste barrier",
                    )
                })?
                .to_owned();
            let status = self.status(execution_id)?;
            let output_before = status.last_sequence;
            let cwd = status.cwd.to_string_lossy().into_owned();
            let transaction = {
                let mut integrations = self.integrations.lock().unwrap();
                let integration = integrations.get_mut(execution_id).unwrap();
                let transaction = integration
                    .armed_transaction
                    .take()
                    .unwrap_or_else(|| "-".to_owned());
                integration.active_transaction = (transaction != "-").then(|| transaction.clone());
                transaction
            };
            self.push_event(
                execution_id,
                "command_started",
                &[transaction.clone(), command, cwd.clone()],
                output_before,
            );
            if self.command_behavior.load(Ordering::Acquire) == FAKE_COMMAND_COMPLETE {
                let output_after = output_before + 7;
                self.push_event(
                    execution_id,
                    "command_finished",
                    &[transaction, "code".to_owned(), "0".to_owned(), cwd.clone()],
                    output_after,
                );
                self.integrations
                    .lock()
                    .unwrap()
                    .get_mut(execution_id)
                    .unwrap()
                    .active_transaction = None;
                self.push_event(
                    execution_id,
                    "prompt_ready",
                    &[cwd, "0".to_owned(), "0".to_owned()],
                    output_after,
                );
            }
            Ok(())
        }

        fn interrupt_foreground(&self, execution_id: &ExecutionId) -> Result<()> {
            let status = self.status(execution_id)?;
            self.interrupts.fetch_add(1, Ordering::Relaxed);
            if self.command_behavior.load(Ordering::Acquire) == FAKE_COMMAND_FINISH_ON_INTERRUPT {
                let cwd = status.cwd.to_string_lossy().into_owned();
                let output_after = status.last_sequence + 7;
                let transaction = self
                    .integrations
                    .lock()
                    .unwrap()
                    .get(execution_id)
                    .and_then(|integration| integration.active_transaction.clone())
                    .unwrap_or_else(|| "-".to_owned());
                self.push_event(
                    execution_id,
                    "command_finished",
                    &[
                        transaction,
                        "code".to_owned(),
                        "130".to_owned(),
                        cwd.clone(),
                    ],
                    output_after,
                );
                self.integrations
                    .lock()
                    .unwrap()
                    .get_mut(execution_id)
                    .unwrap()
                    .active_transaction = None;
                self.push_event(
                    execution_id,
                    "prompt_ready",
                    &[cwd, "130".to_owned(), "0".to_owned()],
                    output_after,
                );
            }
            Ok(())
        }

        fn handoff_managed_terminal(
            &self,
            execution_id: &ExecutionId,
            owner: ExecutionOwner,
            interrupt_foreground: bool,
        ) -> Result<()> {
            let mut records = self.records.lock().unwrap();
            let status = records
                .get_mut(execution_id)
                .ok_or_else(|| terminal_not_found(&TerminalId::generate()))?;
            status.owner = owner;
            drop(records);
            self.writable_leases.lock().unwrap().remove(execution_id);
            if interrupt_foreground {
                self.interrupts.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
    }

    impl FakeStarter {
        fn push_event(
            &self,
            execution_id: &ExecutionId,
            kind: &str,
            fields: &[String],
            output_through_sequence: u64,
        ) {
            let token = self.token(execution_id);
            let mut integrations = self.integrations.lock().unwrap();
            let integration = integrations.get_mut(execution_id).unwrap();
            integration.next_event_sequence += 1;
            let sequence = integration.next_event_sequence.to_string();
            let mut packet = Vec::new();
            for field in std::iter::once("AGL2")
                .chain(std::iter::once(token.expose_to_managed_startup()))
                .chain(std::iter::once(sequence.as_str()))
                .chain(std::iter::once(kind))
                .chain(fields.iter().map(String::as_str))
            {
                packet.extend_from_slice(field.as_bytes());
                packet.push(0);
            }
            integration
                .packets
                .push_back((packet, output_through_sequence));
            integration.output_through_sequence = output_through_sequence;
            drop(integrations);
            self.records
                .lock()
                .unwrap()
                .get_mut(execution_id)
                .unwrap()
                .last_sequence = output_through_sequence;
        }

        fn set_command_behavior(&self, behavior: usize) {
            assert!(matches!(
                behavior,
                FAKE_COMMAND_IDLE
                    | FAKE_COMMAND_COMPLETE
                    | FAKE_COMMAND_FINISH_ON_INTERRUPT
                    | FAKE_COMMAND_NEVER_RECOVERS
            ));
            self.command_behavior.store(behavior, Ordering::Release);
        }

        fn set_foreground_process_group(
            &self,
            execution_id: &ExecutionId,
            process_group: Option<i32>,
        ) {
            self.integrations
                .lock()
                .unwrap()
                .get_mut(execution_id)
                .unwrap()
                .foreground_process_group = process_group;
        }

        fn written_commands(&self) -> Vec<Vec<u8>> {
            self.writes
                .lock()
                .unwrap()
                .iter()
                .map(|(_, bytes)| bytes.clone())
                .collect()
        }

        fn controls(&self, execution_id: &ExecutionId) -> Vec<Vec<String>> {
            self.controls
                .lock()
                .unwrap()
                .iter()
                .filter(|(found, _)| found == execution_id)
                .map(|(_, fields)| fields.clone())
                .collect()
        }

        fn close_integration(&self, execution_id: &ExecutionId) {
            self.integrations
                .lock()
                .unwrap()
                .get_mut(execution_id)
                .unwrap()
                .channel_closed = true;
        }

        fn set_state(&self, execution_id: &ExecutionId, state: ExecutionState) {
            self.records
                .lock()
                .unwrap()
                .get_mut(execution_id)
                .unwrap()
                .state = state;
        }

        fn token(&self, execution_id: &ExecutionId) -> ShellIntegrationToken {
            self.tokens.lock().unwrap()[execution_id].clone()
        }

        fn set_start_probe(&self, probe: Arc<FakeStartProbe>) {
            *self.start_probe.lock().unwrap() = Some(probe);
        }

        fn fail_next_start(&self, error: ProcessError) {
            *self.start_error.lock().unwrap() = Some(error);
        }
    }

    #[derive(Default)]
    struct FaultTerminalRepository {
        inner: Arc<crate::InMemoryTerminalRepository>,
        recoveries: AtomicUsize,
        fail_replaces: AtomicUsize,
        last_reserved: Mutex<Option<StoredTerminalRecord>>,
    }

    impl FaultTerminalRepository {
        fn fail_next_replace(&self) {
            self.fail_replaces.fetch_add(1, Ordering::Release);
        }

        fn last_reserved(&self) -> StoredTerminalRecord {
            self.last_reserved.lock().unwrap().clone().unwrap()
        }
    }

    impl TerminalRepository for FaultTerminalRepository {
        fn reserve(&self, record: &StoredTerminalRecord) -> Result<TerminalReservation> {
            let reservation = self.inner.reserve(record)?;
            if reservation == TerminalReservation::Created {
                *self.last_reserved.lock().unwrap() = Some(record.clone());
            }
            Ok(reservation)
        }

        fn replace(&self, record: &StoredTerminalRecord) -> Result<()> {
            if self
                .fail_replaces
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(ProcessError::new(
                    ProcessErrorCode::StoreCorrupt,
                    "injected terminal replacement failure",
                ));
            }
            self.inner.replace(record)
        }

        fn recover_for_new_owner(&self) -> Result<Vec<StoredTerminalRecord>> {
            self.recoveries.fetch_add(1, Ordering::Relaxed);
            self.inner.recover_for_new_owner()
        }
    }

    fn fixture() -> (PathBuf, ExecutionContextSnapshot, AdmittedShellProfile) {
        let root = std::env::temp_dir().join(format!(
            "agl-terminal-registry-{}-{}",
            std::process::id(),
            RunId::generate()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let program = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join("bash"))
            .find(|candidate| candidate.is_file())
            .expect("registry tests require an admitted Bash")
            .canonicalize()
            .unwrap();
        let bytes = std::fs::read(&program).unwrap();
        let mut executable_digest = String::from("sha256:");
        use std::fmt::Write as _;
        for byte in Sha256::digest(bytes) {
            write!(&mut executable_digest, "{byte:02x}").unwrap();
        }
        let snapshot = crate::ShellProfileSnapshot {
            program,
            command_args: vec!["-c".to_owned()],
            login_command_args: None,
            environment_names: vec!["LANG".to_owned(), "PATH".to_owned()],
            executable_digest,
            config_digest: "sha256:test-bash".to_owned(),
        };
        let shell = AdmittedShellProfile {
            kind: AdmittedShellKind::Bash,
            snapshot: snapshot.clone(),
        };
        let context = ExecutionContextSnapshot {
            workspace_root: root.clone(),
            working_directory: root.clone(),
            private_execution_roots: Vec::new(),
            shell: snapshot,
            revision: 1,
            profile_metadata: "workspace".to_owned(),
        };
        (root, context, shell)
    }

    fn ensure_request(
        session_id: SessionId,
        context: ExecutionContextSnapshot,
        shell: AdmittedShellProfile,
    ) -> TerminalEnsureRequest {
        let runtime_root = shell.snapshot.program.parent().unwrap().to_path_buf();
        let mut environment = TerminalEnvironmentRequest {
            admitted_path_roots: vec![runtime_root.clone()],
            ..TerminalEnvironmentRequest::default()
        };
        environment.admitted_base.insert(
            "PATH".to_owned(),
            runtime_root.to_string_lossy().into_owned(),
        );
        TerminalEnsureRequest {
            topology_id: topology(&session_id),
            owner: human_owner(&session_id),
            lifecycle_scope_id: LifecycleScopeId::new(RunId::generate().as_str()).unwrap(),
            correlation: ExecutionCorrelation::new(
                CallerNamespace::new("agentlibre", 1).unwrap(),
                agl_exec::CorrelationGroupId::new(RunId::generate().as_str()).unwrap(),
                agl_exec::CorrelationOperationId::new(StepId::generate().as_str()).unwrap(),
            ),
            context,
            profile: ExecutionProfile::Workspace,
            shell,
            environment,
            runtime_read_only_roots: Vec::new(),
            host_startup: HostStartupPolicy::ManagedOnly,
            authorization: ExecutionAuthorization::default(),
            grant_lease: None,
            terminal_size: TerminalSize::default(),
            limits: ExecutionLimits {
                timeout_ms: None,
                max_input_bytes: 1024 * 1024,
                max_output_bytes: 1024 * 1024,
            },
            history_seed: TerminalHistorySeed::empty(),
        }
    }

    fn agent_ensure_request(
        session_id: SessionId,
        context: ExecutionContextSnapshot,
        shell: AdmittedShellProfile,
    ) -> TerminalEnsureRequest {
        let mut request = ensure_request(session_id.clone(), context, shell);
        request.owner = persistent_agent_owner(&session_id);
        request
    }

    fn stored_reservation(request: &TerminalEnsureRequest) -> StoredTerminalRecord {
        let environment = request.environment.resolve(&NoSecrets).unwrap();
        let environment_digest = environment.digest().clone();
        let record = TerminalRecord {
            terminal_id: TerminalId::generate(),
            execution_id: ExecutionId::generate(),
            topology_id: request.topology_id.clone(),
            owner: request.owner.clone(),
            lifecycle_scope_id: request.lifecycle_scope_id.clone(),
            profile: request.profile,
            workspace_root: request.context.workspace_root.clone(),
            shell_profile: request.shell.clone(),
            environment_digest: environment_digest.clone(),
            command_sequence: 0,
            prompt_state: TerminalPromptState::Unknown,
            integration_health: ShellIntegrationHealth::AwaitingFirstPrompt,
            cwd: request.context.working_directory.clone(),
            state: TerminalState::Starting,
        };
        StoredTerminalRecord {
            slot_key: terminal_slot_key(&record).unwrap(),
            fingerprint: terminal_fingerprint(request, &environment_digest),
            active_slot: true,
            record,
        }
    }

    fn append_integration_frame(target: &mut Vec<u8>, fields: &[&str]) {
        for field in fields {
            target.extend_from_slice(field.as_bytes());
            target.push(0);
        }
    }

    #[test]
    fn durable_reservation_precedes_the_exact_reserved_start() {
        let starter = Arc::new(FakeStarter::default());
        let repository = Arc::new(FaultTerminalRepository::default());
        let observed = Arc::new(AtomicUsize::new(0));
        let probe_repository = repository.clone();
        let probe_observed = observed.clone();
        starter.set_start_probe(Arc::new(move |execution_id, _, _| {
            let reserved = probe_repository.last_reserved();
            assert_eq!(&reserved.record.execution_id, execution_id);
            assert_eq!(
                probe_repository
                    .inner
                    .record(&reserved.record.terminal_id)?
                    .record
                    .state,
                TerminalState::Starting
            );
            probe_observed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }));
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            repository.clone(),
        )
        .unwrap();
        let (root, context, shell) = fixture();

        let terminal = registry
            .ensure_terminal(ensure_request(SessionId::generate(), context, shell))
            .unwrap();

        assert_eq!(starter.starts.load(Ordering::Relaxed), 1);
        assert_eq!(observed.load(Ordering::Relaxed), 1);
        assert_eq!(repository.recoveries.load(Ordering::Relaxed), 1);
        let durable = repository.inner.record(&terminal.terminal_id).unwrap();
        assert_eq!(durable.record, terminal);
        assert_eq!(durable.record.state, TerminalState::Running);
        assert!(durable.active_slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolved_secret_uses_only_the_private_startup_channel() {
        const SECRET_NAME: &str = "AGL_TEST_PRIVATE_TOKEN";
        const SECRET_REFERENCE: &str = "session-owner:token";
        const SENTINEL: &str = "launch-only-secret-sentinel-7f20e7";

        struct ScopedSecrets;

        impl TerminalSecretResolver for ScopedSecrets {
            fn resolve(
                &self,
                reference: &crate::terminal::environment::TerminalSecretReference,
            ) -> Result<TerminalSecretValue> {
                if reference.as_str() == SECRET_REFERENCE {
                    TerminalSecretValue::new(SENTINEL)
                } else {
                    Err(ProcessError::new(
                        ProcessErrorCode::ExecutionNotOwned,
                        format!("sibling resolver accidentally rendered {SENTINEL}"),
                    ))
                }
            }
        }

        let starter = Arc::new(FakeStarter::default());
        let observed = Arc::new(AtomicUsize::new(0));
        let probe_observed = observed.clone();
        starter.set_start_probe(Arc::new(move |_, request, startup| {
            assert!(!request.environment.values.contains_key(SECRET_NAME));
            assert!(!format!("{request:?}").contains(SENTINEL));
            assert!(!serde_json::to_string(request).unwrap().contains(SENTINEL));
            assert!(!format!("{startup:?}").contains(SENTINEL));
            assert_eq!(
                startup
                    .private_environment
                    .exposed_values()
                    .find(|(name, _)| *name == SECRET_NAME)
                    .map(|(_, value)| value),
                Some(SENTINEL)
            );
            probe_observed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }));
        let repository = Arc::new(FaultTerminalRepository::default());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(ScopedSecrets),
            repository.clone(),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let mut request = ensure_request(SessionId::generate(), context.clone(), shell.clone());
        request.environment.agl_env.insert(
            SECRET_NAME.to_owned(),
            crate::terminal::environment::TerminalEnvironmentValue::Secret(
                crate::terminal::environment::TerminalSecretReference::new(SECRET_REFERENCE)
                    .unwrap(),
            ),
        );
        assert!(!format!("{request:?}").contains(SENTINEL));
        assert!(
            !serde_json::to_string(&request.environment)
                .unwrap()
                .contains(SENTINEL)
        );

        let terminal = registry.ensure_terminal(request).unwrap();

        assert_eq!(starter.starts.load(Ordering::Relaxed), 1);
        assert_eq!(observed.load(Ordering::Relaxed), 1);
        let durable = repository.inner.record(&terminal.terminal_id).unwrap();
        assert!(!format!("{durable:?}").contains(SENTINEL));
        assert!(!durable.fingerprint.contains(SENTINEL));
        assert!(
            !durable
                .record
                .environment_digest
                .as_str()
                .contains(SENTINEL)
        );

        let rejected_starter = Arc::new(FakeStarter::default());
        let rejected_repository = Arc::new(FaultTerminalRepository::default());
        let rejected_registry = TerminalRegistry::with_starter(
            rejected_starter.clone(),
            Arc::new(ScopedSecrets),
            rejected_repository.clone(),
        )
        .unwrap();
        let mut sibling = ensure_request(SessionId::generate(), context, shell);
        sibling.environment.agl_env.insert(
            SECRET_NAME.to_owned(),
            crate::terminal::environment::TerminalEnvironmentValue::Secret(
                crate::terminal::environment::TerminalSecretReference::new("sibling:token")
                    .unwrap(),
            ),
        );
        let error = rejected_registry.ensure_terminal(sibling).unwrap_err();
        assert_eq!(error.code(), ProcessErrorCode::ExecutionNotOwned);
        assert!(!error.message().contains(SENTINEL));
        assert!(!format!("{error:?}").contains(SENTINEL));
        assert_eq!(rejected_starter.starts.load(Ordering::Relaxed), 0);
        let rejected = rejected_repository.last_reserved();
        let rejected = rejected_repository
            .inner
            .record(&rejected.record.terminal_id)
            .unwrap();
        assert_eq!(rejected.record.state, TerminalState::Failed);
        assert!(!rejected.active_slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exact_secret_retry_bypasses_an_unavailable_resolver_and_conflicts_by_reference() {
        const SECRET_NAME: &str = "AGL_TEST_RETRY_SECRET";
        const SECRET_REFERENCE: &str = "session-owner:retry-token";

        struct FlakySecrets {
            available: AtomicBool,
            calls: AtomicUsize,
        }

        impl TerminalSecretResolver for FlakySecrets {
            fn resolve(
                &self,
                _reference: &crate::terminal::environment::TerminalSecretReference,
            ) -> Result<TerminalSecretValue> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                if self.available.load(Ordering::Acquire) {
                    TerminalSecretValue::new("resolved-once-only")
                } else {
                    Err(ProcessError::new(
                        ProcessErrorCode::Internal,
                        "temporarily unavailable test resolver",
                    ))
                }
            }
        }

        fn add_secret(request: &mut TerminalEnsureRequest, reference: &str) {
            request.environment.agl_env.insert(
                SECRET_NAME.to_owned(),
                crate::terminal::environment::TerminalEnvironmentValue::Secret(
                    crate::terminal::environment::TerminalSecretReference::new(reference).unwrap(),
                ),
            );
        }

        let secrets = Arc::new(FlakySecrets {
            available: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
        });
        let starter = Arc::new(FakeStarter::default());
        let repository = Arc::new(FaultTerminalRepository::default());
        let registry =
            TerminalRegistry::with_starter(starter.clone(), secrets.clone(), repository).unwrap();
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let mut first_request = ensure_request(session_id.clone(), context.clone(), shell.clone());
        add_secret(&mut first_request, SECRET_REFERENCE);

        let first = registry.ensure_terminal(first_request).unwrap();
        assert_eq!(secrets.calls.load(Ordering::Relaxed), 1);
        assert_eq!(starter.starts.load(Ordering::Relaxed), 1);
        secrets.available.store(false, Ordering::Release);

        let mut exact_retry = ensure_request(session_id.clone(), context.clone(), shell.clone());
        add_secret(&mut exact_retry, SECRET_REFERENCE);
        let retry = registry.ensure_terminal(exact_retry).unwrap();

        assert_eq!(retry, first);
        assert_eq!(secrets.calls.load(Ordering::Relaxed), 1);
        assert_eq!(starter.starts.load(Ordering::Relaxed), 1);

        let mut conflicting = ensure_request(session_id, context, shell);
        add_secret(&mut conflicting, "session-owner:different-retry-token");
        assert_eq!(
            registry.ensure_terminal(conflicting).unwrap_err().code(),
            ProcessErrorCode::StateConflict
        );
        assert_eq!(secrets.calls.load(Ordering::Relaxed), 1);
        assert_eq!(starter.starts.load(Ordering::Relaxed), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn spawn_failure_is_persisted_and_preserves_the_launch_error() {
        let starter = Arc::new(FakeStarter::default());
        starter.fail_next_start(ProcessError::new(
            ProcessErrorCode::SpawnFailed,
            "injected managed terminal launch failure",
        ));
        let repository = Arc::new(FaultTerminalRepository::default());
        let registry =
            TerminalRegistry::with_starter(starter, Arc::new(NoSecrets), repository.clone())
                .unwrap();
        let (root, context, shell) = fixture();

        let error = registry
            .ensure_terminal(ensure_request(SessionId::generate(), context, shell))
            .unwrap_err();

        assert_eq!(error.code(), ProcessErrorCode::SpawnFailed);
        let reserved = repository.last_reserved();
        let durable = repository
            .inner
            .record(&reserved.record.terminal_id)
            .unwrap();
        assert_eq!(durable.record.state, TerminalState::Failed);
        assert!(!durable.active_slot);
        assert_eq!(
            registry.record(&reserved.record.terminal_id).unwrap().state,
            TerminalState::Failed
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reserved_before_spawn_recovers_unknown_and_retry_never_launches() {
        let starter = Arc::new(FakeStarter::default());
        let repository = Arc::new(FaultTerminalRepository::default());
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let request = ensure_request(session_id.clone(), context.clone(), shell.clone());
        let reserved = stored_reservation(&request);
        repository.reserve(&reserved).unwrap();

        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            repository.clone(),
        )
        .unwrap();
        let retry = registry.ensure_terminal(request).unwrap();

        assert_eq!(retry.terminal_id, reserved.record.terminal_id);
        assert_eq!(retry.execution_id, reserved.record.execution_id);
        assert_eq!(retry.state, TerminalState::OutcomeUnknown);
        assert_eq!(retry.prompt_state, TerminalPromptState::Degraded);
        assert_eq!(retry.integration_health, ShellIntegrationHealth::Degraded);
        assert_eq!(starter.starts.load(Ordering::Relaxed), 0);
        assert!(
            registry.lock().unwrap().terminals[&retry.terminal_id]
                .integration
                .is_none()
        );
        assert_eq!(
            registry
                .poll_private_integration(&retry.terminal_id, 1024)
                .err()
                .unwrap()
                .code(),
            ProcessErrorCode::ExecutionNotLive
        );
        assert_eq!(starter.integration_reads.load(Ordering::Relaxed), 0);

        let mut conflicting = ensure_request(session_id, context, shell);
        conflicting.environment.agl_env.insert(
            "MODE".to_owned(),
            crate::terminal::environment::TerminalEnvironmentValue::Plain("different".to_owned()),
        );
        assert_eq!(
            registry.ensure_terminal(conflicting).unwrap_err().code(),
            ProcessErrorCode::StateConflict
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_reservation_is_returned_without_spawn() {
        let starter = Arc::new(FakeStarter::default());
        let repository = Arc::new(FaultTerminalRepository::default());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            repository.clone(),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let request = ensure_request(SessionId::generate(), context, shell);
        let reserved = stored_reservation(&request);
        repository.reserve(&reserved).unwrap();

        let existing = registry.ensure_terminal(request).unwrap();

        assert_eq!(existing, reserved.record);
        assert_eq!(starter.starts.load(Ordering::Relaxed), 0);
        assert!(
            registry.lock().unwrap().terminals[&existing.terminal_id]
                .integration
                .is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn post_spawn_replace_failure_recovers_same_ids_without_relaunch() {
        let starter = Arc::new(FakeStarter::default());
        let repository = Arc::new(FaultTerminalRepository::default());
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let first_registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            repository.clone(),
        )
        .unwrap();
        repository.fail_next_replace();

        let error = first_registry
            .ensure_terminal(ensure_request(
                session_id.clone(),
                context.clone(),
                shell.clone(),
            ))
            .unwrap_err();

        assert_eq!(error.code(), ProcessErrorCode::StoreCorrupt);
        assert_eq!(starter.starts.load(Ordering::Relaxed), 1);
        assert_eq!(starter.kills.load(Ordering::Relaxed), 1);
        let reserved = repository.last_reserved();
        assert_eq!(
            repository
                .inner
                .record(&reserved.record.terminal_id)
                .unwrap()
                .record
                .state,
            TerminalState::Starting
        );
        assert_eq!(
            first_registry
                .record(&reserved.record.terminal_id)
                .unwrap()
                .state,
            TerminalState::OutcomeUnknown
        );
        drop(first_registry);

        let recovered_registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            repository.clone(),
        )
        .unwrap();
        let retry = recovered_registry
            .ensure_terminal(ensure_request(session_id, context, shell))
            .unwrap();
        assert_eq!(retry.terminal_id, reserved.record.terminal_id);
        assert_eq!(retry.execution_id, reserved.record.execution_id);
        assert_eq!(retry.state, TerminalState::OutcomeUnknown);
        assert_eq!(starter.starts.load(Ordering::Relaxed), 1);
        assert_eq!(repository.recoveries.load(Ordering::Relaxed), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn promotion_persistence_failure_revokes_the_old_owner_conservatively() {
        let starter = Arc::new(FakeStarter::default());
        let repository = Arc::new(FaultTerminalRepository::default());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            repository.clone(),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let owner_run_id = RunId::generate();
        let mut request = ensure_request(session_id.clone(), context.clone(), shell.clone());
        request.owner = ephemeral_agent_owner(&owner_run_id);
        let terminal = registry.ensure_terminal(request).unwrap();
        repository.fail_next_replace();

        let error = registry
            .promote_ephemeral_owner(
                &terminal.terminal_id,
                &TerminalTopologyId::new(caller_id(session_id.as_str())),
                persistent_agent_owner(&session_id).caller().clone(),
            )
            .unwrap_err();

        assert_eq!(error.code(), ProcessErrorCode::StoreCorrupt);
        let conservative = registry.record(&terminal.terminal_id).unwrap();
        assert_eq!(conservative.state, TerminalState::OutcomeUnknown);
        assert_eq!(
            conservative
                .owner
                .previous_owner()
                .unwrap()
                .owner_id()
                .as_str(),
            owner_run_id.as_str()
        );
        assert!(
            registry.lock().unwrap().terminals[&terminal.terminal_id]
                .integration
                .is_none()
        );
        assert_eq!(
            starter
                .status(&terminal.execution_id)
                .unwrap()
                .owner
                .caller()
                .owner_id()
                .as_str(),
            session_id.as_str()
        );
        assert!(
            repository
                .inner
                .record(&terminal.terminal_id)
                .unwrap()
                .record
                .owner
                .is_ephemeral()
        );

        let mut replacement = ensure_request(session_id, context, shell);
        replacement.owner = ephemeral_agent_owner(&owner_run_id);
        assert_eq!(
            registry.ensure_terminal(replacement).unwrap_err().code(),
            ProcessErrorCode::ExecutionNotOwned
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn termination_persistence_failure_fences_runtime_as_outcome_unknown() {
        let starter = Arc::new(FakeStarter::default());
        let repository = Arc::new(FaultTerminalRepository::default());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            repository.clone(),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let terminal = registry
            .ensure_terminal(ensure_request(SessionId::generate(), context, shell))
            .unwrap();
        repository.fail_next_replace();

        let error = registry
            .terminate_terminal(&terminal.terminal_id, KillMode::Immediate)
            .unwrap_err();

        assert_eq!(error.code(), ProcessErrorCode::StoreCorrupt);
        assert_eq!(starter.kills.load(Ordering::Relaxed), 1);
        assert_eq!(
            registry.record(&terminal.terminal_id).unwrap().state,
            TerminalState::OutcomeUnknown
        );
        assert!(
            registry.lock().unwrap().terminals[&terminal.terminal_id]
                .integration
                .is_none()
        );
        assert_eq!(
            repository
                .inner
                .record(&terminal.terminal_id)
                .unwrap()
                .record
                .state,
            TerminalState::Running
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ensure_is_idempotent_and_conflicting_immutable_metadata_is_rejected() {
        let starter = Arc::new(FakeStarter::default());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            Arc::new(crate::InMemoryTerminalRepository::new()),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let first = registry
            .ensure_terminal(ensure_request(
                session_id.clone(),
                context.clone(),
                shell.clone(),
            ))
            .unwrap();
        let second = registry
            .ensure_terminal(ensure_request(
                session_id.clone(),
                context.clone(),
                shell.clone(),
            ))
            .unwrap();
        assert_eq!(first.terminal_id, second.terminal_id);
        assert_eq!(first.execution_id, second.execution_id);
        assert_eq!(starter.starts.load(Ordering::Relaxed), 1);

        let child = root.join("later-chat-cwd");
        std::fs::create_dir(&child).unwrap();
        let mut later_context = context.clone();
        later_context.working_directory = child;
        later_context.revision += 1;
        let later = registry
            .ensure_terminal(ensure_request(
                session_id.clone(),
                later_context,
                shell.clone(),
            ))
            .unwrap();
        assert_eq!(first.terminal_id, later.terminal_id);
        assert_eq!(first.execution_id, later.execution_id);

        let mut changed = ensure_request(session_id, context, shell);
        changed.environment.agl_env.insert(
            "MODE".to_owned(),
            crate::terminal::environment::TerminalEnvironmentValue::Plain("changed".to_owned()),
        );
        assert_eq!(
            registry.ensure_terminal(changed).unwrap_err().code(),
            ProcessErrorCode::StateConflict
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn human_host_terminal_rejects_convertible_capability_grant_authority() {
        let (root, context, shell) = fixture();
        let mut request = ensure_request(SessionId::generate(), context, shell);
        request.profile = ExecutionProfile::Host;
        request.authorization.host_process_execution = true;
        request.grant_lease = Some(crate::ExecutionGrantLease {
            origin: crate::ExecutionLeaseOrigin::ToolGrant,
            grant_id: "model-capability-grant".to_owned(),
            duration: "session".to_owned(),
            scope_digest: "sha256:model-capability".to_owned(),
        });

        assert_eq!(
            validate_ensure_request(&request).unwrap_err().code(),
            ProcessErrorCode::HostAuthorityRequired
        );

        let lease = request.grant_lease.as_mut().unwrap();
        lease.origin = crate::ExecutionLeaseOrigin::LocalOperatorTerminal;
        lease.duration = crate::LOCAL_OPERATOR_TERMINAL_LEASE_DURATION.to_owned();
        validate_ensure_request(&request).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_command_api_serializes_exact_writes_and_returns_output_boundaries() {
        let starter = Arc::new(FakeStarter::default());
        starter.set_command_behavior(FAKE_COMMAND_COMPLETE);
        let registry = Arc::new(
            TerminalRegistry::with_starter(
                starter.clone(),
                Arc::new(NoSecrets),
                Arc::new(crate::InMemoryTerminalRepository::new()),
            )
            .unwrap(),
        );
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let first_request =
            agent_ensure_request(session_id.clone(), context.clone(), shell.clone());
        let second_request = agent_ensure_request(session_id, context, shell);
        let first_registry = registry.clone();
        let first = std::thread::spawn(move || {
            first_registry.execute_agent_command(
                first_request,
                "printf '%s' first".to_owned(),
                Some(Instant::now() + Duration::from_secs(10)),
            )
        });
        let second_registry = registry.clone();
        let second = std::thread::spawn(move || {
            second_registry.execute_agent_command(
                second_request,
                "pwd && printf '%s' second".to_owned(),
                Some(Instant::now() + Duration::from_secs(10)),
            )
        });
        let first = first.join().unwrap().unwrap();
        let second = second.join().unwrap().unwrap();

        assert_eq!(first.execution_id, second.execution_id);
        let mut sequences = [first.command_sequence, second.command_sequence];
        sequences.sort_unstable();
        assert_eq!(sequences, [1, 2]);
        let mut ranges = [first.output.clone(), second.output.clone()];
        ranges.sort_by_key(|range| range.after_sequence);
        assert_eq!(ranges[0].after_sequence, 2);
        assert_eq!(ranges[0].through_sequence, ranges[1].after_sequence);
        assert!(ranges[1].through_sequence > ranges[1].after_sequence);
        let mut writes = starter.written_commands();
        writes.sort();
        assert_eq!(
            writes,
            vec![
                b"\x1b[200~printf '%s' first\x1b[201~\n".to_vec(),
                b"\x1b[200~pwd && printf '%s' second\x1b[201~\n".to_vec(),
            ]
        );
        assert_eq!(starter.starts.load(Ordering::Relaxed), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn timed_out_agent_command_interrupts_then_reuses_recovered_prompt() {
        let starter = Arc::new(FakeStarter::default());
        starter.set_command_behavior(FAKE_COMMAND_FINISH_ON_INTERRUPT);
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            Arc::new(crate::InMemoryTerminalRepository::new()),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let error = registry
            .execute_agent_command(
                agent_ensure_request(session_id.clone(), context.clone(), shell.clone()),
                "sleep forever".to_owned(),
                Some(Instant::now() + Duration::from_millis(10)),
            )
            .unwrap_err();
        assert_eq!(error.code(), ProcessErrorCode::TimedOut);
        assert_eq!(starter.interrupts.load(Ordering::Relaxed), 1);
        assert_eq!(starter.kills.load(Ordering::Relaxed), 0);

        starter.set_command_behavior(FAKE_COMMAND_COMPLETE);
        let recovered = registry
            .execute_agent_command(
                agent_ensure_request(session_id, context, shell),
                "printf recovered".to_owned(),
                Some(Instant::now() + Duration::from_secs(1)),
            )
            .unwrap();
        assert_eq!(recovered.command_sequence, 2);
        assert_eq!(starter.starts.load(Ordering::Relaxed), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn timed_out_agent_command_kills_terminal_without_prompt_recovery() {
        let starter = Arc::new(FakeStarter::default());
        starter.set_command_behavior(FAKE_COMMAND_NEVER_RECOVERS);
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            Arc::new(crate::InMemoryTerminalRepository::new()),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let error = registry
            .execute_agent_command(
                agent_ensure_request(SessionId::generate(), context, shell),
                "sleep forever".to_owned(),
                Some(Instant::now() + Duration::from_millis(10)),
            )
            .unwrap_err();
        assert_eq!(error.code(), ProcessErrorCode::TimedOut);
        assert_eq!(starter.interrupts.load(Ordering::Relaxed), 1);
        assert_eq!(starter.kills.load(Ordering::Relaxed), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn topology_separates_human_main_and_subagents_and_promotion_revokes_agent_owner() {
        let starter = Arc::new(FakeStarter::default());
        let repository = Arc::new(crate::InMemoryTerminalRepository::new());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            repository.clone(),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let human = registry
            .ensure_terminal(ensure_request(
                session_id.clone(),
                context.clone(),
                shell.clone(),
            ))
            .unwrap();
        let mut main = ensure_request(session_id.clone(), context.clone(), shell.clone());
        main.owner = persistent_agent_owner(&session_id);
        let main = registry.ensure_terminal(main).unwrap();
        let owner_run_id = RunId::generate();
        let mut subagent = ensure_request(session_id.clone(), context.clone(), shell.clone());
        subagent.owner = ephemeral_agent_owner(&owner_run_id);
        let subagent = registry.ensure_terminal(subagent).unwrap();
        assert_ne!(human.terminal_id, main.terminal_id);
        assert_ne!(main.terminal_id, subagent.terminal_id);
        let topology_id = TerminalTopologyId::new(caller_id(session_id.as_str()));
        assert_eq!(registry.list_topology(&topology_id).unwrap().len(), 3);

        registry
            .poll_private_integration(&subagent.terminal_id, AGENT_TERMINAL_INTEGRATION_READ_BYTES)
            .unwrap();
        let lease = starter
            .attach(&subagent.execution_id, ExecutionRequestId::generate())
            .unwrap();
        let (active_sequence, queued_sequence) = {
            let mut state = registry.lock().unwrap();
            let entry = state.terminals.get_mut(&subagent.terminal_id).unwrap();
            let active_sequence = entry
                .commands
                .enqueue("sleep forever".to_owned(), None)
                .unwrap();
            let queued_sequence = entry.commands.enqueue("pwd".to_owned(), None).unwrap();
            entry
                .commands
                .begin_next(&entry.record.prompt_state, entry.output_sequence)
                .unwrap()
                .unwrap();
            entry.commands.reserve_submission().unwrap();
            entry.commands.complete_submission().unwrap();
            entry.agent_input_lease = Some(lease);
            (active_sequence, queued_sequence)
        };

        let promoted = registry
            .promote_ephemeral_owner(
                &subagent.terminal_id,
                &topology_id,
                persistent_agent_owner(&session_id).caller().clone(),
            )
            .unwrap();
        assert_eq!(
            promoted.owner.previous_owner().unwrap().owner_id().as_str(),
            owner_run_id.as_str()
        );
        let durable_promoted = repository.record(&subagent.terminal_id).unwrap();
        assert_eq!(durable_promoted.record, promoted);
        assert_eq!(
            durable_promoted.slot_key,
            format!("promoted:workspace:{}", subagent.terminal_id)
        );
        assert!(durable_promoted.active_slot);
        {
            let state = registry.lock().unwrap();
            let entry = state.terminals.get(&subagent.terminal_id).unwrap();
            assert_eq!(entry.commands.active_sequence(), None);
            assert_eq!(entry.commands.queued_len(), 0);
            for sequence in [active_sequence, queued_sequence] {
                assert_eq!(
                    entry.completed_commands[&sequence]
                        .as_ref()
                        .unwrap_err()
                        .code(),
                    ProcessErrorCode::ExecutionNotOwned
                );
            }
            assert!(entry.agent_input_lease.is_none());
        }
        assert!(
            starter
                .writable_leases
                .lock()
                .unwrap()
                .get(&subagent.execution_id)
                .is_none()
        );
        assert_eq!(starter.interrupts.load(Ordering::Relaxed), 1);
        assert_eq!(starter.kills.load(Ordering::Relaxed), 0);
        assert_eq!(
            starter
                .status(&subagent.execution_id)
                .unwrap()
                .owner
                .caller()
                .owner_id()
                .as_str(),
            session_id.as_str()
        );
        assert_eq!(
            registry
                .terminate_ephemeral_owner(&caller_id(owner_run_id.as_str()))
                .unwrap(),
            0
        );
        let mut replacement = ensure_request(session_id.clone(), context, shell);
        replacement.owner = ephemeral_agent_owner(&owner_run_id);
        assert_eq!(
            registry.ensure_terminal(replacement).unwrap_err().code(),
            ProcessErrorCode::ExecutionNotOwned
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_private_contiguous_integration_updates_cwd_and_command_state() {
        let starter = Arc::new(FakeStarter::default());
        let repository = Arc::new(crate::InMemoryTerminalRepository::new());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            repository.clone(),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let terminal = registry
            .ensure_terminal(ensure_request(session_id, context, shell))
            .unwrap();
        let child = root.join("child");
        std::fs::create_dir(&child).unwrap();
        let token = starter.token(&terminal.execution_id);
        let mut frames = Vec::new();
        append_integration_frame(
            &mut frames,
            &[
                "AGL2",
                token.expose_to_managed_startup(),
                "1",
                "prompt_ready",
                root.to_str().unwrap(),
                "-",
                "0",
            ],
        );
        append_integration_frame(
            &mut frames,
            &[
                "AGL2",
                token.expose_to_managed_startup(),
                "2",
                "command_started",
                "-",
                "cd child",
                root.to_str().unwrap(),
            ],
        );
        let batch = registry
            .accept_private_integration(&terminal.terminal_id, &frames)
            .unwrap();
        assert_eq!(batch.events.len(), 2);
        let active = registry.record(&terminal.terminal_id).unwrap();
        assert_eq!(active.command_sequence, 1);
        assert_eq!(active.integration_health, ShellIntegrationHealth::Trusted);
        assert_eq!(
            repository.record(&terminal.terminal_id).unwrap().record,
            active
        );

        let outside = root.parent().unwrap().canonicalize().unwrap();
        let mut escape = Vec::new();
        append_integration_frame(
            &mut escape,
            &[
                "AGL2",
                token.expose_to_managed_startup(),
                "3",
                "command_finished",
                "-",
                "code",
                "0",
                outside.to_str().unwrap(),
            ],
        );
        let batch = registry
            .accept_private_integration(&terminal.terminal_id, &escape)
            .unwrap();
        assert_eq!(batch.notice.unwrap().code, "shell_integration_degraded");
        let degraded = registry.record(&terminal.terminal_id).unwrap();
        assert_eq!(
            degraded.integration_health,
            ShellIntegrationHealth::Degraded
        );
        assert_eq!(degraded.cwd, root);
        assert_eq!(
            repository.record(&terminal.terminal_id).unwrap().record,
            degraded
        );
        std::fs::remove_dir_all(&degraded.workspace_root).unwrap();
    }

    #[test]
    fn final_refresh_degrades_integration_before_the_record_becomes_immutable() {
        let starter = Arc::new(FakeStarter::default());
        let repository = Arc::new(crate::InMemoryTerminalRepository::new());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            repository.clone(),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let terminal = registry
            .ensure_terminal(ensure_request(SessionId::generate(), context, shell))
            .unwrap();

        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        let trusted = registry.record(&terminal.terminal_id).unwrap();
        assert!(trusted.prompt_state.is_trusted_ready());
        assert_eq!(trusted.integration_health, ShellIntegrationHealth::Trusted);

        starter.set_state(&terminal.execution_id, ExecutionState::Cancelled);
        let final_status = starter.status(&terminal.execution_id).unwrap();
        let final_record = registry.refresh(&terminal.terminal_id).unwrap();

        assert_eq!(final_record.state, TerminalState::Failed);
        assert_eq!(final_record.prompt_state, TerminalPromptState::Degraded);
        assert_eq!(
            final_record.integration_health,
            ShellIntegrationHealth::Degraded
        );
        let durable = repository.record(&terminal.terminal_id).unwrap();
        assert_eq!(durable.record, final_record);
        assert!(!durable.active_slot);
        assert_eq!(
            registry
                .integration_closed(&terminal.terminal_id)
                .expect("already-closed final integration must be idempotent"),
            None
        );
        assert_eq!(
            registry
                .persist_execution_status(&terminal.terminal_id, final_status)
                .expect("a concurrent stale refresh must observe the immutable final record"),
            final_record
        );
        assert_eq!(
            registry.record(&terminal.terminal_id).unwrap(),
            final_record
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn confirmed_retirement_releases_slot_but_keeps_finished_record() {
        let starter = Arc::new(FakeStarter::default());
        let repository = Arc::new(crate::InMemoryTerminalRepository::new());
        let registry =
            TerminalRegistry::with_starter(starter, Arc::new(NoSecrets), repository.clone())
                .unwrap();
        let (first_root, first_context, first_shell) = fixture();
        let session_id = SessionId::generate();
        let first = registry
            .ensure_terminal(ensure_request(
                session_id.clone(),
                first_context,
                first_shell,
            ))
            .unwrap();
        registry
            .terminate_terminal(&first.terminal_id, KillMode::Graceful)
            .unwrap();
        let durable_stopping = repository.record(&first.terminal_id).unwrap();
        assert_eq!(durable_stopping.record.state, TerminalState::Stopping);
        assert!(durable_stopping.active_slot);
        let retired = registry.retire_terminal_slot(&first.terminal_id).unwrap();
        assert_eq!(retired.state, TerminalState::Failed);
        let durable_retired = repository.record(&first.terminal_id).unwrap();
        assert_eq!(durable_retired.record, retired);
        assert!(!durable_retired.active_slot);

        let (second_root, second_context, second_shell) = fixture();
        let second = registry
            .ensure_terminal(ensure_request(
                session_id.clone(),
                second_context,
                second_shell,
            ))
            .unwrap();
        assert_ne!(first.terminal_id, second.terminal_id);
        assert_ne!(first.execution_id, second.execution_id);
        let records = registry
            .list_topology(&TerminalTopologyId::new(caller_id(session_id.as_str())))
            .unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|record| {
            record.terminal_id == first.terminal_id && record.state == TerminalState::Failed
        }));
        assert!(records.iter().any(|record| {
            record.terminal_id == second.terminal_id && record.state == TerminalState::Running
        }));
        std::fs::remove_dir_all(first_root).unwrap();
        std::fs::remove_dir_all(second_root).unwrap();
    }

    #[test]
    fn unknown_or_live_terminal_cannot_release_topology_slot() {
        let starter = Arc::new(FakeStarter::default());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            Arc::new(crate::InMemoryTerminalRepository::new()),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let terminal = registry
            .ensure_terminal(ensure_request(
                session_id.clone(),
                context.clone(),
                shell.clone(),
            ))
            .unwrap();
        assert_eq!(
            registry
                .retire_terminal_slot(&terminal.terminal_id)
                .unwrap_err()
                .code(),
            ProcessErrorCode::StateConflict
        );

        starter.set_state(&terminal.execution_id, ExecutionState::OutcomeUnknown);
        assert_eq!(
            registry
                .retire_terminal_slot(&terminal.terminal_id)
                .unwrap_err()
                .code(),
            ProcessErrorCode::StateConflict
        );
        let mut changed = ensure_request(session_id, context, shell);
        changed.context.revision += 1;
        let reused = registry.ensure_terminal(changed).unwrap();
        assert_eq!(reused.terminal_id, terminal.terminal_id);
        assert_eq!(reused.execution_id, terminal.execution_id);
        assert_eq!(reused.state, TerminalState::OutcomeUnknown);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn kernel_foreground_samples_project_authoritative_program_state() {
        let starter = Arc::new(FakeStarter::default());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            Arc::new(crate::InMemoryTerminalRepository::new()),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let terminal = registry
            .ensure_terminal(ensure_request(SessionId::generate(), context, shell))
            .unwrap();
        let initial = registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        assert!(matches!(
            initial.events.as_slice(),
            [ShellIntegrationEvent::PromptReady { sequence: 1, .. }]
        ));

        starter.push_event(
            &terminal.execution_id,
            "command_started",
            &[
                "-".to_owned(),
                "sleep 10".to_owned(),
                root.to_string_lossy().into_owned(),
            ],
            3,
        );
        starter.set_foreground_process_group(&terminal.execution_id, Some(4242));
        let foreground = registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        assert!(matches!(
            foreground.events.as_slice(),
            [
                ShellIntegrationEvent::CommandStarted { sequence: 2, .. },
                ShellIntegrationEvent::ForegroundChanged {
                    sequence: 3,
                    process_group: Some(4242),
                }
            ]
        ));
        assert_eq!(
            registry.record(&terminal.terminal_id).unwrap().prompt_state,
            TerminalPromptState::ForegroundProgram {
                sequence: 3,
                process_group: 4242,
            }
        );

        starter.set_foreground_process_group(&terminal.execution_id, None);
        let shell_foreground = registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        assert_eq!(
            shell_foreground.events,
            vec![ShellIntegrationEvent::ForegroundChanged {
                sequence: 4,
                process_group: None,
            }]
        );
        assert_eq!(
            registry.record(&terminal.terminal_id).unwrap().prompt_state,
            TerminalPromptState::CommandRunning { sequence: 4 }
        );

        starter.push_event(
            &terminal.execution_id,
            "command_finished",
            &[
                "-".to_owned(),
                "code".to_owned(),
                "0".to_owned(),
                root.to_string_lossy().into_owned(),
            ],
            4,
        );
        starter.push_event(
            &terminal.execution_id,
            "prompt_ready",
            &[
                root.to_string_lossy().into_owned(),
                "0".to_owned(),
                "0".to_owned(),
            ],
            5,
        );
        let finished = registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        let prompt = registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        assert_eq!(
            finished
                .events
                .iter()
                .chain(prompt.events.iter())
                .map(ShellIntegrationEvent::sequence)
                .collect::<Vec<_>>(),
            vec![5, 6]
        );
        assert!(
            registry
                .record(&terminal.terminal_id)
                .unwrap()
                .prompt_state
                .is_trusted_ready()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn human_command_admission_is_prompt_generation_gated_and_never_queued() {
        let starter = Arc::new(FakeStarter::default());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            Arc::new(crate::InMemoryTerminalRepository::new()),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let terminal = registry
            .ensure_terminal(ensure_request(session_id.clone(), context, shell))
            .unwrap();
        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();

        let admission = registry
            .admit_human_command(&topology(&session_id), &terminal.terminal_id, 0, 1, "pwd")
            .unwrap();
        assert_eq!(admission.command_sequence, 1);
        assert_eq!(
            admission.submission.decode(128).unwrap(),
            b"\x1b[200~pwd\x1b[201~\n"
        );
        assert_eq!(
            registry
                .admit_human_command(
                    &topology(&session_id),
                    &terminal.terminal_id,
                    0,
                    1,
                    "echo queued"
                )
                .unwrap_err()
                .code(),
            ProcessErrorCode::StateConflict
        );
        assert_eq!(
            registry
                .admit_human_command(
                    &TerminalTopologyId::new(caller_id(SessionId::generate().as_str())),
                    &terminal.terminal_id,
                    0,
                    1,
                    "pwd",
                )
                .unwrap_err()
                .code(),
            ProcessErrorCode::ExecutionNotOwned
        );

        let transaction_id = registry.lock().unwrap().terminals[&terminal.terminal_id]
            .pending_human_command
            .as_ref()
            .unwrap()
            .transaction_id
            .as_str()
            .to_owned();
        starter.push_event(
            &terminal.execution_id,
            "command_started",
            &[
                transaction_id,
                "pwd".to_owned(),
                root.to_string_lossy().into_owned(),
            ],
            1,
        );
        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        assert_eq!(
            registry
                .admit_human_command(
                    &topology(&session_id),
                    &terminal.terminal_id,
                    1,
                    1,
                    "echo busy"
                )
                .unwrap_err()
                .code(),
            ProcessErrorCode::StateConflict
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prompt_probe_with_pending_input_never_installs_a_fresh_generation() {
        let starter = Arc::new(FakeStarter::default());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            Arc::new(crate::InMemoryTerminalRepository::new()),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let terminal = registry
            .ensure_terminal(ensure_request(session_id.clone(), context, shell))
            .unwrap();
        {
            let mut integrations = starter.integrations.lock().unwrap();
            let integration = integrations.get_mut(&terminal.execution_id).unwrap();
            integration.packets.clear();
            integration.next_event_sequence = 0;
        }
        starter.push_event(
            &terminal.execution_id,
            "prompt_ready",
            &[
                root.to_string_lossy().into_owned(),
                "-".to_owned(),
                "1".to_owned(),
            ],
            0,
        );

        let batch = registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();

        assert!(matches!(
            batch.events.as_slice(),
            [ShellIntegrationEvent::PromptReady {
                input_pending: true,
                ..
            }]
        ));
        let record = registry.record(&terminal.terminal_id).unwrap();
        assert_eq!(record.prompt_state, TerminalPromptState::Unknown);
        assert_eq!(record.integration_health, ShellIntegrationHealth::Trusted);
        let controls = starter.controls(&terminal.execution_id);
        assert!(controls.iter().any(|fields| {
            fields.get(2).map(String::as_str) == Some("prompt_ready_ack")
                && fields.get(4).map(String::as_str) == Some("-")
        }));
        assert_eq!(
            registry
                .admit_human_command(&topology(&session_id), &terminal.terminal_id, 0, 1, "pwd")
                .unwrap_err()
                .code(),
            ProcessErrorCode::StateConflict
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn raw_foreground_exit_reprobes_before_restoring_trusted_prompt() {
        let starter = Arc::new(FakeStarter::default());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            Arc::new(crate::InMemoryTerminalRepository::new()),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let terminal = registry
            .ensure_terminal(ensure_request(session_id.clone(), context, shell))
            .unwrap();
        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        let lease = starter
            .attach(&terminal.execution_id, ExecutionRequestId::generate())
            .unwrap();

        registry
            .write_raw_human_input_if_managed(
                &terminal.execution_id,
                lease.clone(),
                ProcessBytes::from_bytes(b"foreground-app\n"),
                false,
            )
            .unwrap();
        starter.push_event(
            &terminal.execution_id,
            "command_started",
            &[
                "-".to_owned(),
                "foreground-app".to_owned(),
                root.to_string_lossy().into_owned(),
            ],
            0,
        );
        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        starter.set_foreground_process_group(&terminal.execution_id, Some(4242));
        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        registry
            .write_raw_human_input_if_managed(
                &terminal.execution_id,
                lease,
                ProcessBytes::from_bytes(b"application-input"),
                false,
            )
            .unwrap();
        starter.set_foreground_process_group(&terminal.execution_id, None);
        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        starter.push_event(
            &terminal.execution_id,
            "command_finished",
            &[
                "-".to_owned(),
                "code".to_owned(),
                "0".to_owned(),
                root.to_string_lossy().into_owned(),
            ],
            0,
        );
        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();

        starter.push_event(
            &terminal.execution_id,
            "prompt_ready",
            &[
                root.to_string_lossy().into_owned(),
                "0".to_owned(),
                "0".to_owned(),
            ],
            0,
        );
        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        assert_eq!(
            registry.record(&terminal.terminal_id).unwrap().prompt_state,
            TerminalPromptState::Unknown
        );

        starter.push_event(
            &terminal.execution_id,
            "prompt_ready",
            &[
                root.to_string_lossy().into_owned(),
                "0".to_owned(),
                "0".to_owned(),
            ],
            0,
        );
        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        let record = registry.record(&terminal.terminal_id).unwrap();
        let TerminalPromptState::Ready {
            sequence: prompt_generation,
            ..
        } = record.prompt_state
        else {
            panic!("bounded clean re-probe did not restore a trusted prompt");
        };
        registry
            .admit_human_command(
                &topology(&session_id),
                &terminal.terminal_id,
                1,
                prompt_generation,
                "pwd",
            )
            .unwrap();
        let controls = starter.controls(&terminal.execution_id);
        assert!(controls.iter().any(|fields| {
            fields.get(2).map(String::as_str) == Some("prompt_ready_ack")
                && fields.get(4).map(String::as_str) == Some("-")
        }));
        assert!(controls.iter().any(|fields| {
            fields.get(2).map(String::as_str) == Some("prompt_ready_ack")
                && fields.get(4).is_some_and(|generation| generation != "-")
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn raw_write_racing_prompt_reprobe_never_installs_generation() {
        let starter = Arc::new(FakeStarter::default());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            Arc::new(crate::InMemoryTerminalRepository::new()),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let terminal = registry
            .ensure_terminal(ensure_request(session_id.clone(), context, shell))
            .unwrap();
        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        let lease = starter
            .attach(&terminal.execution_id, ExecutionRequestId::generate())
            .unwrap();

        registry
            .write_raw_human_input_if_managed(
                &terminal.execution_id,
                lease.clone(),
                ProcessBytes::from_bytes(b"first"),
                false,
            )
            .unwrap();
        starter.push_event(
            &terminal.execution_id,
            "prompt_ready",
            &[
                root.to_string_lossy().into_owned(),
                "0".to_owned(),
                "0".to_owned(),
            ],
            0,
        );
        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();

        registry
            .write_raw_human_input_if_managed(
                &terminal.execution_id,
                lease,
                ProcessBytes::from_bytes(b"raced"),
                false,
            )
            .unwrap();
        starter.push_event(
            &terminal.execution_id,
            "prompt_ready",
            &[
                root.to_string_lossy().into_owned(),
                "0".to_owned(),
                "0".to_owned(),
            ],
            0,
        );
        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();

        assert_eq!(
            registry.record(&terminal.terminal_id).unwrap().prompt_state,
            TerminalPromptState::Unknown
        );
        assert_eq!(
            registry
                .admit_human_command(&topology(&session_id), &terminal.terminal_id, 0, 3, "pwd")
                .unwrap_err()
                .code(),
            ProcessErrorCode::StateConflict
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mismatched_typed_start_gets_no_ack_and_kills_the_terminal() {
        let starter = Arc::new(FakeStarter::default());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            Arc::new(crate::InMemoryTerminalRepository::new()),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let terminal = registry
            .ensure_terminal(ensure_request(session_id.clone(), context, shell))
            .unwrap();
        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        registry
            .admit_human_command(&topology(&session_id), &terminal.terminal_id, 0, 1, "pwd")
            .unwrap();
        let transaction_id = registry.lock().unwrap().terminals[&terminal.terminal_id]
            .pending_human_command
            .as_ref()
            .unwrap()
            .transaction_id
            .as_str()
            .to_owned();
        starter.push_event(
            &terminal.execution_id,
            "command_started",
            &[
                transaction_id,
                "printf wrong".to_owned(),
                root.to_string_lossy().into_owned(),
            ],
            1,
        );

        let batch = registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();

        assert_eq!(batch.notice.unwrap().code, "shell_integration_degraded");
        assert_eq!(starter.kills.load(Ordering::Relaxed), 1);
        assert_eq!(
            registry.record(&terminal.terminal_id).unwrap().state,
            TerminalState::Stopping
        );
        assert!(
            !starter
                .controls(&terminal.execution_id)
                .iter()
                .any(|fields| {
                    fields.get(2).map(String::as_str) == Some("command_boundary_ack")
                })
        );
        assert!(starter.written_commands().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn integration_loss_while_armed_kills_before_typed_execution() {
        let starter = Arc::new(FakeStarter::default());
        let registry = TerminalRegistry::with_starter(
            starter.clone(),
            Arc::new(NoSecrets),
            Arc::new(crate::InMemoryTerminalRepository::new()),
        )
        .unwrap();
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let terminal = registry
            .ensure_terminal(ensure_request(session_id.clone(), context, shell))
            .unwrap();
        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        registry
            .admit_human_command(&topology(&session_id), &terminal.terminal_id, 0, 1, "pwd")
            .unwrap();
        starter.close_integration(&terminal.execution_id);

        let batch = registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();

        assert_eq!(batch.notice.unwrap().code, "shell_integration_degraded");
        assert_eq!(starter.kills.load(Ordering::Relaxed), 1);
        assert!(starter.written_commands().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_raw_and_typed_human_input_have_exactly_one_writer() {
        let starter = Arc::new(FakeStarter::default());
        let registry = Arc::new(
            TerminalRegistry::with_starter(
                starter.clone(),
                Arc::new(NoSecrets),
                Arc::new(crate::InMemoryTerminalRepository::new()),
            )
            .unwrap(),
        );
        let (root, context, shell) = fixture();
        let session_id = SessionId::generate();
        let terminal = registry
            .ensure_terminal(ensure_request(session_id.clone(), context, shell))
            .unwrap();
        registry
            .poll_private_integration(&terminal.terminal_id, 4096)
            .unwrap();
        let lease = starter
            .attach(&terminal.execution_id, ExecutionRequestId::generate())
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));

        let raw_registry = Arc::clone(&registry);
        let raw_execution_id = terminal.execution_id.clone();
        let raw_lease = lease.clone();
        let raw_barrier = Arc::clone(&barrier);
        let raw = std::thread::spawn(move || {
            raw_barrier.wait();
            raw_registry.write_raw_human_input_if_managed(
                &raw_execution_id,
                raw_lease,
                ProcessBytes::from_bytes(b"raw-winner\n"),
                false,
            )
        });

        let typed_registry = Arc::clone(&registry);
        let typed_session_id = session_id.clone();
        let typed_terminal_id = terminal.terminal_id.clone();
        let typed_execution_id = terminal.execution_id.clone();
        let typed_lease = lease;
        let typed_barrier = Arc::clone(&barrier);
        let typed = std::thread::spawn(move || {
            typed_barrier.wait();
            let admission = typed_registry.admit_human_command(
                &topology(&typed_session_id),
                &typed_terminal_id,
                0,
                1,
                "typed-winner",
            )?;
            typed_registry.write_admitted_human_command(
                &typed_terminal_id,
                &typed_execution_id,
                admission.command_sequence,
                typed_lease,
                admission.submission,
            )
        });

        barrier.wait();
        let raw = raw.join().unwrap();
        let typed = typed.join().unwrap();
        assert_eq!(usize::from(raw.is_ok()) + usize::from(typed.is_ok()), 1);
        if let Err(error) = &raw {
            assert_eq!(error.code(), ProcessErrorCode::StateConflict);
        }
        if let Err(error) = &typed {
            assert_eq!(error.code(), ProcessErrorCode::StateConflict);
        }

        let writes = starter.written_commands();
        assert_eq!(writes.len(), 1, "losing input path wrote physical bytes");
        assert!(
            writes[0] == b"raw-winner\n" || writes[0] == b"\x1b[200~typed-winner\x1b[201~\n",
            "winning transaction was altered or concatenated: {:?}",
            writes[0]
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
