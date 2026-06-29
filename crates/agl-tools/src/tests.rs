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
    fs::register(&mut catalog).unwrap();
    memory::register(&mut catalog).unwrap();
    notes::register(&mut catalog).unwrap();
    permissions::register(&mut catalog).unwrap();

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
        MEMORY_SUGGEST_TOOL_ID,
        &catalog,
        ToolCapability::Write,
        ToolOperationKind::Write,
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
