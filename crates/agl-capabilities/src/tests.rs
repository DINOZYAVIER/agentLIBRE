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
}

#[test]
fn mutating_delivery_requires_an_explicit_idempotency_contract() {
    assert_eq!(read_action().delivery, ActionDelivery::ReplaySafe);
    assert_eq!(write_action().delivery, ActionDelivery::AtMostOnce);
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
fn deny_wins_over_function_allow_skill_visibility_and_grant() {
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
fn read_only_mode_requires_explicit_visibility_for_mutating_actions() {
    let id = capability("permissions.request");
    let hidden = ActionDeclaration::from_schema::<EmptyArgs>(
        id.clone(),
        "Request an explicit permission grant",
        OperationKind::Approve,
    )
    .unwrap()
    .with_state_effects([StateEffect::StorePermissionRequests]);
    let hidden_provider =
        ProviderDeclaration::builtin(provider_id("permission-tools"), "Permissions", "1")
            .unwrap()
            .with_action(hidden);
    let hidden_set =
        CapabilityPolicyInput::new([hidden_provider], [id.clone()], ToolAccessMode::ReadOnly)
            .resolve()
            .unwrap();
    assert!(!hidden_set.contains(&id));

    let visible = ActionDeclaration::from_schema::<EmptyArgs>(
        id.clone(),
        "Request an explicit permission grant",
        OperationKind::Approve,
    )
    .unwrap()
    .with_state_effects([StateEffect::StorePermissionRequests])
    .with_visibility(ActionVisibility {
        visible_in_read_only: true,
    });
    let visible_provider =
        ProviderDeclaration::builtin(provider_id("permission-tools"), "Permissions", "1")
            .unwrap()
            .with_action(visible);
    for mode in [
        ToolAccessMode::ReadOnly,
        ToolAccessMode::Write,
        ToolAccessMode::Execute,
    ] {
        let visible_set =
            CapabilityPolicyInput::new([visible_provider.clone()], [id.clone()], mode)
                .resolve()
                .unwrap();
        assert!(visible_set.contains(&id));
    }
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
