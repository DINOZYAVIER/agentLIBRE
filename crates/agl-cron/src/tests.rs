use super::*;
use std::path::PathBuf;

#[test]
fn validates_human_and_cron_schedules() {
    for value in ["hourly", "daily 09:00", "weekly mon 10:30", "0 9 * * *"] {
        validate_schedule_expr(value).unwrap();
    }
    validate_schedule_expr("0 0 1 jan thu").unwrap();

    assert!(validate_schedule_expr("daily 99:00").is_err());
    assert!(validate_schedule_expr("weekly monday 10:00").is_err());
    assert!(validate_schedule_expr("61 * * * *").is_err());
    assert!(validate_schedule_expr("0 0 32 * *").is_err());
}

#[test]
fn validates_utc_and_fixed_offset_timezones() {
    for value in ["UTC", "Z", "+02:00", "-07:00", "UTC+05:30"] {
        parse_timezone(value).unwrap();
    }

    assert!(parse_timezone("local").is_err());
    assert!(parse_timezone("America/Edmonton").is_err());
    assert!(parse_timezone("+24:00").is_err());
}

#[test]
fn adds_lists_and_toggles_jobs() {
    let root = temp_root("jobs");
    let store = AglStore::open_at(&root).unwrap();
    let repo = CronRepository::new(&store);

    let mut draft = CronJobDraft::new(
        "Daily review",
        CronTargetKind::Skill,
        "repo-review",
        "daily 09:00",
    );
    draft.prompt = Some("Review repository changes.".to_string());
    let job = repo.add_job(draft).unwrap();
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

#[test]
fn due_jobs_match_human_schedules() {
    let root = temp_root("due-human");
    let store = AglStore::open_at(&root).unwrap();
    let repo = CronRepository::new(&store);
    let hourly = repo
        .add_job(CronJobDraft::new(
            "Hourly",
            CronTargetKind::Builtin,
            "store-status",
            "hourly",
        ))
        .unwrap();
    let daily = repo
        .add_job(CronJobDraft::new(
            "Daily",
            CronTargetKind::Builtin,
            "store-status",
            "daily 01:30",
        ))
        .unwrap();
    let weekly = repo
        .add_job(CronJobDraft::new(
            "Weekly",
            CronTargetKind::Builtin,
            "store-status",
            "weekly thu 00:00",
        ))
        .unwrap();

    let due_at_epoch = repo.due_jobs(0).unwrap();
    let due_at_0130 = repo.due_jobs(90 * 60).unwrap();

    let mut due_at_epoch_ids = due_at_epoch
        .iter()
        .map(|due| due.job.id.as_str())
        .collect::<Vec<_>>();
    due_at_epoch_ids.sort();
    assert_eq!(due_at_epoch_ids, {
        let mut expected = vec![hourly.id.as_str(), weekly.id.as_str()];
        expected.sort();
        expected
    });
    assert_eq!(
        due_at_0130
            .iter()
            .map(|due| due.job.id.as_str())
            .collect::<Vec<_>>(),
        vec![daily.id.as_str()]
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn due_jobs_match_cron_minute_and_hour_fields() {
    let root = temp_root("due-cron");
    let store = AglStore::open_at(&root).unwrap();
    let repo = CronRepository::new(&store);
    let job = repo
        .add_job(CronJobDraft::new(
            "Cron",
            CronTargetKind::Builtin,
            "store-status",
            "*/15 9-10 * * *",
        ))
        .unwrap();

    let due = repo.due_jobs((9 * 60 + 30) * 60).unwrap();
    let not_due = repo.due_jobs((11 * 60 + 30) * 60).unwrap();

    assert_eq!(due[0].job.id, job.id);
    assert_eq!(due[0].scheduled_for, "unix:34200");
    assert!(not_due.is_empty());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn due_jobs_match_fixed_timezone_offsets() {
    let root = temp_root("due-offset");
    let store = AglStore::open_at(&root).unwrap();
    let repo = CronRepository::new(&store);
    let mut draft = CronJobDraft::new(
        "Offset daily",
        CronTargetKind::Builtin,
        "store-status",
        "daily 09:00",
    );
    draft.timezone = "+02:00".to_string();
    let job = repo.add_job(draft).unwrap();

    let due = repo.due_jobs(7 * 3600).unwrap();
    let not_due = repo.due_jobs(9 * 3600).unwrap();

    assert_eq!(due[0].job.id, job.id);
    assert_eq!(due[0].scheduled_for, "unix:25200");
    assert!(not_due.is_empty());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn due_jobs_match_full_cron_fields() {
    let root = temp_root("due-full-cron");
    let store = AglStore::open_at(&root).unwrap();
    let repo = CronRepository::new(&store);
    let jan_first = repo
        .add_job(CronJobDraft::new(
            "Jan first",
            CronTargetKind::Builtin,
            "store-status",
            "0 0 1 jan *",
        ))
        .unwrap();
    let thursday = repo
        .add_job(CronJobDraft::new(
            "Thursday",
            CronTargetKind::Builtin,
            "store-status",
            "0 0 * * thu",
        ))
        .unwrap();
    let dom_or_dow = repo
        .add_job(CronJobDraft::new(
            "Dom or dow",
            CronTargetKind::Builtin,
            "store-status",
            "0 0 2 * thu",
        ))
        .unwrap();
    repo.add_job(CronJobDraft::new(
        "February",
        CronTargetKind::Builtin,
        "store-status",
        "0 0 * feb *",
    ))
    .unwrap();

    let due = repo.due_jobs(0).unwrap();
    let mut due_ids = due
        .iter()
        .map(|due| due.job.id.as_str())
        .collect::<Vec<_>>();
    due_ids.sort();
    let mut expected = vec![
        jan_first.id.as_str(),
        thursday.id.as_str(),
        dom_or_dow.id.as_str(),
    ];
    expected.sort();

    assert_eq!(due_ids, expected);

    std::fs::remove_dir_all(root).unwrap();
}

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("agl-cron-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}
