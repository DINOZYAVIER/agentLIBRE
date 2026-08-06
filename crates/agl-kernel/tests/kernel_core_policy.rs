use std::collections::BTreeSet;

use agl_ids::{ExecutionScope, RunId, StepId};
use agl_kernel::{
    AuthorityClass, EffectiveToolSet, FunctionToolPolicy, SkillToolPolicy, ToolAccessMode,
    ToolExclusionReason, ToolGrant, ToolPolicyInput, ToolRuntime,
};
use agl_kernel::{
    EffectDeclaration, EffectId, ExtensionDescriptor, ExtensionId, ExtensionRegistration,
    ExtensionSource, ExtensionTrust, OperationKind, SensitiveInput, SkillId, ToolBinding,
    ToolDeclaration, ToolDispatchContext, ToolDispatchControl, ToolHandler, ToolHandlerFuture,
    ToolId, ToolInvocation, ToolResult,
};
use schemars::JsonSchema;
use serde_json::json;

#[derive(JsonSchema)]
#[allow(dead_code)]
struct PathArgs {
    path: String,
}

#[derive(JsonSchema)]
struct EmptyArgs {}

fn extension_id() -> ExtensionId {
    ExtensionId::new("example.policy").unwrap()
}

fn tool_id(local: &str) -> ToolId {
    ToolId::new(format!("example.policy:{local}")).unwrap()
}

fn read_tool() -> ToolDeclaration {
    ToolDeclaration::from_schema::<PathArgs>(tool_id("read"), "Read", OperationKind::Read).unwrap()
}

fn write_tool() -> ToolDeclaration {
    ToolDeclaration::from_schema::<PathArgs>(tool_id("write"), "Write", OperationKind::Write)
        .unwrap()
        .with_state_effects([EffectId::repo_files()])
}

fn admin_tool() -> ToolDeclaration {
    ToolDeclaration::from_schema::<EmptyArgs>(tool_id("admin"), "Admin", OperationKind::Admin)
        .unwrap()
        .with_state_effects([EffectId::store_schema()])
}

fn descriptor() -> ExtensionDescriptor {
    ExtensionDescriptor::new(
        extension_id(),
        "Policy fixture",
        "1.0.0",
        ExtensionSource::TestFixture,
        ExtensionTrust::TrustedRegistered,
    )
    .unwrap()
    .with_effect(EffectDeclaration::new(
        EffectId::repo_files(),
        AuthorityClass::RepositoryMutation.as_str(),
    ))
    .with_effect(EffectDeclaration::new(
        EffectId::store_schema(),
        AuthorityClass::DurableStoreMutation.as_str(),
    ))
    .with_tool(read_tool())
    .with_tool(write_tool())
    .with_tool(admin_tool())
}

fn resolve(baseline: impl IntoIterator<Item = ToolId>, mode: ToolAccessMode) -> EffectiveToolSet {
    ToolPolicyInput::new([descriptor()], baseline, mode)
        .resolve()
        .unwrap()
}

// Retained AGL-154 policy invariant. Mutation: admit one operation above a mode ceiling.
#[test]
fn access_modes_apply_the_complete_operation_ceiling() {
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
            let declaration =
                ToolDeclaration::from_schema::<EmptyArgs>(tool_id("matrix"), "Matrix", operation)
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

// Retained AGL-154 policy invariant. Mutation: make allow override deny or a parent ceiling.
#[test]
fn routing_deny_grant_and_parent_ceiling_precedence_is_closed() {
    let read = tool_id("read");
    let write = tool_id("write");

    let inherited = resolve([read.clone()], ToolAccessMode::ReadOnly);
    assert!(inherited.contains(&read));

    let empty_allow =
        ToolPolicyInput::new([descriptor()], [read.clone()], ToolAccessMode::ReadOnly)
            .with_function_policy(FunctionToolPolicy::default())
            .resolve()
            .unwrap();
    assert_eq!(
        empty_allow.exclusion(&read).unwrap().reason,
        ToolExclusionReason::FunctionAllowDenied
    );

    let denied = ToolPolicyInput::new([descriptor()], [], ToolAccessMode::Admin)
        .with_selected_skills([SkillToolPolicy::new(
            SkillId::new("editor").unwrap(),
            [write.clone()],
        )])
        .with_grants([ToolGrant::new(write.clone(), OperationKind::Admin)
            .with_state_effects([EffectId::repo_files()])])
        .with_function_policy(FunctionToolPolicy::new([write.clone()], [write.clone()]))
        .resolve()
        .unwrap();
    assert_eq!(
        denied.exclusion(&write).unwrap().reason,
        ToolExclusionReason::FunctionDenied
    );

    let parent_denied =
        ToolPolicyInput::new([descriptor()], [write.clone()], ToolAccessMode::Admin)
            .with_grants([ToolGrant::new(write.clone(), OperationKind::Admin)
                .with_state_effects([EffectId::repo_files()])])
            .with_authority_ceiling([read])
            .resolve()
            .unwrap();
    assert_eq!(
        parent_denied.exclusion(&write).unwrap().reason,
        ToolExclusionReason::ParentAuthorityDenied
    );
}

// KCT-EXT-002. Mutation: admit an undeclared or over-granted state Effect.
#[test]
fn operation_effect_and_grant_rules_are_explicit() {
    assert!(write_tool().validate().is_ok());
    assert!(
        ToolDeclaration::from_schema::<EmptyArgs>(
            tool_id("broken-write"),
            "Broken",
            OperationKind::Write,
        )
        .unwrap()
        .validate()
        .is_err()
    );
    assert!(
        read_tool()
            .with_state_effects([EffectId::repo_files()])
            .validate()
            .is_err()
    );

    let admin = tool_id("admin");
    let wrong_operation = ToolPolicyInput::new([descriptor()], [], ToolAccessMode::Admin)
        .with_grants([ToolGrant::new(admin.clone(), OperationKind::Write)])
        .resolve()
        .unwrap();
    assert_eq!(
        wrong_operation.exclusion(&admin).unwrap().reason,
        ToolExclusionReason::GrantOperationDenied
    );
    let wrong_effect = ToolPolicyInput::new([descriptor()], [], ToolAccessMode::Admin)
        .with_grants([ToolGrant::new(admin.clone(), OperationKind::Admin)
            .with_state_effects([EffectId::repo_files()])])
        .resolve()
        .unwrap();
    assert_eq!(
        wrong_effect.exclusion(&admin).unwrap().reason,
        ToolExclusionReason::GrantStateEffectDenied
    );
    let admitted = ToolPolicyInput::new([descriptor()], [], ToolAccessMode::Admin)
        .with_grants([ToolGrant::new(admin.clone(), OperationKind::Admin)
            .with_state_effects([EffectId::store_schema()])])
        .resolve()
        .unwrap();
    assert!(admitted.contains(&admin));
}

// Retained AGL-154 invariant. Mutation: let Request mutate anything except pending permissions.
#[test]
fn request_operation_accepts_only_the_pending_permission_effect() {
    let request_id = ToolId::new("example.permission:request").unwrap();
    let request = || {
        ToolDeclaration::from_schema::<EmptyArgs>(
            request_id.clone(),
            "Request permission",
            OperationKind::Request,
        )
        .unwrap()
    };
    assert!(request().validate().is_ok());
    assert!(
        request()
            .with_state_effects([EffectId::store_permission_requests()])
            .validate()
            .is_ok()
    );

    for effect in [
        EffectId::host_screen_capture(),
        EffectId::spawn_subagent(),
        EffectId::session_working_directory(),
        EffectId::spawn_process(),
        EffectId::control_process(),
        EffectId::host_process_execution(),
        EffectId::shell_login_startup(),
        EffectId::repo_files(),
        EffectId::repo_workspace(),
        EffectId::repo_hooks(),
        EffectId::store_memory_entries(),
        EffectId::store_memory_suggestions(),
        EffectId::store_notes(),
        EffectId::store_note_links(),
        EffectId::store_cron(),
        EffectId::store_schema(),
        EffectId::matrix_outbox(),
        EffectId::store_idempotency(),
        EffectId::store_permission_grants(),
        EffectId::skill_trust(),
    ] {
        assert!(
            request()
                .with_state_effects([effect.clone()])
                .validate()
                .is_err(),
            "Request admitted {}",
            effect.as_str()
        );
    }
    assert!(
        request()
            .with_conditional_state_effects([EffectId::repo_files()])
            .validate()
            .is_err()
    );

    let approve_id = ToolId::new("example.permission:approve").unwrap();
    let extension = ExtensionDescriptor::new(
        ExtensionId::new("example.permission").unwrap(),
        "Permission",
        "1.0.0",
        ExtensionSource::TestFixture,
        ExtensionTrust::TrustedRegistered,
    )
    .unwrap()
    .with_effect(EffectDeclaration::new(
        EffectId::store_permission_requests(),
        AuthorityClass::PermissionMutation.as_str(),
    ))
    .with_effect(EffectDeclaration::new(
        EffectId::store_permission_grants(),
        AuthorityClass::PermissionMutation.as_str(),
    ))
    .with_tool(request().with_state_effects([EffectId::store_permission_requests()]))
    .with_tool(
        ToolDeclaration::from_schema::<EmptyArgs>(
            approve_id.clone(),
            "Approve permission",
            OperationKind::Approve,
        )
        .unwrap()
        .with_state_effects([
            EffectId::store_permission_requests(),
            EffectId::store_permission_grants(),
        ]),
    );
    let policy = ToolPolicyInput::new(
        [extension],
        [request_id.clone(), approve_id.clone()],
        ToolAccessMode::ReadOnly,
    )
    .resolve()
    .unwrap();
    assert!(policy.contains(&request_id));
    assert_eq!(
        policy.exclusion(&approve_id).unwrap().reason,
        ToolExclusionReason::ToolModeDenied
    );
}

// Retained AGL-154 policy invariant. Mutation: trust an unknown or revoked Extension.
#[test]
fn every_non_executable_trust_state_is_excluded() {
    let read = tool_id("read");
    for trust in [
        ExtensionTrust::Unsupported,
        ExtensionTrust::Unknown,
        ExtensionTrust::Changed,
        ExtensionTrust::Revoked,
    ] {
        let set = ToolPolicyInput::new(
            [descriptor().with_trust(trust)],
            [read.clone()],
            ToolAccessMode::ReadOnly,
        )
        .resolve()
        .unwrap();
        assert_eq!(
            set.exclusion(&read).unwrap().reason.code(),
            "extension_untrusted"
        );
    }
}

// KCT-EXT-006. Mutation: make snapshot digest depend on discovery order or ignore trust.
#[test]
fn immutable_snapshot_is_order_stable_and_identity_sensitive() {
    let read = tool_id("read");
    let write = tool_id("write");
    let first = resolve([read.clone(), write.clone()], ToolAccessMode::Write);

    let mut reordered = descriptor();
    reordered.tools.reverse();
    let second = ToolPolicyInput::new(
        [reordered],
        [write.clone(), read.clone()],
        ToolAccessMode::Write,
    )
    .resolve()
    .unwrap();
    assert_eq!(first.policy_hash(), second.policy_hash());
    assert_eq!(
        first
            .tools()
            .map(|entry| entry.declaration().id.clone())
            .collect::<Vec<_>>(),
        [read.clone(), write.clone()]
    );

    let changed_trust = ToolPolicyInput::new(
        [descriptor().with_trust(ExtensionTrust::Revoked)],
        [read.clone(), write.clone()],
        ToolAccessMode::Write,
    )
    .resolve()
    .unwrap();
    assert_ne!(first.policy_hash(), changed_trust.policy_hash());

    let mut changed = descriptor();
    changed.tools[0].description = "Changed".to_string();
    let changed = ToolPolicyInput::new([changed], [read, write], ToolAccessMode::Write)
        .resolve()
        .unwrap();
    assert_ne!(first.policy_hash(), changed.policy_hash());
}

#[derive(Clone)]
struct EchoHandler;

impl ToolHandler for EchoHandler {
    fn dispatch(&self, context: ToolDispatchContext) -> ToolHandlerFuture<'_> {
        let path = context.invocation().arguments["path"].clone();
        Box::pin(std::future::ready(Ok(ToolResult::new(json!({
            "path": path
        })))))
    }
}

fn runtime_with_read(descriptor: ExtensionDescriptor) -> ToolRuntime {
    let mut runtime = ToolRuntime::new();
    runtime
        .register_extension(ExtensionRegistration::new(
            descriptor,
            ["read", "write", "admin"].map(|local| ToolBinding::new(tool_id(local), EchoHandler)),
        ))
        .unwrap();
    runtime
}

fn invocation(set: &EffectiveToolSet, path: &str) -> ToolInvocation {
    let entry = set.tool(&tool_id("read")).unwrap();
    ToolInvocation::new(
        ExecutionScope::builder(RunId::parse("run_01890f17-4a00-7000-8000-000000000001").unwrap())
            .step_id(StepId::parse("step_01890f17-4a00-7000-8000-000000000002").unwrap())
            .build()
            .unwrap(),
        tool_id("read"),
        entry.extension_id().clone(),
        entry.declaration_digest().clone(),
        set.policy_hash().clone(),
        json!({"path": path}),
    )
}

// KCT-EXT-006. Mutation: consult a changed live catalog for an already frozen Turn snapshot.
#[test]
fn registration_changes_affect_later_snapshots_not_the_frozen_snapshot() {
    let original = descriptor();
    let snapshot = ToolPolicyInput::new(
        [original.clone()],
        [tool_id("read")],
        ToolAccessMode::ReadOnly,
    )
    .resolve()
    .unwrap();
    let mut runtime = runtime_with_read(original);

    let unrelated = ExtensionDescriptor::new(
        ExtensionId::new("example.later").unwrap(),
        "Later",
        "1.0.0",
        ExtensionSource::TestFixture,
        ExtensionTrust::TrustedRegistered,
    )
    .unwrap();
    runtime
        .register_extension(ExtensionRegistration::new(unrelated, []))
        .unwrap();

    let outcome = runtime
        .dispatch(
            invocation(&snapshot, "README.md"),
            &snapshot,
            ToolDispatchControl::uncancellable(),
        )
        .expect("later registration must not invalidate a frozen Turn snapshot");
    assert_eq!(outcome.data, Some(json!({"path": "README.md"})));
}

// KCT-RUNTIME-005 and decision 16. Mutation: require memory retained by an earlier handler instance.
#[test]
fn fresh_handler_instances_reproduce_behavior_from_the_same_snapshot() {
    let descriptor = descriptor();
    let snapshot = ToolPolicyInput::new(
        [descriptor.clone()],
        [tool_id("read")],
        ToolAccessMode::ReadOnly,
    )
    .resolve()
    .unwrap();
    let first = runtime_with_read(descriptor.clone())
        .dispatch(
            invocation(&snapshot, "README.md"),
            &snapshot,
            ToolDispatchControl::uncancellable(),
        )
        .unwrap();
    let recreated = runtime_with_read(descriptor)
        .dispatch(
            invocation(&snapshot, "README.md"),
            &snapshot,
            ToolDispatchControl::uncancellable(),
        )
        .unwrap();
    assert_eq!(first, recreated);
}

// Retained AGL-154 invariant. Mutation: admit sensitive input without both grant and availability.
#[test]
fn sensitive_input_requires_matching_effect_grant_and_availability() {
    let id = tool_id("capture");
    let effect = EffectId::host_screen_capture();
    let screen =
        ToolDeclaration::from_schema::<EmptyArgs>(id.clone(), "Capture", OperationKind::Read)
            .unwrap()
            .with_state_effects([effect.clone()])
            .with_sensitive_inputs([SensitiveInput::ScreenCapture]);
    let extension = ExtensionDescriptor::new(
        extension_id(),
        "Screen",
        "1.0.0",
        ExtensionSource::TestFixture,
        ExtensionTrust::TrustedRegistered,
    )
    .unwrap()
    .with_effect(EffectDeclaration::new(
        effect.clone(),
        AuthorityClass::HostObservation.as_str(),
    ))
    .with_tool(screen);

    let denied = ToolPolicyInput::new([extension.clone()], [id.clone()], ToolAccessMode::ReadOnly)
        .resolve()
        .unwrap();
    assert_eq!(
        denied.exclusion(&id).unwrap().reason,
        ToolExclusionReason::GrantSensitiveInputDenied
    );

    let grant = ToolGrant::new(id.clone(), OperationKind::Read)
        .with_state_effects([effect])
        .with_sensitive_inputs([SensitiveInput::ScreenCapture]);
    let admitted =
        ToolPolicyInput::new([extension.clone()], [id.clone()], ToolAccessMode::ReadOnly)
            .with_grants([grant.clone()])
            .resolve()
            .unwrap();
    assert!(admitted.contains(&id));

    let unavailable = ToolPolicyInput::new([extension], [id.clone()], ToolAccessMode::ReadOnly)
        .with_grants([grant])
        .with_unavailable_capabilities([id.clone()])
        .resolve()
        .unwrap();
    assert_eq!(
        unavailable.exclusion(&id).unwrap().reason.code(),
        "extension_unavailable"
    );
}

// Keep the conditional-effect fixture complete and deterministic.
#[test]
fn policy_fixture_effect_ids_are_unique() {
    assert_eq!(
        descriptor()
            .effects
            .iter()
            .map(|effect| effect.id.clone())
            .collect::<BTreeSet<_>>()
            .len(),
        descriptor().effects.len()
    );
}
