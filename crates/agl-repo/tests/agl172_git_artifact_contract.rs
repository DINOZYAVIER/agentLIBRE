use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use agl_artifact::{
    ArtifactCommitEntry, ArtifactCommitEntryKind, ArtifactCommitRequest,
    MemoryArtifactCommitRepository,
};
use agl_kernel::{
    ArtifactAccess, ArtifactDeclaration, ArtifactId, ArtifactKindId, DeclarationDigest, EffectId,
    ExtensionId, ToolDelivery, ToolEffectCorrelation, ToolEffectLifecycleState, ToolEffectMachine,
    ToolId,
};
use agl_repo::{
    ArtifactBindingError, ArtifactCommitError, ArtifactCommitFailpoint, ArtifactGitRepository,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

struct GitFixture {
    root: PathBuf,
    parent: PathBuf,
    child: PathBuf,
}

impl Drop for GitFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn configure(path: &Path) {
    git(path, &["config", "user.name", "AGL Test"]);
    git(path, &["config", "user.email", "agl-test@example.invalid"]);
}

fn fixture() -> GitFixture {
    let root = std::env::temp_dir().join(format!(
        "agl172-git-fixture-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let child = root.join("child");
    let parent = root.join("parent");
    fs::create_dir_all(&child).unwrap();
    fs::create_dir_all(&parent).unwrap();
    git(&child, &["init"]);
    configure(&child);
    fs::write(child.join("README.md"), "initial\n").unwrap();
    git(&child, &["add", "README.md"]);
    git(&child, &["commit", "-m", "initial child"]);
    git(&parent, &["init"]);
    configure(&parent);
    fs::write(parent.join("README.md"), "parent\n").unwrap();
    git(&parent, &["add", "README.md"]);
    git(&parent, &["commit", "-m", "initial parent"]);
    git(
        &parent,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--name",
            "core.repo:tasks",
            child.to_str().unwrap(),
            ".agl/tasks",
        ],
    );
    git(&parent, &["commit", "-am", "register tasks artifact"]);
    let child = parent.join(".agl/tasks");
    GitFixture {
        root,
        parent,
        child,
    }
}

fn declaration() -> ArtifactDeclaration {
    ArtifactDeclaration::new(
        ArtifactId::new("core.repo:tasks").unwrap(),
        ArtifactKindId::new("agentlibre.task-specs").unwrap(),
        [ArtifactAccess::ReadTree, ArtifactAccess::MutateTree],
    )
    .unwrap()
}

fn request(entries: impl IntoIterator<Item = ArtifactCommitEntry>) -> ArtifactCommitRequest {
    let sequence = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut effect = ToolEffectMachine::new(
        format!("artifact-commit-call-{sequence}"),
        "core.repo:artifact.commit".parse::<ToolId>().unwrap(),
        "core.repo".parse::<ExtensionId>().unwrap(),
        DeclarationDigest::from_json(&serde_json::json!({"fixture": sequence})),
        ToolDelivery::AtMostOnce,
        BTreeSet::from([
            "agl:artifact.repository".parse::<EffectId>().unwrap(),
            "agl:repo.gitlink".parse::<EffectId>().unwrap(),
        ]),
    );
    effect
        .apply(ToolEffectLifecycleState::Admitted, Vec::new(), None)
        .unwrap();
    let started = effect
        .apply(ToolEffectLifecycleState::Started, Vec::new(), None)
        .unwrap();
    ArtifactCommitRequest::new(
        format!("artifact-commit-operation-{sequence}"),
        ToolEffectCorrelation::from_record(&started),
        ArtifactId::new("core.repo:tasks").unwrap(),
        entries,
        "Update task specifications",
    )
    .unwrap()
}

// AGL172-017, AGL172-033, AGL172-040 and AGL172-055.
#[test]
fn ordinary_named_git_submodule_is_the_only_accepted_binding_source() {
    let fixture = fixture();
    let repository = ArtifactGitRepository::open(&fixture.parent).unwrap();
    let binding = repository.verify_binding(&declaration()).unwrap();
    assert_eq!(binding.artifact_id().as_str(), "core.repo:tasks");
    assert_eq!(binding.submodule_path(), Path::new(".agl/tasks"));
    assert_eq!(binding.gitlink(), binding.child_head());

    git(&fixture.parent, &["rm", "-f", ".agl/tasks"]);
    assert!(matches!(
        repository.verify_binding(&declaration()),
        Err(ArtifactBindingError::NotRegistered { .. })
    ));
    assert!(fixture.parent.join(".git/modules/core.repo:tasks").exists());
}

// AGL172-017, AGL172-033 and AGL172-040.
#[test]
fn name_path_gitlink_head_kind_and_declaration_mismatches_fail_closed() {
    let fixture = fixture();
    let repository = ArtifactGitRepository::open(&fixture.parent).unwrap();

    let mut wrong_id = declaration();
    wrong_id.id = ArtifactId::new("core.repo:reviews").unwrap();
    assert!(matches!(
        repository.verify_binding(&wrong_id),
        Err(ArtifactBindingError::MissingNamedSubmodule { .. })
    ));

    git(&fixture.child, &["checkout", "HEAD~0"]);
    fs::write(fixture.child.join("drift.md"), "drift\n").unwrap();
    git(&fixture.child, &["add", "drift.md"]);
    git(&fixture.child, &["commit", "-m", "unrecorded child"]);
    assert!(matches!(
        repository.verify_binding(&declaration()),
        Err(ArtifactBindingError::ChildHeadMismatch { .. })
    ));
}

// AGL172-036. A fresh clone reconstructs durable Artifact identity entirely
// from committed .gitmodules/gitlink/child state, without local XDG data.
#[test]
fn fresh_clone_reconstructs_exact_binding_without_local_runtime_state() {
    let fixture = fixture();
    let clone = fixture.root.join("clone");
    let output = Command::new("git")
        .args([
            "-c",
            "protocol.file.allow=always",
            "clone",
            "--recurse-submodules",
            fixture.parent.to_str().unwrap(),
            clone.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let repository = ArtifactGitRepository::open(&clone).unwrap();
    let binding = repository.verify_binding(&declaration()).unwrap();
    assert_eq!(binding.submodule_path(), Path::new(".agl/tasks"));
    assert_eq!(binding.gitlink(), binding.child_head());
    for local in ["cache", "state", "run", "sockets", "tmp"] {
        assert!(!clone.join(".agl").join(local).exists());
    }
}

// AGL172-034 and AGL172-041.
#[test]
fn commit_request_rejects_empty_duplicate_directory_missing_and_unchanged_entries() {
    let fixture = fixture();
    let repository = ArtifactGitRepository::open(&fixture.parent).unwrap();
    let binding = repository.verify_binding(&declaration()).unwrap();
    let operations = MemoryArtifactCommitRepository::default();

    assert!(
        ArtifactCommitRequest::new(
            "empty-operation",
            request_correlation("empty-call"),
            ArtifactId::new("core.repo:tasks").unwrap(),
            [],
            "empty",
        )
        .is_err()
    );
    assert!(
        ArtifactCommitRequest::new(
            "duplicate-operation",
            request_correlation("duplicate-call"),
            ArtifactId::new("core.repo:tasks").unwrap(),
            [
                ArtifactCommitEntry::new("README.md", ArtifactCommitEntryKind::Update).unwrap(),
                ArtifactCommitEntry::new("README.md", ArtifactCommitEntryKind::Update).unwrap(),
            ],
            "duplicate",
        )
        .is_err()
    );
    assert!(ArtifactCommitEntry::new(".", ArtifactCommitEntryKind::Update).is_err());
    for entries in [
        vec![ArtifactCommitEntry::new("missing.md", ArtifactCommitEntryKind::Update).unwrap()],
        vec![ArtifactCommitEntry::new("README.md", ArtifactCommitEntryKind::Update).unwrap()],
    ] {
        assert!(
            repository
                .commit_artifact(&binding, request(entries), &operations)
                .is_err()
        );
    }
}

fn request_correlation(call_id: &str) -> ToolEffectCorrelation {
    let mut effect = ToolEffectMachine::new(
        call_id,
        "core.repo:artifact.commit".parse::<ToolId>().unwrap(),
        "core.repo".parse::<ExtensionId>().unwrap(),
        DeclarationDigest::from_json(&serde_json::json!({"fixture": call_id})),
        ToolDelivery::AtMostOnce,
        BTreeSet::from([
            "agl:artifact.repository".parse::<EffectId>().unwrap(),
            "agl:repo.gitlink".parse::<EffectId>().unwrap(),
        ]),
    );
    effect
        .apply(ToolEffectLifecycleState::Admitted, Vec::new(), None)
        .unwrap();
    let started = effect
        .apply(ToolEffectLifecycleState::Started, Vec::new(), None)
        .unwrap();
    ToolEffectCorrelation::from_record(&started)
}

// AGL172-023, AGL172-024, AGL172-026, AGL172-035, AGL172-038,
// AGL172-039, AGL172-041 and AGL172-067.
#[test]
fn two_phase_commit_preserves_unrelated_real_index_entries_and_is_local_only() {
    let fixture = fixture();
    fs::write(fixture.child.join("README.md"), "selected\n").unwrap();
    fs::write(fixture.child.join("unrelated.md"), "unrelated\n").unwrap();
    git(&fixture.child, &["add", "unrelated.md"]);
    fs::write(fixture.parent.join("README.md"), "parent unrelated\n").unwrap();
    git(&fixture.parent, &["add", "README.md"]);

    let child_unrelated_before = git(&fixture.child, &["ls-files", "--stage", "unrelated.md"]);
    let parent_unrelated_before = git(&fixture.parent, &["ls-files", "--stage", "README.md"]);
    let repository = ArtifactGitRepository::open(&fixture.parent).unwrap();
    let binding = repository.verify_binding(&declaration()).unwrap();
    let operations = MemoryArtifactCommitRepository::default();
    let evidence = repository
        .commit_artifact(
            &binding,
            request([
                ArtifactCommitEntry::new("README.md", ArtifactCommitEntryKind::Update).unwrap(),
            ]),
            &operations,
        )
        .unwrap();

    assert_eq!(
        git(&fixture.child, &["status", "--porcelain", "README.md"]),
        ""
    );
    assert_eq!(
        git(
            &fixture.parent,
            &["diff", "--cached", "--name-only", "--", ".agl/tasks"]
        ),
        ""
    );
    assert_eq!(
        git(&fixture.child, &["ls-files", "--stage", "unrelated.md"]),
        child_unrelated_before
    );
    assert_eq!(
        git(&fixture.parent, &["ls-files", "--stage", "README.md"]),
        parent_unrelated_before
    );
    assert_eq!(
        git(&fixture.child, &["rev-parse", "HEAD"]),
        evidence.child_commit()
    );
    assert_eq!(
        git(&fixture.parent, &["rev-parse", "HEAD"]),
        evidence.parent_commit()
    );
    assert_eq!(
        operations
            .load(evidence.operation_id())
            .unwrap()
            .state_name(),
        "committed"
    );
    assert_eq!(repository.network_operations(), 0);
}

// AGL172-025 and AGL172-029. Idempotency binds the complete typed input, not
// only the operation id or Tool Effect correlation.
#[test]
fn replay_with_different_commit_input_is_a_conflict() {
    let fixture = fixture();
    fs::write(fixture.child.join("README.md"), "selected\n").unwrap();
    let repository = ArtifactGitRepository::open(&fixture.parent).unwrap();
    let binding = repository.verify_binding(&declaration()).unwrap();
    let operations = MemoryArtifactCommitRepository::default();
    let first =
        request([ArtifactCommitEntry::new("README.md", ArtifactCommitEntryKind::Update).unwrap()]);
    let changed = ArtifactCommitRequest::new(
        first.operation_id(),
        first.correlation().clone(),
        first.artifact_id().clone(),
        [ArtifactCommitEntry::new("README.md", ArtifactCommitEntryKind::Update).unwrap()],
        "A different message",
    )
    .unwrap();

    repository
        .commit_artifact(&binding, first, &operations)
        .unwrap();
    let error = repository
        .commit_artifact(&binding, changed, &operations)
        .unwrap_err();

    assert!(matches!(error, ArtifactCommitError::Conflict(_)));
}

// AGL172-025, AGL172-028, AGL172-043 and AGL172-067.
#[test]
fn every_ref_update_failpoint_recovers_without_duplicate_commits() {
    for failpoint in [
        ArtifactCommitFailpoint::AfterChildRefUpdate,
        ArtifactCommitFailpoint::AfterChildDurableRecord,
        ArtifactCommitFailpoint::AfterParentRefUpdate,
        ArtifactCommitFailpoint::BeforeTerminalEvidence,
    ] {
        let fixture = fixture();
        fs::write(fixture.child.join("README.md"), "selected\n").unwrap();
        let repository = ArtifactGitRepository::open(&fixture.parent).unwrap();
        let binding = repository.verify_binding(&declaration()).unwrap();
        let operations = MemoryArtifactCommitRepository::default();
        let child_count_initial = git(&fixture.child, &["rev-list", "--count", "HEAD"])
            .parse::<u64>()
            .unwrap();
        let parent_count_initial = git(&fixture.parent, &["rev-list", "--count", "HEAD"])
            .parse::<u64>()
            .unwrap();
        let request =
            request([
                ArtifactCommitEntry::new("README.md", ArtifactCommitEntryKind::Update).unwrap(),
            ]);
        let operation_id = request.operation_id().to_owned();
        let interrupted =
            repository.commit_artifact_with_failpoint(&binding, request, &operations, failpoint);
        assert!(matches!(
            interrupted,
            Err(ArtifactCommitError::InjectedFailure { .. })
        ));
        let recovered = repository.recover_incomplete(&operations).unwrap();
        assert_eq!(recovered.len(), 1);
        assert!(recovered[0].is_committed());
        assert_eq!(
            git(&fixture.child, &["rev-list", "--count", "HEAD"])
                .parse::<u64>()
                .unwrap(),
            child_count_initial + 1
        );
        assert_eq!(
            git(&fixture.parent, &["rev-list", "--count", "HEAD"])
                .parse::<u64>()
                .unwrap(),
            parent_count_initial + 1
        );
        let child_after = git(&fixture.child, &["rev-parse", "HEAD"]);
        let parent_after = git(&fixture.parent, &["rev-parse", "HEAD"]);
        assert!(
            repository
                .recover_incomplete(&operations)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            operations.load(&operation_id).unwrap().state_name(),
            "committed"
        );
        assert_eq!(git(&fixture.child, &["rev-parse", "HEAD"]), child_after);
        assert_eq!(git(&fixture.parent, &["rev-parse", "HEAD"]), parent_after);
    }
}

// AGL172-029.
#[test]
fn independent_parent_gitlink_change_becomes_immutable_conflict() {
    let fixture = fixture();
    fs::write(fixture.child.join("README.md"), "selected\n").unwrap();
    let repository = ArtifactGitRepository::open(&fixture.parent).unwrap();
    let binding = repository.verify_binding(&declaration()).unwrap();
    let operations = MemoryArtifactCommitRepository::default();
    let request =
        request([ArtifactCommitEntry::new("README.md", ArtifactCommitEntryKind::Update).unwrap()]);
    let operation_id = request.operation_id().to_owned();
    repository
        .commit_artifact_with_failpoint(
            &binding,
            request,
            &operations,
            ArtifactCommitFailpoint::AfterChildDurableRecord,
        )
        .unwrap_err();
    let retained_child = git(&fixture.child, &["rev-parse", "HEAD"]);
    git(
        &fixture.parent,
        &[
            "update-index",
            "--cacheinfo",
            "160000",
            &"9".repeat(40),
            ".agl/tasks",
        ],
    );

    let result = repository.recover_incomplete(&operations).unwrap();
    assert!(result[0].is_conflict());
    assert_eq!(git(&fixture.child, &["rev-parse", "HEAD"]), retained_child);
    assert!(
        repository
            .recover_incomplete(&operations)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        operations.load(&operation_id).unwrap().state_name(),
        "conflict"
    );
}

// AGL172-061 and AGL172-062.
#[test]
fn task_validator_accepts_only_the_fixed_verified_read_only_tasks_handle() {
    let fixture = fixture();
    let task = fixture.child.join("AGL-999_fixture");
    fs::create_dir_all(&task).unwrap();
    fs::write(
        task.join("00_overview.md"),
        "---\nstatus: planned\n---\n\n# Fixture\n\n## Problem\n\nExact.\n\n## Goal\n\nExact.\n\n## Scope\n\nExact.\n\n## Non-Goals\n\nExact.\n\n## Implementation\n\nExact.\n\n## Acceptance Criteria\n\nExact.\n\n## Verification\n\nExact.\n",
    )
    .unwrap();
    let repository = ArtifactGitRepository::open(&fixture.parent).unwrap();
    let binding = repository.verify_binding(&declaration()).unwrap();
    let handle = binding.into_handle(ArtifactAccess::ReadTree).unwrap();
    let report =
        agl_repo::verify_task_specs(&handle, &agl_repo::TaskSpecVerifyOptions { strict: true })
            .unwrap();
    assert!(!report.should_fail(true));

    let wrong = ArtifactDeclaration::new(
        ArtifactId::new("core.repo:reviews").unwrap(),
        ArtifactKindId::new("agentlibre.review-pack").unwrap(),
        [ArtifactAccess::ReadTree],
    )
    .unwrap();
    assert!(repository.verify_binding(&wrong).is_err());
}

// AGL172-062.
#[cfg(unix)]
#[test]
fn task_validator_rejects_symlink_escape_from_verified_artifact_tree() {
    use std::os::unix::fs::symlink;

    let fixture = fixture();
    let outside = fixture.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("00_overview.md"), "outside").unwrap();
    symlink(&outside, fixture.child.join("AGL-999_escape")).unwrap();
    let repository = ArtifactGitRepository::open(&fixture.parent).unwrap();
    let handle = repository
        .verify_binding(&declaration())
        .unwrap()
        .into_handle(ArtifactAccess::ReadTree)
        .unwrap();
    let error =
        agl_repo::verify_task_specs(&handle, &agl_repo::TaskSpecVerifyOptions { strict: true })
            .unwrap_err();
    assert!(error.to_string().contains("symlink escape"));
}
