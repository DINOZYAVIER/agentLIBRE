use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_capabilities::{ActionHandler, ActionHandlerError, ActionInvocation, ActionResult};
use agl_cron::{
    CronJob, CronJobDraft, CronJobUpdate, CronRepository, CronRun, CronRunAdmission, CronRunStatus,
    CronTargetKind, STORE_STATUS_BUILTIN_CRON_TARGET, validate_job_draft,
};
use agl_store::{AglStore, IdempotencyOutcome, MatrixNotificationOutboxDraft};
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::parse_action_args as parse_args;

mod declarations;

pub use declarations::{declaration, register};

pub const PROVIDER_ID: &str = "cron-tools";
pub const CRON_LIST_TOOL_ID: &str = "cron.list";
pub const CRON_SHOW_TOOL_ID: &str = "cron.show";
pub const CRON_HISTORY_TOOL_ID: &str = "cron.history";
pub const CRON_PREFLIGHT_TOOL_ID: &str = "cron.preflight";
pub const CRON_ADD_TOOL_ID: &str = "cron.add";
pub const CRON_UPDATE_TOOL_ID: &str = "cron.update";
pub const CRON_DELETE_TOOL_ID: &str = "cron.delete";
pub const CRON_ENABLE_TOOL_ID: &str = "cron.enable";
pub const CRON_DISABLE_TOOL_ID: &str = "cron.disable";
pub const CRON_RUN_TOOL_ID: &str = "cron.run";
pub const CRON_TICK_TOOL_ID: &str = "cron.tick";

const DEFAULT_HISTORY_LIMIT: usize = 20;
const MAX_HISTORY_LIMIT: usize = 100;

#[derive(Clone, Debug)]
pub struct CronTools {
    store_root: PathBuf,
}

impl CronTools {
    pub fn new(store_root: impl AsRef<Path>) -> Self {
        Self {
            store_root: store_root.as_ref().to_path_buf(),
        }
    }

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<Value> {
        match name {
            CRON_LIST_TOOL_ID => self.list(arguments),
            CRON_SHOW_TOOL_ID => self.show(arguments),
            CRON_HISTORY_TOOL_ID => self.history(arguments),
            CRON_PREFLIGHT_TOOL_ID => self.preflight(arguments),
            CRON_ADD_TOOL_ID => self.add(arguments),
            CRON_UPDATE_TOOL_ID => self.update(arguments),
            CRON_DELETE_TOOL_ID => self.delete(arguments),
            CRON_ENABLE_TOOL_ID => self.set_enabled(arguments, true),
            CRON_DISABLE_TOOL_ID => self.set_enabled(arguments, false),
            CRON_RUN_TOOL_ID => self.run(arguments),
            CRON_TICK_TOOL_ID => self.tick(arguments),
            _ => anyhow::bail!("unknown cron tool `{name}`"),
        }
    }

    fn list(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<ListArgs>(CRON_LIST_TOOL_ID, arguments)?;
        let store = self.open_store_read_only()?;
        let cron = CronRepository::new(&store);
        let jobs = cron.list_jobs(args.include_deleted.unwrap_or(false))?;
        let jobs = jobs.iter().map(job_value).collect::<Vec<_>>();
        Ok(json!({
            "tool": CRON_LIST_TOOL_ID,
            "status": "ok",
            "job_count": jobs.len(),
            "jobs": jobs,
        }))
    }

    fn show(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<IdArgs>(CRON_SHOW_TOOL_ID, arguments)?;
        let store = self.open_store_read_only()?;
        let cron = CronRepository::new(&store);
        let job = cron
            .job(&args.id)?
            .with_context(|| format!("cron job not found: {}", args.id))?;
        Ok(json!({
            "tool": CRON_SHOW_TOOL_ID,
            "status": "ok",
            "job": job_value(&job),
        }))
    }

    fn history(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<HistoryArgs>(CRON_HISTORY_TOOL_ID, arguments)?;
        let limit = args
            .limit
            .unwrap_or(DEFAULT_HISTORY_LIMIT)
            .min(MAX_HISTORY_LIMIT);
        let store = self.open_store_read_only()?;
        let cron = CronRepository::new(&store);
        let mut runs = cron.history(&args.job_id)?;
        runs.truncate(limit);
        let runs = runs.iter().map(run_value).collect::<Vec<_>>();
        Ok(json!({
            "tool": CRON_HISTORY_TOOL_ID,
            "status": "ok",
            "job_id": args.job_id,
            "run_count": runs.len(),
            "runs": runs,
        }))
    }

    fn preflight(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<JobDraftArgs>(CRON_PREFLIGHT_TOOL_ID, arguments)?;
        let draft = args.into_draft()?;
        validate_job_draft(&draft)?;
        Ok(json!({
            "tool": CRON_PREFLIGHT_TOOL_ID,
            "status": "ok",
            "target_kind": draft.target_kind.as_str(),
            "target_ref": draft.target_ref,
            "schedule_expr": draft.schedule_expr,
            "timezone": draft.timezone,
        }))
    }

    fn add(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<JobDraftArgs>(CRON_ADD_TOOL_ID, arguments)?;
        let store = self.open_store_writable()?;
        let cron = CronRepository::new(&store);
        let job = cron.add_job(args.into_draft()?)?;
        Ok(json!({
            "tool": CRON_ADD_TOOL_ID,
            "status": "created",
            "job_id": job.id,
            "job": job_value(&job),
        }))
    }

    fn update(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<UpdateArgs>(CRON_UPDATE_TOOL_ID, arguments)?;
        let id = args.id.clone();
        let store = self.open_store_writable()?;
        let cron = CronRepository::new(&store);
        let job = cron.update_job(&id, args.into_update()?)?;
        Ok(json!({
            "tool": CRON_UPDATE_TOOL_ID,
            "status": "updated",
            "job_id": job.id,
            "job": job_value(&job),
        }))
    }

    fn delete(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<IdArgs>(CRON_DELETE_TOOL_ID, arguments)?;
        let store = self.open_store_writable()?;
        let cron = CronRepository::new(&store);
        let job = cron.delete_job(&args.id)?;
        Ok(json!({
            "tool": CRON_DELETE_TOOL_ID,
            "status": "deleted",
            "job_id": job.id,
            "deleted": job.deleted_at.is_some(),
        }))
    }

    fn set_enabled(&self, arguments: Value, enabled: bool) -> Result<Value> {
        let tool_id = if enabled {
            CRON_ENABLE_TOOL_ID
        } else {
            CRON_DISABLE_TOOL_ID
        };
        let args = parse_args::<IdArgs>(tool_id, arguments)?;
        let store = self.open_store_writable()?;
        let cron = CronRepository::new(&store);
        let job = cron.set_enabled(&args.id, enabled)?;
        Ok(json!({
            "tool": tool_id,
            "status": "updated",
            "job_id": job.id,
            "enabled": job.enabled,
        }))
    }

    fn run(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<RunArgs>(CRON_RUN_TOOL_ID, arguments)?;
        let status = args.status.unwrap_or(CronRunStatusArg::Succeeded).into();
        let store = self.open_store_writable()?;
        let cron = CronRepository::new(&store);
        let (run, outcome) = if let Some(scheduled_for) = args.scheduled_for {
            cron.record_run_for(
                &args.job_id,
                &scheduled_for,
                status,
                args.result_ref.as_deref(),
                args.error.as_deref(),
            )?
        } else {
            cron.record_manual_run_result(
                &args.job_id,
                status,
                args.result_ref.as_deref(),
                args.error.as_deref(),
            )?
        };
        Ok(render_run_with_idempotency(
            CRON_RUN_TOOL_ID,
            &run,
            &outcome,
        ))
    }

    fn tick(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<TickArgs>(CRON_TICK_TOOL_ID, arguments)?;
        let unix_seconds = args.unix_seconds.unwrap_or_else(current_unix_seconds);
        let mock_skill_execution = args.mock_skill_execution.unwrap_or(true);
        let store = self.open_store_writable()?;
        let cron = CronRepository::new(&store);
        let due_jobs = cron.due_jobs(unix_seconds)?;
        let mut recorded = Vec::new();
        let mut notifications = 0usize;
        let mut replays = 0usize;
        let mut pending = 0usize;

        for due in due_jobs {
            match cron.begin_run_for(&due.job, &due.scheduled_for)? {
                CronRunAdmission::Replayed(run, _) => {
                    replays += 1;
                    recorded.push(run);
                }
                CronRunAdmission::Pending(_) => {
                    pending += 1;
                }
                CronRunAdmission::Inserted(_) => {
                    let (status, result_ref, error) =
                        execute_tick_target(&due.job, mock_skill_execution);
                    let run = cron.record_admitted_run(
                        &due.job.id,
                        &due.scheduled_for,
                        status,
                        result_ref.as_deref(),
                        error.as_deref(),
                    )?;
                    if let Some(notify_ref) = &due.job.notify_ref
                        && notify_ref.starts_with("matrix-room:")
                    {
                        enqueue_cron_notification(&store, &due.job, &run, notify_ref)?;
                        notifications += 1;
                    }
                    recorded.push(run);
                }
            }
        }

        let runs = recorded.iter().map(run_value).collect::<Vec<_>>();
        Ok(json!({
            "tool": CRON_TICK_TOOL_ID,
            "status": "ok",
            "unix_seconds": unix_seconds,
            "recorded_run_count": runs.len(),
            "replayed_run_count": replays,
            "pending_run_count": pending,
            "notification_count": notifications,
            "runs": runs,
        }))
    }

    fn open_store_read_only(&self) -> Result<AglStore> {
        AglStore::open_current_read_only_at(&self.store_root)
            .with_context(|| format!("failed to open cron store {}", self.store_root.display()))
    }

    fn open_store_writable(&self) -> Result<AglStore> {
        AglStore::open_current_at(&self.store_root)
            .with_context(|| format!("failed to open cron store {}", self.store_root.display()))
    }
}

impl ActionHandler for CronTools {
    fn dispatch(&self, invocation: ActionInvocation) -> Result<ActionResult, ActionHandlerError> {
        self.dispatch(invocation.capability_id.as_str(), invocation.arguments)
            .map(ActionResult::new)
            .map_err(Into::into)
    }
}

fn job_value(job: &CronJob) -> Value {
    json!({
        "id": job.id,
        "name": job.name,
        "enabled": job.enabled,
        "target_kind": job.target_kind.as_str(),
        "target_ref": job.target_ref,
        "schedule_expr": job.schedule_expr,
        "timezone": job.timezone,
        "notify_ref": job.notify_ref,
        "prompt": job.prompt,
        "input": job.input,
        "created_at": job.created_at,
        "updated_at": job.updated_at,
        "deleted_at": job.deleted_at,
    })
}

fn run_value(run: &CronRun) -> Value {
    json!({
        "id": run.id,
        "job_id": run.job_id,
        "scheduled_for": run.scheduled_for,
        "started_at": run.started_at,
        "finished_at": run.finished_at,
        "status": run.status.as_str(),
        "result_ref": run.result_ref,
        "error": run.error,
        "supervisor_run_id": run.supervisor_run_id,
    })
}

fn render_run_with_idempotency(
    tool_id: &str,
    run: &CronRun,
    outcome: &IdempotencyOutcome,
) -> Value {
    let (admission, namespace, key, initial_status) = match outcome {
        IdempotencyOutcome::Inserted(record) => (
            "inserted",
            record.namespace.as_str(),
            record.key.as_str(),
            record.status.as_str(),
        ),
        IdempotencyOutcome::Replayed(record) => (
            "replayed",
            record.namespace.as_str(),
            record.key.as_str(),
            record.status.as_str(),
        ),
    };
    json!({
        "tool": tool_id,
        "status": run.status.as_str(),
        "run_id": run.id,
        "run": run_value(run),
        "idempotency": {
            "admission": admission,
            "namespace": namespace,
            "key": key,
            "initial_status": initial_status,
        },
    })
}

fn execute_tick_target(
    job: &CronJob,
    mock_skill_execution: bool,
) -> (CronRunStatus, Option<String>, Option<String>) {
    match job.target_kind {
        CronTargetKind::Builtin if job.target_ref == STORE_STATUS_BUILTIN_CRON_TARGET => (
            CronRunStatus::Succeeded,
            Some(format!("tool:cron.tick:builtin:{}", job.target_ref)),
            None,
        ),
        CronTargetKind::Builtin => (
            CronRunStatus::Failed,
            None,
            Some(format!(
                "unsupported builtin cron target `{}`",
                job.target_ref
            )),
        ),
        CronTargetKind::Skill if mock_skill_execution => (
            CronRunStatus::Succeeded,
            Some(format!("tool:cron.tick:mock-skill:{}", job.target_ref)),
            None,
        ),
        CronTargetKind::Skill => (
            CronRunStatus::Failed,
            None,
            Some("skill execution is daemon-owned and unavailable in cron.tick tool".to_string()),
        ),
    }
}

fn enqueue_cron_notification(
    store: &AglStore,
    job: &CronJob,
    run: &CronRun,
    notify_ref: &str,
) -> Result<()> {
    let body = format!(
        "Cron job `{}` ({}) {} for {}.",
        job.name,
        job.id,
        run.status.as_str(),
        run.scheduled_for
    );
    let dedupe_key = format!("cron:{}:{notify_ref}", run.id);
    store.enqueue_matrix_notification(MatrixNotificationOutboxDraft::new(
        notify_ref.to_string(),
        "cron".to_string(),
        run.id.clone(),
        dedupe_key,
        body,
    ))?;
    Ok(())
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    include_deleted: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct IdArgs {
    id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HistoryArgs {
    job_id: String,
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobDraftArgs {
    name: String,
    target_kind: CronTargetKindArg,
    target_ref: String,
    schedule_expr: String,
    enabled: Option<bool>,
    timezone: Option<String>,
    notify_ref: Option<String>,
    prompt: Option<String>,
    input: Option<String>,
}

impl JobDraftArgs {
    fn into_draft(self) -> Result<CronJobDraft> {
        let mut draft = CronJobDraft::new(
            self.name,
            self.target_kind.into(),
            self.target_ref,
            self.schedule_expr,
        );
        if let Some(enabled) = self.enabled {
            draft.enabled = enabled;
        }
        if let Some(timezone) = self.timezone {
            draft.timezone = timezone;
        }
        draft.notify_ref = self.notify_ref;
        draft.prompt = self.prompt;
        draft.input = self.input;
        Ok(draft)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateArgs {
    id: String,
    name: Option<String>,
    enabled: Option<bool>,
    target_kind: Option<CronTargetKindArg>,
    target_ref: Option<String>,
    schedule_expr: Option<String>,
    timezone: Option<String>,
    notify_ref: Option<String>,
    clear_notify_ref: Option<bool>,
    prompt: Option<String>,
    clear_prompt: Option<bool>,
    input: Option<String>,
    clear_input: Option<bool>,
}

impl UpdateArgs {
    fn into_update(self) -> Result<CronJobUpdate> {
        Ok(CronJobUpdate {
            name: self.name,
            enabled: self.enabled,
            target_kind: self.target_kind.map(Into::into),
            target_ref: self.target_ref,
            schedule_expr: self.schedule_expr,
            timezone: self.timezone,
            notify_ref: optional_update(self.notify_ref, self.clear_notify_ref),
            prompt: optional_update(self.prompt, self.clear_prompt),
            input: optional_update(self.input, self.clear_input),
        })
    }
}

fn optional_update(value: Option<String>, clear: Option<bool>) -> Option<Option<String>> {
    if clear.unwrap_or(false) {
        Some(None)
    } else {
        value.map(Some)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    job_id: String,
    scheduled_for: Option<String>,
    status: Option<CronRunStatusArg>,
    result_ref: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TickArgs {
    unix_seconds: Option<u64>,
    mock_skill_execution: Option<bool>,
}

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum CronTargetKindArg {
    Builtin,
    Skill,
}

impl From<CronTargetKindArg> for CronTargetKind {
    fn from(value: CronTargetKindArg) -> Self {
        match value {
            CronTargetKindArg::Builtin => Self::Builtin,
            CronTargetKindArg::Skill => Self::Skill,
        }
    }
}

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum CronRunStatusArg {
    Succeeded,
    Failed,
    Skipped,
}

impl From<CronRunStatusArg> for CronRunStatus {
    fn from(value: CronRunStatusArg) -> Self {
        match value {
            CronRunStatusArg::Succeeded => Self::Succeeded,
            CronRunStatusArg::Failed => Self::Failed,
            CronRunStatusArg::Skipped => Self::Skipped,
        }
    }
}

#[cfg(test)]
mod tests;
