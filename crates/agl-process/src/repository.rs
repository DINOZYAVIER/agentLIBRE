use std::collections::BTreeMap;
use std::sync::Mutex;

use agl_ids::{ExecutionId, WriterLeaseId};

use crate::{
    ExecutionChannel, ExecutionExit, ExecutionListFilter, ExecutionOutputChunk,
    ExecutionPrivateCommand, ExecutionRequest, ExecutionState, ExecutionStatus, InputLease,
    ProcessError, ProcessErrorCode, Result, TerminalSize,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedOutputFrame {
    pub sequence: u64,
    pub channel: ExecutionChannel,
    pub spool_offset: u64,
    pub byte_length: u64,
    pub safe_digest: String,
}

impl CommittedOutputFrame {
    pub fn from_chunk(chunk: &ExecutionOutputChunk, spool_offset: u64) -> Result<Self> {
        let payload = chunk.bytes.decode(usize::MAX)?;
        Ok(Self {
            sequence: chunk.sequence,
            channel: chunk.channel,
            spool_offset,
            byte_length: payload.len() as u64,
            safe_digest: sha256_digest(&payload),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionTerminalUpdate {
    pub state: ExecutionState,
    pub exit: Option<ExecutionExit>,
    pub error_code: Option<String>,
    pub finished_at_unix_ms: i64,
    pub output_truncated: bool,
    pub discarded_output_bytes: u64,
}

impl ExecutionTerminalUpdate {
    pub fn validate(&self) -> Result<()> {
        if !self.state.is_terminal() {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "execution terminal update requires a terminal state",
            ));
        }
        Ok(())
    }
}

/// Persistence-neutral durable execution metadata used by `ProcessSupervisor`.
/// Implementations fence transitions so state is monotonic and output sequence
/// metadata cannot be published before its private spool frame is durable.
pub trait ExecutionRepository: Send + Sync {
    fn admit(
        &self,
        status: &ExecutionStatus,
        request: &ExecutionRequest,
        supervisor_id: &str,
    ) -> Result<()>;

    fn mark_running(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        started_at_unix_ms: i64,
    ) -> Result<()>;

    fn append_lifecycle(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        sequence: u64,
        kind: &str,
        occurred_at_unix_ms: i64,
    ) -> Result<()>;

    fn append_indexed_chunk(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        chunk: &ExecutionOutputChunk,
        spool_offset: u64,
        byte_length: u64,
        occurred_at_unix_ms: i64,
    ) -> Result<()>;

    fn update_terminal_size(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        sequence: u64,
        terminal_size: TerminalSize,
        occurred_at_unix_ms: i64,
    ) -> Result<()>;

    fn bind_input_lease(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        lease: &InputLease,
        occurred_at_unix_ms: i64,
    ) -> Result<()>;

    fn release_input_lease(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        lease: &InputLease,
        occurred_at_unix_ms: i64,
    ) -> Result<()>;

    fn renew_input_lease(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        lease: &InputLease,
        occurred_at_unix_ms: i64,
    ) -> Result<()>;

    fn accept_input(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        lease: &InputLease,
        byte_length: u64,
        eof: bool,
        occurred_at_unix_ms: i64,
    ) -> Result<()>;

    fn mark_terminal(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        sequence: u64,
        update: &ExecutionTerminalUpdate,
    ) -> Result<()>;

    fn status(&self, execution_id: &ExecutionId) -> Result<ExecutionStatus>;

    fn private_command(
        &self,
        execution_id: &ExecutionId,
        maximum_bytes: usize,
    ) -> Result<ExecutionPrivateCommand>;

    fn list(&self, filter: &ExecutionListFilter) -> Result<Vec<ExecutionStatus>>;

    fn committed_output_frames(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Vec<CommittedOutputFrame>>;

    /// Marks live rows from any previous owner as unknown before this owner can
    /// admit work. Returns the affected execution identities.
    fn recover_prior_owners(
        &self,
        current_supervisor_id: &str,
        recovered_at_unix_ms: i64,
    ) -> Result<Vec<ExecutionId>>;

    fn output_retention_candidates(
        &self,
        now_unix_ms: i64,
        limit: usize,
    ) -> Result<Vec<ExecutionId>>;

    fn tombstone_output(
        &self,
        execution_id: &ExecutionId,
        tombstoned_at_unix_ms: i64,
    ) -> Result<()>;

    fn mark_output_expired(
        &self,
        execution_id: &ExecutionId,
        expired_at_unix_ms: i64,
    ) -> Result<()>;
}

/// Private bounded byte storage. Implementations append and sync before the
/// repository publishes matching metadata.
pub trait OutputSpool: Send + Sync {
    fn prepare(&self, execution_id: &ExecutionId) -> Result<()>;
    fn append(&self, execution_id: &ExecutionId, chunk: &ExecutionOutputChunk) -> Result<u64>;
    fn sync(&self, execution_id: &ExecutionId) -> Result<()>;
    fn read(
        &self,
        execution_id: &ExecutionId,
        after_sequence: u64,
        through_sequence: u64,
        maximum_bytes: usize,
    ) -> Result<Vec<ExecutionOutputChunk>>;
    fn recover(&self, execution_id: &ExecutionId, committed: &[CommittedOutputFrame])
    -> Result<()>;
    fn remove(&self, execution_id: &ExecutionId) -> Result<()>;
}

#[derive(Default)]
pub struct InMemoryExecutionRepository {
    inner: Mutex<InMemoryState>,
    #[cfg(test)]
    fail_output_after_commit: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    fail_terminal_after_commit: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    fail_running_after_commit: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    fail_lifecycle_after_commit: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct InMemoryState {
    records: BTreeMap<ExecutionId, InMemoryRecord>,
}

struct InMemoryRecord {
    status: ExecutionStatus,
    request: ExecutionRequest,
    supervisor_id: String,
    writer_lease_id: Option<WriterLeaseId>,
    accepted_input_bytes: u64,
    committed_output: Vec<CommittedOutputFrame>,
}

impl InMemoryExecutionRepository {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn admitted_request(&self, execution_id: &ExecutionId) -> Result<ExecutionRequest> {
        self.inner
            .lock()
            .map_err(|_| {
                ProcessError::new(
                    ProcessErrorCode::Internal,
                    "in-memory execution repository lock is poisoned",
                )
            })?
            .records
            .get(execution_id)
            .map(|record| record.request.clone())
            .ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::ExecutionNotFound,
                    format!("execution `{execution_id}` was not found"),
                )
            })
    }

    #[cfg(test)]
    pub(crate) fn fail_next_output_after_commit(&self) {
        self.fail_output_after_commit
            .store(true, std::sync::atomic::Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_terminal_after_commit(&self) {
        self.fail_terminal_after_commit
            .store(true, std::sync::atomic::Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_running_after_commit(&self) {
        self.fail_running_after_commit
            .store(true, std::sync::atomic::Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_lifecycle_after_commit(&self) {
        self.fail_lifecycle_after_commit
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn with_record<T>(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        operation: impl FnOnce(&mut InMemoryRecord) -> Result<T>,
    ) -> Result<T> {
        let mut state = self.inner.lock().map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::Internal,
                "in-memory execution repository lock is poisoned",
            )
        })?;
        let record = state.records.get_mut(execution_id).ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::ExecutionNotFound,
                format!("execution `{execution_id}` was not found"),
            )
        })?;
        if record.supervisor_id != supervisor_id {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "execution transition was attempted by a stale supervisor owner",
            ));
        }
        operation(record)
    }
}

impl ExecutionRepository for InMemoryExecutionRepository {
    fn admit(
        &self,
        status: &ExecutionStatus,
        request: &ExecutionRequest,
        supervisor_id: &str,
    ) -> Result<()> {
        let accepted_input_bytes = request
            .stdin
            .as_ref()
            .map(|stdin| {
                usize::try_from(request.limits.max_input_bytes)
                    .map_err(|_| {
                        ProcessError::new(
                            ProcessErrorCode::InvalidRequest,
                            "execution input limit does not fit this platform",
                        )
                    })
                    .and_then(|maximum| stdin.decode(maximum))
                    .map(|bytes| bytes.len() as u64)
            })
            .transpose()?
            .unwrap_or(0);
        let mut state = self.inner.lock().map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::Internal,
                "execution repository lock is poisoned",
            )
        })?;
        if state.records.contains_key(&status.execution_id) {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "execution identity is already admitted",
            ));
        }
        state.records.insert(
            status.execution_id.clone(),
            InMemoryRecord {
                status: status.clone(),
                request: request.clone(),
                supervisor_id: supervisor_id.to_owned(),
                writer_lease_id: None,
                accepted_input_bytes,
                committed_output: Vec::new(),
            },
        );
        Ok(())
    }

    fn mark_running(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        started_at_unix_ms: i64,
    ) -> Result<()> {
        self.with_record(execution_id, supervisor_id, |record| {
            if !matches!(
                record.status.state,
                ExecutionState::Admitting | ExecutionState::Starting
            ) {
                return Err(ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "only an admitting or starting execution can become running",
                ));
            }
            record.status.state = ExecutionState::Running;
            record.status.started_at_unix_ms = Some(started_at_unix_ms);
            Ok(())
        })?;
        #[cfg(test)]
        if self
            .fail_running_after_commit
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(ProcessError::new(
                ProcessErrorCode::Internal,
                "injected running metadata post-commit failure",
            ));
        }
        Ok(())
    }

    fn append_lifecycle(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        sequence: u64,
        _kind: &str,
        _occurred_at_unix_ms: i64,
    ) -> Result<()> {
        self.with_record(execution_id, supervisor_id, |record| {
            require_next_sequence(&record.status, sequence)?;
            record.status.last_sequence = sequence;
            Ok(())
        })?;
        #[cfg(test)]
        if self
            .fail_lifecycle_after_commit
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(ProcessError::new(
                ProcessErrorCode::Internal,
                "injected lifecycle metadata post-commit failure",
            ));
        }
        Ok(())
    }

    fn append_indexed_chunk(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        chunk: &ExecutionOutputChunk,
        spool_offset: u64,
        byte_length: u64,
        _occurred_at_unix_ms: i64,
    ) -> Result<()> {
        self.with_record(execution_id, supervisor_id, |record| {
            require_next_sequence(&record.status, chunk.sequence)?;
            let committed = CommittedOutputFrame::from_chunk(chunk, spool_offset)?;
            if committed.byte_length != byte_length {
                return Err(ProcessError::new(
                    ProcessErrorCode::StoreCorrupt,
                    "indexed output byte length does not match the durable spool frame",
                ));
            }
            record.status.last_sequence = chunk.sequence;
            record.committed_output.push(committed);
            record
                .status
                .first_retained_sequence
                .get_or_insert(chunk.sequence);
            record.status.retained_bytes = record.status.retained_bytes.saturating_add(byte_length);
            Ok(())
        })?;
        #[cfg(test)]
        if self
            .fail_output_after_commit
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(ProcessError::new(
                ProcessErrorCode::Internal,
                "injected output metadata post-commit failure",
            ));
        }
        Ok(())
    }

    fn update_terminal_size(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        sequence: u64,
        terminal_size: TerminalSize,
        _occurred_at_unix_ms: i64,
    ) -> Result<()> {
        self.with_record(execution_id, supervisor_id, |record| {
            require_next_sequence(&record.status, sequence)?;
            record.status.terminal_size = Some(terminal_size);
            record.status.last_sequence = sequence;
            Ok(())
        })
    }

    fn bind_input_lease(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        lease: &InputLease,
        _occurred_at_unix_ms: i64,
    ) -> Result<()> {
        self.with_record(execution_id, supervisor_id, |record| {
            let writer_lease_id = lease.writer_lease_id().ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::InvalidRequest,
                    "only writable attachments can bind the input lease",
                )
            })?;
            if record.writer_lease_id.is_some() {
                return Err(ProcessError::new(
                    ProcessErrorCode::InputLeaseBusy,
                    "execution already has a writable input lease",
                ));
            }
            record.writer_lease_id = Some(writer_lease_id.clone());
            Ok(())
        })
    }

    fn release_input_lease(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        lease: &InputLease,
        _occurred_at_unix_ms: i64,
    ) -> Result<()> {
        self.with_record(execution_id, supervisor_id, |record| {
            if record.writer_lease_id.as_ref() != lease.writer_lease_id() {
                return Err(ProcessError::new(
                    ProcessErrorCode::InputLeaseBusy,
                    "input lease is not owned by this attachment",
                ));
            }
            record.writer_lease_id = None;
            Ok(())
        })
    }

    fn renew_input_lease(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        lease: &InputLease,
        _occurred_at_unix_ms: i64,
    ) -> Result<()> {
        self.with_record(execution_id, supervisor_id, |record| {
            if record.writer_lease_id.as_ref() != lease.writer_lease_id() {
                return Err(ProcessError::new(
                    ProcessErrorCode::InputLeaseBusy,
                    "input lease is not owned by this attachment",
                ));
            }
            Ok(())
        })
    }

    fn accept_input(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        lease: &InputLease,
        byte_length: u64,
        _eof: bool,
        _occurred_at_unix_ms: i64,
    ) -> Result<()> {
        self.with_record(execution_id, supervisor_id, |record| {
            if record.writer_lease_id.as_ref() != lease.writer_lease_id() {
                return Err(ProcessError::new(
                    ProcessErrorCode::InputLeaseBusy,
                    "input lease is not owned by this attachment",
                ));
            }
            record.accepted_input_bytes = record.accepted_input_bytes.saturating_add(byte_length);
            Ok(())
        })
    }

    fn mark_terminal(
        &self,
        execution_id: &ExecutionId,
        supervisor_id: &str,
        sequence: u64,
        update: &ExecutionTerminalUpdate,
    ) -> Result<()> {
        update.validate()?;
        self.with_record(execution_id, supervisor_id, |record| {
            if record.status.state.is_terminal() {
                return Err(ProcessError::new(
                    ProcessErrorCode::StateConflict,
                    "execution is already terminal",
                ));
            }
            require_next_sequence(&record.status, sequence)?;
            record.status.state = update.state;
            record.status.exit = update.exit.clone();
            record.status.error_code = update.error_code.clone();
            record.status.finished_at_unix_ms = Some(update.finished_at_unix_ms);
            record.status.output_truncated |= update.output_truncated;
            record.status.discarded_output_bytes = record
                .status
                .discarded_output_bytes
                .saturating_add(update.discarded_output_bytes);
            record.status.last_sequence = sequence;
            record.writer_lease_id = None;
            Ok(())
        })?;
        #[cfg(test)]
        if self
            .fail_terminal_after_commit
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(ProcessError::new(
                ProcessErrorCode::Internal,
                "injected terminal metadata post-commit failure",
            ));
        }
        Ok(())
    }

    fn status(&self, execution_id: &ExecutionId) -> Result<ExecutionStatus> {
        let state = self.inner.lock().map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::Internal,
                "execution repository lock is poisoned",
            )
        })?;
        state
            .records
            .get(execution_id)
            .map(|record| record.status.clone())
            .ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::ExecutionNotFound,
                    format!("execution `{execution_id}` was not found"),
                )
            })
    }

    fn private_command(
        &self,
        execution_id: &ExecutionId,
        maximum_bytes: usize,
    ) -> Result<ExecutionPrivateCommand> {
        let state = self.inner.lock().map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::Internal,
                "execution repository lock is poisoned",
            )
        })?;
        let record = state.records.get(execution_id).ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::ExecutionNotFound,
                format!("execution `{execution_id}` was not found"),
            )
        })?;
        ExecutionPrivateCommand::from_argv(
            &record.request.program,
            &record.request.args,
            maximum_bytes,
        )
    }

    fn list(&self, filter: &ExecutionListFilter) -> Result<Vec<ExecutionStatus>> {
        let state = self.inner.lock().map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::Internal,
                "execution repository lock is poisoned",
            )
        })?;
        Ok(state
            .records
            .values()
            .map(|record| &record.status)
            .filter(|status| filter.include_finished || !status.state.is_terminal())
            .filter(|status| {
                filter.session_id.as_ref().is_none_or(|expected| {
                    matches!(
                        &status.owner,
                        crate::ExecutionOwner::Session { session_id, .. } if session_id == expected
                    )
                })
            })
            .filter(|status| {
                filter
                    .root_run_id
                    .as_ref()
                    .is_none_or(|expected| status.owner.root_run_id() == expected)
            })
            .cloned()
            .collect())
    }

    fn committed_output_frames(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Vec<CommittedOutputFrame>> {
        let state = self.inner.lock().map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::Internal,
                "execution repository lock is poisoned",
            )
        })?;
        state
            .records
            .get(execution_id)
            .map(|record| record.committed_output.clone())
            .ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::ExecutionNotFound,
                    format!("execution `{execution_id}` was not found"),
                )
            })
    }

    fn recover_prior_owners(
        &self,
        current_supervisor_id: &str,
        recovered_at_unix_ms: i64,
    ) -> Result<Vec<ExecutionId>> {
        let mut state = self.inner.lock().map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::Internal,
                "execution repository lock is poisoned",
            )
        })?;
        let mut recovered = Vec::new();
        for (execution_id, record) in &mut state.records {
            if record.supervisor_id != current_supervisor_id && record.status.state.is_live() {
                record.status.state = ExecutionState::OutcomeUnknown;
                record.status.finished_at_unix_ms = Some(recovered_at_unix_ms);
                record.status.error_code = Some("owner_lost".to_owned());
                record.status.last_sequence = record.status.last_sequence.saturating_add(1);
                recovered.push(execution_id.clone());
            }
        }
        Ok(recovered)
    }

    fn output_retention_candidates(
        &self,
        _now_unix_ms: i64,
        _limit: usize,
    ) -> Result<Vec<ExecutionId>> {
        Ok(Vec::new())
    }

    fn tombstone_output(
        &self,
        execution_id: &ExecutionId,
        _tombstoned_at_unix_ms: i64,
    ) -> Result<()> {
        let state = self.inner.lock().map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::Internal,
                "execution repository lock is poisoned",
            )
        })?;
        if !state.records.contains_key(execution_id) {
            return Err(ProcessError::new(
                ProcessErrorCode::ExecutionNotFound,
                format!("execution `{execution_id}` was not found"),
            ));
        }
        Ok(())
    }

    fn mark_output_expired(
        &self,
        execution_id: &ExecutionId,
        _expired_at_unix_ms: i64,
    ) -> Result<()> {
        let mut state = self.inner.lock().map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::Internal,
                "execution repository lock is poisoned",
            )
        })?;
        let record = state.records.get_mut(execution_id).ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::ExecutionNotFound,
                "execution was not found",
            )
        })?;
        record.status.output_expired = true;
        record.status.retained_bytes = 0;
        record.status.first_retained_sequence = None;
        Ok(())
    }
}

fn sha256_digest(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;

    let mut rendered = String::with_capacity(71);
    rendered.push_str("sha256:");
    for byte in Sha256::digest(bytes) {
        write!(&mut rendered, "{byte:02x}").expect("writing to a String cannot fail");
    }
    rendered
}

fn require_next_sequence(status: &ExecutionStatus, sequence: u64) -> Result<()> {
    let expected = status.last_sequence.checked_add(1).ok_or_else(|| {
        ProcessError::new(
            ProcessErrorCode::StateConflict,
            "execution sequence overflowed",
        )
    })?;
    if sequence != expected {
        return Err(ProcessError::new(
            ProcessErrorCode::StateConflict,
            format!("execution sequence {sequence} is not the expected {expected}"),
        ));
    }
    Ok(())
}
