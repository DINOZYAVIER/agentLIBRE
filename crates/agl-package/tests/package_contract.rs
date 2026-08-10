use std::sync::Arc;

use agl_package::{
    ErasedPackagePayload, InMemoryPackageView, PackageAdapter, PackageAdapterDescriptor,
    PackageAdapterRegistry, PackageCandidate, PackageEnvelope, PackageError, PackageId, PackageRef,
    PackageRelativePath, PackageResolver, PackageSourceId, PackageSourceKind, PackageSourceTier,
    PackageTreeDigest, PackageTypeId, PackageVersion, PackageView, StaticPackageSource,
    compute_package_digest,
};

struct FunctionAdapter {
    descriptor: PackageAdapterDescriptor,
}

impl PackageAdapter for FunctionAdapter {
    fn descriptor(&self) -> &PackageAdapterDescriptor {
        &self.descriptor
    }

    fn extract_envelope(&self, package: &dyn PackageView) -> Result<PackageEnvelope, PackageError> {
        let bytes = package.read_file(&"artifact.json".parse().unwrap())?;
        serde_json::from_slice(&bytes).map_err(|error| PackageError::AdapterEnvelope {
            type_id: self.descriptor.type_id.to_string(),
            reason: error.to_string(),
        })
    }

    fn validate_payload(
        &self,
        _package: &dyn PackageView,
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

// AGL171-002, AGL171-006 and AGL171-014. AGL-172 performs the later
// Artifact* -> Package* vocabulary/wire cutover; AGL-171 moves ownership.
#[test]
fn package_view_is_immutable_sorted_and_digest_is_exact_tree_material() {
    let first = view(&[("z.txt", b"z"), ("nested/a.txt", b"a")]);
    let reordered = view(&[("nested/a.txt", b"a"), ("z.txt", b"z")]);
    let readme_changed = view(&[
        ("nested/a.txt", b"a"),
        ("z.txt", b"z"),
        ("README.md", b"layout-only change"),
    ]);

    assert_eq!(
        first.files().unwrap(),
        vec!["nested/a.txt".parse().unwrap(), "z.txt".parse().unwrap()]
    );
    assert_eq!(
        compute_package_digest(&first).unwrap(),
        compute_package_digest(&reordered).unwrap()
    );
    assert_ne!(
        compute_package_digest(&first).unwrap(),
        compute_package_digest(&readme_changed).unwrap()
    );
}

// AGL171-002 and AGL171-013.
#[test]
fn package_relative_paths_fail_with_the_exact_value() {
    for rejected in ["", "/absolute", "../escape", "nested/../../escape", "./dot"] {
        assert!(matches!(
            PackageRelativePath::new(rejected),
            Err(PackageError::InvalidRelativePath { value }) if value == rejected
        ));
    }
}

// AGL171-002 and AGL171-013.
#[test]
fn resolver_source_and_lock_are_owned_by_agl_package() {
    let type_id = PackageTypeId::function();
    let package_id: PackageId = "example/echo".parse().unwrap();
    let source_id = PackageSourceId::new("fixture").unwrap();
    let package = view(&[
        (
            "artifact.json",
            br#"{"schema":"agentlibre.package/v1","type":"function","id":"example/echo","version":"1.0.0","payload_schema":"agentlibre.function/v2","agl":{"compatible":"^1","tested":["1.0.0"]},"requires":[]}"#,
        ),
        ("FUNCTION.md", b"fixture"),
    ]);
    let candidate = PackageCandidate::new(
        type_id.clone(),
        package_id.clone(),
        "1.0.0".parse::<PackageVersion>().unwrap(),
        source_id.clone(),
        PackageSourceTier::Explicit,
        PackageSourceKind::Embedded,
        Arc::new(package),
    );
    let source = StaticPackageSource::new(
        source_id,
        PackageSourceTier::Explicit,
        PackageSourceKind::Embedded,
        vec![candidate],
    )
    .unwrap();
    let resolver = PackageResolver::new(
        Arc::new(
            PackageAdapterRegistry::new([FunctionAdapter {
                descriptor: PackageAdapterDescriptor::new(
                    PackageTypeId::function(),
                    "functions",
                    "artifact.json".parse().unwrap(),
                )
                .unwrap(),
            }])
            .unwrap(),
        ),
        vec![Arc::new(source)],
    );
    let reference = PackageRef::new(type_id, package_id, "^1".parse().unwrap());
    let resolved = resolver.resolve(&reference).unwrap();
    let lock = resolved.lock().unwrap();

    assert_eq!(resolved.nodes.len(), 1);
    assert_eq!(lock.packages.len(), 1);
    assert!(matches!(
        resolver.resolve(&"function:missing/package@*".parse().unwrap()),
        Err(PackageError::PackageNotFound { .. })
    ));
}

// AGL171-005 and AGL171-006. Two Extensions in one Rust bundle still have
// independent package roots and package-tree identities.
#[test]
fn sibling_extension_packages_have_independent_tree_digests() {
    let alpha = view(&[("extension-root.json", b"alpha"), ("tools/a.json", b"a")]);
    let beta = view(&[("extension-root.json", b"beta"), ("tools/b.json", b"b")]);
    let beta_changed = view(&[
        ("extension-root.json", b"beta"),
        ("tools/b.json", b"changed"),
    ]);

    let alpha_digest: PackageTreeDigest = compute_package_digest(&alpha).unwrap();
    assert_ne!(alpha_digest, compute_package_digest(&beta).unwrap());
    assert_eq!(alpha_digest, compute_package_digest(&alpha).unwrap());
    assert_ne!(
        compute_package_digest(&beta).unwrap(),
        compute_package_digest(&beta_changed).unwrap()
    );
}
