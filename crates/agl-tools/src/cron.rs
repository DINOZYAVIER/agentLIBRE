use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_cron::{
    CronJob, CronJobDraft, CronJobUpdate, CronRepository, CronRun, CronRunAdmission, CronRunStatus,
    CronTargetKind, validate_job_draft,
};
use agl_store::{AglStore, IdempotencyOutcome, MatrixNotificationOutboxDraft};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    ToolCapability, ToolCatalog, ToolCatalogError, ToolDeclaration, ToolHandler, ToolId, ToolInput,
    ToolOperationKind, ToolOutput, ToolProviderDeclaration, ToolProviderId, ToolStateEffect,
};

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

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<String> {
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

    fn list(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<ListArgs>(CRON_LIST_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let cron = CronRepository::new(&store);
        let jobs = cron.list_jobs(args.include_deleted.unwrap_or(false))?;
        Ok(render_jobs(CRON_LIST_TOOL_ID, &jobs))
    }

    fn show(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<IdArgs>(CRON_SHOW_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let cron = CronRepository::new(&store);
        let job = cron
            .job(&args.id)?
            .with_context(|| format!("cron job not found: {}", args.id))?;
        Ok(render_jobs(CRON_SHOW_TOOL_ID, &[job]))
    }

    fn history(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<HistoryArgs>(CRON_HISTORY_TOOL_ID, arguments)?;
        let limit = args
            .limit
            .unwrap_or(DEFAULT_HISTORY_LIMIT)
            .min(MAX_HISTORY_LIMIT);
        let store = self.open_store()?;
        let cron = CronRepository::new(&store);
        let mut runs = cron.history(&args.job_id)?;
        runs.truncate(limit);
        Ok(render_runs(CRON_HISTORY_TOOL_ID, &runs))
    }

    fn preflight(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<JobDraftArgs>(CRON_PREFLIGHT_TOOL_ID, arguments)?;
        let draft = args.into_draft()?;
        validate_job_draft(&draft)?;
        Ok(format!(
            "tool=cron.preflight\nstatus=ok\ntarget_kind={}\ntarget_ref={}\nschedule_expr={}\ntimezone={}",
            draft.target_kind.as_str(),
            draft.target_ref,
            draft.schedule_expr,
            draft.timezone
        ))
    }

    fn add(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<JobDraftArgs>(CRON_ADD_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let cron = CronRepository::new(&store);
        let job = cron.add_job(args.into_draft()?)?;
        Ok(format!(
            "tool=cron.add\njob_id={}\nstatus=created\nname={}",
            job.id, job.name
        ))
    }

    fn update(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<UpdateArgs>(CRON_UPDATE_TOOL_ID, arguments)?;
        let id = args.id.clone();
        let store = self.open_store()?;
        let cron = CronRepository::new(&store);
        let job = cron.update_job(&id, args.into_update()?)?;
        Ok(format!(
            "tool=cron.update\njob_id={}\nstatus=updated\nname={}",
            job.id, job.name
        ))
    }

    fn delete(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<IdArgs>(CRON_DELETE_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let cron = CronRepository::new(&store);
        let job = cron.delete_job(&args.id)?;
        Ok(format!(
            "tool=cron.delete\njob_id={}\ndeleted={}\nstatus=deleted",
            job.id,
            job.deleted_at.is_some()
        ))
    }

    fn set_enabled(&self, arguments: Value, enabled: bool) -> Result<String> {
        let tool_id = if enabled {
            CRON_ENABLE_TOOL_ID
        } else {
            CRON_DISABLE_TOOL_ID
        };
        let args = parse_args::<IdArgs>(tool_id, arguments)?;
        let store = self.open_store()?;
        let cron = CronRepository::new(&store);
        let job = cron.set_enabled(&args.id, enabled)?;
        Ok(format!(
            "tool={tool_id}\njob_id={}\nenabled={}\nstatus=updated",
            job.id, job.enabled
        ))
    }

    fn run(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<RunArgs>(CRON_RUN_TOOL_ID, arguments)?;
        let status = parse_run_status(args.status.as_deref().unwrap_or("succeeded"))?;
        let store = self.open_store()?;
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

    fn tick(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<TickArgs>(CRON_TICK_TOOL_ID, arguments)?;
        let unix_seconds = args.unix_seconds.unwrap_or_else(current_unix_seconds);
        let mock_skill_execution = args.mock_skill_execution.unwrap_or(true);
        let store = self.open_store()?;
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

        let mut output = format!(
            "tool=cron.tick\nunix_seconds={unix_seconds}\nrecorded_runs={}\nreplayed_runs={replays}\npending_runs={pending}\nnotifications={notifications}\n---",
            recorded.len()
        );
        for run in recorded {
            output.push('\n');
            output.push_str(&format!(
                "run id={} job_id={} scheduled_for={} status={}",
                run.id,
                run.job_id,
                run.scheduled_for,
                run.status.as_str()
            ));
        }
        Ok(output)
    }

    fn open_store(&self) -> Result<AglStore> {
        AglStore::open_at(&self.store_root)
            .with_context(|| format!("failed to open cron store {}", self.store_root.display()))
    }
}

impl ToolHandler for CronTools {
    fn dispatch(&self, input: ToolInput) -> Result<ToolOutput> {
        let observation = self.dispatch(input.id.as_str(), input.arguments)?;
        Ok(ToolOutput { observation })
    }
}

pub fn declaration() -> ToolProviderDeclaration {
    ToolProviderDeclaration::new(
        ToolProviderId::new(PROVIDER_ID).expect("builtin cron provider id is valid"),
        "Cron Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin cron provider declaration is valid")
    .with_tool(tool(
        CRON_LIST_TOOL_ID,
        "List cron jobs.",
        ToolCapability::Read,
        &[],
    ))
    .with_tool(tool(
        CRON_SHOW_TOOL_ID,
        "Show one cron job.",
        ToolCapability::Read,
        &["id"],
    ))
    .with_tool(tool(
        CRON_HISTORY_TOOL_ID,
        "Show recorded runs for one cron job.",
        ToolCapability::Read,
        &["job_id"],
    ))
    .with_tool(tool(
        CRON_PREFLIGHT_TOOL_ID,
        "Validate a cron job draft without writing it.",
        ToolCapability::Read,
        &["name", "target_kind", "target_ref", "schedule_expr"],
    ))
    .with_tool(tool(
        CRON_ADD_TOOL_ID,
        "Create a local cron job.",
        ToolCapability::Write,
        &["name", "target_kind", "target_ref", "schedule_expr"],
    ))
    .with_tool(tool(
        CRON_UPDATE_TOOL_ID,
        "Update a local cron job.",
        ToolCapability::Write,
        &["id"],
    ))
    .with_tool(tool(
        CRON_DELETE_TOOL_ID,
        "Tombstone a local cron job.",
        ToolCapability::Write,
        &["id"],
    ))
    .with_tool(tool(
        CRON_ENABLE_TOOL_ID,
        "Enable a local cron job.",
        ToolCapability::Write,
        &["id"],
    ))
    .with_tool(tool(
        CRON_DISABLE_TOOL_ID,
        "Disable a local cron job.",
        ToolCapability::Write,
        &["id"],
    ))
    .with_tool(
        tool(
            CRON_RUN_TOOL_ID,
            "Record a manual cron run for an exact job and optional scheduled timestamp.",
            ToolCapability::Write,
            &["job_id"],
        )
        .with_operation_kind(ToolOperationKind::Execute),
    )
    .with_tool(
        tool(
            CRON_TICK_TOOL_ID,
            "Run one local scheduler tick and enqueue Matrix notifications locally.",
            ToolCapability::Write,
            &[],
        )
        .with_operation_kind(ToolOperationKind::Execute)
        .with_state_effects([ToolStateEffect::StoreCron, ToolStateEffect::MatrixOutbox]),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn tool(
    id: &str,
    description: &str,
    capability: ToolCapability,
    required_arguments: &[&str],
) -> ToolDeclaration {
    let declaration = ToolDeclaration::new(
        ToolId::new(id).expect("builtin cron tool id is valid"),
        description,
        capability,
        required_arguments.iter().copied(),
    );
    match id {
        CRON_ADD_TOOL_ID | CRON_UPDATE_TOOL_ID | CRON_DELETE_TOOL_ID | CRON_ENABLE_TOOL_ID
        | CRON_DISABLE_TOOL_ID => declaration.with_state_effects([ToolStateEffect::StoreCron]),
        CRON_RUN_TOOL_ID => declaration.with_state_effects([ToolStateEffect::StoreCron]),
        _ => declaration,
    }
}

fn parse_args<T: for<'de> Deserialize<'de>>(tool: &str, arguments: Value) -> Result<T> {
    serde_json::from_value(arguments).with_context(|| format!("{tool} arguments are invalid"))
}

fn parse_target_kind(value: &str) -> Result<CronTargetKind> {
    match value {
        "builtin" => Ok(CronTargetKind::Builtin),
        "skill" => Ok(CronTargetKind::Skill),
        _ => anyhow::bail!("unknown cron target kind `{value}`"),
    }
}

fn parse_run_status(value: &str) -> Result<CronRunStatus> {
    match value {
        "succeeded" => Ok(CronRunStatus::Succeeded),
        "failed" => Ok(CronRunStatus::Failed),
        "skipped" => Ok(CronRunStatus::Skipped),
        _ => anyhow::bail!("unknown cron run status `{value}`"),
    }
}

fn render_jobs(tool_id: &str, jobs: &[CronJob]) -> String {
    let mut output = format!("tool={tool_id}\njobs={}\n---", jobs.len());
    for job in jobs {
        output.push('\n');
        output.push_str(&format!(
            "job id={} name={} enabled={} target={}:{} schedule={} timezone={} deleted={}",
            job.id,
            job.name,
            job.enabled,
            job.target_kind.as_str(),
            job.target_ref,
            job.schedule_expr,
            job.timezone,
            job.deleted_at.is_some()
        ));
    }
    output
}

fn render_runs(tool_id: &str, runs: &[CronRun]) -> String {
    let mut output = format!("tool={tool_id}\nruns={}\n---", runs.len());
    for run in runs {
        output.push('\n');
        output.push_str(&format!(
            "run id={} job_id={} scheduled_for={} status={}",
            run.id,
            run.job_id,
            run.scheduled_for,
            run.status.as_str()
        ));
    }
    output
}

fn render_run_with_idempotency(
    tool_id: &str,
    run: &CronRun,
    outcome: &IdempotencyOutcome,
) -> String {
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
    format!(
        "tool={tool_id}\nrun_id={}\njob_id={}\nscheduled_for={}\nstatus={}\nidempotency.admission={admission}\nidempotency.namespace={namespace}\nidempotency.key={key}\nidempotency.initial_status={initial_status}",
        run.id,
        run.job_id,
        run.scheduled_for,
        run.status.as_str()
    )
}

fn execute_tick_target(
    job: &CronJob,
    mock_skill_execution: bool,
) -> (CronRunStatus, Option<String>, Option<String>) {
    match job.target_kind {
        CronTargetKind::Builtin if job.target_ref == "store-status" => (
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

#[derive(Deserialize)]
struct ListArgs {
    include_deleted: Option<bool>,
}

#[derive(Deserialize)]
struct IdArgs {
    id: String,
}

#[derive(Deserialize)]
struct HistoryArgs {
    job_id: String,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct JobDraftArgs {
    name: String,
    target_kind: String,
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
            parse_target_kind(&self.target_kind)?,
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

#[derive(Deserialize)]
struct UpdateArgs {
    id: String,
    name: Option<String>,
    enabled: Option<bool>,
    target_kind: Option<String>,
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
            target_kind: self
                .target_kind
                .as_deref()
                .map(parse_target_kind)
                .transpose()?,
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

#[derive(Deserialize)]
struct RunArgs {
    job_id: String,
    scheduled_for: Option<String>,
    status: Option<String>,
    result_ref: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct TickArgs {
    unix_seconds: Option<u64>,
    mock_skill_execution: Option<bool>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::*;

    #[test]
    fn cron_tools_manage_jobs_runs_and_scheduler_ticks() {
        let root = temp_root("lifecycle");
        let tools = CronTools::new(&root);

        let preflight = tools
            .dispatch(
                CRON_PREFLIGHT_TOOL_ID,
                json!({
                    "name": "Store status",
                    "target_kind": "builtin",
                    "target_ref": "store-status",
                    "schedule_expr": "* * * * *",
                    "timezone": "UTC",
                    "notify_ref": "matrix-room:!room:example.org"
                }),
            )
            .unwrap();
        assert!(preflight.contains("status=ok"));

        let add = tools
            .dispatch(
                CRON_ADD_TOOL_ID,
                json!({
                    "name": "Store status",
                    "target_kind": "builtin",
                    "target_ref": "store-status",
                    "schedule_expr": "* * * * *",
                    "timezone": "UTC",
                    "notify_ref": "matrix-room:!room:example.org"
                }),
            )
            .unwrap();
        let job_id = value_for(&add, "job_id=").unwrap();
        assert!(add.contains("status=created"));

        let list = tools.dispatch(CRON_LIST_TOOL_ID, json!({})).unwrap();
        let show = tools
            .dispatch(CRON_SHOW_TOOL_ID, json!({"id": job_id}))
            .unwrap();
        assert!(list.contains("jobs=1"));
        assert!(show.contains("target=builtin:store-status"));

        let updated = tools
            .dispatch(
                CRON_UPDATE_TOOL_ID,
                json!({"id": job_id, "name": "Store status tick"}),
            )
            .unwrap();
        assert!(updated.contains("status=updated"));

        let manual_run = tools
            .dispatch(
                CRON_RUN_TOOL_ID,
                json!({"job_id": job_id, "status": "succeeded"}),
            )
            .unwrap();
        assert!(manual_run.contains("status=succeeded"));

        let tick = tools
            .dispatch(CRON_TICK_TOOL_ID, json!({"unix_seconds": 60}))
            .unwrap();
        let replay = tools
            .dispatch(CRON_TICK_TOOL_ID, json!({"unix_seconds": 60}))
            .unwrap();
        let history = tools
            .dispatch(CRON_HISTORY_TOOL_ID, json!({"job_id": job_id}))
            .unwrap();

        assert!(tick.contains("recorded_runs=1"));
        assert!(tick.contains("notifications=1"));
        assert!(replay.contains("replayed_runs=1"));
        assert!(history.contains("runs="));

        let disabled = tools
            .dispatch(CRON_DISABLE_TOOL_ID, json!({"id": job_id}))
            .unwrap();
        let enabled = tools
            .dispatch(CRON_ENABLE_TOOL_ID, json!({"id": job_id}))
            .unwrap();
        let deleted = tools
            .dispatch(CRON_DELETE_TOOL_ID, json!({"id": job_id}))
            .unwrap();

        assert!(disabled.contains("enabled=false"));
        assert!(enabled.contains("enabled=true"));
        assert!(deleted.contains("status=deleted"));

        cleanup(root);
    }

    fn value_for(output: &str, prefix: &str) -> Option<String> {
        output
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .map(str::to_string)
    }

    fn temp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "agl-cron-tools-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn cleanup(root: PathBuf) {
        let _ = std::fs::remove_dir_all(root);
    }
}
