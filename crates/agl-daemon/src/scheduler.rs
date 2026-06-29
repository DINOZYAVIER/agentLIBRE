use agl_cron::{CronJob, CronRepository, CronRun, CronRunAdmission, CronRunStatus};
use agl_store::AglStore;
use anyhow::{Context, Result};

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
