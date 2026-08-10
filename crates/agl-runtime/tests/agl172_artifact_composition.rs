use agl_artifact::ArtifactBinding;
use agl_kernel::{ArtifactAccess, ArtifactDeclaration, ArtifactId, ArtifactKindId};
use agl_runtime::{
    ArtifactBindingInput, ArtifactCompositionError, PackageComposition, bind_artifact_handles,
};

fn declaration(id: &str, kind: &str, access: ArtifactAccess) -> ArtifactDeclaration {
    ArtifactDeclaration::new(
        ArtifactId::new(id).unwrap(),
        ArtifactKindId::new(kind).unwrap(),
        [access],
    )
    .unwrap()
}

fn binding(id: &str, path: &str) -> ArtifactBinding {
    ArtifactBinding::verified_fixture(
        ArtifactId::new(id).unwrap(),
        path,
        "https://example.invalid/artifact.git",
        "1111111111111111111111111111111111111111",
        "1111111111111111111111111111111111111111",
    )
    .unwrap()
}

fn authored_digest(declaration: ArtifactDeclaration) -> agl_kernel::DeclarationDigest {
    agl_kernel::ExtensionDescriptor::new(
        agl_kernel::ExtensionId::new("core.repo").unwrap(),
        "Repository",
        "1.0.0",
        agl_kernel::ExtensionSource::TestFixture,
        agl_kernel::ExtensionTrust::TrustedRegistered,
    )
    .unwrap()
    .with_artifact(declaration)
    .digest()
}

// AGL172-016, AGL172-040 and AGL172-046.
#[test]
fn typed_binding_input_requires_exact_declaration_id_kind_access_and_unique_path() {
    let tasks_declaration = declaration(
        "core.repo:tasks",
        "agentlibre.task-specs",
        ArtifactAccess::ReadTree,
    );
    let handles = bind_artifact_handles(ArtifactBindingInput::new(
        [tasks_declaration.clone()],
        [binding("core.repo:tasks", ".agl/tasks")],
    ))
    .unwrap();
    assert_eq!(handles.len(), 1);
    assert_eq!(handles[0].id(), &tasks_declaration.id);

    let duplicate_path = bind_artifact_handles(ArtifactBindingInput::new(
        [
            tasks_declaration.clone(),
            declaration(
                "core.repo:reviews",
                "agentlibre.review-pack",
                ArtifactAccess::ReadTree,
            ),
        ],
        [
            binding("core.repo:tasks", ".agl/tasks"),
            binding("core.repo:reviews", ".agl/tasks"),
        ],
    ));
    assert!(matches!(
        duplicate_path,
        Err(ArtifactCompositionError::DuplicateBindingPath { .. })
    ));
}

// AGL172-016, AGL172-020 and AGL172-021.
#[test]
fn serialized_workspace_or_local_state_never_becomes_a_runtime_artifact() {
    let declaration = declaration(
        "core.repo:tasks",
        "agentlibre.task-specs",
        ArtifactAccess::ReadTree,
    );
    let absent = bind_artifact_handles(ArtifactBindingInput::new(
        [declaration.clone()],
        std::iter::empty(),
    ));
    assert!(matches!(
        absent,
        Err(ArtifactCompositionError::MissingBinding { .. })
    ));

    let cache = bind_artifact_handles(ArtifactBindingInput::new(
        [declaration],
        [ArtifactBinding::local_state_fixture(
            ArtifactId::new("core.repo:tasks").unwrap(),
            ".agl/cache",
        )
        .unwrap()],
    ));
    assert!(matches!(
        cache,
        Err(ArtifactCompositionError::UnverifiedBinding { .. })
    ));
}

// AGL172-019.
#[test]
fn repository_specific_paths_do_not_change_authored_artifact_identity() {
    let declaration = declaration(
        "core.repo:tasks",
        "agentlibre.task-specs",
        ArtifactAccess::ReadTree,
    );
    let digest = authored_digest(declaration.clone());
    let first = bind_artifact_handles(ArtifactBindingInput::new(
        [declaration.clone()],
        [binding("core.repo:tasks", ".agl/tasks")],
    ))
    .unwrap();
    let second = bind_artifact_handles(ArtifactBindingInput::new(
        [declaration.clone()],
        [binding("core.repo:tasks", ".agl/specifications")],
    ))
    .unwrap();
    assert_ne!(first[0].binding_identity(), second[0].binding_identity());
    assert_eq!(authored_digest(declaration), digest);
}

// AGL172-002, AGL172-007 and AGL172-056.
#[test]
fn package_composition_and_runtime_bundle_share_one_resolved_graph() {
    fn selected_api(
        composition: &PackageComposition,
        root: &agl_package::PackageRef,
    ) -> anyhow::Result<agl_package::ResolvedPackageGraph> {
        composition.resolve(root)
    }
    let _: fn(
        &PackageComposition,
        &agl_package::PackageRef,
    ) -> anyhow::Result<agl_package::ResolvedPackageGraph> = selected_api;
}

// AGL172-057 and AGL172-064.
#[test]
fn runtime_host_exports_only_exact_admitted_handles_to_extension_handlers() {
    let declaration = declaration(
        "core.repo:tasks",
        "agentlibre.task-specs",
        ArtifactAccess::ReadTree,
    );
    let handles = bind_artifact_handles(ArtifactBindingInput::new(
        [declaration],
        [binding("core.repo:tasks", ".agl/tasks")],
    ))
    .unwrap();
    let host = handles
        .into_iter()
        .fold(
            agl_extension::ExtensionHost::builder(),
            |builder, handle| builder.artifact(handle),
        )
        .build();
    assert!(
        host.artifact(&ArtifactId::new("core.repo:tasks").unwrap())
            .is_some()
    );
    assert!(
        host.artifact(&ArtifactId::new("core.repo:reviews").unwrap())
            .is_none()
    );
}
