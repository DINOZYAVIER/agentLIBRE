use std::time::{SystemTime, UNIX_EPOCH};

use agl_events::SafeRuntimeEventEnvelope;
use agl_ids::{RunId, SessionId, StepId, TurnId};
use rusqlite::{OptionalExtension, Row, Transaction, params};

use crate::{
    AglStore, DurableRunAdmission, DurableRunDraft, DurableRunRecord, EffectDeliveryClass,
    IdempotencyStatus, RecoveryReport, Result, RunLease, RunState, RunStepDraft, RunStepRecord,
    RunStepState, RunUsage, SafeRunStatus, StepLease, StoreError,
};

const RUN_COLUMNS: &str = "id, session_id, turn_id, kind, state, priority, input_json,
    checkpoint_json, effective_policy_hash, budget_json, usage_json, lease_owner,
    lease_generation, lease_expires_at_ms, cancellation_requested_at_ms, attempts,
    not_before_ms, created_at_ms, updated_at_ms, started_at_ms, finished_at_ms,
    terminal_result_json, error_code, error_message";

const STEP_COLUMNS: &str = "id, run_id, turn_id, effect_sequence, effect_kind,
    delivery_class, request_json, result_json, state, attempts, lease_owner,
    lease_generation, lease_expires_at_ms, not_before_ms, error_code, created_at_ms,
    updated_at_ms, finished_at_ms";

impl AglStore {
    pub fn admit_run(&self, draft: &DurableRunDraft) -> Result<DurableRunRecord> {
        self.admit_run_at(draft, unix_millis())
    }

    pub fn admit_run_at(&self, draft: &DurableRunDraft, now_ms: i64) -> Result<DurableRunRecord> {
        validate_run_draft(draft)?;
        self.transaction(|tx| {
            insert_run(tx, draft, now_ms)?;
            load_run(tx, &draft.run_id)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn admit_idempotent_run(
        &self,
        draft: &DurableRunDraft,
        namespace: &str,
        key: &str,
        fingerprint: &str,
        owner: &str,
        lease_expires_at_ms: i64,
        now_ms: i64,
    ) -> Result<DurableRunAdmission> {
        validate_run_draft(draft)?;
        validate_non_blank(namespace, "namespace")?;
        validate_non_blank(key, "key")?;
        validate_non_blank(fingerprint, "fingerprint")?;
        validate_non_blank(owner, "lease_owner")?;
        if lease_expires_at_ms <= now_ms {
            return invalid(
                "lease_expires_at_ms",
                lease_expires_at_ms,
                "lease must be future",
            );
        }

        self.transaction(|tx| {
            let existing = load_idempotency_for_admission(tx, namespace, key)?;
            if let Some(existing) = existing {
                if existing.fingerprint != fingerprint {
                    return Err(StoreError::IdempotencyConflict {
                        namespace: namespace.to_string(),
                        key: key.to_string(),
                        existing_fingerprint: existing.fingerprint,
                        requested_fingerprint: fingerprint.to_string(),
                    });
                }
                if let Some(run_id) = existing.admitted_run_id {
                    return Ok(DurableRunAdmission {
                        run: load_run(tx, &run_id)?,
                        replayed: true,
                    });
                }
                if existing.status == IdempotencyStatus::InProgress
                    && existing
                        .lease_expires_at_ms
                        .is_some_and(|expiry| expiry > now_ms)
                {
                    return Err(StoreError::TransitionRejected {
                        resource: format!("idempotency key {namespace}/{key}"),
                        from: "in_progress".to_string(),
                        to: "reclaimed".to_string(),
                    });
                }

                insert_run(tx, draft, now_ms)?;
                tx.execute(
                    "UPDATE idempotency_keys
                     SET status = 'in_progress', result_ref = NULL, lease_owner = ?3,
                         lease_expires_at_ms = ?4, admitted_run_id = ?5,
                         attempts = attempts + 1, last_error_code = NULL, updated_at = ?6
                     WHERE namespace = ?1 AND key = ?2",
                    params![
                        namespace,
                        key,
                        owner,
                        lease_expires_at_ms,
                        draft.run_id.as_str(),
                        legacy_timestamp(now_ms)
                    ],
                )?;
                return Ok(DurableRunAdmission {
                    run: load_run(tx, &draft.run_id)?,
                    replayed: false,
                });
            }

            insert_run(tx, draft, now_ms)?;
            tx.execute(
                "INSERT INTO idempotency_keys
                 (namespace, key, fingerprint, status, result_ref, created_at, updated_at,
                  lease_owner, lease_expires_at_ms, admitted_run_id, attempts, last_error_code)
                 VALUES (?1, ?2, ?3, 'in_progress', NULL, ?4, ?4, ?5, ?6, ?7, 1, NULL)",
                params![
                    namespace,
                    key,
                    fingerprint,
                    legacy_timestamp(now_ms),
                    owner,
                    lease_expires_at_ms,
                    draft.run_id.as_str()
                ],
            )?;
            Ok(DurableRunAdmission {
                run: load_run(tx, &draft.run_id)?,
                replayed: false,
            })
        })
    }

    pub fn run(&self, run_id: &RunId) -> Result<Option<DurableRunRecord>> {
        load_run_optional(&self.conn, run_id)
    }

    pub fn safe_run_status(&self, run_id: &RunId) -> Result<Option<SafeRunStatus>> {
        self.run(run_id)?.map(safe_status).transpose()
    }

    pub fn claim_next_run(
        &self,
        owner: &str,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<Option<RunLease>> {
        validate_non_blank(owner, "lease_owner")?;
        if lease_duration_ms <= 0 {
            return invalid("lease_duration_ms", lease_duration_ms, "must be positive");
        }
        let expires_at_ms = now_ms.saturating_add(lease_duration_ms);
        self.transaction(|tx| {
            let candidate: Option<String> = tx
                .query_row(
                    "SELECT r.id
                     FROM runs r
                     WHERE r.state IN ('queued', 'waiting')
                       AND r.cancellation_requested_at_ms IS NULL
                       AND (r.not_before_ms IS NULL OR r.not_before_ms <= ?1)
                       AND (
                           r.session_id IS NULL OR (
                               NOT EXISTS (
                                   SELECT 1 FROM runs active
                                   WHERE active.session_id = r.session_id
                                     AND active.state = 'running'
                               )
                               AND NOT EXISTS (
                                   SELECT 1 FROM runs earlier
                                   WHERE earlier.session_id = r.session_id
                                     AND earlier.state IN ('queued', 'waiting')
                                     AND (
                                         earlier.created_at_ms < r.created_at_ms OR
                                         (earlier.created_at_ms = r.created_at_ms AND earlier.rowid < r.rowid)
                                     )
                               )
                           )
                       )
                     ORDER BY r.priority DESC, r.created_at_ms, r.rowid
                     LIMIT 1",
                    [now_ms],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(candidate) = candidate else {
                return Ok(None);
            };
            let run_id = parse_run_id(&candidate, "runs.id")?;
            let changed = tx.execute(
                "UPDATE runs
                 SET state = 'running', lease_owner = ?2,
                     lease_generation = lease_generation + 1,
                     lease_expires_at_ms = ?3, attempts = attempts + 1,
                     started_at_ms = COALESCE(started_at_ms, MAX(?1, COALESCE((
                         SELECT MAX(previous.finished_at_ms) FROM runs previous
                         WHERE previous.session_id = runs.session_id
                           AND previous.id != runs.id
                     ), ?1))),
                     updated_at_ms = MAX(?1, COALESCE((
                         SELECT MAX(previous.finished_at_ms) FROM runs previous
                         WHERE previous.session_id = runs.session_id
                           AND previous.id != runs.id
                     ), ?1))
                 WHERE id = ?4 AND state IN ('queued', 'waiting')
                   AND cancellation_requested_at_ms IS NULL",
                params![now_ms, owner, expires_at_ms, run_id.as_str()],
            )?;
            if changed != 1 {
                return Ok(None);
            }
            let generation: u64 = tx.query_row(
                "SELECT lease_generation FROM runs WHERE id = ?1",
                [run_id.as_str()],
                |row| row.get(0),
            )?;
            tx.execute(
                "UPDATE idempotency_keys
                 SET lease_owner = ?1, lease_expires_at_ms = ?2,
                     attempts = attempts + 1, updated_at = ?3
                 WHERE admitted_run_id = ?4 AND status = 'in_progress'",
                params![
                    owner,
                    expires_at_ms,
                    legacy_timestamp(now_ms),
                    run_id.as_str()
                ],
            )?;
            Ok(Some(RunLease {
                run_id,
                owner: owner.to_string(),
                generation,
                expires_at_ms,
            }))
        })
    }

    pub fn heartbeat_run(&self, lease: &RunLease, expires_at_ms: i64, now_ms: i64) -> Result<()> {
        self.transaction(|tx| {
            let changed = tx.execute(
                "UPDATE runs SET lease_expires_at_ms = ?1, updated_at_ms = ?2
                 WHERE id = ?3 AND state = 'running' AND lease_owner = ?4
                   AND lease_generation = ?5",
                params![
                    expires_at_ms,
                    now_ms,
                    lease.run_id.as_str(),
                    lease.owner,
                    lease.generation
                ],
            )?;
            require_fenced_change(changed, format!("run {}", lease.run_id))?;
            tx.execute(
                "UPDATE run_steps SET lease_expires_at_ms = ?1, updated_at_ms = ?2
                 WHERE run_id = ?3 AND state = 'running' AND lease_owner = ?4",
                params![expires_at_ms, now_ms, lease.run_id.as_str(), lease.owner],
            )?;
            tx.execute(
                "UPDATE idempotency_keys SET lease_expires_at_ms = ?1, updated_at = ?2
                 WHERE admitted_run_id = ?3 AND status = 'in_progress' AND lease_owner = ?4",
                params![
                    expires_at_ms,
                    legacy_timestamp(now_ms),
                    lease.run_id.as_str(),
                    lease.owner
                ],
            )?;
            Ok(())
        })
    }

    pub fn request_run_cancellation(&self, run_id: &RunId, now_ms: i64) -> Result<SafeRunStatus> {
        self.transaction(|tx| {
            let run = load_run(tx, run_id)?;
            if run.state.is_terminal() {
                return safe_status(run);
            }
            let (state, finished_at_ms) = match run.state {
                RunState::Queued | RunState::Waiting => (RunState::Cancelled, Some(now_ms)),
                RunState::Running => (RunState::Running, None),
                terminal => return safe_status_with_state(run, terminal),
            };
            tx.execute(
                "UPDATE runs
                 SET state = ?1, cancellation_requested_at_ms = COALESCE(cancellation_requested_at_ms, ?2),
                     finished_at_ms = COALESCE(finished_at_ms, ?3),
                     lease_owner = CASE WHEN ?1 = 'cancelled' THEN NULL ELSE lease_owner END,
                     lease_expires_at_ms = CASE WHEN ?1 = 'cancelled' THEN NULL ELSE lease_expires_at_ms END,
                     updated_at_ms = ?2
                 WHERE id = ?4",
                params![state.as_str(), now_ms, finished_at_ms, run_id.as_str()],
            )?;
            if state == RunState::Cancelled {
                tx.execute(
                    "UPDATE run_steps
                     SET state = 'cancelled', lease_owner = NULL, lease_expires_at_ms = NULL,
                         error_code = COALESCE(error_code, 'run_cancelled'),
                         updated_at_ms = ?2, finished_at_ms = ?2
                     WHERE run_id = ?1 AND state IN ('pending', 'running')",
                    params![run_id.as_str(), now_ms],
                )?;
                finish_linked_idempotency(tx, run_id, state, Some("run_cancelled"), now_ms)?;
            }
            safe_status(load_run(tx, run_id)?)
        })
    }

    pub fn publish_run_step(
        &self,
        lease: &RunLease,
        checkpoint: &serde_json::Value,
        step: &RunStepDraft,
        events: &[SafeRuntimeEventEnvelope],
        now_ms: i64,
    ) -> Result<RunStepRecord> {
        validate_step_draft(lease, step)?;
        self.transaction(|tx| {
            require_run_lease(tx, lease)?;
            tx.execute(
                "UPDATE runs SET checkpoint_json = ?1, updated_at_ms = ?2 WHERE id = ?3",
                params![
                    serde_json::to_string(checkpoint)?,
                    now_ms,
                    lease.run_id.as_str()
                ],
            )?;
            tx.execute(
                "INSERT INTO run_steps
                 (id, run_id, turn_id, effect_sequence, effect_kind, delivery_class,
                  request_json, result_json, state, attempts, lease_owner,
                  lease_generation, lease_expires_at_ms, not_before_ms, error_code,
                  created_at_ms, updated_at_ms, finished_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, 'pending', 0,
                         NULL, 0, NULL, NULL, NULL, ?8, ?8, NULL)",
                params![
                    step.step_id.as_str(),
                    lease.run_id.as_str(),
                    step.turn_id.as_ref().map(TurnId::as_str),
                    step.effect_sequence,
                    step.effect_kind,
                    step.delivery_class.as_str(),
                    serde_json::to_string(&step.request)?,
                    now_ms
                ],
            )?;
            append_events(tx, &lease.run_id, events)?;
            load_step(tx, &step.step_id)
        })
    }

    pub fn claim_run_step(
        &self,
        run_lease: &RunLease,
        step_id: &StepId,
        expires_at_ms: i64,
        now_ms: i64,
    ) -> Result<StepLease> {
        self.transaction(|tx| {
            require_run_lease(tx, run_lease)?;
            let changed = tx.execute(
                "UPDATE run_steps
                 SET state = 'running', attempts = attempts + 1,
                     lease_owner = ?1, lease_generation = lease_generation + 1,
                     lease_expires_at_ms = ?2, updated_at_ms = ?3
                 WHERE id = ?4 AND run_id = ?5 AND state = 'pending'
                   AND (not_before_ms IS NULL OR not_before_ms <= ?3)",
                params![
                    run_lease.owner,
                    expires_at_ms,
                    now_ms,
                    step_id.as_str(),
                    run_lease.run_id.as_str()
                ],
            )?;
            require_transition_change(changed, format!("step {step_id}"), "pending", "running")?;
            let generation: u64 = tx.query_row(
                "SELECT lease_generation FROM run_steps WHERE id = ?1",
                [step_id.as_str()],
                |row| row.get(0),
            )?;
            Ok(StepLease {
                step_id: step_id.clone(),
                run_id: run_lease.run_id.clone(),
                owner: run_lease.owner.clone(),
                generation,
                expires_at_ms,
            })
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn complete_run_step(
        &self,
        run_lease: &RunLease,
        step_lease: &StepLease,
        state: RunStepState,
        result: Option<&serde_json::Value>,
        checkpoint: &serde_json::Value,
        usage: &RunUsage,
        events: &[SafeRuntimeEventEnvelope],
        error_code: Option<&str>,
        now_ms: i64,
    ) -> Result<RunStepRecord> {
        if !matches!(
            state,
            RunStepState::Succeeded | RunStepState::Failed | RunStepState::Cancelled
        ) {
            return invalid(
                "run_steps.state",
                state.as_str(),
                "completion must be terminal",
            );
        }
        self.transaction(|tx| {
            require_run_lease(tx, run_lease)?;
            let changed = tx.execute(
                "UPDATE run_steps
                 SET state = ?1, result_json = ?2, error_code = ?3,
                     lease_owner = NULL, lease_expires_at_ms = NULL,
                     updated_at_ms = ?4, finished_at_ms = ?4
                 WHERE id = ?5 AND run_id = ?6 AND state = 'running'
                   AND lease_owner = ?7 AND lease_generation = ?8",
                params![
                    state.as_str(),
                    result.map(serde_json::to_string).transpose()?,
                    error_code,
                    now_ms,
                    step_lease.step_id.as_str(),
                    run_lease.run_id.as_str(),
                    step_lease.owner,
                    step_lease.generation
                ],
            )?;
            require_fenced_change(changed, format!("step {}", step_lease.step_id))?;
            let changed = tx.execute(
                "UPDATE runs SET checkpoint_json = ?1, usage_json = ?2, updated_at_ms = ?3
                 WHERE id = ?4 AND state = 'running' AND lease_owner = ?5
                   AND lease_generation = ?6",
                params![
                    serde_json::to_string(checkpoint)?,
                    serde_json::to_string(usage)?,
                    now_ms,
                    run_lease.run_id.as_str(),
                    run_lease.owner,
                    run_lease.generation
                ],
            )?;
            require_fenced_change(changed, format!("run {}", run_lease.run_id))?;
            append_events(tx, &run_lease.run_id, events)?;
            load_step(tx, &step_lease.step_id)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn retry_run_step(
        &self,
        run_lease: &RunLease,
        step_lease: &StepLease,
        not_before_ms: i64,
        error_code: &str,
        checkpoint: &serde_json::Value,
        usage: &RunUsage,
        events: &[SafeRuntimeEventEnvelope],
        now_ms: i64,
    ) -> Result<()> {
        self.transaction(|tx| {
            require_run_lease(tx, run_lease)?;
            let changed = tx.execute(
                "UPDATE run_steps
                 SET state = 'pending', lease_owner = NULL, lease_expires_at_ms = NULL,
                     not_before_ms = ?1, error_code = ?2, updated_at_ms = ?3
                 WHERE id = ?4 AND state = 'running' AND lease_owner = ?5
                   AND lease_generation = ?6",
                params![
                    not_before_ms,
                    error_code,
                    now_ms,
                    step_lease.step_id.as_str(),
                    step_lease.owner,
                    step_lease.generation
                ],
            )?;
            require_fenced_change(changed, format!("step {}", step_lease.step_id))?;
            let changed = tx.execute(
                "UPDATE runs SET state = 'waiting', not_before_ms = ?1,
                     checkpoint_json = ?2, usage_json = ?3,
                     lease_owner = NULL, lease_expires_at_ms = NULL, updated_at_ms = ?4
                 WHERE id = ?5 AND lease_owner = ?6 AND lease_generation = ?7",
                params![
                    not_before_ms,
                    serde_json::to_string(checkpoint)?,
                    serde_json::to_string(usage)?,
                    now_ms,
                    run_lease.run_id.as_str(),
                    run_lease.owner,
                    run_lease.generation
                ],
            )?;
            require_fenced_change(changed, format!("run {}", run_lease.run_id))?;
            append_events(tx, &run_lease.run_id, events)?;
            Ok(())
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn finish_run(
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
    ) -> Result<DurableRunRecord> {
        if !state.is_terminal() {
            return invalid(
                "runs.state",
                state.as_str(),
                "finish state must be terminal",
            );
        }
        self.transaction(|tx| {
            let changed = tx.execute(
                "UPDATE runs
                 SET state = ?1, checkpoint_json = COALESCE(?2, checkpoint_json),
                     usage_json = ?3, terminal_result_json = ?4,
                     error_code = ?5, error_message = ?6,
                     lease_owner = NULL, lease_expires_at_ms = NULL,
                     updated_at_ms = ?7, finished_at_ms = ?7
                 WHERE id = ?8 AND state = 'running' AND lease_owner = ?9
                   AND lease_generation = ?10",
                params![
                    state.as_str(),
                    checkpoint.map(serde_json::to_string).transpose()?,
                    serde_json::to_string(usage)?,
                    terminal_result.map(serde_json::to_string).transpose()?,
                    error_code,
                    error_message,
                    now_ms,
                    lease.run_id.as_str(),
                    lease.owner,
                    lease.generation
                ],
            )?;
            require_fenced_change(changed, format!("run {}", lease.run_id))?;
            if state == RunState::Cancelled {
                tx.execute(
                    "UPDATE run_steps
                     SET state = 'cancelled', lease_owner = NULL, lease_expires_at_ms = NULL,
                         error_code = COALESCE(error_code, 'run_cancelled'),
                         updated_at_ms = ?2, finished_at_ms = ?2
                     WHERE run_id = ?1 AND state IN ('pending', 'running')",
                    params![lease.run_id.as_str(), now_ms],
                )?;
            }
            append_events(tx, &lease.run_id, events)?;
            finish_linked_idempotency(tx, &lease.run_id, state, error_code, now_ms)?;
            load_run(tx, &lease.run_id)
        })
    }

    pub fn run_steps(&self, run_id: &RunId) -> Result<Vec<RunStepRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {STEP_COLUMNS} FROM run_steps WHERE run_id = ?1 ORDER BY effect_sequence"
        ))?;
        let rows = stmt.query_map([run_id.as_str()], read_step_row)?;
        rows.map(|row| decode_step(row?)).collect()
    }

    pub fn run_step_by_sequence(
        &self,
        run_id: &RunId,
        effect_sequence: u64,
    ) -> Result<Option<RunStepRecord>> {
        let sql = format!(
            "SELECT {STEP_COLUMNS} FROM run_steps WHERE run_id = ?1 AND effect_sequence = ?2"
        );
        self.conn
            .query_row(
                &sql,
                params![run_id.as_str(), effect_sequence],
                read_step_row,
            )
            .optional()?
            .map(decode_step)
            .transpose()
    }

    pub fn run_events_after(
        &self,
        run_id: &RunId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<SafeRuntimeEventEnvelope>> {
        if limit == 0 {
            return invalid("run_events.limit", limit, "must be positive");
        }
        let mut stmt = self.conn.prepare(
            "SELECT envelope_json FROM run_events
             WHERE run_id = ?1 AND sequence > ?2 ORDER BY sequence LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![run_id.as_str(), after_sequence, limit], |row| {
            row.get::<_, String>(0)
        })?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn latest_run_event_sequence(&self, run_id: &RunId) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM run_events WHERE run_id = ?1",
            [run_id.as_str()],
            |row| row.get(0),
        )?)
    }

    pub fn recover_expired_work(&self, now_ms: i64) -> Result<RecoveryReport> {
        self.transaction(|tx| {
            let outcome_unknown_steps = tx.execute(
                "UPDATE run_steps
                 SET state = 'outcome_unknown', error_code = 'effect_outcome_unknown',
                     lease_owner = NULL, lease_expires_at_ms = NULL,
                     updated_at_ms = ?1, finished_at_ms = ?1
                 WHERE state = 'running' AND delivery_class = 'at_most_once'
                   AND lease_expires_at_ms <= ?1",
                [now_ms],
            )? as u64;
            let failed_runs = tx.execute(
                "UPDATE runs
                 SET state = 'failed', error_code = 'effect_outcome_unknown',
                     error_message = 'an at-most-once effect lease expired before its outcome was recorded',
                     lease_owner = NULL, lease_expires_at_ms = NULL,
                     updated_at_ms = ?1, finished_at_ms = ?1
                 WHERE state = 'running' AND EXISTS (
                     SELECT 1 FROM run_steps s
                     WHERE s.run_id = runs.id AND s.state = 'outcome_unknown'
                 )",
                [now_ms],
            )? as u64;
            let requeued_steps = tx.execute(
                "UPDATE run_steps
                 SET state = 'pending', lease_owner = NULL, lease_expires_at_ms = NULL,
                     not_before_ms = ?1, error_code = 'lease_expired', updated_at_ms = ?1
                 WHERE state = 'running' AND delivery_class IN ('replay_safe', 'idempotent')
                   AND lease_expires_at_ms <= ?1",
                [now_ms],
            )? as u64;
            let requeued_runs = tx.execute(
                "UPDATE runs
                 SET state = 'queued', lease_owner = NULL, lease_expires_at_ms = NULL,
                     not_before_ms = ?1, updated_at_ms = ?1
                 WHERE state = 'running' AND lease_expires_at_ms <= ?1
                   AND error_code IS NULL",
                [now_ms],
            )? as u64;
            let reclaimed_idempotency_keys = tx.execute(
                "UPDATE idempotency_keys
                 SET lease_owner = NULL, lease_expires_at_ms = NULL,
                     last_error_code = 'lease_expired', updated_at = ?2
                 WHERE status = 'in_progress' AND lease_expires_at_ms <= ?1",
                params![now_ms, legacy_timestamp(now_ms)],
            )? as u64;
            Ok(RecoveryReport {
                requeued_runs,
                requeued_steps,
                outcome_unknown_steps,
                failed_runs,
                reclaimed_idempotency_keys,
            })
        })
    }
}

fn insert_run(tx: &Transaction<'_>, draft: &DurableRunDraft, now_ms: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO runs
         (id, session_id, turn_id, kind, state, priority, input_json, checkpoint_json,
          effective_policy_hash, budget_json, usage_json, lease_owner, lease_generation,
          lease_expires_at_ms, cancellation_requested_at_ms, attempts, not_before_ms,
          created_at_ms, updated_at_ms, started_at_ms, finished_at_ms,
          terminal_result_json, error_code, error_message)
         VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6, ?7, ?8, ?9, ?10,
                 NULL, 0, NULL, NULL, 0, ?11, ?12, ?12, NULL, NULL, NULL, NULL, NULL)",
        params![
            draft.run_id.as_str(),
            draft.session_id.as_ref().map(SessionId::as_str),
            draft.turn_id.as_ref().map(TurnId::as_str),
            draft.kind.as_str(),
            draft.priority,
            serde_json::to_string(&draft.input)?,
            draft
                .checkpoint
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            draft.effective_policy_hash,
            serde_json::to_string(&draft.budget)?,
            serde_json::to_string(&RunUsage::default())?,
            draft.not_before_ms,
            now_ms
        ],
    )?;
    Ok(())
}

fn validate_run_draft(draft: &DurableRunDraft) -> Result<()> {
    if draft.session_id.is_some() != draft.turn_id.is_some() {
        return invalid(
            "runs.identity",
            draft.run_id.as_str(),
            "session_id and turn_id must either both be present or both be absent",
        );
    }
    if draft.kind == crate::RunKind::Turn && draft.turn_id.is_none() {
        return invalid(
            "runs.kind",
            draft.kind.as_str(),
            "turn runs require session and turn IDs",
        );
    }
    Ok(())
}

fn validate_step_draft(lease: &RunLease, step: &RunStepDraft) -> Result<()> {
    if step.effect_sequence == 0 {
        return invalid("run_steps.effect_sequence", 0, "must be positive");
    }
    validate_non_blank(&step.effect_kind, "run_steps.effect_kind")?;
    if step.turn_id.is_some() && lease.run_id.as_str().is_empty() {
        unreachable!("typed run IDs are never empty");
    }
    Ok(())
}

fn append_events(
    tx: &Transaction<'_>,
    run_id: &RunId,
    events: &[SafeRuntimeEventEnvelope],
) -> Result<()> {
    let mut expected: u64 = tx.query_row(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM run_events WHERE run_id = ?1",
        [run_id.as_str()],
        |row| row.get(0),
    )?;
    for event in events {
        if event.scope.run_id() != run_id {
            return invalid(
                "run_events.run_id",
                event.scope.run_id().as_str(),
                "event scope does not match the durable run",
            );
        }
        if event.sequence != expected {
            return invalid(
                "run_events.sequence",
                event.sequence,
                "event sequence must be contiguous for the durable run",
            );
        }
        tx.execute(
            "INSERT INTO run_events
             (event_id, run_id, sequence, kind, occurred_at_unix_ms, envelope_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.event_id.as_str(),
                run_id.as_str(),
                event.sequence,
                event.payload.kind(),
                event.occurred_at_unix_ms,
                serde_json::to_string(event)?
            ],
        )?;
        expected = expected
            .checked_add(1)
            .ok_or_else(|| StoreError::InvalidValue {
                field: "run_events.sequence",
                value: event.sequence.to_string(),
                reason: "event sequence overflow",
            })?;
    }
    Ok(())
}

fn require_run_lease(tx: &Transaction<'_>, lease: &RunLease) -> Result<()> {
    let owned = tx
        .query_row(
            "SELECT 1 FROM runs
             WHERE id = ?1 AND state = 'running' AND lease_owner = ?2
               AND lease_generation = ?3",
            params![lease.run_id.as_str(), lease.owner, lease.generation],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if owned {
        Ok(())
    } else {
        Err(StoreError::LeaseLost {
            resource: format!("run {}", lease.run_id),
        })
    }
}

fn finish_linked_idempotency(
    tx: &Transaction<'_>,
    run_id: &RunId,
    state: RunState,
    error_code: Option<&str>,
    now_ms: i64,
) -> Result<()> {
    let status = match state {
        RunState::Succeeded | RunState::Cancelled => IdempotencyStatus::Completed,
        RunState::Failed => IdempotencyStatus::Failed,
        _ => return Ok(()),
    };
    tx.execute(
        "UPDATE idempotency_keys
         SET status = ?1, result_ref = ?2, lease_owner = NULL,
             lease_expires_at_ms = NULL, last_error_code = ?3, updated_at = ?4
         WHERE admitted_run_id = ?2 AND status = 'in_progress'",
        params![
            status.as_str(),
            run_id.as_str(),
            error_code,
            legacy_timestamp(now_ms)
        ],
    )?;
    Ok(())
}

fn load_run(conn: &rusqlite::Connection, run_id: &RunId) -> Result<DurableRunRecord> {
    load_run_optional(conn, run_id)?.ok_or_else(|| StoreError::NotFound {
        resource: format!("run {run_id}"),
    })
}

fn load_run_optional(
    conn: &rusqlite::Connection,
    run_id: &RunId,
) -> Result<Option<DurableRunRecord>> {
    let sql = format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1");
    conn.query_row(&sql, [run_id.as_str()], read_run_row)
        .optional()?
        .map(decode_run)
        .transpose()
}

#[derive(Debug)]
struct RawRunRow {
    id: String,
    session_id: Option<String>,
    turn_id: Option<String>,
    kind: String,
    state: String,
    priority: i32,
    input_json: String,
    checkpoint_json: Option<String>,
    effective_policy_hash: Option<String>,
    budget_json: String,
    usage_json: String,
    lease_owner: Option<String>,
    lease_generation: u64,
    lease_expires_at_ms: Option<i64>,
    cancellation_requested_at_ms: Option<i64>,
    attempts: u32,
    not_before_ms: Option<i64>,
    created_at_ms: i64,
    updated_at_ms: i64,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
    terminal_result_json: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
}

fn read_run_row(row: &Row<'_>) -> rusqlite::Result<RawRunRow> {
    Ok(RawRunRow {
        id: row.get(0)?,
        session_id: row.get(1)?,
        turn_id: row.get(2)?,
        kind: row.get(3)?,
        state: row.get(4)?,
        priority: row.get(5)?,
        input_json: row.get(6)?,
        checkpoint_json: row.get(7)?,
        effective_policy_hash: row.get(8)?,
        budget_json: row.get(9)?,
        usage_json: row.get(10)?,
        lease_owner: row.get(11)?,
        lease_generation: row.get(12)?,
        lease_expires_at_ms: row.get(13)?,
        cancellation_requested_at_ms: row.get(14)?,
        attempts: row.get(15)?,
        not_before_ms: row.get(16)?,
        created_at_ms: row.get(17)?,
        updated_at_ms: row.get(18)?,
        started_at_ms: row.get(19)?,
        finished_at_ms: row.get(20)?,
        terminal_result_json: row.get(21)?,
        error_code: row.get(22)?,
        error_message: row.get(23)?,
    })
}

fn decode_run(raw: RawRunRow) -> Result<DurableRunRecord> {
    Ok(DurableRunRecord {
        run_id: parse_run_id(&raw.id, "runs.id")?,
        session_id: raw
            .session_id
            .as_deref()
            .map(|value| parse_session_id(value, "runs.session_id"))
            .transpose()?,
        turn_id: raw
            .turn_id
            .as_deref()
            .map(|value| parse_turn_id(value, "runs.turn_id"))
            .transpose()?,
        kind: crate::RunKind::parse(&raw.kind)?,
        state: RunState::parse(&raw.state)?,
        priority: raw.priority,
        input: serde_json::from_str(&raw.input_json)?,
        checkpoint: raw
            .checkpoint_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?,
        effective_policy_hash: raw.effective_policy_hash,
        budget: serde_json::from_str(&raw.budget_json)?,
        usage: serde_json::from_str(&raw.usage_json)?,
        lease_owner: raw.lease_owner,
        lease_generation: raw.lease_generation,
        lease_expires_at_ms: raw.lease_expires_at_ms,
        cancellation_requested_at_ms: raw.cancellation_requested_at_ms,
        attempts: raw.attempts,
        not_before_ms: raw.not_before_ms,
        created_at_ms: raw.created_at_ms,
        updated_at_ms: raw.updated_at_ms,
        started_at_ms: raw.started_at_ms,
        finished_at_ms: raw.finished_at_ms,
        terminal_result: raw
            .terminal_result_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?,
        error_code: raw.error_code,
        error_message: raw.error_message,
    })
}

fn safe_status(run: DurableRunRecord) -> Result<SafeRunStatus> {
    safe_status_with_state(run.clone(), run.state)
}

fn safe_status_with_state(run: DurableRunRecord, state: RunState) -> Result<SafeRunStatus> {
    Ok(SafeRunStatus {
        run_id: run.run_id,
        session_id: run.session_id,
        turn_id: run.turn_id,
        kind: run.kind,
        state,
        priority: run.priority,
        usage: run.usage,
        cancellation_requested: run.cancellation_requested_at_ms.is_some(),
        attempts: run.attempts,
        created_at_ms: run.created_at_ms,
        updated_at_ms: run.updated_at_ms,
        started_at_ms: run.started_at_ms,
        finished_at_ms: run.finished_at_ms,
        error_code: run.error_code,
    })
}

fn load_step(tx: &Transaction<'_>, step_id: &StepId) -> Result<RunStepRecord> {
    let sql = format!("SELECT {STEP_COLUMNS} FROM run_steps WHERE id = ?1");
    tx.query_row(&sql, [step_id.as_str()], read_step_row)
        .optional()?
        .map(decode_step)
        .transpose()?
        .ok_or_else(|| StoreError::NotFound {
            resource: format!("run step {step_id}"),
        })
}

#[derive(Debug)]
struct RawStepRow {
    id: String,
    run_id: String,
    turn_id: Option<String>,
    effect_sequence: u64,
    effect_kind: String,
    delivery_class: String,
    request_json: String,
    result_json: Option<String>,
    state: String,
    attempts: u32,
    lease_owner: Option<String>,
    lease_generation: u64,
    lease_expires_at_ms: Option<i64>,
    not_before_ms: Option<i64>,
    error_code: Option<String>,
    created_at_ms: i64,
    updated_at_ms: i64,
    finished_at_ms: Option<i64>,
}

fn read_step_row(row: &Row<'_>) -> rusqlite::Result<RawStepRow> {
    Ok(RawStepRow {
        id: row.get(0)?,
        run_id: row.get(1)?,
        turn_id: row.get(2)?,
        effect_sequence: row.get(3)?,
        effect_kind: row.get(4)?,
        delivery_class: row.get(5)?,
        request_json: row.get(6)?,
        result_json: row.get(7)?,
        state: row.get(8)?,
        attempts: row.get(9)?,
        lease_owner: row.get(10)?,
        lease_generation: row.get(11)?,
        lease_expires_at_ms: row.get(12)?,
        not_before_ms: row.get(13)?,
        error_code: row.get(14)?,
        created_at_ms: row.get(15)?,
        updated_at_ms: row.get(16)?,
        finished_at_ms: row.get(17)?,
    })
}

fn decode_step(raw: RawStepRow) -> Result<RunStepRecord> {
    Ok(RunStepRecord {
        step_id: parse_step_id(&raw.id, "run_steps.id")?,
        run_id: parse_run_id(&raw.run_id, "run_steps.run_id")?,
        turn_id: raw
            .turn_id
            .as_deref()
            .map(|value| parse_turn_id(value, "run_steps.turn_id"))
            .transpose()?,
        effect_sequence: raw.effect_sequence,
        effect_kind: raw.effect_kind,
        delivery_class: EffectDeliveryClass::parse(&raw.delivery_class)?,
        request: serde_json::from_str(&raw.request_json)?,
        result: raw
            .result_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?,
        state: RunStepState::parse(&raw.state)?,
        attempts: raw.attempts,
        lease_owner: raw.lease_owner,
        lease_generation: raw.lease_generation,
        lease_expires_at_ms: raw.lease_expires_at_ms,
        not_before_ms: raw.not_before_ms,
        error_code: raw.error_code,
        created_at_ms: raw.created_at_ms,
        updated_at_ms: raw.updated_at_ms,
        finished_at_ms: raw.finished_at_ms,
    })
}

struct AdmissionIdempotency {
    fingerprint: String,
    status: IdempotencyStatus,
    lease_expires_at_ms: Option<i64>,
    admitted_run_id: Option<RunId>,
}

fn load_idempotency_for_admission(
    tx: &Transaction<'_>,
    namespace: &str,
    key: &str,
) -> Result<Option<AdmissionIdempotency>> {
    tx.query_row(
        "SELECT fingerprint, status, lease_expires_at_ms, admitted_run_id
         FROM idempotency_keys WHERE namespace = ?1 AND key = ?2",
        params![namespace, key],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        },
    )
    .optional()?
    .map(
        |(fingerprint, status, lease_expires_at_ms, admitted_run_id)| {
            Ok(AdmissionIdempotency {
                fingerprint,
                status: IdempotencyStatus::parse(&status)?,
                lease_expires_at_ms,
                admitted_run_id: admitted_run_id
                    .as_deref()
                    .map(|value| parse_run_id(value, "idempotency_keys.admitted_run_id"))
                    .transpose()?,
            })
        },
    )
    .transpose()
}

fn parse_run_id(value: &str, field: &'static str) -> Result<RunId> {
    RunId::parse(value).map_err(|_| StoreError::InvalidValue {
        field,
        value: value.to_string(),
        reason: "invalid typed run ID",
    })
}

fn parse_session_id(value: &str, field: &'static str) -> Result<SessionId> {
    SessionId::parse(value).map_err(|_| StoreError::InvalidValue {
        field,
        value: value.to_string(),
        reason: "invalid typed session ID",
    })
}

fn parse_turn_id(value: &str, field: &'static str) -> Result<TurnId> {
    TurnId::parse(value).map_err(|_| StoreError::InvalidValue {
        field,
        value: value.to_string(),
        reason: "invalid typed turn ID",
    })
}

fn parse_step_id(value: &str, field: &'static str) -> Result<StepId> {
    StepId::parse(value).map_err(|_| StoreError::InvalidValue {
        field,
        value: value.to_string(),
        reason: "invalid typed step ID",
    })
}

fn require_fenced_change(changed: usize, resource: String) -> Result<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::LeaseLost { resource })
    }
}

fn require_transition_change(changed: usize, resource: String, from: &str, to: &str) -> Result<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::TransitionRejected {
            resource,
            from: from.to_string(),
            to: to.to_string(),
        })
    }
}

fn validate_non_blank(value: &str, field: &'static str) -> Result<()> {
    if value.trim().is_empty() {
        invalid(field, value, "value cannot be blank")
    } else {
        Ok(())
    }
}

fn invalid<T>(field: &'static str, value: impl ToString, reason: &'static str) -> Result<T> {
    Err(StoreError::InvalidValue {
        field,
        value: value.to_string(),
        reason,
    })
}

fn unix_millis() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

fn legacy_timestamp(now_ms: i64) -> String {
    format!("unix:{}", now_ms.div_euclid(1_000))
}
