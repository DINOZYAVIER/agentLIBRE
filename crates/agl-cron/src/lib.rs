use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use agl_store::{AglStore, IdempotencyOutcome, StoreError};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, CronError>;

const DEFAULT_TIMEZONE: &str = "local";
const IDEMPOTENCY_NAMESPACE: &str = "cron.run";

#[derive(Debug)]
pub enum CronError {
    InvalidValue {
        field: &'static str,
        value: String,
        reason: &'static str,
    },
    NotFound {
        id: String,
    },
    Store(StoreError),
    Sqlite(rusqlite::Error),
}

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidValue {
                field,
                value,
                reason,
            } => write!(f, "invalid cron {field} value {value:?}: {reason}"),
            Self::NotFound { id } => write!(f, "cron record not found: {id}"),
            Self::Store(err) => write!(f, "{err}"),
            Self::Sqlite(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for CronError {}

impl From<StoreError> for CronError {
    fn from(err: StoreError) -> Self {
        Self::Store(err)
    }
}

impl From<rusqlite::Error> for CronError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronTargetKind {
    Skill,
    Builtin,
}

impl CronTargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Builtin => "builtin",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "skill" => Ok(Self::Skill),
            "builtin" => Ok(Self::Builtin),
            _ => Err(CronError::InvalidValue {
                field: "target_kind",
                value: value.to_string(),
                reason: "unknown cron target kind",
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronRunStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Skipped,
}

impl CronRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "skipped" => Ok(Self::Skipped),
            _ => Err(CronError::InvalidValue {
                field: "status",
                value: value.to_string(),
                reason: "unknown cron run status",
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub target_kind: CronTargetKind,
    pub target_ref: String,
    pub schedule_expr: String,
    pub timezone: String,
    pub notify_ref: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CronJobDraft {
    pub name: String,
    pub enabled: bool,
    pub target_kind: CronTargetKind,
    pub target_ref: String,
    pub schedule_expr: String,
    pub timezone: String,
    pub notify_ref: Option<String>,
}

impl CronJobDraft {
    pub fn new(
        name: impl Into<String>,
        target_kind: CronTargetKind,
        target_ref: impl Into<String>,
        schedule_expr: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            enabled: true,
            target_kind,
            target_ref: target_ref.into(),
            schedule_expr: schedule_expr.into(),
            timezone: DEFAULT_TIMEZONE.to_string(),
            notify_ref: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CronRun {
    pub id: String,
    pub job_id: String,
    pub scheduled_for: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub status: CronRunStatus,
    pub result_ref: Option<String>,
    pub error: Option<String>,
}

pub struct CronRepository<'a> {
    store: &'a AglStore,
}

impl<'a> CronRepository<'a> {
    pub fn new(store: &'a AglStore) -> Self {
        Self { store }
    }

    pub fn add_job(&self, draft: CronJobDraft) -> Result<CronJob> {
        validate_draft(&draft)?;
        let id = cron_id("cron_job");
        let now = timestamp();
        self.store.connection().execute(
            "INSERT INTO cron_jobs
             (id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, created_at, updated_at, deleted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, NULL)",
            params![
                id,
                draft.name,
                draft.enabled,
                draft.target_kind.as_str(),
                draft.target_ref,
                draft.schedule_expr,
                draft.timezone,
                draft.notify_ref,
                now
            ],
        )?;
        self.job(&id)?
            .ok_or_else(|| CronError::NotFound { id: id.to_string() })
    }

    pub fn list_jobs(&self, include_deleted: bool) -> Result<Vec<CronJob>> {
        if include_deleted {
            self.query_jobs(
                "SELECT id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, created_at, updated_at, deleted_at
                 FROM cron_jobs
                 ORDER BY updated_at DESC, id DESC",
                [],
            )
        } else {
            self.query_jobs(
                "SELECT id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, created_at, updated_at, deleted_at
                 FROM cron_jobs
                 WHERE deleted_at IS NULL
                 ORDER BY updated_at DESC, id DESC",
                [],
            )
        }
    }

    pub fn job(&self, id: &str) -> Result<Option<CronJob>> {
        validate_non_blank("id", id)?;
        self.store
            .connection()
            .query_row(
                "SELECT id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, created_at, updated_at, deleted_at
                 FROM cron_jobs
                 WHERE id = ?1",
                params![id],
                job_from_row,
            )
            .optional()?
            .transpose()
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<CronJob> {
        validate_non_blank("id", id)?;
        let now = timestamp();
        self.store.connection().execute(
            "UPDATE cron_jobs
             SET enabled = ?2, updated_at = ?3
             WHERE id = ?1",
            params![id, enabled, now],
        )?;
        self.job(id)?
            .ok_or_else(|| CronError::NotFound { id: id.to_string() })
    }

    pub fn delete_job(&self, id: &str) -> Result<CronJob> {
        validate_non_blank("id", id)?;
        let now = timestamp();
        self.store.connection().execute(
            "UPDATE cron_jobs
             SET deleted_at = COALESCE(deleted_at, ?2), updated_at = ?2
             WHERE id = ?1",
            params![id, now],
        )?;
        self.job(id)?
            .ok_or_else(|| CronError::NotFound { id: id.to_string() })
    }

    pub fn record_manual_run(
        &self,
        job_id: &str,
        result_ref: Option<&str>,
    ) -> Result<(CronRun, IdempotencyOutcome)> {
        let job = self.job(job_id)?.ok_or_else(|| CronError::NotFound {
            id: job_id.to_string(),
        })?;
        validate_runnable_job(&job)?;
        let scheduled_for = timestamp();
        self.record_run_for(
            job_id,
            &scheduled_for,
            CronRunStatus::Succeeded,
            result_ref,
            None,
        )
    }

    pub fn record_run_for(
        &self,
        job_id: &str,
        scheduled_for: &str,
        status: CronRunStatus,
        result_ref: Option<&str>,
        error: Option<&str>,
    ) -> Result<(CronRun, IdempotencyOutcome)> {
        validate_non_blank("job_id", job_id)?;
        validate_non_blank("scheduled_for", scheduled_for)?;
        let fingerprint = format!("status:{} result:{result_ref:?}", status.as_str());
        let idempotency_key = format!("{job_id}:{scheduled_for}");
        let outcome =
            self.store
                .begin_idempotency(IDEMPOTENCY_NAMESPACE, &idempotency_key, &fingerprint)?;
        if let IdempotencyOutcome::Replayed(record) = &outcome
            && let Some(result_ref) = &record.result_ref
            && let Some(run) = self.run(result_ref)?
        {
            return Ok((run, outcome));
        }

        let id = cron_id("cron_run");
        let now = timestamp();
        self.store.connection().execute(
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
        )?;
        let run = self
            .run(&id)?
            .ok_or_else(|| CronError::NotFound { id: id.clone() })?;
        match status {
            CronRunStatus::Succeeded => {
                self.store.complete_idempotency(
                    IDEMPOTENCY_NAMESPACE,
                    &idempotency_key,
                    Some(&id),
                )?;
            }
            CronRunStatus::Failed => {
                self.store
                    .fail_idempotency(IDEMPOTENCY_NAMESPACE, &idempotency_key, Some(&id))?;
            }
            CronRunStatus::Skipped => {
                self.store
                    .skip_idempotency(IDEMPOTENCY_NAMESPACE, &idempotency_key, Some(&id))?;
            }
            CronRunStatus::Queued | CronRunStatus::Running => {}
        }
        Ok((run, outcome))
    }

    pub fn history(&self, job_id: &str) -> Result<Vec<CronRun>> {
        validate_non_blank("job_id", job_id)?;
        let mut stmt = self.store.connection().prepare(
            "SELECT id, job_id, scheduled_for, started_at, finished_at, status, result_ref, error
             FROM cron_runs
             WHERE job_id = ?1
             ORDER BY scheduled_for DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![job_id], run_from_row)?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(row??);
        }
        Ok(runs)
    }

    fn run(&self, id: &str) -> Result<Option<CronRun>> {
        self.store
            .connection()
            .query_row(
                "SELECT id, job_id, scheduled_for, started_at, finished_at, status, result_ref, error
                 FROM cron_runs
                 WHERE id = ?1",
                params![id],
                run_from_row,
            )
            .optional()?
            .transpose()
    }

    fn query_jobs<P>(&self, sql: &str, params: P) -> Result<Vec<CronJob>>
    where
        P: rusqlite::Params,
    {
        let mut stmt = self.store.connection().prepare(sql)?;
        let rows = stmt.query_map(params, job_from_row)?;
        let mut jobs = Vec::new();
        for row in rows {
            jobs.push(row??);
        }
        Ok(jobs)
    }
}

pub fn validate_schedule_expr(value: &str) -> Result<()> {
    let value = value.trim();
    validate_non_blank("schedule_expr", value)?;
    if value == "hourly" {
        return Ok(());
    }
    let parts = value.split_whitespace().collect::<Vec<_>>();
    if parts.len() == 2 && parts[0] == "daily" && valid_time(parts[1]) {
        return Ok(());
    }
    if parts.len() == 3 && parts[0] == "weekly" && valid_weekday(parts[1]) && valid_time(parts[2]) {
        return Ok(());
    }
    if parts.len() == 5 && parts.iter().all(|part| valid_cron_field(part)) {
        return Ok(());
    }
    Err(CronError::InvalidValue {
        field: "schedule_expr",
        value: value.to_string(),
        reason: "expected hourly, daily HH:MM, weekly <weekday> HH:MM, or 5-field cron expression",
    })
}

fn validate_draft(draft: &CronJobDraft) -> Result<()> {
    validate_non_blank("name", &draft.name)?;
    validate_non_blank("target_ref", &draft.target_ref)?;
    validate_schedule_expr(&draft.schedule_expr)?;
    validate_non_blank("timezone", &draft.timezone)?;
    if let Some(notify_ref) = &draft.notify_ref {
        validate_non_blank("notify_ref", notify_ref)?;
    }
    Ok(())
}

fn validate_runnable_job(job: &CronJob) -> Result<()> {
    if job.deleted_at.is_some() {
        return Err(CronError::InvalidValue {
            field: "job_id",
            value: job.id.clone(),
            reason: "cannot run deleted job",
        });
    }
    Ok(())
}

fn job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<CronJob>> {
    let target_kind: String = row.get(3)?;
    Ok((|| {
        Ok(CronJob {
            id: row.get(0)?,
            name: row.get(1)?,
            enabled: row.get(2)?,
            target_kind: CronTargetKind::parse(&target_kind)?,
            target_ref: row.get(4)?,
            schedule_expr: row.get(5)?,
            timezone: row.get(6)?,
            notify_ref: row.get(7)?,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
            deleted_at: row.get(10)?,
        })
    })())
}

fn run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<CronRun>> {
    let status: String = row.get(5)?;
    Ok((|| {
        Ok(CronRun {
            id: row.get(0)?,
            job_id: row.get(1)?,
            scheduled_for: row.get(2)?,
            started_at: row.get(3)?,
            finished_at: row.get(4)?,
            status: CronRunStatus::parse(&status)?,
            result_ref: row.get(6)?,
            error: row.get(7)?,
        })
    })())
}

fn validate_non_blank(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(CronError::InvalidValue {
            field,
            value: value.to_string(),
            reason: "value cannot be blank",
        });
    }
    Ok(())
}

fn valid_time(value: &str) -> bool {
    let Some((hour, minute)) = value.split_once(':') else {
        return false;
    };
    matches!(
        (hour.parse::<u8>(), minute.parse::<u8>()),
        (Ok(hour), Ok(minute)) if hour < 24 && minute < 60
    )
}

fn valid_weekday(value: &str) -> bool {
    matches!(value, "mon" | "tue" | "wed" | "thu" | "fri" | "sat" | "sun")
}

fn valid_cron_field(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '*' | '/' | ',' | '-'))
}

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{seconds}")
}

fn cron_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}_{}_{}", std::process::id(), nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn validates_human_and_cron_schedules() {
        for value in ["hourly", "daily 09:00", "weekly mon 10:30", "0 9 * * *"] {
            validate_schedule_expr(value).unwrap();
        }

        assert!(validate_schedule_expr("daily 99:00").is_err());
        assert!(validate_schedule_expr("weekly monday 10:00").is_err());
    }

    #[test]
    fn adds_lists_and_toggles_jobs() {
        let root = temp_root("jobs");
        let store = AglStore::open_at(&root).unwrap();
        let repo = CronRepository::new(&store);

        let job = repo
            .add_job(CronJobDraft::new(
                "Daily review",
                CronTargetKind::Skill,
                "repo-review",
                "daily 09:00",
            ))
            .unwrap();
        let disabled = repo.set_enabled(&job.id, false).unwrap();
        let jobs = repo.list_jobs(false).unwrap();

        assert!(!disabled.enabled);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "Daily review");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manual_run_records_history_and_idempotency() {
        let root = temp_root("run");
        let store = AglStore::open_at(&root).unwrap();
        let repo = CronRepository::new(&store);
        let job = repo
            .add_job(CronJobDraft::new(
                "Store status",
                CronTargetKind::Builtin,
                "store-status",
                "0 9 * * *",
            ))
            .unwrap();

        let (run, outcome) = repo
            .record_manual_run(&job.id, Some("builtin:store-status"))
            .unwrap();
        let history = repo.history(&job.id).unwrap();

        assert!(matches!(outcome, IdempotencyOutcome::Inserted(_)));
        assert_eq!(run.status, CronRunStatus::Succeeded);
        assert_eq!(history, vec![run]);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn record_run_for_replays_same_scheduled_time() {
        let root = temp_root("replay");
        let store = AglStore::open_at(&root).unwrap();
        let repo = CronRepository::new(&store);
        let job = repo
            .add_job(CronJobDraft::new(
                "Store status",
                CronTargetKind::Builtin,
                "store-status",
                "hourly",
            ))
            .unwrap();

        let (first, first_outcome) = repo
            .record_run_for(
                &job.id,
                "unix:100",
                CronRunStatus::Succeeded,
                Some("builtin:store-status"),
                None,
            )
            .unwrap();
        let (second, second_outcome) = repo
            .record_run_for(
                &job.id,
                "unix:100",
                CronRunStatus::Succeeded,
                Some("builtin:store-status"),
                None,
            )
            .unwrap();

        assert!(matches!(first_outcome, IdempotencyOutcome::Inserted(_)));
        assert!(matches!(second_outcome, IdempotencyOutcome::Replayed(_)));
        assert_eq!(first, second);

        std::fs::remove_dir_all(root).unwrap();
    }

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("agl-cron-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }
}
