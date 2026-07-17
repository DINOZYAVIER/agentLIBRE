use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use agl_ids::{ExecutionId, RunId, SessionId, StepId};
use agl_process::{
    CommittedOutputFrame, ExecutionChannel, ExecutionExit, ExecutionIo, ExecutionKind,
    ExecutionListFilter, ExecutionOutputChunk, ExecutionOwner, ExecutionPrivateCommand,
    ExecutionProfile, ExecutionRepository, ExecutionRequest, ExecutionState, ExecutionStatus,
    ExecutionTerminalUpdate, InputLease, ProcessBytes, ProcessError, ProcessErrorCode,
    Result as ProcessResult, TerminalSize,
};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{AglStore, Result as StoreResult, StoreError};

const EXECUTION_COLUMNS: &str = "id, owner_kind, owner_session_id, owner_run_id,
    root_run_id, state, profile, io, cwd, terminal_columns, terminal_rows, exit_kind,
    exit_code, exit_signal, exit_error_code, error_code, started_at_ms, finished_at_ms,
    first_retained_sequence, last_sequence, retained_bytes, discarded_output_bytes,
    output_truncated, output_expired";

pub struct AglExecutionRepository {
    store: Mutex<AglStore>,
    finished_retention_ms: i64,
}

impl AglExecutionRepository {
    pub fn open_at(root: impl AsRef<Path>, finished_retention: Duration) -> StoreResult<Self> {
        Ok(Self::from_store(
            AglStore::open_at(root)?,
            finished_retention,
        ))
    }

    pub fn from_store(store: AglStore, finished_retention: Duration) -> Self {
        Self {
            store: Mutex::new(store),
            finished_retention_ms: i64::try_from(finished_retention.as_millis())
                .unwrap_or(i64::MAX),
        }
    }

    fn with_store<T>(
        &self,
        operation: impl FnOnce(&AglStore) -> StoreResult<T>,
    ) -> ProcessResult<T> {
        let store = self.store.lock().map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                "execution repository lock is poisoned",
            )
        })?;
        operation(&store).map_err(store_error)
    }
}

impl ExecutionRepository for AglExecutionRepository {
    fn admit(
        &self,
        status: &ExecutionStatus,
        request: &ExecutionRequest,
        supervisor_id: &str,
    ) -> ProcessResult<()> {
        request.validate()?;
        if status.execution_id.as_str().is_empty() || supervisor_id.trim().is_empty() {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "execution admission identity and supervisor owner must be nonempty",
            ));
        }
        let (owner_kind, owner_session_id, owner_run_id, root_run_id) =
            owner_columns(&request.owner);
        let (columns, rows) = terminal_columns(request.terminal_size);
        let grant_lease_json = request
            .grant_lease
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(json_process_error)?;
        let invocation_json =
            serde_json::to_string(&PrivateInvocation::from(request)).map_err(json_process_error)?;
        let accepted_input_bytes = request
            .stdin
            .as_ref()
            .map(|stdin| {
                usize::try_from(request.limits.max_input_bytes)
                    .map_err(|_| integer_process_error())
                    .and_then(|maximum| stdin.decode(maximum))
                    .and_then(|bytes| {
                        i64::try_from(bytes.len()).map_err(|_| integer_process_error())
                    })
            })
            .transpose()?
            .unwrap_or(0);
        let spool_ref = format!("executions/{}/stream.aglspool", status.execution_id);
        self.with_store(|store| {
            store.transaction(|tx| {
                tx.execute(
                    "INSERT INTO executions
                     (id, owner_kind, owner_session_id, owner_run_id, root_run_id,
                      creating_run_id, creating_step_id, execution_kind, state, profile, io, cwd,
                      terminal_columns, terminal_rows, supervisor_id, created_at_ms, updated_at_ms,
                      grant_lease_json, invocation_json, spool_ref, accepted_input_bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                             ?13, ?14, ?15, ?16, ?16, ?17, ?18, ?19, ?20)",
                    params![
                        status.execution_id.as_str(),
                        owner_kind,
                        owner_session_id,
                        owner_run_id,
                        root_run_id,
                        request.creating_run_id.as_str(),
                        request.creating_step_id.as_str(),
                        execution_kind(request.kind),
                        execution_state(status.state),
                        execution_profile(status.profile),
                        execution_io(status.io),
                        status.cwd.to_string_lossy(),
                        columns,
                        rows,
                        supervisor_id,
                        unix_millis(),
                        grant_lease_json,
                        invocation_json,
                        spool_ref,
                        accepted_input_bytes,
                    ],
                )?;
                Ok(())
            })
        })
    }

    fn mark_running(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        started_at_unix_ms: i64,
    ) -> ProcessResult<()> {
        self.with_store(|store| {
            store.transaction(|tx| {
                let changed = tx.execute(
                    "UPDATE executions
                     SET state = 'running', started_at_ms = ?1, updated_at_ms = ?1
                     WHERE id = ?2 AND supervisor_id = ?3 AND state = 'starting'",
                    params![started_at_unix_ms, execution_id.as_str(), supervisor_id],
                )?;
                require_fenced_change(changed, execution_id)?;
                Ok(())
            })
        })
    }

    fn append_lifecycle(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        sequence: u64,
        kind: &str,
        occurred_at_unix_ms: i64,
    ) -> ProcessResult<()> {
        validate_event_kind(kind)?;
        self.with_store(|store| {
            store.transaction(|tx| {
                advance_sequence(
                    tx,
                    execution_id,
                    supervisor_id,
                    sequence,
                    occurred_at_unix_ms,
                )?;
                insert_metadata_event(tx, execution_id, sequence, kind, occurred_at_unix_ms)?;
                Ok(())
            })
        })
    }

    fn append_indexed_chunk(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        chunk: &ExecutionOutputChunk,
        spool_offset: u64,
        byte_length: u64,
        occurred_at_unix_ms: i64,
    ) -> ProcessResult<()> {
        if byte_length == 0 {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "indexed output chunk must be nonempty",
            ));
        }
        let payload = chunk.bytes.decode(usize::MAX)?;
        if payload.len() as u64 != byte_length {
            return Err(ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                "indexed output byte length does not match the durable spool frame",
            ));
        }
        let preview = ProcessBytes::from_bytes(&payload[..payload.len().min(256)]);
        let preview_json = serde_json::to_string(&preview).map_err(json_process_error)?;
        let digest = sha256_digest(&payload);
        let offset = i64::try_from(spool_offset).map_err(|_| integer_process_error())?;
        let length = i64::try_from(byte_length).map_err(|_| integer_process_error())?;
        self.with_store(|store| {
            store.transaction(|tx| {
                let previous =
                    chunk
                        .sequence
                        .checked_sub(1)
                        .ok_or_else(|| StoreError::InvalidValue {
                            field: "execution_events.sequence",
                            value: chunk.sequence.to_string(),
                            reason: "sequence must be positive",
                        })?;
                let changed = tx.execute(
                    "UPDATE executions
                     SET last_sequence = ?1,
                         first_retained_sequence = COALESCE(first_retained_sequence, ?1),
                         retained_bytes = retained_bytes + ?2,
                         updated_at_ms = ?3
                     WHERE id = ?4 AND supervisor_id = ?5 AND last_sequence = ?6
                       AND state IN ('starting', 'running')",
                    params![
                        chunk.sequence,
                        length,
                        occurred_at_unix_ms,
                        execution_id.as_str(),
                        supervisor_id,
                        previous,
                    ],
                )?;
                require_fenced_change(changed, execution_id)?;
                tx.execute(
                    "INSERT INTO execution_events
                     (execution_id, sequence, kind, channel, spool_offset, byte_length,
                      bounded_preview_json, occurred_at_ms, safe_digest)
                     VALUES (?1, ?2, 'output', ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        execution_id.as_str(),
                        chunk.sequence,
                        execution_channel(chunk.channel),
                        offset,
                        length,
                        preview_json,
                        occurred_at_unix_ms,
                        digest,
                    ],
                )?;
                Ok(())
            })
        })
    }

    fn update_terminal_size(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        sequence: u64,
        terminal_size: TerminalSize,
        occurred_at_unix_ms: i64,
    ) -> ProcessResult<()> {
        let terminal_size = terminal_size.validate()?;
        self.with_store(|store| {
            store.transaction(|tx| {
                let previous = sequence
                    .checked_sub(1)
                    .ok_or_else(|| StoreError::InvalidValue {
                        field: "execution_events.sequence",
                        value: sequence.to_string(),
                        reason: "sequence must be positive",
                    })?;
                let changed = tx.execute(
                    "UPDATE executions
                     SET terminal_columns = ?1, terminal_rows = ?2, last_sequence = ?3,
                         updated_at_ms = ?4
                     WHERE id = ?5 AND supervisor_id = ?6 AND io = 'pty'
                       AND state IN ('starting', 'running') AND last_sequence = ?7",
                    params![
                        terminal_size.columns,
                        terminal_size.rows,
                        sequence,
                        occurred_at_unix_ms,
                        execution_id.as_str(),
                        supervisor_id,
                        previous,
                    ],
                )?;
                require_fenced_change(changed, execution_id)?;
                insert_metadata_event(tx, execution_id, sequence, "resize", occurred_at_unix_ms)?;
                Ok(())
            })
        })
    }

    fn bind_input_lease(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        lease: &InputLease,
        occurred_at_unix_ms: i64,
    ) -> ProcessResult<()> {
        if !lease.writable {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "only writable attachments can bind the input lease",
            ));
        }
        self.with_store(|store| {
            store.transaction(|tx| {
                let changed = tx.execute(
                    "UPDATE executions
                     SET input_lease_id = ?1, input_lease_renewed_at_ms = ?2, updated_at_ms = ?2
                     WHERE id = ?3 AND supervisor_id = ?4 AND state IN ('starting', 'running')
                       AND input_lease_id IS NULL",
                    params![
                        lease.attachment_id.as_str(),
                        occurred_at_unix_ms,
                        execution_id.as_str(),
                        supervisor_id,
                    ],
                )?;
                if changed != 1 {
                    return Err(StoreError::LeaseLost {
                        resource: format!("execution input {execution_id}"),
                    });
                }
                Ok(())
            })
        })
        .map_err(|error| {
            if error.code() == ProcessErrorCode::StateConflict {
                ProcessError::new(
                    ProcessErrorCode::InputLeaseBusy,
                    "execution input lease is already held or stale",
                )
            } else {
                error
            }
        })
    }

    fn release_input_lease(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        lease: &InputLease,
        occurred_at_unix_ms: i64,
    ) -> ProcessResult<()> {
        self.with_store(|store| {
            store.transaction(|tx| {
                let changed = tx.execute(
                    "UPDATE executions
                     SET input_lease_id = NULL, input_lease_renewed_at_ms = NULL,
                         updated_at_ms = ?1
                     WHERE id = ?2 AND supervisor_id = ?3 AND input_lease_id = ?4",
                    params![
                        occurred_at_unix_ms,
                        execution_id.as_str(),
                        supervisor_id,
                        lease.attachment_id.as_str(),
                    ],
                )?;
                require_fenced_change(changed, execution_id)?;
                Ok(())
            })
        })
    }

    fn renew_input_lease(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        lease: &InputLease,
        occurred_at_unix_ms: i64,
    ) -> ProcessResult<()> {
        if !lease.writable {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "only writable attachments can renew the input lease",
            ));
        }
        self.with_store(|store| {
            store.transaction(|tx| {
                let changed = tx.execute(
                    "UPDATE executions
                     SET input_lease_renewed_at_ms = ?1, updated_at_ms = ?1
                     WHERE id = ?2 AND supervisor_id = ?3
                       AND state IN ('starting', 'running') AND input_lease_id = ?4",
                    params![
                        occurred_at_unix_ms,
                        execution_id.as_str(),
                        supervisor_id,
                        lease.attachment_id.as_str(),
                    ],
                )?;
                if changed == 0 {
                    return Err(StoreError::LeaseLost {
                        resource: format!("execution input {execution_id}"),
                    });
                }
                Ok(())
            })
        })
        .map_err(|error| {
            if error.code() == ProcessErrorCode::StateConflict {
                ProcessError::new(
                    ProcessErrorCode::InputLeaseExpired,
                    "execution input lease expired or is no longer owned by this attachment",
                )
            } else {
                error
            }
        })
    }

    fn accept_input(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        lease: &InputLease,
        byte_length: u64,
        _eof: bool,
        occurred_at_unix_ms: i64,
    ) -> ProcessResult<()> {
        let byte_length = i64::try_from(byte_length).map_err(|_| integer_process_error())?;
        self.with_store(|store| {
            store.transaction(|tx| {
                let changed = tx.execute(
                    "UPDATE executions
                     SET accepted_input_bytes = accepted_input_bytes + ?1, updated_at_ms = ?2
                     WHERE id = ?3 AND supervisor_id = ?4 AND state IN ('starting', 'running')
                       AND input_lease_id = ?5",
                    params![
                        byte_length,
                        occurred_at_unix_ms,
                        execution_id.as_str(),
                        supervisor_id,
                        lease.attachment_id.as_str(),
                    ],
                )?;
                require_fenced_change(changed, execution_id)?;
                Ok(())
            })
        })
    }

    fn mark_terminal(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        sequence: u64,
        update: &ExecutionTerminalUpdate,
    ) -> ProcessResult<()> {
        update.validate()?;
        let previous = sequence.checked_sub(1).ok_or_else(integer_process_error)?;
        let (exit_kind, exit_code, exit_signal, exit_error_code) =
            exit_columns(update.exit.as_ref());
        let retention_deadline = update
            .finished_at_unix_ms
            .saturating_add(self.finished_retention_ms);
        self.with_store(|store| {
            store.transaction(|tx| {
                let changed = tx.execute(
                    "UPDATE executions
                     SET state = ?1, exit_kind = ?2, exit_code = ?3, exit_signal = ?4,
                         exit_error_code = ?5, error_code = ?6, finished_at_ms = ?7,
                         output_truncated = MAX(output_truncated, ?8),
                         discarded_output_bytes = discarded_output_bytes + ?9,
                         last_sequence = ?10, input_lease_id = NULL,
                         input_lease_renewed_at_ms = NULL,
                         retention_deadline_ms = ?11, updated_at_ms = ?7
                     WHERE id = ?12 AND supervisor_id = ?13 AND last_sequence = ?14
                       AND state IN ('starting', 'running')",
                    params![
                        execution_state(update.state),
                        exit_kind,
                        exit_code,
                        exit_signal,
                        exit_error_code,
                        update.error_code,
                        update.finished_at_unix_ms,
                        update.output_truncated,
                        update.discarded_output_bytes,
                        sequence,
                        retention_deadline,
                        execution_id.as_str(),
                        supervisor_id,
                        previous,
                    ],
                )?;
                require_fenced_change(changed, execution_id)?;
                insert_metadata_event(
                    tx,
                    execution_id,
                    sequence,
                    "terminal",
                    update.finished_at_unix_ms,
                )?;
                Ok(())
            })
        })
    }

    fn status(&self, execution_id: &ExecutionId) -> ProcessResult<ExecutionStatus> {
        self.with_store(|store| {
            let sql = format!("SELECT {EXECUTION_COLUMNS} FROM executions WHERE id = ?1");
            store
                .connection()
                .query_row(&sql, [execution_id.as_str()], read_execution_row)
                .optional()?
                .map(decode_execution_row)
                .transpose()?
                .ok_or_else(|| StoreError::NotFound {
                    resource: format!("execution {execution_id}"),
                })
        })
        .map_err(|error| {
            if error.message().contains("not found") {
                ProcessError::new(
                    ProcessErrorCode::ExecutionNotFound,
                    format!("execution `{execution_id}` was not found"),
                )
            } else {
                error
            }
        })
    }

    fn private_command(
        &self,
        execution_id: &ExecutionId,
        maximum_bytes: usize,
    ) -> ProcessResult<ExecutionPrivateCommand> {
        let invocation_json = self
            .with_store(|store| {
                store
                    .connection()
                    .query_row(
                        "SELECT invocation_json FROM executions WHERE id = ?1",
                        [execution_id.as_str()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .ok_or_else(|| StoreError::NotFound {
                        resource: format!("execution {execution_id}"),
                    })
            })
            .map_err(|error| {
                if error.message().contains("not found") {
                    ProcessError::new(
                        ProcessErrorCode::ExecutionNotFound,
                        format!("execution `{execution_id}` was not found"),
                    )
                } else {
                    error
                }
            })?;
        let invocation: StoredPrivateInvocation =
            serde_json::from_str(&invocation_json).map_err(json_process_error)?;
        ExecutionPrivateCommand::from_argv(&invocation.program, &invocation.args, maximum_bytes)
    }

    fn list(&self, filter: &ExecutionListFilter) -> ProcessResult<Vec<ExecutionStatus>> {
        self.with_store(|store| {
            let sql = format!("SELECT {EXECUTION_COLUMNS} FROM executions ORDER BY created_at_ms");
            let mut statement = store.connection().prepare(&sql)?;
            let rows = statement.query_map([], read_execution_row)?;
            let statuses = rows
                .map(|row| row.map_err(StoreError::from).and_then(decode_execution_row))
                .collect::<StoreResult<Vec<_>>>()?;
            Ok(statuses
                .into_iter()
                .filter(|status| filter.include_finished || !status.state.is_terminal())
                .filter(|status| {
                    filter.session_id.as_ref().is_none_or(|expected| {
                        matches!(
                            &status.owner,
                            ExecutionOwner::Session { session_id, .. } if session_id == expected
                        )
                    })
                })
                .filter(|status| {
                    filter
                        .root_run_id
                        .as_ref()
                        .is_none_or(|expected| status.owner.root_run_id() == expected)
                })
                .collect())
        })
    }

    fn committed_output_frames(
        &self,
        execution_id: &ExecutionId,
    ) -> ProcessResult<Vec<CommittedOutputFrame>> {
        self.with_store(|store| {
            let exists = store.connection().query_row(
                "SELECT EXISTS(SELECT 1 FROM executions WHERE id = ?1)",
                [execution_id.as_str()],
                |row| row.get::<_, bool>(0),
            )?;
            if !exists {
                return Err(StoreError::NotFound {
                    resource: format!("execution {execution_id}"),
                });
            }
            let mut statement = store.connection().prepare(
                "SELECT sequence, channel, spool_offset, byte_length, safe_digest
                 FROM execution_events
                 WHERE execution_id = ?1 AND kind = 'output'
                 ORDER BY sequence",
            )?;
            let rows = statement.query_map([execution_id.as_str()], |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            rows.map(|row| {
                let (sequence, channel, spool_offset, byte_length, safe_digest) = row?;
                Ok(CommittedOutputFrame {
                    sequence,
                    channel: parse_execution_channel(&channel)?,
                    spool_offset,
                    byte_length,
                    safe_digest,
                })
            })
            .collect()
        })
        .map_err(|error| {
            if error.message().contains("not found") {
                ProcessError::new(
                    ProcessErrorCode::ExecutionNotFound,
                    format!("execution `{execution_id}` was not found"),
                )
            } else {
                error
            }
        })
    }

    fn recover_prior_owners(
        &self,
        current_supervisor_id: &str,
        recovered_at_unix_ms: i64,
    ) -> ProcessResult<Vec<ExecutionId>> {
        self.with_store(|store| {
            store.transaction(|tx| {
                let mut statement = tx.prepare(
                    "SELECT id, last_sequence, creating_run_id, creating_step_id FROM executions
                     WHERE state IN ('starting', 'running') AND supervisor_id != ?1
                     ORDER BY created_at_ms",
                )?;
                let rows = statement.query_map([current_supervisor_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?;
                let candidates = rows.collect::<rusqlite::Result<Vec<_>>>()?;
                drop(statement);
                let mut recovered = Vec::with_capacity(candidates.len());
                for (raw_id, last_sequence, raw_run_id, raw_step_id) in candidates {
                    let execution_id = parse_execution_id(&raw_id, "executions.id")?;
                    let creating_run_id = parse_run_id(&raw_run_id, "executions.creating_run_id")?;
                    let creating_step_id =
                        parse_step_id(&raw_step_id, "executions.creating_step_id")?;
                    let sequence =
                        last_sequence
                            .checked_add(1)
                            .ok_or_else(|| StoreError::InvalidValue {
                                field: "executions.last_sequence",
                                value: last_sequence.to_string(),
                                reason: "sequence overflow",
                            })?;
                    let changed = tx.execute(
                        "UPDATE executions
                         SET state = 'outcome_unknown', error_code = 'owner_lost',
                             finished_at_ms = ?1, updated_at_ms = ?1, last_sequence = ?2,
                             input_lease_id = NULL, input_lease_renewed_at_ms = NULL,
                             retention_deadline_ms = ?3
                         WHERE id = ?4 AND state IN ('starting', 'running')
                           AND supervisor_id != ?5 AND last_sequence = ?6",
                        params![
                            recovered_at_unix_ms,
                            sequence,
                            recovered_at_unix_ms.saturating_add(self.finished_retention_ms),
                            execution_id.as_str(),
                            current_supervisor_id,
                            last_sequence,
                        ],
                    )?;
                    require_fenced_change(changed, &execution_id)?;
                    insert_metadata_event(
                        tx,
                        &execution_id,
                        sequence,
                        "owner_lost",
                        recovered_at_unix_ms,
                    )?;
                    let step_changed = tx.execute(
                        "UPDATE run_steps
                         SET state = 'outcome_unknown', error_code = 'effect_outcome_unknown',
                             lease_owner = NULL, lease_expires_at_ms = NULL,
                             updated_at_ms = ?1, finished_at_ms = ?1
                         WHERE id = ?2 AND run_id = ?3 AND state = 'running'
                           AND delivery_class = 'at_most_once'",
                        params![
                            recovered_at_unix_ms,
                            creating_step_id.as_str(),
                            creating_run_id.as_str(),
                        ],
                    )?;
                    if step_changed == 1 {
                        let run_changed = tx.execute(
                            "UPDATE runs
                             SET state = 'failed', error_code = 'effect_outcome_unknown',
                                 error_message = 'a process owner exited before its at-most-once effect outcome was recorded',
                                 lease_owner = NULL, lease_expires_at_ms = NULL,
                                 updated_at_ms = ?1, finished_at_ms = ?1
                             WHERE id = ?2 AND state = 'running'",
                            params![recovered_at_unix_ms, creating_run_id.as_str()],
                        )?;
                        if run_changed != 1 {
                            return Err(StoreError::LeaseLost {
                                resource: format!(
                                    "run {creating_run_id} linked to execution {execution_id}"
                                ),
                            });
                        }
                    }
                    recovered.push(execution_id);
                }
                Ok(recovered)
            })
        })
    }

    fn output_retention_candidates(
        &self,
        now_unix_ms: i64,
        limit: usize,
    ) -> ProcessResult<Vec<ExecutionId>> {
        if limit == 0 {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "output retention candidate limit must be nonzero",
            ));
        }
        let limit = i64::try_from(limit).map_err(|_| integer_process_error())?;
        self.with_store(|store| {
            let mut statement = store.connection().prepare(
                "SELECT id FROM executions
                 WHERE state NOT IN ('starting', 'running')
                   AND output_expired = 0
                   AND retention_deadline_ms IS NOT NULL
                   AND retention_deadline_ms <= ?1
                   AND cleanup_state IN ('live', 'tombstoned')
                 ORDER BY retention_deadline_ms, id
                 LIMIT ?2",
            )?;
            let rows =
                statement.query_map(params![now_unix_ms, limit], |row| row.get::<_, String>(0))?;
            rows.map(|row| {
                let raw = row?;
                parse_execution_id(&raw, "executions.id")
            })
            .collect()
        })
    }

    fn tombstone_output(
        &self,
        execution_id: &ExecutionId,
        tombstoned_at_unix_ms: i64,
    ) -> ProcessResult<()> {
        self.with_store(|store| {
            let changed = store.connection().execute(
                "UPDATE executions
                 SET cleanup_state = 'tombstoned', updated_at_ms = ?1
                 WHERE id = ?2 AND state NOT IN ('starting', 'running')
                   AND output_expired = 0 AND cleanup_state IN ('live', 'tombstoned')",
                params![tombstoned_at_unix_ms, execution_id.as_str()],
            )?;
            require_fenced_change(changed, execution_id)
        })
    }

    fn mark_output_expired(
        &self,
        execution_id: &ExecutionId,
        expired_at_unix_ms: i64,
    ) -> ProcessResult<()> {
        self.with_store(|store| {
            store.transaction(|tx| {
                let changed = tx.execute(
                    "UPDATE executions
                     SET output_expired = 1, retained_bytes = 0,
                         first_retained_sequence = NULL, cleanup_state = 'cleaned',
                         updated_at_ms = ?1
                     WHERE id = ?2 AND state NOT IN ('starting', 'running')
                       AND cleanup_state IN ('live', 'tombstoned')",
                    params![expired_at_unix_ms, execution_id.as_str()],
                )?;
                require_fenced_change(changed, execution_id)?;
                tx.execute(
                    "UPDATE execution_events SET bounded_preview_json = NULL
                     WHERE execution_id = ?1 AND kind = 'output'",
                    [execution_id.as_str()],
                )?;
                Ok(())
            })
        })
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct PrivateInvocation<'a> {
    kind: &'static str,
    program: &'a Path,
    args: &'a [String],
    environment_keys: Vec<&'a str>,
    profile: &'static str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPrivateInvocation {
    #[allow(dead_code)]
    kind: String,
    program: PathBuf,
    args: Vec<String>,
    #[allow(dead_code)]
    environment_keys: Vec<String>,
    #[allow(dead_code)]
    profile: String,
}

impl<'a> From<&'a ExecutionRequest> for PrivateInvocation<'a> {
    fn from(request: &'a ExecutionRequest) -> Self {
        Self {
            kind: execution_kind(request.kind),
            program: &request.program,
            args: &request.args,
            environment_keys: request
                .environment
                .values
                .keys()
                .map(String::as_str)
                .collect(),
            profile: execution_profile(request.profile),
        }
    }
}

struct RawExecutionRow {
    id: String,
    owner_kind: String,
    owner_session_id: Option<String>,
    owner_run_id: Option<String>,
    root_run_id: String,
    state: String,
    profile: String,
    io: String,
    cwd: String,
    terminal_columns: Option<u16>,
    terminal_rows: Option<u16>,
    exit_kind: Option<String>,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    exit_error_code: Option<String>,
    error_code: Option<String>,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
    first_retained_sequence: Option<u64>,
    last_sequence: u64,
    retained_bytes: u64,
    discarded_output_bytes: u64,
    output_truncated: bool,
    output_expired: bool,
}

fn read_execution_row(row: &Row<'_>) -> rusqlite::Result<RawExecutionRow> {
    Ok(RawExecutionRow {
        id: row.get(0)?,
        owner_kind: row.get(1)?,
        owner_session_id: row.get(2)?,
        owner_run_id: row.get(3)?,
        root_run_id: row.get(4)?,
        state: row.get(5)?,
        profile: row.get(6)?,
        io: row.get(7)?,
        cwd: row.get(8)?,
        terminal_columns: row.get(9)?,
        terminal_rows: row.get(10)?,
        exit_kind: row.get(11)?,
        exit_code: row.get(12)?,
        exit_signal: row.get(13)?,
        exit_error_code: row.get(14)?,
        error_code: row.get(15)?,
        started_at_ms: row.get(16)?,
        finished_at_ms: row.get(17)?,
        first_retained_sequence: row.get(18)?,
        last_sequence: row.get(19)?,
        retained_bytes: row.get(20)?,
        discarded_output_bytes: row.get(21)?,
        output_truncated: row.get(22)?,
        output_expired: row.get(23)?,
    })
}

fn decode_execution_row(raw: RawExecutionRow) -> StoreResult<ExecutionStatus> {
    let root_run_id = parse_run_id(&raw.root_run_id, "executions.root_run_id")?;
    let owner = match raw.owner_kind.as_str() {
        "session" => ExecutionOwner::Session {
            session_id: parse_session_id(
                raw.owner_session_id.as_deref().ok_or_else(|| {
                    invalid_store_value(
                        "executions.owner_session_id",
                        "null",
                        "session owner requires a session ID",
                    )
                })?,
                "executions.owner_session_id",
            )?,
            root_run_id,
        },
        "run" => ExecutionOwner::Run {
            run_id: parse_run_id(
                raw.owner_run_id.as_deref().ok_or_else(|| {
                    invalid_store_value(
                        "executions.owner_run_id",
                        "null",
                        "run owner requires a run ID",
                    )
                })?,
                "executions.owner_run_id",
            )?,
            root_run_id,
        },
        value => {
            return Err(invalid_store_value(
                "executions.owner_kind",
                value,
                "unknown execution owner kind",
            ));
        }
    };
    let terminal_size = match (raw.terminal_columns, raw.terminal_rows) {
        (Some(columns), Some(rows)) => Some(TerminalSize { columns, rows }),
        (None, None) => None,
        _ => {
            return Err(invalid_store_value(
                "executions.terminal_size",
                "partial",
                "terminal dimensions must both be present or absent",
            ));
        }
    };
    let exit = match raw.exit_kind.as_deref() {
        None => None,
        Some("code") => Some(ExecutionExit::Code {
            code: raw.exit_code.ok_or_else(|| {
                invalid_store_value("executions.exit_code", "null", "code exit requires a value")
            })?,
        }),
        Some("signal") => Some(ExecutionExit::Signal {
            signal: raw.exit_signal.ok_or_else(|| {
                invalid_store_value(
                    "executions.exit_signal",
                    "null",
                    "signal exit requires a value",
                )
            })?,
        }),
        Some("error") => Some(ExecutionExit::Error {
            code: raw.exit_error_code.ok_or_else(|| {
                invalid_store_value(
                    "executions.exit_error_code",
                    "null",
                    "error exit requires a code",
                )
            })?,
        }),
        Some(value) => {
            return Err(invalid_store_value(
                "executions.exit_kind",
                value,
                "unknown execution exit kind",
            ));
        }
    };
    Ok(ExecutionStatus {
        execution_id: parse_execution_id(&raw.id, "executions.id")?,
        owner,
        state: parse_execution_state(&raw.state)?,
        profile: parse_execution_profile(&raw.profile)?,
        io: parse_execution_io(&raw.io)?,
        cwd: PathBuf::from(raw.cwd),
        terminal_size,
        exit,
        first_retained_sequence: raw.first_retained_sequence,
        last_sequence: raw.last_sequence,
        retained_bytes: raw.retained_bytes,
        discarded_output_bytes: raw.discarded_output_bytes,
        output_truncated: raw.output_truncated,
        output_expired: raw.output_expired,
        started_at_unix_ms: raw.started_at_ms,
        finished_at_unix_ms: raw.finished_at_ms,
        error_code: raw.error_code,
    })
}

fn advance_sequence(
    tx: &rusqlite::Transaction<'_>,
    execution_id: &ExecutionId,
    supervisor_id: &str,
    sequence: u64,
    occurred_at_unix_ms: i64,
) -> StoreResult<()> {
    let previous = sequence
        .checked_sub(1)
        .ok_or_else(|| StoreError::InvalidValue {
            field: "execution_events.sequence",
            value: sequence.to_string(),
            reason: "sequence must be positive",
        })?;
    let changed = tx.execute(
        "UPDATE executions SET last_sequence = ?1, updated_at_ms = ?2
         WHERE id = ?3 AND supervisor_id = ?4 AND last_sequence = ?5
           AND state IN ('starting', 'running')",
        params![
            sequence,
            occurred_at_unix_ms,
            execution_id.as_str(),
            supervisor_id,
            previous,
        ],
    )?;
    require_fenced_change(changed, execution_id)
}

fn insert_metadata_event(
    tx: &rusqlite::Transaction<'_>,
    execution_id: &ExecutionId,
    sequence: u64,
    kind: &str,
    occurred_at_unix_ms: i64,
) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO execution_events
         (execution_id, sequence, kind, channel, spool_offset, byte_length,
          bounded_preview_json, occurred_at_ms, safe_digest)
         VALUES (?1, ?2, ?3, 'lifecycle', NULL, 0, NULL, ?4, ?5)",
        params![
            execution_id.as_str(),
            sequence,
            kind,
            occurred_at_unix_ms,
            sha256_digest(format!("{execution_id}:{sequence}:{kind}").as_bytes()),
        ],
    )?;
    Ok(())
}

fn require_fenced_change(changed: usize, execution_id: &ExecutionId) -> StoreResult<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::LeaseLost {
            resource: format!("execution {execution_id}"),
        })
    }
}

fn owner_columns(owner: &ExecutionOwner) -> (&'static str, Option<&str>, Option<&str>, &str) {
    match owner {
        ExecutionOwner::Session {
            session_id,
            root_run_id,
        } => (
            "session",
            Some(session_id.as_str()),
            None,
            root_run_id.as_str(),
        ),
        ExecutionOwner::Run {
            run_id,
            root_run_id,
        } => ("run", None, Some(run_id.as_str()), root_run_id.as_str()),
    }
}

fn terminal_columns(size: Option<TerminalSize>) -> (Option<u16>, Option<u16>) {
    size.map_or((None, None), |size| (Some(size.columns), Some(size.rows)))
}

fn exit_columns(
    exit: Option<&ExecutionExit>,
) -> (Option<&'static str>, Option<i32>, Option<i32>, Option<&str>) {
    match exit {
        None => (None, None, None, None),
        Some(ExecutionExit::Code { code }) => (Some("code"), Some(*code), None, None),
        Some(ExecutionExit::Signal { signal }) => (Some("signal"), None, Some(*signal), None),
        Some(ExecutionExit::Error { code }) => (Some("error"), None, None, Some(code)),
    }
}

fn execution_kind(value: ExecutionKind) -> &'static str {
    match value {
        ExecutionKind::Argv => "argv",
        ExecutionKind::Shell => "shell",
    }
}

fn execution_state(value: ExecutionState) -> &'static str {
    match value {
        ExecutionState::Admitting => "admitting",
        ExecutionState::Starting => "starting",
        ExecutionState::Running => "running",
        ExecutionState::Exited => "exited",
        ExecutionState::Signalled => "signalled",
        ExecutionState::Cancelled => "cancelled",
        ExecutionState::TimedOut => "timed_out",
        ExecutionState::Failed => "failed",
        ExecutionState::OutcomeUnknown => "outcome_unknown",
    }
}

fn parse_execution_state(value: &str) -> StoreResult<ExecutionState> {
    match value {
        "admitting" => Ok(ExecutionState::Admitting),
        "starting" => Ok(ExecutionState::Starting),
        "running" => Ok(ExecutionState::Running),
        "exited" => Ok(ExecutionState::Exited),
        "signalled" => Ok(ExecutionState::Signalled),
        "cancelled" => Ok(ExecutionState::Cancelled),
        "timed_out" => Ok(ExecutionState::TimedOut),
        "failed" => Ok(ExecutionState::Failed),
        "outcome_unknown" => Ok(ExecutionState::OutcomeUnknown),
        _ => Err(invalid_store_value(
            "executions.state",
            value,
            "unknown execution state",
        )),
    }
}

fn execution_profile(value: ExecutionProfile) -> &'static str {
    match value {
        ExecutionProfile::Workspace => "workspace",
        ExecutionProfile::Host => "host",
    }
}

fn parse_execution_profile(value: &str) -> StoreResult<ExecutionProfile> {
    match value {
        "workspace" => Ok(ExecutionProfile::Workspace),
        "host" => Ok(ExecutionProfile::Host),
        _ => Err(invalid_store_value(
            "executions.profile",
            value,
            "unknown execution profile",
        )),
    }
}

fn execution_io(value: ExecutionIo) -> &'static str {
    match value {
        ExecutionIo::Pipes => "pipes",
        ExecutionIo::Pty => "pty",
    }
}

fn parse_execution_io(value: &str) -> StoreResult<ExecutionIo> {
    match value {
        "pipes" => Ok(ExecutionIo::Pipes),
        "pty" => Ok(ExecutionIo::Pty),
        _ => Err(invalid_store_value(
            "executions.io",
            value,
            "unknown execution I/O mode",
        )),
    }
}

fn execution_channel(value: ExecutionChannel) -> &'static str {
    match value {
        ExecutionChannel::Stdout => "stdout",
        ExecutionChannel::Stderr => "stderr",
        ExecutionChannel::Terminal => "terminal",
        ExecutionChannel::Lifecycle => "lifecycle",
    }
}

fn parse_execution_channel(value: &str) -> StoreResult<ExecutionChannel> {
    match value {
        "stdout" => Ok(ExecutionChannel::Stdout),
        "stderr" => Ok(ExecutionChannel::Stderr),
        "terminal" => Ok(ExecutionChannel::Terminal),
        "lifecycle" => Ok(ExecutionChannel::Lifecycle),
        _ => Err(invalid_store_value(
            "execution_events.channel",
            value,
            "unknown execution output channel",
        )),
    }
}

fn validate_event_kind(kind: &str) -> ProcessResult<()> {
    if kind.is_empty()
        || kind.len() > 64
        || !kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "execution lifecycle event kind is invalid",
        ));
    }
    Ok(())
}

fn parse_execution_id(value: &str, field: &'static str) -> StoreResult<ExecutionId> {
    ExecutionId::parse(value).map_err(|_| invalid_store_value(field, value, "invalid execution ID"))
}

fn parse_run_id(value: &str, field: &'static str) -> StoreResult<RunId> {
    RunId::parse(value).map_err(|_| invalid_store_value(field, value, "invalid run ID"))
}

fn parse_session_id(value: &str, field: &'static str) -> StoreResult<SessionId> {
    SessionId::parse(value).map_err(|_| invalid_store_value(field, value, "invalid session ID"))
}

#[allow(dead_code)]
fn parse_step_id(value: &str, field: &'static str) -> StoreResult<StepId> {
    StepId::parse(value).map_err(|_| invalid_store_value(field, value, "invalid step ID"))
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

fn store_error(error: StoreError) -> ProcessError {
    let code = match error {
        StoreError::LeaseLost { .. } | StoreError::TransitionRejected { .. } => {
            ProcessErrorCode::StateConflict
        }
        StoreError::NotFound { .. } => ProcessErrorCode::ExecutionNotFound,
        _ => ProcessErrorCode::StoreCorrupt,
    };
    ProcessError::new(code, error.to_string())
}

fn json_process_error(error: serde_json::Error) -> ProcessError {
    ProcessError::new(
        ProcessErrorCode::StoreCorrupt,
        format!("failed to encode execution metadata: {error}"),
    )
}

fn integer_process_error() -> ProcessError {
    ProcessError::new(
        ProcessErrorCode::StoreCorrupt,
        "execution metadata integer is out of range",
    )
}

fn sha256_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(bytes);
    let mut rendered = String::with_capacity(71);
    rendered.push_str("sha256:");
    for byte in digest {
        write!(&mut rendered, "{byte:02x}").expect("writing to a String cannot fail");
    }
    rendered
}

fn unix_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}
