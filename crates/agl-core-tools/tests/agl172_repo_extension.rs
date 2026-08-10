use agl_kernel::{ArtifactAccess, ArtifactTargetSelector, AuthorityClass};

// AGL172-023, AGL172-039 and AGL172-061.
#[test]
fn core_repo_declares_tasks_artifact_and_exact_two_phase_commit_effects() {
    let definition = agl_core_tools::repo_extension_factory().definition();
    let descriptor = definition.descriptor();
    let tasks = descriptor
        .artifacts
        .iter()
        .find(|artifact| artifact.id.as_str() == "core.repo:tasks")
        .expect("core.repo declares the fixed tasks Artifact");
    assert_eq!(tasks.kind.as_str(), "agentlibre.task-specs");
    assert_eq!(
        tasks.access,
        [ArtifactAccess::ReadTree].into_iter().collect()
    );

    for effect_id in ["agl:artifact.repository", "agl:repo.gitlink"] {
        let effect = descriptor
            .effects
            .iter()
            .find(|effect| effect.id.as_str() == effect_id)
            .unwrap_or_else(|| panic!("missing effect {effect_id}"));
        assert_eq!(
            effect.authority_class,
            AuthorityClass::RepositoryMutation.as_str()
        );
    }

    let commit = descriptor
        .tools
        .iter()
        .find(|tool| tool.id.as_str() == "core.repo:artifact.commit")
        .expect("core.repo owns one generic Artifact commit Tool");
    assert_eq!(
        commit
            .state_effects
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["agl:artifact.repository", "agl:repo.gitlink"]
    );
    assert_eq!(commit.artifact_links.len(), 1);
    assert!(matches!(
        &commit.artifact_links[0].selector,
        ArtifactTargetSelector::FromArgument { pointer, access }
            if pointer == "/artifact_id" && access == &ArtifactAccess::MutateTree
    ));
}

// AGL172-010 and AGL172-052.
#[test]
fn core_repo_catalog_has_no_profile_component_or_legacy_status_tools() {
    let definition = agl_core_tools::repo_extension_factory().definition();
    let ids = definition
        .descriptor()
        .tools
        .iter()
        .map(|tool| tool.id.as_str())
        .collect::<Vec<_>>();
    for forbidden in [
        "core.repo:status",
        "core.repo:import_profile",
        "core.repo:export_profile",
        "core.repo:component.status",
        "core.repo:component.sync",
        "core.repo:component.lock",
    ] {
        assert!(
            !ids.contains(&forbidden),
            "obsolete Tool remains: {forbidden}"
        );
    }
}

// AGL172-062.
#[test]
fn verify_tasks_handler_is_bound_to_the_fixed_read_only_tasks_handle() {
    let definition = agl_core_tools::repo_extension_factory().definition();
    let verify = definition
        .descriptor()
        .tools
        .iter()
        .find(|tool| tool.id.as_str() == "core.repo:tasks.verify")
        .expect("task validation Tool exists");
    let encoded = serde_json::to_value(&verify.input_schema).unwrap();
    assert!(encoded.pointer("/properties/path").is_none());
    assert!(encoded.pointer("/properties/artifact_id").is_none());
    assert_eq!(verify.artifact_links.len(), 1);
    assert!(matches!(
        &verify.artifact_links[0].selector,
        ArtifactTargetSelector::Fixed(id) if id.as_str() == "core.repo:tasks"
    ));
    assert_eq!(verify.artifact_links[0].access, ArtifactAccess::ReadTree);
}
