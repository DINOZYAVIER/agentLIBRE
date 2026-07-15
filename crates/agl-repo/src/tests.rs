use super::*;

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("agl-repo-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".git")).unwrap();
    root
}

fn init_workspace_with_artifacts(root: &Path) {
    init_repo_workspace(root, &RepoInitOptions::default()).unwrap();
    fs::write(
        root.join(WORKSPACE_MANIFEST_PATH),
        r#"
version = 1
profile = "repo-workflow"

[functions]
default = "gemma4-12b"

[artifacts.tasks]
kind = "local"
path = ".agl/tasks"
required = true
access = "read_write"
validation = "agl.task_spec.v1"
create = ["."]

[artifacts.reviews]
kind = "local"
path = ".agl/reviews"
required = false
access = "read_write"
create = ["."]

[artifacts.state]
kind = "ignored"
path = ".agl/state"
required = false
access = "read_write"
create = ["."]
"#,
    )
    .unwrap();
    fs::create_dir_all(root.join(".agl/tasks")).unwrap();
    fs::create_dir_all(root.join(".agl/reviews")).unwrap();
    fs::create_dir_all(root.join(".agl/state")).unwrap();
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
        r#"---
status: planned
---

# Problem

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
        r#"---
status: planned
---

# Problem

Only a partial task spec.
"#
    };
    fs::write(path, content).unwrap();
}

#[test]
fn init_creates_minimal_manifest_without_implicit_artifacts() {
    let root = temp_root("init");
    let report = init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();

    assert_eq!(report.workspace_root, root);
    assert!(report.manifest_path.exists());
    assert!(report.manifest_path.ends_with(WORKSPACE_MANIFEST_PATH));
    assert!(!root.join(".agl/tasks").exists());
    assert!(!root.join(".agl/reviews").exists());
    assert!(!root.join(".agl/state").exists());
    assert!(!root.join(".agl/skills").exists());

    let manifest = fs::read_to_string(root.join(WORKSPACE_MANIFEST_PATH)).unwrap();
    assert!(manifest.contains("[functions]"));
    assert!(manifest.contains(&format!("default = \"{DEFAULT_FUNCTION}\"")));
    assert!(!manifest.contains("[artifacts."));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn init_repairs_missing_workspace_default_function() {
    let root = temp_root("init-repair-default-function");
    fs::create_dir_all(root.join(".agl")).unwrap();
    fs::write(
        root.join(WORKSPACE_MANIFEST_PATH),
        r#"
version = 1
profile = "repo-workflow"

[artifacts.state]
path = ".agl/state"
kind = "ignored"
required = false
access = "read_write"
create = ["."]
"#,
    )
    .unwrap();

    let dry_run = init_repo_workspace(
        &root,
        &RepoInitOptions {
            dry_run: true,
            ..RepoInitOptions::default()
        },
    )
    .unwrap();
    assert!(dry_run.changes.iter().any(|change| {
        change.path == Path::new(WORKSPACE_MANIFEST_PATH)
            && change.action == RepoInitAction::WouldOverwriteFile
    }));

    let report = init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    assert!(report.changes.iter().any(|change| {
        change.path == Path::new(WORKSPACE_MANIFEST_PATH)
            && change.action == RepoInitAction::OverwroteFile
    }));
    let manifest = read_manifest(&root.join(WORKSPACE_MANIFEST_PATH)).unwrap();
    assert_eq!(manifest.functions.default, DEFAULT_FUNCTION);

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
fn status_after_minimal_init_is_ok() {
    let root = temp_root("status-ok");
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    let report = status_repo_workspace(
        &root,
        &RepoStatusOptions {
            component: None,
            strict: false,
        },
    )
    .unwrap();

    assert_eq!(report.state, RepoStatusState::Ok);
    assert!(!report.should_fail(false));
    assert!(!report.should_fail(true));
    assert!(report.warnings.is_empty());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_status_reports_declared_artifacts() {
    let root = temp_root("artifact-status");
    init_workspace_with_artifacts(&root);

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
            && artifact.validation.as_deref() == Some("agl.task_spec.v1")
    }));
    assert!(report.artifacts.iter().any(|artifact| {
        artifact.id == "tasks" && artifact.storage == WorkspaceArtifactKind::Local
    }));
    assert!(
        report
            .artifacts
            .iter()
            .any(|artifact| { artifact.id == "state" && artifact.kind == ArtifactKind::State })
    );
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
    init_workspace_with_artifacts(&root);
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
    assert!(report.actions.iter().any(|action| {
        action.artifact_id == "tasks" && action.action == ArtifactSyncActionKind::CreatedDir
    }));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_lock_writes_definition_hashes() {
    let root = temp_root("artifact-lock");
    init_workspace_with_artifacts(&root);

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
    assert_eq!(locked.id, "tasks");
    assert_eq!(locked.storage, WorkspaceArtifactKind::Local);
    assert_eq!(locked.path, PathBuf::from(".agl/tasks"));
    assert_eq!(locked.definition_hash.len(), 64);
    assert_ne!(report.lock.locked_at_unix_ms, 0);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_lock_records_git_identity_and_detects_drift() {
    let root = temp_root("artifact-lock-source-identity");
    init_git_repo(&root);
    let source = root.join(".agl/tasks");
    fs::create_dir_all(&source).unwrap();
    init_git_repo(&source);
    fs::write(source.join("README.md"), "core source\n").unwrap();
    git(&source, &["add", "."]);
    git_with_identity(&source, &["commit", "-m", "Add source"]);
    let commit = git_output(&source, ["rev-parse", "HEAD"]).unwrap();
    let tree = git_output(&source, ["rev-parse", "HEAD^{tree}"]).unwrap();
    fs::write(
        root.join(WORKSPACE_MANIFEST_PATH),
        r#"
version = 1
profile = "repo-workflow"

[functions]
default = "gemma4-12b"

[artifacts.tasks]
kind = "git"
path = ".agl/tasks"
required = true
access = "read_write"
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
    assert_eq!(locked.id, "tasks");
    assert_eq!(locked.commit.as_deref(), Some(commit.trim()));
    assert_eq!(locked.tree.as_deref(), Some(tree.trim()));

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
            .any(|error| error == "artifact.tasks.commit_changed"),
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
            .commit
            .as_deref(),
        Some(refreshed_commit.trim())
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_lock_rejects_entries_missing_definition_identity() {
    let root = temp_root("artifact-lock-missing-definition-identity");
    init_workspace_with_artifacts(&root);
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
            !line.trim_start().starts_with("storage")
                && !line.trim_start().starts_with("definition_hash")
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
    init_workspace_with_artifacts(&root);
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
    fs::create_dir_all(root.join(".agl/sources/core")).unwrap();
    fs::write(
        root.join(WORKSPACE_MANIFEST_PATH),
        r#"
version = 1
profile = "repo-workflow"

[functions]
default = "gemma4-12b"

[artifacts.core]
kind = "local"
path = ".agl/sources/core"
required = true
access = "read_write"
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
    init_workspace_with_artifacts(&root);
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
    init_workspace_with_artifacts(&root);
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
    init_workspace_with_artifacts(&root);

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
    assert!(err.to_string().contains("is not declared"));

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

[functions]
default = "gemma4-12b"

[artifacts.tasks]
kind = "local"
path = ".agl/tasks"
required = true
access = "write"
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
fn artifact_status_detects_definition_drift() {
    let root = temp_root("artifact-definition-drift");
    init_workspace_with_artifacts(&root);
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
            .contains(&"artifact.tasks.definition_changed".to_string()),
        "{:?}",
        report.errors
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_status_detects_locked_path_drift() {
    let root = temp_root("artifact-path-drift");
    init_workspace_with_artifacts(&root);
    let report = lock_artifacts(
        &root,
        &ArtifactLockOptions {
            dry_run: false,
            strict: false,
        },
    )
    .unwrap();
    let mut lock = report.lock;
    lock.artifacts.get_mut("tasks").unwrap().path = PathBuf::from(".agl/other");
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
            .contains(&"artifact.tasks.path_changed".to_string()),
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

[functions]
default = "gemma4-12b"

[artifacts.bad]
kind = "local"
path = "outside"
access = "read"
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

[artifacts.skills]
kind = "git"
path = ".agl/skills"
url = "ssh://git@example.invalid/agentlibre/agl-skills.git"
rev = "v0.2.0"
required = true
access = "read"

[artifacts.tasks]
kind = "git"
path = ".agl/tasks"
url = "ssh://git@example.invalid/agentlibre/tasks.git"
rev = "main"
required = true
access = "read_write"
validation = "agl.task_spec.v1"

[artifacts.reviews]
kind = "git"
path = ".agl/reviews"
url = "ssh://git@example.invalid/agentlibre/reviews.git"
rev = "main"
required = true
access = "read_write"

[artifacts.state]
kind = "ignored"
path = ".agl/state"
required = false
access = "read_write"
create = ["."]
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
    assert_eq!(manifest.functions.default, DEFAULT_FUNCTION);
    assert_eq!(manifest.artifacts["tasks"].kind, WorkspaceArtifactKind::Git);
    assert_eq!(
        manifest.artifacts["reviews"].kind,
        WorkspaceArtifactKind::Git
    );
    assert!(root.join(".agl/state").is_dir());
    assert!(!root.join(".agl/tasks").exists());
    assert!(report.changes.iter().any(|change| {
        change.path == Path::new(".agl/tasks")
            && change.action == RepoInitAction::DeclaredGitComponent
    }));
    assert!(report.changes.iter().any(|change| {
        change.path == Path::new(".agl/reviews")
            && change.action == RepoInitAction::DeclaredGitComponent
    }));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn profile_file_allows_no_artifacts() {
    let root = temp_root("profile-without-artifacts");
    let profile_path = root.join("profile.toml");
    fs::write(
        &profile_path,
        r#"
version = 1
name = "repo-workflow"

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
    let manifest = read_manifest(&report.manifest_path).unwrap();
    assert!(manifest.artifacts.is_empty());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn init_can_override_skills_and_externalize_tasks() {
    let root = temp_root("init-external-artifacts");
    let report = init_repo_workspace(
        &root,
        &RepoInitOptions {
            skills_url: Some("ssh://git@example.invalid/agentlibre/skills.git".to_string()),
            skills_rev: Some("v1".to_string()),
            tasks_url: Some("ssh://git@example.invalid/agentlibre/specs.git".to_string()),
            tasks_rev: Some("main".to_string()),
            ..RepoInitOptions::default()
        },
    )
    .unwrap();
    let manifest = read_manifest(&root.join(WORKSPACE_MANIFEST_PATH)).unwrap();

    let skills = &manifest.artifacts["skills"];
    assert_eq!(skills.kind, WorkspaceArtifactKind::Git);
    assert_eq!(
        skills.url.as_deref(),
        Some("ssh://git@example.invalid/agentlibre/skills.git")
    );
    assert_eq!(skills.rev.as_deref(), Some("v1"));

    let tasks = &manifest.artifacts["tasks"];
    assert_eq!(tasks.kind, WorkspaceArtifactKind::Git);
    assert_eq!(
        tasks.url.as_deref(),
        Some("ssh://git@example.invalid/agentlibre/specs.git")
    );
    assert_eq!(tasks.rev.as_deref(), Some("main"));
    assert_eq!(tasks.validation.as_deref(), Some("agl.task_spec.v1"));
    assert!(!root.join(".agl/tasks").exists());
    assert!(report.changes.iter().any(|change| {
        change.path == Path::new(".agl/tasks")
            && change.action == RepoInitAction::DeclaredGitComponent
    }));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn init_accepts_generic_artifacts() {
    let root = temp_root("init-generic-artifacts");
    let report = init_repo_workspace(
        &root,
        &RepoInitOptions {
            artifacts: vec![
                RepoArtifactOverride {
                    name: "tasks".to_string(),
                    url: "ssh://git@example.invalid/agentlibre/agl-specs.git".to_string(),
                    rev: Some("main".to_string()),
                },
                RepoArtifactOverride {
                    name: "reviews".to_string(),
                    url: "ssh://git@example.invalid/agentlibre/reviews.git".to_string(),
                    rev: None,
                },
            ],
            ..RepoInitOptions::default()
        },
    )
    .unwrap();
    let manifest = read_manifest(&root.join(WORKSPACE_MANIFEST_PATH)).unwrap();

    let tasks = &manifest.artifacts["tasks"];
    assert_eq!(tasks.kind, WorkspaceArtifactKind::Git);
    assert_eq!(
        tasks.url.as_deref(),
        Some("ssh://git@example.invalid/agentlibre/agl-specs.git")
    );
    assert_eq!(tasks.rev.as_deref(), Some("main"));
    let reviews = &manifest.artifacts["reviews"];
    assert_eq!(reviews.kind, WorkspaceArtifactKind::Git);
    assert_eq!(
        reviews.url.as_deref(),
        Some("ssh://git@example.invalid/agentlibre/reviews.git")
    );
    assert!(report.changes.iter().any(|change| {
        change.path == Path::new(".agl/reviews")
            && change.action == RepoInitAction::DeclaredGitComponent
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
fn init_component_dry_run_plans_git_clone() {
    let root = temp_root("component-init-dry-run");
    init_git_repo(&root);
    init_repo_workspace(
        &root,
        &RepoInitOptions {
            tasks_url: Some("ssh://git@example.invalid/agentlibre/specs.git".to_string()),
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
            RepoComponentInitAction::WouldClone,
            RepoComponentInitAction::WouldCheckoutRev
        ]
    );
    assert!(!root.join(".agl/tasks").exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn init_component_clones_external_tasks_repository() {
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
            RepoComponentInitAction::Cloned,
            RepoComponentInitAction::CheckedOutRev
        ]
    );
    assert!(root.join(".agl/tasks/README.md").is_file());
    assert!(!root.join(".gitmodules").exists());

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn init_component_rejects_local_tasks_component() {
    let root = temp_root("component-init-local-tasks");
    init_git_repo(&root);
    init_workspace_with_artifacts(&root);

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
            .any(|error| error.contains("component_not_git_backed"))
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn verify_task_specs_is_neutral_when_tasks_are_not_configured() {
    let root = temp_root("verify-tasks-not-configured");
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();

    let report = verify_task_specs(&root, &TaskSpecVerifyOptions { strict: true }).unwrap();

    assert_eq!(report.state, TaskSpecVerifyState::NotConfigured);
    assert!(report.errors.is_empty());
    assert!(report.warnings.is_empty());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn verify_task_specs_fails_when_required_tasks_root_is_missing() {
    let root = temp_root("verify-tasks-required-missing");
    init_workspace_with_artifacts(&root);
    fs::remove_dir_all(root.join(".agl/tasks")).unwrap();

    let report = verify_task_specs(&root, &TaskSpecVerifyOptions { strict: false }).unwrap();

    assert_eq!(report.state, TaskSpecVerifyState::Invalid);
    assert!(!report.errors.is_empty());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn verify_task_specs_accepts_valid_markdown() {
    let root = temp_root("verify-valid-task-spec");
    init_git_repo(&root);
    init_workspace_with_artifacts(&root);
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
    init_workspace_with_artifacts(&root);
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
fn verify_task_specs_checks_only_planned_task_overviews() {
    let root = temp_root("verify-planned-task-overviews");
    init_git_repo(&root);
    init_workspace_with_artifacts(&root);
    fs::write(root.join(".agl/tasks/README.md"), "# Task index\n").unwrap();
    fs::write(root.join(".agl/tasks/AGENTS.md"), "# Instructions\n").unwrap();
    write_task_spec(root.join(".agl/tasks/AGL-001/00_overview.md"), true);
    fs::write(
        root.join(".agl/tasks/AGL-001/01_notes.md"),
        "---\nstatus: planned\n---\n\n# Implementation Notes\n",
    )
    .unwrap();
    fs::create_dir_all(root.join(".agl/tasks/AGL-002")).unwrap();
    fs::write(
        root.join(".agl/tasks/AGL-002/00_overview.md"),
        "---\nstatus: implemented\n---\n\n# Historical task\n",
    )
    .unwrap();

    let report = verify_task_specs(&root, &TaskSpecVerifyOptions { strict: false }).unwrap();

    assert_eq!(report.state, TaskSpecVerifyState::Ok);
    assert_eq!(report.files.len(), 1);
    assert!(report.files[0].path.ends_with("AGL-001/00_overview.md"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn verify_task_specs_rejects_unsupported_task_status() {
    let root = temp_root("verify-unsupported-task-status");
    init_git_repo(&root);
    init_workspace_with_artifacts(&root);
    let path = root.join(".agl/tasks/AGL-001/00_overview.md");
    write_task_spec(path.clone(), true);
    let content = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        content.replacen("status: planned", "status: backlog", 1),
    )
    .unwrap();

    let report = verify_task_specs(&root, &TaskSpecVerifyOptions { strict: false }).unwrap();

    assert_eq!(report.state, TaskSpecVerifyState::Invalid);
    assert_eq!(report.files.len(), 1);
    assert!(
        report.files[0]
            .errors
            .iter()
            .any(|error| error.contains("unsupported task spec status `backlog`"))
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn verify_task_specs_rejects_empty_tasks_component() {
    let root = temp_root("verify-empty-task-specs");
    init_git_repo(&root);
    init_workspace_with_artifacts(&root);

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
    init_workspace_with_artifacts(&root);
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
    assert!(profile.artifacts.contains_key("tasks"));
    assert!(profile.artifacts.contains_key("state"));
    assert!(profile.policy.hooks.managed);
    assert_eq!(
        profile.policy.hooks.install,
        vec!["pre-commit".to_string(), "pre-push".to_string()]
    );
    assert!(!profile.policy.trust.import_local_trust);
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
fn export_profile_round_trips_artifact_identity() {
    let root = temp_root("export-profile-artifact-identity");
    fs::create_dir_all(root.join(".agl")).unwrap();
    fs::write(
        root.join(WORKSPACE_MANIFEST_PATH),
        r#"
version = 1
profile = "repo-workflow"

[functions]
default = "gemma4-12b"

[artifacts.skills]
kind = "git"
path = ".agl/skills"
url = "ssh://git@example.invalid/agentlibre/skills.git"
rev = "v1"
commit = "0123456789abcdef"
tree = "fedcba9876543210"
required = true
access = "read"
"#,
    )
    .unwrap();

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
    let artifact = profile.artifacts.get("skills").unwrap();
    assert_eq!(
        artifact.url.as_deref(),
        Some("ssh://git@example.invalid/agentlibre/skills.git")
    );
    assert_eq!(artifact.commit.as_deref(), Some("0123456789abcdef"));
    assert_eq!(artifact.tree.as_deref(), Some("fedcba9876543210"));

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
    assert!(imported_manifest.contains("ssh://git@example.invalid/agentlibre/skills.git"));
    assert!(imported_manifest.contains("0123456789abcdef"));
    assert!(imported_manifest.contains("fedcba9876543210"));

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(imported).unwrap();
}

#[test]
fn profile_validation_rejects_removed_skill_pack_schema() {
    let root = temp_root("profile-old-skill-pack-schema");
    let profile_path = root.join("profile.toml");
    fs::write(
        &profile_path,
        r#"
version = 1
name = "repo-workflow"

[skill_pack]
component = "skills"
path = ".agl/skills"
url = "ssh://git@example.invalid/agentlibre/skills.git"
rev = "v0.1.0"
same_ids_when_pinned = true
"#,
    )
    .unwrap();

    let err = read_workspace_profile(&profile_path).unwrap_err();
    assert!(format!("{err:#}").contains("unknown field"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn workspace_manifest_rejects_removed_component_and_nested_artifact_shape() {
    let root = temp_root("manifest-old-artifact-shape");
    let manifest_path = root.join(WORKSPACE_MANIFEST_PATH);
    fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    fs::write(
        &manifest_path,
        r#"
version = 1
profile = "repo-workflow"

[functions]
default = "gemma4-12b"

[components.tasks]
path = ".agl/tasks"
kind = "git"

[artifact_sources.tasks]
role = "planning"
kind = "git"
path = ".agl/tasks"
required = true

[[artifact_sources.tasks.artifacts]]
id = "tasks"
kind = "source"
path = "."
access = "read_write"
required = true
"#,
    )
    .unwrap();

    let error = read_manifest(&manifest_path).unwrap_err();
    assert!(format!("{error:#}").contains("unknown field"));

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

[artifacts.state]
kind = "ignored"
path = ".agl/state"
required = false
access = "read_write"
create = ["."]
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
    init_workspace_with_artifacts(&root);
    let manifest_path = root.join(WORKSPACE_MANIFEST_PATH);
    let mut manifest = fs::read_to_string(&manifest_path).unwrap();
    manifest.push_str(
        r#"

[artifacts.skills]
kind = "git"
path = ".agl/skills"
url = "ssh://git@example.invalid/agentlibre/skills.git"
required = true
access = "read"
"#,
    );
    fs::write(&manifest_path, manifest).unwrap();
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
    assert_eq!(skills.submodule_registered, None);
    assert_eq!(skills.gitlink_present, None);
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
