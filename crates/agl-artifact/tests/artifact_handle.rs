use agl_artifact::{ArtifactHandle, FixtureArtifactTree};
use agl_kernel::{ArtifactAccess, ArtifactDeclaration, ArtifactId, ArtifactKindId};

fn declaration() -> ArtifactDeclaration {
    ArtifactDeclaration::new(
        ArtifactId::new("example.workspace:data").unwrap(),
        ArtifactKindId::new("agl.file-tree").unwrap(),
        [ArtifactAccess::ReadTree],
    )
    .unwrap()
}

// AGL171-022 and AGL171-023.
#[test]
fn fixture_handle_is_opaque_and_confined_to_the_admitted_tree() {
    let tree = FixtureArtifactTree::new([
        ("README.md", b"inside".as_slice()),
        ("nested/data.txt", b"data".as_slice()),
    ])
    .unwrap();
    let handle = ArtifactHandle::fixture(declaration(), tree).unwrap();

    assert_eq!(handle.id().as_str(), "example.workspace:data");
    assert_eq!(
        handle.read("nested/data.txt".parse().unwrap()).unwrap(),
        b"data"
    );
    for rejected in ["../outside", "/absolute", "nested/../../outside"] {
        assert!(rejected.parse().and_then(|path| handle.read(path)).is_err());
    }

    let debug = format!("{handle:?}");
    assert!(!debug.contains("/tmp/"));
    assert!(!debug.contains("checkout"));
}

// AGL171-022. ArtifactHandle is only for file-tree Artifacts; a process or
// remote API cannot be smuggled through this boundary.
#[test]
fn fixture_handle_rejects_non_file_artifact_kinds_and_excess_access() {
    let remote = ArtifactDeclaration::new(
        ArtifactId::new("example.remote:api").unwrap(),
        ArtifactKindId::new("vendor.remote-api").unwrap(),
        [ArtifactAccess::ReadTree],
    )
    .unwrap();
    let tree = FixtureArtifactTree::new(std::iter::empty::<(&str, &[u8])>()).unwrap();
    assert!(ArtifactHandle::fixture(remote, tree).is_err());

    let handle = ArtifactHandle::fixture(
        declaration(),
        FixtureArtifactTree::new(std::iter::empty::<(&str, &[u8])>()).unwrap(),
    )
    .unwrap();
    assert!(handle.require_access(ArtifactAccess::MutateTree).is_err());
}
