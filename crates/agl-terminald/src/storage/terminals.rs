use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agl_exec::{CallerOwnerId, ExecutionId, LifecycleScopeId};
use agl_exec::{ExecutionProfile, ProcessError, ProcessErrorCode, ShellProfileSnapshot};
use agl_terminal::{
    AdmittedShellKind, AdmittedShellProfile, ShellIntegrationHealth, StoredTerminalRecord,
    TerminalId, TerminalOwner, TerminalPromptState, TerminalRecord, TerminalRepository,
    TerminalReservation, TerminalState, TerminalTopologyId, validate_terminal_replacement,
    validate_terminal_reservation,
};
use rusqlite::{ErrorCode, OptionalExtension, Row, params};

use super::{Result as StoreResult, StoreError, TerminalStore};

const TERMINAL_COLUMNS: &str = "terminal_id, execution_id, topology_id, owner_json,
    lifecycle_scope_id, profile,
    workspace_root, shell_kind, shell_program, shell_argv_json, shell_login_argv_json,
    shell_environment_names_json, shell_executable_digest, shell_config_digest,
    environment_digest, command_sequence, prompt_kind, prompt_sequence, prompt_last_exit,
    prompt_process_group, integration_health, cwd, state, slot_key, fingerprint, active_slot";

/// SQLite-backed durable owner for persistent terminal identity and metadata.
///
/// The repository stores path fields as exact platform bytes. It never stores
/// environment values, integration credentials, command text, PTY output,
/// writer leases, process IDs, or private spool locations.
pub struct SqliteTerminalRepository {
    store: Mutex<TerminalStore>,
}

impl SqliteTerminalRepository {
    pub fn open_at(root: impl AsRef<Path>) -> StoreResult<Self> {
        Ok(Self::from_store(TerminalStore::open_at(root)?))
    }

    fn from_store(store: TerminalStore) -> Self {
        Self {
            store: Mutex::new(store),
        }
    }

    pub fn record(&self, terminal_id: &TerminalId) -> agl_exec::Result<StoredTerminalRecord> {
        self.with_store(|store| {
            let raw =
                select_terminal(store.connection(), terminal_id.as_str())?.ok_or_else(|| {
                    StoreError::NotFound {
                        resource: format!("terminal {terminal_id}"),
                    }
                })?;
            decode_terminal(raw)
        })
    }

    fn with_store<T>(
        &self,
        operation: impl FnOnce(&TerminalStore) -> StoreResult<T>,
    ) -> agl_exec::Result<T> {
        let store = self.store.lock().map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                "terminal repository lock is poisoned",
            )
        })?;
        operation(&store).map_err(terminal_store_error)
    }
}

impl TerminalRepository for SqliteTerminalRepository {
    fn reserve(&self, record: &StoredTerminalRecord) -> agl_exec::Result<TerminalReservation> {
        validate_terminal_reservation(record)?;
        self.with_store(|store| {
            store.transaction(|tx| {
                if let Some(raw) = select_active_slot(tx, &record.slot_key)? {
                    let existing = decode_terminal(raw)?;
                    if existing.fingerprint != record.fingerprint {
                        return Err(StoreError::TransitionRejected {
                            resource: format!("terminal slot {}", record.slot_key),
                            from: existing.fingerprint,
                            to: record.fingerprint.clone(),
                        });
                    }
                    return Ok(TerminalReservation::Existing(Box::new(existing)));
                }

                let encoded = encode_terminal(record)?;
                insert_terminal(tx, &encoded)?;
                Ok(TerminalReservation::Created)
            })
        })
    }

    fn replace(&self, record: &StoredTerminalRecord) -> agl_exec::Result<()> {
        record.validate()?;
        self.with_store(|store| {
            store.transaction(|tx| {
                let previous = select_terminal(tx, record.record.terminal_id.as_str())?
                    .ok_or_else(|| StoreError::NotFound {
                        resource: format!("terminal {}", record.record.terminal_id),
                    })
                    .and_then(decode_terminal)?;
                validate_terminal_replacement(&previous, record).map_err(|error| {
                    StoreError::TransitionRejected {
                        resource: format!("terminal {}", record.record.terminal_id),
                        from: format!("{:?}", previous.record.state),
                        to: error.to_string(),
                    }
                })?;

                let encoded = encode_terminal(record)?;
                let changed = update_terminal(tx, &encoded)?;
                if changed != 1 {
                    return Err(StoreError::NotFound {
                        resource: format!("terminal {}", record.record.terminal_id),
                    });
                }
                Ok(())
            })
        })
    }

    fn recover_for_new_owner(&self) -> agl_exec::Result<Vec<StoredTerminalRecord>> {
        self.with_store(|store| {
            store.transaction(|tx| {
                tx.execute(
                    "UPDATE terminal_sessions
                     SET state = 'outcome_unknown', prompt_kind = 'degraded',
                         prompt_sequence = NULL, prompt_last_exit = NULL,
                         prompt_process_group = NULL, integration_health = 'degraded'
                     WHERE state IN ('starting', 'running', 'stopping')",
                    [],
                )?;
                select_all_terminals(tx)
            })
        })
    }
}

struct EncodedTerminal {
    terminal_id: String,
    execution_id: String,
    topology_id: String,
    owner_json: String,
    lifecycle_scope_id: String,
    profile: &'static str,
    workspace_root: Vec<u8>,
    shell_kind: &'static str,
    shell_program: Vec<u8>,
    shell_argv_json: String,
    shell_login_argv_json: Option<String>,
    shell_environment_names_json: String,
    shell_executable_digest: String,
    shell_config_digest: String,
    environment_digest: String,
    command_sequence: i64,
    prompt_kind: &'static str,
    prompt_sequence: Option<i64>,
    prompt_last_exit: Option<i32>,
    prompt_process_group: Option<i32>,
    integration_health: &'static str,
    cwd: Vec<u8>,
    state: &'static str,
    slot_key: String,
    fingerprint: String,
    active_slot: bool,
}

fn encode_terminal(record: &StoredTerminalRecord) -> StoreResult<EncodedTerminal> {
    let (prompt_kind, prompt_sequence, prompt_last_exit, prompt_process_group) =
        prompt_columns(&record.record.prompt_state)?;
    Ok(EncodedTerminal {
        terminal_id: record.record.terminal_id.as_str().to_owned(),
        execution_id: record.record.execution_id.as_str().to_owned(),
        topology_id: record.record.topology_id.as_str().to_owned(),
        owner_json: serde_json::to_string(&record.record.owner)?,
        lifecycle_scope_id: record.record.lifecycle_scope_id.as_str().to_owned(),
        profile: execution_profile(record.record.profile),
        workspace_root: path_bytes(&record.record.workspace_root)?,
        shell_kind: shell_kind(record.record.shell_profile.kind),
        shell_program: path_bytes(&record.record.shell_profile.snapshot.program)?,
        shell_argv_json: serde_json::to_string(&record.record.shell_profile.snapshot.command_args)?,
        shell_login_argv_json: record
            .record
            .shell_profile
            .snapshot
            .login_command_args
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?,
        shell_environment_names_json: serde_json::to_string(
            &record.record.shell_profile.snapshot.environment_names,
        )?,
        shell_executable_digest: record
            .record
            .shell_profile
            .snapshot
            .executable_digest
            .clone(),
        shell_config_digest: record.record.shell_profile.snapshot.config_digest.clone(),
        environment_digest: record.record.environment_digest.as_str().to_owned(),
        command_sequence: encode_u64(
            record.record.command_sequence,
            "terminal_sessions.command_sequence",
        )?,
        prompt_kind,
        prompt_sequence,
        prompt_last_exit,
        prompt_process_group,
        integration_health: integration_health(record.record.integration_health),
        cwd: path_bytes(&record.record.cwd)?,
        state: terminal_state(record.record.state),
        slot_key: record.slot_key.clone(),
        fingerprint: record.fingerprint.clone(),
        active_slot: record.active_slot,
    })
}

fn insert_terminal(tx: &rusqlite::Transaction<'_>, record: &EncodedTerminal) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO terminal_sessions
         (terminal_id, execution_id, topology_id, owner_json, lifecycle_scope_id, profile, workspace_root,
          shell_kind, shell_program, shell_argv_json, shell_login_argv_json,
          shell_environment_names_json, shell_executable_digest, shell_config_digest,
          environment_digest, command_sequence, prompt_kind, prompt_sequence,
          prompt_last_exit, prompt_process_group, integration_health, cwd, state, slot_key,
          fingerprint, active_slot)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
        terminal_params(record),
    )?;
    Ok(())
}

fn update_terminal(tx: &rusqlite::Transaction<'_>, record: &EncodedTerminal) -> StoreResult<usize> {
    Ok(tx.execute(
        "UPDATE terminal_sessions
         SET execution_id = ?2, topology_id = ?3, owner_json = ?4, lifecycle_scope_id = ?5,
             profile = ?6, workspace_root = ?7, shell_kind = ?8, shell_program = ?9,
             shell_argv_json = ?10, shell_login_argv_json = ?11,
             shell_environment_names_json = ?12, shell_executable_digest = ?13,
             shell_config_digest = ?14, environment_digest = ?15, command_sequence = ?16,
             prompt_kind = ?17, prompt_sequence = ?18, prompt_last_exit = ?19,
             prompt_process_group = ?20, integration_health = ?21, cwd = ?22, state = ?23,
             slot_key = ?24, fingerprint = ?25, active_slot = ?26
         WHERE terminal_id = ?1",
        terminal_params(record),
    )?)
}

fn terminal_params(record: &EncodedTerminal) -> [&dyn rusqlite::ToSql; 26] {
    [
        &record.terminal_id,
        &record.execution_id,
        &record.topology_id,
        &record.owner_json,
        &record.lifecycle_scope_id,
        &record.profile,
        &record.workspace_root,
        &record.shell_kind,
        &record.shell_program,
        &record.shell_argv_json,
        &record.shell_login_argv_json,
        &record.shell_environment_names_json,
        &record.shell_executable_digest,
        &record.shell_config_digest,
        &record.environment_digest,
        &record.command_sequence,
        &record.prompt_kind,
        &record.prompt_sequence,
        &record.prompt_last_exit,
        &record.prompt_process_group,
        &record.integration_health,
        &record.cwd,
        &record.state,
        &record.slot_key,
        &record.fingerprint,
        &record.active_slot,
    ]
}

struct RawTerminal {
    terminal_id: String,
    execution_id: String,
    topology_id: String,
    owner_json: String,
    lifecycle_scope_id: String,
    profile: String,
    workspace_root: Vec<u8>,
    shell_kind: String,
    shell_program: Vec<u8>,
    shell_argv_json: String,
    shell_login_argv_json: Option<String>,
    shell_environment_names_json: String,
    shell_executable_digest: String,
    shell_config_digest: String,
    environment_digest: String,
    command_sequence: i64,
    prompt_kind: String,
    prompt_sequence: Option<i64>,
    prompt_last_exit: Option<i32>,
    prompt_process_group: Option<i32>,
    integration_health: String,
    cwd: Vec<u8>,
    state: String,
    slot_key: String,
    fingerprint: String,
    active_slot: bool,
}

fn read_terminal(row: &Row<'_>) -> rusqlite::Result<RawTerminal> {
    Ok(RawTerminal {
        terminal_id: row.get(0)?,
        execution_id: row.get(1)?,
        topology_id: row.get(2)?,
        owner_json: row.get(3)?,
        lifecycle_scope_id: row.get(4)?,
        profile: row.get(5)?,
        workspace_root: row.get(6)?,
        shell_kind: row.get(7)?,
        shell_program: row.get(8)?,
        shell_argv_json: row.get(9)?,
        shell_login_argv_json: row.get(10)?,
        shell_environment_names_json: row.get(11)?,
        shell_executable_digest: row.get(12)?,
        shell_config_digest: row.get(13)?,
        environment_digest: row.get(14)?,
        command_sequence: row.get(15)?,
        prompt_kind: row.get(16)?,
        prompt_sequence: row.get(17)?,
        prompt_last_exit: row.get(18)?,
        prompt_process_group: row.get(19)?,
        integration_health: row.get(20)?,
        cwd: row.get(21)?,
        state: row.get(22)?,
        slot_key: row.get(23)?,
        fingerprint: row.get(24)?,
        active_slot: row.get(25)?,
    })
}

fn select_terminal(
    connection: &rusqlite::Connection,
    terminal_id: &str,
) -> StoreResult<Option<RawTerminal>> {
    let sql = format!("SELECT {TERMINAL_COLUMNS} FROM terminal_sessions WHERE terminal_id = ?1");
    Ok(connection
        .query_row(&sql, params![terminal_id], read_terminal)
        .optional()?)
}

fn select_active_slot(
    connection: &rusqlite::Connection,
    slot_key: &str,
) -> StoreResult<Option<RawTerminal>> {
    let sql = format!(
        "SELECT {TERMINAL_COLUMNS} FROM terminal_sessions WHERE slot_key = ?1 AND active_slot = 1"
    );
    Ok(connection
        .query_row(&sql, params![slot_key], read_terminal)
        .optional()?)
}

fn select_all_terminals(
    connection: &rusqlite::Connection,
) -> StoreResult<Vec<StoredTerminalRecord>> {
    let sql = format!("SELECT {TERMINAL_COLUMNS} FROM terminal_sessions ORDER BY terminal_id");
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([], read_terminal)?;
    let mut records = Vec::new();
    for row in rows {
        records.push(decode_terminal(row?)?);
    }
    Ok(records)
}

fn decode_terminal(raw: RawTerminal) -> StoreResult<StoredTerminalRecord> {
    let terminal_id = parse_terminal_id(&raw.terminal_id, "terminal_sessions.terminal_id")?;
    let topology_id =
        TerminalTopologyId::new(CallerOwnerId::new(raw.topology_id.clone()).map_err(|_| {
            invalid_store_value(
                "terminal_sessions.topology_id",
                &raw.topology_id,
                "invalid opaque topology ID",
            )
        })?);
    let owner: TerminalOwner = serde_json::from_str(&raw.owner_json)?;
    let lifecycle_scope_id =
        LifecycleScopeId::new(raw.lifecycle_scope_id.clone()).map_err(|_| {
            invalid_store_value(
                "terminal_sessions.lifecycle_scope_id",
                &raw.lifecycle_scope_id,
                "invalid lifecycle scope ID",
            )
        })?;
    let prompt_state = decode_prompt(&raw)?;
    let shell_profile = AdmittedShellProfile {
        kind: parse_shell_kind(&raw.shell_kind)?,
        snapshot: ShellProfileSnapshot {
            program: path_from_bytes(raw.shell_program)?,
            command_args: serde_json::from_str(&raw.shell_argv_json)?,
            login_command_args: raw
                .shell_login_argv_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
            environment_names: serde_json::from_str(&raw.shell_environment_names_json)?,
            executable_digest: raw.shell_executable_digest,
            config_digest: raw.shell_config_digest,
        },
    };
    let environment_digest =
        serde_json::from_value(serde_json::Value::String(raw.environment_digest))?;
    let stored = StoredTerminalRecord {
        record: TerminalRecord {
            terminal_id,
            execution_id: parse_execution_id(&raw.execution_id, "terminal_sessions.execution_id")?,
            topology_id,
            owner,
            lifecycle_scope_id,
            profile: parse_execution_profile(&raw.profile)?,
            workspace_root: path_from_bytes(raw.workspace_root)?,
            shell_profile,
            environment_digest,
            command_sequence: decode_u64(
                raw.command_sequence,
                "terminal_sessions.command_sequence",
            )?,
            prompt_state,
            integration_health: parse_integration_health(&raw.integration_health)?,
            cwd: path_from_bytes(raw.cwd)?,
            state: parse_terminal_state(&raw.state)?,
        },
        slot_key: raw.slot_key,
        fingerprint: raw.fingerprint,
        active_slot: raw.active_slot,
    };
    stored.validate().map_err(|_| StoreError::InvalidValue {
        field: "terminal_sessions",
        value: stored.record.terminal_id.as_str().to_owned(),
        reason: "stored terminal violates persistence-neutral terminal invariants",
    })?;
    Ok(stored)
}

fn execution_profile(profile: ExecutionProfile) -> &'static str {
    match profile {
        ExecutionProfile::Workspace => "workspace",
        ExecutionProfile::Host => "host",
    }
}

fn parse_execution_profile(value: &str) -> StoreResult<ExecutionProfile> {
    match value {
        "workspace" => Ok(ExecutionProfile::Workspace),
        "host" => Ok(ExecutionProfile::Host),
        _ => Err(invalid_store_value(
            "terminal_sessions.profile",
            value,
            "unknown terminal execution profile",
        )),
    }
}

fn shell_kind(kind: AdmittedShellKind) -> &'static str {
    match kind {
        AdmittedShellKind::Bash => "bash",
        AdmittedShellKind::Zsh => "zsh",
    }
}

fn parse_shell_kind(value: &str) -> StoreResult<AdmittedShellKind> {
    match value {
        "bash" => Ok(AdmittedShellKind::Bash),
        "zsh" => Ok(AdmittedShellKind::Zsh),
        _ => Err(invalid_store_value(
            "terminal_sessions.shell_kind",
            value,
            "unknown admitted shell kind",
        )),
    }
}

fn terminal_state(state: TerminalState) -> &'static str {
    match state {
        TerminalState::Reserved => "reserved",
        TerminalState::Starting => "starting",
        TerminalState::Running => "running",
        TerminalState::Stopping => "stopping",
        TerminalState::Exited => "exited",
        TerminalState::Failed => "failed",
        TerminalState::OutcomeUnknown => "outcome_unknown",
    }
}

fn parse_terminal_state(value: &str) -> StoreResult<TerminalState> {
    match value {
        "reserved" => Ok(TerminalState::Reserved),
        "starting" => Ok(TerminalState::Starting),
        "running" => Ok(TerminalState::Running),
        "stopping" => Ok(TerminalState::Stopping),
        "exited" => Ok(TerminalState::Exited),
        "failed" => Ok(TerminalState::Failed),
        "outcome_unknown" => Ok(TerminalState::OutcomeUnknown),
        _ => Err(invalid_store_value(
            "terminal_sessions.state",
            value,
            "unknown terminal state",
        )),
    }
}

fn integration_health(health: ShellIntegrationHealth) -> &'static str {
    match health {
        ShellIntegrationHealth::AwaitingFirstPrompt => "awaiting_first_prompt",
        ShellIntegrationHealth::Trusted => "trusted",
        ShellIntegrationHealth::Degraded => "degraded",
    }
}

fn parse_integration_health(value: &str) -> StoreResult<ShellIntegrationHealth> {
    match value {
        "awaiting_first_prompt" => Ok(ShellIntegrationHealth::AwaitingFirstPrompt),
        "trusted" => Ok(ShellIntegrationHealth::Trusted),
        "degraded" => Ok(ShellIntegrationHealth::Degraded),
        _ => Err(invalid_store_value(
            "terminal_sessions.integration_health",
            value,
            "unknown shell integration health",
        )),
    }
}

type PromptColumns = (&'static str, Option<i64>, Option<i32>, Option<i32>);

fn prompt_columns(prompt: &TerminalPromptState) -> StoreResult<PromptColumns> {
    match prompt {
        TerminalPromptState::Unknown => Ok(("unknown", None, None, None)),
        TerminalPromptState::Ready {
            sequence,
            last_exit,
        } => Ok((
            "ready",
            Some(encode_u64(*sequence, "terminal_sessions.prompt_sequence")?),
            *last_exit,
            None,
        )),
        TerminalPromptState::CommandRunning { sequence } => Ok((
            "command_running",
            Some(encode_u64(*sequence, "terminal_sessions.prompt_sequence")?),
            None,
            None,
        )),
        TerminalPromptState::ForegroundProgram {
            sequence,
            process_group,
        } => Ok((
            "foreground_program",
            Some(encode_u64(*sequence, "terminal_sessions.prompt_sequence")?),
            None,
            Some(*process_group),
        )),
        TerminalPromptState::Degraded => Ok(("degraded", None, None, None)),
    }
}

fn decode_prompt(raw: &RawTerminal) -> StoreResult<TerminalPromptState> {
    match raw.prompt_kind.as_str() {
        "unknown" => Ok(TerminalPromptState::Unknown),
        "ready" => Ok(TerminalPromptState::Ready {
            sequence: decode_required_prompt_sequence(raw)?,
            last_exit: raw.prompt_last_exit,
        }),
        "command_running" => Ok(TerminalPromptState::CommandRunning {
            sequence: decode_required_prompt_sequence(raw)?,
        }),
        "foreground_program" => Ok(TerminalPromptState::ForegroundProgram {
            sequence: decode_required_prompt_sequence(raw)?,
            process_group: raw.prompt_process_group.ok_or_else(|| {
                invalid_store_value(
                    "terminal_sessions.prompt_process_group",
                    "null",
                    "foreground prompt state requires a process group",
                )
            })?,
        }),
        "degraded" => Ok(TerminalPromptState::Degraded),
        value => Err(invalid_store_value(
            "terminal_sessions.prompt_kind",
            value,
            "unknown terminal prompt state",
        )),
    }
}

fn decode_required_prompt_sequence(raw: &RawTerminal) -> StoreResult<u64> {
    let value = raw.prompt_sequence.ok_or_else(|| {
        invalid_store_value(
            "terminal_sessions.prompt_sequence",
            "null",
            "terminal prompt state requires a sequence",
        )
    })?;
    decode_u64(value, "terminal_sessions.prompt_sequence")
}

fn encode_u64(value: u64, field: &'static str) -> StoreResult<i64> {
    i64::try_from(value).map_err(|_| {
        invalid_store_value(field, "out_of_range", "value exceeds SQLite INTEGER range")
    })
}

fn decode_u64(value: i64, field: &'static str) -> StoreResult<u64> {
    u64::try_from(value).map_err(|_| invalid_store_value(field, value, "value must be nonnegative"))
}

fn parse_terminal_id(value: &str, field: &'static str) -> StoreResult<TerminalId> {
    TerminalId::parse(value)
        .map_err(|_| invalid_store_value(field, value, "invalid terminal session ID"))
}

fn parse_execution_id(value: &str, field: &'static str) -> StoreResult<ExecutionId> {
    ExecutionId::parse(value).map_err(|_| invalid_store_value(field, value, "invalid execution ID"))
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> StoreResult<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt as _;

    Ok(path.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> StoreResult<Vec<u8>> {
    path.as_os_str()
        .to_str()
        .map(|value| value.as_bytes().to_vec())
        .ok_or_else(|| {
            invalid_store_value(
                "terminal_sessions.path",
                "non_unicode",
                "exact non-Unicode path persistence is supported only on Unix",
            )
        })
}

#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> StoreResult<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;

    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> StoreResult<PathBuf> {
    String::from_utf8(bytes).map(PathBuf::from).map_err(|_| {
        invalid_store_value(
            "terminal_sessions.path",
            "non_unicode",
            "stored path is not representable exactly on this platform",
        )
    })
}

fn invalid_store_value(
    field: &'static str,
    value: impl ToString,
    reason: &'static str,
) -> StoreError {
    StoreError::InvalidValue {
        field,
        value: value.to_string(),
        reason,
    }
}

fn terminal_store_error(error: StoreError) -> ProcessError {
    let code = match &error {
        StoreError::NotFound { .. } => ProcessErrorCode::ExecutionNotFound,
        StoreError::TransitionRejected { .. } | StoreError::LeaseLost { .. } => {
            ProcessErrorCode::StateConflict
        }
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
            if code.code == ErrorCode::ConstraintViolation =>
        {
            ProcessErrorCode::StateConflict
        }
        _ => ProcessErrorCode::StoreCorrupt,
    };
    ProcessError::new(code, error.to_string())
}
