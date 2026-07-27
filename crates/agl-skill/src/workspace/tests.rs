use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use agl_artifact::{ArtifactLock, LockedWorkspaceComponent};
use agl_extension::{HookId, SkillId, ToolId};

use super::*;
use crate::SkillReferencePolicy;

#[test]
fn legacy_trust_record_migrates_only_for_locked_source_and_current_digest() {
    let root = temp_root("trust-migration");
    fs::create_dir_all(root.join(".agl")).unwrap();
    let component = component();
    let harness = harness();
    let skill = status_from_harness(&root, PathBuf::from(".agl/skills/agl/repo-change"), harness);
    let report = report(&root, component.clone(), skill);
    write_lock(&root, &component);

    let mut store = SkillTrustStore {
        version: 1,
        records: vec![legacy_record(&root, &component)],
    };
    assert!(migrate_legacy_trust_records(&report, &mut store).unwrap());
    assert_eq!(
        store.records[0].artifact_identity,
        "skill:repo-change@1.0.0"
    );
    assert_eq!(store.records[0].package_digest, "sha256:package-digest");

    store.records[0].remote = "https://wrong.example/skills.git".to_string();
    assert!(!migrate_legacy_trust_records(&report, &mut store).unwrap());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn trust_identity_requires_common_identity_and_digest() {
    let root = PathBuf::from("/tmp/agl-skill-trust-identity");
    let mut left = legacy_record(&root, &component());
    left.artifact_identity = "skill:repo-change@1.0.0".to_string();
    left.package_digest = "sha256:one".to_string();
    let mut right = left.clone();
    assert!(trust_identity_matches(&left, &right));
    right.package_digest = "sha256:two".to_string();
    assert!(!trust_identity_matches(&left, &right));
}

fn report(
    root: &std::path::Path,
    component: ComponentStatus,
    skill: WorkspaceSkillStatus,
) -> WorkspaceSkillReport {
    WorkspaceSkillReport {
        state: SkillReportState::Ok,
        workspace_root: root.to_path_buf(),
        component: Some(component),
        lock_path: root.join(agl_repo::ARTIFACT_LOCK_PATH),
        skills: vec![skill],
        diagnostics: Vec::new(),
        warnings: Vec::new(),
        errors: Vec::new(),
        next_steps: Vec::new(),
    }
}

fn component() -> ComponentStatus {
    ComponentStatus {
        name: "skills".to_string(),
        path: PathBuf::from(".agl/skills"),
        kind: WorkspaceComponentKind::Git,
        exists: true,
        state: ComponentState::Ok,
        warnings: Vec::new(),
        errors: Vec::new(),
        expected_url: Some("https://example.invalid/skills.git".to_string()),
        actual_url: Some("https://example.invalid/skills.git".to_string()),
        expected_rev: Some("main".to_string()),
        expected_commit: None,
        actual_commit: Some("commit-1".to_string()),
        expected_tree: None,
        actual_tree: Some("tree-1".to_string()),
        submodule_registered: None,
        gitlink_present: None,
        nested_git_top: None,
        tracked_dirty: Some(false),
        untracked_suspicious: Some(false),
    }
}

fn harness() -> SkillHarness {
    SkillHarness {
        artifact: agl_artifact::ArtifactEnvelope::new(
            agl_artifact::ArtifactTypeId::skill(),
            agl_artifact::ArtifactPackageId::new("repo-change").unwrap(),
            agl_artifact::ArtifactVersion::new("1.0.0").unwrap(),
            agl_artifact::ArtifactSchemaId::new("agentlibre.skill/v2").unwrap(),
            agl_artifact::AglCompatibility::new(
                agl_artifact::ArtifactVersionReq::new(">=1.0.0-alpha.12").unwrap(),
                [agl_artifact::ArtifactVersion::new("1.0.0-alpha.12").unwrap()],
            )
            .unwrap(),
            Vec::new(),
        )
        .unwrap(),
        id: SkillId::new("repo-change").unwrap(),
        name: "repo-change".to_string(),
        description: "test skill".to_string(),
        version: agl_artifact::ArtifactVersion::new("1.0.0").unwrap(),
        source: SkillSource::Local,
        pack: "agl".to_string(),
        required_hooks: vec![HookId::new("core:repo_path.validate").unwrap()],
        allowed_tools: vec![ToolId::new("core.workspace:fs.read").unwrap()],
        requestable_tools: Vec::new(),
        denied_tools: Vec::new(),
        permission_request_templates: Vec::new(),
        permissions: Default::default(),
        context_budget_tokens: 256,
        reference_policy: SkillReferencePolicy {
            include: Vec::new(),
        },
        references: Vec::new(),
        artifacts: Vec::new(),
        guarantees: vec!["test".to_string()],
        body: "Body.".to_string(),
        source_path: "agl/repo-change/SKILL.md".to_string(),
        manifest_sha256: "manifest".to_string(),
        tree_sha256: "sha256:package-digest".to_string(),
    }
}

fn legacy_record(root: &std::path::Path, component: &ComponentStatus) -> TrustedSkillRecord {
    TrustedSkillRecord {
        skill_name: "repo-change".to_string(),
        source: "local".to_string(),
        workspace_root: root.to_path_buf(),
        artifact_identity: String::new(),
        package_digest: String::new(),
        remote: component.actual_url.clone().unwrap(),
        ref_name: component.expected_rev.clone().unwrap(),
        commit: component.actual_commit.clone().unwrap(),
        tree: component.actual_tree.clone().unwrap(),
        approved_at: "2026-01-01T00:00:00Z".to_string(),
        agentlibre_version: "test".to_string(),
        revoked: false,
        revoked_at: None,
    }
}

fn write_lock(root: &std::path::Path, component: &ComponentStatus) {
    let locked = LockedWorkspaceComponent {
        kind: Some(component.kind),
        path: Some(component.path.clone()),
        definition_digest: Some("definition".to_string()),
        source_id: None,
        source_kind: None,
        url: component.actual_url.clone(),
        rev: component.expected_rev.clone(),
        commit: component.actual_commit.clone(),
        tree: component.actual_tree.clone(),
    };
    let lock = ArtifactLock::new(
        BTreeMap::from([(component.name.clone(), locked)]),
        BTreeMap::new(),
    )
    .unwrap();
    lock.write_atomic(root.join(agl_repo::ARTIFACT_LOCK_PATH))
        .unwrap();
}

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agl-skill-workspace-{label}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    root
}
