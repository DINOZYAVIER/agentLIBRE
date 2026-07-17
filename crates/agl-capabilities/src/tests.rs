use std::collections::BTreeSet;

use agl_ids::{ExecutionScope, RunId, StepId};
use schemars::JsonSchema;
use serde_json::{Value, json};

use super::*;

const RUN_ID: &str = "run_01890f17-4a00-7000-8000-000000000001";

#[derive(JsonSchema)]
#[allow(dead_code)]
struct ReadArgs {
    path: String,
    limit: Option<u32>,
}

#[derive(JsonSchema)]
struct EmptyArgs {}

fn capability(value: &str) -> CapabilityId {
    CapabilityId::new(value).unwrap()
}

fn provider_id(value: &str) -> ProviderId {
    ProviderId::new(value).unwrap()
}

fn read_action() -> ActionDeclaration {
    ActionDeclaration::from_schema::<ReadArgs>(
        capability("fs.read"),
        "Read a file",
        OperationKind::Read,
    )
    .unwrap()
}

fn write_action() -> ActionDeclaration {
    ActionDeclaration::from_schema::<ReadArgs>(
        capability("fs.edit"),
        "Edit a file",
        OperationKind::Write,
    )
    .unwrap()
    .with_state_effects([StateEffect::RepoFiles])
}

fn admin_action() -> ActionDeclaration {
    ActionDeclaration::from_schema::<EmptyArgs>(
        capability("store.migrate"),
        "Migrate the store",
        OperationKind::Admin,
    )
    .unwrap()
    .with_state_effects([StateEffect::StoreSchema])
}

fn screen_action() -> ActionDeclaration {
    ActionDeclaration::from_schema::<EmptyArgs>(
        capability("screen.capture"),
        "Capture the screen",
        OperationKind::Read,
    )
    .unwrap()
    .with_state_effects([StateEffect::HostScreenCapture])
    .with_sensitive_inputs([SensitiveInput::ScreenCapture])
}

fn provider() -> ProviderDeclaration {
    ProviderDeclaration::builtin(provider_id("core"), "Core", "1")
        .unwrap()
        .with_action(read_action())
        .with_action(write_action())
        .with_action(admin_action())
}

fn resolve(
    baseline: impl IntoIterator<Item = CapabilityId>,
    mode: ToolAccessMode,
) -> EffectiveCapabilitySet {
    CapabilityPolicyInput::new([provider()], baseline, mode)
        .resolve()
        .unwrap()
}

fn invocation(
    set: &EffectiveCapabilitySet,
    id: &CapabilityId,
    arguments: Value,
) -> ActionInvocation {
    let effective = set.capability(id).unwrap();
    ActionInvocation::new(
        ExecutionScope::builder(RunId::parse(RUN_ID).unwrap())
            .build()
            .unwrap(),
        id.clone(),
        effective.provider_id().clone(),
        effective.declaration_digest().clone(),
        set.policy_hash().clone(),
        arguments,
    )
}

#[test]
fn identifiers_are_typed_strict_and_ordered() {
    let first = capability("fs.read");
    let second = capability("repo:status");
    assert!(first < second);
    assert_eq!(first.to_string(), "fs.read");
    assert!(CapabilityId::new("FS.read").is_err());
    assert!(ProviderId::new("two:namespace:parts").is_err());
    assert!(serde_json::from_str::<SkillId>(r#""bad id""#).is_err());

    let hook = HookId::new("core:repo_path.validate").unwrap();
    assert_eq!(hook.provider_namespace(), "core");
    assert_eq!(hook.local_name(), "repo_path.validate");
    assert_eq!(
        serde_json::to_string(&hook).unwrap(),
        r#""core:repo_path.validate""#
    );
    assert_eq!(
        serde_json::from_str::<HookId>(r#""core:repo_path.validate""#).unwrap(),
        hook
    );
    assert!(HookId::new("repo_path.validate").is_err());
    assert!(HookId::new("core:one:two").is_err());
}

#[test]
fn provider_hook_namespace_and_core_reservation_are_enforced() {
    let mismatched = ProviderDeclaration::new(
        provider_id("workspace"),
        "Workspace",
        "1",
        ProviderSource::ThirdPartyRegistered,
        ProviderTrust::TrustedRegistered,
    )
    .unwrap()
    .with_hook(HookDeclaration {
        id: HookId::new("other:validate").unwrap(),
        event: HookEvent::ArtifactWrite,
        required: true,
    });
    assert!(matches!(
        mismatched.validate(),
        Err(DeclarationError::HookProviderMismatch { .. })
    ));

    assert!(matches!(
        ProviderDeclaration::new(
            provider_id("core"),
            "Core shadow",
            "1",
            ProviderSource::ThirdPartyRegistered,
            ProviderTrust::TrustedRegistered,
        ),
        Err(DeclarationError::ReservedProviderNamespace { .. })
    ));

    let duplicate = ProviderDeclaration::builtin(provider_id("core"), "Core", "1")
        .unwrap()
        .with_hook(HookDeclaration {
            id: HookId::new("core:validate").unwrap(),
            event: HookEvent::ArtifactWrite,
            required: true,
        })
        .with_hook(HookDeclaration {
            id: HookId::new("core:validate").unwrap(),
            event: HookEvent::ModelResponse,
            required: false,
        });
    assert!(matches!(
        duplicate.validate(),
        Err(DeclarationError::DuplicateId { kind: "hook", .. })
    ));
}

#[test]
fn generated_schema_is_draft_2020_12_and_closes_objects() {
    let declaration = read_action();
    assert_eq!(
        declaration.input_schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(
        declaration.input_schema["additionalProperties"],
        Value::Bool(false)
    );
    let schema = declaration.compile_schema().unwrap();
    schema
        .validate(&json!({"path": "README.md", "limit": 3}))
        .unwrap();
    assert!(schema.validate(&json!({})).is_err());
    assert!(schema.validate(&json!({"path": 7})).is_err());
    assert!(
        schema
            .validate(&json!({"path": "README.md", "extra": true}))
            .is_err()
    );
}

#[test]
fn invalid_schema_is_rejected_at_declaration_creation() {
    let error = ActionDeclaration::new(
        capability("broken.schema"),
        "Broken",
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": 42
        }),
        OperationKind::Read,
    )
    .unwrap_err();
    assert!(matches!(error, DeclarationError::InvalidSchema(_)));
}

#[test]
fn incomplete_or_open_argument_schemas_are_rejected() {
    for schema in [
        json!({}),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"value": {"type": "string"}}
        }),
        json!({
            "$schema": "https://json-schema.org/draft/2019-09/schema",
            "type": "object",
            "additionalProperties": false
        }),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema"
        }),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "anyOf": [
                {"type": "object", "additionalProperties": false},
                {"type": "string", "minLength": 1}
            ]
        }),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "nested": {
                    "type": "object",
                    "properties": {"value": {"type": "string"}}
                }
            },
            "additionalProperties": false
        }),
    ] {
        assert!(matches!(
            ActionDeclaration::new(
                capability("broken.incomplete"),
                "Incomplete",
                schema,
                OperationKind::Read,
            ),
            Err(DeclarationError::IncompleteSchema(_))
        ));
    }
}

#[test]
fn operation_and_state_effect_invariants_are_enforced() {
    assert!(write_action().validate().is_ok());
    assert!(
        ActionDeclaration::from_schema::<EmptyArgs>(
            capability("broken.write"),
            "Broken write",
            OperationKind::Write,
        )
        .unwrap()
        .validate()
        .is_err()
    );
    assert!(
        read_action()
            .with_state_effects([StateEffect::RepoFiles])
            .validate()
            .is_err()
    );
    assert!(
        ActionDeclaration::from_schema::<EmptyArgs>(
            capability("permissions.request"),
            "Request an explicit permission grant",
            OperationKind::Request,
        )
        .unwrap()
        .validate()
        .is_ok()
    );
    assert!(
        ActionDeclaration::from_schema::<EmptyArgs>(
            capability("permissions.request"),
            "Request an explicit permission grant",
            OperationKind::Request,
        )
        .unwrap()
        .with_state_effects([StateEffect::StorePermissionRequests])
        .validate()
        .is_ok()
    );
}

#[test]
fn mutating_delivery_requires_explicit_idempotency() {
    assert_eq!(read_action().delivery, ActionDelivery::ReplaySafe);
    assert_eq!(write_action().delivery, ActionDelivery::AtMostOnce);
    assert_eq!(
        ActionDeclaration::from_schema::<EmptyArgs>(
            capability("permissions.request"),
            "Request an explicit permission grant",
            OperationKind::Request,
        )
        .unwrap()
        .delivery,
        ActionDelivery::AtMostOnce
    );
    let idempotent = write_action().with_run_step_idempotency();
    assert_eq!(idempotent.delivery, ActionDelivery::IdempotentRunStep);
    assert!(idempotent.validate().is_ok());
    assert!(
        read_action()
            .with_run_step_idempotency()
            .validate()
            .is_err()
    );
}

#[test]
fn canonical_observation_is_recursive_and_insertion_order_independent() {
    let left: Value =
        serde_json::from_str(r#"{"z":{"b":2,"a":1},"items":[{"d":4,"c":3}],"a":0}"#).unwrap();
    let right: Value =
        serde_json::from_str(r#"{"a":0,"items":[{"c":3,"d":4}],"z":{"a":1,"b":2}}"#).unwrap();
    let expected = r#"{"a":0,"items":[{"c":3,"d":4}],"z":{"a":1,"b":2}}"#;
    assert_eq!(ActionResult::new(left).render_observation(), expected);
    assert_eq!(ActionResult::new(right).render_observation(), expected);
}

#[test]
fn structured_result_round_trips_without_text_conversion() {
    let result = ActionResult::new(json!({"status": "ok", "count": 2}));
    let encoded = serde_json::to_value(&result).unwrap();
    assert_eq!(
        serde_json::from_value::<ActionResult>(encoded).unwrap(),
        result
    );
    assert!(serde_json::from_value::<ActionResult>(json!({"data": {}, "unknown": true})).is_err());
}

#[test]
fn invocation_exposes_stable_run_step_idempotency_key() {
    let set = resolve([capability("fs.edit")], ToolAccessMode::Write);
    let effective = set.capability(&capability("fs.edit")).unwrap();
    let run_id = RunId::parse(RUN_ID).unwrap();
    let step_id = StepId::generate();
    let invocation = ActionInvocation::new(
        ExecutionScope::builder(run_id.clone())
            .step_id(step_id.clone())
            .build()
            .unwrap(),
        capability("fs.edit"),
        effective.provider_id().clone(),
        effective.declaration_digest().clone(),
        set.policy_hash().clone(),
        json!({"path": "README.md"}),
    );
    assert_eq!(
        invocation.run_step_idempotency_key().as_deref(),
        Some(format!("{run_id}:{step_id}").as_str())
    );
}

#[test]
fn absent_function_policy_inherits_and_present_empty_allow_denies_all() {
    let ids = [capability("fs.read")];
    let inherited = resolve(ids.clone(), ToolAccessMode::ReadOnly);
    assert!(inherited.contains(&ids[0]));

    let denied = CapabilityPolicyInput::new([provider()], ids.clone(), ToolAccessMode::ReadOnly)
        .with_function_policy(FunctionToolPolicy::default())
        .resolve()
        .unwrap();
    assert!(!denied.contains(&ids[0]));
    assert_eq!(
        denied.exclusion(&ids[0]).unwrap().reason,
        CapabilityExclusionReason::FunctionAllowDenied
    );
}

#[test]
fn deny_wins_over_function_allow_skill_routing_and_grant() {
    let id = capability("fs.edit");
    let set = CapabilityPolicyInput::new([provider()], [], ToolAccessMode::Write)
        .with_selected_skills([SkillCapabilityPolicy::new(
            SkillId::new("editor").unwrap(),
            [id.clone()],
        )])
        .with_grants([CapabilityGrant::new(id.clone(), OperationKind::Write)])
        .with_function_policy(FunctionToolPolicy::new([id.clone()], [id.clone()]))
        .resolve()
        .unwrap();
    assert!(!set.contains(&id));
    assert_eq!(
        set.exclusion(&id).unwrap().reason,
        CapabilityExclusionReason::FunctionDenied
    );
}

#[test]
fn read_only_admits_permission_requests_but_not_permission_grants() {
    let request_id = capability("permissions.request");
    let grant_id = capability("permissions.grant");
    let provider =
        ProviderDeclaration::builtin(provider_id("permission-tools"), "Permissions", "1")
            .unwrap()
            .with_action(
                ActionDeclaration::from_schema::<EmptyArgs>(
                    request_id.clone(),
                    "Create one pending permission request",
                    OperationKind::Request,
                )
                .unwrap()
                .with_state_effects([StateEffect::StorePermissionRequests]),
            )
            .with_action(
                ActionDeclaration::from_schema::<EmptyArgs>(
                    grant_id.clone(),
                    "Approve one pending permission request",
                    OperationKind::Approve,
                )
                .unwrap()
                .with_state_effects([
                    StateEffect::StorePermissionRequests,
                    StateEffect::StorePermissionGrants,
                ]),
            );
    let effective = CapabilityPolicyInput::new(
        [provider],
        [request_id.clone(), grant_id.clone()],
        ToolAccessMode::ReadOnly,
    )
    .resolve()
    .unwrap();

    assert!(effective.contains(&request_id));
    assert!(!effective.contains(&grant_id));
    assert_eq!(
        effective.exclusion(&grant_id).unwrap().reason,
        CapabilityExclusionReason::ToolModeDenied
    );
}

#[test]
fn requestable_skill_policy_changes_the_snapshot_and_preserves_exclusions() {
    let requestable = capability("fs.edit");
    let skill_id = SkillId::new("request-editor").unwrap();
    let without_requestable = CapabilityPolicyInput::new([provider()], [], ToolAccessMode::Write)
        .with_selected_skills([SkillCapabilityPolicy::new(skill_id.clone(), [])])
        .resolve()
        .unwrap();
    let with_requestable = CapabilityPolicyInput::new([provider()], [], ToolAccessMode::Write)
        .with_selected_skills([
            SkillCapabilityPolicy::new(skill_id, []).with_requestable([requestable.clone()])
        ])
        .resolve()
        .unwrap();

    assert_ne!(
        without_requestable.policy_hash(),
        with_requestable.policy_hash()
    );
    assert!(!with_requestable.contains(&requestable));
    assert_eq!(
        with_requestable.exclusion(&requestable).unwrap().reason,
        CapabilityExclusionReason::NotRouted
    );
    assert!(
        with_requestable
            .exclusion(&requestable)
            .unwrap()
            .reason
            .is_grant_resolvable()
    );
}

#[test]
fn tool_mode_is_an_operation_ceiling_even_for_grants() {
    let read = capability("fs.read");
    let write = capability("fs.edit");
    let set = CapabilityPolicyInput::new(
        [provider()],
        [read.clone(), write.clone()],
        ToolAccessMode::ReadOnly,
    )
    .with_grants([CapabilityGrant::new(write.clone(), OperationKind::Admin)])
    .resolve()
    .unwrap();
    assert!(set.contains(&read));
    assert!(!set.contains(&write));
    assert_eq!(
        set.exclusion(&write).unwrap().reason,
        CapabilityExclusionReason::ToolModeDenied
    );
}

#[test]
fn parent_authority_ceiling_is_an_immutable_subset_boundary() {
    let read = capability("fs.read");
    let write = capability("fs.edit");
    let set = CapabilityPolicyInput::new(
        [provider()],
        [read.clone(), write.clone()],
        ToolAccessMode::Admin,
    )
    .with_selected_skills([SkillCapabilityPolicy::new(
        SkillId::new("editor").unwrap(),
        [write.clone()],
    )])
    .with_grants([CapabilityGrant::new(write.clone(), OperationKind::Admin)])
    .with_authority_ceiling([read.clone()])
    .resolve()
    .unwrap();

    assert!(set.contains(&read));
    assert!(!set.contains(&write));
    assert_eq!(
        set.exclusion(&write).unwrap().reason,
        CapabilityExclusionReason::ParentAuthorityDenied
    );
    assert!(
        set.capabilities()
            .all(|entry| entry.declaration().id == read)
    );
}

#[test]
fn delegation_declaration_has_strict_bounded_arguments() {
    let provider = delegation_provider();
    let declaration = provider
        .actions
        .iter()
        .find(|action| action.id.as_str() == AGENT_DELEGATE_CAPABILITY_ID)
        .unwrap();
    let schema = declaration.compile_schema().unwrap();

    schema
        .validate(&json!({"subagent_id": "reviewer", "task": "Review this patch"}))
        .unwrap();
    assert!(
        schema
            .validate(&json!({"subagent_id": "reviewer", "task": "x", "extra": true}))
            .is_err()
    );
    assert!(
        DelegateActionArgs {
            subagent_id: " reviewer".to_string(),
            task: "Review".to_string(),
        }
        .validate()
        .is_err()
    );
    assert!(
        DelegateActionArgs {
            subagent_id: "reviewer".to_string(),
            task: " ".to_string(),
        }
        .validate()
        .is_err()
    );
    assert!(
        DelegateActionArgs {
            subagent_id: "reviewer".to_string(),
            task: "x".repeat(MAX_DELEGATED_TASK_BYTES + 1),
        }
        .validate()
        .is_err()
    );
}

#[test]
fn tool_access_mode_uses_the_complete_operation_matrix() {
    let operations = [
        OperationKind::Read,
        OperationKind::Request,
        OperationKind::Write,
        OperationKind::Execute,
        OperationKind::Approve,
        OperationKind::Admin,
    ];
    let cases = [
        (
            ToolAccessMode::ReadOnly,
            [true, true, false, false, false, false],
        ),
        (
            ToolAccessMode::Write,
            [true, true, true, false, false, false],
        ),
        (
            ToolAccessMode::Execute,
            [true, true, true, true, false, false],
        ),
        (
            ToolAccessMode::Approve,
            [true, true, true, true, true, false],
        ),
        (ToolAccessMode::Admin, [true, true, true, true, true, true]),
    ];

    for (mode, expected) in cases {
        for (index, operation) in operations.into_iter().enumerate() {
            let declaration = ActionDeclaration::from_schema::<EmptyArgs>(
                capability("matrix.operation"),
                "Operation matrix fixture",
                operation,
            )
            .unwrap();
            assert_eq!(
                mode.permits(&declaration),
                expected[index],
                "mode={} operation={}",
                mode.as_str(),
                operation.as_str()
            );
        }
    }
}

#[test]
fn request_operations_reject_every_non_request_store_effect() {
    let invalid_effects = [
        StateEffect::HostScreenCapture,
        StateEffect::SpawnSubagent,
        StateEffect::SessionWorkingDirectory,
        StateEffect::SpawnProcess,
        StateEffect::ControlProcess,
        StateEffect::HostProcessExecution,
        StateEffect::ShellLoginStartup,
        StateEffect::RepoFiles,
        StateEffect::RepoWorkspace,
        StateEffect::RepoHooks,
        StateEffect::StoreMemoryEntries,
        StateEffect::StoreMemorySuggestions,
        StateEffect::StoreNotes,
        StateEffect::StoreNoteLinks,
        StateEffect::StoreCron,
        StateEffect::StoreSchema,
        StateEffect::MatrixOutbox,
        StateEffect::StoreIdempotency,
        StateEffect::StorePermissionGrants,
        StateEffect::SkillTrust,
    ];
    for effect in invalid_effects {
        let declaration = ActionDeclaration::from_schema::<EmptyArgs>(
            capability("permissions.request"),
            "Request an explicit permission grant",
            OperationKind::Request,
        )
        .unwrap()
        .with_state_effects([effect]);
        assert!(
            declaration.validate().is_err(),
            "effect={}",
            effect.as_str()
        );
    }

    let conditional = ActionDeclaration::from_schema::<EmptyArgs>(
        capability("permissions.request"),
        "Request an explicit permission grant",
        OperationKind::Request,
    )
    .unwrap()
    .with_conditional_state_effects([StateEffect::RepoFiles]);
    assert!(conditional.validate().is_err());
}

#[test]
fn removed_visibility_field_is_rejected_during_deserialization() {
    let mut value = serde_json::to_value(read_action()).unwrap();
    value.as_object_mut().unwrap().insert(
        "visibility".to_string(),
        json!({["visible", "in", "read", "only"].join("_"): true}),
    );
    assert!(serde_json::from_value::<ActionDeclaration>(value).is_err());
}

#[test]
fn grants_enforce_operation_and_state_effect_limits() {
    let id = capability("store.migrate");
    let operation_denied = CapabilityPolicyInput::new([provider()], [], ToolAccessMode::Admin)
        .with_grants([CapabilityGrant::new(id.clone(), OperationKind::Write)])
        .resolve()
        .unwrap();
    assert_eq!(
        operation_denied.exclusion(&id).unwrap().reason,
        CapabilityExclusionReason::GrantOperationDenied
    );

    let effect_denied = CapabilityPolicyInput::new([provider()], [], ToolAccessMode::Admin)
        .with_grants([CapabilityGrant::new(id.clone(), OperationKind::Admin)
            .with_state_effects([StateEffect::RepoFiles])])
        .resolve()
        .unwrap();
    assert_eq!(
        effect_denied.exclusion(&id).unwrap().reason,
        CapabilityExclusionReason::GrantStateEffectDenied
    );

    let admitted = CapabilityPolicyInput::new([provider()], [], ToolAccessMode::Admin)
        .with_grants([CapabilityGrant::new(id.clone(), OperationKind::Admin)
            .with_state_effects([StateEffect::StoreSchema])])
        .resolve()
        .unwrap();
    assert!(admitted.contains(&id));

    let operation = CapabilityGrant::new(id.clone(), OperationKind::Write);
    let effect = CapabilityGrant::new(id.clone(), OperationKind::Admin)
        .with_state_effects([StateEffect::RepoFiles]);
    let first = CapabilityPolicyInput::new([provider()], [], ToolAccessMode::Admin)
        .with_grants([operation.clone(), effect.clone()])
        .resolve()
        .unwrap();
    let reversed = CapabilityPolicyInput::new([provider()], [], ToolAccessMode::Admin)
        .with_grants([effect, operation])
        .resolve()
        .unwrap();
    assert_eq!(first.exclusion(&id), reversed.exclusion(&id));
    assert_eq!(first.policy_hash(), reversed.policy_hash());
}

#[test]
fn untrusted_providers_are_excluded() {
    let id = capability("fs.read");
    for trust in [
        ProviderTrust::Unsupported,
        ProviderTrust::Unknown,
        ProviderTrust::Changed,
        ProviderTrust::Revoked,
    ] {
        let set = CapabilityPolicyInput::new(
            [provider().with_trust(trust)],
            [id.clone()],
            ToolAccessMode::ReadOnly,
        )
        .resolve()
        .unwrap();
        assert_eq!(
            set.exclusion(&id).unwrap().reason,
            CapabilityExclusionReason::ProviderUntrusted
        );
    }
}

#[test]
fn policy_hash_is_order_stable_and_changes_with_trust_or_declaration() {
    let read = capability("fs.read");
    let write = capability("fs.edit");
    let first = CapabilityPolicyInput::new(
        [provider()],
        [read.clone(), write.clone()],
        ToolAccessMode::Write,
    )
    .resolve()
    .unwrap();

    let mut reordered = provider();
    reordered.actions.reverse();
    let second = CapabilityPolicyInput::new(
        [reordered],
        [write.clone(), read.clone()],
        ToolAccessMode::Write,
    )
    .resolve()
    .unwrap();
    assert_eq!(first.policy_hash(), second.policy_hash());

    let changed_trust = CapabilityPolicyInput::new(
        [provider().with_trust(ProviderTrust::Revoked)],
        [read.clone(), write.clone()],
        ToolAccessMode::Write,
    )
    .resolve()
    .unwrap();
    assert_ne!(first.policy_hash(), changed_trust.policy_hash());

    let mut changed_declaration = provider();
    changed_declaration.actions[0].description = "Changed".to_owned();
    let changed_declaration =
        CapabilityPolicyInput::new([changed_declaration], [read, write], ToolAccessMode::Write)
            .resolve()
            .unwrap();
    assert_ne!(first.policy_hash(), changed_declaration.policy_hash());
}

#[test]
fn authorization_rechecks_snapshot_provider_declaration_and_arguments() {
    let id = capability("fs.read");
    let set = resolve([id.clone()], ToolAccessMode::ReadOnly);
    let current = provider();
    let valid = invocation(&set, &id, json!({"path": "README.md", "limit": null}));
    assert!(
        set.authorize(&valid, std::slice::from_ref(&current))
            .is_ok()
    );

    let mut stale_policy = valid.clone();
    stale_policy.policy_hash = PolicyHash::parse(&format!("sha256:{}", "0".repeat(64))).unwrap();
    assert_eq!(
        set.authorize(&stale_policy, std::slice::from_ref(&current))
            .unwrap_err()
            .code,
        DispatchDenialCode::StalePolicy
    );

    let mut stale_declaration = valid.clone();
    stale_declaration.declaration_digest =
        DeclarationDigest::parse(&format!("sha256:{}", "0".repeat(64))).unwrap();
    assert_eq!(
        set.authorize(&stale_declaration, std::slice::from_ref(&current))
            .unwrap_err()
            .code,
        DispatchDenialCode::StaleDeclaration
    );

    let invalid = invocation(&set, &id, json!({"path": 12}));
    assert_eq!(
        set.authorize(&invalid, std::slice::from_ref(&current))
            .unwrap_err()
            .code,
        DispatchDenialCode::InvalidArguments
    );

    assert_eq!(
        set.authorize(
            &valid,
            &[current.clone().with_trust(ProviderTrust::Revoked)],
        )
        .unwrap_err()
        .code,
        DispatchDenialCode::ProviderUntrusted
    );
}

#[test]
fn policy_and_hook_dtos_reject_unknown_fields() {
    assert!(
        serde_json::from_value::<FunctionToolPolicy>(json!({
            "allow": [],
            "deny": [],
            "extra": true
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<HookBatchRequest>(json!({
            "event": "turn_finish",
            "hooks": [],
            "payload": {},
            "extra": true
        }))
        .is_err()
    );
}

#[test]
fn declaration_digest_is_stable_for_recursive_object_order() {
    let mut first = read_action();
    first.input_schema = serde_json::from_str(
        r#"{"type":"object","properties":{"z":{"type":"string"},"a":{"type":"integer"}},"additionalProperties":false}"#,
    )
    .unwrap();
    let mut second = first.clone();
    second.input_schema = serde_json::from_str(
        r#"{"additionalProperties":false,"properties":{"a":{"type":"integer"},"z":{"type":"string"}},"type":"object"}"#,
    )
    .unwrap();
    assert_eq!(first.digest(), second.digest());
}

#[test]
fn capability_collections_are_exposed_in_stable_id_order() {
    let set = resolve(
        [capability("fs.edit"), capability("fs.read")],
        ToolAccessMode::Write,
    );
    let ids = set
        .capabilities()
        .map(|entry| entry.declaration().id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["fs.edit", "fs.read"]);
    assert_eq!(BTreeSet::from_iter(ids).len(), 2);
}

#[test]
fn sensitive_input_requires_matching_grant_and_availability() {
    let id = capability("screen.capture");
    let provider = provider().with_action(screen_action());
    let denied =
        CapabilityPolicyInput::new([provider.clone()], [id.clone()], ToolAccessMode::ReadOnly)
            .resolve()
            .unwrap();
    assert_eq!(
        denied.exclusion(&id).unwrap().reason,
        CapabilityExclusionReason::GrantSensitiveInputDenied
    );

    let missing_effect = CapabilityGrant::new(id.clone(), OperationKind::Read)
        .with_sensitive_inputs([SensitiveInput::ScreenCapture]);
    let denied =
        CapabilityPolicyInput::new([provider.clone()], [id.clone()], ToolAccessMode::ReadOnly)
            .with_grants([missing_effect])
            .resolve()
            .unwrap();
    assert_eq!(
        denied.exclusion(&id).unwrap().reason,
        CapabilityExclusionReason::GrantStateEffectDenied
    );

    let grant = CapabilityGrant::new(id.clone(), OperationKind::Read)
        .with_state_effects([StateEffect::HostScreenCapture])
        .with_sensitive_inputs([SensitiveInput::ScreenCapture]);
    let admitted =
        CapabilityPolicyInput::new([provider.clone()], [id.clone()], ToolAccessMode::ReadOnly)
            .with_grants([grant.clone()])
            .resolve()
            .unwrap();
    assert!(admitted.contains(&id));

    let unavailable =
        CapabilityPolicyInput::new([provider], [id.clone()], ToolAccessMode::ReadOnly)
            .with_grants([grant])
            .with_unavailable_capabilities([id.clone()])
            .resolve()
            .unwrap();
    assert_eq!(
        unavailable.exclusion(&id).unwrap().reason,
        CapabilityExclusionReason::ProviderUnavailable
    );
}
