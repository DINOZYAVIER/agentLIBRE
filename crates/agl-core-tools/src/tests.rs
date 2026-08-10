use std::collections::BTreeSet;

use agl_kernel::{EffectId, OperationKind, ToolId};

use super::*;

#[test]
fn read_actions_do_not_create_or_migrate_an_absent_store() {
    let root = test_support::temp_root("cron-read");
    assert!(
        CronTools::new(&root)
            .dispatch(CRON_LIST_TOOL_ID, serde_json::json!({}))
            .is_err()
    );
    assert!(!root.join(agl_store::DEFAULT_DATABASE_FILE).exists());

    let root = test_support::temp_root("memory-read");
    assert!(
        MemoryTools::new(&root)
            .dispatch(MEMORY_LIST_TOOL_ID, serde_json::json!({}))
            .is_err()
    );
    assert!(!root.join(agl_store::DEFAULT_DATABASE_FILE).exists());

    let root = test_support::temp_root("notes-read");
    assert!(
        NotesTools::new(&root)
            .dispatch(NOTES_SEARCH_TOOL_ID, serde_json::json!({"query": "x"}))
            .is_err()
    );
    assert!(!root.join(agl_store::DEFAULT_DATABASE_FILE).exists());

    let root = test_support::temp_root("permissions-read");
    assert!(
        PermissionTools::new(&root)
            .dispatch(PERMISSIONS_STATUS_TOOL_ID, serde_json::json!({}))
            .is_err()
    );
    assert!(!root.join(agl_store::DEFAULT_DATABASE_FILE).exists());

    let root = test_support::temp_root("matrix-read");
    assert!(
        MatrixTools::new(&root)
            .dispatch(MATRIX_OUTBOX_STATUS_TOOL_ID, serde_json::json!({}))
            .is_err()
    );
    assert!(!root.join(agl_store::DEFAULT_DATABASE_FILE).exists());
}

#[test]
fn builtin_catalog_has_complete_valid_schemas_and_expected_coverage() {
    let catalog = builtin_tool_catalog().unwrap();
    let actions = catalog
        .extensions()
        .iter()
        .flat_map(|extension| extension.tools.iter())
        .collect::<Vec<_>>();

    assert_eq!(actions.len(), 56);
    for extension in catalog.extensions() {
        extension.validate().unwrap();
        for action in &extension.tools {
            let schema = action.compile_schema().unwrap();
            assert!(
                schema
                    .validate(&serde_json::json!({"__unknown": true}))
                    .is_err(),
                "{} must reject unknown top-level fields",
                action.id
            );
        }
    }
}

#[test]
fn builtin_actions_declare_operation_kinds_and_state_effects() {
    let catalog = builtin_tool_catalog().unwrap();
    let expected = [
        (FS_READ_TOOL_ID, OperationKind::Read, &[][..]),
        (FS_LIST_TOOL_ID, OperationKind::Read, &[]),
        (FS_SEARCH_TOOL_ID, OperationKind::Read, &[]),
        (
            FS_APPLY_PATCH_TOOL_ID,
            OperationKind::Write,
            &[EffectId::repo_files()],
        ),
        (MEMORY_SEARCH_TOOL_ID, OperationKind::Read, &[]),
        (MEMORY_LIST_TOOL_ID, OperationKind::Read, &[]),
        (
            MEMORY_SUGGEST_TOOL_ID,
            OperationKind::Write,
            &[EffectId::store_memory_suggestions()],
        ),
        (
            MEMORY_ADD_TOOL_ID,
            OperationKind::Write,
            &[EffectId::store_memory_entries()],
        ),
        (
            MEMORY_APPROVE_TOOL_ID,
            OperationKind::Approve,
            &[
                EffectId::store_memory_entries(),
                EffectId::store_memory_suggestions(),
            ],
        ),
        (
            MEMORY_REJECT_TOOL_ID,
            OperationKind::Approve,
            &[EffectId::store_memory_suggestions()],
        ),
        (
            NOTES_ADD_TOOL_ID,
            OperationKind::Write,
            &[EffectId::store_notes()],
        ),
        (NOTES_SEARCH_TOOL_ID, OperationKind::Read, &[]),
        (NOTES_SHOW_TOOL_ID, OperationKind::Read, &[]),
        (
            NOTES_UPDATE_TOOL_ID,
            OperationKind::Write,
            &[EffectId::store_notes()],
        ),
        (
            NOTES_LINK_TOOL_ID,
            OperationKind::Write,
            &[EffectId::store_note_links()],
        ),
        (
            NOTES_DELETE_TOOL_ID,
            OperationKind::Write,
            &[EffectId::store_notes()],
        ),
        (
            NOTES_REMEMBER_TOOL_ID,
            OperationKind::Approve,
            &[
                EffectId::store_memory_entries(),
                EffectId::store_note_links(),
            ],
        ),
        (CRON_LIST_TOOL_ID, OperationKind::Read, &[]),
        (CRON_SHOW_TOOL_ID, OperationKind::Read, &[]),
        (CRON_HISTORY_TOOL_ID, OperationKind::Read, &[]),
        (CRON_PREFLIGHT_TOOL_ID, OperationKind::Read, &[]),
        (
            CRON_ADD_TOOL_ID,
            OperationKind::Write,
            &[EffectId::store_cron()],
        ),
        (
            CRON_UPDATE_TOOL_ID,
            OperationKind::Write,
            &[EffectId::store_cron()],
        ),
        (
            CRON_DELETE_TOOL_ID,
            OperationKind::Write,
            &[EffectId::store_cron()],
        ),
        (
            CRON_ENABLE_TOOL_ID,
            OperationKind::Write,
            &[EffectId::store_cron()],
        ),
        (
            CRON_DISABLE_TOOL_ID,
            OperationKind::Write,
            &[EffectId::store_cron()],
        ),
        (
            CRON_RUN_TOOL_ID,
            OperationKind::Execute,
            &[EffectId::store_cron(), EffectId::store_idempotency()],
        ),
        (
            CRON_TICK_TOOL_ID,
            OperationKind::Execute,
            &[
                EffectId::store_cron(),
                EffectId::store_idempotency(),
                EffectId::matrix_outbox(),
            ],
        ),
        (MATRIX_OUTBOX_STATUS_TOOL_ID, OperationKind::Read, &[]),
        (
            MATRIX_OUTBOX_ENQUEUE_TOOL_ID,
            OperationKind::Write,
            &[EffectId::matrix_outbox()],
        ),
        (
            MATRIX_OUTBOX_DELIVER_TOOL_ID,
            OperationKind::Execute,
            &[EffectId::matrix_outbox()],
        ),
        (STORE_STATUS_TOOL_ID, OperationKind::Read, &[]),
        (STORE_EXPORT_TOOL_ID, OperationKind::Read, &[]),
        (
            STORE_MIGRATE_TOOL_ID,
            OperationKind::Admin,
            &[EffectId::store_schema()],
        ),
        (
            ARTIFACT_COMMIT_TOOL_ID,
            OperationKind::Write,
            &[
                EffectId::new("agl:artifact.repository").unwrap(),
                EffectId::new("agl:repo.gitlink").unwrap(),
            ],
        ),
        (TASKS_VERIFY_TOOL_ID, OperationKind::Read, &[]),
        (SKILL_LIST_TOOL_ID, OperationKind::Read, &[]),
        (SKILL_INSPECT_TOOL_ID, OperationKind::Read, &[]),
        (SKILL_STATUS_TOOL_ID, OperationKind::Read, &[]),
        (SKILL_VERIFY_TOOL_ID, OperationKind::Read, &[]),
        (
            SKILL_TRUST_TOOL_ID,
            OperationKind::Approve,
            &[EffectId::skill_trust()],
        ),
        (
            SKILL_REVOKE_TOOL_ID,
            OperationKind::Approve,
            &[EffectId::skill_trust()],
        ),
        (PERMISSIONS_STATUS_TOOL_ID, OperationKind::Read, &[]),
        (
            PERMISSIONS_REQUEST_TOOL_ID,
            OperationKind::Request,
            &[EffectId::store_permission_requests()],
        ),
        (
            PERMISSIONS_GRANT_TOOL_ID,
            OperationKind::Approve,
            &[
                EffectId::store_permission_grants(),
                EffectId::store_permission_requests(),
            ],
        ),
        (
            PERMISSIONS_REVOKE_TOOL_ID,
            OperationKind::Approve,
            &[EffectId::store_permission_grants()],
        ),
        (PROCESS_PWD_TOOL_ID, OperationKind::Read, &[]),
        (
            PROCESS_CD_TOOL_ID,
            OperationKind::Write,
            &[EffectId::session_working_directory()],
        ),
        (
            PROCESS_EXEC_TOOL_ID,
            OperationKind::Execute,
            &[EffectId::spawn_process()],
        ),
        (
            PROCESS_START_TOOL_ID,
            OperationKind::Execute,
            &[EffectId::spawn_process()],
        ),
        (PROCESS_STATUS_TOOL_ID, OperationKind::Read, &[]),
        (PROCESS_READ_TOOL_ID, OperationKind::Read, &[]),
        (
            PROCESS_WRITE_TOOL_ID,
            OperationKind::Execute,
            &[EffectId::control_process()],
        ),
        (
            PROCESS_RESIZE_TOOL_ID,
            OperationKind::Execute,
            &[EffectId::control_process()],
        ),
        (
            PROCESS_KILL_TOOL_ID,
            OperationKind::Execute,
            &[EffectId::control_process()],
        ),
        (
            SHELL_EXEC_TOOL_ID,
            OperationKind::Execute,
            &[EffectId::spawn_process()],
        ),
    ];

    assert_eq!(expected.len(), 56);
    for (id, operation_kind, effects) in expected {
        assert_action_metadata(&catalog, id, operation_kind, effects);
    }
}

fn assert_action_metadata(
    catalog: &ToolCatalog,
    id: &str,
    operation_kind: OperationKind,
    state_effects: &[EffectId],
) {
    let id = ToolId::new(id).unwrap();
    let action = catalog.tool(&id).unwrap();
    assert_eq!(action.operation_kind, operation_kind, "{id}");
    assert_eq!(
        action.state_effects,
        state_effects.iter().cloned().collect::<BTreeSet<_>>(),
        "{id}"
    );
}
