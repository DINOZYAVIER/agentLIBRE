use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::sync::Mutex;

use agl_exec::ExecutionId;
use agl_terminal::TerminalId;

use super::registry::{TerminalRecord, TerminalState};
use super::shell::{ShellIntegrationHealth, TerminalPromptState};
use crate::{ProcessError, ProcessErrorCode, Result};

/// Persistence-neutral terminal identity and metadata. It deliberately
/// contains no environment values, integration token, command text, PTY
/// bytes, input lease, process ID, or private spool path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredTerminalRecord {
    pub record: TerminalRecord,
    pub slot_key: String,
    pub fingerprint: String,
    pub active_slot: bool,
}

impl StoredTerminalRecord {
    pub fn validate(&self) -> Result<()> {
        if self.slot_key.is_empty()
            || self.slot_key.len() > 512
            || self.slot_key.contains(['\0', '\n', '\r'])
        {
            return Err(invalid_record(
                "terminal topology slot key must be nonempty bounded single-line text",
            ));
        }
        validate_sha256_digest(&self.fingerprint, "terminal fingerprint")?;
        if !self.record.workspace_root.is_absolute() || !self.record.cwd.is_absolute() {
            return Err(invalid_record(
                "stored terminal workspace root and cwd must be absolute",
            ));
        }
        if !is_lexically_normalized(&self.record.workspace_root)
            || !is_lexically_normalized(&self.record.cwd)
            || path_contains_nul(&self.record.workspace_root)
            || path_contains_nul(&self.record.cwd)
        {
            return Err(invalid_record(
                "stored terminal workspace root and cwd must be normalized and contain no NUL",
            ));
        }
        self.record.shell_profile.validate().map_err(|_| {
            invalid_record("stored terminal shell profile violates its admitted contract")
        })?;
        if self.record.owner.is_persistent()
            && self.record.owner.caller().owner_id().as_str() != self.record.topology_id.as_str()
        {
            return Err(invalid_record(
                "persistent terminal owner must match the stored topology",
            ));
        }
        if self.record.owner.is_agent() && self.record.profile != crate::ExecutionProfile::Workspace
        {
            return Err(invalid_record(
                "only a Human terminal may retain the Host profile",
            ));
        }
        if self.record.profile == crate::ExecutionProfile::Workspace
            && !self.record.cwd.starts_with(&self.record.workspace_root)
        {
            return Err(invalid_record(
                "stored Workspace terminal cwd must remain inside its immutable workspace root",
            ));
        }
        validate_sha256_digest(
            self.record.environment_digest.as_str(),
            "terminal environment digest",
        )?;
        validate_prompt_metadata(&self.record)?;
        if self.record.command_sequence > i64::MAX as u64 {
            return Err(invalid_record(
                "terminal command sequence exceeds the durable signed 64-bit range",
            ));
        }
        if self.slot_key != terminal_slot_key(&self.record)? {
            return Err(invalid_record(
                "stored terminal topology slot key is not canonical for its owner and profile",
            ));
        }
        match (self.active_slot, self.record.state) {
            (false, TerminalState::Exited | TerminalState::Failed) => {}
            (false, _) => {
                return Err(invalid_record(
                    "only a terminal with a known final outcome may release its topology slot",
                ));
            }
            (true, TerminalState::Exited | TerminalState::Failed) => {
                return Err(invalid_record(
                    "a terminal with a known final outcome must release its topology slot",
                ));
            }
            (true, _) => {}
        }
        Ok(())
    }
}

/// Derives the only admitted durable topology key for a terminal record.
/// Canonical typed IDs make each colon-delimited form unambiguous.
pub fn terminal_slot_key(record: &TerminalRecord) -> Result<String> {
    if record.owner.previous_owner().is_some()
        && record.profile == crate::ExecutionProfile::Workspace
    {
        return Ok(format!("promoted:workspace:{}", record.terminal_id));
    }
    match (
        record.owner.caller().owner_kind(),
        record.owner.caller().role(),
        record.profile,
    ) {
        (
            agl_exec::CallerOwnerKind::Persistent,
            agl_exec::CallerRole::Human,
            crate::ExecutionProfile::Workspace,
        ) => Ok(format!("human:workspace:{}", record.topology_id.as_str())),
        (
            agl_exec::CallerOwnerKind::Persistent,
            agl_exec::CallerRole::Human,
            crate::ExecutionProfile::Host,
        ) => Ok(format!("human:host:{}", record.topology_id.as_str())),
        (
            agl_exec::CallerOwnerKind::Persistent,
            agl_exec::CallerRole::Agent,
            crate::ExecutionProfile::Workspace,
        ) => Ok(format!(
            "persistent-agent:workspace:{}",
            record.topology_id.as_str()
        )),
        (
            agl_exec::CallerOwnerKind::Ephemeral,
            agl_exec::CallerRole::Agent,
            crate::ExecutionProfile::Workspace,
        ) => Ok(format!(
            "ephemeral-agent:workspace:{}",
            record.owner.caller().owner_id()
        )),
        _ => Err(invalid_record(
            "terminal owner and profile do not map to an admitted topology slot",
        )),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalReservation {
    Created,
    Existing(Box<StoredTerminalRecord>),
}

/// Durable terminal identity owner used by `TerminalRegistry`.
///
/// `reserve` commits `TerminalId -> ExecutionId` before process spawn.
/// `recover_for_new_owner` is called once when a new process owner opens the
/// store and atomically converts every previously-live record to
/// `outcome_unknown`; it never relaunches or claims to reattach a PTY.
/// Persistence port implemented by the terminal service data owner.
pub trait TerminalRepository: Send + Sync {
    fn reserve(&self, record: &StoredTerminalRecord) -> Result<TerminalReservation>;

    fn replace(&self, record: &StoredTerminalRecord) -> Result<()>;

    fn recover_for_new_owner(&self) -> Result<Vec<StoredTerminalRecord>>;
}

#[derive(Default)]
pub struct InMemoryTerminalRepository {
    inner: Mutex<InMemoryTerminalState>,
}

#[derive(Default)]
struct InMemoryTerminalState {
    records: BTreeMap<TerminalId, StoredTerminalRecord>,
    active_slots: BTreeMap<String, TerminalId>,
    executions: BTreeMap<ExecutionId, TerminalId>,
}

impl InMemoryTerminalRepository {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, terminal_id: &TerminalId) -> Result<StoredTerminalRecord> {
        self.lock()?
            .records
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::ExecutionNotFound,
                    format!("terminal {} was not found", terminal_id),
                )
            })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, InMemoryTerminalState>> {
        self.inner.lock().map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                "terminal repository lock is poisoned",
            )
        })
    }
}

impl TerminalRepository for InMemoryTerminalRepository {
    fn reserve(&self, record: &StoredTerminalRecord) -> Result<TerminalReservation> {
        validate_terminal_reservation(record)?;
        let mut state = self.lock()?;
        if let Some(terminal_id) = state.active_slots.get(&record.slot_key) {
            let existing = state.records.get(terminal_id).ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::StoreCorrupt,
                    "terminal slot index refers to a missing durable record",
                )
            })?;
            existing.validate()?;
            if existing.fingerprint != record.fingerprint {
                return Err(ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "terminal topology slot already has different immutable admission metadata",
                ));
            }
            return Ok(TerminalReservation::Existing(Box::new(existing.clone())));
        }
        if state.records.contains_key(&record.record.terminal_id)
            || state.executions.contains_key(&record.record.execution_id)
        {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "terminal or backing execution identity is already reserved",
            ));
        }
        state
            .active_slots
            .insert(record.slot_key.clone(), record.record.terminal_id.clone());
        state.executions.insert(
            record.record.execution_id.clone(),
            record.record.terminal_id.clone(),
        );
        state
            .records
            .insert(record.record.terminal_id.clone(), record.clone());
        Ok(TerminalReservation::Created)
    }

    fn replace(&self, record: &StoredTerminalRecord) -> Result<()> {
        record.validate()?;
        let mut state = self.lock()?;
        let previous = state
            .records
            .get(&record.record.terminal_id)
            .cloned()
            .ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::ExecutionNotFound,
                    format!("terminal {} was not found", record.record.terminal_id),
                )
            })?;
        validate_terminal_replacement(&previous, record)?;
        if record.active_slot
            && let Some(conflict) = state.active_slots.get(&record.slot_key)
            && conflict != &record.record.terminal_id
        {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "terminal replacement conflicts with an occupied topology slot",
            ));
        }
        if previous.active_slot {
            state.active_slots.remove(&previous.slot_key);
        }
        if record.active_slot {
            state
                .active_slots
                .insert(record.slot_key.clone(), record.record.terminal_id.clone());
        }
        state
            .records
            .insert(record.record.terminal_id.clone(), record.clone());
        Ok(())
    }

    fn recover_for_new_owner(&self) -> Result<Vec<StoredTerminalRecord>> {
        let mut state = self.lock()?;
        for stored in state.records.values() {
            stored.validate()?;
        }
        for stored in state.records.values_mut() {
            if stored.record.state.is_live() {
                stored.record.state = TerminalState::OutcomeUnknown;
                stored.record.prompt_state = TerminalPromptState::Degraded;
                stored.record.integration_health = ShellIntegrationHealth::Degraded;
            }
        }
        Ok(state.records.values().cloned().collect())
    }
}

/// Validates a new durable reservation independently of a repository backend.
pub fn validate_terminal_reservation(record: &StoredTerminalRecord) -> Result<()> {
    record.validate()?;
    if !record.active_slot || record.record.state != TerminalState::Starting {
        return Err(invalid_record(
            "new terminal reservation must own an active starting slot",
        ));
    }
    Ok(())
}

/// Validates the mutable subset and monotonic transitions shared by all
/// terminal repository implementations.
pub fn validate_terminal_replacement(
    previous: &StoredTerminalRecord,
    replacement: &StoredTerminalRecord,
) -> Result<()> {
    previous.validate()?;
    replacement.validate()?;
    if previous.record.terminal_id != replacement.record.terminal_id
        || previous.record.execution_id != replacement.record.execution_id
        || previous.record.topology_id != replacement.record.topology_id
        || previous.record.authority_scope != replacement.record.authority_scope
        || previous.record.profile != replacement.record.profile
        || previous.record.workspace_root != replacement.record.workspace_root
        || previous.record.shell_profile != replacement.record.shell_profile
        || previous.record.environment_digest != replacement.record.environment_digest
        || previous.fingerprint != replacement.fingerprint
    {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "terminal replacement changed immutable admission metadata",
        ));
    }
    if replacement.record.command_sequence < previous.record.command_sequence {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "terminal command sequence cannot move backwards",
        ));
    }
    if matches!(
        previous.record.state,
        TerminalState::Exited | TerminalState::Failed | TerminalState::OutcomeUnknown
    ) && previous != replacement
    {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "a terminal with a final or unknown outcome is immutable",
        ));
    }
    let promoted = validate_owner_transition(&previous.record, &replacement.record)?;
    validate_state_transition(previous.record.state, replacement.record.state)?;
    if !previous.active_slot && replacement.active_slot {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "a retired terminal cannot reacquire a topology slot",
        ));
    }
    if previous.slot_key != replacement.slot_key && !promoted {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "terminal topology slot may change only during exact subagent promotion",
        ));
    }
    Ok(())
}

fn validate_owner_transition(
    previous: &TerminalRecord,
    replacement: &TerminalRecord,
) -> Result<bool> {
    if previous.owner == replacement.owner {
        return Ok(false);
    }
    if previous.owner.is_ephemeral()
        && replacement.owner.is_persistent()
        && replacement.owner.is_agent()
        && replacement.owner.previous_owner() == Some(previous.owner.caller())
    {
        Ok(true)
    } else {
        Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "stored terminal owner may change only through exact ephemeral-owner promotion",
        ))
    }
}

fn validate_state_transition(previous: TerminalState, next: TerminalState) -> Result<()> {
    let valid = previous == next
        || matches!(
            (previous, next),
            (TerminalState::Starting, TerminalState::Running)
                | (TerminalState::Starting, TerminalState::Failed)
                | (TerminalState::Starting, TerminalState::OutcomeUnknown)
                | (TerminalState::Running, TerminalState::Stopping)
                | (TerminalState::Running, TerminalState::Exited)
                | (TerminalState::Running, TerminalState::Failed)
                | (TerminalState::Running, TerminalState::OutcomeUnknown)
                | (TerminalState::Stopping, TerminalState::Exited)
                | (TerminalState::Stopping, TerminalState::Failed)
                | (TerminalState::Stopping, TerminalState::OutcomeUnknown)
        );
    if valid {
        Ok(())
    } else {
        Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            "stored terminal state transition is not monotonic",
        ))
    }
}

fn validate_prompt_metadata(record: &TerminalRecord) -> Result<()> {
    match &record.prompt_state {
        TerminalPromptState::Unknown | TerminalPromptState::Degraded => {}
        TerminalPromptState::Ready {
            sequence,
            last_exit,
        } => {
            if *sequence == 0
                || *sequence > i64::MAX as u64
                || last_exit.is_some_and(|exit| !(0..=255).contains(&exit))
            {
                return Err(invalid_record(
                    "trusted ready prompt metadata has an invalid sequence or exit status",
                ));
            }
        }
        TerminalPromptState::CommandRunning { sequence }
            if *sequence == 0 || *sequence > i64::MAX as u64 =>
        {
            return Err(invalid_record(
                "running command prompt metadata requires a positive sequence",
            ));
        }
        TerminalPromptState::ForegroundProgram {
            sequence,
            process_group,
        } if *sequence == 0 || *sequence > i64::MAX as u64 || *process_group <= 0 => {
            return Err(invalid_record(
                "foreground prompt metadata requires a positive sequence and process group",
            ));
        }
        TerminalPromptState::CommandRunning { .. }
        | TerminalPromptState::ForegroundProgram { .. } => {}
    }
    let consistent = match record.integration_health {
        ShellIntegrationHealth::AwaitingFirstPrompt => {
            matches!(record.prompt_state, TerminalPromptState::Unknown)
        }
        ShellIntegrationHealth::Degraded => {
            matches!(record.prompt_state, TerminalPromptState::Degraded)
        }
        ShellIntegrationHealth::Trusted => {
            !matches!(record.prompt_state, TerminalPromptState::Degraded)
        }
    };
    if !consistent {
        return Err(invalid_record(
            "terminal prompt state is inconsistent with shell integration health",
        ));
    }
    Ok(())
}

fn validate_sha256_digest(value: &str, label: &'static str) -> Result<()> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(invalid_record(format!(
            "{label} must use the sha256 scheme"
        )));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_record(format!(
            "{label} must contain 64 lowercase hexadecimal digits"
        )));
    }
    Ok(())
}

fn is_lexically_normalized(path: &Path) -> bool {
    path.components().all(|component| {
        matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::Normal(_)
        )
    })
}

#[cfg(unix)]
fn path_contains_nul(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    path.as_os_str().as_bytes().contains(&0)
}

#[cfg(not(unix))]
fn path_contains_nul(path: &Path) -> bool {
    path.as_os_str()
        .to_str()
        .is_some_and(|value| value.contains('\0'))
}

fn invalid_record(message: impl Into<String>) -> ProcessError {
    ProcessError::new(ProcessErrorCode::StoreCorrupt, message)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::test_support::{RunId, SessionId};
    use agl_exec::{CallerNamespace, CallerOwner, CallerOwnerKind, CallerRole, OpaqueOwnerId};
    use agl_terminal::{TerminalId, TerminalOwner, TerminalTopologyId};

    use super::*;
    use crate::terminal::shell::{AdmittedShellKind, AdmittedShellProfile};
    use crate::{ExecutionProfile, ShellProfileSnapshot};

    fn opaque(value: &str) -> OpaqueOwnerId {
        OpaqueOwnerId::new(value).unwrap()
    }

    fn owner(value: &str, kind: CallerOwnerKind, role: CallerRole) -> CallerOwner {
        CallerOwner::new(
            CallerNamespace::new("agentlibre", 1).unwrap(),
            opaque(value),
            kind,
            role,
        )
    }

    fn stored_terminal() -> StoredTerminalRecord {
        let session_id = SessionId::generate();
        let mut stored = StoredTerminalRecord {
            record: TerminalRecord {
                terminal_id: TerminalId::generate(),
                execution_id: ExecutionId::generate(),
                topology_id: TerminalTopologyId::new(opaque(session_id.as_str())),
                owner: TerminalOwner::new(owner(
                    session_id.as_str(),
                    CallerOwnerKind::Persistent,
                    CallerRole::Human,
                )),
                authority_scope: opaque(RunId::generate().as_str()),
                profile: ExecutionProfile::Workspace,
                workspace_root: PathBuf::from("/workspace"),
                shell_profile: AdmittedShellProfile {
                    kind: AdmittedShellKind::Bash,
                    snapshot: ShellProfileSnapshot {
                        program: PathBuf::from("/bin/bash"),
                        command_args: vec!["-c".to_owned()],
                        login_command_args: None,
                        environment_names: vec!["PATH".to_owned()],
                        executable_digest: "sha256:shell".to_owned(),
                        config_digest: "sha256:config".to_owned(),
                    },
                },
                environment_digest: serde_json::from_str(
                    "\"sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789\"",
                )
                .unwrap(),
                command_sequence: 0,
                prompt_state: TerminalPromptState::Unknown,
                integration_health: ShellIntegrationHealth::AwaitingFirstPrompt,
                cwd: PathBuf::from("/workspace"),
                state: TerminalState::Starting,
            },
            slot_key: String::new(),
            fingerprint: "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_owned(),
            active_slot: true,
        };
        stored.slot_key = terminal_slot_key(&stored.record).unwrap();
        stored
    }

    #[test]
    fn reservation_is_idempotent_and_conflicting_fingerprint_fails_closed() {
        let repository = InMemoryTerminalRepository::new();
        let record = stored_terminal();
        assert_eq!(
            repository.reserve(&record).unwrap(),
            TerminalReservation::Created
        );
        assert_eq!(
            repository.reserve(&record).unwrap(),
            TerminalReservation::Existing(Box::new(record.clone()))
        );

        let mut retry = record.clone();
        retry.record.terminal_id = TerminalId::generate();
        retry.record.execution_id = ExecutionId::generate();
        assert_eq!(
            repository.reserve(&retry).unwrap(),
            TerminalReservation::Existing(Box::new(record.clone()))
        );

        let mut conflicting = record;
        conflicting.record.terminal_id = TerminalId::generate();
        conflicting.record.execution_id = ExecutionId::generate();
        conflicting.fingerprint =
            "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned();
        assert_eq!(
            repository.reserve(&conflicting).unwrap_err().code(),
            ProcessErrorCode::StateConflict
        );
    }

    #[test]
    fn recovery_marks_live_terminal_unknown_without_releasing_its_slot() {
        let repository = InMemoryTerminalRepository::new();
        let mut record = stored_terminal();
        let retry = record.clone();
        repository.reserve(&record).unwrap();
        record.record.state = TerminalState::Running;
        repository.replace(&record).unwrap();

        let recovered = repository.recover_for_new_owner().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].record.state, TerminalState::OutcomeUnknown);
        assert!(recovered[0].active_slot);
        let TerminalReservation::Existing(existing) = repository.reserve(&retry).unwrap() else {
            panic!("restart retry must return the durable identity");
        };
        assert_eq!(existing.record.state, TerminalState::OutcomeUnknown);
    }

    #[test]
    fn only_exact_subagent_promotion_can_change_lifecycle_owner() {
        let repository = InMemoryTerminalRepository::new();
        let mut record = stored_terminal();
        let session_id = record.record.topology_id.as_str().to_owned();
        let owner_run_id = RunId::generate();
        let previous = owner(
            owner_run_id.as_str(),
            CallerOwnerKind::Ephemeral,
            CallerRole::Agent,
        );
        record.record.owner = TerminalOwner::new(previous.clone());
        record.slot_key = terminal_slot_key(&record.record).unwrap();
        repository.reserve(&record).unwrap();
        record.record.state = TerminalState::Running;
        repository.replace(&record).unwrap();

        record.record.owner = TerminalOwner::promoted(
            owner(&session_id, CallerOwnerKind::Persistent, CallerRole::Agent),
            previous,
        );
        record.slot_key = terminal_slot_key(&record.record).unwrap();
        repository.replace(&record).unwrap();
    }

    #[test]
    fn slot_rename_requires_promotion_and_known_outcome_retires_once() {
        let repository = InMemoryTerminalRepository::new();
        let mut record = stored_terminal();
        repository.reserve(&record).unwrap();

        let mut renamed = record.clone();
        renamed.slot_key = "human:workspace:invalid".to_owned();
        assert_eq!(
            repository.replace(&renamed).unwrap_err().code(),
            ProcessErrorCode::StoreCorrupt
        );
        assert_eq!(
            repository.record(&record.record.terminal_id).unwrap(),
            record
        );

        record.record.state = TerminalState::Failed;
        record.active_slot = false;
        repository.replace(&record).unwrap();

        let mut reactivated = record.clone();
        reactivated.active_slot = true;
        assert_eq!(
            repository.replace(&reactivated).unwrap_err().code(),
            ProcessErrorCode::StoreCorrupt
        );

        let mut successor = record.clone();
        successor.record.terminal_id = TerminalId::generate();
        successor.record.execution_id = ExecutionId::generate();
        successor.record.state = TerminalState::Starting;
        successor.active_slot = true;
        assert_eq!(
            repository.reserve(&successor).unwrap(),
            TerminalReservation::Created
        );
    }

    #[test]
    fn duplicate_backing_execution_and_promoted_host_profile_fail_closed() {
        let repository = InMemoryTerminalRepository::new();
        let record = stored_terminal();
        repository.reserve(&record).unwrap();

        let mut duplicate_execution = stored_terminal();
        duplicate_execution.record.execution_id = record.record.execution_id.clone();
        assert_eq!(
            repository.reserve(&duplicate_execution).unwrap_err().code(),
            ProcessErrorCode::StateConflict
        );

        let mut invalid_promoted = stored_terminal();
        invalid_promoted.record.profile = crate::ExecutionProfile::Host;
        let previous = owner(
            RunId::generate().as_str(),
            CallerOwnerKind::Ephemeral,
            CallerRole::Agent,
        );
        invalid_promoted.record.owner = TerminalOwner::promoted(
            owner(
                invalid_promoted.record.topology_id.as_str(),
                CallerOwnerKind::Persistent,
                CallerRole::Agent,
            ),
            previous,
        );
        assert_eq!(
            invalid_promoted.validate().unwrap_err().code(),
            ProcessErrorCode::StoreCorrupt
        );
    }

    #[test]
    fn stored_contract_rejects_noncanonical_scope_digest_and_prompt_metadata() {
        let mut outside_workspace = stored_terminal();
        outside_workspace.record.cwd = PathBuf::from("/outside");
        assert_eq!(
            outside_workspace.validate().unwrap_err().code(),
            ProcessErrorCode::StoreCorrupt
        );

        let mut malformed_digest = stored_terminal();
        malformed_digest.record.environment_digest =
            serde_json::from_str("\"sha256:not-a-digest\"").unwrap();
        assert_eq!(
            malformed_digest.validate().unwrap_err().code(),
            ProcessErrorCode::StoreCorrupt
        );

        let mut malformed_prompt = stored_terminal();
        malformed_prompt.record.integration_health = ShellIntegrationHealth::Trusted;
        malformed_prompt.record.prompt_state = TerminalPromptState::Ready {
            sequence: 0,
            last_exit: Some(256),
        };
        assert_eq!(
            malformed_prompt.validate().unwrap_err().code(),
            ProcessErrorCode::StoreCorrupt
        );

        let mut inconsistent_health = stored_terminal();
        inconsistent_health.record.prompt_state = TerminalPromptState::Degraded;
        assert_eq!(
            inconsistent_health.validate().unwrap_err().code(),
            ProcessErrorCode::StoreCorrupt
        );

        let mut host = stored_terminal();
        host.record.profile = crate::ExecutionProfile::Host;
        host.record.cwd = PathBuf::from("/outside");
        host.slot_key = terminal_slot_key(&host.record).unwrap();
        host.validate().unwrap();
    }
}
