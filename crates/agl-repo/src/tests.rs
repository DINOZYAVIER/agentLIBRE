use super::*;

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("agl-repo-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".git")).unwrap();
    root
}

fn init_git_repo(root: &Path) {
    let status = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg(root)
        .status()
        .expect("git init should run");
    assert!(status.success(), "git init failed for {}", root.display());
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("git {:?} failed to start: {err}", args));
    assert!(
        output.status.success(),
        "git {:?} failed in {}\nstdout:\n{}\nstderr:\n{}",
        args,
        root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_with_identity(root: &Path, args: &[&str]) {
    let mut full_args = vec![
        "-c",
        "user.name=agl-test",
        "-c",
        "user.email=agl-test@example.invalid",
    ];
    full_args.extend_from_slice(args);
    git(root, &full_args);
}

fn write_task_spec(path: PathBuf, valid: bool) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let content = if valid {
        r#"# Problem

Problem statement.

# Goal

Goal statement.

# Scope

Scope statement.

# Non-goals

Non-goals statement.

# Implementation

Implementation steps.

# Acceptance Criteria

Acceptance criteria.

# Verification

Verification commands.
"#
    } else {
        r#"# Problem

Only a partial task spec.
"#
    };
    fs::write(path, content).unwrap();
}

#[test]
fn init_creates_manifest_and_local_component_dirs() {
    let root = temp_root("init");
    let report = init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();

    assert_eq!(report.workspace_root, root);
    assert!(report.manifest_path.exists());
    assert!(report.manifest_path.ends_with(WORKSPACE_MANIFEST_PATH));
    assert!(root.join(".agl/tasks").is_dir());
    assert!(root.join(".agl/reviews").is_dir());
    assert!(root.join(".agl/state").is_dir());
    assert!(!root.join(".agl/skills").exists());

    let manifest = fs::read_to_string(root.join(WORKSPACE_MANIFEST_PATH)).unwrap();
    assert!(manifest.contains("kind = \"submodule\""));
    assert!(manifest.contains(DEFAULT_SKILLS_URL));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn status_missing_manifest_reports_init_next_step() {
    let root = temp_root("missing-manifest");
    let report = status_repo_workspace(
        &root,
        &RepoStatusOptions {
            component: None,
            strict: false,
        },
    )
    .unwrap();

    assert_eq!(report.state, RepoStatusState::Invalid);
    assert!(
        report
            .errors
            .contains(&"workspace_manifest_missing".to_string())
    );
    assert!(report.next_steps.contains(&"agl init".to_string()));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn status_after_init_warns_about_missing_skills_submodule() {
    let root = temp_root("status-warning");
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    let report = status_repo_workspace(
        &root,
        &RepoStatusOptions {
            component: None,
            strict: false,
        },
    )
    .unwrap();

    assert_eq!(report.state, RepoStatusState::Warning);
    assert!(!report.should_fail(false));
    assert!(report.should_fail(true));
    assert!(
        report
            .warnings
            .contains(&"component.skills.missing".to_string())
    );
    assert!(report.next_steps.contains(&"agl skill init".to_string()));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_status_reports_default_contracts() {
    let root = temp_root("artifact-status");
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();

    let report = status_artifacts(
        &root,
        &ArtifactStatusOptions {
            artifact: None,
            strict: false,
        },
    )
    .unwrap();

    assert_eq!(report.state, ArtifactReportState::Warning);
    assert!(report.lock_path.ends_with(ARTIFACT_LOCK_PATH));
    assert!(report.artifacts.iter().any(|artifact| {
        artifact.id == "tasks"
            && artifact.path.as_path() == std::path::Path::new(".agl/tasks")
            && artifact.kind == ArtifactKind::Source
            && artifact.schema.as_deref() == Some("agl.task_spec.v1")
    }));
    assert!(report.artifacts.iter().any(|artifact| {
        artifact.id == "decision-docs"
            && artifact.path.as_path() == std::path::Path::new(".agl/decision-docs")
    }));
    assert!(report.artifacts.iter().any(|artifact| {
        artifact.id == "smoke" && artifact.path.as_path() == std::path::Path::new(".agl/smoke")
    }));
    assert!(report.artifacts.iter().any(|artifact| {
        artifact.id == "handoffs"
            && artifact.path.as_path() == std::path::Path::new(".agl/handoffs")
    }));
    assert!(report.sources.iter().any(|source| {
        source.id == "skills"
            && source.role == ArtifactSourceRole::Core
            && source.kind == ArtifactSourceKind::Submodule
    }));
    assert!(report.sources.iter().any(|source| {
        source.id == "tasks"
            && source.role == ArtifactSourceRole::Planning
            && source.kind == ArtifactSourceKind::Local
    }));
    assert!(report.artifacts.iter().any(|artifact| {
        artifact.id == "state"
            && artifact.source_role == ArtifactSourceRole::State
            && artifact.provides.contains(&"notes".to_string())
            && artifact.provides.contains(&"memory".to_string())
            && artifact.provides.contains(&"matrix".to_string())
            && artifact.provides.contains(&"cron".to_string())
    }));
    assert!(
        report
            .warnings
            .contains(&"artifact_lock_missing".to_string())
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_sync_creates_missing_declared_roots() {
    let root = temp_root("artifact-sync");
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    fs::remove_dir_all(root.join(".agl/tasks")).unwrap();

    let report = sync_artifacts(
        &root,
        &ArtifactSyncOptions {
            dry_run: false,
            strict: false,
        },
    )
    .unwrap();

    assert!(root.join(".agl/tasks").is_dir());
    assert!(!root.join(".agl/skills").exists());
    assert!(report.actions.iter().any(|action| {
        action.artifact_id == "tasks" && action.action == ArtifactSyncActionKind::CreatedDir
    }));
    assert!(report.actions.iter().any(|action| {
        action.artifact_id == "skills"
            && action.action == ArtifactSyncActionKind::SkippedNoCreateRule
    }));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_lock_writes_contract_hashes() {
    let root = temp_root("artifact-lock");
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();

    let report = lock_artifacts(
        &root,
        &ArtifactLockOptions {
            dry_run: false,
            strict: false,
        },
    )
    .unwrap();

    assert!(report.wrote);
    assert!(
        !report
            .warnings
            .contains(&"artifact_lock_missing".to_string())
    );
    assert!(root.join(ARTIFACT_LOCK_PATH).is_file());
    let locked = report.lock.artifacts.get("tasks").unwrap();
    assert_eq!(locked.source_id, "tasks");
    assert_eq!(locked.source_role, ArtifactSourceRole::Planning);
    assert_eq!(locked.source_kind, ArtifactSourceKind::Local);
    assert_eq!(locked.source_path, PathBuf::from(".agl/tasks"));
    assert_eq!(locked.contract_hash.len(), 64);
    assert_ne!(report.lock.locked_at_unix_ms, 0);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_lock_records_git_source_identity_and_detects_drift() {
    let root = temp_root("artifact-lock-source-identity");
    init_git_repo(&root);
    let source = root.join(".agl/sources/core");
    fs::create_dir_all(&source).unwrap();
    init_git_repo(&source);
    fs::write(source.join("README.md"), "core source\n").unwrap();
    git(&source, &["add", "."]);
    git_with_identity(&source, &["commit", "-m", "Add source"]);
    let commit = git_output(&source, ["rev-parse", "HEAD"]).unwrap();
    let tree = git_output(&source, ["rev-parse", "HEAD^{tree}"]).unwrap();
    fs::create_dir_all(root.join(".agl/tasks")).unwrap();
    fs::write(
        root.join(WORKSPACE_MANIFEST_PATH),
        r#"
version = 1
profile = "repo-workflow"

[components.tasks]
path = ".agl/tasks"
kind = "local"

[artifact_sources.core]
role = "core"
kind = "git"
path = ".agl/sources/core"
required = true
provides = ["tasks"]

[[artifact_sources.core.artifacts]]
id = "tasks"
kind = "source"
path = ".agl/tasks"
access = "read_write"
provides = ["tasks"]
schema = "agl.task_spec.v1"
required = true
shared = true
conflict_policy = "identical"
"#,
    )
    .unwrap();

    let report = lock_artifacts(
        &root,
        &ArtifactLockOptions {
            dry_run: false,
            strict: false,
        },
    )
    .unwrap();

    let locked = report.lock.artifacts.get("tasks").unwrap();
    assert_eq!(locked.source_id, "core");
    assert_eq!(locked.source_commit.as_deref(), Some(commit.trim()));
    assert_eq!(locked.source_tree.as_deref(), Some(tree.trim()));

    fs::write(source.join("README.md"), "core source changed\n").unwrap();
    git(&source, &["add", "."]);
    git_with_identity(&source, &["commit", "-m", "Change source"]);
    let drift = status_artifacts(
        &root,
        &ArtifactStatusOptions {
            artifact: None,
            strict: false,
        },
    )
    .unwrap();

    assert_eq!(drift.state, ArtifactReportState::Invalid);
    assert!(
        drift
            .errors
            .iter()
            .any(|error| error == "artifact.tasks.source_commit_changed"),
        "{:?}",
        drift.errors
    );

    let refreshed = lock_artifacts(
        &root,
        &ArtifactLockOptions {
            dry_run: false,
            strict: false,
        },
    )
    .unwrap();
    let refreshed_commit = git_output(&source, ["rev-parse", "HEAD"]).unwrap();
    assert!(refreshed.wrote);
    assert!(refreshed.errors.is_empty(), "{:?}", refreshed.errors);
    assert_eq!(
        refreshed
            .lock
            .artifacts
            .get("tasks")
            .unwrap()
            .source_commit
            .as_deref(),
        Some(refreshed_commit.trim())
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_lock_rejects_entries_missing_source_identity() {
    let root = temp_root("artifact-lock-missing-source-identity");
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    let report = lock_artifacts(
        &root,
        &ArtifactLockOptions {
            dry_run: false,
            strict: false,
        },
    )
    .unwrap();
    assert!(report.wrote);
    let lock_path = root.join(ARTIFACT_LOCK_PATH);
    let incomplete_lock = fs::read_to_string(&lock_path)
        .unwrap()
        .lines()
        .filter(|line| {
            !line.trim_start().starts_with("source_role")
                && !line.trim_start().starts_with("source_kind")
                && !line.trim_start().starts_with("source_path")
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&lock_path, incomplete_lock).unwrap();

    let report = status_artifacts(
        &root,
        &ArtifactStatusOptions {
            artifact: Some("tasks".to_string()),
            strict: false,
        },
    )
    .unwrap();

    assert!(
        report
            .errors
            .iter()
            .any(|error| error.starts_with("artifact_lock_invalid")),
        "{:?}",
        report.errors
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_status_reports_undeclared_agl_roots() {
    let root = temp_root("artifact-undeclared");
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    fs::create_dir_all(root.join(".agl/mystery")).unwrap();

    let report = status_artifacts(
        &root,
        &ArtifactStatusOptions {
            artifact: None,
            strict: false,
        },
    )
    .unwrap();

    assert!(report.undeclared.iter().any(|root| {
        root.path.as_path() == std::path::Path::new(".agl/mystery")
            && root.suggested_target.as_path() == std::path::Path::new(".agl/generated/mystery")
    }));
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("undeclared_artifact_root: .agl/mystery"))
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_status_does_not_report_declared_source_parent_as_undeclared() {
    let root = temp_root("artifact-declared-source-root");
    fs::create_dir_all(root.join(".agl/tasks")).unwrap();
    fs::create_dir_all(root.join(".agl/sources/core")).unwrap();
    fs::write(
        root.join(WORKSPACE_MANIFEST_PATH),
        r#"
version = 1
profile = "repo-workflow"

[components.tasks]
path = ".agl/tasks"
kind = "local"

[artifact_sources.core]
role = "core"
kind = "local"
path = ".agl/sources/core"
required = true
provides = ["tasks"]

[[artifact_sources.core.artifacts]]
id = "tasks"
kind = "source"
path = ".agl/tasks"
access = "read_write"
provides = ["tasks"]
required = true
"#,
    )
    .unwrap();

    let report = status_artifacts(
        &root,
        &ArtifactStatusOptions {
            artifact: None,
            strict: false,
        },
    )
    .unwrap();

    assert!(
        !report
            .undeclared
            .iter()
            .any(|root| root.path == std::path::Path::new(".agl/sources")),
        "{:?}",
        report.undeclared
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_status_reports_task_schema_failures() {
    let root = temp_root("artifact-schema-invalid");
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    write_task_spec(root.join(".agl/tasks/AGL-001/00_overview.md"), false);

    let report = status_artifacts(
        &root,
        &ArtifactStatusOptions {
            artifact: Some("tasks".to_string()),
            strict: true,
        },
    )
    .unwrap();

    assert_eq!(report.state, ArtifactReportState::Invalid);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("schema_invalid")),
        "{:?}",
        report.errors
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_status_does_not_report_unrelated_stale_locks_when_scoped() {
    let root = temp_root("artifact-scoped-stale-lock");
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    let report = lock_artifacts(
        &root,
        &ArtifactLockOptions {
            dry_run: false,
            strict: false,
        },
    )
    .unwrap();
    let mut lock = report.lock;
    let mut stale = lock.artifacts.get("tasks").unwrap().clone();
    stale.id = "old".to_string();
    stale.path = PathBuf::from(".agl/old");
    lock.artifacts.insert("old".to_string(), stale);
    fs::write(
        root.join(ARTIFACT_LOCK_PATH),
        toml::to_string(&lock).unwrap(),
    )
    .unwrap();

    let scoped = status_artifacts(
        &root,
        &ArtifactStatusOptions {
            artifact: Some("tasks".to_string()),
            strict: true,
        },
    )
    .unwrap();
    assert!(
        !scoped
            .warnings
            .iter()
            .any(|warning| warning == "artifact_lock_stale: old"),
        "{:?}",
        scoped.warnings
    );

    let full = status_artifacts(
        &root,
        &ArtifactStatusOptions {
            artifact: None,
            strict: false,
        },
    )
    .unwrap();
    assert!(
        full.warnings
            .iter()
            .any(|warning| warning == "artifact_lock_stale: old"),
        "{:?}",
        full.warnings
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_path_handle_resolves_declared_writable_path() {
    let root = temp_root("artifact-handle");
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();

    let handle = resolve_artifact_path_handle(
        &root,
        &ArtifactPathHandleRequest {
            path: PathBuf::from(".agl/tasks/AGL-001/00_overview.md"),
            access: ArtifactAccess::Write,
        },
    )
    .unwrap();

    assert_eq!(handle.artifact_id, "tasks");
    assert_eq!(handle.root, PathBuf::from(".agl/tasks"));
    assert_eq!(
        handle.path_in_artifact,
        PathBuf::from("AGL-001/00_overview.md")
    );

    let err = resolve_artifact_path_handle(
        &root,
        &ArtifactPathHandleRequest {
            path: PathBuf::from(".agl/skills/agl/skill/SKILL.md"),
            access: ArtifactAccess::Write,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("does not permit"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_path_handle_does_not_treat_write_as_read() {
    let root = temp_root("artifact-handle-access");
    fs::create_dir_all(root.join(".agl/tasks")).unwrap();
    fs::write(
        root.join(WORKSPACE_MANIFEST_PATH),
        r#"
version = 1
profile = "repo-workflow"

[components.tasks]
path = ".agl/tasks"
kind = "local"

[artifact_sources.local]
role = "local"
kind = "local"
path = ".agl"
required = true

[[artifact_sources.local.artifacts]]
id = "tasks"
kind = "source"
path = ".agl/tasks"
access = "write"
required = true
"#,
    )
    .unwrap();

    let handle = resolve_artifact_path_handle(
        &root,
        &ArtifactPathHandleRequest {
            path: PathBuf::from(".agl/tasks/AGL-001/00_overview.md"),
            access: ArtifactAccess::Write,
        },
    )
    .unwrap();
    assert_eq!(handle.artifact_id, "tasks");

    let err = resolve_artifact_path_handle(
        &root,
        &ArtifactPathHandleRequest {
            path: PathBuf::from(".agl/tasks/AGL-001/00_overview.md"),
            access: ArtifactAccess::Read,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("does not permit"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_status_detects_contract_hash_drift() {
    let root = temp_root("artifact-contract-drift");
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    lock_artifacts(
        &root,
        &ArtifactLockOptions {
            dry_run: false,
            strict: false,
        },
    )
    .unwrap();
    let manifest_path = root.join(WORKSPACE_MANIFEST_PATH);
    let content = fs::read_to_string(&manifest_path).unwrap();
    fs::write(
        &manifest_path,
        content.replace("agl.task_spec.v1", "agl.task_spec.v2"),
    )
    .unwrap();

    let report = status_artifacts(
        &root,
        &ArtifactStatusOptions {
            artifact: Some("tasks".to_string()),
            strict: false,
        },
    )
    .unwrap();

    assert_eq!(report.state, ArtifactReportState::Invalid);
    assert!(
        report
            .errors
            .contains(&"artifact.tasks.contract_changed".to_string()),
        "{:?}",
        report.errors
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_status_detects_source_path_drift() {
    let root = temp_root("artifact-source-path-drift");
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    let report = lock_artifacts(
        &root,
        &ArtifactLockOptions {
            dry_run: false,
            strict: false,
        },
    )
    .unwrap();
    let mut lock = report.lock;
    lock.artifacts.get_mut("tasks").unwrap().source_path = PathBuf::from(".agl/other");
    fs::write(
        root.join(ARTIFACT_LOCK_PATH),
        toml::to_string_pretty(&lock).unwrap(),
    )
    .unwrap();

    let report = status_artifacts(
        &root,
        &ArtifactStatusOptions {
            artifact: Some("tasks".to_string()),
            strict: false,
        },
    )
    .unwrap();

    assert!(
        report
            .errors
            .contains(&"artifact.tasks.source_path_changed".to_string()),
        "{:?}",
        report.errors
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_status_rejects_paths_outside_agl() {
    let root = temp_root("artifact-invalid-path");
    fs::create_dir_all(root.join(".agl")).unwrap();
    fs::write(
        root.join(WORKSPACE_MANIFEST_PATH),
        r#"
version = 1
profile = "repo-workflow"

[components.tasks]
path = ".agl/tasks"
kind = "local"

[artifact_sources.bad]
role = "local"
kind = "local"
path = ".agl/sources/bad"

[[artifact_sources.bad.artifacts]]
id = "bad"
kind = "source"
path = "outside"
access = "read"
"#,
    )
    .unwrap();

    let report = status_artifacts(
        &root,
        &ArtifactStatusOptions {
            artifact: None,
            strict: false,
        },
    )
    .unwrap();

    assert_eq!(report.state, ArtifactReportState::Invalid);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("artifact.bad.path_invalid"))
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn init_can_apply_local_workspace_profile_file() {
    let root = temp_root("profile-file");
    let profile_path = root.join("portable-profile.toml");
    fs::write(
        &profile_path,
        r#"
version = 1
name = "portable-repo-workflow"

[components.skills]
path = ".agl/skills"
kind = "submodule"
url = "git@example.com:agentlibre/agl-skills.git"
rev = "v0.2.0"
lock = ".agl/skills.lock"

[components.tasks]
path = ".agl/tasks"
kind = "git"
url = "git@example.com:agentlibre/tasks.git"
rev = "main"

[components.reviews]
path = ".agl/reviews"
kind = "submodule"
url = "git@example.com:agentlibre/reviews.git"
rev = "main"

[components.state]
path = ".agl/state"
kind = "ignored"

[artifact_sources.skills]
role = "core"
kind = "submodule"
path = ".agl/skills"

[artifact_sources.tasks]
role = "planning"
kind = "git"
path = ".agl/tasks"

[artifact_sources.reviews]
role = "generated"
kind = "submodule"
path = ".agl/reviews"
"#,
    )
    .unwrap();

    let report = init_repo_workspace(
        &root,
        &RepoInitOptions {
            profile: DEFAULT_PROFILE.to_string(),
            profile_file: Some(profile_path),
            ..RepoInitOptions::default()
        },
    )
    .unwrap();
    let manifest = read_manifest(&root.join(WORKSPACE_MANIFEST_PATH)).unwrap();

    assert_eq!(manifest.profile, "portable-repo-workflow");
    assert_eq!(manifest.components["tasks"].kind, ComponentKind::Git);
    assert_eq!(
        manifest.components["reviews"].kind,
        ComponentKind::Submodule
    );
    assert!(root.join(".agl/state").is_dir());
    assert!(!root.join(".agl/tasks").exists());
    assert!(report.changes.iter().any(|change| {
        change.path == Path::new(".agl/tasks")
            && change.action == RepoInitAction::DeclaredGitComponent
    }));
    assert!(report.changes.iter().any(|change| {
        change.path == Path::new(".agl/reviews")
            && change.action == RepoInitAction::DeclaredSubmodule
    }));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn profile_file_requires_artifact_sources() {
    let root = temp_root("profile-requires-artifact-sources");
    let profile_path = root.join("profile.toml");
    fs::write(
        &profile_path,
        r#"
version = 1
name = "repo-workflow"

[components.skills]
path = ".agl/skills"
kind = "submodule"
url = "git@example.com:agentlibre/skills.git"
rev = "v1"
lock = ".agl/skills.lock"

[components.tasks]
path = ".agl/tasks"
kind = "local"
"#,
    )
    .unwrap();

    let err = init_repo_workspace(
        &root,
        &RepoInitOptions {
            profile: DEFAULT_PROFILE.to_string(),
            profile_file: Some(profile_path),
            ..RepoInitOptions::default()
        },
    )
    .unwrap_err();

    assert!(err.to_string().contains("must define artifact_sources"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn init_can_override_skills_and_externalize_tasks() {
    let root = temp_root("init-external-artifacts");
    let report = init_repo_workspace(
        &root,
        &RepoInitOptions {
            skills_url: Some("git@example.com:agentlibre/skills.git".to_string()),
            skills_rev: Some("v1".to_string()),
            tasks_url: Some("git@example.com:private/specs.git".to_string()),
            tasks_rev: Some("main".to_string()),
            ..RepoInitOptions::default()
        },
    )
    .unwrap();
    let manifest = read_manifest(&root.join(WORKSPACE_MANIFEST_PATH)).unwrap();

    let skills = &manifest.components["skills"];
    assert_eq!(skills.kind, ComponentKind::Submodule);
    assert_eq!(
        skills.url.as_deref(),
        Some("git@example.com:agentlibre/skills.git")
    );
    assert_eq!(skills.rev.as_deref(), Some("v1"));

    let tasks = &manifest.components["tasks"];
    assert_eq!(tasks.kind, ComponentKind::Submodule);
    assert_eq!(
        tasks.url.as_deref(),
        Some("git@example.com:private/specs.git")
    );
    assert_eq!(tasks.rev.as_deref(), Some("main"));
    assert_eq!(tasks.lock.as_deref(), Some(Path::new(".agl/tasks.lock")));
    let tasks_source = &manifest.artifact_sources["tasks"];
    assert_eq!(tasks_source.kind, ArtifactSourceKind::Submodule);
    assert_eq!(
        tasks_source.url.as_deref(),
        Some("git@example.com:private/specs.git")
    );
    assert_eq!(tasks_source.rev.as_deref(), Some("main"));
    assert!(!root.join(".agl/tasks").exists());
    assert!(report.changes.iter().any(|change| {
        change.path == Path::new(".agl/tasks") && change.action == RepoInitAction::DeclaredSubmodule
    }));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn init_accepts_generic_artifact_sources() {
    let root = temp_root("init-generic-artifact-sources");
    let report = init_repo_workspace(
        &root,
        &RepoInitOptions {
            artifact_sources: vec![
                RepoArtifactSourceOverride {
                    name: "tasks".to_string(),
                    url: "rpi:/home/dinozyavier/git/agl-specs.git".to_string(),
                    rev: Some("main".to_string()),
                },
                RepoArtifactSourceOverride {
                    name: "reviews".to_string(),
                    url: "git@example.com:agentlibre/reviews.git".to_string(),
                    rev: None,
                },
            ],
            ..RepoInitOptions::default()
        },
    )
    .unwrap();
    let manifest = read_manifest(&root.join(WORKSPACE_MANIFEST_PATH)).unwrap();

    let tasks = &manifest.components["tasks"];
    assert_eq!(tasks.kind, ComponentKind::Submodule);
    assert_eq!(
        tasks.url.as_deref(),
        Some("rpi:/home/dinozyavier/git/agl-specs.git")
    );
    assert_eq!(tasks.rev.as_deref(), Some("main"));
    assert_eq!(
        manifest.artifact_sources["tasks"].role,
        ArtifactSourceRole::Planning
    );
    assert_eq!(
        manifest.artifact_sources["tasks"].url.as_deref(),
        Some("rpi:/home/dinozyavier/git/agl-specs.git")
    );

    let reviews = &manifest.components["reviews"];
    assert_eq!(reviews.kind, ComponentKind::Submodule);
    assert_eq!(
        reviews.url.as_deref(),
        Some("git@example.com:agentlibre/reviews.git")
    );
    assert_eq!(
        manifest.artifact_sources["reviews"].role,
        ArtifactSourceRole::Generated
    );
    assert!(report.changes.iter().any(|change| {
        change.path == Path::new(".agl/reviews")
            && change.action == RepoInitAction::DeclaredSubmodule
    }));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn init_rejects_tasks_rev_without_tasks_url() {
    let root = temp_root("init-rejects-tasks-rev");
    let err = init_repo_workspace(
        &root,
        &RepoInitOptions {
            tasks_rev: Some("main".to_string()),
            ..RepoInitOptions::default()
        },
    )
    .unwrap_err();

    assert!(err.to_string().contains("--tasks-rev requires --tasks-url"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn init_component_dry_run_plans_submodule_add() {
    let root = temp_root("component-init-dry-run");
    init_git_repo(&root);
    init_repo_workspace(
        &root,
        &RepoInitOptions {
            tasks_url: Some("git@example.com:private/specs.git".to_string()),
            tasks_rev: Some("main".to_string()),
            ..RepoInitOptions::default()
        },
    )
    .unwrap();

    let report = init_repo_component(
        &root,
        &RepoComponentInitOptions {
            component: "tasks".to_string(),
            dry_run: true,
        },
    )
    .unwrap();

    assert!(!report.has_errors());
    assert_eq!(
        report.actions,
        vec![
            RepoComponentInitAction::WouldAddSubmodule,
            RepoComponentInitAction::WouldCheckoutRev
        ]
    );
    assert!(!root.join(".agl/tasks").exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn init_component_adds_external_tasks_submodule() {
    let source = temp_root("component-init-tasks-source");
    init_git_repo(&source);
    fs::write(source.join("README.md"), "private specs\n").unwrap();
    git(&source, &["add", "."]);
    git_with_identity(&source, &["commit", "-m", "Add specs"]);
    let commit = git_output(&source, ["rev-parse", "HEAD"]).unwrap();

    let root = temp_root("component-init-tasks-root");
    init_git_repo(&root);
    init_repo_workspace(
        &root,
        &RepoInitOptions {
            tasks_url: Some(source.to_str().unwrap().to_string()),
            tasks_rev: Some(commit.trim().to_string()),
            ..RepoInitOptions::default()
        },
    )
    .unwrap();

    let report = init_repo_component(
        &root,
        &RepoComponentInitOptions {
            component: "tasks".to_string(),
            dry_run: false,
        },
    )
    .unwrap();

    assert!(!report.has_errors());
    assert_eq!(
        report.actions,
        vec![
            RepoComponentInitAction::AddedSubmodule,
            RepoComponentInitAction::CheckedOutRev
        ]
    );
    assert!(root.join(".agl/tasks/README.md").is_file());
    let modules = fs::read_to_string(root.join(".gitmodules")).unwrap();
    assert!(modules.contains("path = .agl/tasks"));

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn init_component_rejects_local_tasks_component() {
    let root = temp_root("component-init-local-tasks");
    init_git_repo(&root);
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();

    let report = init_repo_component(
        &root,
        &RepoComponentInitOptions {
            component: "tasks".to_string(),
            dry_run: false,
        },
    )
    .unwrap();

    assert!(report.has_errors());
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("component_not_submodule"))
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn verify_task_specs_accepts_valid_markdown() {
    let root = temp_root("verify-valid-task-spec");
    init_git_repo(&root);
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    write_task_spec(root.join(".agl/tasks/AGL-001/00_overview.md"), true);

    let report = verify_task_specs(&root, &TaskSpecVerifyOptions { strict: true }).unwrap();

    assert_eq!(report.state, TaskSpecVerifyState::Ok);
    assert!(!report.should_fail(true));
    assert_eq!(report.files.len(), 1);
    assert!(report.files[0].valid);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn verify_task_specs_reports_missing_sections_per_file() {
    let root = temp_root("verify-invalid-task-spec");
    init_git_repo(&root);
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    write_task_spec(root.join(".agl/tasks/AGL-001/00_overview.md"), false);

    let report = verify_task_specs(&root, &TaskSpecVerifyOptions { strict: false }).unwrap();

    assert_eq!(report.state, TaskSpecVerifyState::Invalid);
    assert!(report.should_fail(false));
    assert_eq!(report.files.len(), 1);
    assert!(!report.files[0].valid);
    assert!(
        report.files[0]
            .missing_sections
            .contains(&"acceptance criteria".to_string())
    );
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.starts_with("invalid_task_spec:"))
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn verify_task_specs_rejects_empty_tasks_component() {
    let root = temp_root("verify-empty-task-specs");
    init_git_repo(&root);
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();

    let report = verify_task_specs(&root, &TaskSpecVerifyOptions { strict: false }).unwrap();

    assert_eq!(report.state, TaskSpecVerifyState::Invalid);
    assert!(
        report
            .errors
            .contains(&"no_task_spec_markdown_files".to_string())
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn export_profile_writes_policy_and_excludes_local_state() {
    let root = temp_root("export-profile");
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    fs::write(
        root.join(".agl/skill-trust.toml"),
        "SECRET_LOCAL_TRUST_SHOULD_NOT_EXPORT",
    )
    .unwrap();
    fs::write(
        root.join(".agl/state/cache"),
        "SECRET_STATE_SHOULD_NOT_EXPORT",
    )
    .unwrap();
    let out = root.join("repo-workflow.toml");

    let report = export_repo_profile(
        &root,
        &RepoExportProfileOptions {
            out: out.clone(),
            force: false,
        },
    )
    .unwrap();
    let content = fs::read_to_string(&out).unwrap();
    let profile = read_workspace_profile(&out).unwrap();

    assert!(report.wrote);
    assert_eq!(profile.name, DEFAULT_PROFILE);
    assert!(profile.components.contains_key("skills"));
    assert!(profile.policy.hooks.managed);
    assert_eq!(
        profile.policy.hooks.install,
        vec!["pre-commit".to_string(), "pre-push".to_string()]
    );
    assert!(!profile.policy.trust.import_local_trust);
    assert!(
        profile
            .skill_pack
            .as_ref()
            .is_some_and(|identity| identity.same_ids_when_pinned)
    );
    assert!(!content.contains("SECRET_LOCAL_TRUST_SHOULD_NOT_EXPORT"));
    assert!(!content.contains("SECRET_STATE_SHOULD_NOT_EXPORT"));

    let overwrite =
        export_repo_profile(&root, &RepoExportProfileOptions { out, force: false }).unwrap_err();
    assert!(
        overwrite
            .to_string()
            .contains("failed to create profile export")
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn export_profile_round_trips_actual_skills_component_identity() {
    let root = temp_root("export-profile-actual-skills");
    let source = temp_root("export-profile-actual-skills-source");
    fs::remove_dir_all(source.join(".git")).unwrap();
    init_git_repo(&source);
    fs::write(source.join("README.md"), "skills\n").unwrap();
    git(&source, &["add", "."]);
    git_with_identity(&source, &["commit", "-m", "Add skills"]);
    let commit = git_output(&source, ["rev-parse", "HEAD"]).unwrap();
    let tree = git_output(&source, ["rev-parse", "HEAD^{tree}"]).unwrap();

    init_git_repo(&root);
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    git(
        &root,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            source.to_str().unwrap(),
            ".agl/skills",
        ],
    );

    let out = root.join("repo-workflow.toml");
    export_repo_profile(
        &root,
        &RepoExportProfileOptions {
            out: out.clone(),
            force: false,
        },
    )
    .unwrap();
    let profile = read_workspace_profile(&out).unwrap();
    let component = profile.components.get("skills").unwrap();
    let skill_pack = profile.skill_pack.as_ref().unwrap();

    assert_eq!(component.url.as_deref(), Some(source.to_str().unwrap()));
    assert_eq!(component.commit.as_deref(), Some(commit.trim()));
    assert_eq!(component.tree.as_deref(), Some(tree.trim()));
    assert_eq!(skill_pack.url, component.url);
    assert_eq!(skill_pack.commit, component.commit);
    assert_eq!(skill_pack.tree, component.tree);

    let imported = temp_root("export-profile-imported");
    init_git_repo(&imported);
    init_repo_workspace(
        &imported,
        &RepoInitOptions {
            profile: DEFAULT_PROFILE.to_string(),
            profile_file: Some(out),
            ..RepoInitOptions::default()
        },
    )
    .unwrap();
    let imported_manifest = fs::read_to_string(imported.join(WORKSPACE_MANIFEST_PATH)).unwrap();
    assert!(imported_manifest.contains(source.to_str().unwrap()));
    assert!(imported_manifest.contains(commit.trim()));
    assert!(imported_manifest.contains(tree.trim()));

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(imported).unwrap();
}

#[test]
fn profile_validation_rejects_mismatched_skill_pack_identity() {
    let root = temp_root("profile-skill-pack-mismatch");
    let profile_path = root.join("profile.toml");
    fs::write(
        &profile_path,
        r#"
version = 1
name = "repo-workflow"

[components.skills]
path = ".agl/skills"
kind = "submodule"
url = "git@example.com:agentlibre/agl-skills.git"
rev = "v0.1.0"
lock = ".agl/skills.lock"

[artifact_sources.skills]
role = "core"
kind = "submodule"
path = ".agl/skills"

[skill_pack]
component = "skills"
path = ".agl/skills"
url = "git@example.com:agentlibre/other-skills.git"
rev = "v0.1.0"
lock = ".agl/skills.lock"
same_ids_when_pinned = true
"#,
    )
    .unwrap();

    let err = read_workspace_profile(&profile_path).unwrap_err();
    assert!(err.to_string().contains("skill_pack.skills.url_mismatch"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn profile_file_name_must_match_requested_non_default_profile() {
    let root = temp_root("profile-name-mismatch");
    let profile_path = root.join("profile.toml");
    fs::write(
        &profile_path,
        r#"
version = 1
name = "actual-profile"

[components.state]
path = ".agl/state"
kind = "ignored"

[artifact_sources.state]
role = "state"
kind = "ignored"
path = ".agl/state"
"#,
    )
    .unwrap();

    let err = init_repo_workspace(
        &root,
        &RepoInitOptions {
            profile: "requested-profile".to_string(),
            profile_file: Some(profile_path),
            ..RepoInitOptions::default()
        },
    )
    .unwrap_err();

    assert!(err.to_string().contains("does not match requested profile"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn existing_plain_skills_directory_is_not_component_git_worktree() {
    let root = temp_root("plain-skills");
    init_git_repo(&root);
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    fs::create_dir_all(root.join(".agl/skills")).unwrap();

    let report = status_repo_workspace(
        &root,
        &RepoStatusOptions {
            component: Some("skills".to_string()),
            strict: false,
        },
    )
    .unwrap();

    assert_eq!(report.state, RepoStatusState::Invalid);
    let skills = report.components.first().expect("skills status");
    assert!(skills.exists);
    assert_eq!(skills.submodule_registered, Some(false));
    assert_eq!(skills.gitlink_present, Some(false));
    assert!(
        skills
            .errors
            .contains(&"not_component_git_worktree".to_string())
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn install_hooks_does_not_overwrite_unmanaged_hooks() {
    let root = temp_root("hooks-conflict");
    let hooks = root.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(hooks.join("pre-commit"), "#!/bin/sh\nexit 0\n").unwrap();

    let report = install_repo_hooks(
        &root,
        &RepoHooksOptions {
            dry_run: false,
            force: false,
        },
    )
    .unwrap();

    assert!(report.has_errors());
    assert_eq!(
        report.hooks[0].action,
        HookInstallAction::Conflict,
        "pre-commit should report conflict"
    );
    assert_eq!(
        report.hooks[1].action,
        HookInstallAction::WouldInstall,
        "pre-push should be planned but not written when another hook conflicts"
    );
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("hook_conflict"))
    );
    assert!(
        !hooks.join("pre-push").exists(),
        "hook install must be atomic when conflicts are present"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn install_hooks_labels_unmanaged_force_replacement() {
    let root = temp_root("hooks-force-unmanaged");
    let hooks = root.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(hooks.join("pre-commit"), "#!/bin/sh\nexit 0\n").unwrap();

    let report = install_repo_hooks(
        &root,
        &RepoHooksOptions {
            dry_run: true,
            force: true,
        },
    )
    .unwrap();

    assert!(!report.has_errors());
    assert_eq!(
        report.hooks[0].action,
        HookInstallAction::WouldReplaceUnmanaged
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn managed_hook_checks_agl_binary_before_running() {
    let content = hook_content("pre-commit");

    assert!(content.contains("command -v \"$AGL_BIN\""));
    assert!(content.contains("agentLIBRE hook error"));
    assert!(content.contains("\"$AGL_BIN\" status --strict"));
    assert!(content.contains("\"$AGL_BIN\" skill verify"));
}
