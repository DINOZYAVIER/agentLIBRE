use agl_cron::{CronJob, CronRepository, CronRun, CronRunAdmission, CronRunStatus};
use agl_ids::RunId;
use agl_store::AglStore;
use anyhow::{Context, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CronExecution {
    pub status: CronRunStatus,
    pub result_ref: Option<String>,
    pub error: Option<String>,
    pub supervisor_run_id: Option<RunId>,
}

impl CronExecution {
    pub fn succeeded(result_ref: impl Into<String>) -> Self {
        Self {
            status: CronRunStatus::Succeeded,
            result_ref: Some(result_ref.into()),
            error: None,
            supervisor_run_id: None,
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            status: CronRunStatus::Failed,
            result_ref: None,
            error: Some(error.into()),
            supervisor_run_id: None,
        }
    }

    pub fn queued(supervisor_run_id: RunId) -> Self {
        Self {
            status: CronRunStatus::Queued,
            result_ref: Some(format!("run:{supervisor_run_id}")),
            error: None,
            supervisor_run_id: Some(supervisor_run_id),
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
    fn execute(&mut self, job: &CronJob, scheduled_for: &str) -> CronExecution;
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
            CronRunAdmission::Pending(_) | CronRunAdmission::Inserted(_) => {}
        }
        let execution = executor.execute(&due.job, &due.scheduled_for);
        let run = if execution.status == CronRunStatus::Queued {
            let supervisor_run_id = execution
                .supervisor_run_id
                .as_ref()
                .context("queued cron execution is missing supervisor run ID")?;
            repo.record_admitted_supervisor_run(&due.job.id, &due.scheduled_for, supervisor_run_id)
        } else {
            repo.record_admitted_run(
                &due.job.id,
                &due.scheduled_for,
                execution.status,
                execution.result_ref.as_deref(),
                execution.error.as_deref(),
            )
        }
        .context("failed to record cron scheduler run")?;
        if run.status != CronRunStatus::Queued
            && run.status != CronRunStatus::Running
            && let Some(notify_ref) = &due.job.notify_ref
        {
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
