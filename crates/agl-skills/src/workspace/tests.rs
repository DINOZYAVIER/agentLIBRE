use std::process::Command;

use agl_repo::{DEFAULT_SKILLS_URL, RepoInitOptions, init_repo_workspace};

use super::*;

#[test]
fn plain_skills_dir_is_discovered_but_not_usable() {
    let root = temp_root("plain-skills");
    init_git_repo(&root);
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    let skill_dir = root.join(".agl/skills/agl/repo-change");
    write_workspace_skill(&skill_dir, "repo-change", &[], &[]);

    let report = workspace_skill_report(&root).unwrap();

    assert_eq!(report.state, SkillReportState::Invalid);
    assert_eq!(report.skills.len(), 1);
    assert_eq!(report.skills[0].name.as_deref(), Some("repo-change"));
    assert!(report.skills[0].valid);
    assert!(!report.skills[0].usable);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("not_component_git_worktree"))
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn invalid_workspace_manifest_is_reported() {
    let root = temp_root("invalid-skill");
    init_git_repo(&root);
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    let skill_dir = root.join(".agl/skills/agl/bad-skill");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        r#"---
name: bad-skill
description: Bad.
---
Body.
"#,
    )
    .unwrap();

    let report = workspace_skill_report(&root).unwrap();

    assert_eq!(report.skills.len(), 1);
    assert!(!report.skills[0].valid);
    assert!(
        report.skills[0]
            .errors
            .iter()
            .any(|error| error.contains("missing field"))
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn workspace_status_reports_declared_skill_folders() {
    let root = temp_root("skill-folders");
    init_git_repo(&root);
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    let skill_dir = root.join(".agl/skills/agl/repo-change");
    write_workspace_skill_with_folders(
        &skill_dir,
        "repo-change",
        r#"
folders:
  - id: task-drafts
    kind: generated
    path: .agl/tasks/repo-change
    access: read_write
    provides:
      - task-drafts
    schema: agl.task_draft.v1"#,
    );

    let report = workspace_skill_report(&root).unwrap();
    let skill = &report.skills[0];

    assert_eq!(skill.artifact_folders.len(), 1);
    assert_eq!(skill.artifact_folders[0].id, "task-drafts");
    assert_eq!(
        skill.artifact_folders[0].path,
        PathBuf::from(".agl/tasks/repo-change")
    );
    assert!(!skill.artifact_folders[0].exists);
    assert!(
        skill
            .warnings
            .contains(&"artifact_folder.task-drafts.missing".to_string())
    );

    let sync =
        sync_workspace_skill_folders(&root, &SkillFolderSyncOptions { dry_run: false }).unwrap();
    assert!(!sync.has_errors());
    assert!(root.join(".agl/tasks/repo-change").is_dir());
    assert!(sync.actions.iter().any(|action| {
        action.folder_id == "task-drafts" && action.action == SkillFolderSyncActionKind::CreatedDir
    }));
    assert!(sync.warnings.is_empty());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn lock_refuses_unusable_component() {
    let root = temp_root("lock-refuses");
    init_git_repo(&root);
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    write_workspace_skill(
        &root.join(".agl/skills/agl/repo-change"),
        "repo-change",
        &[],
        &[],
    );

    let report = lock_workspace_skills(&root, &SkillLockOptions { dry_run: false }).unwrap();

    assert!(report.has_errors());
    assert!(
        report
            .errors
            .contains(&"skills_component_not_usable".to_string())
    );
    assert!(!report.lock_path.exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn lock_roundtrip_accepts_clean_skills_submodule() {
    let (root, source) = clean_skills_submodule_fixture("lock-roundtrip");

    let unlocked = workspace_skill_report(&root).unwrap();
    assert_eq!(unlocked.state, SkillReportState::Warning);
    assert!(
        unlocked
            .warnings
            .contains(&"skills_lock_missing".to_string())
    );
    assert_eq!(unlocked.skills[0].name.as_deref(), Some("repo-change"));
    assert!(!unlocked.skills[0].usable);
    assert_eq!(unlocked.skills[0].trust_state, SkillTrustState::Unsupported);

    let first_lock = lock_workspace_skills(&root, &SkillLockOptions { dry_run: false }).unwrap();
    assert!(!first_lock.has_errors());
    assert!(first_lock.wrote);
    assert!(
        !first_lock
            .warnings
            .contains(&"skills_lock_missing".to_string())
    );
    assert!(first_lock.lock_path.exists());

    let locked = workspace_skill_report(&root).unwrap();
    assert_eq!(locked.state, SkillReportState::Ok);
    assert!(!locked.skills[0].usable);
    assert_eq!(locked.skills[0].trust_state, SkillTrustState::Unknown);

    let second_lock = lock_workspace_skills(&root, &SkillLockOptions { dry_run: false }).unwrap();
    assert!(!second_lock.has_errors());
    assert!(!second_lock.wrote);

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn lock_mismatch_marks_workspace_skills_not_usable() {
    let (root, source) = clean_skills_submodule_fixture("lock-mismatch");
    lock_workspace_skills(&root, &SkillLockOptions { dry_run: false }).unwrap();
    let lock_path = root.join(SKILLS_LOCK_PATH);
    let lock = fs::read_to_string(&lock_path).unwrap();
    let lock = lock
        .lines()
        .map(|line| {
            if line.starts_with("commit = ") {
                "commit = \"0000000000000000000000000000000000000000\""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&lock_path, format!("{lock}\n")).unwrap();

    let report = workspace_skill_report(&root).unwrap();

    assert_eq!(report.state, SkillReportState::Invalid);
    assert!(
        report
            .errors
            .contains(&"skills_lock_commit_mismatch".to_string())
    );
    assert_eq!(report.skills[0].trust_state, SkillTrustState::RevMismatch);
    assert!(!report.skills[0].usable);
    assert!(
        !report
            .next_steps
            .contains(&"initialize .agl/skills submodule".to_string())
    );
    assert!(
        report
            .next_steps
            .contains(&"review .agl/skills and run agl skill lock".to_string())
    );

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn trust_promotes_changes_and_revokes_locked_workspace_skill() {
    let (root, source) = clean_skills_submodule_fixture("trust");
    lock_workspace_skills(&root, &SkillLockOptions { dry_run: false }).unwrap();
    let trust_store = root.join("state/skill-trust.toml");

    let pending = workspace_skill_report_with_trust(&root, &trust_store).unwrap();
    assert_eq!(pending.skills[0].trust_state, SkillTrustState::Unknown);
    assert!(!pending.skills[0].usable);

    let approval = trust_workspace_skill(
        &root,
        &trust_store,
        "repo-change",
        &SkillTrustOptions {
            approve: true,
            agentlibre_version: "test-version".to_string(),
        },
    )
    .unwrap();
    assert!(!approval.has_errors());
    assert!(approval.wrote);

    let trusted = workspace_skill_report_with_trust(&root, &trust_store).unwrap();
    assert_eq!(trusted.skills[0].trust_state, SkillTrustState::TrustedLocal);
    assert!(trusted.skills[0].usable);

    let registry = trusted_workspace_registry(&root, &trust_store).unwrap();
    let trusted_skill = registry
        .get(&agl_tools::SkillId::new("repo-change").unwrap())
        .expect("trusted workspace skill should be registered");
    assert!(trusted_skill.permits_context_injection());

    let revoke = revoke_workspace_skill(&root, &trust_store, "repo-change").unwrap();
    assert!(!revoke.has_errors());
    assert!(revoke.wrote);
    let record = revoke
        .record
        .expect("revoke should return persisted record");
    assert!(record.revoked);
    assert!(record.revoked_at.is_some());
    assert_eq!(record.agentlibre_version, "test-version");
    let revoked = workspace_skill_report_with_trust(&root, &trust_store).unwrap();
    assert_eq!(revoked.skills[0].trust_state, SkillTrustState::Revoked);
    assert!(!revoked.skills[0].usable);

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn pinned_same_name_workspace_skill_overrides_builtin_when_trusted() {
    let (root, source) = clean_skills_submodule_fixture_with_skill("same-name", "repo-review");
    lock_workspace_skills(&root, &SkillLockOptions { dry_run: false }).unwrap();
    let trust_store = root.join("state/skill-trust.toml");

    let approval = trust_workspace_skill(
        &root,
        &trust_store,
        "repo-review",
        &SkillTrustOptions {
            approve: true,
            agentlibre_version: "test-version".to_string(),
        },
    )
    .unwrap();
    assert!(!approval.has_errors());

    let trusted = workspace_skill_report_with_trust(&root, &trust_store).unwrap();
    assert!(trusted.skills[0].shadowed_by_builtin);
    assert!(trusted.skills[0].overrides_builtin);
    assert_eq!(trusted.skills[0].trust_state, SkillTrustState::TrustedLocal);
    assert!(trusted.skills[0].usable);

    let registry = trusted_workspace_registry(&root, &trust_store).unwrap();
    let skill = registry
        .get(&agl_tools::SkillId::new("repo-review").unwrap())
        .expect("trusted workspace repo-review should be registered");
    assert_eq!(skill.harness.source, SkillSource::Workspace);

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn same_name_workspace_skill_reports_routing_broadening() {
    let (root, source) = clean_skills_submodule_fixture_with_allowed_tools(
        "same-name-broad-routing",
        "repo-review",
        &["fs.edit"],
    );
    lock_workspace_skills(&root, &SkillLockOptions { dry_run: false }).unwrap();
    let trust_store = root.join("state/skill-trust.toml");

    let pending = workspace_skill_report_with_trust(&root, &trust_store).unwrap();
    assert!(pending.skills[0].shadowed_by_builtin);
    assert!(pending.skills[0].broadens_builtin_routing);
    assert!(
        pending.skills[0]
            .warnings
            .contains(&"broadens_builtin_routing".to_string())
    );
    assert!(!pending.skills[0].usable);

    let approval = trust_workspace_skill(
        &root,
        &trust_store,
        "repo-review",
        &SkillTrustOptions {
            approve: true,
            agentlibre_version: "test-version".to_string(),
        },
    )
    .unwrap();
    assert!(!approval.has_errors());

    let trusted = workspace_skill_report_with_trust(&root, &trust_store).unwrap();
    assert!(trusted.skills[0].overrides_builtin);
    assert!(trusted.skills[0].broadens_builtin_routing);
    assert!(trusted.skills[0].usable);

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn trust_rejects_missing_requestable_tools() {
    let (root, source) = clean_skills_submodule_fixture_with_routing(
        "missing-requestable-tool",
        "repo-change",
        &[],
        &["missing.tool"],
    );
    lock_workspace_skills(&root, &SkillLockOptions { dry_run: false }).unwrap();
    let trust_store = root.join("state/skill-trust.toml");

    let err = trust_workspace_skill(
        &root,
        &trust_store,
        "repo-change",
        &SkillTrustOptions {
            approve: true,
            agentlibre_version: "test-version".to_string(),
        },
    )
    .unwrap_err();

    let message = err.to_string();
    assert!(message.contains("missing.tool"), "{message}");
    assert!(message.contains("requestable_tools"), "{message}");

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(source).unwrap();
}

fn clean_skills_submodule_fixture(label: &str) -> (PathBuf, PathBuf) {
    clean_skills_submodule_fixture_with_skill(label, "repo-change")
}

fn clean_skills_submodule_fixture_with_skill(label: &str, skill_name: &str) -> (PathBuf, PathBuf) {
    clean_skills_submodule_fixture_with_allowed_tools(label, skill_name, &[])
}

fn clean_skills_submodule_fixture_with_allowed_tools(
    label: &str,
    skill_name: &str,
    allowed_tools: &[&str],
) -> (PathBuf, PathBuf) {
    clean_skills_submodule_fixture_with_routing(label, skill_name, allowed_tools, &[])
}

fn clean_skills_submodule_fixture_with_routing(
    label: &str,
    skill_name: &str,
    allowed_tools: &[&str],
    requestable_tools: &[&str],
) -> (PathBuf, PathBuf) {
    let source = temp_root(&format!("{label}-skills-source"));
    init_git_repo(&source);
    write_workspace_skill(
        &source.join("agl").join(skill_name),
        skill_name,
        allowed_tools,
        requestable_tools,
    );
    git_run(&source, ["add", "."]);
    git_run(
        &source,
        [
            "-c",
            "user.name=AgentLIBRE Test",
            "-c",
            "user.email=agentlibre-test@example.invalid",
            "commit",
            "-q",
            "-m",
            "add workspace skill",
        ],
    );

    let root = temp_root(&format!("{label}-skills-submodule"));
    init_git_repo(&root);
    init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
    git_run(
        &root,
        [
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            source.to_str().unwrap(),
            ".agl/skills",
        ],
    );
    let manifest_path = root.join(agl_repo::WORKSPACE_MANIFEST_PATH);
    let manifest = fs::read_to_string(&manifest_path)
        .unwrap()
        .replace(DEFAULT_SKILLS_URL, source.to_str().unwrap());
    fs::write(&manifest_path, manifest).unwrap();

    (root, source)
}

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agl-skills-workspace-{label}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
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

fn git_run<const N: usize>(root: &Path, args: [&str; N]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git in {}: {err}", root.display()));
    assert!(
        output.status.success(),
        "git failed in {}\nstdout:\n{}\nstderr:\n{}",
        root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_workspace_skill(
    skill_dir: &Path,
    name: &str,
    allowed_tools: &[&str],
    requestable_tools: &[&str],
) {
    fs::create_dir_all(skill_dir).unwrap();
    let allowed_tools = render_yaml_string_list(allowed_tools);
    let requestable_tools = render_yaml_string_list(requestable_tools);
    fs::write(
        skill_dir.join("SKILL.md"),
        format!(
            r#"---
name: {name}
description: Review repository changes.
version: 1
source: workspace
pack: agl
required_hooks:
  - repo_path.validate
allowed_tools:
{allowed_tools}
requestable_tools:
{requestable_tools}
context_budget_tokens: 256
references:
  include: []
guarantees:
  - repository paths are checked
---
Body.
"#
        ),
    )
    .unwrap();
}

fn write_workspace_skill_with_folders(skill_dir: &Path, name: &str, folders_yaml: &str) {
    fs::create_dir_all(skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        format!(
            r#"---
name: {name}
description: Review repository changes.
version: 1
source: workspace
pack: agl
required_hooks:
  - repo_path.validate
allowed_tools:
  []
requestable_tools:
  []
context_budget_tokens: 256
references:
  include: []
{folders_yaml}
guarantees:
  - repository paths are checked
---
Body.
"#
        ),
    )
    .unwrap();
}

fn render_yaml_string_list(values: &[&str]) -> String {
    if values.is_empty() {
        "  []".to_string()
    } else {
        values
            .iter()
            .map(|value| format!("  - {value}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}
