use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use agl_events::SafeRuntimeEventEnvelope;
use agl_ids::{RunId, StepId};
use agl_kernel::{
    ChildRunAdmission, ChildRunDraft, DurableRunAdmission, DurableRunDraft, DurableRunRecord,
    IdempotentRunRecord, RecoveryReport, RunConcurrencyKey, RunLease, RunRepository,
    RunRepositoryError, RunRequestResult, RunState, RunStepDraft, RunStepRecord, RunStepState,
    RunUsage, SafeRunStatus, StepLease,
};

use crate::{AglStore, StoreError};

#[derive(Debug)]
pub struct StoreHandle {
    store: Mutex<AglStore>,
}

impl StoreHandle {
    pub fn open_at(root: impl AsRef<Path>) -> crate::Result<Self> {
        Ok(Self {
            store: Mutex::new(AglStore::open_at(root)?),
        })
    }

    pub fn open_current_at(root: impl AsRef<Path>) -> crate::Result<Self> {
        Ok(Self {
            store: Mutex::new(AglStore::open_current_at(root)?),
        })
    }

    pub fn open_current_read_only_at(root: impl AsRef<Path>) -> crate::Result<Self> {
        Ok(Self {
            store: Mutex::new(AglStore::open_current_read_only_at(root)?),
        })
    }

    pub(crate) fn lock(&self) -> crate::Result<MutexGuard<'_, AglStore>> {
        self.store.lock().map_err(|_| StoreError::InvalidValue {
            field: "store mutex",
            value: "poisoned".to_owned(),
            reason: "store operation panicked while holding the connection",
        })
    }

    pub fn health(&self) -> crate::Result<crate::StoreHealth> {
        self.lock()?.health()
    }

    pub fn status(&self) -> crate::Result<crate::StoreStatus> {
        self.lock()?.status()
    }

    pub fn domain_health(
        &self,
        domain: crate::StoreDomain,
    ) -> crate::Result<crate::StoreDomainHealth> {
        self.lock()?.domain_health(domain)
    }

    pub fn export_jsonl(
        &self,
        options: &crate::StoreExportOptions,
    ) -> crate::Result<(usize, Vec<u8>)> {
        let mut output = Vec::new();
        let records = self.lock()?.export_domain_jsonl(options, &mut output)?;
        Ok((records, output))
    }

    pub fn schema_status(&self) -> crate::Result<crate::StoreSchemaStatus> {
        let store = self.lock()?;
        let schema_version = store.schema_version()?;
        let applied_migrations = store.applied_migration_versions()?;
        Ok(crate::StoreSchemaStatus {
            database_path: store.database_path().to_path_buf(),
            database_exists: true,
            schema_version: Some(schema_version),
            current_schema_version: crate::CURRENT_SCHEMA_VERSION,
            migration_required: schema_version != crate::CURRENT_SCHEMA_VERSION
                || applied_migrations.len() != crate::STORE_MIGRATIONS.len()
                || applied_migrations.last().copied() != Some(crate::CURRENT_SCHEMA_VERSION),
            applied_migrations,
        })
    }

    pub fn migrate(&self) -> crate::Result<crate::StoreMigrationReport> {
        self.lock()?.migrate()
    }
}

impl RunRepository for StoreHandle {
    fn admit_run(&self, draft: &DurableRunDraft) -> Result<DurableRunRecord, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .admit_run(draft)
            .map_err(run_error)
    }

    fn compare_and_set_run_execution_context(
        &self,
        run_id: &RunId,
        expected_revision: u64,
        next: &agl_exec::ExecutionContextSnapshot,
    ) -> Result<agl_exec::ExecutionContextSnapshot, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .compare_and_set_run_execution_context(run_id, expected_revision, next)
            .map_err(run_error)
    }

    fn admit_run_at(
        &self,
        draft: &DurableRunDraft,
        now_ms: i64,
    ) -> Result<DurableRunRecord, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .admit_run_at(draft, now_ms)
            .map_err(run_error)
    }

    fn admit_child_run(
        &self,
        draft: &ChildRunDraft,
    ) -> Result<ChildRunAdmission, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .admit_child_run(draft)
            .map_err(run_error)
    }

    fn admit_child_run_at(
        &self,
        draft: &ChildRunDraft,
        now_ms: i64,
    ) -> Result<ChildRunAdmission, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .admit_child_run_at(draft, now_ms)
            .map_err(run_error)
    }

    fn child_run_by_spawn_step(
        &self,
        step_id: &StepId,
    ) -> Result<Option<DurableRunRecord>, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .child_run_by_spawn_step(step_id)
            .map_err(run_error)
    }

    fn run_children(
        &self,
        parent_run_id: &RunId,
    ) -> Result<Vec<DurableRunRecord>, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .run_children(parent_run_id)
            .map_err(run_error)
    }

    fn run_tree(&self, run_id: &RunId) -> Result<Vec<SafeRunStatus>, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .run_tree(run_id)
            .map_err(run_error)
    }

    fn expire_delegation_trees(
        &self,
        now_ms: i64,
    ) -> Result<Vec<SafeRunStatus>, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .expire_delegation_trees(now_ms)
            .map_err(run_error)
    }

    fn admit_idempotent_run(
        &self,
        draft: &DurableRunDraft,
        namespace: &str,
        key: &str,
        fingerprint: &str,
        owner: &str,
        lease_expires_at_ms: i64,
        now_ms: i64,
    ) -> Result<DurableRunAdmission, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .admit_idempotent_run(
                draft,
                namespace,
                key,
                fingerprint,
                owner,
                lease_expires_at_ms,
                now_ms,
            )
            .map_err(run_error)
    }

    fn idempotent_run(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<IdempotentRunRecord>, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .idempotency_record(namespace, key)
            .map(|record| {
                record.map(|record| IdempotentRunRecord {
                    namespace: record.namespace,
                    key: record.key,
                    fingerprint: record.fingerprint,
                    admitted_run_id: record.admitted_run_id,
                })
            })
            .map_err(run_error)
    }

    fn run(&self, run_id: &RunId) -> Result<Option<DurableRunRecord>, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .run(run_id)
            .map_err(run_error)
    }

    fn safe_run_status(&self, run_id: &RunId) -> Result<Option<SafeRunStatus>, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .safe_run_status(run_id)
            .map_err(run_error)
    }

    fn safe_runs_for_concurrency_key(
        &self,
        key: &RunConcurrencyKey,
        include_terminal: bool,
    ) -> Result<Vec<SafeRunStatus>, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .safe_runs_for_concurrency_key(key, include_terminal)
            .map_err(run_error)
    }

    fn claim_next_run(
        &self,
        owner: &str,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<Option<RunLease>, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .claim_next_run(owner, now_ms, lease_duration_ms)
            .map_err(run_error)
    }

    fn heartbeat_run(
        &self,
        lease: &RunLease,
        expires_at_ms: i64,
        now_ms: i64,
    ) -> Result<(), RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .heartbeat_run(lease, expires_at_ms, now_ms)
            .map_err(run_error)
    }

    fn request_run_cancellation(
        &self,
        run_id: &RunId,
        now_ms: i64,
    ) -> Result<SafeRunStatus, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .request_run_cancellation(run_id, now_ms)
            .map_err(run_error)
    }

    fn request_run_tree_cancellation(
        &self,
        run_id: &RunId,
        now_ms: i64,
    ) -> Result<Vec<SafeRunStatus>, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .request_run_tree_cancellation(run_id, now_ms)
            .map_err(run_error)
    }

    fn publish_run_step(
        &self,
        lease: &RunLease,
        checkpoint: &serde_json::Value,
        step: &RunStepDraft,
        events: &[SafeRuntimeEventEnvelope],
        now_ms: i64,
    ) -> Result<RunStepRecord, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .publish_run_step(lease, checkpoint, step, events, now_ms)
            .map_err(run_error)
    }

    fn claim_run_step(
        &self,
        run_lease: &RunLease,
        step_id: &StepId,
        expires_at_ms: i64,
        now_ms: i64,
    ) -> Result<StepLease, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .claim_run_step(run_lease, step_id, expires_at_ms, now_ms)
            .map_err(run_error)
    }

    fn complete_run_step(
        &self,
        run_lease: &RunLease,
        step_lease: &StepLease,
        state: RunStepState,
        result: Option<&RunRequestResult>,
        checkpoint: &serde_json::Value,
        usage: &RunUsage,
        events: &[SafeRuntimeEventEnvelope],
        error_code: Option<&str>,
        now_ms: i64,
    ) -> Result<RunStepRecord, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .complete_run_step(
                run_lease, step_lease, state, result, checkpoint, usage, events, error_code, now_ms,
            )
            .map_err(run_error)
    }

    fn retry_run_step(
        &self,
        run_lease: &RunLease,
        step_lease: &StepLease,
        retry_limit: u32,
        not_before_ms: i64,
        error_code: &str,
        checkpoint: &serde_json::Value,
        usage: &RunUsage,
        events: &[SafeRuntimeEventEnvelope],
        now_ms: i64,
    ) -> Result<(), RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .retry_run_step(
                run_lease,
                step_lease,
                retry_limit,
                not_before_ms,
                error_code,
                checkpoint,
                usage,
                events,
                now_ms,
            )
            .map_err(run_error)
    }

    fn finish_run(
        &self,
        lease: &RunLease,
        state: RunState,
        checkpoint: Option<&serde_json::Value>,
        usage: &RunUsage,
        terminal_result: Option<&serde_json::Value>,
        error_code: Option<&str>,
        error_message: Option<&str>,
        events: &[SafeRuntimeEventEnvelope],
        now_ms: i64,
    ) -> Result<DurableRunRecord, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .finish_run(
                lease,
                state,
                checkpoint,
                usage,
                terminal_result,
                error_code,
                error_message,
                events,
                now_ms,
            )
            .map_err(run_error)
    }

    fn run_steps(&self, run_id: &RunId) -> Result<Vec<RunStepRecord>, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .run_steps(run_id)
            .map_err(run_error)
    }

    fn run_step_by_sequence(
        &self,
        run_id: &RunId,
        request_sequence: u64,
    ) -> Result<Option<RunStepRecord>, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .run_step_by_sequence(run_id, request_sequence)
            .map_err(run_error)
    }

    fn run_events_after(
        &self,
        run_id: &RunId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<SafeRuntimeEventEnvelope>, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .run_events_after(run_id, after_sequence, limit)
            .map_err(run_error)
    }

    fn latest_run_event_sequence(&self, run_id: &RunId) -> Result<u64, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .latest_run_event_sequence(run_id)
            .map_err(run_error)
    }

    fn recover_expired_work(&self, now_ms: i64) -> Result<RecoveryReport, RunRepositoryError> {
        self.lock()
            .map_err(run_error)?
            .recover_expired_work(now_ms)
            .map_err(run_error)
    }
}

fn run_error(error: StoreError) -> RunRepositoryError {
    match error {
        StoreError::InvalidValue { field, reason, .. } => RunRepositoryError::InvalidValue {
            field,
            reason: reason.to_owned(),
        },
        StoreError::NotFound { resource } => RunRepositoryError::NotFound { resource },
        StoreError::TransitionRejected { resource, from, to } => {
            RunRepositoryError::TransitionRejected { resource, from, to }
        }
        StoreError::LeaseLost { resource } => RunRepositoryError::LeaseLost { resource },
        StoreError::IdempotencyConflict { namespace, key, .. } => {
            RunRepositoryError::IdempotencyConflict { namespace, key }
        }
        StoreError::DelegationDenied { code } => RunRepositoryError::DelegationDenied { code },
        other => RunRepositoryError::Repository {
            reason: other.to_string(),
        },
    }
}
