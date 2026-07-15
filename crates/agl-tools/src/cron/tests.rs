use serde_json::json;

use crate::test_support::{temp_root, value_for};

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
}
