use std::sync::Arc;

use agl_package::{
    DirectoryPackageSource, ErasedPackagePayload, InMemoryPackageView, LockedPackage,
    PackageAdapter, PackageAdapterDescriptor, PackageAdapterRegistry, PackageCandidate,
    PackageEnvelope, PackageError, PackageId, PackageLock, PackageRef, PackageRelativePath,
    PackageResolver, PackageSchemaId, PackageSourceDeclaration, PackageSourceId, PackageSourceKind,
    PackageSourceTier, PackageTypeId, PackageVersion, ResolvedPackageGraph, StaticPackageSource,
    WorkspaceManifest, compute_package_digest,
};

struct FixtureAdapter {
    descriptor: PackageAdapterDescriptor,
}

impl PackageAdapter for FixtureAdapter {
    fn descriptor(&self) -> &PackageAdapterDescriptor {
        &self.descriptor
    }

    fn extract_envelope(
        &self,
        package: &dyn agl_package::PackageView,
    ) -> Result<PackageEnvelope, PackageError> {
        let bytes = package.read_file(&"package.json".parse().unwrap())?;
        serde_json::from_slice(&bytes).map_err(|error| PackageError::AdapterEnvelope {
            type_id: self.descriptor.type_id.to_string(),
            reason: error.to_string(),
        })
    }

    fn validate_payload(
        &self,
        _package: &dyn agl_package::PackageView,
        _envelope: &PackageEnvelope,
    ) -> Result<ErasedPackagePayload, PackageError> {
        Ok(Box::new(()))
    }
}

fn view(entries: &[(&str, &[u8])]) -> InMemoryPackageView {
    InMemoryPackageView::new(
        entries
            .iter()
            .map(|(path, bytes)| (path.parse::<PackageRelativePath>().unwrap(), bytes.to_vec())),
    )
    .unwrap()
}

fn envelope(id: &str, version: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schema": "agentlibre.package/v1",
        "type": "function",
        "id": id,
        "version": version,
        "payload_schema": "agentlibre.function/v3",
        "agl": {"compatible": "^1", "tested": ["1.0.0"]},
        "requires": []
    }))
    .unwrap()
}

fn adapter_registry() -> Arc<PackageAdapterRegistry> {
    Arc::new(
        PackageAdapterRegistry::new([FixtureAdapter {
            descriptor: PackageAdapterDescriptor::new(
                PackageTypeId::function(),
                "functions",
                "package.json".parse().unwrap(),
            )
            .unwrap(),
        }])
        .unwrap(),
    )
}

// AGL172-001, AGL172-015, AGL172-050 and AGL172-060.
#[test]
fn workspace_manifest_v3_is_the_only_exact_serialized_workspace_type() {
    assert_eq!(WorkspaceManifest::VERSION, 3);
    let manifest = WorkspaceManifest::from_toml(
        r#"
version = 3
default_function = "function:agentlibre/default@^1"

[[sources]]
id = "workspace"
tier = "workspace"
kind = "directory"
path = ".agl/packages"

[policy]

[config]
"#,
    )
    .unwrap();
    let encoded = manifest.to_toml().unwrap();
    let value: toml::Value = toml::from_str(&encoded).unwrap();
    let table = value.as_table().unwrap();
    assert_eq!(
        table.keys().map(String::as_str).collect::<Vec<_>>(),
        ["config", "default_function", "policy", "sources", "version"]
    );
    assert!(WorkspaceManifest::from_toml("version = 2\ncomponents = {}\n").is_err());
    assert!(WorkspaceManifest::from_toml("version = 3\ncomponents = {}\n").is_err());
}

// AGL172-003.
#[test]
fn package_adapter_registry_rejects_one_duplicate_type_before_resolution() {
    let descriptor = || {
        PackageAdapterDescriptor::new(
            PackageTypeId::function(),
            "functions",
            "package.json".parse().unwrap(),
        )
        .unwrap()
    };
    let error = PackageAdapterRegistry::new([
        FixtureAdapter {
            descriptor: descriptor(),
        },
        FixtureAdapter {
            descriptor: descriptor(),
        },
    ])
    .unwrap_err();
    assert!(matches!(error, PackageError::DuplicateAdapterType { .. }));
}

// AGL172-006, AGL172-007 and AGL172-009.
#[test]
fn source_tier_semver_lock_and_root_override_select_one_identical_node() {
    let type_id = PackageTypeId::function();
    let package_id: PackageId = "example/echo".parse().unwrap();
    let low_id = PackageSourceId::new("low").unwrap();
    let high_id = PackageSourceId::new("high").unwrap();
    let low_view = view(&[("package.json", &envelope("example/echo", "1.0.0"))]);
    let high_view = view(&[("package.json", &envelope("example/echo", "1.1.0"))]);
    let low = PackageCandidate::new(
        type_id.clone(),
        package_id.clone(),
        "1.0.0".parse::<PackageVersion>().unwrap(),
        low_id.clone(),
        PackageSourceTier::Builtin,
        PackageSourceKind::Embedded,
        Arc::new(low_view),
    );
    let high = PackageCandidate::new(
        type_id.clone(),
        package_id.clone(),
        "1.1.0".parse::<PackageVersion>().unwrap(),
        high_id.clone(),
        PackageSourceTier::Explicit,
        PackageSourceKind::Embedded,
        Arc::new(high_view),
    );
    let resolver = PackageResolver::new(
        adapter_registry(),
        vec![
            Arc::new(
                StaticPackageSource::new(
                    low_id,
                    PackageSourceTier::Builtin,
                    PackageSourceKind::Embedded,
                    [low],
                )
                .unwrap(),
            ),
            Arc::new(
                StaticPackageSource::new(
                    high_id,
                    PackageSourceTier::Explicit,
                    PackageSourceKind::Embedded,
                    [high],
                )
                .unwrap(),
            ),
        ],
    );
    let reference: PackageRef = "function:example/echo@^1".parse().unwrap();
    let selected = resolver.resolve(&reference).unwrap();
    let lock = selected.lock().unwrap();
    let locked = resolver.resolve_locked(&reference, &lock).unwrap();
    let selected_node = selected.nodes.values().next().unwrap();

    assert_eq!(
        selected
            .nodes
            .iter()
            .map(|(key, node)| (key, &node.package_tree_digest))
            .collect::<Vec<_>>(),
        locked
            .nodes
            .iter()
            .map(|(key, node)| (key, &node.package_tree_digest))
            .collect::<Vec<_>>()
    );
    assert_eq!(selected.nodes.len(), 1);
    assert_eq!(
        selected_node.candidate.source_id,
        PackageSourceId::new("high").unwrap()
    );
    assert_eq!(
        selected_node.package_tree_digest,
        compute_package_digest(selected_node.candidate.view()).unwrap()
    );
    assert_eq!(
        lock.packages[0].package_tree_digest,
        selected_node.package_tree_digest
    );
}

// AGL172-047 and AGL172-048.
#[test]
fn package_envelope_uses_only_package_schema_and_rejects_artifact_schema() {
    let value = envelope("example/echo", "1.0.0");
    let parsed: PackageEnvelope = serde_json::from_slice(&value).unwrap();
    assert_eq!(
        parsed.schema,
        PackageSchemaId::new("agentlibre.package/v1").unwrap()
    );

    let current_schema = "agentlibre.package/v1";
    let old_schema = ["agentlibre.", "artifact", "/v1"].concat();
    let old = value
        .windows(current_schema.len())
        .position(|window| window == current_schema.as_bytes())
        .unwrap();
    let mut old_value = value;
    old_value.splice(old..old + current_schema.len(), old_schema.bytes());
    assert!(serde_json::from_slice::<PackageEnvelope>(&old_value).is_err());
}

// AGL172-050 and AGL172-060.
#[test]
fn package_lock_v1_contains_packages_only_and_rejects_component_records() {
    assert_eq!(PackageLock::VERSION, 1);
    let lock = PackageLock::new([LockedPackage::fixture(
        PackageTypeId::function(),
        "example/echo".parse().unwrap(),
        "1.0.0".parse().unwrap(),
        PackageSourceId::new("fixture").unwrap(),
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .parse()
            .unwrap(),
    )])
    .unwrap();
    let encoded = lock.to_toml().unwrap();
    assert!(!encoded.contains("component"));
    assert_eq!(PackageLock::from_toml(&encoded).unwrap(), lock);
    assert!(PackageLock::from_toml("version = 1\n[[components]]\nid = 'tasks'\n").is_err());
    assert!(PackageLock::from_toml("version = 2\npackages = []\n").is_err());
}

// AGL172-053.
#[test]
fn package_source_declarations_round_trip_without_component_semantics() {
    let declaration: PackageSourceDeclaration = toml::from_str(
        r#"
id = "workspace"
tier = "workspace"
kind = "directory"
path = ".agl/packages"
"#,
    )
    .unwrap();
    let encoded = toml::to_string(&declaration).unwrap();
    assert_eq!(
        toml::from_str::<PackageSourceDeclaration>(&encoded).unwrap(),
        declaration
    );
    assert!(!encoded.contains("component"));
    let _: Option<DirectoryPackageSource> = None;
    let _: Option<ResolvedPackageGraph> = None;
}
