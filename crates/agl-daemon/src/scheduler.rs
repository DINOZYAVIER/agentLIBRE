use agl_chat::{ChatOptions, ChatService, ChatTurnStatus, InferenceOptions, ToolAccessMode};
use agl_cron::{CronJob, CronRepository, CronRun, CronRunAdmission, CronRunStatus};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_store::AglStore;
use anyhow::{Context, Result, bail};

pub const STORE_STATUS_BUILTIN_CRON_TARGET: &str = "store-status";

pub fn supported_builtin_cron_targets() -> &'static [&'static str] {
    &[STORE_STATUS_BUILTIN_CRON_TARGET]
}

pub fn validate_builtin_cron_target(target_ref: &str) -> Result<()> {
    if supported_builtin_cron_targets().contains(&target_ref) {
        return Ok(());
    }
    bail!("{}", unsupported_builtin_cron_target_message(target_ref))
}

pub fn unsupported_builtin_cron_target_message(target_ref: &str) -> String {
    format!(
        "unknown builtin cron target: {target_ref}; supported builtin targets: {}",
        supported_builtin_cron_targets().join(", ")
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CronExecution {
    pub status: CronRunStatus,
    pub result_ref: Option<String>,
    pub error: Option<String>,
}

impl CronExecution {
    pub fn succeeded(result_ref: impl Into<String>) -> Self {
        Self {
            status: CronRunStatus::Succeeded,
            result_ref: Some(result_ref.into()),
            error: None,
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            status: CronRunStatus::Failed,
            result_ref: None,
            error: Some(error.into()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CronNotification {
    pub notify_ref: String,
    pub run_id: String,
    pub job_id: String,
    pub job_name: String,
    pub scheduled_for: String,
    pub status: CronRunStatus,
    pub result_ref: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CronSchedulerReport {
    pub due_jobs: usize,
    pub recorded_runs: Vec<CronRun>,
    pub notifications: usize,
}

pub trait CronTargetExecutor {
    fn execute(&mut self, job: &CronJob) -> CronExecution;
}

pub trait CronNotifier {
    fn notify(&mut self, notification: CronNotification) -> Result<()>;
}

pub fn render_cron_skill_prompt(job: &CronJob) -> Result<String> {
    let prompt = job
        .prompt
        .as_deref()
        .context("skill cron job missing prompt")?;
    if let Some(input) = job.input.as_deref() {
        Ok(format!("{prompt}\n\nCron input:\n{input}"))
    } else {
        Ok(prompt.to_string())
    }
}

pub fn render_cron_notification_body(notification: &CronNotification) -> String {
    let mut body = format!(
        "Cron job `{}` ({}) {} for {}.",
        notification.job_name,
        notification.job_id,
        notification.status.as_str(),
        notification.scheduled_for
    );
    if let Some(result_ref) = &notification.result_ref {
        body.push_str(&format!("\nresult_ref: {result_ref}"));
    }
    if let Some(error) = &notification.error {
        body.push_str(&format!("\nerror: {error}"));
    }
    body
}

pub fn run_cron_skill_chat_turn(
    job: &CronJob,
    runtime: &AgentLibreRuntimeConfig,
    mut inference: InferenceOptions,
    context_label: Option<&str>,
) -> Result<String> {
    let prompt = render_cron_skill_prompt(job)?;
    inference.skills.push(job.target_ref.clone());
    inference.tool_mode = ToolAccessMode::Write;
    let mut service = ChatService::open(
        ChatOptions {
            inference,
            workspace_root: None,
            session_id: None,
            no_history: false,
            new_session: true,
        },
        runtime,
    )
    .with_context(|| cron_skill_context("open", context_label, "chat session"))?;
    let summary = service.summary();
    let output = service
        .run_user_turn(&prompt)
        .with_context(|| cron_skill_context("run", context_label, "turn"))?;
    service
        .finish_eof_if_needed()
        .with_context(|| cron_skill_context("finish", context_label, "session"))?;
    match output.status {
        ChatTurnStatus::Answered { .. } => Ok(format!(
            "skill:{}:session:{}:run:{}",
            job.target_ref, summary.session_id, summary.run_id
        )),
        ChatTurnStatus::Stopped { reason } => bail!("cron skill stopped before answer: {reason:?}"),
    }
}

fn cron_skill_context(verb: &str, label: Option<&str>, noun: &str) -> String {
    match label.filter(|value| !value.is_empty()) {
        Some(label) => format!("failed to {verb} {label} cron skill {noun}"),
        None => format!("failed to {verb} cron skill {noun}"),
    }
}

#[derive(Default)]
pub struct NoopCronNotifier;

impl CronNotifier for NoopCronNotifier {
    fn notify(&mut self, _notification: CronNotification) -> Result<()> {
        Ok(())
    }
}

pub fn run_cron_tick(
    store: &AglStore,
    unix_seconds: u64,
    executor: &mut impl CronTargetExecutor,
    notifier: &mut impl CronNotifier,
) -> Result<CronSchedulerReport> {
    let repo = CronRepository::new(store);
    let due_jobs = repo
        .due_jobs(unix_seconds)
        .context("failed to compute due cron jobs")?;
    let mut report = CronSchedulerReport {
        due_jobs: due_jobs.len(),
        ..CronSchedulerReport::default()
    };

    for due in due_jobs {
        let admission = repo
            .begin_run_for(&due.job, &due.scheduled_for)
            .context("failed to admit cron scheduler run")?;
        match admission {
            CronRunAdmission::Replayed(run, _) => {
                report.recorded_runs.push(run);
                continue;
            }
            CronRunAdmission::Pending(_) => {
                continue;
            }
            CronRunAdmission::Inserted(_) => {}
        }
        let execution = executor.execute(&due.job);
        let run = repo
            .record_admitted_run(
                &due.job.id,
                &due.scheduled_for,
                execution.status,
                execution.result_ref.as_deref(),
                execution.error.as_deref(),
            )
            .context("failed to record cron scheduler run")?;
        if let Some(notify_ref) = &due.job.notify_ref {
            notifier
                .notify(CronNotification {
                    notify_ref: notify_ref.clone(),
                    run_id: run.id.clone(),
                    job_id: due.job.id.clone(),
                    job_name: due.job.name.clone(),
                    scheduled_for: due.scheduled_for,
                    status: run.status,
                    result_ref: run.result_ref.clone(),
                    error: run.error.clone(),
                })
                .context("failed to notify cron run")?;
            report.notifications += 1;
        }
        report.recorded_runs.push(run);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use agl_cron::CronTargetKind;

    use super::*;

    #[test]
    fn cron_skill_prompt_renders_optional_input() {
        let mut job = cron_job();
        job.prompt = Some("Review changes.".to_string());
        job.input = Some("Only docs.".to_string());

        assert_eq!(
            render_cron_skill_prompt(&job).unwrap(),
            "Review changes.\n\nCron input:\nOnly docs."
        );
    }

    #[test]
    fn builtin_cron_target_validation_reports_supported_targets() {
        validate_builtin_cron_target(STORE_STATUS_BUILTIN_CRON_TARGET).unwrap();
        let err = validate_builtin_cron_target("missing-target").unwrap_err();

        assert_eq!(
            err.to_string(),
            "unknown builtin cron target: missing-target; supported builtin targets: store-status"
        );
    }

    #[test]
    fn cron_notification_body_includes_result_and_error_refs() {
        let body = render_cron_notification_body(&CronNotification {
            notify_ref: "matrix-room:!room".to_string(),
            run_id: "run-001".to_string(),
            job_id: "job-001".to_string(),
            job_name: "Nightly".to_string(),
            scheduled_for: "2026-07-03T00:00:00Z".to_string(),
            status: CronRunStatus::Failed,
            result_ref: Some("result:ref".to_string()),
            error: Some("boom".to_string()),
        });

        assert!(body.contains("Cron job `Nightly` (job-001) failed"));
        assert!(body.contains("result_ref: result:ref"));
        assert!(body.contains("error: boom"));
    }

    fn cron_job() -> CronJob {
        CronJob {
            id: "job-001".to_string(),
            name: "Nightly".to_string(),
            enabled: true,
            target_kind: CronTargetKind::Skill,
            target_ref: "repo-review".to_string(),
            schedule_expr: "daily".to_string(),
            timezone: "UTC".to_string(),
            notify_ref: None,
            prompt: None,
            input: None,
            created_at: "2026-07-03T00:00:00Z".to_string(),
            updated_at: "2026-07-03T00:00:00Z".to_string(),
            deleted_at: None,
        }
    }
}
