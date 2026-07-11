use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use agl_ids::RunId;
use agl_store::{AglStore, IdempotencyOutcome, StoreError};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, CronError>;

const DEFAULT_TIMEZONE: &str = "UTC";
const IDEMPOTENCY_NAMESPACE: &str = "cron.run";

pub const STORE_STATUS_BUILTIN_CRON_TARGET: &str = "store-status";

pub fn supported_builtin_cron_targets() -> &'static [&'static str] {
    &[STORE_STATUS_BUILTIN_CRON_TARGET]
}

pub fn validate_builtin_cron_target(target_ref: &str) -> std::result::Result<(), String> {
    if supported_builtin_cron_targets().contains(&target_ref) {
        return Ok(());
    }
    Err(unsupported_builtin_cron_target_message(target_ref))
}

pub fn unsupported_builtin_cron_target_message(target_ref: &str) -> String {
    format!(
        "unknown builtin cron target: {target_ref}; supported builtin targets: {}",
        supported_builtin_cron_targets().join(", ")
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CronFieldKind {
    Minute,
    Hour,
    DayOfMonth,
    Month,
    DayOfWeek,
}

impl CronFieldKind {
    fn range(self) -> (u32, u32) {
        match self {
            Self::Minute => (0, 59),
            Self::Hour => (0, 23),
            Self::DayOfMonth => (1, 31),
            Self::Month => (1, 12),
            Self::DayOfWeek => (0, 7),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UtcMinuteFields {
    minute: u32,
    hour: u32,
    day_of_month: u32,
    month: u32,
    weekday_sunday_zero: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TimezoneOffset {
    seconds: i32,
}

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
    pub prompt: Option<String>,
    pub input: Option<String>,
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
    pub prompt: Option<String>,
    pub input: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CronJobUpdate {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub target_kind: Option<CronTargetKind>,
    pub target_ref: Option<String>,
    pub schedule_expr: Option<String>,
    pub timezone: Option<String>,
    pub notify_ref: Option<Option<String>>,
    pub prompt: Option<Option<String>>,
    pub input: Option<Option<String>>,
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
            prompt: None,
            input: None,
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
    pub supervisor_run_id: Option<RunId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CronDueJob {
    pub job: CronJob,
    pub scheduled_for: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CronRunAdmission {
    Inserted(IdempotencyOutcome),
    Replayed(CronRun, IdempotencyOutcome),
    Pending(IdempotencyOutcome),
}

pub struct CronRepository<'a> {
    store: &'a AglStore,
}

impl<'a> CronRepository<'a> {
    pub fn new(store: &'a AglStore) -> Self {
        Self { store }
    }

    pub fn update_job(&self, id: &str, update: CronJobUpdate) -> Result<CronJob> {
        validate_non_blank("id", id)?;
        let current = self
            .job(id)?
            .ok_or_else(|| CronError::NotFound { id: id.to_string() })?;
        if current.deleted_at.is_some() {
            return Err(CronError::InvalidValue {
                field: "id",
                value: id.to_string(),
                reason: "cannot update deleted job",
            });
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
        validate_draft(&draft)?;
        let now = timestamp();
        self.store.connection().execute(
            "UPDATE cron_jobs
             SET name = ?2, enabled = ?3, target_kind = ?4, target_ref = ?5, schedule_expr = ?6,
                 timezone = ?7, notify_ref = ?8, prompt = ?9, input = ?10, updated_at = ?11
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
                now
            ],
        )?;
        self.job(id)?
            .ok_or_else(|| CronError::NotFound { id: id.to_string() })
    }

    pub fn add_job(&self, draft: CronJobDraft) -> Result<CronJob> {
        validate_draft(&draft)?;
        let id = cron_id("cron_job");
        let now = timestamp();
        self.store.connection().execute(
            "INSERT INTO cron_jobs
             (id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, prompt, input, created_at, updated_at, deleted_at)
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
        )?;
        self.job(&id)?
            .ok_or_else(|| CronError::NotFound { id: id.to_string() })
    }

    pub fn list_jobs(&self, include_deleted: bool) -> Result<Vec<CronJob>> {
        if include_deleted {
            self.query_jobs(
                "SELECT id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, prompt, input, created_at, updated_at, deleted_at
                 FROM cron_jobs
                 ORDER BY updated_at DESC, id DESC",
                [],
            )
        } else {
            self.query_jobs(
                "SELECT id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, prompt, input, created_at, updated_at, deleted_at
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
                "SELECT id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, prompt, input, created_at, updated_at, deleted_at
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
        let current = self
            .job(id)?
            .ok_or_else(|| CronError::NotFound { id: id.to_string() })?;
        if current.deleted_at.is_some() {
            return Err(CronError::InvalidValue {
                field: "id",
                value: id.to_string(),
                reason: "cannot update deleted job",
            });
        }
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
        self.record_manual_run_result(job_id, CronRunStatus::Succeeded, result_ref, None)
    }

    pub fn record_manual_run_result(
        &self,
        job_id: &str,
        status: CronRunStatus,
        result_ref: Option<&str>,
        error: Option<&str>,
    ) -> Result<(CronRun, IdempotencyOutcome)> {
        let job = self.job(job_id)?.ok_or_else(|| CronError::NotFound {
            id: job_id.to_string(),
        })?;
        validate_runnable_job(&job)?;
        let scheduled_for = timestamp();
        self.record_run_for(job_id, &scheduled_for, status, result_ref, error)
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
        let job = self.job(job_id)?.ok_or_else(|| CronError::NotFound {
            id: job_id.to_string(),
        })?;
        match self.begin_run_for(&job, scheduled_for)? {
            CronRunAdmission::Inserted(outcome) => {
                let run =
                    self.record_admitted_run(job_id, scheduled_for, status, result_ref, error)?;
                Ok((run, outcome))
            }
            CronRunAdmission::Replayed(run, outcome) => Ok((run, outcome)),
            CronRunAdmission::Pending(_) => Err(CronError::InvalidValue {
                field: "idempotency",
                value: format!("{job_id}:{scheduled_for}"),
                reason: "cron run is already admitted but has no recorded result",
            }),
        }
    }

    pub fn begin_run_for(&self, job: &CronJob, scheduled_for: &str) -> Result<CronRunAdmission> {
        validate_runnable_job(job)?;
        validate_non_blank("scheduled_for", scheduled_for)?;
        let idempotency_key = idempotency_key(&job.id, scheduled_for);
        let fingerprint = idempotency_fingerprint(job);
        let outcome =
            self.store
                .begin_idempotency(IDEMPOTENCY_NAMESPACE, &idempotency_key, &fingerprint)?;
        if let IdempotencyOutcome::Replayed(record) = &outcome {
            if let Some(result_ref) = &record.result_ref
                && let Some(run) = self.run(result_ref)?
            {
                return Ok(CronRunAdmission::Replayed(run, outcome));
            }
            return Ok(CronRunAdmission::Pending(outcome));
        }
        Ok(CronRunAdmission::Inserted(outcome))
    }

    pub fn record_admitted_run(
        &self,
        job_id: &str,
        scheduled_for: &str,
        status: CronRunStatus,
        result_ref: Option<&str>,
        error: Option<&str>,
    ) -> Result<CronRun> {
        validate_non_blank("job_id", job_id)?;
        validate_non_blank("scheduled_for", scheduled_for)?;
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
        let idempotency_key = idempotency_key(job_id, scheduled_for);
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
        Ok(run)
    }

    pub fn record_admitted_supervisor_run(
        &self,
        job_id: &str,
        scheduled_for: &str,
        supervisor_run_id: &RunId,
    ) -> Result<CronRun> {
        validate_non_blank("job_id", job_id)?;
        validate_non_blank("scheduled_for", scheduled_for)?;
        if let Some(existing) = self.run_for_schedule(job_id, scheduled_for)? {
            if existing.supervisor_run_id.as_ref() == Some(supervisor_run_id) {
                return Ok(existing);
            }
            return Err(CronError::InvalidValue {
                field: "supervisor_run_id",
                value: supervisor_run_id.to_string(),
                reason: "scheduled cron run is linked to another supervisor run",
            });
        }

        let id = cron_id("cron_run");
        let result_ref = format!("run:{supervisor_run_id}");
        self.store.connection().execute(
            "INSERT INTO cron_runs
             (id, job_id, scheduled_for, started_at, finished_at, status, result_ref, error,
              supervisor_run_id)
             VALUES (?1, ?2, ?3, NULL, NULL, 'queued', ?4, NULL, ?5)",
            params![
                id,
                job_id,
                scheduled_for,
                result_ref,
                supervisor_run_id.as_str()
            ],
        )?;
        let idempotency_key = idempotency_key(job_id, scheduled_for);
        self.store.connection().execute(
            "UPDATE idempotency_keys SET result_ref = ?3, updated_at = ?4
             WHERE namespace = ?1 AND key = ?2 AND status = 'in_progress'",
            params![IDEMPOTENCY_NAMESPACE, idempotency_key, id, timestamp()],
        )?;
        self.run(&id)?.ok_or_else(|| CronError::NotFound { id })
    }

    pub fn active_supervisor_runs(&self) -> Result<Vec<CronRun>> {
        let mut statement = self.store.connection().prepare(
            "SELECT id, job_id, scheduled_for, started_at, finished_at, status, result_ref,
                    error, supervisor_run_id
             FROM cron_runs
             WHERE status IN ('queued', 'running') AND supervisor_run_id IS NOT NULL
             ORDER BY scheduled_for, id",
        )?;
        let rows = statement.query_map([], run_from_row)?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(row??);
        }
        Ok(runs)
    }

    pub fn finish_supervisor_run(
        &self,
        supervisor_run_id: &RunId,
        status: CronRunStatus,
        result_ref: Option<&str>,
        error: Option<&str>,
    ) -> Result<CronRun> {
        if !matches!(
            status,
            CronRunStatus::Succeeded | CronRunStatus::Failed | CronRunStatus::Skipped
        ) {
            return Err(CronError::InvalidValue {
                field: "status",
                value: status.as_str().to_string(),
                reason: "linked supervisor completion must be terminal",
            });
        }
        let now = timestamp();
        let changed = self.store.connection().execute(
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
        )?;
        let run =
            self.run_for_supervisor(supervisor_run_id)?
                .ok_or_else(|| CronError::NotFound {
                    id: supervisor_run_id.to_string(),
                })?;
        if changed > 0 {
            let key = idempotency_key(&run.job_id, &run.scheduled_for);
            match status {
                CronRunStatus::Succeeded => {
                    self.store
                        .complete_idempotency(IDEMPOTENCY_NAMESPACE, &key, Some(&run.id))?;
                }
                CronRunStatus::Failed => {
                    self.store
                        .fail_idempotency(IDEMPOTENCY_NAMESPACE, &key, Some(&run.id))?;
                }
                CronRunStatus::Skipped => {
                    self.store
                        .skip_idempotency(IDEMPOTENCY_NAMESPACE, &key, Some(&run.id))?;
                }
                CronRunStatus::Queued | CronRunStatus::Running => unreachable!(),
            }
        }
        Ok(run)
    }

    pub fn history(&self, job_id: &str) -> Result<Vec<CronRun>> {
        validate_non_blank("job_id", job_id)?;
        let mut stmt = self.store.connection().prepare(
            "SELECT id, job_id, scheduled_for, started_at, finished_at, status, result_ref, error,
                    supervisor_run_id
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

    pub fn due_jobs(&self, unix_seconds: u64) -> Result<Vec<CronDueJob>> {
        let scheduled_for = minute_timestamp(unix_seconds);
        let mut due = Vec::new();
        for job in self.enabled_jobs()? {
            if schedule_matches(&job.schedule_expr, unix_seconds, &job.timezone)? {
                due.push(CronDueJob {
                    job,
                    scheduled_for: scheduled_for.clone(),
                });
            }
        }
        Ok(due)
    }

    fn enabled_jobs(&self) -> Result<Vec<CronJob>> {
        self.query_jobs(
            "SELECT id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, prompt, input, created_at, updated_at, deleted_at
             FROM cron_jobs
             WHERE enabled = 1 AND deleted_at IS NULL
             ORDER BY updated_at DESC, id DESC",
            [],
        )
    }

    fn run(&self, id: &str) -> Result<Option<CronRun>> {
        self.store
            .connection()
            .query_row(
                "SELECT id, job_id, scheduled_for, started_at, finished_at, status, result_ref,
                        error, supervisor_run_id
                 FROM cron_runs
                 WHERE id = ?1",
                params![id],
                run_from_row,
            )
            .optional()?
            .transpose()
    }

    fn run_for_schedule(&self, job_id: &str, scheduled_for: &str) -> Result<Option<CronRun>> {
        self.store
            .connection()
            .query_row(
                "SELECT id, job_id, scheduled_for, started_at, finished_at, status, result_ref,
                        error, supervisor_run_id
                 FROM cron_runs WHERE job_id = ?1 AND scheduled_for = ?2",
                params![job_id, scheduled_for],
                run_from_row,
            )
            .optional()?
            .transpose()
    }

    fn run_for_supervisor(&self, run_id: &RunId) -> Result<Option<CronRun>> {
        self.store
            .connection()
            .query_row(
                "SELECT id, job_id, scheduled_for, started_at, finished_at, status, result_ref,
                        error, supervisor_run_id
                 FROM cron_runs WHERE supervisor_run_id = ?1",
                [run_id.as_str()],
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
    if parts.len() == 5 && valid_cron_expression(&parts) {
        return Ok(());
    }
    Err(CronError::InvalidValue {
        field: "schedule_expr",
        value: value.to_string(),
        reason: "expected hourly, daily HH:MM, weekly <weekday> HH:MM, or 5-field cron expression",
    })
}

pub fn validate_job_draft(draft: &CronJobDraft) -> Result<()> {
    validate_draft(draft)
}

fn validate_draft(draft: &CronJobDraft) -> Result<()> {
    validate_non_blank("name", &draft.name)?;
    validate_non_blank("target_ref", &draft.target_ref)?;
    validate_schedule_expr(&draft.schedule_expr)?;
    validate_non_blank("timezone", &draft.timezone)?;
    parse_timezone(&draft.timezone)?;
    if let Some(prompt) = &draft.prompt {
        validate_non_blank("prompt", prompt)?;
    }
    if let Some(input) = &draft.input {
        validate_non_blank("input", input)?;
    }
    if draft.target_kind == CronTargetKind::Skill && draft.prompt.is_none() {
        return Err(CronError::InvalidValue {
            field: "prompt",
            value: String::new(),
            reason: "skill cron jobs require a stored prompt",
        });
    }
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
        job.input
    )
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
            prompt: row.get(8)?,
            input: row.get(9)?,
            created_at: row.get(10)?,
            updated_at: row.get(11)?,
            deleted_at: row.get(12)?,
        })
    })())
}

fn run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<CronRun>> {
    let status: String = row.get(5)?;
    let supervisor_run_id: Option<String> = row.get(8)?;
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
            supervisor_run_id: supervisor_run_id
                .as_deref()
                .map(parse_supervisor_run_id)
                .transpose()?,
        })
    })())
}

fn parse_supervisor_run_id(value: &str) -> Result<RunId> {
    RunId::parse(value).map_err(|_| CronError::InvalidValue {
        field: "supervisor_run_id",
        value: value.to_string(),
        reason: "invalid typed run ID",
    })
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

fn valid_cron_expression(parts: &[&str]) -> bool {
    cron_field_valid(parts[0], CronFieldKind::Minute)
        && cron_field_valid(parts[1], CronFieldKind::Hour)
        && cron_field_valid(parts[2], CronFieldKind::DayOfMonth)
        && cron_field_valid(parts[3], CronFieldKind::Month)
        && cron_field_valid(parts[4], CronFieldKind::DayOfWeek)
}

fn cron_field_valid(field: &str, kind: CronFieldKind) -> bool {
    !field.is_empty()
        && field
            .split(',')
            .all(|part| cron_field_part_bounds(part, kind).is_ok())
}

fn schedule_matches(expr: &str, unix_seconds: u64, timezone: &str) -> Result<bool> {
    let expr = expr.trim();
    let timezone = parse_timezone(timezone)?;
    let fields = local_minute_fields(unix_seconds, timezone);

    if expr == "hourly" {
        return Ok(fields.minute == 0);
    }
    let parts = expr.split_whitespace().collect::<Vec<_>>();
    if parts.len() == 2 && parts[0] == "daily" {
        let (expected_hour, expected_minute) = parse_time(parts[1])?;
        return Ok(fields.hour == expected_hour && fields.minute == expected_minute);
    }
    if parts.len() == 3 && parts[0] == "weekly" {
        let expected_weekday = parse_weekday(parts[1])?;
        let (expected_hour, expected_minute) = parse_time(parts[2])?;
        let weekday_monday_zero = (fields.weekday_sunday_zero + 6) % 7;
        return Ok(weekday_monday_zero == expected_weekday
            && fields.hour == expected_hour
            && fields.minute == expected_minute);
    }
    if parts.len() == 5 {
        let day_of_month_matches =
            cron_field_matches(parts[2], fields.day_of_month, CronFieldKind::DayOfMonth)?;
        let day_of_week_matches = cron_field_matches(
            parts[4],
            fields.weekday_sunday_zero,
            CronFieldKind::DayOfWeek,
        )?;
        let day_matches = if !cron_field_is_any(parts[2]) && !cron_field_is_any(parts[4]) {
            day_of_month_matches || day_of_week_matches
        } else {
            day_of_month_matches && day_of_week_matches
        };
        return Ok(
            cron_field_matches(parts[0], fields.minute, CronFieldKind::Minute)?
                && cron_field_matches(parts[1], fields.hour, CronFieldKind::Hour)?
                && day_matches
                && cron_field_matches(parts[3], fields.month, CronFieldKind::Month)?,
        );
    }
    validate_schedule_expr(expr)?;
    Ok(false)
}

fn parse_time(value: &str) -> Result<(u32, u32)> {
    if !valid_time(value) {
        return Err(CronError::InvalidValue {
            field: "schedule_expr",
            value: value.to_string(),
            reason: "invalid time",
        });
    }
    let (hour, minute) = value.split_once(':').expect("valid time contains colon");
    Ok((hour.parse().unwrap_or(0), minute.parse().unwrap_or(0)))
}

fn parse_weekday(value: &str) -> Result<u32> {
    match value {
        "mon" => Ok(0),
        "tue" => Ok(1),
        "wed" => Ok(2),
        "thu" => Ok(3),
        "fri" => Ok(4),
        "sat" => Ok(5),
        "sun" => Ok(6),
        _ => Err(CronError::InvalidValue {
            field: "schedule_expr",
            value: value.to_string(),
            reason: "invalid weekday",
        }),
    }
}

fn cron_field_matches(field: &str, value: u32, kind: CronFieldKind) -> Result<bool> {
    for part in field.split(',') {
        if cron_field_part_matches(part, value, kind)?
            || (kind == CronFieldKind::DayOfWeek
                && value == 0
                && cron_field_part_matches(part, 7, kind)?)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn cron_field_part_matches(part: &str, value: u32, kind: CronFieldKind) -> Result<bool> {
    let (start, end, step) = cron_field_part_bounds(part, kind)?;
    Ok(value >= start && value <= end && (value - start).is_multiple_of(step))
}

fn cron_field_part_bounds(part: &str, kind: CronFieldKind) -> Result<(u32, u32, u32)> {
    let (base, step) = if let Some((base, step)) = part.split_once('/') {
        let step = step.parse::<u32>().map_err(|_| CronError::InvalidValue {
            field: "schedule_expr",
            value: part.to_string(),
            reason: "cron step must be a positive integer",
        })?;
        if step == 0 {
            return Err(CronError::InvalidValue {
                field: "schedule_expr",
                value: part.to_string(),
                reason: "cron step must be greater than zero",
            });
        }
        (base, step)
    } else {
        (part, 1)
    };
    if base.is_empty() {
        return Err(CronError::InvalidValue {
            field: "schedule_expr",
            value: part.to_string(),
            reason: "cron field base cannot be empty",
        });
    }

    let (min, max) = kind.range();
    let (start, end) = if base == "*" {
        (min, max)
    } else if let Some((start, end)) = base.split_once('-') {
        (parse_cron_atom(start, kind)?, parse_cron_atom(end, kind)?)
    } else {
        let number = parse_cron_atom(base, kind)?;
        (number, number)
    };
    if start > end {
        return Err(CronError::InvalidValue {
            field: "schedule_expr",
            value: part.to_string(),
            reason: "cron range start cannot be greater than range end",
        });
    }
    Ok((start, end, step))
}

fn parse_cron_atom(value: &str, kind: CronFieldKind) -> Result<u32> {
    let lowered = value.to_ascii_lowercase();
    let number = match kind {
        CronFieldKind::Month => match lowered.as_str() {
            "jan" => Some(1),
            "feb" => Some(2),
            "mar" => Some(3),
            "apr" => Some(4),
            "may" => Some(5),
            "jun" => Some(6),
            "jul" => Some(7),
            "aug" => Some(8),
            "sep" => Some(9),
            "oct" => Some(10),
            "nov" => Some(11),
            "dec" => Some(12),
            _ => None,
        },
        CronFieldKind::DayOfWeek => match lowered.as_str() {
            "sun" => Some(0),
            "mon" => Some(1),
            "tue" => Some(2),
            "wed" => Some(3),
            "thu" => Some(4),
            "fri" => Some(5),
            "sat" => Some(6),
            _ => None,
        },
        _ => None,
    };
    let number = if let Some(number) = number {
        number
    } else {
        value.parse::<u32>().map_err(|_| CronError::InvalidValue {
            field: "schedule_expr",
            value: value.to_string(),
            reason: "cron field value must be numeric or a supported name",
        })?
    };
    let (min, max) = kind.range();
    if number < min || number > max {
        return Err(CronError::InvalidValue {
            field: "schedule_expr",
            value: value.to_string(),
            reason: "cron field value out of supported range",
        });
    }
    Ok(number)
}

fn cron_field_is_any(field: &str) -> bool {
    field == "*" || field == "*/1"
}

fn parse_timezone(value: &str) -> Result<TimezoneOffset> {
    let value = value.trim();
    validate_non_blank("timezone", value)?;
    if matches!(value, "UTC" | "Z") {
        return Ok(TimezoneOffset { seconds: 0 });
    }
    let offset = value
        .strip_prefix("UTC")
        .filter(|rest| !rest.is_empty())
        .unwrap_or(value);
    let Some(sign) = offset.chars().next() else {
        unreachable!("non-empty timezone offset checked above");
    };
    if sign != '+' && sign != '-' {
        return Err(CronError::InvalidValue {
            field: "timezone",
            value: value.to_string(),
            reason: "expected UTC, Z, or fixed offset such as +02:00 or UTC-07:00",
        });
    }
    let body = &offset[1..];
    let Some((hours, minutes)) = body.split_once(':') else {
        return Err(CronError::InvalidValue {
            field: "timezone",
            value: value.to_string(),
            reason: "fixed offset must use HH:MM",
        });
    };
    let hours = hours.parse::<i32>().map_err(|_| CronError::InvalidValue {
        field: "timezone",
        value: value.to_string(),
        reason: "fixed offset hour must be numeric",
    })?;
    let minutes = minutes
        .parse::<i32>()
        .map_err(|_| CronError::InvalidValue {
            field: "timezone",
            value: value.to_string(),
            reason: "fixed offset minute must be numeric",
        })?;
    if hours > 23 || minutes > 59 {
        return Err(CronError::InvalidValue {
            field: "timezone",
            value: value.to_string(),
            reason: "fixed offset is out of range",
        });
    }
    let seconds = hours * 3600 + minutes * 60;
    Ok(TimezoneOffset {
        seconds: if sign == '-' { -seconds } else { seconds },
    })
}

fn local_minute_fields(unix_seconds: u64, timezone: TimezoneOffset) -> UtcMinuteFields {
    let local_seconds = unix_seconds as i64 + timezone.seconds as i64;
    minute_fields(local_seconds)
}

fn minute_fields(unix_seconds: i64) -> UtcMinuteFields {
    let minute = unix_seconds.div_euclid(60).rem_euclid(60) as u32;
    let hour = unix_seconds.div_euclid(3600).rem_euclid(24) as u32;
    let days = unix_seconds.div_euclid(86_400);
    let (_, month, day_of_month) = civil_from_unix_days(days);
    UtcMinuteFields {
        minute,
        hour,
        day_of_month,
        month,
        weekday_sunday_zero: ((days + 4).rem_euclid(7)) as u32,
    }
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
    let year = y + if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{seconds}")
}

fn minute_timestamp(unix_seconds: u64) -> String {
    format!("unix:{}", unix_seconds - (unix_seconds % 60))
}

fn cron_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}_{}_{}", std::process::id(), nanos)
}

#[cfg(test)]
mod tests;
