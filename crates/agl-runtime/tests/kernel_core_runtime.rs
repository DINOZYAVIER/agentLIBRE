#[path = "core/support.rs"]
mod support;

use agl_artifact::ExtensionArtifactManifest;
use serde_json::json;
use sha2::{Digest, Sha256};
use support::{ExecutionProbe, FactoryKey, ProductionRuntimeHarness, RegistrationFixture};

fn digest(bytes: &[u8]) -> String {
    let value = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{value}")
}

fn key(extension_id: &str, api_major: u32, declaration_digest: &str) -> FactoryKey {
    FactoryKey {
        extension_id: extension_id.to_string(),
        api_major,
        declaration_digest: digest(declaration_digest.as_bytes()),
    }
}

// KCT-RUNTIME-001. Mutation: key factories only by ExtensionId.
#[test]
fn static_factory_key_uses_extension_api_major_and_declaration_digest() {
    let probe = ExecutionProbe::new();
    let mut runtime = ProductionRuntimeHarness::new(probe);
    let first = key("example.echo", 1, "sha256:aaa");
    runtime
        .register_factory(first.clone())
        .expect("first factory registers");
    runtime
        .register_factory(key("example.echo", 2, "sha256:aaa"))
        .expect("different API major is a distinct key");
    runtime
        .register_factory(key("example.echo", 1, "sha256:bbb"))
        .expect("different declaration digest is a distinct key");
    assert!(
        runtime.register_factory(first).is_err(),
        "duplicate complete factory key was accepted"
    );
}

// KCT-RUNTIME-001. Mutation: fall back by ExtensionId after exact lookup fails.
#[test]
fn static_factory_lookup_fails_closed_for_missing_or_mismatched_keys() {
    let probe = ExecutionProbe::new();
    let mut runtime = ProductionRuntimeHarness::new(probe);
    let exact = key("example.echo", 1, "sha256:aaa");
    runtime.register_factory(exact.clone()).unwrap();
    runtime
        .resolve_factory(&exact)
        .expect("exact complete key resolves");

    for rejected in [
        key("example.missing", 1, "sha256:aaa"),
        key("example.echo", 2, "sha256:aaa"),
        key("example.echo", 1, "sha256:bbb"),
    ] {
        assert!(
            runtime.resolve_factory(&rejected).is_err(),
            "mismatched key resolved: {rejected:?}"
        );
    }
}

// KCT-RUNTIME-002 and KCT-EXT-005. Mutation: let factory metadata replace artifact identity.
#[test]
fn artifact_descriptor_is_authoritative_and_bindings_match_exactly() {
    let probe = ExecutionProbe::new();
    let mut runtime = ProductionRuntimeHarness::new(probe);
    let key = key("example.echo", 1, "sha256:descriptor");
    let descriptor_digest = key.declaration_digest.clone();
    runtime.register_factory(key.clone()).unwrap();

    let registration = runtime
        .compose_registration(RegistrationFixture {
            artifact_extension_id: "example.echo",
            artifact_digest: &descriptor_digest,
            factory_key: key.clone(),
            declared_tools: &["example.echo:run"],
            bound_tools: &["example.echo:run"],
            declared_hooks: &["example.echo:validate"],
            bound_hooks: &["example.echo:validate"],
        })
        .expect("exact declaration and bindings compose");
    assert_eq!(registration.descriptor_extension_id, "example.echo");
    assert_eq!(registration.descriptor_digest, descriptor_digest);
    assert_eq!(
        registration.tool_ids,
        ["example.echo:run".to_string()].into()
    );
    assert_eq!(
        registration.hook_ids,
        ["example.echo:validate".to_string()].into()
    );

    for (declared_tools, bound_tools, declared_hooks, bound_hooks) in [
        (
            vec!["example.echo:run"],
            Vec::new(),
            vec!["example.echo:validate"],
            vec!["example.echo:validate"],
        ),
        (
            vec!["example.echo:run"],
            vec!["example.echo:run", "example.echo:extra"],
            vec!["example.echo:validate"],
            vec!["example.echo:validate"],
        ),
        (
            vec!["example.echo:run"],
            vec!["example.echo:run"],
            vec!["example.echo:validate"],
            Vec::new(),
        ),
        (
            vec!["example.echo:run"],
            vec!["example.echo:run"],
            vec!["example.echo:validate"],
            vec!["example.echo:validate", "example.echo:extra"],
        ),
    ] {
        assert!(
            runtime
                .compose_registration(RegistrationFixture {
                    artifact_extension_id: "example.echo",
                    artifact_digest: &key.declaration_digest,
                    factory_key: key.clone(),
                    declared_tools: &declared_tools,
                    bound_tools: &bound_tools,
                    declared_hooks: &declared_hooks,
                    bound_hooks: &bound_hooks,
                })
                .is_err(),
            "mismatched bindings were admitted"
        );
    }
}

// Decision 17 manifest shape. Mutation: restore an opaque implementation string or ID.
#[test]
fn rust_static_manifest_has_only_a_kind_without_implementation_identity() {
    let manifest = json!({
        "schema": "agentlibre.artifact/v1",
        "type": "extension",
        "id": "example.echo",
        "version": "1.0.0",
        "payload_schema": "agentlibre.extension/v1",
        "agl": {"compatible": ">=1.0.0", "tested": ["1.0.0"]},
        "requires": [],
        "api_major": 1,
        "implementation": {"kind": "rust-static"}
    });
    let parsed = serde_json::from_value::<ExtensionArtifactManifest>(manifest.clone())
        .unwrap_or_else(|error| panic!("selected rust-static manifest was rejected: {error}"));
    let encoded = serde_json::to_value(parsed).unwrap();
    assert_eq!(encoded["implementation"], json!({"kind": "rust-static"}));
    assert!(encoded["implementation"].get("id").is_none());
    assert!(encoded["implementation"].get("crate").is_none());
}

// KCT-RUNTIME-003. Mutation: verify a path and later reread changed bytes.
#[test]
fn binary_digest_is_verified_against_the_retained_immutable_bytes() {
    let probe = ExecutionProbe::new();
    let runtime = ProductionRuntimeHarness::new(probe);
    let original = b"extension payload v1";
    let changed = b"extension payload v2";
    let expected = digest(original);

    assert!(
        runtime
            .verify_binary("bin/extension", &expected, changed)
            .is_err(),
        "changed binary bytes matched the old digest"
    );
    let retained = runtime
        .verify_binary("bin/extension", &expected, original)
        .expect("matching binary verifies");
    assert_eq!(retained.relative_path, "bin/extension");
    assert_eq!(retained.digest, expected);
    assert_eq!(retained.bytes, original);

    for unsafe_path in ["../extension", "/absolute/extension", "bin/../../extension"] {
        assert!(
            runtime
                .verify_binary(unsafe_path, &expected, original)
                .is_err(),
            "unsafe payload path was admitted: {unsafe_path}"
        );
    }
    assert!(
        runtime
            .verify_binary("bin/extension", "sha256:not-hex", original)
            .is_err()
    );
}

// KCT-RUNTIME-003. Mutation: treat an absent declared binary as an empty payload.
#[test]
fn declared_binary_must_exist_before_registration() {
    let probe = ExecutionProbe::new();
    let runtime = ProductionRuntimeHarness::new(probe);
    let expected = digest(b"expected payload");
    assert!(
        runtime
            .verify_optional_binary("bin/missing", &expected, None)
            .is_err()
    );
}

// KCT-RUNTIME-004. Mutation: execute or load verified binary content.
#[test]
fn verified_binary_payload_is_stored_but_never_executed() {
    let probe = ExecutionProbe::new();
    let runtime = ProductionRuntimeHarness::new(probe.clone());
    let bytes = b"not an executable contract";
    runtime
        .verify_binary("bin/inert", &digest(bytes), bytes)
        .expect("matching binary verifies");
    assert_eq!(probe.count(), 0, "binary verification executed the payload");
}

// KCT-RUNTIME-005. Mutation: discover factories through process-global state.
#[test]
fn static_registries_are_explicit_and_process_local() {
    let key = key("example.echo", 1, "sha256:aaa");
    let mut first = ProductionRuntimeHarness::new(ExecutionProbe::new());
    let second = ProductionRuntimeHarness::new(ExecutionProbe::new());
    first.register_factory(key.clone()).unwrap();

    assert!(first.contains_factory(&key));
    assert!(
        !second.contains_factory(&key),
        "factory leaked into an independently composed registry"
    );
    assert!(second.resolve_factory(&key).is_err());
}
