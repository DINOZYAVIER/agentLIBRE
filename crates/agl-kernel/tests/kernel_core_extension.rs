#[path = "core/support/extension.rs"]
mod extension_support;
#[path = "core/support/mod.rs"]
mod support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agl_ids::{ExecutionScope, RunId};
use agl_kernel::{
    DispatchDenialCode, ToolAccessMode, ToolCatalogError, ToolDispatchError, ToolPolicyInput,
    ToolRuntime,
};
use agl_kernel::{
    EffectId, ExtensionRegistration, OperationKind, ToolBinding, ToolDeclaration,
    ToolDispatchContext, ToolDispatchControl, ToolHandler, ToolHandlerFuture, ToolInvocation,
    ToolResult,
};
use extension_support::{ProductionHookHarness, ProductionRegistrationHarness};
use serde_json::{Value, json};
use support::{extension_id, extension_with_tool, hook_declaration, tool_declaration, tool_id};

#[derive(Clone)]
struct CountingHandler {
    count: Arc<AtomicUsize>,
    result: ToolResult,
}

impl ToolHandler for CountingHandler {
    fn dispatch(&self, _context: ToolDispatchContext) -> ToolHandlerFuture<'_> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(Ok(self.result.clone())))
    }
}

fn invocation(
    descriptor: &agl_kernel::ExtensionDescriptor,
    effective: &agl_kernel::EffectiveToolSet,
    arguments: Value,
) -> ToolInvocation {
    let id = descriptor.tools[0].id.clone();
    ToolInvocation::new(
        ExecutionScope::builder(RunId::generate()).build().unwrap(),
        id,
        descriptor.id.clone(),
        descriptor.tools[0].digest(),
        effective.policy_hash().clone(),
        arguments,
    )
}

// KCT-EXT-001. Mutation: publish a descriptor or handler before full validation.
#[test]
fn failed_atomic_registration_leaves_catalog_and_handlers_unchanged() {
    let primary = extension_with_tool("example.primary", "example.primary:echo");
    let primary_count = Arc::new(AtomicUsize::new(0));
    let mut runtime = ToolRuntime::new();
    runtime
        .register_extension(ExtensionRegistration::new(
            primary.clone(),
            [ToolBinding::new(
                tool_id("example.primary:echo"),
                CountingHandler {
                    count: primary_count,
                    result: ToolResult::new(json!({})),
                },
            )],
        ))
        .unwrap();
    let catalog_before = runtime.catalog().extensions().to_vec();
    let handlers_before = runtime.handler_ids().cloned().collect::<Vec<_>>();

    let missing = extension_with_tool("example.missing", "example.missing:echo");
    assert!(matches!(
        runtime.register_extension(ExtensionRegistration::new(missing, [])),
        Err(ToolCatalogError::MissingHandler { .. })
    ));
    assert_eq!(runtime.catalog().extensions(), catalog_before);
    assert_eq!(
        runtime.handler_ids().cloned().collect::<Vec<_>>(),
        handlers_before
    );

    let declared = extension_with_tool("example.extra", "example.extra:declared");
    assert!(matches!(
        runtime.register_extension(ExtensionRegistration::new(
            declared,
            [ToolBinding::new(
                tool_id("example.extra:undeclared"),
                CountingHandler {
                    count: Arc::new(AtomicUsize::new(0)),
                    result: ToolResult::new(json!({})),
                },
            )],
        )),
        Err(ToolCatalogError::UndeclaredHandler { .. })
    ));
    assert_eq!(runtime.catalog().extensions(), catalog_before);
    assert_eq!(
        runtime.handler_ids().cloned().collect::<Vec<_>>(),
        handlers_before
    );
}

// KCT-EXT-001. Mutation: validate Tool bindings but publish incomplete Hook bindings.
#[test]
fn complete_registration_is_atomic_for_tool_and_hook_bindings() {
    struct BindingCase<'a> {
        declared_tools: &'a [&'a str],
        bound_tools: &'a [&'a str],
        declared_hooks: &'a [&'a str],
        bound_hooks: &'a [&'a str],
    }

    const VALID_TOOLS: &[&str] = &["example.complete:run"];
    const VALID_HOOKS: &[&str] = &["example.complete:validate"];
    let invalid_cases = [
        BindingCase {
            declared_tools: VALID_TOOLS,
            bound_tools: &[],
            declared_hooks: VALID_HOOKS,
            bound_hooks: VALID_HOOKS,
        },
        BindingCase {
            declared_tools: VALID_TOOLS,
            bound_tools: &["example.complete:run", "example.complete:extra"],
            declared_hooks: VALID_HOOKS,
            bound_hooks: VALID_HOOKS,
        },
        BindingCase {
            declared_tools: VALID_TOOLS,
            bound_tools: &["example.complete:run", "example.complete:run"],
            declared_hooks: VALID_HOOKS,
            bound_hooks: VALID_HOOKS,
        },
        BindingCase {
            declared_tools: VALID_TOOLS,
            bound_tools: VALID_TOOLS,
            declared_hooks: VALID_HOOKS,
            bound_hooks: &[],
        },
        BindingCase {
            declared_tools: VALID_TOOLS,
            bound_tools: VALID_TOOLS,
            declared_hooks: VALID_HOOKS,
            bound_hooks: &["example.complete:validate", "example.complete:extra"],
        },
        BindingCase {
            declared_tools: VALID_TOOLS,
            bound_tools: VALID_TOOLS,
            declared_hooks: VALID_HOOKS,
            bound_hooks: &["example.complete:validate", "example.complete:validate"],
        },
    ];

    for case in invalid_cases {
        let mut runtime = ProductionRegistrationHarness::new();
        runtime
            .register(
                "example.baseline",
                &["example.baseline:run"],
                &["example.baseline:run"],
                &["example.baseline:validate"],
                &["example.baseline:validate"],
            )
            .expect("valid baseline registration is admitted");
        let before = runtime.snapshot_bytes();
        assert!(
            runtime
                .register(
                    "example.complete",
                    case.declared_tools,
                    case.bound_tools,
                    case.declared_hooks,
                    case.bound_hooks,
                )
                .is_err(),
            "incomplete or duplicate registration was admitted"
        );
        assert_eq!(
            runtime.snapshot_bytes(),
            before,
            "partial state was published"
        );
    }
}

// KCT-EXT-002. Mutation: infer a standard Effect from Tool metadata.
#[test]
fn extension_never_infers_an_effect_declaration() {
    let tool = ToolDeclaration::new(
        tool_id("example.workspace:write"),
        "Write",
        support::empty_schema(),
        OperationKind::Write,
    )
    .unwrap()
    .with_state_effects([EffectId::repo_files()]);
    let descriptor = agl_kernel::ExtensionDescriptor::new(
        extension_id("example.workspace"),
        "Workspace",
        "1.0.0",
        agl_kernel::ExtensionSource::TestFixture,
        agl_kernel::ExtensionTrust::TrustedRegistered,
    )
    .unwrap()
    .with_tool(tool);

    assert!(
        descriptor.effects.is_empty(),
        "Extension builder inferred Effect declarations: {:?}",
        descriptor.effects
    );
    assert!(
        descriptor.validate().is_err(),
        "undeclared Tool Effect was admitted"
    );
}

// KCT-EXT-002. Mutation: accept an unresolved Effect or open AuthorityClass value.
#[test]
fn every_tool_effect_reference_resolves_to_one_closed_authority_class() {
    let effect = EffectId::repo_files();
    let mut unresolved = extension_with_tool("example.effects", "example.effects:read");
    unresolved.tools[0].conditional_state_effects = BTreeSet::from([effect.clone()]);
    unresolved.effects.clear();
    assert!(unresolved.validate().is_err(), "undeclared Effect resolved");

    let unknown_authority = agl_kernel::ExtensionDescriptor::new(
        extension_id("example.unknown"),
        "Unknown authority",
        "1.0.0",
        agl_kernel::ExtensionSource::TestFixture,
        agl_kernel::ExtensionTrust::TrustedRegistered,
    )
    .unwrap()
    .with_effect(agl_kernel::EffectDeclaration::new(
        effect.clone(),
        "invented_authority",
    ));
    assert!(
        matches!(
            agl_kernel::ToolCatalog::new().register(unknown_authority),
            Err(ToolCatalogError::UnknownAuthorityClass { .. })
        ),
        "open AuthorityClass value was admitted"
    );

    let duplicate = agl_kernel::ExtensionDescriptor::new(
        extension_id("example.duplicate"),
        "Duplicate effect",
        "1.0.0",
        agl_kernel::ExtensionSource::TestFixture,
        agl_kernel::ExtensionTrust::TrustedRegistered,
    )
    .unwrap()
    .with_effect(agl_kernel::EffectDeclaration::new(
        effect.clone(),
        agl_kernel::AuthorityClass::RepositoryMutation.as_str(),
    ))
    .with_effect(agl_kernel::EffectDeclaration::new(
        effect,
        agl_kernel::AuthorityClass::RepositoryMutation.as_str(),
    ));
    assert!(
        duplicate.validate().is_err(),
        "duplicate EffectId was admitted"
    );
}

// KCT-EXT-003. Mutation: restore requiredness in reusable HookDeclaration.
#[test]
fn hook_declaration_has_no_requiredness_or_resource_effect_fields() {
    let value = serde_json::to_value(hook_declaration(
        "example.guard:validate",
        agl_kernel::HookEvent::ArtifactWrite,
    ))
    .unwrap();
    let object = value.as_object().unwrap();
    for forbidden in [
        "required",
        "optional",
        "effects",
        "state_effects",
        "conditional_state_effects",
    ] {
        assert!(
            !object.contains_key(forbidden),
            "HookDeclaration retains forbidden field {forbidden}: {value}"
        );
    }
}

// KCT-EXT-004. Mutation: call the handler before argument validation.
#[test]
fn invalid_tool_input_is_rejected_before_handler_execution() {
    let mut descriptor = extension_with_tool("example.input", "example.input:echo");
    descriptor.tools[0] = ToolDeclaration::new(
        tool_id("example.input:echo"),
        "Echo",
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"],
            "additionalProperties": false
        }),
        OperationKind::Read,
    )
    .unwrap();
    let effective = ToolPolicyInput::new(
        [descriptor.clone()],
        [tool_id("example.input:echo")],
        ToolAccessMode::ReadOnly,
    )
    .resolve()
    .unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let mut runtime = ToolRuntime::new();
    runtime
        .register_extension(ExtensionRegistration::new(
            descriptor.clone(),
            [ToolBinding::new(
                tool_id("example.input:echo"),
                CountingHandler {
                    count: count.clone(),
                    result: ToolResult::new(json!({})),
                },
            )],
        ))
        .unwrap();

    let error = runtime
        .dispatch(
            invocation(&descriptor, &effective, json!({"value": 7})),
            &effective,
            ToolDispatchControl::uncancellable(),
        )
        .unwrap_err();
    assert_eq!(
        error.denial().map(|denial| denial.code),
        Some(DispatchDenialCode::InvalidArguments)
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

// KCT-EXT-004. Mutation: accept handler output before checking declared output schema.
#[test]
fn invalid_tool_output_is_rejected_after_one_handler_execution() {
    let mut descriptor = extension_with_tool("example.output", "example.output:echo");
    descriptor.tools[0].output_schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {"ok": {"type": "boolean"}},
        "required": ["ok"],
        "additionalProperties": false
    });
    descriptor.validate().unwrap();
    let effective = ToolPolicyInput::new(
        [descriptor.clone()],
        [tool_id("example.output:echo")],
        ToolAccessMode::ReadOnly,
    )
    .resolve()
    .unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let mut runtime = ToolRuntime::new();
    runtime
        .register_extension(ExtensionRegistration::new(
            descriptor.clone(),
            [ToolBinding::new(
                tool_id("example.output:echo"),
                CountingHandler {
                    count: count.clone(),
                    result: ToolResult::new(json!({"unexpected": true})),
                },
            )],
        ))
        .unwrap();

    assert!(matches!(
        runtime.dispatch(
            invocation(&descriptor, &effective, json!({})),
            &effective,
            ToolDispatchControl::uncancellable(),
        ),
        Err(ToolDispatchError::InvalidResult { .. })
    ));
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

// KCT-EXT-004. Mutation: invoke a Hook before input validation or accept invalid output.
#[test]
fn hook_input_and_output_schemas_wrap_exactly_one_handler_call() {
    let input_schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"],
        "additionalProperties": false
    });
    let output_schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {"status": {"const": "pass"}},
        "required": ["status"],
        "additionalProperties": false
    });

    let invalid_input_calls = Arc::new(AtomicUsize::new(0));
    let invalid_input = ProductionHookHarness::new(
        input_schema.clone(),
        output_schema.clone(),
        json!({"status": "pass"}),
        invalid_input_calls.clone(),
    )
    .expect("Hook binding is valid");
    assert!(invalid_input.invoke(json!({"path": 7})).is_err());
    assert_eq!(invalid_input_calls.load(Ordering::SeqCst), 0);

    let invalid_output_calls = Arc::new(AtomicUsize::new(0));
    let invalid_output = ProductionHookHarness::new(
        input_schema,
        output_schema,
        json!({"status": "unexpected"}),
        invalid_output_calls.clone(),
    )
    .expect("Hook binding is valid");
    assert!(invalid_output.invoke(json!({"path": "README.md"})).is_err());
    assert_eq!(invalid_output_calls.load(Ordering::SeqCst), 1);
}

// Existing AGL-154 invariant retained by the new suite.
#[test]
fn declaration_digest_is_stable_for_recursive_object_order() {
    let mut first = tool_declaration("example.digest:read");
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

// Keep the explicit Effect fixture honest.
#[test]
fn explicit_effect_set_is_not_empty_by_accident() {
    assert_eq!(
        BTreeSet::from([EffectId::repo_files()]).len(),
        1,
        "Effect fixture collapsed"
    );
}
