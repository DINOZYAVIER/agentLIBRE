use std::collections::BTreeSet;

use agl_capabilities::{CapabilityId, OperationKind, StateEffect};

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
        .providers()
        .iter()
        .flat_map(|provider| provider.actions.iter())
        .collect::<Vec<_>>();

    assert_eq!(actions.len(), 51);
    for provider in catalog.providers() {
        provider.validate().unwrap();
        for action in &provider.actions {
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
            FS_EDIT_TOOL_ID,
            OperationKind::Write,
            &[StateEffect::RepoFiles],
        ),
        (MEMORY_SEARCH_TOOL_ID, OperationKind::Read, &[]),
        (MEMORY_LIST_TOOL_ID, OperationKind::Read, &[]),
        (
            MEMORY_SUGGEST_TOOL_ID,
            OperationKind::Write,
            &[StateEffect::StoreMemorySuggestions],
        ),
        (
            MEMORY_ADD_TOOL_ID,
            OperationKind::Write,
            &[StateEffect::StoreMemoryEntries],
        ),
        (
            MEMORY_APPROVE_TOOL_ID,
            OperationKind::Approve,
            &[
                StateEffect::StoreMemoryEntries,
                StateEffect::StoreMemorySuggestions,
            ],
        ),
        (
            MEMORY_REJECT_TOOL_ID,
            OperationKind::Approve,
            &[StateEffect::StoreMemorySuggestions],
        ),
        (
            NOTES_ADD_TOOL_ID,
            OperationKind::Write,
            &[StateEffect::StoreNotes],
        ),
        (NOTES_SEARCH_TOOL_ID, OperationKind::Read, &[]),
        (NOTES_SHOW_TOOL_ID, OperationKind::Read, &[]),
        (
            NOTES_UPDATE_TOOL_ID,
            OperationKind::Write,
            &[StateEffect::StoreNotes],
        ),
        (
            NOTES_LINK_TOOL_ID,
            OperationKind::Write,
            &[StateEffect::StoreNoteLinks],
        ),
        (
            NOTES_DELETE_TOOL_ID,
            OperationKind::Write,
            &[StateEffect::StoreNotes],
        ),
        (
            NOTES_REMEMBER_TOOL_ID,
            OperationKind::Approve,
            &[StateEffect::StoreMemoryEntries, StateEffect::StoreNoteLinks],
        ),
        (CRON_LIST_TOOL_ID, OperationKind::Read, &[]),
        (CRON_SHOW_TOOL_ID, OperationKind::Read, &[]),
        (CRON_HISTORY_TOOL_ID, OperationKind::Read, &[]),
        (CRON_PREFLIGHT_TOOL_ID, OperationKind::Read, &[]),
        (
            CRON_ADD_TOOL_ID,
            OperationKind::Write,
            &[StateEffect::StoreCron],
        ),
        (
            CRON_UPDATE_TOOL_ID,
            OperationKind::Write,
            &[StateEffect::StoreCron],
        ),
        (
            CRON_DELETE_TOOL_ID,
            OperationKind::Write,
            &[StateEffect::StoreCron],
        ),
        (
            CRON_ENABLE_TOOL_ID,
            OperationKind::Write,
            &[StateEffect::StoreCron],
        ),
        (
            CRON_DISABLE_TOOL_ID,
            OperationKind::Write,
            &[StateEffect::StoreCron],
        ),
        (
            CRON_RUN_TOOL_ID,
            OperationKind::Execute,
            &[StateEffect::StoreCron, StateEffect::StoreIdempotency],
        ),
        (
            CRON_TICK_TOOL_ID,
            OperationKind::Execute,
            &[
                StateEffect::StoreCron,
                StateEffect::StoreIdempotency,
                StateEffect::MatrixOutbox,
            ],
        ),
        (MATRIX_OUTBOX_STATUS_TOOL_ID, OperationKind::Read, &[]),
        (
            MATRIX_OUTBOX_ENQUEUE_TOOL_ID,
            OperationKind::Write,
            &[StateEffect::MatrixOutbox],
        ),
        (
            MATRIX_OUTBOX_DELIVER_TOOL_ID,
            OperationKind::Execute,
            &[StateEffect::MatrixOutbox],
        ),
        (STORE_STATUS_TOOL_ID, OperationKind::Read, &[]),
        (STORE_EXPORT_TOOL_ID, OperationKind::Read, &[]),
        (
            STORE_MIGRATE_TOOL_ID,
            OperationKind::Admin,
            &[StateEffect::StoreSchema],
        ),
        (REPO_STATUS_TOOL_ID, OperationKind::Read, &[]),
        (REPO_EXPORT_PROFILE_TOOL_ID, OperationKind::Read, &[]),
        (REPO_HOOKS_STATUS_TOOL_ID, OperationKind::Read, &[]),
        (
            REPO_INIT_TOOL_ID,
            OperationKind::Admin,
            &[StateEffect::RepoWorkspace],
        ),
        (
            REPO_IMPORT_PROFILE_TOOL_ID,
            OperationKind::Admin,
            &[StateEffect::RepoWorkspace],
        ),
        (
            REPO_INSTALL_HOOKS_TOOL_ID,
            OperationKind::Admin,
            &[StateEffect::RepoHooks],
        ),
        (SKILL_LIST_TOOL_ID, OperationKind::Read, &[]),
        (SKILL_INSPECT_TOOL_ID, OperationKind::Read, &[]),
        (SKILL_STATUS_TOOL_ID, OperationKind::Read, &[]),
        (SKILL_VERIFY_TOOL_ID, OperationKind::Read, &[]),
        (
            SKILL_LOCK_TOOL_ID,
            OperationKind::Admin,
            &[StateEffect::RepoWorkspace],
        ),
        (
            SKILL_TRUST_TOOL_ID,
            OperationKind::Approve,
            &[StateEffect::SkillTrust],
        ),
        (
            SKILL_REVOKE_TOOL_ID,
            OperationKind::Approve,
            &[StateEffect::SkillTrust],
        ),
        (PERMISSIONS_STATUS_TOOL_ID, OperationKind::Read, &[]),
        (
            PERMISSIONS_REQUEST_TOOL_ID,
            OperationKind::Approve,
            &[StateEffect::StorePermissionRequests],
        ),
        (
            PERMISSIONS_GRANT_TOOL_ID,
            OperationKind::Approve,
            &[
                StateEffect::StorePermissionGrants,
                StateEffect::StorePermissionRequests,
            ],
        ),
        (
            PERMISSIONS_REVOKE_TOOL_ID,
            OperationKind::Approve,
            &[StateEffect::StorePermissionGrants],
        ),
    ];

    assert_eq!(expected.len(), 51);
    for (id, operation_kind, effects) in expected {
        assert_action_metadata(&catalog, id, operation_kind, effects);
    }
}

fn assert_action_metadata(
    catalog: &ToolCatalog,
    id: &str,
    operation_kind: OperationKind,
    state_effects: &[StateEffect],
) {
    let id = CapabilityId::new(id).unwrap();
    let action = catalog.action(&id).unwrap();
    assert_eq!(action.operation_kind, operation_kind, "{id}");
    assert_eq!(
        action.state_effects,
        state_effects.iter().copied().collect::<BTreeSet<_>>(),
        "{id}"
    );
}
