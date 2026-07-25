use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use agl_artifact::{
    AglCompatibility, ArtifactAdapter, ArtifactAdapterDescriptor, ArtifactAdapterRegistry,
    ArtifactCandidate, ArtifactDataClass, ArtifactEnvelope, ArtifactError, ArtifactPackageId,
    ArtifactPackageRef, ArtifactPackageView, ArtifactPathRouter, ArtifactPathScope,
    ArtifactRelativePath, ArtifactSource, ArtifactSourceId, ArtifactSourceKind, ArtifactSourceTier,
    ArtifactTypeId, ArtifactVersion, ArtifactVersionReq, DirectoryArtifactSource,
    DirectoryPackageView, EXTENSION_ROOT, ErasedArtifactPayload, FUNCTION_ROOT,
    InMemoryPackageView, SKILL_ROOT, StaticArtifactSource,
};

struct FixtureAdapter {
    descriptor: ArtifactAdapterDescriptor,
}

impl ArtifactAdapter for FixtureAdapter {
    fn descriptor(&self) -> &ArtifactAdapterDescriptor {
        &self.descriptor
    }

    fn extract_envelope(
        &self,
        _package: &dyn ArtifactPackageView,
    ) -> Result<ArtifactEnvelope, ArtifactError> {
        Err(ArtifactError::AdapterPayload {
            type_id: self.descriptor.type_id.to_string(),
            reason: "fixture".to_owned(),
        })
    }

    fn validate_payload(
        &self,
        _package: &dyn ArtifactPackageView,
        _envelope: &ArtifactEnvelope,
    ) -> Result<ErasedArtifactPayload, ArtifactError> {
        Err(ArtifactError::AdapterPayload {
            type_id: self.descriptor.type_id.to_string(),
            reason: "fixture".to_owned(),
        })
    }
}

fn descriptor(type_id: ArtifactTypeId, root: &str, entrypoint: &str) -> ArtifactAdapterDescriptor {
    ArtifactAdapterDescriptor::new(type_id, root, entrypoint.parse().unwrap()).unwrap()
}

struct LifecycleAdapter {
    descriptor: ArtifactAdapterDescriptor,
}

impl ArtifactAdapter for LifecycleAdapter {
    fn descriptor(&self) -> &ArtifactAdapterDescriptor {
        &self.descriptor
    }

    fn extract_envelope(
        &self,
        package: &dyn ArtifactPackageView,
    ) -> Result<ArtifactEnvelope, ArtifactError> {
        let path: ArtifactRelativePath = "artifact.json".parse().unwrap();
        let bytes = package.read_file(&path)?;
        let envelope: ArtifactEnvelope =
            serde_json::from_slice(&bytes).map_err(|error| ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: error.to_string(),
            })?;
        envelope.validate()?;
        Ok(envelope)
    }

    fn validate_payload(
        &self,
        package: &dyn ArtifactPackageView,
        envelope: &ArtifactEnvelope,
    ) -> Result<ErasedArtifactPayload, ArtifactError> {
        if envelope.type_id != self.descriptor.type_id {
            return Err(ArtifactError::AdapterTypeMismatch {
                type_id: self.descriptor.type_id.to_string(),
                actual_type: envelope.type_id.to_string(),
            });
        }
        let path: ArtifactRelativePath = "payload.txt".parse().unwrap();
        let payload = String::from_utf8(package.read_file(&path)?).map_err(|error| {
            ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: error.to_string(),
            }
        })?;
        Ok(Box::new(payload))
    }
}

fn version(value: &str) -> ArtifactVersion {
    value.parse().unwrap()
}

fn envelope_json() -> serde_json::Value {
    serde_json::json!({
        "schema": "agentlibre.artifact/v1",
        "type": "function",
        "id": "example",
        "version": "1.0.0",
        "payload_schema": "agentlibre.function/v2",
        "agl": {
            "compatible": ">=1.0.0, <2.0.0",
            "tested": ["1.0.0"]
        },
        "requires": ["skill:vendor/workflow@^1.0"]
    })
}

#[test]
fn envelope_wire_round_trip_is_canonical() {
    let envelope: ArtifactEnvelope = serde_json::from_value(envelope_json()).unwrap();
    assert_eq!(serde_json::to_value(&envelope).unwrap(), envelope_json());
}

#[test]
fn envelope_wire_rejects_unknown_fields_and_invalid_evidence() {
    let mut unknown = envelope_json();
    unknown["unexpected"] = serde_json::json!(true);
    let error = serde_json::from_value::<ArtifactEnvelope>(unknown).unwrap_err();
    assert!(error.to_string().contains("unknown field"));

    let mut empty_tested = envelope_json();
    empty_tested["agl"]["tested"] = serde_json::json!([]);
    assert!(serde_json::from_value::<ArtifactEnvelope>(empty_tested).is_err());
}

#[test]
fn reference_and_semver_serde_use_strings() {
    let reference: ArtifactPackageRef = "model:vendor/base@>=1.0.0, <2.0.0".parse().unwrap();
    let encoded = serde_json::to_string(&reference).unwrap();
    assert_eq!(encoded, r#""model:vendor/base@>=1.0.0, <2.0.0""#);
    assert_eq!(
        serde_json::from_str::<ArtifactPackageRef>(&encoded).unwrap(),
        reference
    );

    let requirement: ArtifactVersionReq = "^1.2.0".parse().unwrap();
    assert_eq!(serde_json::to_string(&requirement).unwrap(), r#""^1.2.0""#);
    assert!(requirement.matches(&version("1.5.0")));
    assert!(!requirement.matches(&version("2.0.0")));

    let prerelease: ArtifactVersionReq = ">=1.0.0-alpha.1, <2.0.0".parse().unwrap();
    assert!(prerelease.matches(&version("1.0.0-alpha.2")));
    assert!(prerelease.matches(&version("1.0.0")));
}

#[test]
fn compatibility_serde_validates_tested_versions() {
    let empty = serde_json::json!({
        "compatible": ">=1.0.0",
        "tested": []
    });
    assert!(serde_json::from_value::<AglCompatibility>(empty).is_err());
}

#[test]
fn registry_rejects_duplicate_and_reserved_roots() {
    let function = descriptor(ArtifactTypeId::function(), FUNCTION_ROOT, "FUNCTION.md");
    let duplicate_type = descriptor(ArtifactTypeId::function(), FUNCTION_ROOT, "other.md");
    assert!(matches!(
        ArtifactAdapterRegistry::new([
            FixtureAdapter {
                descriptor: function
            },
            FixtureAdapter {
                descriptor: duplicate_type,
            },
        ]),
        Err(ArtifactError::DuplicateAdapterType { .. })
    ));

    let skill = descriptor(ArtifactTypeId::skill(), SKILL_ROOT, "SKILL.md");
    let duplicate_root = descriptor("vendor.workflow".parse().unwrap(), SKILL_ROOT, "entry.md");
    assert!(matches!(
        ArtifactAdapterRegistry::new([
            FixtureAdapter { descriptor: skill },
            FixtureAdapter {
                descriptor: duplicate_root,
            },
        ]),
        Err(ArtifactError::DuplicateAdapterRoot { .. })
    ));

    let reserved = descriptor(
        "vendor.workflow".parse().unwrap(),
        EXTENSION_ROOT,
        "entry.md",
    );
    assert!(matches!(
        ArtifactAdapterRegistry::new([FixtureAdapter {
            descriptor: reserved
        }]),
        Err(ArtifactError::ReservedRootCollision { .. })
    ));
}

#[test]
fn adapter_descriptor_rejects_path_like_values() {
    let type_id = ArtifactTypeId::function();
    assert!(matches!(
        ArtifactAdapterDescriptor::new(type_id.clone(), "", "FUNCTION.md".parse().unwrap()),
        Err(ArtifactError::InvalidAdapterRoot { .. })
    ));
    assert!(matches!(
        "/nested/FUNCTION.md".parse::<agl_artifact::ArtifactEntrypoint>(),
        Err(ArtifactError::InvalidAdapterEntrypoint { .. })
    ));
}

#[test]
fn package_views_are_sorted_and_share_logical_contents() {
    let files = vec![
        ("z.txt".parse().unwrap(), b"z".to_vec()),
        ("nested/a.txt".parse().unwrap(), b"a".to_vec()),
    ];
    let memory = InMemoryPackageView::new(files.clone()).unwrap();
    assert_eq!(
        memory
            .files()
            .unwrap()
            .into_iter()
            .map(|path| path.to_string())
            .collect::<Vec<_>>(),
        vec!["nested/a.txt", "z.txt"]
    );

    let root = temp_dir("views");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("z.txt"), b"z").unwrap();
    fs::write(root.join("nested/a.txt"), b"a").unwrap();
    let directory = DirectoryPackageView::new(&root).unwrap();
    assert_eq!(directory.files().unwrap(), memory.files().unwrap());
    assert_eq!(
        directory
            .read_file(&"nested/a.txt".parse().unwrap())
            .unwrap(),
        b"a"
    );
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn directory_views_reject_symlinks() {
    use std::os::unix::fs::symlink;

    let root = temp_dir("symlink");
    fs::write(root.join("outside.txt"), b"outside").unwrap();
    symlink(root.join("outside.txt"), root.join("link.txt")).unwrap();
    let view = DirectoryPackageView::new(&root).unwrap();
    assert!(matches!(
        view.files(),
        Err(ArtifactError::PackageSymlinkRejected { .. })
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn source_tiers_and_kinds_are_independent_and_serializable() {
    assert_eq!(
        serde_json::to_string(&ArtifactSourceTier::Builtin).unwrap(),
        "\"builtin\""
    );
    assert_eq!(
        serde_json::to_string(&ArtifactSourceKind::Git).unwrap(),
        "\"git\""
    );
    let source_id: ArtifactSourceId = "workspace".parse().unwrap();
    let view = Arc::new(InMemoryPackageView::default());
    let candidate = ArtifactCandidate::new(
        ArtifactTypeId::function(),
        "example".parse().unwrap(),
        version("1.0.0"),
        source_id.clone(),
        ArtifactSourceTier::Workspace,
        ArtifactSourceKind::Git,
        view,
    );
    let source = StaticArtifactSource::new(
        source_id,
        ArtifactSourceTier::Workspace,
        ArtifactSourceKind::Git,
        vec![candidate],
    )
    .unwrap();
    let candidates = source.candidates(&ArtifactTypeId::function()).unwrap();
    assert_eq!(candidates[0].tier, ArtifactSourceTier::Workspace);
    assert_eq!(candidates[0].kind, ArtifactSourceKind::Git);
}

#[test]
fn adapter_lifecycle_returns_checked_payload_and_directory_candidates() {
    let descriptor = descriptor(ArtifactTypeId::function(), FUNCTION_ROOT, "FUNCTION.md");
    let adapter = LifecycleAdapter { descriptor };
    let registry = Arc::new(ArtifactAdapterRegistry::new([adapter]).unwrap());
    let envelope: ArtifactEnvelope = serde_json::from_value(envelope_json()).unwrap();
    let memory = Arc::new(
        InMemoryPackageView::new(vec![
            (
                "artifact.json".parse().unwrap(),
                serde_json::to_vec(&envelope).unwrap(),
            ),
            ("payload.txt".parse().unwrap(), b"checked".to_vec()),
            ("FUNCTION.md".parse().unwrap(), b"entry".to_vec()),
        ])
        .unwrap(),
    );
    let adapter = registry.lookup(&ArtifactTypeId::function()).unwrap();
    let extracted = adapter.extract_envelope(memory.as_ref()).unwrap();
    let payload = adapter
        .validate_payload(memory.as_ref(), &extracted)
        .unwrap();
    assert_eq!(payload.downcast_ref::<String>().unwrap(), "checked");

    let root = temp_dir("source");
    let package = root.join(FUNCTION_ROOT).join("example").join("1.0.0");
    fs::create_dir_all(&package).unwrap();
    fs::write(
        package.join("artifact.json"),
        serde_json::to_vec(&envelope).unwrap(),
    )
    .unwrap();
    fs::write(package.join("payload.txt"), b"checked").unwrap();
    fs::write(package.join("FUNCTION.md"), b"entry").unwrap();
    let source = DirectoryArtifactSource::new(
        "workspace".parse().unwrap(),
        ArtifactSourceTier::Workspace,
        ArtifactSourceKind::Git,
        &root,
        registry,
    );
    let candidates = source.candidates(&ArtifactTypeId::function()).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].package_id,
        "example".parse::<ArtifactPackageId>().unwrap()
    );
    assert_eq!(candidates[0].version, version("1.0.0"));
    assert_eq!(candidates[0].kind, ArtifactSourceKind::Git);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn path_router_derives_separate_scopes_without_touching_filesystem() {
    let adapter = LifecycleAdapter {
        descriptor: descriptor(ArtifactTypeId::function(), FUNCTION_ROOT, "FUNCTION.md"),
    };
    let registry = Arc::new(ArtifactAdapterRegistry::new([adapter]).unwrap());
    let router = ArtifactPathRouter::new(
        "/workspace-that-does-not-exist",
        "/xdg/data",
        "/xdg/config",
        "/xdg/state",
        "/xdg/cache",
        registry,
    );
    let type_id = ArtifactTypeId::function();
    let package_id: ArtifactPackageId = "vendor/example".parse().unwrap();
    let version = version("1.2.3");
    assert_eq!(
        router
            .workspace_package_path(&type_id, &package_id, &version)
            .unwrap(),
        PathBuf::from("/workspace-that-does-not-exist/.agl/functions/vendor/example/1.2.3")
    );
    assert_eq!(
        router.xdg_config_path(&type_id, &package_id).unwrap(),
        PathBuf::from("/xdg/config/functions/vendor/example")
    );
    assert_eq!(
        router
            .root(ArtifactPathScope::Xdg, ArtifactDataClass::Cache, &type_id)
            .unwrap(),
        PathBuf::from("/xdg/cache/functions")
    );
}

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("agl-artifact-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}
