use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agl_exec::ExecutionId;
use agl_ids::{RunId, SessionId};
use agl_process::{
    AdmittedShellKind, AdmittedShellProfile, ExecutionProfile, ProcessError, ProcessErrorCode,
    ShellIntegrationHealth, ShellProfileSnapshot, StoredTerminalRecord, TerminalOwner,
    TerminalPromptState, TerminalRecord, TerminalRepository, TerminalReservation, TerminalState,
    validate_terminal_replacement, validate_terminal_reservation,
};
use agl_terminal::TerminalId;
use rusqlite::{ErrorCode, OptionalExtension, Row, params};

use crate::{AglStore, Result as StoreResult, StoreError};

const TERMINAL_COLUMNS: &str = "terminal_id, execution_id, session_id, owner_kind,
    owner_session_id, owner_root_run_id, owner_run_id, previous_owner_run_id, profile,
    workspace_root, shell_kind, shell_program, shell_argv_json, shell_login_argv_json,
    shell_environment_names_json, shell_executable_digest, shell_config_digest,
    environment_digest, command_sequence, prompt_kind, prompt_sequence, prompt_last_exit,
    prompt_process_group, integration_health, cwd, state, slot_key, fingerprint, active_slot";

/// SQLite-backed durable owner for persistent terminal identity and metadata.
///
/// The repository stores path fields as exact platform bytes. It never stores
/// environment values, integration credentials, command text, PTY output,
/// writer leases, process IDs, or private spool locations.
pub struct AglTerminalRepository {
    store: Mutex<AglStore>,
}

impl AglTerminalRepository {
    pub fn open_at(root: impl AsRef<Path>) -> StoreResult<Self> {
        Ok(Self::from_store(AglStore::open_at(root)?))
    }

    pub fn from_store(store: AglStore) -> Self {
        Self {
            store: Mutex::new(store),
        }
    }

    pub fn record(&self, terminal_id: &TerminalId) -> agl_process::Result<StoredTerminalRecord> {
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
        operation: impl FnOnce(&AglStore) -> StoreResult<T>,
    ) -> agl_process::Result<T> {
        let store = self.store.lock().map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                "terminal repository lock is poisoned",
            )
        })?;
        operation(&store).map_err(terminal_store_error)
    }
}

impl TerminalRepository for AglTerminalRepository {
    fn reserve(&self, record: &StoredTerminalRecord) -> agl_process::Result<TerminalReservation> {
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

    fn replace(&self, record: &StoredTerminalRecord) -> agl_process::Result<()> {
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

    fn recover_for_new_owner(&self) -> agl_process::Result<Vec<StoredTerminalRecord>> {
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
    session_id: String,
    owner_kind: &'static str,
    owner_session_id: Option<String>,
    owner_root_run_id: Option<String>,
    owner_run_id: Option<String>,
    previous_owner_run_id: Option<String>,
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
    let (owner_kind, owner_session_id, owner_root_run_id, owner_run_id, previous_owner_run_id) =
        owner_columns(&record.record.owner);
    let (prompt_kind, prompt_sequence, prompt_last_exit, prompt_process_group) =
        prompt_columns(&record.record.prompt_state)?;
    Ok(EncodedTerminal {
        terminal_id: record.record.terminal_id.as_str().to_owned(),
        execution_id: record.record.execution_id.as_str().to_owned(),
        session_id: record.record.session_id.as_str().to_owned(),
        owner_kind,
        owner_session_id,
        owner_root_run_id,
        owner_run_id,
        previous_owner_run_id,
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
         (terminal_id, execution_id, session_id, owner_kind, owner_session_id,
          owner_root_run_id, owner_run_id, previous_owner_run_id, profile, workspace_root,
          shell_kind, shell_program, shell_argv_json, shell_login_argv_json,
          shell_environment_names_json, shell_executable_digest, shell_config_digest,
          environment_digest, command_sequence, prompt_kind, prompt_sequence,
          prompt_last_exit, prompt_process_group, integration_health, cwd, state, slot_key,
          fingerprint, active_slot)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                 ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26,
                 ?27, ?28, ?29)",
        terminal_params(record),
    )?;
    Ok(())
}

fn update_terminal(tx: &rusqlite::Transaction<'_>, record: &EncodedTerminal) -> StoreResult<usize> {
    Ok(tx.execute(
        "UPDATE terminal_sessions
         SET execution_id = ?2, session_id = ?3, owner_kind = ?4, owner_session_id = ?5,
             owner_root_run_id = ?6, owner_run_id = ?7, previous_owner_run_id = ?8,
             profile = ?9, workspace_root = ?10, shell_kind = ?11, shell_program = ?12,
             shell_argv_json = ?13, shell_login_argv_json = ?14,
             shell_environment_names_json = ?15, shell_executable_digest = ?16,
             shell_config_digest = ?17, environment_digest = ?18, command_sequence = ?19,
             prompt_kind = ?20, prompt_sequence = ?21, prompt_last_exit = ?22,
             prompt_process_group = ?23, integration_health = ?24, cwd = ?25, state = ?26,
             slot_key = ?27, fingerprint = ?28, active_slot = ?29
         WHERE terminal_id = ?1",
        terminal_params(record),
    )?)
}

fn terminal_params(record: &EncodedTerminal) -> [&dyn rusqlite::ToSql; 29] {
    [
        &record.terminal_id,
        &record.execution_id,
        &record.session_id,
        &record.owner_kind,
        &record.owner_session_id,
        &record.owner_root_run_id,
        &record.owner_run_id,
        &record.previous_owner_run_id,
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
    session_id: String,
    owner_kind: String,
    owner_session_id: Option<String>,
    owner_root_run_id: Option<String>,
    owner_run_id: Option<String>,
    previous_owner_run_id: Option<String>,
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
        session_id: row.get(2)?,
        owner_kind: row.get(3)?,
        owner_session_id: row.get(4)?,
        owner_root_run_id: row.get(5)?,
        owner_run_id: row.get(6)?,
        previous_owner_run_id: row.get(7)?,
        profile: row.get(8)?,
        workspace_root: row.get(9)?,
        shell_kind: row.get(10)?,
        shell_program: row.get(11)?,
        shell_argv_json: row.get(12)?,
        shell_login_argv_json: row.get(13)?,
        shell_environment_names_json: row.get(14)?,
        shell_executable_digest: row.get(15)?,
        shell_config_digest: row.get(16)?,
        environment_digest: row.get(17)?,
        command_sequence: row.get(18)?,
        prompt_kind: row.get(19)?,
        prompt_sequence: row.get(20)?,
        prompt_last_exit: row.get(21)?,
        prompt_process_group: row.get(22)?,
        integration_health: row.get(23)?,
        cwd: row.get(24)?,
        state: row.get(25)?,
        slot_key: row.get(26)?,
        fingerprint: row.get(27)?,
        active_slot: row.get(28)?,
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
    let session_id = parse_session_id(&raw.session_id, "terminal_sessions.session_id")?;
    let owner = decode_owner(&raw, &session_id)?;
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
            session_id,
            owner,
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
        reason: "stored terminal violates the persistence-neutral terminal contract",
    })?;
    Ok(stored)
}

type OwnerColumns = (
    &'static str,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn owner_columns(owner: &TerminalOwner) -> OwnerColumns {
    match owner {
        TerminalOwner::Human { session_id } => (
            "human",
            Some(session_id.as_str().to_owned()),
            None,
            None,
            None,
        ),
        TerminalOwner::MainAgent { session_id } => (
            "main_agent",
            Some(session_id.as_str().to_owned()),
            None,
            None,
            None,
        ),
        TerminalOwner::Subagent {
            root_run_id,
            owner_run_id,
        } => (
            "subagent",
            None,
            Some(root_run_id.as_str().to_owned()),
            Some(owner_run_id.as_str().to_owned()),
            None,
        ),
        TerminalOwner::SessionPromoted {
            session_id,
            previous_owner_run_id,
        } => (
            "session_promoted",
            Some(session_id.as_str().to_owned()),
            None,
            None,
            Some(previous_owner_run_id.as_str().to_owned()),
        ),
    }
}

fn decode_owner(raw: &RawTerminal, session_id: &SessionId) -> StoreResult<TerminalOwner> {
    match raw.owner_kind.as_str() {
        "human" => Ok(TerminalOwner::Human {
            session_id: parse_required_session_owner(raw, session_id)?,
        }),
        "main_agent" => Ok(TerminalOwner::MainAgent {
            session_id: parse_required_session_owner(raw, session_id)?,
        }),
        "subagent" => Ok(TerminalOwner::Subagent {
            root_run_id: parse_run_id(
                required(
                    raw.owner_root_run_id.as_deref(),
                    "terminal_sessions.owner_root_run_id",
                )?,
                "terminal_sessions.owner_root_run_id",
            )?,
            owner_run_id: parse_run_id(
                required(
                    raw.owner_run_id.as_deref(),
                    "terminal_sessions.owner_run_id",
                )?,
                "terminal_sessions.owner_run_id",
            )?,
        }),
        "session_promoted" => Ok(TerminalOwner::SessionPromoted {
            session_id: parse_required_session_owner(raw, session_id)?,
            previous_owner_run_id: parse_run_id(
                required(
                    raw.previous_owner_run_id.as_deref(),
                    "terminal_sessions.previous_owner_run_id",
                )?,
                "terminal_sessions.previous_owner_run_id",
            )?,
        }),
        value => Err(invalid_store_value(
            "terminal_sessions.owner_kind",
            value,
            "unknown terminal owner kind",
        )),
    }
}

fn parse_required_session_owner(raw: &RawTerminal, expected: &SessionId) -> StoreResult<SessionId> {
    let value = required(
        raw.owner_session_id.as_deref(),
        "terminal_sessions.owner_session_id",
    )?;
    let parsed = parse_session_id(value, "terminal_sessions.owner_session_id")?;
    if &parsed != expected {
        return Err(invalid_store_value(
            "terminal_sessions.owner_session_id",
            value,
            "terminal owner session does not match durable session",
        ));
    }
    Ok(parsed)
}

fn required<'a>(value: Option<&'a str>, field: &'static str) -> StoreResult<&'a str> {
    value.ok_or_else(|| invalid_store_value(field, "null", "required terminal field is absent"))
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

fn parse_session_id(value: &str, field: &'static str) -> StoreResult<SessionId> {
    SessionId::parse(value).map_err(|_| invalid_store_value(field, value, "invalid session ID"))
}

fn parse_run_id(value: &str, field: &'static str) -> StoreResult<RunId> {
    RunId::parse(value).map_err(|_| invalid_store_value(field, value, "invalid run ID"))
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agl_exec::ExecutionId;
    use agl_ids::{RunId, SessionId};
    use agl_process::{ProcessErrorCode, TerminalRepository, terminal_slot_key};
    use agl_terminal::TerminalId;

    use super::*;
    use crate::{CURRENT_SCHEMA_VERSION, STORE_MIGRATIONS};

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "agl-terminal-store-{label}-{}-{}",
                std::process::id(),
                TerminalId::generate()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn stored_terminal() -> StoredTerminalRecord {
        let session_id = SessionId::generate();
        let mut stored = StoredTerminalRecord {
            record: TerminalRecord {
                terminal_id: TerminalId::generate(),
                execution_id: ExecutionId::generate(),
                session_id: session_id.clone(),
                owner: TerminalOwner::Human { session_id },
                profile: ExecutionProfile::Workspace,
                workspace_root: PathBuf::from("/workspace"),
                shell_profile: AdmittedShellProfile {
                    kind: AdmittedShellKind::Bash,
                    snapshot: ShellProfileSnapshot {
                        program: PathBuf::from("/bin/bash"),
                        command_args: vec!["--noprofile".to_owned(), "--norc".to_owned()],
                        login_command_args: Some(vec!["--login".to_owned()]),
                        environment_names: vec!["LANG".to_owned(), "PATH".to_owned()],
                        executable_digest: "sha256:shell-executable".to_owned(),
                        config_digest: "sha256:shell-config".to_owned(),
                    },
                },
                environment_digest: serde_json::from_value(serde_json::Value::String(
                    "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                        .to_owned(),
                ))
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
    fn current_schema_keeps_private_terminal_columns() {
        assert_eq!(CURRENT_SCHEMA_VERSION, 17);
        assert_eq!(
            STORE_MIGRATIONS.last().map(|migration| migration.name),
            Some("017_incomplete_run_state")
        );
        let root = TempRoot::new("migration");
        let store = AglStore::open_at(&root.0).unwrap();
        let version: u32 = store
            .connection()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 17);

        let mut statement = store
            .connection()
            .prepare("SELECT name FROM pragma_table_info('terminal_sessions') ORDER BY cid")
            .unwrap();
        let columns: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for forbidden in [
            "environment_values_json",
            "integration_token",
            "command_text",
            "output",
            "input_lease",
            "spool_path",
        ] {
            assert!(!columns.iter().any(|column| column == forbidden));
        }
        let index_exists: bool = store
            .connection()
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_master
                    WHERE type = 'index' AND name = 'terminal_sessions_active_slot_unique_idx'
                )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(index_exists);
    }

    #[cfg(unix)]
    #[test]
    fn linux_paths_round_trip_as_exact_blob_bytes() {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

        let root = TempRoot::new("non-utf8");
        let repository = AglTerminalRepository::open_at(&root.0).unwrap();
        let mut record = stored_terminal();
        record.record.workspace_root =
            PathBuf::from(OsString::from_vec(b"/workspace/\x80-root".to_vec()));
        record.record.cwd = record
            .record
            .workspace_root
            .join(OsString::from_vec(vec![0xfe]));
        record.record.shell_profile.snapshot.program =
            PathBuf::from(OsString::from_vec(b"/runtime/\xff/bash".to_vec()));

        repository.reserve(&record).unwrap();
        let loaded = repository.record(&record.record.terminal_id).unwrap();
        assert_eq!(
            loaded.record.workspace_root.as_os_str().as_bytes(),
            record.record.workspace_root.as_os_str().as_bytes()
        );
        assert_eq!(
            loaded.record.cwd.as_os_str().as_bytes(),
            record.record.cwd.as_os_str().as_bytes()
        );
        assert_eq!(
            loaded
                .record
                .shell_profile
                .snapshot
                .program
                .as_os_str()
                .as_bytes(),
            record
                .record
                .shell_profile
                .snapshot
                .program
                .as_os_str()
                .as_bytes()
        );
        let storage_types: (String, String, String) = repository
            .store
            .lock()
            .unwrap()
            .connection()
            .query_row(
                "SELECT typeof(workspace_root), typeof(cwd), typeof(shell_program)
                 FROM terminal_sessions WHERE terminal_id = ?1",
                params![record.record.terminal_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            storage_types,
            ("blob".to_owned(), "blob".to_owned(), "blob".to_owned())
        );
    }

    #[test]
    fn reservation_is_fingerprint_idempotent_and_identities_are_unique() {
        let root = TempRoot::new("reservation");
        let repository = AglTerminalRepository::open_at(&root.0).unwrap();
        let record = stored_terminal();
        assert_eq!(
            repository.reserve(&record).unwrap(),
            TerminalReservation::Created
        );

        let mut retry = record.clone();
        retry.record.terminal_id = TerminalId::generate();
        retry.record.execution_id = ExecutionId::generate();
        assert_eq!(
            repository.reserve(&retry).unwrap(),
            TerminalReservation::Existing(Box::new(record.clone()))
        );

        let mut fingerprint_conflict = retry;
        fingerprint_conflict.fingerprint =
            "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned();
        assert_eq!(
            repository
                .reserve(&fingerprint_conflict)
                .unwrap_err()
                .code(),
            ProcessErrorCode::StateConflict
        );

        let mut execution_conflict = stored_terminal();
        execution_conflict.record.execution_id = record.record.execution_id.clone();
        assert_eq!(
            repository.reserve(&execution_conflict).unwrap_err().code(),
            ProcessErrorCode::StateConflict
        );
    }

    #[test]
    fn replacement_is_atomic_across_promotion_conflict_and_retirement() {
        let root = TempRoot::new("replace");
        let repository = AglTerminalRepository::open_at(&root.0).unwrap();
        let mut first = stored_terminal();
        let session_id = first.record.session_id.clone();
        let root_run_id = RunId::generate();
        let owner_run_id = RunId::generate();
        first.record.owner = TerminalOwner::Subagent {
            root_run_id,
            owner_run_id: owner_run_id.clone(),
        };
        first.slot_key = terminal_slot_key(&first.record).unwrap();
        repository.reserve(&first).unwrap();
        first.record.state = TerminalState::Running;
        repository.replace(&first).unwrap();

        let mut invalid_transition = first.clone();
        invalid_transition.record.state = TerminalState::Starting;
        assert_eq!(
            repository.replace(&invalid_transition).unwrap_err().code(),
            ProcessErrorCode::StateConflict
        );
        assert_eq!(repository.record(&first.record.terminal_id).unwrap(), first);

        first.record.owner = TerminalOwner::SessionPromoted {
            session_id,
            previous_owner_run_id: owner_run_id,
        };
        first.slot_key = terminal_slot_key(&first.record).unwrap();
        repository.replace(&first).unwrap();
        first.record.state = TerminalState::Exited;
        first.active_slot = false;
        repository.replace(&first).unwrap();
        assert_eq!(repository.record(&first.record.terminal_id).unwrap(), first);

        let mut human = stored_terminal();
        repository.reserve(&human).unwrap();
        human.record.state = TerminalState::Running;
        repository.replace(&human).unwrap();
        human.record.state = TerminalState::Exited;
        human.active_slot = false;
        repository.replace(&human).unwrap();
        let mut successor = human;
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
    fn restart_recovery_marks_live_records_unknown_and_retains_active_slot() {
        let root = TempRoot::new("recovery");
        let mut live = stored_terminal();
        let mut finished = stored_terminal();
        {
            let repository = AglTerminalRepository::open_at(&root.0).unwrap();
            repository.reserve(&live).unwrap();
            live.record.state = TerminalState::Running;
            live.record.prompt_state = TerminalPromptState::Ready {
                sequence: 7,
                last_exit: Some(0),
            };
            live.record.integration_health = ShellIntegrationHealth::Trusted;
            repository.replace(&live).unwrap();

            repository.reserve(&finished).unwrap();
            finished.record.state = TerminalState::Failed;
            finished.active_slot = false;
            repository.replace(&finished).unwrap();
        }

        let repository = AglTerminalRepository::open_at(&root.0).unwrap();
        let recovered = repository.recover_for_new_owner().unwrap();
        let recovered_live = recovered
            .iter()
            .find(|record| record.record.terminal_id == live.record.terminal_id)
            .unwrap();
        assert_eq!(recovered_live.record.state, TerminalState::OutcomeUnknown);
        assert_eq!(
            recovered_live.record.prompt_state,
            TerminalPromptState::Degraded
        );
        assert_eq!(
            recovered_live.record.integration_health,
            ShellIntegrationHealth::Degraded
        );
        assert!(recovered_live.active_slot);
        let recovered_finished = recovered
            .iter()
            .find(|record| record.record.terminal_id == finished.record.terminal_id)
            .unwrap();
        assert_eq!(recovered_finished.record.state, TerminalState::Failed);
        assert!(!recovered_finished.active_slot);

        let retry = stored_retry(&live);
        let TerminalReservation::Existing(existing) = repository.reserve(&retry).unwrap() else {
            panic!("recovery retry must resolve to the durable terminal identity");
        };
        assert_eq!(existing.record.state, TerminalState::OutcomeUnknown);
        assert!(existing.active_slot);
    }

    #[test]
    fn corrupt_slot_scope_digest_and_prompt_rows_fail_closed_on_decode() {
        let root = TempRoot::new("corrupt-row");
        let repository = AglTerminalRepository::open_at(&root.0).unwrap();
        let record = stored_terminal();
        repository.reserve(&record).unwrap();

        {
            let store = repository.store.lock().unwrap();
            store
                .connection()
                .execute(
                    "UPDATE terminal_sessions SET slot_key = 'human:workspace:wrong'
                     WHERE terminal_id = ?1",
                    params![record.record.terminal_id.as_str()],
                )
                .unwrap();
        }
        assert_eq!(
            repository
                .record(&record.record.terminal_id)
                .unwrap_err()
                .code(),
            ProcessErrorCode::StoreCorrupt
        );

        {
            let store = repository.store.lock().unwrap();
            store
                .connection()
                .execute(
                    "UPDATE terminal_sessions SET slot_key = ?1, cwd = ?2
                     WHERE terminal_id = ?3",
                    params![
                        &record.slot_key,
                        b"/outside".as_slice(),
                        record.record.terminal_id.as_str()
                    ],
                )
                .unwrap();
        }
        assert_eq!(
            repository
                .record(&record.record.terminal_id)
                .unwrap_err()
                .code(),
            ProcessErrorCode::StoreCorrupt
        );

        {
            let store = repository.store.lock().unwrap();
            store
                .connection()
                .execute(
                    "UPDATE terminal_sessions SET cwd = ?1, environment_digest = 'sha256:bad'
                     WHERE terminal_id = ?2",
                    params![
                        path_bytes(&record.record.cwd).unwrap(),
                        record.record.terminal_id.as_str()
                    ],
                )
                .unwrap();
        }
        assert_eq!(
            repository
                .record(&record.record.terminal_id)
                .unwrap_err()
                .code(),
            ProcessErrorCode::StoreCorrupt
        );

        {
            let store = repository.store.lock().unwrap();
            store
                .connection()
                .execute_batch("PRAGMA ignore_check_constraints = ON;")
                .unwrap();
            store
                .connection()
                .execute(
                    "UPDATE terminal_sessions
                     SET environment_digest = ?1, prompt_kind = 'ready', prompt_sequence = 0,
                         prompt_last_exit = 256, integration_health = 'trusted'
                     WHERE terminal_id = ?2",
                    params![
                        record.record.environment_digest.as_str(),
                        record.record.terminal_id.as_str()
                    ],
                )
                .unwrap();
        }
        assert_eq!(
            repository
                .record(&record.record.terminal_id)
                .unwrap_err()
                .code(),
            ProcessErrorCode::StoreCorrupt
        );
    }

    fn stored_retry(existing: &StoredTerminalRecord) -> StoredTerminalRecord {
        let mut retry = existing.clone();
        retry.record.terminal_id = TerminalId::generate();
        retry.record.execution_id = ExecutionId::generate();
        retry.record.state = TerminalState::Starting;
        retry.record.prompt_state = TerminalPromptState::Unknown;
        retry.record.integration_health = ShellIntegrationHealth::AwaitingFirstPrompt;
        retry
    }
}
