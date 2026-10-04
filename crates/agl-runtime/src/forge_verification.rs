//! Local human choices for Forge content checks, separate from project locks.
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::ForgeDeclaration;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationDecision {
    Verify,
    Skip,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationKey {
    workspace: PathBuf,
    source_id: String,
    git: String,
    revision: String,
    declared_path: String,
    relative_file_path: String,
}

impl VerificationKey {
    pub fn new(
        workspace: &Path,
        source: &ForgeDeclaration,
        relative_file_path: &str,
    ) -> Result<Self> {
        let workspace = workspace
            .canonicalize()
            .context("verification workspace is unavailable")?;
        ensure!(
            workspace.is_dir(),
            "verification workspace is not a directory"
        );
        let relative = Path::new(relative_file_path);
        ensure!(
            !relative_file_path.is_empty()
                && !relative_file_path.contains('\0')
                && relative
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "verification file path must be relative without parent components"
        );
        ensure!(
            matches!(source.revision.len(), 40 | 64)
                && source.revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "verification revision must be a full Git commit"
        );
        Ok(Self {
            workspace,
            source_id: source.id.clone(),
            git: source.git.clone(),
            revision: source.revision.clone(),
            declared_path: source.path.clone(),
            relative_file_path: relative
                .components()
                .collect::<PathBuf>()
                .to_string_lossy()
                .into_owned(),
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Choice {
    key: VerificationKey,
    decision: VerificationDecision,
}

pub struct VerificationChoices {
    directory: PathBuf,
}

impl VerificationChoices {
    pub fn new(data_root: &Path) -> Result<Self> {
        ensure!(
            data_root.is_absolute(),
            "application data root must be absolute"
        );
        Ok(Self {
            directory: data_root.join("forge"),
        })
    }

    pub fn get(&self, key: &VerificationKey) -> Result<Option<VerificationDecision>> {
        let _guard = self.lock(false)?;
        Ok(self
            .read()?
            .into_iter()
            .find(|choice| &choice.key == key)
            .map(|choice| choice.decision))
    }

    pub fn set(&self, key: VerificationKey, decision: VerificationDecision) -> Result<()> {
        let _guard = self.lock(true)?;
        let mut choices = self.read()?;
        if let Some(choice) = choices.iter_mut().find(|choice| choice.key == key) {
            choice.decision = decision;
        } else {
            choices.push(Choice { key, decision });
        }
        let bytes = serde_json::to_vec_pretty(&choices)?;
        static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
        let temporary = self.directory.join(format!(
            ".verification-choices-{}-{}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        let result = (|| -> Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, self.directory.join("verification-choices.json"))?;
            File::open(&self.directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    fn read(&self) -> Result<Vec<Choice>> {
        let path = self.directory.join("verification-choices.json");
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("cannot read Forge verification choices"),
        };
        ensure!(
            file.metadata()?.is_file(),
            "Forge verification choices must be a regular file"
        );
        serde_json::from_reader(file).context("invalid Forge verification choices")
    }

    fn lock(&self, exclusive: bool) -> Result<ChoiceGuard> {
        fs::create_dir_all(&self.directory)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(self.directory.join(".verification-choices.lock"))?;
        ensure!(
            file.metadata()?.is_file(),
            "Forge verification choice lock must be a regular file"
        );
        let operation = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        };
        if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("Forge verification choices are busy");
        }
        Ok(ChoiceGuard(file))
    }
}

struct ChoiceGuard(File);
impl Drop for ChoiceGuard {
    fn drop(&mut self) {
        // Forked children must not prolong a completed writer's lock.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_survive_reopen_and_distinguish_workspace_revision_and_file() {
        let root = std::env::temp_dir().join(format!("agl-verification-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        let mut source = ForgeDeclaration {
            id: "source".into(),
            kind: "document".into(),
            schema: "example/v1".into(),
            git: "/repository".into(),
            revision: "0123456789012345678901234567890123456789".into(),
            path: ".".into(),
        };
        let key = VerificationKey::new(&root.join("a"), &source, "large.bin").unwrap();
        let store = VerificationChoices::new(&root.join("data")).unwrap();
        assert_eq!(store.get(&key).unwrap(), None);
        store.set(key.clone(), VerificationDecision::Skip).unwrap();
        let reopened = VerificationChoices::new(&root.join("data")).unwrap();
        assert_eq!(
            reopened.get(&key).unwrap(),
            Some(VerificationDecision::Skip)
        );
        assert_eq!(
            store
                .get(&VerificationKey::new(&root.join("b"), &source, "large.bin").unwrap())
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .get(&VerificationKey::new(&root.join("a"), &source, "other.bin").unwrap())
                .unwrap(),
            None
        );
        source.revision = "abcdefabcdefabcdefabcdefabcdefabcdefabcd".into();
        assert_eq!(
            store
                .get(&VerificationKey::new(&root.join("a"), &source, "large.bin").unwrap())
                .unwrap(),
            None
        );
        store
            .set(key.clone(), VerificationDecision::Verify)
            .unwrap();
        assert_eq!(
            reopened.get(&key).unwrap(),
            Some(VerificationDecision::Verify)
        );
        let guard = store.lock(true).unwrap();
        assert!(
            reopened
                .set(key.clone(), VerificationDecision::Skip)
                .is_err()
        );
        drop(guard);
        assert_eq!(
            reopened.get(&key).unwrap(),
            Some(VerificationDecision::Verify)
        );
        assert!(VerificationKey::new(&root.join("a"), &source, "../escape").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
