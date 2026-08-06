#[path = "core/support/mod.rs"]
mod support;

use agl_kernel::{
    DeclarationError, ExtensionDescriptor, ExtensionId, ExtensionSource, ExtensionTrust, HookEvent,
    HookId, ToolId,
};
use support::{extension_with_tool, hook_declaration};

// KCT-ID-001. Mutation: permit one colon in ExtensionId.
#[test]
fn extension_id_never_accepts_a_namespace_colon() {
    for valid in [
        "core",
        "core.workspace",
        "agent.supervisor",
        "vendor-name.extension_v2",
    ] {
        assert!(ExtensionId::new(valid).is_ok(), "valid ExtensionId {valid}");
    }

    for invalid in ["core:workspace", ":core", "core:", "core:one:two"] {
        assert!(
            ExtensionId::new(invalid).is_err(),
            "invalid ExtensionId was admitted: {invalid}"
        );
    }
}

// KCT-ID-002. Mutation: permit an unqualified or multiply-qualified ToolId.
#[test]
fn tool_id_requires_exactly_one_namespace_colon() {
    for valid in [
        "core.workspace:fs.read",
        "core.process:shell.exec",
        "agent.supervisor:delegate",
    ] {
        assert!(ToolId::new(valid).is_ok(), "valid ToolId {valid}");
    }

    for invalid in [
        "fs.read",
        "core.workspace:",
        ":fs.read",
        "core.workspace:fs:read",
    ] {
        assert!(
            ToolId::new(invalid).is_err(),
            "invalid ToolId was admitted: {invalid}"
        );
    }
}

// KCT-ID-002. Mutation: validate Tool grammar without checking its owner.
#[test]
fn tool_namespace_must_equal_its_owner_extension() {
    assert!(
        extension_with_tool("core.workspace", "core.workspace:fs.read")
            .validate()
            .is_ok()
    );
    assert!(
        extension_with_tool("core.workspace", "core.process:fs.read")
            .validate()
            .is_err(),
        "foreign Tool namespace was admitted"
    );
}

// KCT-ID-003. Mutation: restore an old unqualified first-party Tool ID.
#[test]
fn complete_first_party_tool_map_uses_canonical_owner_namespaces() {
    const MAP: &[(&str, &[&str])] = &[
        (
            "core.workspace",
            &[
                "core.workspace:fs.read",
                "core.workspace:fs.list",
                "core.workspace:fs.search",
                "core.workspace:fs.apply_patch",
            ],
        ),
        (
            "core.process",
            &[
                "core.process:process.pwd",
                "core.process:process.cd",
                "core.process:process.exec",
                "core.process:process.start",
                "core.process:process.status",
                "core.process:process.read",
                "core.process:process.write",
                "core.process:process.resize",
                "core.process:process.kill",
                "core.process:shell.exec",
            ],
        ),
        (
            "core.cron",
            &[
                "core.cron:list",
                "core.cron:show",
                "core.cron:history",
                "core.cron:preflight",
                "core.cron:add",
                "core.cron:update",
                "core.cron:delete",
                "core.cron:enable",
                "core.cron:disable",
                "core.cron:run",
                "core.cron:tick",
            ],
        ),
        (
            "core.memory",
            &[
                "core.memory:search",
                "core.memory:list",
                "core.memory:suggest",
                "core.memory:add",
                "core.memory:approve",
                "core.memory:reject",
            ],
        ),
        (
            "core.note",
            &[
                "core.note:add",
                "core.note:search",
                "core.note:show",
                "core.note:update",
                "core.note:link",
                "core.note:delete",
                "core.note:remember",
            ],
        ),
        (
            "core.permission",
            &[
                "core.permission:status",
                "core.permission:request",
                "core.permission:grant",
                "core.permission:revoke",
            ],
        ),
        (
            "core.repo",
            &[
                "core.repo:status",
                "core.repo:export_profile",
                "core.repo:hooks.status",
                "core.repo:init",
                "core.repo:import_profile",
                "core.repo:install_hooks",
            ],
        ),
        (
            "core.store",
            &[
                "core.store:status",
                "core.store:export",
                "core.store:migrate",
            ],
        ),
        (
            "core.skill",
            &[
                "core.skill:list",
                "core.skill:inspect",
                "core.skill:status",
                "core.skill:verify",
                "core.skill:trust",
                "core.skill:revoke",
            ],
        ),
        (
            "matrix.outbox",
            &["matrix.outbox:status", "matrix.outbox:enqueue"],
        ),
        ("matrix.bridge", &["matrix.bridge:outbox.deliver"]),
        ("host.screen", &["host.screen:capture"]),
        ("agent.supervisor", &["agent.supervisor:delegate"]),
    ];

    for (owner, tools) in MAP {
        assert!(ExtensionId::new(*owner).is_ok(), "owner {owner}");
        for tool in *tools {
            let id = ToolId::new(*tool).unwrap_or_else(|error| panic!("{tool}: {error}"));
            assert_eq!(
                id.as_str().split_once(':').map(|parts| parts.0),
                Some(*owner),
                "Tool owner mismatch for {tool}"
            );
        }
    }

    for obsolete in [
        "memory.search",
        "repo.status",
        "screen.capture",
        "agent.delegate",
    ] {
        assert!(
            ToolId::new(obsolete).is_err(),
            "obsolete unqualified ToolId was admitted: {obsolete}"
        );
    }
}

// Retained Hook namespace invariant. Mutation: admit an unqualified or foreign-owned Hook ID.
#[test]
fn hook_ids_are_fully_qualified_owned_and_core_is_reserved() {
    for valid in ["core:repo_path.validate", "example.guard:artifact.validate"] {
        assert!(HookId::new(valid).is_ok(), "valid HookId {valid}");
    }
    for invalid in ["repo_path.validate", "core:one:two", "Core:validate"] {
        assert!(HookId::new(invalid).is_err(), "invalid HookId {invalid}");
    }

    let mismatched = ExtensionDescriptor::new(
        ExtensionId::new("example.guard").unwrap(),
        "Guard",
        "1.0.0",
        ExtensionSource::ThirdPartyRegistered,
        ExtensionTrust::TrustedRegistered,
    )
    .unwrap()
    .with_hook(hook_declaration(
        "other.guard:validate",
        HookEvent::ArtifactWrite,
    ));
    assert!(matches!(
        mismatched.validate(),
        Err(DeclarationError::HookExtensionMismatch { .. })
    ));

    assert!(matches!(
        ExtensionDescriptor::new(
            ExtensionId::new("core").unwrap(),
            "Core shadow",
            "1.0.0",
            ExtensionSource::ThirdPartyRegistered,
            ExtensionTrust::TrustedRegistered,
        ),
        Err(DeclarationError::ReservedExtensionNamespace { .. })
    ));

    let duplicate = ExtensionDescriptor::new(
        ExtensionId::new("example.guard").unwrap(),
        "Guard",
        "1.0.0",
        ExtensionSource::TestFixture,
        ExtensionTrust::TrustedRegistered,
    )
    .unwrap()
    .with_hook(hook_declaration(
        "example.guard:validate",
        HookEvent::ArtifactWrite,
    ))
    .with_hook(hook_declaration(
        "example.guard:validate",
        HookEvent::ModelResponse,
    ));
    assert!(matches!(
        duplicate.validate(),
        Err(DeclarationError::DuplicateId { kind: "hook", .. })
    ));
}
