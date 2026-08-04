use crate::{
    CommittedOutputFrame, ExecutionId, ExecutionListFilter, ExecutionOutputChunk,
    ExecutionPrivateCommand, ExecutionRequest, ExecutionStatus, ExecutionTerminalUpdate,
    InputLease, Result, TerminalSize,
};

/// Persistence-neutral durable execution metadata. Implementations fence
/// transitions so state is monotonic and output metadata is never published
/// before its private spool frame is durable.
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
