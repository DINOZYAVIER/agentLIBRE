use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use agl_core::implementation_plan::{
    PlanDigest, PlanValidationError, SliceChangedPath, WorkspaceFileDigest, WorkspacePath,
    WorkspaceSnapshot,
};
use sha2::{Digest as _, Sha256};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceState {
    pub snapshot: WorkspaceSnapshot,
    pub(crate) files: BTreeMap<String, PlanDigest>,
}

pub(crate) fn capture(root: &Path) -> Result<WorkspaceState, PlanValidationError> {
    let root = root
        .canonicalize()
        .map_err(|e| PlanValidationError(format!("cannot canonicalize workspace: {e}")))?;
    let mut files = BTreeMap::new();
    let output_directories =
        agl_runtime::read_workspace_output_directories(&root).map_err(|e| {
            PlanValidationError(format!("cannot read workspace output directories: {e:#}"))
        })?;
    let excluded = output_directories
        .iter()
        .map(|directory| root.join(directory))
        .collect::<Vec<_>>();
    walk(&root, &root, &mut files, &excluded)?;
    let mut overall = Sha256::new();
    // Bind the parsed scope as well as the manifest bytes, even if the manifest changes during capture.
    overall.update(b"output_directories\0");
    overall.update((output_directories.len() as u64).to_le_bytes());
    for directory in &output_directories {
        overall.update(directory.as_bytes());
        overall.update([0]);
    }
    let mut entries = Vec::with_capacity(files.len());
    for (path, digest) in &files {
        overall.update(path.as_bytes());
        overall.update([0]);
        overall.update(digest.as_bytes());
        entries.push(WorkspaceFileDigest {
            path: WorkspacePath::new(path.clone()).map_err(|e| PlanValidationError(e.into()))?,
            digest: digest.clone(),
        });
    }
    Ok(WorkspaceState {
        snapshot: WorkspaceSnapshot {
            digest: PlanDigest::from_bytes(overall.finalize().into()),
            files: entries,
        },
        files,
    })
}

impl WorkspaceState {
    pub(crate) fn changed_paths(&self, before: &WorkspaceState) -> Vec<SliceChangedPath> {
        let mut paths = before
            .files
            .keys()
            .chain(self.files.keys())
            .cloned()
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        paths
            .into_iter()
            .filter_map(|path| {
                let old = before.files.get(&path);
                let new = self.files.get(&path);
                (old != new).then(|| SliceChangedPath {
                    path: WorkspacePath::new(path).expect("captured paths are valid"),
                    before: old.cloned(),
                    after: new.cloned(),
                })
            })
            .collect()
    }
}

fn walk(
    root: &Path,
    directory: &Path,
    output: &mut BTreeMap<String, PlanDigest>,
    excluded: &[std::path::PathBuf],
) -> Result<(), PlanValidationError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|e| PlanValidationError(format!("cannot read workspace: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| PlanValidationError(format!("cannot enumerate workspace: {e}")))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        // Git metadata (a directory, or a gitlink file), at any depth, is not workspace content.
        if entry.file_name() == ".git" {
            continue;
        }
        if excluded.iter().any(|root| path.starts_with(root)) {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|e| PlanValidationError(format!("workspace path error: {e}")))?;
        let relative = relative
            .to_str()
            .ok_or_else(|| PlanValidationError("workspace contains a non-UTF-8 path".into()))?;
        let metadata = fs::symlink_metadata(&path)
            .map_err(|e| PlanValidationError(format!("cannot inspect {relative}: {e}")))?;
        let digest = if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path)
                .map_err(|e| PlanValidationError(format!("cannot read link {relative}: {e}")))?;
            hash_bytes(b"symlink\0", target.to_string_lossy().as_bytes())
        } else if metadata.is_dir() {
            hash_bytes(b"directory\0", &[])
        } else if metadata.is_file() {
            hash_file(&path, is_executable(&metadata))
                .map_err(|e| PlanValidationError(format!("cannot read {relative}: {e}")))?
        } else {
            return Err(PlanValidationError(format!(
                "unsupported workspace entry {relative}"
            )));
        };
        output.insert(relative.replace('\\', "/"), digest);
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            walk(root, &path, output, excluded)?;
        }
    }
    Ok(())
}

/// A regular file is executable when any of the 0o111 permission bits is set.
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

fn hash_file(path: &Path, executable: bool) -> std::io::Result<PlanDigest> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    // The executable bit selects the domain prefix; content bytes follow unchanged.
    hash.update(if executable {
        &b"executable-file\0"[..]
    } else {
        &b"file\0"[..]
    });
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        hash.update(&buffer[..read]);
    }
    Ok(PlanDigest::from_bytes(hash.finalize().into()))
}

fn hash_bytes(prefix: &[u8], bytes: &[u8]) -> PlanDigest {
    let mut hash = Sha256::new();
    hash.update(prefix);
    hash.update(bytes);
    PlanDigest::from_bytes(hash.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_exact_changes_and_ignores_order() {
        let root = std::env::temp_dir().join(format!("agl-workspace-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a"), "a").unwrap();
        let first = capture(&root).unwrap();
        fs::write(root.join("src/a"), "b").unwrap();
        fs::write(root.join("new"), "new").unwrap();
        let second = capture(&root).unwrap();
        let paths = second.changed_paths(&first);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].path.as_str(), "new");
        assert_eq!(paths[1].path.as_str(), "src/a");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn build_outputs_do_not_drift_but_sources_and_scope_changes_do() {
        let root =
            std::env::temp_dir().join(format!("agl-workspace-outputs-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(root.join("target")).unwrap();
        fs::create_dir_all(root.join("target-source")).unwrap();
        fs::write(
            root.join("agentLIBRE.toml"),
            "format = 2\nforge = []\n[workspace]\noutput_directories = [\"target\"]\n",
        )
        .unwrap();
        fs::write(root.join("target/build"), "old build").unwrap();
        fs::write(root.join("source.rs"), "old source").unwrap();
        fs::write(root.join("target-source/input"), "source").unwrap();
        fs::write(root.join(".gitignore"), "local.conf\n").unwrap();
        fs::write(root.join("local.conf"), "important config").unwrap();
        let before = capture(&root).unwrap();
        assert!(!before.files.contains_key("target"));
        assert!(!before.files.contains_key("target/build"));
        assert!(before.files.contains_key("target-source/input"));
        assert!(before.files.contains_key("local.conf"));
        fs::write(root.join("target/build"), "new build").unwrap();
        fs::write(root.join("target/cache"), "new cache").unwrap();
        assert_eq!(capture(&root).unwrap(), before);
        fs::write(root.join("source.rs"), "new source").unwrap();
        fs::write(root.join("new-source.rs"), "new untracked source").unwrap();
        let sources = capture(&root).unwrap();
        let changed = sources.changed_paths(&before);
        assert_eq!(
            changed
                .iter()
                .map(|path| path.path.as_str())
                .collect::<Vec<_>>(),
            ["new-source.rs", "source.rs"]
        );
        fs::write(
            root.join("agentLIBRE.toml"),
            "format = 2\nforge = []\n[workspace]\noutput_directories = []\n",
        )
        .unwrap();
        let scope = capture(&root).unwrap();
        assert_ne!(scope.snapshot, sources.snapshot);
        assert!(scope.files.contains_key("target/build"));
        assert!(
            scope
                .changed_paths(&sources)
                .iter()
                .any(|path| path.path.as_str() == "agentLIBRE.toml")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capture_preserves_file_digests_across_read_chunks_and_empty_files() {
        let root =
            std::env::temp_dir().join(format!("agl-workspace-chunks-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&root).unwrap();
        let bytes = (0..3 * 65_536 + 17)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(root.join("large"), &bytes).unwrap();
        fs::write(root.join("empty"), []).unwrap();
        let before = capture(&root).unwrap();
        // SHA-256 of b"file\0" followed by the fixture bytes, computed independently.
        assert_eq!(
            before.files["large"].to_string(),
            "sha256:a69aa042deb5c081cf3966079bfbbde61c0809697fc85d5bd16cc674effd43c8"
        );
        assert_eq!(before.files["empty"], hash_bytes(b"file\0", &[]));
        let mut changed = bytes;
        *changed.last_mut().unwrap() ^= 1;
        fs::write(root.join("large"), changed).unwrap();
        let after = capture(&root).unwrap();
        let changes = after.changed_paths(&before);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path.as_str(), "large");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_metadata_is_excluded_at_any_depth() {
        let root = std::env::temp_dir().join(format!("agl-workspace-git-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::create_dir_all(root.join("sub/.git")).unwrap();
        fs::write(root.join("src"), "source").unwrap();
        fs::write(root.join(".git/index"), "i1").unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/a").unwrap();
        fs::write(root.join("sub/.git/config"), "c").unwrap();
        let before = capture(&root).unwrap();
        assert!(
            !before
                .files
                .keys()
                .any(|p| p == ".git" || p.starts_with(".git/"))
        );
        assert!(!before.files.keys().any(|p| p.starts_with("sub/.git")));
        fs::write(root.join("sub/.git/config"), "c2").unwrap();
        fs::write(root.join(".git/index"), "i2").unwrap();
        fs::write(root.join(".git/objects/ab"), "obj").unwrap();
        fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        fs::write(root.join(".git/refs/heads/a"), "sha").unwrap();
        let after = capture(&root).unwrap();
        assert_eq!(after, before);
        assert!(after.changed_paths(&before).is_empty());
        // A gitlink file is excluded as well.
        fs::remove_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git"), "gitdir: ../elsewhere").unwrap();
        assert_eq!(capture(&root).unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn executable_bit_is_tracked_but_mtime_and_other_modes_are_not() {
        use std::os::unix::fs::PermissionsExt as _;
        let root =
            std::env::temp_dir().join(format!("agl-workspace-exec-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("tool.sh");
        fs::write(&file, "echo").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let before = capture(&root).unwrap();
        // Other permission bits do not matter.
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(capture(&root).unwrap(), before);
        // mtime-only change (rewrite identical bytes later) does not matter.
        let handle = fs::File::options().write(true).open(&file).unwrap();
        handle
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600))
            .unwrap();
        drop(handle);
        assert_eq!(capture(&root).unwrap(), before);
        // chmod +x changes the fingerprint and is reported as a changed path.
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        let exec = capture(&root).unwrap();
        assert_ne!(exec.snapshot, before.snapshot);
        let changes = exec.changed_paths(&before);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path.as_str(), "tool.sh");
        // Any one of the 0o111 bits counts as executable.
        fs::set_permissions(&file, fs::Permissions::from_mode(0o601)).unwrap();
        assert_eq!(capture(&root).unwrap(), exec);
        fs::remove_dir_all(root).unwrap();
    }
}
