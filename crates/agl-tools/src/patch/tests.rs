use std::io::Write as _;
use std::num::NonZeroU32;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use agl_core::agent::{
    AbsolutePath, AgentOperationKey, AuthorityGrantSet, RelativePath, ToolFailureEffect,
    WorkspaceScope,
};
use agl_core::{AgentRunId, AuthorityGrant, CanonicalJson, EffectId};
use agl_tool_api::{ServiceGitNotStarted, ToolCancellation};

use super::*;
use crate::WorkspaceMutation;
use crate::filesystem::{APPLY_PATCH, WRITE_EFFECT};

/// Unconfined test runner with the same contract as the daemon callback.
fn real_git() -> ServiceGit {
    Arc::new(|request: ServiceGitRequest| {
        let mut child = Command::new("git")
            .args(&request.args)
            .current_dir(&request.cwd)
            .env_clear()
            .envs(request.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| ServiceGitNotStarted)?;
        let mut stdin = child.stdin.take().unwrap();
        let input = request.stdin;
        let writer = thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let output = child.wait_with_output().unwrap();
        writer.join().unwrap();
        Ok(ServiceGitOutput {
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
            ..Default::default()
        })
    })
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agl-patch-{name}-{}", AgentRunId::generate()));
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn context(root: &Path, working_directory: &str) -> ToolContext {
    ToolContext::new(
        AgentOperationKey {
            run_id: AgentRunId::generate(),
            ordinal: NonZeroU32::MIN,
        },
        None,
        WorkspaceScope {
            root: AbsolutePath::try_from(root.to_string_lossy().into_owned()).unwrap(),
            working_directory: RelativePath::try_from(working_directory.to_owned()).unwrap(),
        },
        AuthorityGrantSet(vec![AuthorityGrant {
            effect: EffectId::new(WRITE_EFFECT).unwrap(),
            scope: CanonicalJson::new(json!({"root": root})).unwrap(),
        }]),
        i64::MAX,
        65_536,
        ToolCancellation::new(),
    )
}

fn run(root: &Path, patch: &str) -> Result<Value, ToolFailure> {
    run_with(root, ".", patch, &real_git())
}

fn run_with(
    root: &Path,
    working_directory: &str,
    patch: &str,
    git: &ServiceGit,
) -> Result<Value, ToolFailure> {
    let mutations: WorkspaceMutation = Arc::new(|_: &Path| Arc::new(Mutex::new(())));
    crate::filesystem::execute(
        APPLY_PATCH,
        &context(root, working_directory),
        json!({"patch": patch}),
        &mutations,
        git,
    )
    .map(|(value, _)| value)
}

fn modify(path: &str, old: &str, new: &str) -> String {
    format!(
        "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-{old}\n+{new}\n"
    )
}

fn create(path: &str, line: &str) -> String {
    format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+{line}\n"
    )
}

fn assert_no_effect(failure: &ToolFailure, kind: ToolFailureKind) {
    assert_eq!(failure.kind, kind);
    let ToolFailureEffect::None { next_actions, .. } = &failure.effect else {
        panic!("expected a proven no-effect failure: {failure:?}");
    };
    assert!(next_actions[0].contains("Nothing was changed"));
}

#[test]
fn modifies_creates_deletes_and_renames_regular_files() {
    let root = temp_dir("ops");
    fs::write(root.join("a.txt"), "before\n").unwrap();
    fs::write(root.join("gone.txt"), "bye\n").unwrap();
    fs::write(root.join("old.txt"), "moved\n").unwrap();
    let patch = [
        modify("a.txt", "before", "after"),
        create("dir/new.txt", "fresh"),
        "diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n".to_owned(),
        "diff --git a/old.txt b/renamed.txt\nsimilarity index 100%\nrename from old.txt\nrename to renamed.txt\n".to_owned(),
    ]
    .concat();
    let value = run(&root, &patch).unwrap();
    assert_eq!(value["status"], "committed");
    assert_eq!(
        value["paths"],
        json!(["a.txt", "dir/new.txt", "gone.txt", "old.txt", "renamed.txt"])
    );
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "after\n");
    assert_eq!(
        fs::read_to_string(root.join("dir/new.txt")).unwrap(),
        "fresh\n"
    );
    assert!(!root.join("gone.txt").exists());
    assert!(!root.join("old.txt").exists());
    assert_eq!(
        fs::read_to_string(root.join("renamed.txt")).unwrap(),
        "moved\n"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn failed_check_changes_nothing_and_says_so() {
    let root = temp_dir("check");
    fs::write(root.join("a.txt"), "actual\n").unwrap();
    fs::write(root.join("b.txt"), "b\n").unwrap();
    let patch = [
        modify("b.txt", "b", "changed"),
        modify("a.txt", "different", "after"),
    ]
    .concat();
    let failure = run(&root, &patch).unwrap_err();
    assert_no_effect(&failure, ToolFailureKind::InvalidInput);
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "actual\n");
    assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "b\n");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn mode_change_between_regular_modes_is_applied() {
    let root = temp_dir("mode");
    fs::write(root.join("run.sh"), "echo\n").unwrap();
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    run(
        &root,
        "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n",
    )
    .unwrap();
    let mode = fs::metadata(root.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn symlink_and_submodule_modes_and_binary_patches_are_rejected() {
    let root = temp_dir("kinds");
    for patch in [
        "diff --git a/l b/l\nnew file mode 120000\n--- /dev/null\n+++ b/l\n@@ -0,0 +1 @@\n+target\n\\ No newline at end of file\n",
        "diff --git a/sub b/sub\nnew file mode 160000\nindex 0000000..1111111\n--- /dev/null\n+++ b/sub\n@@ -0,0 +1 @@\n+Subproject commit 1111111111111111111111111111111111111111\n",
        "diff --git a/b.bin b/b.bin\nnew file mode 100644\nindex 0000000..e69de29\nGIT binary patch\nliteral 0\nHcmV?d00001\n\n",
        "diff --git a/b.bin b/b.bin\nnew file mode 100644\nBinary files /dev/null and b/b.bin differ\n",
    ] {
        let failure = run(&root, patch).unwrap_err();
        assert_no_effect(&failure, ToolFailureKind::InvalidInput);
    }
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn input_and_path_count_limits_are_enforced() {
    let root = temp_dir("limits");
    let large = format!(
        "{}{}",
        create("big.txt", "x"),
        "#".repeat(MAX_PATCH_INPUT_BYTES)
    );
    assert_no_effect(
        &run(&root, &large).unwrap_err(),
        ToolFailureKind::InvalidInput,
    );

    let many = (0..=MAX_PATCH_PATHS)
        .map(|index| create(&format!("f{index}.txt"), "x"))
        .collect::<String>();
    assert_no_effect(
        &run(&root, &many).unwrap_err(),
        ToolFailureKind::InvalidInput,
    );
    let exact = (0..MAX_PATCH_PATHS)
        .map(|index| create(&format!("f{index}.txt"), "x"))
        .collect::<String>();
    run(&root, &exact).unwrap();
    assert_eq!(fs::read_dir(&root).unwrap().count(), MAX_PATCH_PATHS);

    // A rename counts its source and destination separately: 33 renames = 66.
    let renames = (0..33)
        .map(|index| {
            format!(
                "diff --git a/f{index}.txt b/r{index}.txt\nsimilarity index 100%\nrename from f{index}.txt\nrename to r{index}.txt\n"
            )
        })
        .collect::<String>();
    assert_no_effect(
        &run(&root, &renames).unwrap_err(),
        ToolFailureKind::InvalidInput,
    );
    assert!(root.join("f0.txt").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn symlink_parents_and_symlink_targets_are_rejected() {
    let root = temp_dir("links");
    let outside = temp_dir("links-outside");
    symlink(&outside, root.join("link")).unwrap();
    fs::write(root.join("real.txt"), "x\n").unwrap();
    symlink(root.join("real.txt"), root.join("alias.txt")).unwrap();

    let failure = run(&root, &create("link/escaped.txt", "x")).unwrap_err();
    assert_no_effect(&failure, ToolFailureKind::InvalidInput);
    assert!(!outside.join("escaped.txt").exists());
    let failure = run(&root, &modify("alias.txt", "x", "y")).unwrap_err();
    assert_no_effect(&failure, ToolFailureKind::InvalidInput);
    assert_eq!(fs::read_to_string(root.join("real.txt")).unwrap(), "x\n");
    // A pure rename's source is not in `--numstat`; it is still validated.
    let rename = "diff --git a/alias.txt b/moved.txt\nsimilarity index 100%\nrename from alias.txt\nrename to moved.txt\n";
    let failure = run(&root, rename).unwrap_err();
    assert_no_effect(&failure, ToolFailureKind::InvalidInput);
    assert!(fs::symlink_metadata(root.join("alias.txt")).is_ok());
    assert!(!root.join("moved.txt").exists());
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(outside).unwrap();
}

#[test]
fn git_metadata_absolute_and_traversal_paths_are_rejected() {
    let root = temp_dir("paths");
    let parent = root.parent().unwrap().to_owned();
    let nested = root.join("work");
    fs::create_dir_all(&nested).unwrap();
    for path in [
        ".git/hooks/post-merge",
        "sub/.GIT/config",
        "../escaped.txt",
        "/abs-escaped.txt",
    ] {
        let patch = format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+x\n"
        );
        let failure = run_with(&nested, ".", &patch, &real_git()).unwrap_err();
        assert_no_effect(&failure, ToolFailureKind::InvalidInput);
    }
    assert!(!nested.join(".git").exists());
    assert!(!root.join("escaped.txt").exists());
    assert!(!parent.join("escaped.txt").exists());
    assert!(!Path::new("/abs-escaped.txt").exists());
    // Direct validation covers a header Git itself might accept.
    for path in ["", ".", "./a", "a/../b", "/a", "a/.git/b", ".git"] {
        assert!(validate_path(&root, path).is_err(), "{path}");
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn paths_are_relative_to_the_workspace_root_not_the_working_directory() {
    let root = temp_dir("rootrel");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub/a.txt"), "before\n").unwrap();
    fs::write(root.join("a.txt"), "root\n").unwrap();
    let working = "sub";
    // `a.txt` names the root file, not `sub/a.txt`.
    run_with(
        &root,
        working,
        &modify("a.txt", "root", "root2"),
        &real_git(),
    )
    .unwrap();
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "root2\n");
    assert_eq!(
        fs::read_to_string(root.join("sub/a.txt")).unwrap(),
        "before\n"
    );
    run_with(
        &root,
        working,
        &modify("sub/a.txt", "before", "after"),
        &real_git(),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(root.join("sub/a.txt")).unwrap(),
        "after\n"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unproven_mutating_apply_is_an_unknown_outcome_and_is_not_retried() {
    for timed_out in [false, true] {
        let root = temp_dir("unknown");
        fs::write(root.join("a.txt"), "before\n").unwrap();
        let real = real_git();
        let applies = Arc::new(Mutex::new(0u32));
        let counter = Arc::clone(&applies);
        let git: ServiceGit = Arc::new(move |request: ServiceGitRequest| {
            if request.args == ["apply", "-"] {
                *counter.lock().unwrap() += 1;
                return Ok(ServiceGitOutput {
                    exit_code: if timed_out { None } else { Some(1) },
                    timed_out,
                    ..Default::default()
                });
            }
            real(request)
        });
        let failure = run_with(&root, ".", &modify("a.txt", "before", "after"), &git).unwrap_err();
        assert_eq!(failure.kind, ToolFailureKind::OutcomeUnknown);
        assert_eq!(failure.effect, ToolFailureEffect::Unknown);
        assert_eq!(*applies.lock().unwrap(), 1);
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn write_authority_is_required_and_a_receipt_is_emitted() {
    let root = temp_dir("authority");
    fs::write(root.join("a.txt"), "before\n").unwrap();
    let mutations: WorkspaceMutation = Arc::new(|_: &Path| Arc::new(Mutex::new(())));
    let mut denied = context(&root, ".");
    denied.authority = AuthorityGrantSet(vec![AuthorityGrant {
        effect: EffectId::new(WRITE_EFFECT).unwrap(),
        scope: CanonicalJson::new(json!({"root": "/"})).unwrap(),
    }]);
    let failure = crate::filesystem::execute(
        APPLY_PATCH,
        &denied,
        json!({"patch": modify("a.txt", "before", "after")}),
        &mutations,
        &real_git(),
    )
    .unwrap_err();
    assert_eq!(failure.kind, ToolFailureKind::Unauthorized);
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "before\n");
    let (_, receipts) = crate::filesystem::execute(
        APPLY_PATCH,
        &context(&root, "."),
        json!({"patch": modify("a.txt", "before", "after"), }),
        &mutations,
        &real_git(),
    )
    .unwrap();
    assert_eq!(receipts.len(), 1);
    assert!(
        crate::filesystem::execute(
            APPLY_PATCH,
            &context(&root, "."),
            json!({"patch": "x", "operations": []}),
            &mutations,
            &real_git(),
        )
        .is_err()
    );
    fs::remove_dir_all(root).unwrap();
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "{args:?}");
}

/// Workspace that is a Git repository whose attributes and config would run
/// filters or convert line endings for `*.txt` on any ordinary `git apply`.
fn hostile_workspace(name: &str, attributes: &str) -> (PathBuf, PathBuf) {
    let root = temp_dir(name);
    let markers = temp_dir(&format!("{name}-markers"));
    git(&root, &["init", "--quiet"]);
    fs::write(root.join(".gitattributes"), attributes).unwrap();
    fs::write(root.join(".git/info/attributes"), "*.txt filter=probe\n").unwrap();
    let marker = markers.join("ran");
    let script = format!("touch {}; cat", marker.display());
    git(&root, &["config", "filter.probe.clean", &script]);
    git(&root, &["config", "filter.probe.smudge", &script]);
    fs::write(root.join("file.txt"), "one\n").unwrap();
    (root, markers)
}

#[test]
fn git_filters_and_conversions_never_run_and_bytes_are_raw() {
    for attributes in [
        "*.txt filter=probe\n",
        "*.txt text eol=crlf\n",
        "*.txt text eol=crlf filter=probe working-tree-encoding=UTF-16 ident\n",
    ] {
        // Control: an ordinary `git apply` in this workspace does run the
        // probe (or convert), so the probe below can detect a regression.
        let (control, control_markers) = hostile_workspace("control", "*.txt filter=probe\n");
        let mut ordinary = Command::new("git")
            .args(["apply", "-"])
            .current_dir(&control)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        ordinary
            .stdin
            .take()
            .unwrap()
            .write_all(modify("file.txt", "one", "two").as_bytes())
            .unwrap();
        assert!(ordinary.wait().unwrap().success());
        let control_changed = control_markers.join("ran").exists();

        let (root, markers) = hostile_workspace("probe", attributes);
        run(&root, &modify("file.txt", "one", "two")).unwrap();
        assert!(
            !markers.join("ran").exists(),
            "a Git filter ran: {attributes}"
        );
        assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"two\n");
        // A CRLF file stays CRLF-exact where the diff says so.
        fs::write(root.join("crlf.txt"), "a\r\nb\r\n").unwrap();
        let crlf = "diff --git a/crlf.txt b/crlf.txt\n--- a/crlf.txt\n+++ b/crlf.txt\n@@ -1,2 +1,2 @@\n a\r\n-b\r\n+c\r\n";
        run(&root, crlf).unwrap();
        assert_eq!(fs::read(root.join("crlf.txt")).unwrap(), b"a\r\nc\r\n");
        assert!(
            !markers.join("ran").exists(),
            "a Git filter ran: {attributes}"
        );
        assert!(
            control_changed,
            "control did not exercise Git conversions: {attributes}"
        );
        for dir in [control, control_markers, root, markers] {
            fs::remove_dir_all(dir).unwrap();
        }
    }
}

#[test]
fn quoted_rename_paths_keep_git_quoting_and_validate_both_paths() {
    let root = temp_dir("quoted-rename");
    fs::write(root.join("old\tname"), "content\n").unwrap();
    let patch = "diff --git \"a/old\\tname\" \"b/new\\tname\"\nsimilarity index 100%\nrename from \"old\\tname\"\nrename to \"new\\tname\"\n";
    let result = run(&root, patch).unwrap();
    assert_eq!(result["status"], "committed");
    assert!(!root.join("old\tname").exists());
    assert_eq!(fs::read(root.join("new\tname")).unwrap(), b"content\n");
    let escape = "diff --git a/new b/escape\nsimilarity index 100%\nrename from \"new\\tname\"\nrename to \"../escape\"\n";
    assert!(run(&root, escape).is_err());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn expired_or_cancelled_patch_never_launches_git_including_init() {
    let root = temp_dir("no-start");
    fs::write(root.join("input"), "old\n").unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    let git: ServiceGit = Arc::new(move |_| {
        count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Err(ServiceGitNotStarted)
    });
    for cancelled in [false, true] {
        let mut context = context(&root, ".");
        if cancelled {
            context.cancellation().cancel();
        } else {
            context.deadline_at_ms = 0;
        }
        let result = apply(
            &root,
            &context,
            json!({"patch": modify("input", "old", "new")}),
            &git,
        )
        .unwrap_err();
        assert_eq!(
            result.kind,
            if cancelled {
                ToolFailureKind::Cancelled
            } else {
                ToolFailureKind::Deadline
            }
        );
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(fs::read(root.join("input")).unwrap(), b"old\n");
    fs::remove_dir_all(root).unwrap();
}
