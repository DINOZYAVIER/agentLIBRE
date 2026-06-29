use super::*;

#[test]
fn ids_accept_namespaced_values() {
    assert_eq!(
        HookId::new("task_spec.validate").unwrap().as_str(),
        "task_spec.validate"
    );
    assert_eq!(SkillId::new("task-spec").unwrap().as_str(), "task-spec");
}

#[test]
fn ids_reject_invalid_values() {
    assert!(HookId::new("").is_err());
    assert!(HookId::new("TaskSpec.Validate").is_err());
    assert!(HookId::new("a:b:c").is_err());
    assert!(HookId::new(":bad").is_err());
}

#[test]
fn id_deserialization_uses_validation() {
    let hook: HookId = serde_json::from_str("\"task_spec.validate\"").unwrap();

    assert_eq!(hook.as_str(), "task_spec.validate");
    assert!(serde_json::from_str::<HookId>("\"TaskSpec.Validate\"").is_err());
}

#[test]
fn declaration_rejects_duplicate_hooks() {
    let declaration = ToolProviderDeclaration::new(
        ToolProviderId::new("core-guards").unwrap(),
        "Core Guards",
        "1",
    )
    .unwrap()
    .with_hook(HookDeclaration {
        id: HookId::new("json.validate").unwrap(),
        event: HookEvent::ModelResponse,
        required: true,
    })
    .with_hook(HookDeclaration {
        id: HookId::new("json.validate").unwrap(),
        event: HookEvent::ArtifactWrite,
        required: true,
    });

    assert_eq!(
        declaration.validate().unwrap_err(),
        ToolProviderDeclarationError::DuplicateId {
            kind: "hook",
            id: "json.validate".to_string(),
        }
    );
}

#[test]
fn builtin_tools_declare_operation_kinds_and_state_effects() {
    let mut catalog = ToolCatalog::new();
    cron::register(&mut catalog).unwrap();
    fs::register(&mut catalog).unwrap();
    matrix::register(&mut catalog).unwrap();
    memory::register(&mut catalog).unwrap();
    notes::register(&mut catalog).unwrap();
    permissions::register(&mut catalog).unwrap();
    repo::register(&mut catalog).unwrap();
    store::register(&mut catalog).unwrap();

    assert_tool_metadata(
        FS_READ_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        FS_LIST_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        FS_SEARCH_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        FS_EDIT_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Write,
        &[ToolStateEffect::RepoFiles],
    );
    assert_tool_metadata(
        MEMORY_SEARCH_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        MEMORY_LIST_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        MEMORY_SUGGEST_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Write,
        &[ToolStateEffect::StoreMemorySuggestions],
    );
    assert_tool_metadata(
        MEMORY_ADD_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Write,
        &[ToolStateEffect::StoreMemoryEntries],
    );
    assert_tool_metadata(
        MEMORY_APPROVE_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Approve,
        &[
            ToolStateEffect::StoreMemorySuggestions,
            ToolStateEffect::StoreMemoryEntries,
        ],
    );
    assert_tool_metadata(
        MEMORY_REJECT_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Approve,
        &[ToolStateEffect::StoreMemorySuggestions],
    );
    assert_tool_metadata(
        NOTES_ADD_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Write,
        &[ToolStateEffect::StoreNotes],
    );
    assert_tool_metadata(
        NOTES_SEARCH_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        NOTES_SHOW_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        NOTES_UPDATE_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Write,
        &[ToolStateEffect::StoreNotes],
    );
    assert_tool_metadata(
        NOTES_LINK_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Write,
        &[ToolStateEffect::StoreNoteLinks],
    );
    assert_tool_metadata(
        NOTES_DELETE_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Write,
        &[ToolStateEffect::StoreNotes],
    );
    assert_tool_metadata(
        NOTES_REMEMBER_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Approve,
        &[
            ToolStateEffect::StoreMemoryEntries,
            ToolStateEffect::StoreNoteLinks,
        ],
    );
    assert_tool_metadata(
        CRON_LIST_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        CRON_SHOW_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        CRON_HISTORY_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        CRON_PREFLIGHT_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        CRON_ADD_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Write,
        &[ToolStateEffect::StoreCron],
    );
    assert_tool_metadata(
        CRON_UPDATE_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Write,
        &[ToolStateEffect::StoreCron],
    );
    assert_tool_metadata(
        CRON_DELETE_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Write,
        &[ToolStateEffect::StoreCron],
    );
    assert_tool_metadata(
        CRON_ENABLE_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Write,
        &[ToolStateEffect::StoreCron],
    );
    assert_tool_metadata(
        CRON_DISABLE_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Write,
        &[ToolStateEffect::StoreCron],
    );
    assert_tool_metadata(
        CRON_RUN_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Execute,
        &[ToolStateEffect::StoreCron],
    );
    assert_tool_metadata(
        CRON_TICK_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Execute,
        &[ToolStateEffect::StoreCron, ToolStateEffect::MatrixOutbox],
    );
    assert_tool_metadata(
        MATRIX_OUTBOX_STATUS_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        MATRIX_OUTBOX_ENQUEUE_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Write,
        &[ToolStateEffect::MatrixOutbox],
    );
    assert_tool_metadata(
        STORE_STATUS_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        STORE_EXPORT_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        REPO_STATUS_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        REPO_EXPORT_PROFILE_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        REPO_HOOKS_STATUS_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        REPO_INIT_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Admin,
        &[ToolStateEffect::RepoWorkspace],
    );
    assert_tool_metadata(
        REPO_INSTALL_HOOKS_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Admin,
        &[ToolStateEffect::RepoHooks],
    );
    assert_tool_metadata(
        PERMISSIONS_STATUS_TOOL_ID,
        &catalog,
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
    );
    assert_tool_metadata(
        PERMISSIONS_REQUEST_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Approve,
        &[ToolStateEffect::StorePermissionRequests],
    );
    assert_tool_metadata(
        PERMISSIONS_GRANT_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Approve,
        &[ToolStateEffect::StorePermissionGrants],
    );
    assert_tool_metadata(
        PERMISSIONS_REVOKE_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Approve,
        &[ToolStateEffect::StorePermissionGrants],
    );
}

fn assert_tool_metadata(
    tool_id: &str,
    catalog: &ToolCatalog,
    capability: ToolCapability,
    operation_kind: ToolOperationKind,
    state_effects: &[ToolStateEffect],
) {
    let tool = catalog.tool(&ToolId::new(tool_id).unwrap()).unwrap();
    assert_eq!(tool.capability, capability);
    assert_eq!(tool.operation_kind, operation_kind);
    assert_eq!(tool.state_effects, state_effects);
}
