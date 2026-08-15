use std::time::{SystemTime, UNIX_EPOCH};

use agl_cron::{
    CronDueJob, CronError, CronIdempotencyOutcome, CronIdempotencyRecord, CronIdempotencyStatus,
    CronJob, CronJobDraft, CronJobUpdate, CronRepository, CronRun, CronRunAdmission, CronRunStatus,
    CronTargetKind, validate_job_draft,
};
use agl_ids::RunId;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::{
    AglStore, IdempotencyOutcome, IdempotencyRecord, IdempotencyStatus, StoreError, StoreHandle,
};

const IDEMPOTENCY_NAMESPACE: &str = "core.cron:run";

impl CronRepository for StoreHandle {
    fn update_job(&self, id: &str, update: CronJobUpdate) -> agl_cron::Result<CronJob> {
        update_job(&*self.lock().map_err(repository_error)?, id, update)
    }

    fn add_job(&self, draft: CronJobDraft) -> agl_cron::Result<CronJob> {
        add_job(&*self.lock().map_err(repository_error)?, draft)
    }

    fn list_jobs(&self, include_deleted: bool) -> agl_cron::Result<Vec<CronJob>> {
        list_jobs(&*self.lock().map_err(repository_error)?, include_deleted)
    }

    fn job(&self, id: &str) -> agl_cron::Result<Option<CronJob>> {
        job(&*self.lock().map_err(repository_error)?, id)
    }

    fn set_enabled(&self, id: &str, enabled: bool) -> agl_cron::Result<CronJob> {
        set_enabled(&*self.lock().map_err(repository_error)?, id, enabled)
    }

    fn delete_job(&self, id: &str) -> agl_cron::Result<CronJob> {
        delete_job(&*self.lock().map_err(repository_error)?, id)
    }

    fn record_manual_run(
        &self,
        job_id: &str,
        result_ref: Option<&str>,
    ) -> agl_cron::Result<(CronRun, CronIdempotencyOutcome)> {
        record_manual_run(&*self.lock().map_err(repository_error)?, job_id, result_ref)
    }

    fn record_manual_run_result(
        &self,
        job_id: &str,
        status: CronRunStatus,
        result_ref: Option<&str>,
        error: Option<&str>,
    ) -> agl_cron::Result<(CronRun, CronIdempotencyOutcome)> {
        record_manual_run_result(
            &*self.lock().map_err(repository_error)?,
            job_id,
            status,
            result_ref,
            error,
        )
    }

    fn record_run_for(
        &self,
        job_id: &str,
        scheduled_for: &str,
        status: CronRunStatus,
        result_ref: Option<&str>,
        error: Option<&str>,
    ) -> agl_cron::Result<(CronRun, CronIdempotencyOutcome)> {
        record_run_for(
            &*self.lock().map_err(repository_error)?,
            job_id,
            scheduled_for,
            status,
            result_ref,
            error,
        )
    }

    fn begin_run_for(
        &self,
        job_value: &CronJob,
        scheduled_for: &str,
    ) -> agl_cron::Result<CronRunAdmission> {
        begin_run_for(
            &*self.lock().map_err(repository_error)?,
            job_value,
            scheduled_for,
        )
    }

    fn record_admitted_run(
        &self,
        job_id: &str,
        scheduled_for: &str,
        status: CronRunStatus,
        result_ref: Option<&str>,
        error: Option<&str>,
    ) -> agl_cron::Result<CronRun> {
        record_admitted_run(
            &*self.lock().map_err(repository_error)?,
            job_id,
            scheduled_for,
            status,
            result_ref,
            error,
        )
    }

    fn record_admitted_supervisor_run(
        &self,
        job_id: &str,
        scheduled_for: &str,
        supervisor_run_id: &RunId,
    ) -> agl_cron::Result<CronRun> {
        record_admitted_supervisor_run(
            &*self.lock().map_err(repository_error)?,
            job_id,
            scheduled_for,
            supervisor_run_id,
        )
    }

    fn active_supervisor_runs(&self) -> agl_cron::Result<Vec<CronRun>> {
        active_supervisor_runs(&*self.lock().map_err(repository_error)?)
    }

    fn finish_supervisor_run(
        &self,
        supervisor_run_id: &RunId,
        status: CronRunStatus,
        result_ref: Option<&str>,
        error: Option<&str>,
    ) -> agl_cron::Result<CronRun> {
        finish_supervisor_run(
            &*self.lock().map_err(repository_error)?,
            supervisor_run_id,
            status,
            result_ref,
            error,
        )
    }

    fn history(&self, job_id: &str) -> agl_cron::Result<Vec<CronRun>> {
        history(&*self.lock().map_err(repository_error)?, job_id)
    }

    fn due_jobs(&self, unix_seconds: u64) -> agl_cron::Result<Vec<CronDueJob>> {
        due_jobs(&*self.lock().map_err(repository_error)?, unix_seconds)
    }
}

fn update_job(store: &AglStore, id: &str, update: CronJobUpdate) -> agl_cron::Result<CronJob> {
    validate_non_blank("id", id)?;
    let current = job(store, id)?.ok_or_else(|| CronError::NotFound { id: id.to_owned() })?;
    if current.deleted_at.is_some() {
        return Err(invalid("id", id, "cannot update deleted job"));
    }
    let draft = CronJobDraft {
        name: update.name.unwrap_or(current.name),
        enabled: update.enabled.unwrap_or(current.enabled),
        target_kind: update.target_kind.unwrap_or(current.target_kind),
        target_ref: update.target_ref.unwrap_or(current.target_ref),
        schedule_expr: update.schedule_expr.unwrap_or(current.schedule_expr),
        timezone: update.timezone.unwrap_or(current.timezone),
        notify_ref: update.notify_ref.unwrap_or(current.notify_ref),
        prompt: update.prompt.unwrap_or(current.prompt),
        input: update.input.unwrap_or(current.input),
    };
    validate_job_draft(&draft)?;
    store
        .connection()
        .execute(
            "UPDATE cron_jobs
         SET name = ?2, enabled = ?3, target_kind = ?4, target_ref = ?5,
             schedule_expr = ?6, timezone = ?7, notify_ref = ?8, prompt = ?9,
             input = ?10, updated_at = ?11
         WHERE id = ?1",
            params![
                id,
                draft.name,
                draft.enabled,
                draft.target_kind.as_str(),
                draft.target_ref,
                draft.schedule_expr,
                draft.timezone,
                draft.notify_ref,
                draft.prompt,
                draft.input,
                timestamp()
            ],
        )
        .map_err(repository_error)?;
    job(store, id)?.ok_or_else(|| CronError::NotFound { id: id.to_owned() })
}

fn add_job(store: &AglStore, draft: CronJobDraft) -> agl_cron::Result<CronJob> {
    validate_job_draft(&draft)?;
    let id = unique_id("cron_job");
    let now = timestamp();
    store
        .connection()
        .execute(
            "INSERT INTO cron_jobs
         (id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref,
          prompt, input, created_at, updated_at, deleted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, NULL)",
            params![
                id,
                draft.name,
                draft.enabled,
                draft.target_kind.as_str(),
                draft.target_ref,
                draft.schedule_expr,
                draft.timezone,
                draft.notify_ref,
                draft.prompt,
                draft.input,
                now
            ],
        )
        .map_err(repository_error)?;
    job(store, &id)?.ok_or(CronError::NotFound { id })
}

fn list_jobs(store: &AglStore, include_deleted: bool) -> agl_cron::Result<Vec<CronJob>> {
    Ok(all_jobs(store)?
        .into_iter()
        .filter(|job| include_deleted || job.deleted_at.is_none())
        .collect())
}

fn all_jobs(store: &AglStore) -> agl_cron::Result<Vec<CronJob>> {
    let mut statement = store.connection().prepare(
        "SELECT id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref,
                prompt, input, created_at, updated_at, deleted_at
         FROM cron_jobs ORDER BY updated_at DESC, id DESC",
    ).map_err(repository_error)?;
    let rows = statement
        .query_map([], job_from_row)
        .map_err(repository_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(repository_error)
}

fn job(store: &AglStore, id: &str) -> agl_cron::Result<Option<CronJob>> {
    validate_non_blank("id", id)?;
    store.connection().query_row(
        "SELECT id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref,
                prompt, input, created_at, updated_at, deleted_at
         FROM cron_jobs WHERE id = ?1",
        params![id],
        job_from_row,
    ).optional().map_err(repository_error)
}

fn set_enabled(store: &AglStore, id: &str, enabled: bool) -> agl_cron::Result<CronJob> {
    let current = job(store, id)?.ok_or_else(|| CronError::NotFound { id: id.to_owned() })?;
    if current.deleted_at.is_some() {
        return Err(invalid("id", id, "cannot update deleted job"));
    }
    store
        .connection()
        .execute(
            "UPDATE cron_jobs SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, enabled, timestamp()],
        )
        .map_err(repository_error)?;
    job(store, id)?.ok_or_else(|| CronError::NotFound { id: id.to_owned() })
}

fn delete_job(store: &AglStore, id: &str) -> agl_cron::Result<CronJob> {
    validate_non_blank("id", id)?;
    store
        .connection()
        .execute(
            "UPDATE cron_jobs SET deleted_at = COALESCE(deleted_at, ?2), updated_at = ?2
         WHERE id = ?1",
            params![id, timestamp()],
        )
        .map_err(repository_error)?;
    job(store, id)?.ok_or_else(|| CronError::NotFound { id: id.to_owned() })
}

fn record_manual_run(
    store: &AglStore,
    job_id: &str,
    result_ref: Option<&str>,
) -> agl_cron::Result<(CronRun, CronIdempotencyOutcome)> {
    record_manual_run_result(store, job_id, CronRunStatus::Succeeded, result_ref, None)
}

fn record_manual_run_result(
    store: &AglStore,
    job_id: &str,
    status: CronRunStatus,
    result_ref: Option<&str>,
    error: Option<&str>,
) -> agl_cron::Result<(CronRun, CronIdempotencyOutcome)> {
    let job_value = job(store, job_id)?.ok_or_else(|| CronError::NotFound {
        id: job_id.to_owned(),
    })?;
    validate_runnable_job(&job_value)?;
    record_run_for(store, job_id, &timestamp(), status, result_ref, error)
}

fn record_run_for(
    store: &AglStore,
    job_id: &str,
    scheduled_for: &str,
    status: CronRunStatus,
    result_ref: Option<&str>,
    error: Option<&str>,
) -> agl_cron::Result<(CronRun, CronIdempotencyOutcome)> {
    let job_value = job(store, job_id)?.ok_or_else(|| CronError::NotFound {
        id: job_id.to_owned(),
    })?;
    match begin_run_for(store, &job_value, scheduled_for)? {
        CronRunAdmission::Inserted(outcome) => {
            let run = record_admitted_run(store, job_id, scheduled_for, status, result_ref, error)?;
            Ok((run, outcome))
        }
        CronRunAdmission::Replayed(run, outcome) => Ok((run, outcome)),
        CronRunAdmission::Pending(_) => Err(invalid(
            "idempotency",
            format!("{job_id}:{scheduled_for}"),
            "cron run is already admitted but has no recorded result",
        )),
    }
}

fn begin_run_for(
    store: &AglStore,
    job_value: &CronJob,
    scheduled_for: &str,
) -> agl_cron::Result<CronRunAdmission> {
    validate_runnable_job(job_value)?;
    validate_non_blank("scheduled_for", scheduled_for)?;
    let key = idempotency_key(&job_value.id, scheduled_for);
    let fingerprint = idempotency_fingerprint(job_value);
    let outcome = store
        .begin_idempotency(IDEMPOTENCY_NAMESPACE, &key, &fingerprint)
        .map_err(cron_store_error)?;
    let domain_outcome = convert_idempotency(&outcome);
    if let IdempotencyOutcome::Replayed(record) = outcome {
        if let Some(result_ref) = record.result_ref
            && let Some(run) = run(store.connection(), &result_ref)?
        {
            return Ok(CronRunAdmission::Replayed(run, domain_outcome));
        }
        return Ok(CronRunAdmission::Pending(domain_outcome));
    }
    Ok(CronRunAdmission::Inserted(domain_outcome))
}

fn record_admitted_run(
    store: &AglStore,
    job_id: &str,
    scheduled_for: &str,
    status: CronRunStatus,
    result_ref: Option<&str>,
    error: Option<&str>,
) -> agl_cron::Result<CronRun> {
    validate_non_blank("job_id", job_id)?;
    validate_non_blank("scheduled_for", scheduled_for)?;
    let id = unique_id("cron_run");
    let now = timestamp();
    let tx = store
        .connection()
        .unchecked_transaction()
        .map_err(repository_error)?;
    tx.execute(
        "INSERT INTO cron_runs
         (id, job_id, scheduled_for, started_at, finished_at, status, result_ref, error)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7)",
        params![
            id,
            job_id,
            scheduled_for,
            now,
            status.as_str(),
            result_ref,
            error
        ],
    )
    .map_err(repository_error)?;
    finish_idempotency_on_connection(&tx, job_id, scheduled_for, status, &id)?;
    let result = run(&tx, &id)?.ok_or_else(|| CronError::NotFound { id: id.clone() })?;
    tx.commit().map_err(repository_error)?;
    Ok(result)
}

fn record_admitted_supervisor_run(
    store: &AglStore,
    job_id: &str,
    scheduled_for: &str,
    supervisor_run_id: &RunId,
) -> agl_cron::Result<CronRun> {
    validate_non_blank("job_id", job_id)?;
    validate_non_blank("scheduled_for", scheduled_for)?;
    let tx = store
        .connection()
        .unchecked_transaction()
        .map_err(repository_error)?;
    if let Some(existing) = run_for_schedule(&tx, job_id, scheduled_for)? {
        if existing.supervisor_run_id.as_ref() == Some(supervisor_run_id) {
            return Ok(existing);
        }
        return Err(invalid(
            "supervisor_run_id",
            supervisor_run_id.to_string(),
            "scheduled cron run is linked to another supervisor run",
        ));
    }
    let id = unique_id("cron_run");
    tx.execute(
        "INSERT INTO cron_runs
         (id, job_id, scheduled_for, started_at, finished_at, status, result_ref, error,
          supervisor_run_id)
         VALUES (?1, ?2, ?3, NULL, NULL, 'queued', ?4, NULL, ?5)",
        params![
            id,
            job_id,
            scheduled_for,
            format!("run:{supervisor_run_id}"),
            supervisor_run_id.as_str()
        ],
    )
    .map_err(repository_error)?;
    let changed = tx
        .execute(
            "UPDATE idempotency_keys SET result_ref = ?3, updated_at = ?4
         WHERE namespace = ?1 AND key = ?2 AND status = 'in_progress'",
            params![
                IDEMPOTENCY_NAMESPACE,
                idempotency_key(job_id, scheduled_for),
                id,
                timestamp()
            ],
        )
        .map_err(repository_error)?;
    if changed != 1 {
        return Err(CronError::Repository {
            reason: "cron admission idempotency record is missing or terminal".to_owned(),
        });
    }
    let result = run(&tx, &id)?.ok_or_else(|| CronError::NotFound { id: id.clone() })?;
    tx.commit().map_err(repository_error)?;
    Ok(result)
}

fn active_supervisor_runs(store: &AglStore) -> agl_cron::Result<Vec<CronRun>> {
    let mut statement = store
        .connection()
        .prepare(
            "SELECT id, job_id, scheduled_for, started_at, finished_at, status, result_ref,
                error, supervisor_run_id
         FROM cron_runs
         WHERE status IN ('queued', 'running') AND supervisor_run_id IS NOT NULL
         ORDER BY scheduled_for, id",
        )
        .map_err(repository_error)?;
    let rows = statement
        .query_map([], run_from_row)
        .map_err(repository_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(repository_error)
}

fn finish_supervisor_run(
    store: &AglStore,
    supervisor_run_id: &RunId,
    status: CronRunStatus,
    result_ref: Option<&str>,
    error: Option<&str>,
) -> agl_cron::Result<CronRun> {
    if !matches!(
        status,
        CronRunStatus::Succeeded | CronRunStatus::Failed | CronRunStatus::Skipped
    ) {
        return Err(invalid(
            "status",
            status.as_str(),
            "linked supervisor completion must be terminal",
        ));
    }
    let tx = store
        .connection()
        .unchecked_transaction()
        .map_err(repository_error)?;
    let now = timestamp();
    tx.execute(
        "UPDATE cron_runs
         SET status = ?2, started_at = COALESCE(started_at, ?3), finished_at = ?3,
             result_ref = ?4, error = ?5
         WHERE supervisor_run_id = ?1 AND status IN ('queued', 'running')",
        params![
            supervisor_run_id.as_str(),
            status.as_str(),
            now,
            result_ref,
            error
        ],
    )
    .map_err(repository_error)?;
    let result =
        run_for_supervisor(&tx, supervisor_run_id)?.ok_or_else(|| CronError::NotFound {
            id: supervisor_run_id.to_string(),
        })?;
    finish_idempotency_on_connection(
        &tx,
        &result.job_id,
        &result.scheduled_for,
        status,
        &result.id,
    )?;
    tx.commit().map_err(repository_error)?;
    Ok(result)
}

fn history(store: &AglStore, job_id: &str) -> agl_cron::Result<Vec<CronRun>> {
    validate_non_blank("job_id", job_id)?;
    let mut statement = store
        .connection()
        .prepare(
            "SELECT id, job_id, scheduled_for, started_at, finished_at, status, result_ref,
                error, supervisor_run_id
         FROM cron_runs WHERE job_id = ?1 ORDER BY scheduled_for DESC, id DESC",
        )
        .map_err(repository_error)?;
    let rows = statement
        .query_map(params![job_id], run_from_row)
        .map_err(repository_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(repository_error)
}

fn due_jobs(store: &AglStore, unix_seconds: u64) -> agl_cron::Result<Vec<CronDueJob>> {
    let scheduled_for = format!("unix:{}", unix_seconds - unix_seconds % 60);
    let mut due = Vec::new();
    for job in all_jobs(store)?
        .into_iter()
        .filter(|job| job.enabled && job.deleted_at.is_none())
    {
        if schedule_matches(&job.schedule_expr, unix_seconds, &job.timezone)? {
            due.push(CronDueJob {
                job,
                scheduled_for: scheduled_for.clone(),
            });
        }
    }
    Ok(due)
}

fn finish_idempotency_on_connection(
    conn: &Connection,
    job_id: &str,
    scheduled_for: &str,
    status: CronRunStatus,
    run_id: &str,
) -> agl_cron::Result<()> {
    let idempotency_status = match status {
        CronRunStatus::Succeeded => "completed",
        CronRunStatus::Failed => "failed",
        CronRunStatus::Skipped => "skipped",
        CronRunStatus::Queued | CronRunStatus::Running => return Ok(()),
    };
    let changed = conn
        .execute(
            "UPDATE idempotency_keys
         SET status = ?3, result_ref = ?4, lease_owner = NULL, lease_expires_at_ms = NULL,
             updated_at = ?5
         WHERE namespace = ?1 AND key = ?2 AND status = 'in_progress'",
            params![
                IDEMPOTENCY_NAMESPACE,
                idempotency_key(job_id, scheduled_for),
                idempotency_status,
                run_id,
                timestamp()
            ],
        )
        .map_err(repository_error)?;
    if changed != 1 {
        return Err(CronError::Repository {
            reason: "cron run and idempotency record cannot be committed atomically".to_owned(),
        });
    }
    Ok(())
}

fn run(conn: &Connection, id: &str) -> agl_cron::Result<Option<CronRun>> {
    conn.query_row(
        "SELECT id, job_id, scheduled_for, started_at, finished_at, status, result_ref,
                error, supervisor_run_id FROM cron_runs WHERE id = ?1",
        params![id],
        run_from_row,
    )
    .optional()
    .map_err(repository_error)
}

fn run_for_schedule(
    conn: &Connection,
    job_id: &str,
    scheduled_for: &str,
) -> agl_cron::Result<Option<CronRun>> {
    conn.query_row(
        "SELECT id, job_id, scheduled_for, started_at, finished_at, status, result_ref,
                error, supervisor_run_id
         FROM cron_runs WHERE job_id = ?1 AND scheduled_for = ?2",
        params![job_id, scheduled_for],
        run_from_row,
    )
    .optional()
    .map_err(repository_error)
}

fn run_for_supervisor(conn: &Connection, run_id: &RunId) -> agl_cron::Result<Option<CronRun>> {
    conn.query_row(
        "SELECT id, job_id, scheduled_for, started_at, finished_at, status, result_ref,
                error, supervisor_run_id FROM cron_runs WHERE supervisor_run_id = ?1",
        params![run_id.as_str()],
        run_from_row,
    )
    .optional()
    .map_err(repository_error)
}

fn job_from_row(row: &Row<'_>) -> rusqlite::Result<CronJob> {
    let target_kind = parse_row(CronTargetKind::parse(&row.get::<_, String>(3)?), 3)?;
    Ok(CronJob {
        id: row.get(0)?,
        name: row.get(1)?,
        enabled: row.get(2)?,
        target_kind,
        target_ref: row.get(4)?,
        schedule_expr: row.get(5)?,
        timezone: row.get(6)?,
        notify_ref: row.get(7)?,
        prompt: row.get(8)?,
        input: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        deleted_at: row.get(12)?,
    })
}

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<CronRun> {
    let status = parse_row(CronRunStatus::parse(&row.get::<_, String>(5)?), 5)?;
    let supervisor = row.get::<_, Option<String>>(8)?;
    let supervisor_run_id = supervisor
        .map(|value| {
            RunId::parse(&value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    8,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()?;
    Ok(CronRun {
        id: row.get(0)?,
        job_id: row.get(1)?,
        scheduled_for: row.get(2)?,
        started_at: row.get(3)?,
        finished_at: row.get(4)?,
        status,
        result_ref: row.get(6)?,
        error: row.get(7)?,
        supervisor_run_id,
    })
}

fn parse_row<T>(result: agl_cron::Result<T>, column: usize) -> rusqlite::Result<T> {
    result.map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn convert_idempotency(outcome: &IdempotencyOutcome) -> CronIdempotencyOutcome {
    match outcome {
        IdempotencyOutcome::Inserted(record) => {
            CronIdempotencyOutcome::Inserted(convert_idempotency_record(record))
        }
        IdempotencyOutcome::Replayed(record) => {
            CronIdempotencyOutcome::Replayed(convert_idempotency_record(record))
        }
    }
}

fn convert_idempotency_record(record: &IdempotencyRecord) -> CronIdempotencyRecord {
    CronIdempotencyRecord {
        key: record.key.clone(),
        fingerprint: record.fingerprint.clone(),
        status: match record.status {
            IdempotencyStatus::InProgress => CronIdempotencyStatus::InProgress,
            IdempotencyStatus::Completed => CronIdempotencyStatus::Completed,
            IdempotencyStatus::Failed => CronIdempotencyStatus::Failed,
            IdempotencyStatus::Skipped => CronIdempotencyStatus::Skipped,
        },
        result_ref: record.result_ref.clone(),
    }
}

fn validate_runnable_job(job: &CronJob) -> agl_cron::Result<()> {
    if job.deleted_at.is_some() {
        Err(invalid("job_id", &job.id, "cannot run deleted job"))
    } else {
        Ok(())
    }
}

fn validate_non_blank(field: &'static str, value: &str) -> agl_cron::Result<()> {
    if value.trim().is_empty() {
        Err(invalid(field, value, "value cannot be blank"))
    } else {
        Ok(())
    }
}

fn idempotency_key(job_id: &str, scheduled_for: &str) -> String {
    format!("{job_id}:{scheduled_for}")
}

fn idempotency_fingerprint(job: &CronJob) -> String {
    format!(
        "target:{}:{} schedule:{} timezone:{} notify:{:?} prompt:{:?} input:{:?}",
        job.target_kind.as_str(),
        job.target_ref,
        job.schedule_expr,
        job.timezone,
        job.notify_ref,
        job.prompt,
        job.input,
    )
}

fn schedule_matches(expr: &str, unix_seconds: u64, timezone: &str) -> agl_cron::Result<bool> {
    let offset = timezone_offset(timezone)?;
    let local_seconds = unix_seconds as i64 + i64::from(offset);
    let minute = local_seconds.div_euclid(60).rem_euclid(60) as u32;
    let hour = local_seconds.div_euclid(3600).rem_euclid(24) as u32;
    let days = local_seconds.div_euclid(86_400);
    let (_, month, day) = civil_from_unix_days(days);
    let weekday_sunday_zero = ((days + 4).rem_euclid(7)) as u32;
    let expr = expr.trim();
    if expr == "hourly" {
        return Ok(minute == 0);
    }
    let parts = expr.split_whitespace().collect::<Vec<_>>();
    if parts.len() == 2 && parts[0] == "daily" {
        let (expected_hour, expected_minute) = parse_time(parts[1])?;
        return Ok(hour == expected_hour && minute == expected_minute);
    }
    if parts.len() == 3 && parts[0] == "weekly" {
        let expected_weekday = match parts[1] {
            "mon" => 0,
            "tue" => 1,
            "wed" => 2,
            "thu" => 3,
            "fri" => 4,
            "sat" => 5,
            "sun" => 6,
            _ => return Err(invalid("schedule_expr", expr, "invalid weekday")),
        };
        let (expected_hour, expected_minute) = parse_time(parts[2])?;
        return Ok((weekday_sunday_zero + 6) % 7 == expected_weekday
            && hour == expected_hour
            && minute == expected_minute);
    }
    if parts.len() == 5 {
        let dom = cron_field_matches(parts[2], day, 1, 31)?;
        let dow = cron_field_matches(parts[4], weekday_sunday_zero, 0, 7)?;
        let day_match = if parts[2] != "*" && parts[4] != "*" {
            dom || dow
        } else {
            dom && dow
        };
        return Ok(cron_field_matches(parts[0], minute, 0, 59)?
            && cron_field_matches(parts[1], hour, 0, 23)?
            && day_match
            && cron_field_matches(parts[3], month, 1, 12)?);
    }
    Err(invalid(
        "schedule_expr",
        expr,
        "invalid schedule expression",
    ))
}

fn cron_field_matches(field: &str, value: u32, min: u32, max: u32) -> agl_cron::Result<bool> {
    for part in field.split(',') {
        let (base, step) = part.split_once('/').map_or((part, 1), |(base, step)| {
            (base, step.parse::<u32>().unwrap_or(0))
        });
        if step == 0 {
            return Err(invalid("schedule_expr", part, "cron step must be positive"));
        }
        let (start, end) = if base == "*" {
            (min, max)
        } else if let Some((start, end)) = base.split_once('-') {
            (
                parse_cron_number(start, min, max)?,
                parse_cron_number(end, min, max)?,
            )
        } else {
            let number = parse_cron_number(base, min, max)?;
            (number, number)
        };
        if start <= value && value <= end && (value - start).is_multiple_of(step) {
            return Ok(true);
        }
        if max == 7 && value == 0 && start <= 7 && 7 <= end && (7 - start).is_multiple_of(step) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn parse_cron_number(value: &str, min: u32, max: u32) -> agl_cron::Result<u32> {
    let number = value
        .parse::<u32>()
        .map_err(|_| invalid("schedule_expr", value, "cron field must be numeric"))?;
    if number < min || number > max {
        return Err(invalid(
            "schedule_expr",
            value,
            "cron field is out of range",
        ));
    }
    Ok(number)
}

fn parse_time(value: &str) -> agl_cron::Result<(u32, u32)> {
    let Some((hour, minute)) = value.split_once(':') else {
        return Err(invalid("schedule_expr", value, "invalid time"));
    };
    let hour = hour
        .parse::<u32>()
        .map_err(|_| invalid("schedule_expr", value, "invalid time"))?;
    let minute = minute
        .parse::<u32>()
        .map_err(|_| invalid("schedule_expr", value, "invalid time"))?;
    if hour > 23 || minute > 59 {
        return Err(invalid("schedule_expr", value, "invalid time"));
    }
    Ok((hour, minute))
}

fn timezone_offset(value: &str) -> agl_cron::Result<i32> {
    if matches!(value, "UTC" | "Z") {
        return Ok(0);
    }
    let offset = value.strip_prefix("UTC").unwrap_or(value);
    let sign = match offset.as_bytes().first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return Err(invalid("timezone", value, "invalid fixed offset")),
    };
    let (hour, minute) = parse_time(&offset[1..])?;
    Ok(sign * (hour as i32 * 3_600 + minute as i32 * 60))
}

fn civil_from_unix_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(month <= 2);
    (year as i32, month as u32, day as u32)
}

fn cron_store_error(error: StoreError) -> CronError {
    match error {
        StoreError::IdempotencyConflict { key, .. } => CronError::IdempotencyConflict { key },
        other => repository_error(other),
    }
}

fn repository_error(error: impl std::fmt::Display) -> CronError {
    CronError::Repository {
        reason: error.to_string(),
    }
}

fn invalid(field: &'static str, value: impl Into<String>, reason: &'static str) -> CronError {
    CronError::InvalidValue {
        field,
        value: value.into(),
        reason,
    }
}

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{seconds}")
}

fn unique_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}_{}_{}", std::process::id(), nanos)
}
