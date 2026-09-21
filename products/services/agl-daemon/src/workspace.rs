use std::collections::BTreeMap;
use std::fs;
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
    walk(&root, &root, &mut files)?;
    let mut overall = Sha256::new();
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
) -> Result<(), PlanValidationError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|e| PlanValidationError(format!("cannot read workspace: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| PlanValidationError(format!("cannot enumerate workspace: {e}")))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
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
            let bytes = fs::read(&path)
                .map_err(|e| PlanValidationError(format!("cannot read {relative}: {e}")))?;
            hash_bytes(b"file\0", &bytes)
        } else {
            return Err(PlanValidationError(format!(
                "unsupported workspace entry {relative}"
            )));
        };
        output.insert(relative.replace('\\', "/"), digest);
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            walk(root, &path, output)?;
        }
    }
    Ok(())
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
}
