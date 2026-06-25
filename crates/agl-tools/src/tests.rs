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
