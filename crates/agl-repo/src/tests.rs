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
    assert!(!root.join(".agl/tasks").exists());
    assert!(report.changes.iter().any(|change| {
        change.path == Path::new(".agl/tasks") && change.action == RepoInitAction::DeclaredSubmodule
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
