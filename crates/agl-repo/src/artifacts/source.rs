use std::path::Path;

use crate::{
    ArtifactSource, ArtifactSourceKind, ArtifactSourceState, ArtifactSourceStatus, ComponentKind,
    WorkspaceComponent, WorkspaceManifest, component_status,
};

pub(super) fn artifact_source_statuses(
    workspace_root: &Path,
    manifest: &WorkspaceManifest,
) -> Vec<ArtifactSourceStatus> {
    manifest
        .artifact_sources
        .iter()
        .map(|(source_id, source)| artifact_source_status(workspace_root, source_id, source))
        .collect()
}

fn artifact_source_status(
    workspace_root: &Path,
    source_id: &str,
    source: &ArtifactSource,
) -> ArtifactSourceStatus {
    let component_kind = match source.kind {
        ArtifactSourceKind::Git => ComponentKind::Git,
        ArtifactSourceKind::Submodule => ComponentKind::Submodule,
        ArtifactSourceKind::Local | ArtifactSourceKind::Compatibility => ComponentKind::Local,
        ArtifactSourceKind::Generated => ComponentKind::Generated,
        ArtifactSourceKind::Ignored => ComponentKind::Ignored,
    };
    let component = WorkspaceComponent {
        path: source.path.clone(),
        kind: component_kind,
        url: source.url.clone(),
        rev: source.rev.clone(),
        commit: source.commit.clone(),
        tree: source.tree.clone(),
        lock: None,
    };
    let component_status = component_status(workspace_root, source_id, &component);
    let mut warnings = component_status.warnings.clone();
    let mut errors = component_status.errors.clone();

    if !source.required && errors.iter().any(|error| error == "missing") {
        errors.retain(|error| error != "missing");
        warnings.push("missing_optional".to_string());
    }

    let state = if component_status.exists {
        if !errors.is_empty() {
            ArtifactSourceState::Invalid
        } else if !warnings.is_empty() {
            ArtifactSourceState::Warning
        } else {
            ArtifactSourceState::Ok
        }
    } else if source.required {
        ArtifactSourceState::Missing
    } else {
        ArtifactSourceState::Warning
    };

    ArtifactSourceStatus {
        id: source_id.to_string(),
        role: source.role,
        kind: source.kind,
        path: source.path.clone(),
        required: source.required,
        exists: component_status.exists,
        state,
        expected_url: source.url.clone(),
        actual_url: component_status.actual_url,
        expected_rev: source.rev.clone(),
        expected_commit: source.commit.clone(),
        actual_commit: component_status.actual_commit,
        expected_tree: source.tree.clone(),
        actual_tree: component_status.actual_tree,
        tracked_dirty: component_status.tracked_dirty,
        untracked_suspicious: component_status.untracked_suspicious,
        warnings,
        errors,
    }
}
