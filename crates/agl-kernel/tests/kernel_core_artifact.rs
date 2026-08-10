use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agl_ids::{ExecutionScope, RunId};
use agl_kernel::{
    ArtifactAccess, ArtifactDeclaration, ArtifactEffectLink, ArtifactId, ArtifactKindId,
    ArtifactTargetSelector, CatalogDigest, DeclarationError, EffectDeclaration, EffectId,
    ExtensionDescriptor, ExtensionId, ExtensionRequirement, ExtensionSource, ExtensionTrust,
    OperationKind, ToolAccessMode, ToolBinding, ToolDeclaration, ToolDispatchContext,
    ToolDispatchControl, ToolDispatchError, ToolHandler, ToolHandlerFuture, ToolId, ToolInvocation,
    ToolPolicyInput, ToolResult, ToolRuntime,
};
use serde_json::json;

struct CountingHandler(Arc<AtomicUsize>);

impl ToolHandler for CountingHandler {
    fn dispatch(&self, _context: ToolDispatchContext) -> ToolHandlerFuture<'_> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(Ok(ToolResult::new(json!({})))))
    }
}

fn artifact_id(value: &str) -> ArtifactId {
    ArtifactId::new(value).unwrap()
}

fn extension_id(value: &str) -> ExtensionId {
    ExtensionId::new(value).unwrap()
}

fn descriptor(id: &str) -> ExtensionDescriptor {
    ExtensionDescriptor::new(
        extension_id(id),
        id,
        "1.0.0",
        ExtensionSource::TestFixture,
        ExtensionTrust::TrustedRegistered,
    )
    .unwrap()
    .with_effect(EffectDeclaration::for_standard(EffectId::repo_files()).unwrap())
}

fn tool(id: &str) -> ToolDeclaration {
    ToolDeclaration::new(
        ToolId::new(id).unwrap(),
        id,
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"artifact": {"type": "string"}},
            "additionalProperties": false
        }),
        OperationKind::Write,
    )
    .unwrap()
    .with_state_effects([EffectId::repo_files()])
}

// AGL171-018 and AGL171-020.
#[test]
fn artifact_declaration_is_kernel_owned_and_contains_no_runtime_location() {
    let declaration = ArtifactDeclaration::new(
        artifact_id("example.workspace:data"),
        ArtifactKindId::new("agl.file-tree").unwrap(),
        [ArtifactAccess::ReadTree, ArtifactAccess::MutateTree],
    )
    .unwrap();
    let value = serde_json::to_value(&declaration).unwrap();

    assert_eq!(value["id"], "example.workspace:data");
    assert_eq!(value["kind"], "agl.file-tree");
    for forbidden in ["path", "checkout_path", "remote_url", "repository_path"] {
        assert!(
            value.get(forbidden).is_none(),
            "runtime location leaked: {forbidden}"
        );
    }
    assert!(ArtifactKindId::new("vendor.custom-tree").is_ok());
}

// AGL171-018 and AGL171-027.
#[test]
fn artifact_selectors_validate_before_handler_and_affect_declaration_digest() {
    let artifact = ArtifactDeclaration::new(
        artifact_id("example.workspace:data"),
        ArtifactKindId::new("agl.file-tree").unwrap(),
        [ArtifactAccess::ReadTree],
    )
    .unwrap();
    let fixed = tool("example.workspace:fixed").with_artifact_link(ArtifactEffectLink::new(
        EffectId::repo_files(),
        ArtifactTargetSelector::Fixed(artifact.id.clone()),
        ArtifactAccess::ReadTree,
    ));
    let from_argument =
        tool("example.workspace:argument").with_artifact_link(ArtifactEffectLink::new(
            EffectId::repo_files(),
            ArtifactTargetSelector::FromArgument {
                pointer: "/artifact".parse().unwrap(),
                access: ArtifactAccess::ReadTree,
            },
            ArtifactAccess::ReadTree,
        ));
    let without_selector = descriptor("example.workspace")
        .with_artifact(artifact.clone())
        .with_tool(tool("example.workspace:fixed"));
    let with_fixed = descriptor("example.workspace")
        .with_artifact(artifact.clone())
        .with_tool(fixed);
    let with_argument = descriptor("example.workspace")
        .with_artifact(artifact)
        .with_tool(from_argument);

    assert_ne!(without_selector.digest(), with_fixed.digest());
    assert_ne!(with_fixed.digest(), with_argument.digest());
    assert_eq!(
        with_argument
            .resolve_artifact_targets(&json!({"artifact": "example.workspace:data"}))
            .unwrap()[0]
            .artifact_id,
        artifact_id("example.workspace:data")
    );
    assert!(
        with_argument
            .resolve_artifact_targets(&json!({"artifact": "unknown:data"}))
            .is_err()
    );
}

// AGL171-027. The dynamic target is resolved after schema validation and
// before handler entry on the real ToolRuntime dispatch path.
#[test]
fn unknown_argument_selected_artifact_stops_before_handler() {
    let artifact = ArtifactDeclaration::new(
        artifact_id("example.workspace:data"),
        ArtifactKindId::new("agl.file-tree").unwrap(),
        [ArtifactAccess::ReadTree],
    )
    .unwrap();
    let declaration = descriptor("example.workspace")
        .with_artifact(artifact)
        .with_tool(
            tool("example.workspace:argument").with_artifact_link(ArtifactEffectLink::new(
                EffectId::repo_files(),
                ArtifactTargetSelector::FromArgument {
                    pointer: "/artifact".to_owned(),
                    access: ArtifactAccess::ReadTree,
                },
                ArtifactAccess::ReadTree,
            )),
        );
    let calls = Arc::new(AtomicUsize::new(0));
    let mut runtime = ToolRuntime::new();
    runtime
        .register_extension(agl_kernel::ExtensionRegistration::new(
            declaration.clone(),
            [ToolBinding::new(
                ToolId::new("example.workspace:argument").unwrap(),
                CountingHandler(calls.clone()),
            )],
        ))
        .unwrap();
    let effective = ToolPolicyInput::new(
        [declaration.clone()],
        [ToolId::new("example.workspace:argument").unwrap()],
        ToolAccessMode::Write,
    )
    .resolve()
    .unwrap();
    let invocation = ToolInvocation::new(
        ExecutionScope::builder(RunId::generate()).build().unwrap(),
        ToolId::new("example.workspace:argument").unwrap(),
        declaration.id.clone(),
        declaration.tools[0].digest(),
        effective.policy_hash().clone(),
        json!({"artifact": "example.workspace:unknown"}),
    );

    assert!(matches!(
        runtime.dispatch(invocation, &effective, ToolDispatchControl::uncancellable()),
        Err(ToolDispatchError::ArtifactTarget(
            DeclarationError::UnknownArtifact { .. }
        ))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// AGL171-018, AGL171-023 and AGL171-027.
#[test]
fn artifact_links_reject_undeclared_access_and_implicit_foreign_owner() {
    let local = ArtifactDeclaration::new(
        artifact_id("example.consumer:data"),
        ArtifactKindId::new("agl.file-tree").unwrap(),
        [ArtifactAccess::ReadTree],
    )
    .unwrap();
    let undeclared = descriptor("example.consumer").with_tool(
        tool("example.consumer:missing").with_artifact_link(ArtifactEffectLink::new(
            EffectId::repo_files(),
            ArtifactTargetSelector::Fixed(artifact_id("example.consumer:missing")),
            ArtifactAccess::ReadTree,
        )),
    );
    assert!(matches!(
        undeclared.validate(),
        Err(DeclarationError::UnknownArtifact { .. })
    ));

    let excess = descriptor("example.consumer")
        .with_artifact(local)
        .with_tool(
            tool("example.consumer:write").with_artifact_link(ArtifactEffectLink::new(
                EffectId::repo_files(),
                ArtifactTargetSelector::Fixed(artifact_id("example.consumer:data")),
                ArtifactAccess::MutateTree,
            )),
        );
    assert!(matches!(
        excess.validate(),
        Err(DeclarationError::ArtifactAccessMismatch { .. })
    ));

    let foreign_link =
        tool("example.consumer:foreign").with_artifact_link(ArtifactEffectLink::new(
            EffectId::repo_files(),
            ArtifactTargetSelector::Fixed(artifact_id("example.provider:data")),
            ArtifactAccess::ReadTree,
        ));
    let implicit = descriptor("example.consumer").with_tool(foreign_link.clone());
    assert!(matches!(
        implicit.validate(),
        Err(DeclarationError::ForeignArtifactOwner { .. })
    ));

    let explicit = descriptor("example.consumer")
        .with_requirement(ExtensionRequirement::new(
            extension_id("example.provider"),
            1,
        ))
        .with_tool(foreign_link);
    assert!(explicit.validate().is_ok());
}

// AGL171-006 and AGL171-014.
#[test]
fn declaration_and_catalog_digests_have_separate_material() {
    let authored = descriptor("example.digest");
    let authored_digest = authored.digest();
    let source_changed = authored
        .clone()
        .with_runtime_identity(ExtensionSource::Builtin, ExtensionTrust::TrustedRegistered);
    let trust_changed = authored
        .clone()
        .with_runtime_identity(ExtensionSource::TestFixture, ExtensionTrust::Changed);

    assert_eq!(source_changed.digest(), authored_digest);
    assert_eq!(trust_changed.digest(), authored_digest);
    assert_ne!(
        CatalogDigest::from_admitted([&authored]),
        CatalogDigest::from_admitted([&source_changed])
    );
    assert_ne!(
        CatalogDigest::from_admitted([&authored]),
        CatalogDigest::from_admitted([&trust_changed])
    );
}
