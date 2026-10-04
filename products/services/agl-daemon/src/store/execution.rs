use super::{Result, StoreHandle};
use crate::execution::{ExecutionFsm, ExecutionInput};
use agl_core::AgentRunId;
use agl_core::Fsm;
use agl_core::agent::AgentOperationKey;
use agl_core::execution::{
    ExecutionId, ExecutionOwner, ExecutionStartRequest, ExecutionStatus, ExecutionStopCause,
};
use rusqlite::{OptionalExtension, params};

pub(super) fn create_schema(connection: &rusqlite::Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS executions (
        id TEXT PRIMARY KEY, operation TEXT UNIQUE, run_id TEXT,
        request_json TEXT NOT NULL CHECK(json_valid(request_json)),
        status_json TEXT NOT NULL CHECK(json_valid(status_json)),
        stop_cause TEXT
    ); CREATE TABLE IF NOT EXISTS execution_cancelled_runs (run_id TEXT PRIMARY KEY);",
    )?;
    Ok(())
}
impl StoreHandle {
    pub(crate) fn reserve_execution(
        &self,
        operation: Option<AgentOperationKey>,
        request: &ExecutionStartRequest,
        initial: &ExecutionStatus,
    ) -> Result<(ExecutionId, bool)> {
        let operation = operation.map(|key| format!("{}:{}", key.run_id, key.ordinal));
        self.lock()?.transaction(|tx| {
            if let Some(operation) = &operation
                && let Some(id) = tx.query_row("SELECT id FROM executions WHERE operation=?", [operation], |row| row.get::<_,String>(0)).optional()? {
                return Ok((id.parse().map_err(|_| super::StoreError::NotFound { resource: id })?, false));
            }
            let run_id = match request.owner { ExecutionOwner::Agent {run_id, ..} => Some(run_id.to_string()), _ => None };
            if let Some(run_id) = &run_id
                && tx.query_row("SELECT 1 FROM execution_cancelled_runs WHERE run_id=?", [run_id], |_| Ok(())).optional()?.is_some() {
                return Err(super::StoreError::TransitionRejected {resource: run_id.clone(), from: "cancelled".into(), to: "launch".into()});
            }
            tx.execute("INSERT INTO executions(id,operation,run_id,request_json,status_json) VALUES (?,?,?,?,?)", params![initial.execution_id.to_string(),operation,run_id,serde_json::to_string(request)?,serde_json::to_string(initial)?])?;
            Ok((initial.execution_id,true))
        })
    }
    pub(crate) fn execution(
        &self,
        id: ExecutionId,
    ) -> Result<(
        ExecutionStartRequest,
        ExecutionStatus,
        Option<ExecutionStopCause>,
    )> {
        self.read(|conn| {
            let (request, status, cause): (String, String, Option<String>) = conn.query_row(
                "SELECT request_json,status_json,stop_cause FROM executions WHERE id=?",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            Ok((
                serde_json::from_str(&request)?,
                serde_json::from_str(&status)?,
                cause
                    .map(|value| serde_json::from_str(&value))
                    .transpose()?,
            ))
        })
    }
    pub(crate) fn apply_execution_input(
        &self,
        id: ExecutionId,
        input: ExecutionInput,
    ) -> Result<ExecutionStatus> {
        self.lock()?.transaction(|tx| {
            let saved: String = tx.query_row(
                "SELECT status_json FROM executions WHERE id=?",
                [id.to_string()],
                |row| row.get(0),
            )?;
            let saved: ExecutionStatus = serde_json::from_str(&saved)?;
            let next = ExecutionFsm
                .transition(&saved, input)
                .map_err(|reason| super::StoreError::TransitionRejected {
                    resource: id.to_string(),
                    from: format!("{:?}", saved.state),
                    to: reason.into(),
                })?
                .state;
            tx.execute(
                "UPDATE executions SET status_json=? WHERE id=?",
                params![serde_json::to_string(&next)?, next.execution_id.to_string()],
            )?;
            Ok(next)
        })
    }
    pub(crate) fn execution_stop_cause(
        &self,
        id: ExecutionId,
        cause: ExecutionStopCause,
    ) -> Result<()> {
        self.lock()?.conn.execute(
            "UPDATE executions SET stop_cause=COALESCE(stop_cause,?) WHERE id=?",
            params![serde_json::to_string(&cause)?, id.to_string()],
        )?;
        Ok(())
    }
    pub(crate) fn cancel_execution_run(&self, run_id: AgentRunId) -> Result<Vec<ExecutionId>> {
        self.lock()?.transaction(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO execution_cancelled_runs VALUES (?)",
                [run_id.to_string()],
            )?;
            let mut query = tx.prepare("SELECT id FROM executions WHERE run_id=?")?;
            let ids = query
                .query_map([run_id.to_string()], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            ids.into_iter()
                .map(|id| {
                    id.parse()
                        .map_err(|_| super::StoreError::NotFound { resource: id })
                })
                .collect()
        })
    }
}
