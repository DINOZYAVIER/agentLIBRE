use agl_artifact::{ArtifactHandle, ArtifactHandleError, ArtifactPath, FixtureArtifactTree};
use agl_kernel::{
    ArtifactAccess, ArtifactDeclaration, ArtifactId, ArtifactKindId, ExtensionDescriptor,
    ExtensionId, ExtensionSource, ExtensionTrust,
};

fn declaration(access: ArtifactAccess) -> ArtifactDeclaration {
    ArtifactDeclaration::new(
        ArtifactId::new("example.data:tree").unwrap(),
        ArtifactKindId::new("agentlibre.file-tree").unwrap(),
        [access],
    )
    .unwrap()
}

fn authored_digest(declaration: ArtifactDeclaration) -> agl_kernel::DeclarationDigest {
    ExtensionDescriptor::new(
        ExtensionId::new("example.data").unwrap(),
        "Example data",
        "1.0.0",
        ExtensionSource::TestFixture,
        ExtensionTrust::TrustedRegistered,
    )
    .unwrap()
    .with_artifact(declaration)
    .digest()
}

// AGL172-018, AGL172-021 and AGL172-045.
#[test]
fn handle_rejects_absolute_parent_and_symlink_escape_before_io() {
    let tree = FixtureArtifactTree::new([
        ("safe/readme.md", b"safe".as_slice()),
        ("link-out", b"fixture-symlink:../outside".as_slice()),
    ])
    .unwrap();
    let handle = ArtifactHandle::fixture(declaration(ArtifactAccess::ReadTree), tree).unwrap();

    for rejected in ["/absolute", "../escape", "safe/../../escape"] {
        assert!(ArtifactPath::new(rejected).is_err());
    }
    assert!(matches!(
        handle.read("link-out/secret".parse().unwrap()),
        Err(ArtifactHandleError::SymlinkEscape { .. })
    ));
}

// AGL172-022, AGL172-045 and AGL172-057.
#[test]
fn read_tree_cannot_mutate_and_mutate_tree_returns_exact_dirty_evidence() {
    let tree = FixtureArtifactTree::new([("notes/a.md", b"old".as_slice())]).unwrap();
    let read_only =
        ArtifactHandle::fixture(declaration(ArtifactAccess::ReadTree), tree.clone()).unwrap();
    assert!(matches!(
        read_only.write("notes/a.md".parse().unwrap(), b"new"),
        Err(ArtifactHandleError::AccessDenied { .. })
    ));
    assert!(matches!(
        read_only.remove("notes/a.md".parse().unwrap()),
        Err(ArtifactHandleError::AccessDenied { .. })
    ));

    let mutable = ArtifactHandle::fixture(declaration(ArtifactAccess::MutateTree), tree).unwrap();
    let update = mutable
        .write("notes/a.md".parse().unwrap(), b"new")
        .unwrap();
    let create = mutable
        .write("notes/new.md".parse().unwrap(), b"created")
        .unwrap();
    let delete = mutable.remove("notes/a.md".parse().unwrap()).unwrap();

    assert_eq!(update.path().as_str(), "notes/a.md");
    assert_eq!(create.path().as_str(), "notes/new.md");
    assert_eq!(delete.path().as_str(), "notes/a.md");
    assert!(update.before().is_some() && update.after().is_some());
    assert!(create.before().is_none() && create.after().is_some());
    assert!(delete.before().is_some() && delete.after().is_none());
}

// AGL172-013, AGL172-019 and AGL172-046.
#[test]
fn concrete_binding_identity_does_not_change_kernel_declaration_digest() {
    let authored = declaration(ArtifactAccess::ReadTree);
    let original_digest = authored_digest(authored.clone());
    let first = agl_artifact::ArtifactBinding::fixture(
        authored.id.clone(),
        ".agl/tasks",
        "https://example.invalid/tasks.git",
        "1111111111111111111111111111111111111111",
    )
    .unwrap();
    let second = agl_artifact::ArtifactBinding::fixture(
        authored.id.clone(),
        ".agl/specs",
        "https://mirror.invalid/tasks.git",
        "2222222222222222222222222222222222222222",
    )
    .unwrap();

    assert_ne!(first.submodule_path(), second.submodule_path());
    assert_eq!(authored_digest(authored), original_digest);
    assert_eq!(first.artifact_id(), second.artifact_id());
}

// AGL172-020, AGL172-021 and AGL172-040.
#[test]
fn only_unique_declared_file_tree_bindings_can_create_handles() {
    let declaration = declaration(ArtifactAccess::ReadTree);
    let cache = agl_artifact::ArtifactBinding::fixture(
        declaration.id.clone(),
        ".agl/cache",
        "local-state://cache",
        "0000000000000000000000000000000000000000",
    )
    .unwrap();
    assert!(matches!(
        ArtifactHandle::bind(declaration.clone(), cache),
        Err(ArtifactHandleError::UnverifiedBinding { .. })
    ));

    let non_file = ArtifactDeclaration::new(
        ArtifactId::new("example.data:database").unwrap(),
        ArtifactKindId::new("vendor.database").unwrap(),
        [ArtifactAccess::ReadTree],
    )
    .unwrap();
    assert!(matches!(
        ArtifactHandle::fixture(non_file, FixtureArtifactTree::default()),
        Err(ArtifactHandleError::UnsupportedKind { .. })
    ));
}
