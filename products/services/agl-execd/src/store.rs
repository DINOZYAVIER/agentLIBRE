use std::path::Path;
use std::sync::{Arc, Mutex};

use agl_execution_api::{
    ExecutionId, ExecutionOutcome, ExecutionOutput, ExecutionOutputChunk, ExecutionOutputStream,
    ExecutionOwner, ExecutionState, ExecutionStatus, TerminalId,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, thiserror::Error)]
pub(crate) enum ExecutionStoreLookupError {
    #[error("execution not found")]
    ExecutionNotFound,
    #[error("terminal not found")]
    TerminalNotFound,
}

#[derive(Clone)]
pub(crate) struct ExecutionStore {
    connection: Arc<Mutex<Connection>>,
}

impl ExecutionStore {
    pub(crate) fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("failed to create {}", root.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = root.join("executions.sqlite3");
        let connection = Connection::open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS executions (
               id TEXT PRIMARY KEY,
               terminal_id TEXT UNIQUE,
               owner_json TEXT NOT NULL,
               state TEXT NOT NULL,
               outcome_json TEXT,
               output_bytes INTEGER NOT NULL DEFAULT 0,
               output_limit INTEGER NOT NULL,
               output_truncated INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS execution_output (
               execution_id TEXT NOT NULL REFERENCES executions(id) ON DELETE CASCADE,
               start_offset INTEGER NOT NULL,
               end_offset INTEGER NOT NULL,
               stream TEXT NOT NULL,
               data BLOB NOT NULL,
               PRIMARY KEY (execution_id, start_offset)
             );",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
        };
        store.recover_interrupted()?;
        Ok(store)
    }

    fn recover_interrupted(&self) -> Result<()> {
        let outcome = serde_json::to_string(&ExecutionOutcome::UnknownAfterServiceRestart)?;
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("execution store lock is poisoned"))?
            .execute(
                "UPDATE executions
                 SET state = 'outcome_unknown', outcome_json = ?1
                 WHERE state = 'running'",
                [outcome],
            )?;
        Ok(())
    }

    pub(crate) fn insert(
        &self,
        execution_id: ExecutionId,
        terminal_id: Option<TerminalId>,
        owner: &ExecutionOwner,
        output_limit: u64,
    ) -> Result<()> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("execution store lock is poisoned"))?
            .execute(
                "INSERT INTO executions
                 (id, terminal_id, owner_json, state, output_limit)
                 VALUES (?1, ?2, ?3, 'running', ?4)",
                params![
                    execution_id.to_string(),
                    terminal_id.map(|id| id.to_string()),
                    serde_json::to_string(owner)?,
                    i64::try_from(output_limit)?,
                ],
            )?;
        Ok(())
    }

    pub(crate) fn append(
        &self,
        execution_id: ExecutionId,
        stream: ExecutionOutputStream,
        bytes: &[u8],
    ) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("execution store lock is poisoned"))?;
        let transaction = connection.transaction()?;
        let (output_bytes, limit, truncated): (u64, u64, bool) = transaction.query_row(
            "SELECT output_bytes, output_limit, output_truncated FROM executions WHERE id = ?1",
            [execution_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let remaining = usize::try_from(limit.saturating_sub(output_bytes)).unwrap_or(usize::MAX);
        let accepted = bytes.len().min(remaining);
        let now_truncated = truncated || bytes.len() > remaining;
        if accepted > 0 {
            let end = output_bytes.saturating_add(accepted as u64);
            transaction.execute(
                "INSERT INTO execution_output
                 (execution_id, start_offset, end_offset, stream, data)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    execution_id.to_string(),
                    i64::try_from(output_bytes)?,
                    i64::try_from(end)?,
                    match stream {
                        ExecutionOutputStream::Stdout => "stdout",
                        ExecutionOutputStream::Stderr => "stderr",
                        ExecutionOutputStream::Pty => "pty",
                    },
                    &bytes[..accepted],
                ],
            )?;
        }
        let new_output_bytes = output_bytes.saturating_add(accepted as u64);
        transaction.execute(
            "UPDATE executions
             SET output_bytes = ?1, output_truncated = ?2
             WHERE id = ?3",
            params![
                i64::try_from(new_output_bytes)?,
                now_truncated,
                execution_id.to_string(),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn finish(
        &self,
        execution_id: ExecutionId,
        outcome: &ExecutionOutcome,
    ) -> Result<()> {
        let changed = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("execution store lock is poisoned"))?
            .execute(
                "UPDATE executions SET state = 'exited', outcome_json = ?1
                 WHERE id = ?2 AND state = 'running'",
                params![serde_json::to_string(outcome)?, execution_id.to_string()],
            )?;
        ensure!(changed == 1, "execution is not running");
        Ok(())
    }

    pub(crate) fn status(&self, execution_id: ExecutionId) -> Result<ExecutionStatus> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("execution store lock is poisoned"))?
            .query_row(
                "SELECT terminal_id, owner_json, state, outcome_json,
                        output_bytes, output_truncated
                 FROM executions WHERE id = ?1",
                [execution_id.to_string()],
                |row| {
                    let terminal_id: Option<String> = row.get(0)?;
                    let owner: String = row.get(1)?;
                    let state: String = row.get(2)?;
                    let outcome: Option<String> = row.get(3)?;
                    Ok((terminal_id, owner, state, outcome, row.get(4)?, row.get(5)?))
                },
            )
            .optional()?
            .map(
                |(terminal_id, owner, state, outcome, output_bytes, output_truncated)| {
                    Ok(ExecutionStatus {
                        execution_id,
                        terminal_id: terminal_id
                            .map(|value| value.parse())
                            .transpose()
                            .map_err(anyhow::Error::msg)?,
                        owner: serde_json::from_str(&owner)?,
                        state: match state.as_str() {
                            "running" => ExecutionState::Running,
                            "exited" => ExecutionState::Exited,
                            "outcome_unknown" => ExecutionState::OutcomeUnknown,
                            _ => anyhow::bail!("invalid stored execution state"),
                        },
                        outcome: outcome
                            .map(|value| serde_json::from_str(&value))
                            .transpose()?,
                        output_bytes,
                        output_truncated,
                    })
                },
            )
            .transpose()?
            .ok_or_else(|| ExecutionStoreLookupError::ExecutionNotFound.into())
    }

    pub(crate) fn execution_for_terminal(&self, terminal_id: TerminalId) -> Result<ExecutionId> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("execution store lock is poisoned"))?
            .query_row(
                "SELECT id FROM executions WHERE terminal_id = ?1",
                [terminal_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| value.parse().map_err(anyhow::Error::msg))
            .transpose()?
            .ok_or_else(|| ExecutionStoreLookupError::TerminalNotFound.into())
    }

    pub(crate) fn read(
        &self,
        execution_id: ExecutionId,
        after: u64,
        max_bytes: u32,
    ) -> Result<ExecutionOutput> {
        let status = self.status(execution_id)?;
        let start = after.min(status.output_bytes);
        let end = start
            .saturating_add(u64::from(max_bytes))
            .min(status.output_bytes);
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("execution store lock is poisoned"))?;
        let mut statement = connection.prepare(
            "SELECT start_offset, end_offset, stream, data
             FROM execution_output
             WHERE execution_id = ?1 AND end_offset > ?2 AND start_offset < ?3
             ORDER BY start_offset",
        )?;
        let rows = statement.query_map(
            params![
                execution_id.to_string(),
                i64::try_from(start)?,
                i64::try_from(end)?
            ],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            },
        )?;
        let mut chunks = Vec::new();
        for row in rows {
            let (chunk_start, chunk_end, stream, data) = row?;
            let from = usize::try_from(start.saturating_sub(chunk_start)).unwrap_or(usize::MAX);
            let to = usize::try_from(end.min(chunk_end).saturating_sub(chunk_start))
                .unwrap_or(usize::MAX);
            if from < to && to <= data.len() {
                chunks.push(ExecutionOutputChunk {
                    stream: match stream.as_str() {
                        "stdout" => ExecutionOutputStream::Stdout,
                        "stderr" => ExecutionOutputStream::Stderr,
                        "pty" => ExecutionOutputStream::Pty,
                        _ => anyhow::bail!("invalid stored output stream"),
                    },
                    data: data[from..to].to_vec(),
                });
            }
        }
        Ok(ExecutionOutput {
            execution_id,
            after: start,
            next: end,
            chunks,
            eof: status.state != ExecutionState::Running && end == status.output_bytes,
            truncated: status.output_truncated,
        })
    }
}
