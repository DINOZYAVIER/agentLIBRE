use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use agl_artifact::{
    AglCompatibility, ArtifactAdapter, ArtifactAdapterDescriptor, ArtifactAdapterRegistry,
    ArtifactCandidate, ArtifactConfigLayer, ArtifactDataClass, ArtifactEnvelope, ArtifactError,
    ArtifactLock, ArtifactPackageId, ArtifactPackageRef, ArtifactPackageView, ArtifactPathRouter,
    ArtifactPathScope, ArtifactRelativePath, ArtifactResolver, ArtifactSource,
    ArtifactSourceDeclaration, ArtifactSourceId, ArtifactSourceKind, ArtifactSourceTier,
    ArtifactTypeId, ArtifactVersion, ArtifactVersionReq, DirectoryArtifactSource,
    DirectoryPackageView, EXTENSION_ROOT, ErasedArtifactPayload, FUNCTION_ROOT,
    InMemoryPackageView, SKILL_ROOT, StaticArtifactSource, WorkspaceComponent,
    WorkspaceComponentKind, WorkspaceManifest,
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
            serde_json::from_slice(&bytes).map_err(|error| ArtifactError::AdapterEnvelope {
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
    assert_eq!(
        candidates[0].package_root.as_deref(),
        Some(package.as_path())
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn directory_inventory_preserves_invalid_envelope_but_resolution_stays_fail_closed() {
    let descriptor = descriptor(ArtifactTypeId::function(), FUNCTION_ROOT, "FUNCTION.md");
    let registry =
        Arc::new(ArtifactAdapterRegistry::new([LifecycleAdapter { descriptor }]).unwrap());
    let root = temp_dir("invalid-envelope-inventory");
    let package = root.join(FUNCTION_ROOT).join("broken");
    fs::create_dir_all(&package).unwrap();
    let mut invalid = envelope_json();
    invalid["schema"] = serde_json::json!("agentlibre.artifact/v999");
    fs::write(
        package.join("artifact.json"),
        serde_json::to_vec(&invalid).unwrap(),
    )
    .unwrap();
    fs::write(package.join("payload.txt"), b"unchecked").unwrap();
    fs::write(package.join("FUNCTION.md"), b"entry").unwrap();
    let source = Arc::new(DirectoryArtifactSource::new(
        "workspace".parse().unwrap(),
        ArtifactSourceTier::Workspace,
        ArtifactSourceKind::Directory,
        &root,
        registry.clone(),
    ));

    assert!(matches!(
        source.candidates(&ArtifactTypeId::function()),
        Err(ArtifactError::AdapterEnvelope { .. })
    ));
    let inventory = source
        .inventory_candidates(&ArtifactTypeId::function())
        .unwrap();
    assert_eq!(inventory.len(), 1);
    assert_eq!(
        inventory[0].package_id,
        "broken".parse::<ArtifactPackageId>().unwrap()
    );
    assert_eq!(inventory[0].version, version("0.0.0-invalid"));
    assert!(matches!(
        inventory[0].discovery_error(),
        Some(ArtifactError::AdapterEnvelope { .. })
    ));

    let reference: ArtifactPackageRef = "function:broken@*".parse().unwrap();
    let resolver = ArtifactResolver::new(registry.clone(), vec![source]);
    assert!(matches!(
        resolver.resolve(&reference),
        Err(ArtifactError::AdapterEnvelope { .. })
    ));

    let frozen = Arc::new(
        StaticArtifactSource::new(
            "workspace".parse().unwrap(),
            ArtifactSourceTier::Workspace,
            ArtifactSourceKind::Directory,
            inventory
                .iter()
                .map(ArtifactCandidate::snapshot)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
        )
        .unwrap(),
    );
    assert_eq!(
        frozen
            .inventory_candidates(&ArtifactTypeId::function())
            .unwrap()
            .len(),
        1
    );
    assert!(matches!(
        frozen.candidates(&ArtifactTypeId::function()),
        Err(ArtifactError::AdapterEnvelope { .. })
    ));
    let resolver = ArtifactResolver::new(registry, vec![frozen]);
    assert!(matches!(
        resolver.resolve(&reference),
        Err(ArtifactError::AdapterEnvelope { .. })
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn fixture_extension_resolves_from_a_function_requirement() {
    let extension: ArtifactEnvelope = serde_json::from_value(serde_json::json!({
        "schema": "agentlibre.artifact/v1",
        "type": "extension",
        "id": "core.workspace",
        "version": "1.0.0",
        "payload_schema": "agentlibre.extension/v1",
        "agl": {
            "compatible": ">=1.0.0",
            "tested": ["1.0.0"]
        },
        "requires": []
    }))
    .unwrap();
    let function: ArtifactEnvelope = serde_json::from_value(serde_json::json!({
        "schema": "agentlibre.artifact/v1",
        "type": "function",
        "id": "fixture",
        "version": "1.0.0",
        "payload_schema": "agentlibre.function/v2",
        "agl": {
            "compatible": ">=1.0.0",
            "tested": ["1.0.0"]
        },
        "requires": ["extension:core.workspace@^1.0"]
    }))
    .unwrap();
    let extension_adapter = LifecycleAdapter {
        descriptor: descriptor(ArtifactTypeId::extension(), EXTENSION_ROOT, "artifact.json"),
    };
    let function_adapter = LifecycleAdapter {
        descriptor: descriptor(ArtifactTypeId::function(), FUNCTION_ROOT, "artifact.json"),
    };
    let registry =
        Arc::new(ArtifactAdapterRegistry::new([extension_adapter, function_adapter]).unwrap());
    let source_id: ArtifactSourceId = "workspace".parse().unwrap();
    let package = |type_id: ArtifactTypeId, id: &str, envelope: &ArtifactEnvelope| {
        ArtifactCandidate::new(
            type_id,
            id.parse().unwrap(),
            version("1.0.0"),
            source_id.clone(),
            ArtifactSourceTier::Workspace,
            ArtifactSourceKind::Directory,
            Arc::new(
                InMemoryPackageView::new(vec![
                    (
                        "artifact.json".parse().unwrap(),
                        serde_json::to_vec(envelope).unwrap(),
                    ),
                    ("payload.txt".parse().unwrap(), b"fixture".to_vec()),
                ])
                .unwrap(),
            ),
        )
    };
    let source = Arc::new(
        StaticArtifactSource::new(
            source_id.clone(),
            ArtifactSourceTier::Workspace,
            ArtifactSourceKind::Directory,
            vec![
                package(ArtifactTypeId::extension(), "core.workspace", &extension),
                package(ArtifactTypeId::function(), "fixture", &function),
            ],
        )
        .unwrap(),
    );
    let resolver = ArtifactResolver::new(registry, vec![source]);
    let graph = resolver
        .resolve(&"function:fixture@^1.0".parse().unwrap())
        .unwrap();
    assert!(graph.nodes.contains_key("function:fixture@1.0.0"));
    assert!(graph.nodes.contains_key("extension:core.workspace@1.0.0"));
    assert_eq!(graph.lock().unwrap().packages.len(), 2);
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

#[test]
fn config_layers_are_ordered_and_identify_override_presence() {
    let root = temp_dir("config-layers");
    fs::create_dir_all(root.join(".agl/config/functions/example")).unwrap();
    fs::write(
        root.join(".agl/config/functions/example/config.toml"),
        b"enabled = true",
    )
    .unwrap();
    let adapter = LifecycleAdapter {
        descriptor: descriptor(ArtifactTypeId::function(), FUNCTION_ROOT, "FUNCTION.md"),
    };
    let registry = Arc::new(ArtifactAdapterRegistry::new([adapter]).unwrap());
    let router = ArtifactPathRouter::new(
        &root,
        root.join("data"),
        root.join("config"),
        root.join("state"),
        root.join("cache"),
        registry,
    );
    let layers = router
        .config_layers(&ArtifactTypeId::function(), &"example".parse().unwrap())
        .unwrap();
    assert_eq!(layers.len(), 3);
    assert_eq!(layers[0].layer, ArtifactConfigLayer::PackageDefaults);
    assert!(!layers[1].present);
    assert!(layers[2].present);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn package_digest_matches_golden_vector_and_is_order_independent() {
    let first = InMemoryPackageView::new(vec![
        ("b.txt".parse().unwrap(), b"B".to_vec()),
        ("a.txt".parse().unwrap(), b"A".to_vec()),
    ])
    .unwrap();
    let second = InMemoryPackageView::new(vec![
        ("a.txt".parse().unwrap(), b"A".to_vec()),
        ("b.txt".parse().unwrap(), b"B".to_vec()),
    ])
    .unwrap();
    let left = agl_artifact::compute_package_digest(&first).unwrap();
    let right = agl_artifact::compute_package_digest(&second).unwrap();
    assert_eq!(left, right);
    assert_eq!(
        left.to_string(),
        "sha256:15ad0b9dce7df0c6839e598cc00e588881323770ebe311bbeadc0d5e01c23a25"
    );
    let forbidden = InMemoryPackageView::new(vec![(
        "artifact-lock.toml".parse().unwrap(),
        b"mutable".to_vec(),
    )])
    .unwrap();
    assert!(matches!(
        agl_artifact::compute_package_digest(&forbidden),
        Err(ArtifactError::ReservedPackageFile { .. })
    ));
}

#[test]
fn resolver_obeys_tier_precedence_compatibility_and_ambiguity() {
    let registry = Arc::new(
        ArtifactAdapterRegistry::new([LifecycleAdapter {
            descriptor: descriptor(ArtifactTypeId::function(), FUNCTION_ROOT, "FUNCTION.md"),
        }])
        .unwrap(),
    );
    let workspace = StaticArtifactSource::new(
        "workspace".parse().unwrap(),
        ArtifactSourceTier::Workspace,
        ArtifactSourceKind::Directory,
        vec![candidate_for(
            "workspace",
            ArtifactSourceTier::Workspace,
            "2.0.0",
            "function",
            "example",
            &[],
            false,
        )],
    )
    .unwrap();
    let user = StaticArtifactSource::new(
        "user".parse().unwrap(),
        ArtifactSourceTier::User,
        ArtifactSourceKind::Directory,
        vec![
            candidate_for(
                "user",
                ArtifactSourceTier::User,
                "1.2.0",
                "function",
                "example",
                &[],
                false,
            ),
            candidate_for(
                "user",
                ArtifactSourceTier::User,
                "1.5.0",
                "function",
                "example",
                &[],
                false,
            ),
        ],
    )
    .unwrap();
    let resolver =
        ArtifactResolver::new(registry, vec![Arc::new(workspace), Arc::new(user.clone())]);
    let root: ArtifactPackageRef = "function:example@^1.0".parse().unwrap();
    let graph = resolver.resolve(&root).unwrap();
    let node = graph.nodes.get(&graph.root).unwrap();
    assert_eq!(node.candidate.version, version("1.5.0"));

    let ambiguous = StaticArtifactSource::new(
        "user-two".parse().unwrap(),
        ArtifactSourceTier::User,
        ArtifactSourceKind::Directory,
        vec![candidate_for(
            "user-two",
            ArtifactSourceTier::User,
            "1.5.0",
            "function",
            "example",
            &[],
            false,
        )],
    )
    .unwrap();
    let resolver = ArtifactResolver::new(
        Arc::new(
            ArtifactAdapterRegistry::new([LifecycleAdapter {
                descriptor: descriptor(ArtifactTypeId::function(), FUNCTION_ROOT, "FUNCTION.md"),
            }])
            .unwrap(),
        ),
        vec![Arc::new(user), Arc::new(ambiguous)],
    );
    assert!(matches!(
        resolver.resolve(&root),
        Err(ArtifactError::AmbiguousCandidate { .. })
    ));
}

#[test]
fn explicit_sources_do_not_fall_through() {
    let registry = Arc::new(
        ArtifactAdapterRegistry::new([LifecycleAdapter {
            descriptor: descriptor(ArtifactTypeId::function(), FUNCTION_ROOT, "FUNCTION.md"),
        }])
        .unwrap(),
    );
    let explicit = StaticArtifactSource::new(
        "explicit".parse().unwrap(),
        ArtifactSourceTier::Explicit,
        ArtifactSourceKind::Directory,
        vec![candidate_for(
            "explicit",
            ArtifactSourceTier::Explicit,
            "2.0.0",
            "function",
            "example",
            &[],
            false,
        )],
    )
    .unwrap();
    let user = StaticArtifactSource::new(
        "user".parse().unwrap(),
        ArtifactSourceTier::User,
        ArtifactSourceKind::Directory,
        vec![candidate_for(
            "user",
            ArtifactSourceTier::User,
            "1.0.0",
            "function",
            "example",
            &[],
            false,
        )],
    )
    .unwrap();
    let resolver = ArtifactResolver::new(registry, vec![Arc::new(explicit), Arc::new(user)]);
    let root: ArtifactPackageRef = "function:example@^1.0".parse().unwrap();
    assert!(matches!(
        resolver.resolve(&root),
        Err(ArtifactError::IncompatibleVersion { .. })
    ));
}

#[test]
fn root_scoped_explicit_source_resolves_dependencies_from_ordinary_tiers() {
    let registry = Arc::new(
        ArtifactAdapterRegistry::new([
            LifecycleAdapter {
                descriptor: descriptor(ArtifactTypeId::function(), FUNCTION_ROOT, "FUNCTION.md"),
            },
            LifecycleAdapter {
                descriptor: descriptor(ArtifactTypeId::skill(), SKILL_ROOT, "SKILL.md"),
            },
        ])
        .unwrap(),
    );
    let explicit_id: ArtifactSourceId = "explicit".parse().unwrap();
    let explicit = StaticArtifactSource::new(
        explicit_id.clone(),
        ArtifactSourceTier::Explicit,
        ArtifactSourceKind::Directory,
        vec![candidate_for(
            "explicit",
            ArtifactSourceTier::Explicit,
            "1.0.0",
            "function",
            "example",
            &["skill:dependency@^1.0"],
            true,
        )],
    )
    .unwrap();
    let user = StaticArtifactSource::new(
        "user".parse().unwrap(),
        ArtifactSourceTier::User,
        ArtifactSourceKind::Directory,
        vec![
            candidate_for(
                "user",
                ArtifactSourceTier::User,
                "1.0.0",
                "function",
                "example",
                &[],
                true,
            ),
            candidate_for(
                "user",
                ArtifactSourceTier::User,
                "1.0.0",
                "skill",
                "dependency",
                &[],
                true,
            ),
        ],
    )
    .unwrap();
    let resolver = ArtifactResolver::new(registry, vec![Arc::new(explicit), Arc::new(user)]);
    let root: ArtifactPackageRef = "function:example@^1.0".parse().unwrap();

    let graph = resolver
        .resolve_and_validate_with_explicit_root(&root, &explicit_id, None)
        .unwrap();

    assert_eq!(
        graph.nodes[&graph.root].candidate.source_id.as_str(),
        "explicit"
    );
    let dependency = graph
        .nodes
        .values()
        .find(|node| node.candidate.type_id == ArtifactTypeId::skill())
        .unwrap();
    assert_eq!(dependency.candidate.source_id.as_str(), "user");
}

#[test]
fn resolved_candidates_retain_the_admitted_package_bytes() {
    let root = temp_dir("candidate-snapshot");
    fs::write(
        root.join("artifact.json"),
        serde_json::to_vec(&envelope_for("function", "example", "1.0.0", &[])).unwrap(),
    )
    .unwrap();
    fs::write(root.join("payload.txt"), "before").unwrap();
    let candidate = ArtifactCandidate::new(
        ArtifactTypeId::function(),
        "example".parse().unwrap(),
        version("1.0.0"),
        "workspace".parse().unwrap(),
        ArtifactSourceTier::Workspace,
        ArtifactSourceKind::Directory,
        Arc::new(DirectoryPackageView::new(&root).unwrap()),
    );
    let source = StaticArtifactSource::new(
        "workspace".parse().unwrap(),
        ArtifactSourceTier::Workspace,
        ArtifactSourceKind::Directory,
        vec![candidate],
    )
    .unwrap();
    let registry = Arc::new(
        ArtifactAdapterRegistry::new([LifecycleAdapter {
            descriptor: descriptor(ArtifactTypeId::function(), FUNCTION_ROOT, "FUNCTION.md"),
        }])
        .unwrap(),
    );
    let resolver = ArtifactResolver::new(registry.clone(), vec![Arc::new(source)]);
    let reference: ArtifactPackageRef = "function:example@1.0.0".parse().unwrap();
    let graph = resolver.resolve_and_validate(&reference, None).unwrap();

    fs::write(root.join("payload.txt"), "after").unwrap();
    let node = &graph.nodes[&graph.root];
    let payload = registry
        .lookup(&node.candidate.type_id)
        .unwrap()
        .validate_payload(node.candidate.view(), &node.envelope)
        .unwrap()
        .downcast::<String>()
        .unwrap();
    assert_eq!(*payload, "before");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn graph_merges_dependencies_and_lock_is_deterministic() {
    let function = ArtifactTypeId::function();
    let skill = ArtifactTypeId::skill();
    let registry = Arc::new(
        ArtifactAdapterRegistry::new([
            LifecycleAdapter {
                descriptor: descriptor(function.clone(), FUNCTION_ROOT, "FUNCTION.md"),
            },
            LifecycleAdapter {
                descriptor: descriptor(skill.clone(), SKILL_ROOT, "SKILL.md"),
            },
        ])
        .unwrap(),
    );
    let root_candidate = candidate_for(
        "workspace",
        ArtifactSourceTier::Workspace,
        "1.0.0",
        "function",
        "example",
        &["skill:workflow@^1.0"],
        true,
    );
    let skill_candidate = candidate_for(
        "workspace",
        ArtifactSourceTier::Workspace,
        "1.2.0",
        "skill",
        "workflow",
        &[],
        true,
    );
    let source = StaticArtifactSource::new(
        "workspace".parse().unwrap(),
        ArtifactSourceTier::Workspace,
        ArtifactSourceKind::Directory,
        vec![root_candidate, skill_candidate],
    )
    .unwrap();
    let resolver = ArtifactResolver::new(registry.clone(), vec![Arc::new(source)]);
    let root: ArtifactPackageRef = "function:example@^1.0".parse().unwrap();
    let graph = resolver.resolve(&root).unwrap();
    assert_eq!(graph.nodes.len(), 2);
    assert!(graph.validate_payloads(&registry).is_ok());
    let lock = graph.lock().unwrap();
    let first = lock.to_toml().unwrap();
    let second = graph.lock().unwrap().to_toml().unwrap();
    assert_eq!(first, second);
    assert_eq!(ArtifactLock::from_toml(&first).unwrap(), lock);
    graph.verify_lock(&lock).unwrap();
}

#[test]
fn git_provenance_is_locked_and_drift_is_distinct_from_package_digest() {
    let registry = Arc::new(
        ArtifactAdapterRegistry::new([LifecycleAdapter {
            descriptor: descriptor(ArtifactTypeId::function(), FUNCTION_ROOT, "FUNCTION.md"),
        }])
        .unwrap(),
    );
    let mut candidate = candidate_for(
        "git-source",
        ArtifactSourceTier::Workspace,
        "1.0.0",
        "function",
        "example",
        &[],
        true,
    );
    candidate.kind = ArtifactSourceKind::Git;
    candidate = candidate.with_source_provenance("a".repeat(40), "b".repeat(40));
    let source = StaticArtifactSource::new(
        "git-source".parse().unwrap(),
        ArtifactSourceTier::Workspace,
        ArtifactSourceKind::Git,
        vec![candidate],
    )
    .unwrap();
    let graph = ArtifactResolver::new(registry, vec![Arc::new(source)])
        .resolve(&"function:example@^1.0".parse().unwrap())
        .unwrap();
    let mut lock = graph.lock().unwrap();
    let package = lock.packages.values_mut().next().unwrap();
    assert_eq!(
        package.source_revision.as_deref(),
        Some("a".repeat(40).as_str())
    );
    package.source_revision = Some("c".repeat(40));

    let error = graph.verify_lock(&lock).unwrap_err();
    assert_eq!(error.code(), "source_drift");
}

#[test]
fn resolver_reports_missing_dependencies_and_full_cycles() {
    let registry = Arc::new(
        ArtifactAdapterRegistry::new([
            LifecycleAdapter {
                descriptor: descriptor(ArtifactTypeId::function(), FUNCTION_ROOT, "FUNCTION.md"),
            },
            LifecycleAdapter {
                descriptor: descriptor(ArtifactTypeId::skill(), SKILL_ROOT, "SKILL.md"),
            },
        ])
        .unwrap(),
    );
    let missing = StaticArtifactSource::new(
        "workspace".parse().unwrap(),
        ArtifactSourceTier::Workspace,
        ArtifactSourceKind::Directory,
        vec![candidate_for(
            "workspace",
            ArtifactSourceTier::Workspace,
            "1.0.0",
            "function",
            "example",
            &["skill:missing@^1.0"],
            false,
        )],
    )
    .unwrap();
    let resolver = ArtifactResolver::new(registry.clone(), vec![Arc::new(missing)]);
    let root: ArtifactPackageRef = "function:example@^1.0".parse().unwrap();
    assert!(matches!(
        resolver.resolve(&root),
        Err(ArtifactError::MissingDependency { .. })
    ));

    let cycle = StaticArtifactSource::new(
        "workspace".parse().unwrap(),
        ArtifactSourceTier::Workspace,
        ArtifactSourceKind::Directory,
        vec![
            candidate_for(
                "workspace",
                ArtifactSourceTier::Workspace,
                "1.0.0",
                "function",
                "example",
                &["skill:workflow@^1.0"],
                false,
            ),
            candidate_for(
                "workspace",
                ArtifactSourceTier::Workspace,
                "1.0.0",
                "skill",
                "workflow",
                &["function:example@^1.0"],
                false,
            ),
        ],
    )
    .unwrap();
    let resolver = ArtifactResolver::new(registry, vec![Arc::new(cycle)]);
    let error = resolver.resolve(&root).unwrap_err();
    assert!(matches!(error, ArtifactError::DependencyCycle { .. }));
    assert_eq!(error.code(), "dependency_cycle");
}

#[test]
fn failed_lock_refresh_preserves_previous_bytes() {
    let path = temp_dir("lock").join("artifact-lock.toml");
    let lock = ArtifactLock::new(Default::default(), Default::default()).unwrap();
    lock.write_atomic(&path).unwrap();
    let before = fs::read(&path).unwrap();
    let mut invalid = lock;
    invalid.version = 1;
    assert!(invalid.write_atomic(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn workspace_v2_round_trips_and_rejects_v1_or_package_metadata() {
    let manifest = WorkspaceManifest {
        version: 2,
        default_function: "function:example@^1.0".parse().unwrap(),
        sources: Default::default(),
        components: [(
            "tasks".to_owned(),
            WorkspaceComponent {
                kind: WorkspaceComponentKind::Git,
                path: ".agl/tasks".into(),
                url: Some("https://example.invalid/tasks.git".to_owned()),
                rev: Some("main".to_owned()),
                commit: None,
                tree: None,
                required: true,
                access: agl_artifact::ArtifactAccess::ReadWrite,
                validation: Some("agl.task_spec.v1".to_owned()),
                create: Vec::new(),
            },
        )]
        .into_iter()
        .collect(),
        policy: Default::default(),
        config: Default::default(),
    };
    let encoded = manifest.to_toml().unwrap();
    assert_eq!(WorkspaceManifest::from_toml(&encoded).unwrap(), manifest);
    assert!(WorkspaceManifest::from_toml("version = 1\nprofile = \"repo-workflow\"\n").is_err());
    assert!(
        WorkspaceManifest::from_toml(
            "version = 2\ndefault_function = \"function:example@^1\"\npackage = \"copied\"\n"
        )
        .is_err()
    );
}

#[test]
fn workspace_v2_rejects_source_path_escape() {
    let manifest = WorkspaceManifest {
        version: 2,
        default_function: "function:example@^1.0".parse().unwrap(),
        sources: [(
            "local".to_owned(),
            ArtifactSourceDeclaration {
                kind: ArtifactSourceKind::Directory,
                path: Some("../outside".into()),
                url: None,
                rev: None,
            },
        )]
        .into_iter()
        .collect(),
        components: Default::default(),
        policy: Default::default(),
        config: Default::default(),
    };
    assert!(manifest.to_toml().is_err());
}

fn candidate_for(
    source_id: &str,
    tier: ArtifactSourceTier,
    version: &str,
    type_name: &str,
    id: &str,
    requires: &[&str],
    with_payload: bool,
) -> ArtifactCandidate {
    let envelope = envelope_for(type_name, id, version, requires);
    let mut files = vec![
        (
            "artifact.json".parse().unwrap(),
            serde_json::to_vec(&envelope).unwrap(),
        ),
        (
            if type_name == "skill" {
                "SKILL.md".parse().unwrap()
            } else {
                "FUNCTION.md".parse().unwrap()
            },
            b"entry".to_vec(),
        ),
    ];
    if with_payload {
        files.push(("payload.txt".parse().unwrap(), b"checked".to_vec()));
    }
    ArtifactCandidate::new(
        type_name.parse().unwrap(),
        id.parse().unwrap(),
        version.parse().unwrap(),
        source_id.parse().unwrap(),
        tier,
        ArtifactSourceKind::Directory,
        Arc::new(InMemoryPackageView::new(files).unwrap()),
    )
}

fn envelope_for(type_name: &str, id: &str, version: &str, requires: &[&str]) -> ArtifactEnvelope {
    let schema = format!("agentlibre.{type_name}/v2");
    serde_json::from_value(serde_json::json!({
        "schema": "agentlibre.artifact/v1",
        "type": type_name,
        "id": id,
        "version": version,
        "payload_schema": schema,
        "agl": {
            "compatible": ">=1.0.0, <2.0.0",
            "tested": ["1.0.0"]
        },
        "requires": requires
    }))
    .unwrap()
}

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("agl-artifact-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}
