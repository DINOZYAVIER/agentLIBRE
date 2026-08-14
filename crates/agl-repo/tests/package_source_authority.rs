use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use agl_package::{PackageSourceKind, PackageSourceTier};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    source: PathBuf,
    revision: String,
    tree: String,
}

impl Drop for Fixture {
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

fn write_manifest(workspace: &Path, revision: &str) {
    fs::write(
        workspace.join(".agl/workspace.toml"),
        format!(
            r#"version = 3
default_function = "function:gemma4-e4b@^1"

[[sources]]
id = "private"
tier = "workspace"
kind = "git"
path = ".agl/private"
url = "fixture"
rev = "{revision}"

[policy]
[config]
"#
        ),
    )
    .unwrap();
}

fn fixture() -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "agl172-package-source-authority-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let workspace = root.join("workspace");
    let source = workspace.join(".agl/private");
    fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(&source, &["config", "user.name", "AGL Test"]);
    git(
        &source,
        &["config", "user.email", "agl-test@example.invalid"],
    );
    fs::write(source.join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(source.join("README.md"), "fixture\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "fixture"]);
    let revision = git(&source, &["rev-parse", "HEAD"]);
    let tree = git(&source, &["rev-parse", "HEAD^{tree}"]);
    write_manifest(&workspace, &revision);
    Fixture {
        root,
        workspace,
        source,
        revision,
        tree,
    }
}

// AGL172-070. Repository composition owns canonical path and Git provenance
// checks and returns only typed package input to runtime.
#[test]
fn repository_prepares_exact_package_source_input() {
    let fixture = fixture();
    let input = agl_repo::package_composition_input(&fixture.workspace).unwrap();
    assert_eq!(
        input.workspace_root(),
        fixture.workspace.canonicalize().unwrap()
    );
    assert_eq!(input.sources().len(), 1);
    let source = &input.sources()[0];
    assert_eq!(source.id().as_str(), "private");
    assert_eq!(source.tier(), PackageSourceTier::Workspace);
    assert_eq!(source.kind(), PackageSourceKind::Git);
    assert_eq!(source.root(), fixture.source.canonicalize().unwrap());
    let provenance = source.provenance().unwrap();
    assert_eq!(provenance.revision(), fixture.revision);
    assert_eq!(provenance.tree(), fixture.tree);
}

// AGL172-071. Every mutable or mismatched Git source state fails before
// agl-runtime receives a PackageCompositionInput.
#[test]
fn repository_rejects_revision_dirty_untracked_and_ignored_source_state() {
    let fixture = fixture();
    write_manifest(&fixture.workspace, "HEAD~1");
    assert!(agl_repo::package_composition_input(&fixture.workspace).is_err());

    write_manifest(&fixture.workspace, &fixture.revision);
    fs::write(fixture.source.join("README.md"), "dirty\n").unwrap();
    assert!(agl_repo::package_composition_input(&fixture.workspace).is_err());
    fs::write(fixture.source.join("README.md"), "fixture\n").unwrap();

    fs::write(fixture.source.join("untracked.txt"), "untracked\n").unwrap();
    assert!(agl_repo::package_composition_input(&fixture.workspace).is_err());
    fs::remove_file(fixture.source.join("untracked.txt")).unwrap();

    fs::write(fixture.source.join("ignored.txt"), "ignored\n").unwrap();
    assert!(agl_repo::package_composition_input(&fixture.workspace).is_err());
}
