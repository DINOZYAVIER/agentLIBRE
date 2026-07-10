use serde_json::json;

use crate::test_support::migrated_temp_root;

use super::*;

#[test]
fn cron_tools_manage_jobs_runs_and_scheduler_ticks() {
    let root = migrated_temp_root("lifecycle");
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
    assert_eq!(preflight["status"], "ok");

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
    let job_id = add["job_id"].as_str().unwrap();
    assert_eq!(add["status"], "created");

    let list = tools.dispatch(CRON_LIST_TOOL_ID, json!({})).unwrap();
    let show = tools
        .dispatch(CRON_SHOW_TOOL_ID, json!({"id": job_id}))
        .unwrap();
    assert_eq!(list["job_count"], 1);
    assert_eq!(show["job"]["target_kind"], "builtin");
    assert_eq!(show["job"]["target_ref"], "store-status");

    let updated = tools
        .dispatch(
            CRON_UPDATE_TOOL_ID,
            json!({"id": job_id, "name": "Store status tick"}),
        )
        .unwrap();
    assert_eq!(updated["status"], "updated");

    let manual_run = tools
        .dispatch(
            CRON_RUN_TOOL_ID,
            json!({"job_id": job_id, "status": "succeeded"}),
        )
        .unwrap();
    assert_eq!(manual_run["status"], "succeeded");

    let tick = tools
        .dispatch(CRON_TICK_TOOL_ID, json!({"unix_seconds": 60}))
        .unwrap();
    let replay = tools
        .dispatch(CRON_TICK_TOOL_ID, json!({"unix_seconds": 60}))
        .unwrap();
    let history = tools
        .dispatch(CRON_HISTORY_TOOL_ID, json!({"job_id": job_id}))
        .unwrap();

    assert_eq!(tick["recorded_run_count"], 1);
    assert_eq!(tick["notification_count"], 1);
    assert_eq!(replay["replayed_run_count"], 1);
    assert!(history["runs"].is_array());

    let disabled = tools
        .dispatch(CRON_DISABLE_TOOL_ID, json!({"id": job_id}))
        .unwrap();
    let enabled = tools
        .dispatch(CRON_ENABLE_TOOL_ID, json!({"id": job_id}))
        .unwrap();
    let deleted = tools
        .dispatch(CRON_DELETE_TOOL_ID, json!({"id": job_id}))
        .unwrap();

    assert_eq!(disabled["enabled"], false);
    assert_eq!(enabled["enabled"], true);
    assert_eq!(deleted["status"], "deleted");
}

#[test]
fn cron_declarations_expose_closed_schemas_and_structured_results() {
    let declaration = declaration();
    for action in &declaration.actions {
        assert_eq!(action.input_schema["additionalProperties"], false);
    }

    let add = declaration
        .actions
        .iter()
        .find(|action| action.id.as_str() == CRON_ADD_TOOL_ID)
        .unwrap();
    let required = add.input_schema["required"].as_array().unwrap();
    for field in ["name", "target_kind", "target_ref", "schedule_expr"] {
        assert!(required.iter().any(|value| value == field));
    }
    assert!(
        add.compile_schema()
            .unwrap()
            .validate(&json!({
                "name": "invalid",
                "target_kind": "shell",
                "target_ref": "noop",
                "schedule_expr": "* * * * *"
            }))
            .is_err()
    );
}
