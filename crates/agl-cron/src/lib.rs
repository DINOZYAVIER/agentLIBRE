use std::fmt;

use agl_ids::RunId;
use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, CronError>;

const DEFAULT_TIMEZONE: &str = "UTC";
pub const STORE_STATUS_BUILTIN_CRON_TARGET: &str = "store-status";

pub fn supported_builtin_cron_targets() -> &'static [&'static str] {
    &[STORE_STATUS_BUILTIN_CRON_TARGET]
}

pub fn validate_builtin_cron_target(target_ref: &str) -> std::result::Result<(), String> {
    if supported_builtin_cron_targets().contains(&target_ref) {
        Ok(())
    } else {
        Err(unsupported_builtin_cron_target_message(target_ref))
    }
}

pub fn unsupported_builtin_cron_target_message(target_ref: &str) -> String {
    format!(
        "unknown builtin cron target: {target_ref}; supported builtin targets: {}",
        supported_builtin_cron_targets().join(", ")
    )
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
    IdempotencyConflict {
        key: String,
    },
    Repository {
        reason: String,
    },
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
            Self::IdempotencyConflict { key } => {
                write!(
                    f,
                    "cron idempotency key {key:?} was reused with different input"
                )
            }
            Self::Repository { reason } => write!(f, "cron repository failed: {reason}"),
        }
    }
}

impl std::error::Error for CronError {}

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

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "skill" => Ok(Self::Skill),
            "builtin" => Ok(Self::Builtin),
            _ => Err(invalid("target_kind", value, "unknown cron target kind")),
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

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "skipped" => Ok(Self::Skipped),
            _ => Err(invalid("status", value, "unknown cron run status")),
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
            timezone: DEFAULT_TIMEZONE.to_owned(),
            notify_ref: None,
            prompt: None,
            input: None,
        }
    }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronIdempotencyStatus {
    InProgress,
    Completed,
    Failed,
    Skipped,
}

impl CronIdempotencyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CronIdempotencyRecord {
    pub key: String,
    pub fingerprint: String,
    pub status: CronIdempotencyStatus,
    pub result_ref: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CronIdempotencyOutcome {
    Inserted(CronIdempotencyRecord),
    Replayed(CronIdempotencyRecord),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CronRunAdmission {
    Inserted(CronIdempotencyOutcome),
    Replayed(CronRun, CronIdempotencyOutcome),
    Pending(CronIdempotencyOutcome),
}

pub trait CronRepository: Send + Sync {
    fn update_job(&self, id: &str, update: CronJobUpdate) -> Result<CronJob>;
    fn add_job(&self, draft: CronJobDraft) -> Result<CronJob>;
    fn list_jobs(&self, include_deleted: bool) -> Result<Vec<CronJob>>;
    fn job(&self, id: &str) -> Result<Option<CronJob>>;
    fn set_enabled(&self, id: &str, enabled: bool) -> Result<CronJob>;
    fn delete_job(&self, id: &str) -> Result<CronJob>;
    fn record_manual_run(
        &self,
        job_id: &str,
        result_ref: Option<&str>,
    ) -> Result<(CronRun, CronIdempotencyOutcome)>;
    fn record_manual_run_result(
        &self,
        job_id: &str,
        status: CronRunStatus,
        result_ref: Option<&str>,
        error: Option<&str>,
    ) -> Result<(CronRun, CronIdempotencyOutcome)>;
    fn record_run_for(
        &self,
        job_id: &str,
        scheduled_for: &str,
        status: CronRunStatus,
        result_ref: Option<&str>,
        error: Option<&str>,
    ) -> Result<(CronRun, CronIdempotencyOutcome)>;
    fn begin_run_for(&self, job: &CronJob, scheduled_for: &str) -> Result<CronRunAdmission>;
    fn record_admitted_run(
        &self,
        job_id: &str,
        scheduled_for: &str,
        status: CronRunStatus,
        result_ref: Option<&str>,
        error: Option<&str>,
    ) -> Result<CronRun>;
    fn record_admitted_supervisor_run(
        &self,
        job_id: &str,
        scheduled_for: &str,
        supervisor_run_id: &RunId,
    ) -> Result<CronRun>;
    fn active_supervisor_runs(&self) -> Result<Vec<CronRun>>;
    fn finish_supervisor_run(
        &self,
        supervisor_run_id: &RunId,
        status: CronRunStatus,
        result_ref: Option<&str>,
        error: Option<&str>,
    ) -> Result<CronRun>;
    fn history(&self, job_id: &str) -> Result<Vec<CronRun>>;
    fn due_jobs(&self, unix_seconds: u64) -> Result<Vec<CronDueJob>>;
}

pub fn validate_job_draft(draft: &CronJobDraft) -> Result<()> {
    validate_non_blank("name", &draft.name)?;
    validate_non_blank("target_ref", &draft.target_ref)?;
    validate_schedule_expr(&draft.schedule_expr)?;
    validate_timezone(&draft.timezone)?;
    if let Some(prompt) = &draft.prompt {
        validate_non_blank("prompt", prompt)?;
    }
    if let Some(input) = &draft.input {
        validate_non_blank("input", input)?;
    }
    if draft.target_kind == CronTargetKind::Skill && draft.prompt.is_none() {
        return Err(invalid(
            "prompt",
            "",
            "skill cron jobs require a stored prompt",
        ));
    }
    if let Some(notify_ref) = &draft.notify_ref {
        validate_non_blank("notify_ref", notify_ref)?;
    }
    Ok(())
}

pub fn validate_schedule_expr(value: &str) -> Result<()> {
    let value = value.trim();
    if value == "hourly" {
        return Ok(());
    }
    let parts = value.split_whitespace().collect::<Vec<_>>();
    if parts.len() == 2 && parts[0] == "daily" && valid_time(parts[1]) {
        return Ok(());
    }
    if parts.len() == 3
        && parts[0] == "weekly"
        && matches!(
            parts[1],
            "mon" | "tue" | "wed" | "thu" | "fri" | "sat" | "sun"
        )
        && valid_time(parts[2])
    {
        return Ok(());
    }
    if parts.len() == 5
        && cron_field_valid(parts[0], 0, 59)
        && cron_field_valid(parts[1], 0, 23)
        && cron_field_valid(parts[2], 1, 31)
        && cron_field_valid(parts[3], 1, 12)
        && cron_field_valid(parts[4], 0, 7)
    {
        return Ok(());
    }
    Err(invalid(
        "schedule_expr",
        value,
        "expected hourly, daily HH:MM, weekly <weekday> HH:MM, or 5-field cron expression",
    ))
}

pub fn validate_timezone(value: &str) -> Result<()> {
    let value = value.trim();
    if matches!(value, "UTC" | "Z") {
        return Ok(());
    }
    let offset = value.strip_prefix("UTC").unwrap_or(value);
    let Some((sign_and_hour, minute)) = offset.split_once(':') else {
        return Err(invalid(
            "timezone",
            value,
            "expected UTC, Z, or fixed offset such as +02:00 or UTC-07:00",
        ));
    };
    let Some(sign) = sign_and_hour.chars().next() else {
        return Err(invalid("timezone", value, "fixed offset is empty"));
    };
    if !matches!(sign, '+' | '-') {
        return Err(invalid("timezone", value, "fixed offset requires a sign"));
    }
    let hour = sign_and_hour[1..]
        .parse::<u8>()
        .map_err(|_| invalid("timezone", value, "fixed offset hour must be numeric"))?;
    let minute = minute
        .parse::<u8>()
        .map_err(|_| invalid("timezone", value, "fixed offset minute must be numeric"))?;
    if hour > 23 || minute > 59 {
        return Err(invalid("timezone", value, "fixed offset is out of range"));
    }
    Ok(())
}

fn validate_non_blank(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(invalid(field, value, "value cannot be blank"))
    } else {
        Ok(())
    }
}

fn valid_time(value: &str) -> bool {
    let Some((hour, minute)) = value.split_once(':') else {
        return false;
    };
    matches!((hour.parse::<u8>(), minute.parse::<u8>()), (Ok(hour), Ok(minute)) if hour < 24 && minute < 60)
}

fn cron_field_valid(field: &str, min: u32, max: u32) -> bool {
    !field.is_empty() && field.split(',').all(|part| cron_part_valid(part, min, max))
}

fn cron_part_valid(part: &str, min: u32, max: u32) -> bool {
    let (base, step) = part
        .split_once('/')
        .map_or((part, None), |(base, step)| (base, Some(step)));
    if step.is_some_and(|step| step.parse::<u32>().ok().is_none_or(|step| step == 0)) {
        return false;
    }
    if base == "*" {
        return true;
    }
    if let Some((start, end)) = base.split_once('-') {
        return matches!((start.parse::<u32>(), end.parse::<u32>()), (Ok(start), Ok(end)) if start >= min && start <= end && end <= max);
    }
    base.parse::<u32>()
        .is_ok_and(|value| value >= min && value <= max)
}

fn invalid(field: &'static str, value: impl Into<String>, reason: &'static str) -> CronError {
    CronError::InvalidValue {
        field,
        value: value.into(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_cron_without_sqlite() {
        for valid in ["hourly", "daily 09:30", "weekly mon 18:00", "*/5 * * * *"] {
            assert!(validate_schedule_expr(valid).is_ok(), "{valid}");
        }
        assert!(validate_schedule_expr("daily 99:99").is_err());
        assert!(validate_timezone("UTC-07:00").is_ok());
    }
}
