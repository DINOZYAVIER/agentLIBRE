//! The field kinds that need git (AGL-124, D8): a `commit` resolved by
//! `git rev-parse`, a `range` of two, the first an ancestor of the second,
//! and its computed state.

use std::path::Path;

use serde_json::Value;

use crate::error::Error;
use crate::forge;
use crate::forge::error::Error as ForgeError;

/// The value of a field of kind `range`: the git range of the work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    /// A commit id (anything `git rev-parse` resolves), empty until set.
    pub anchor: String,
    /// A commit id, empty until set. `anchor..last` is the review range.
    pub last: String,
}

impl Range {
    /// The range of a checked `range` value; an empty one for any other.
    pub fn of(value: Option<&Value>) -> Range {
        let id = |key| value.and_then(|value| value.get(key)).and_then(Value::as_str).unwrap_or_default().to_owned();
        Range { anchor: id("anchor"), last: id("last") }
    }

    /// The value a `range` field holds.
    pub fn value(&self) -> Value {
        serde_json::json!({ "anchor": self.anchor, "last": self.last })
    }
}

/// A computed state of a range (never stored).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitFlag {
    /// The range is valid.
    Ok,
    /// `anchor` is not an ancestor of `last` (both ids set).
    Broken,
}

/// The full commit id that `id` resolves to in the repository at
/// `repo`; an error when it does not resolve.
pub fn resolve_commit(repo: &Path, id: &str) -> Result<String, Error> {
    let arg = format!("{id}^{{commit}}");
    forge::git(repo, &["rev-parse", "--verify", &arg]).map_err(|_| Error::from(ForgeError::CommitNotFound {
        id: id.to_string(),
        repo: repo.into(),
    }))
}

/// Whether `ancestor` is an ancestor of `descendant` in the
/// repository at `repo`; both ids must resolve.
fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool, Error> {
    let output = std::process::Command::new(agentlibre_common::programs::program("git")?)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(repo)
        .output()
        .map_err(|source| Error::from(ForgeError::GitSpawn { source }))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(ForgeError::GitFailed {
            args: "merge-base --is-ancestor".into(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        }
        .into()),
    }
}

/// The computed git state of `range` in the repository at `repo`:
/// `None` when `last` is empty (no range to check). The HEAD says nothing
/// of a task's range when other tasks are worked on too (AGL-044 D3).
pub fn git_flag(range: &Range, repo: &Path) -> Result<Option<GitFlag>, Error> {
    if range.last.is_empty() {
        return Ok(None);
    }
    let last = resolve_commit(repo, &range.last)?;
    if !range.anchor.is_empty() {
        let anchor = resolve_commit(repo, &range.anchor)?;
        if !is_ancestor(repo, &anchor, &last)? {
            return Ok(Some(GitFlag::Broken));
        }
    }
    Ok(Some(GitFlag::Ok))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentlibre_common::testutil::TempDir;
    use std::fs;
    use std::path::PathBuf;

    /// A git repository at `root` with `count` commits.
    fn git_repo(root: &Path, count: u32) -> PathBuf {
        let dir = root.join("proj");
        fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        for i in 0..count {
            fs::write(dir.join("f.txt"), format!("{i}\n")).unwrap();
            run(&["add", "f.txt"]);
            run(&["commit", "-q", "-m", &format!("commit {i}")]);
        }
        dir
    }

    /// The commit `back` before the HEAD (`0` is the HEAD).
    fn back(repo: &Path, back: u32) -> String {
        crate::forge::git(repo, &["rev-parse", &format!("HEAD~{back}")]).unwrap()
    }

    #[test]
    fn the_git_flags() {
        let dir = TempDir::new();
        let repo = git_repo(&dir.0, 4);
        let head = back(&repo, 0);
        // A valid range that does not reach the HEAD is as good as one that
        // does: other tasks may have commits after it.
        let range = Range { anchor: back(&repo, 2), last: back(&repo, 1) };
        assert_eq!(git_flag(&range, &repo).unwrap(), Some(GitFlag::Ok));
        let range = Range { anchor: back(&repo, 1), last: head.clone() };
        assert_eq!(git_flag(&range, &repo).unwrap(), Some(GitFlag::Ok));
        // An empty anchor: the range is not checked.
        let range = Range { anchor: String::new(), last: head };
        assert_eq!(git_flag(&range, &repo).unwrap(), Some(GitFlag::Ok));
        // A broken range: the anchor is not an ancestor of the last.
        let range = Range { anchor: back(&repo, 1), last: back(&repo, 2) };
        assert_eq!(git_flag(&range, &repo).unwrap(), Some(GitFlag::Broken));
        // An empty last: no flag.
        let range = Range { anchor: back(&repo, 2), last: String::new() };
        assert_eq!(git_flag(&range, &repo).unwrap(), None);
        // An id that does not resolve.
        let range = Range { anchor: String::new(), last: "nope".into() };
        assert!(matches!(
            git_flag(&range, &repo),
            Err(crate::error::Error::Forge(ForgeError::CommitNotFound { id, .. })) if id == "nope"
        ));
    }

    #[test]
    fn a_range_reads_and_writes_its_value() {
        let range = Range { anchor: "a".into(), last: "b".into() };
        assert_eq!(Range::of(Some(&range.value())), range);
        assert_eq!(Range::of(None), Range { anchor: String::new(), last: String::new() });
    }
}
