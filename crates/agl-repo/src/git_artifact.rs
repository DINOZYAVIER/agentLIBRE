use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use agl_artifact::{
    ArtifactBinding, ArtifactChange, ArtifactChangeKind, ArtifactCommitEntryKind,
    ArtifactCommitEvidence, ArtifactCommitInput, ArtifactCommitMachine, ArtifactCommitPrepare,
    ArtifactCommitRecord, ArtifactCommitRepository, ArtifactCommitRequest, ArtifactCommitState,
    GitCommitMaterial,
};
use agl_kernel::{ArtifactDeclaration, ArtifactId};
use thiserror::Error;

static TEMP_INDEX_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ArtifactBindingError {
    #[error("Artifact submodule is not registered: {artifact_id}")]
    NotRegistered { artifact_id: String },
    #[error("named Artifact submodule is missing: {artifact_id}")]
    MissingNamedSubmodule { artifact_id: String },
    #[error("duplicate Artifact submodule path: {path}")]
    DuplicatePath { path: String },
    #[error("Artifact gitlink is missing: {path}")]
    MissingGitlink { path: String },
    #[error("Artifact child HEAD differs from parent gitlink")]
    ChildHeadMismatch { gitlink: String, child_head: String },
    #[error("invalid Artifact binding: {0}")]
    Invalid(String),
    #[error("Git command failed: {0}")]
    Git(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactCommitFailpoint {
    AfterChildRefUpdate,
    AfterChildDurableRecord,
    AfterParentRefUpdate,
    BeforeTerminalEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ArtifactCommitError {
    #[error("injected Artifact commit failure at {failpoint:?}")]
    InjectedFailure { failpoint: ArtifactCommitFailpoint },
    #[error("invalid Artifact commit request: {0}")]
    InvalidRequest(String),
    #[error("Artifact commit conflict: {0}")]
    Conflict(String),
    #[error("Artifact commit failed: {0}")]
    Git(String),
    #[error("Artifact operation repository failed: {0}")]
    Repository(String),
}

pub struct ArtifactGitRepository {
    root: PathBuf,
}

impl ArtifactGitRepository {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ArtifactBindingError> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|error| ArtifactBindingError::Invalid(error.to_string()))?;
        let top = PathBuf::from(git(&root, &["rev-parse", "--show-toplevel"])?);
        let top = top
            .canonicalize()
            .map_err(|error| ArtifactBindingError::Invalid(error.to_string()))?;
        if top != root {
            return Err(ArtifactBindingError::Invalid(format!(
                "expected repository root `{}`, found `{}`",
                root.display(),
                top.display()
            )));
        }
        Ok(Self { root })
    }

    pub fn verify_binding(
        &self,
        declaration: &ArtifactDeclaration,
    ) -> Result<ArtifactBinding, ArtifactBindingError> {
        let modules = self.root.join(".gitmodules");
        if !modules.is_file() {
            return Err(ArtifactBindingError::NotRegistered {
                artifact_id: declaration.id.to_string(),
            });
        }
        let entries = match git(
            &self.root,
            &[
                "config",
                "-f",
                ".gitmodules",
                "--get-regexp",
                "^submodule\\..*\\.path$",
            ],
        ) {
            Ok(entries) => entries,
            Err(_)
                if fs::read_to_string(&modules)
                    .unwrap_or_default()
                    .trim()
                    .is_empty() =>
            {
                String::new()
            }
            Err(error) => return Err(error),
        };
        if entries.trim().is_empty() {
            return Err(ArtifactBindingError::NotRegistered {
                artifact_id: declaration.id.to_string(),
            });
        }
        let mut paths = BTreeSet::new();
        for line in entries.lines() {
            let Some((_, path)) = line.split_once(char::is_whitespace) else {
                continue;
            };
            if !paths.insert(path.trim().to_owned()) {
                return Err(ArtifactBindingError::DuplicatePath {
                    path: path.trim().to_owned(),
                });
            }
        }
        let path = self.binding_path(&declaration.id)?;
        let path_text = path.to_string_lossy().into_owned();
        let url_key = format!("submodule.{}.url", declaration.id);
        let url = git(
            &self.root,
            &["config", "-f", ".gitmodules", "--get", &url_key],
        )?;
        let gitlink = index_gitlink(&self.root, &path_text)?.ok_or_else(|| {
            ArtifactBindingError::MissingGitlink {
                path: path_text.clone(),
            }
        })?;
        let child_root = self.root.join(&path);
        let child_head = git(&child_root, &["rev-parse", "HEAD"])?;
        if gitlink != child_head {
            return Err(ArtifactBindingError::ChildHeadMismatch {
                gitlink,
                child_head,
            });
        }
        ArtifactBinding::verified_checkout(
            declaration.id.clone(),
            path,
            url,
            gitlink,
            child_head,
            child_root,
        )
        .map_err(|error| ArtifactBindingError::Invalid(error.to_string()))
    }

    fn binding_path(&self, artifact_id: &ArtifactId) -> Result<PathBuf, ArtifactBindingError> {
        if !self.root.join(".gitmodules").is_file() {
            return Err(ArtifactBindingError::NotRegistered {
                artifact_id: artifact_id.to_string(),
            });
        }
        let key = format!("submodule.{artifact_id}.path");
        let path =
            git(&self.root, &["config", "-f", ".gitmodules", "--get", &key]).map_err(|_| {
                ArtifactBindingError::MissingNamedSubmodule {
                    artifact_id: artifact_id.to_string(),
                }
            })?;
        let path = PathBuf::from(path);
        if !path.starts_with(".agl") || path.components().count() < 2 {
            return Err(ArtifactBindingError::Invalid(format!(
                "Artifact path `{}` is outside .agl",
                path.display()
            )));
        }
        Ok(path)
    }

    pub fn commit_artifact(
        &self,
        binding: &ArtifactBinding,
        request: ArtifactCommitRequest,
        operations: &dyn ArtifactCommitRepository,
    ) -> Result<ArtifactCommitEvidence, ArtifactCommitError> {
        self.commit_artifact_inner(binding, request, operations, None)
    }

    pub fn commit_artifact_with_failpoint(
        &self,
        binding: &ArtifactBinding,
        request: ArtifactCommitRequest,
        operations: &dyn ArtifactCommitRepository,
        failpoint: ArtifactCommitFailpoint,
    ) -> Result<ArtifactCommitEvidence, ArtifactCommitError> {
        self.commit_artifact_inner(binding, request, operations, Some(failpoint))
    }

    fn commit_artifact_inner(
        &self,
        binding: &ArtifactBinding,
        request: ArtifactCommitRequest,
        operations: &dyn ArtifactCommitRepository,
        failpoint: Option<ArtifactCommitFailpoint>,
    ) -> Result<ArtifactCommitEvidence, ArtifactCommitError> {
        if binding.artifact_id() != request.artifact_id() || !binding.is_verified() {
            return Err(ArtifactCommitError::InvalidRequest(
                "request does not match a verified binding".to_owned(),
            ));
        }
        if let Ok(existing) = operations.load(request.operation_id()) {
            if !request_matches_prepare(&request, existing.prepare()) {
                return Err(ArtifactCommitError::Conflict(
                    "operation identity is already bound to different commit input".to_owned(),
                ));
            }
            return self.advance(existing, operations, failpoint);
        }
        let child = binding.checkout_root().ok_or_else(|| {
            ArtifactCommitError::InvalidRequest("binding has no checkout".to_owned())
        })?;
        let paths = validate_entries(child, &request)?;
        ensure_unstaged(child, &paths, "selected child entries")?;
        let parent_path = binding.submodule_path().to_string_lossy().into_owned();
        ensure_unstaged(
            &self.root,
            std::slice::from_ref(&parent_path),
            "parent gitlink",
        )?;

        let parent_head = git_commit(&self.root, &["rev-parse", "HEAD"])?;
        let parent_gitlink = index_gitlink(&self.root, &parent_path)?.ok_or_else(|| {
            ArtifactCommitError::InvalidRequest("parent gitlink is missing".to_owned())
        })?;
        let child_head = git_commit(child, &["rev-parse", "HEAD"])?;
        if parent_gitlink != child_head {
            return Err(ArtifactCommitError::InvalidRequest(
                "child HEAD differs from the parent gitlink".to_owned(),
            ));
        }

        // Both repositories must have effective identities before the first ref mutation.
        let child_author = git_commit(child, &["var", "GIT_AUTHOR_IDENT"])?;
        let child_committer = git_commit(child, &["var", "GIT_COMMITTER_IDENT"])?;
        let parent_author = git_commit(&self.root, &["var", "GIT_AUTHOR_IDENT"])?;
        let parent_committer = git_commit(&self.root, &["var", "GIT_COMMITTER_IDENT"])?;
        let child_tree = child_tree(child, &child_head, &paths)?;
        let child_bytes = commit_bytes(
            &child_tree,
            &child_head,
            &child_author,
            &child_committer,
            request.message(),
        );
        let expected_child = hash_commit(child, &child_bytes, false)?;
        let changes = request
            .entries()
            .iter()
            .map(|entry| {
                let kind = match entry.kind() {
                    ArtifactCommitEntryKind::Create => ArtifactChangeKind::Create,
                    ArtifactCommitEntryKind::Update => ArtifactChangeKind::Update,
                    ArtifactCommitEntryKind::Delete => ArtifactChangeKind::Delete,
                };
                ArtifactChange::new(entry.path().as_str(), kind)
                    .map_err(|error| ArtifactCommitError::InvalidRequest(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let prepare = ArtifactCommitPrepare::builder(
            request.operation_id(),
            request.artifact_id().clone(),
            request.correlation().clone(),
        )
        .parent_head(parent_head)
        .parent_gitlink(parent_gitlink)
        .child_head(&child_head)
        .changes(changes)
        .child_commit(GitCommitMaterial::exact(
            &child_head,
            child_tree,
            expected_child,
            request.message(),
            child_author,
            child_committer,
        ))
        .parent_identity(parent_author, parent_committer)
        .parent_message(request.message())
        .build()
        .map_err(domain_error)?;
        let mut machine = ArtifactCommitMachine::default();
        let state = machine
            .apply(ArtifactCommitInput::Prepare(Box::new(prepare.clone())))
            .map_err(domain_error)?;
        let record = ArtifactCommitRecord::new(
            request.operation_id(),
            request.correlation().clone(),
            prepare,
            machine.revision(),
            state,
        );
        operations.save(record.clone()).map_err(domain_error)?;
        self.advance(record, operations, failpoint)
    }

    fn advance(
        &self,
        mut record: ArtifactCommitRecord,
        operations: &dyn ArtifactCommitRepository,
        failpoint: Option<ArtifactCommitFailpoint>,
    ) -> Result<ArtifactCommitEvidence, ArtifactCommitError> {
        loop {
            match record.state().clone() {
                ArtifactCommitState::Prepared { prepare } => {
                    let path = self.binding_path(&prepare.artifact_id)?;
                    let child = self.root.join(&path);
                    let current_child = git_commit(&child, &["rev-parse", "HEAD"])?;
                    if current_child == prepare.child_head {
                        write_expected_commit(&child, &prepare.child_commit)?;
                        guarded_update_ref(
                            &child,
                            &prepare.child_commit.commit,
                            &prepare.child_head,
                        )?;
                        if failpoint == Some(ArtifactCommitFailpoint::AfterChildRefUpdate) {
                            return Err(ArtifactCommitError::InjectedFailure {
                                failpoint: failpoint.expect("matched failpoint"),
                            });
                        }
                    } else if current_child != prepare.child_commit.commit {
                        let mut machine = ArtifactCommitMachine::from_record(&record);
                        let state = machine
                            .apply(ArtifactCommitInput::ObserveUnexpectedChild {
                                observed_commit: current_child.clone(),
                            })
                            .map_err(domain_error)?;
                        record = persist(&record, machine.revision(), state, operations)?;
                        return Ok(evidence(
                            &record,
                            &prepare.child_commit.commit,
                            &current_child,
                        ));
                    }
                    reset_exact_paths(
                        &child,
                        &prepare.child_commit.commit,
                        &change_paths(&prepare),
                    )?;
                    let parent_path = path.to_string_lossy().into_owned();
                    let parent_tree = parent_tree(
                        &self.root,
                        &prepare.parent_head,
                        &parent_path,
                        &prepare.child_commit.commit,
                    )?;
                    let bytes = commit_bytes(
                        &parent_tree,
                        &prepare.parent_head,
                        &prepare.parent_author,
                        &prepare.parent_committer,
                        &prepare.parent_message,
                    );
                    let expected_parent = hash_commit(&self.root, &bytes, false)?;
                    let material = GitCommitMaterial::exact(
                        &prepare.parent_head,
                        parent_tree,
                        expected_parent,
                        &prepare.parent_message,
                        &prepare.parent_author,
                        &prepare.parent_committer,
                    );
                    let mut machine = ArtifactCommitMachine::from_record(&record);
                    let state = machine
                        .apply(ArtifactCommitInput::RecordChildCommit {
                            observed_commit: prepare.child_commit.commit.clone(),
                            parent_commit: material,
                        })
                        .map_err(domain_error)?;
                    record = persist(&record, machine.revision(), state, operations)?;
                    if failpoint == Some(ArtifactCommitFailpoint::AfterChildDurableRecord) {
                        return Err(ArtifactCommitError::InjectedFailure {
                            failpoint: failpoint.expect("matched failpoint"),
                        });
                    }
                }
                ArtifactCommitState::ChildCommitted {
                    prepare,
                    child_commit,
                    parent_commit,
                } => {
                    let path = self.binding_path(&prepare.artifact_id)?;
                    let path_text = path.to_string_lossy().into_owned();
                    let current_parent = git_commit(&self.root, &["rev-parse", "HEAD"])?;
                    let current_gitlink =
                        index_gitlink(&self.root, &path_text)?.unwrap_or_default();
                    if current_parent == prepare.parent_head
                        && current_gitlink == prepare.parent_gitlink
                    {
                        write_expected_commit(&self.root, &parent_commit)?;
                        guarded_update_ref(
                            &self.root,
                            &parent_commit.commit,
                            &prepare.parent_head,
                        )?;
                        if failpoint == Some(ArtifactCommitFailpoint::AfterParentRefUpdate) {
                            return Err(ArtifactCommitError::InjectedFailure {
                                failpoint: failpoint.expect("matched failpoint"),
                            });
                        }
                    } else if current_parent != parent_commit.commit
                        || (current_gitlink != prepare.parent_gitlink
                            && current_gitlink != child_commit)
                    {
                        let mut machine = ArtifactCommitMachine::from_record(&record);
                        let state = machine
                            .apply(ArtifactCommitInput::ObserveUnsafeParent {
                                observed_head: current_parent.clone(),
                                observed_gitlink: current_gitlink.clone(),
                            })
                            .map_err(domain_error)?;
                        record = persist(&record, machine.revision(), state, operations)?;
                        return Ok(evidence(&record, &child_commit, &current_parent));
                    }
                    reset_exact_paths(
                        &self.root,
                        &parent_commit.commit,
                        std::slice::from_ref(&path_text),
                    )?;
                    let mut machine = ArtifactCommitMachine::from_record(&record);
                    let state = machine
                        .apply(ArtifactCommitInput::RecordParentCommit {
                            observed_commit: parent_commit.commit,
                        })
                        .map_err(domain_error)?;
                    record = persist(&record, machine.revision(), state, operations)?;
                }
                ArtifactCommitState::ParentCommitted {
                    child_commit,
                    parent_commit,
                } => {
                    if failpoint == Some(ArtifactCommitFailpoint::BeforeTerminalEvidence) {
                        return Err(ArtifactCommitError::InjectedFailure {
                            failpoint: failpoint.expect("matched failpoint"),
                        });
                    }
                    let mut machine = ArtifactCommitMachine::from_record(&record);
                    let state = machine
                        .apply(ArtifactCommitInput::ConfirmDurableEvidence)
                        .map_err(domain_error)?;
                    record = persist(&record, machine.revision(), state, operations)?;
                    return Ok(evidence(&record, &child_commit, &parent_commit));
                }
                ArtifactCommitState::Committed {
                    child_commit,
                    parent_commit,
                } => return Ok(evidence(&record, &child_commit, &parent_commit)),
                ArtifactCommitState::Conflict {
                    child_commit,
                    observed_head,
                    ..
                } => return Ok(evidence(&record, &child_commit, &observed_head)),
                ArtifactCommitState::Failed { reason } => {
                    return Err(ArtifactCommitError::Repository(reason));
                }
            }
        }
    }

    pub fn recover_incomplete(
        &self,
        operations: &dyn ArtifactCommitRepository,
    ) -> Result<Vec<ArtifactCommitEvidence>, ArtifactCommitError> {
        operations
            .incomplete()
            .map_err(domain_error)?
            .into_iter()
            .map(|record| self.advance(record, operations, None))
            .collect()
    }

    pub fn network_operations(&self) -> usize {
        0
    }
}

fn validate_entries(
    child: &Path,
    request: &ArtifactCommitRequest,
) -> Result<Vec<String>, ArtifactCommitError> {
    let canonical_root = child
        .canonicalize()
        .map_err(|error| ArtifactCommitError::InvalidRequest(error.to_string()))?;
    let mut paths = Vec::with_capacity(request.entries().len());
    for entry in request.entries() {
        let path = entry.path().as_str();
        reject_symlink_path(&canonical_root, path)?;
        let absolute = child.join(path);
        let tracked =
            !git_commit(child, &["ls-tree", "--name-only", "HEAD", "--", path])?.is_empty();
        match entry.kind() {
            ArtifactCommitEntryKind::Create if !absolute.is_file() || tracked => {
                return Err(ArtifactCommitError::InvalidRequest(format!(
                    "created entry `{path}` is not a new regular file"
                )));
            }
            ArtifactCommitEntryKind::Update if !absolute.is_file() || !tracked => {
                return Err(ArtifactCommitError::InvalidRequest(format!(
                    "updated entry `{path}` is not an existing regular file"
                )));
            }
            ArtifactCommitEntryKind::Delete if absolute.exists() || !tracked => {
                return Err(ArtifactCommitError::InvalidRequest(format!(
                    "deleted entry `{path}` is not an exact tracked deletion"
                )));
            }
            _ => {}
        }
        if git_commit(child, &["status", "--porcelain", "--", path])?.is_empty() {
            return Err(ArtifactCommitError::InvalidRequest(format!(
                "entry `{path}` is unchanged"
            )));
        }
        paths.push(path.to_owned());
    }
    Ok(paths)
}

fn request_matches_prepare(
    request: &ArtifactCommitRequest,
    prepare: &ArtifactCommitPrepare,
) -> bool {
    request.operation_id() == prepare.operation_id
        && request.artifact_id() == &prepare.artifact_id
        && request.correlation() == &prepare.correlation
        && request.message() == prepare.child_commit.message
        && request.message() == prepare.parent_message
        && request.entries().len() == prepare.changes.len()
        && request
            .entries()
            .iter()
            .zip(&prepare.changes)
            .all(|(entry, change)| {
                entry.path() == &change.path
                    && matches!(
                        (entry.kind(), change.kind),
                        (ArtifactCommitEntryKind::Create, ArtifactChangeKind::Create)
                            | (ArtifactCommitEntryKind::Update, ArtifactChangeKind::Update)
                            | (ArtifactCommitEntryKind::Delete, ArtifactChangeKind::Delete)
                    )
            })
}

fn reject_symlink_path(root: &Path, relative: &str) -> Result<(), ArtifactCommitError> {
    let mut cursor = root.to_path_buf();
    for component in Path::new(relative).components() {
        cursor.push(component.as_os_str());
        if let Ok(metadata) = fs::symlink_metadata(&cursor)
            && metadata.file_type().is_symlink()
        {
            return Err(ArtifactCommitError::InvalidRequest(format!(
                "entry `{relative}` traverses a symlink"
            )));
        }
    }
    Ok(())
}

fn ensure_unstaged(root: &Path, paths: &[String], label: &str) -> Result<(), ArtifactCommitError> {
    let mut args = vec![
        "diff".to_owned(),
        "--cached".to_owned(),
        "--name-only".to_owned(),
        "--".to_owned(),
    ];
    args.extend(paths.iter().cloned());
    if !git_owned(root, &args)?.is_empty() {
        return Err(ArtifactCommitError::InvalidRequest(format!(
            "{label} must be unstaged before commit"
        )));
    }
    Ok(())
}

fn child_tree(root: &Path, head: &str, paths: &[String]) -> Result<String, ArtifactCommitError> {
    let index = TempIndex::new();
    git_index(root, index.path(), &["read-tree", head], None)?;
    let mut args = vec!["add".to_owned(), "-A".to_owned(), "--".to_owned()];
    args.extend(paths.iter().cloned());
    git_index_owned(root, index.path(), &args, None)?;
    let changed = git_index(
        root,
        index.path(),
        &["diff", "--cached", "--name-only", head],
        None,
    )?;
    let observed = changed.lines().map(str::to_owned).collect::<BTreeSet<_>>();
    let expected = paths.iter().cloned().collect::<BTreeSet<_>>();
    if observed != expected {
        return Err(ArtifactCommitError::InvalidRequest(format!(
            "resulting child diff does not match the requested entries: expected {expected:?}, observed {observed:?}"
        )));
    }
    git_index(root, index.path(), &["write-tree"], None)
}

fn parent_tree(
    root: &Path,
    head: &str,
    path: &str,
    child_commit: &str,
) -> Result<String, ArtifactCommitError> {
    let index = TempIndex::new();
    git_index(root, index.path(), &["read-tree", head], None)?;
    git_index(
        root,
        index.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            child_commit,
            path,
        ],
        None,
    )?;
    git_index(root, index.path(), &["write-tree"], None)
}

fn commit_bytes(tree: &str, parent: &str, author: &str, committer: &str, message: &str) -> Vec<u8> {
    let mut body = format!(
        "tree {tree}\nparent {parent}\nauthor {author}\ncommitter {committer}\n\n{message}"
    )
    .into_bytes();
    if !body.ends_with(b"\n") {
        body.push(b'\n');
    }
    body
}

fn hash_commit(root: &Path, bytes: &[u8], write: bool) -> Result<String, ArtifactCommitError> {
    let args = if write {
        vec!["hash-object", "-t", "commit", "-w", "--stdin"]
    } else {
        vec!["hash-object", "-t", "commit", "--stdin"]
    };
    git_input(root, &args, bytes)
}

fn write_expected_commit(
    root: &Path,
    material: &GitCommitMaterial,
) -> Result<(), ArtifactCommitError> {
    let bytes = commit_bytes(
        &material.tree,
        &material.parent,
        &material.author,
        &material.committer,
        &material.message,
    );
    let observed = hash_commit(root, &bytes, true)?;
    if observed != material.commit {
        return Err(ArtifactCommitError::Conflict(format!(
            "prepared commit identity {}, observed {observed}",
            material.commit
        )));
    }
    Ok(())
}

fn guarded_update_ref(root: &Path, new: &str, old: &str) -> Result<(), ArtifactCommitError> {
    git_commit(root, &["update-ref", "HEAD", new, old]).map(|_| ())
}

fn reset_exact_paths(
    root: &Path,
    commit: &str,
    paths: &[String],
) -> Result<(), ArtifactCommitError> {
    let mut args = vec![
        "reset".to_owned(),
        "-q".to_owned(),
        commit.to_owned(),
        "--".to_owned(),
    ];
    args.extend(paths.iter().cloned());
    git_owned(root, &args).map(|_| ())
}

fn change_paths(prepare: &ArtifactCommitPrepare) -> Vec<String> {
    prepare
        .changes
        .iter()
        .map(|change| change.path.as_str().to_owned())
        .collect()
}

fn persist(
    previous: &ArtifactCommitRecord,
    revision: u64,
    state: ArtifactCommitState,
    operations: &dyn ArtifactCommitRepository,
) -> Result<ArtifactCommitRecord, ArtifactCommitError> {
    let record = ArtifactCommitRecord::new(
        previous.operation_id(),
        previous.correlation().clone(),
        previous.prepare().clone(),
        revision,
        state,
    );
    operations.save(record.clone()).map_err(domain_error)?;
    Ok(record)
}

fn evidence(record: &ArtifactCommitRecord, child: &str, parent: &str) -> ArtifactCommitEvidence {
    ArtifactCommitEvidence::new(record.operation_id(), child, parent, record.state().clone())
}

fn index_gitlink(root: &Path, path: &str) -> Result<Option<String>, ArtifactBindingError> {
    let output = git(root, &["ls-files", "--stage", "--", path])?;
    if output.is_empty() {
        return Ok(None);
    }
    let mut fields = output.split_whitespace();
    let mode = fields.next().unwrap_or_default();
    let oid = fields.next().unwrap_or_default();
    if mode != "160000" || oid.len() != 40 {
        return Ok(None);
    }
    Ok(Some(oid.to_owned()))
}

struct TempIndex(PathBuf);

impl TempIndex {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "agl-artifact-index-{}-{}",
            std::process::id(),
            TEMP_INDEX_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let _ = fs::remove_file(self.0.with_extension("lock"));
    }
}

fn git(root: &Path, args: &[&str]) -> Result<String, ArtifactBindingError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|error| ArtifactBindingError::Git(error.to_string()))?;
    if !output.status.success() {
        return Err(ArtifactBindingError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_commit(root: &Path, args: &[&str]) -> Result<String, ArtifactCommitError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|error| ArtifactCommitError::Git(error.to_string()))?;
    if !output.status.success() {
        return Err(ArtifactCommitError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_owned(root: &Path, args: &[String]) -> Result<String, ArtifactCommitError> {
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    git_commit(root, &refs)
}

fn git_index(
    root: &Path,
    index: &Path,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<String, ArtifactCommitError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_INDEX_FILE", index);
    run_command(command, input)
}

fn git_index_owned(
    root: &Path,
    index: &Path,
    args: &[String],
    input: Option<&[u8]>,
) -> Result<String, ArtifactCommitError> {
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    git_index(root, index, &refs, input)
}

fn git_input(root: &Path, args: &[&str], input: &[u8]) -> Result<String, ArtifactCommitError> {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(args);
    run_command(command, Some(input))
}

fn run_command(mut command: Command, input: Option<&[u8]>) -> Result<String, ArtifactCommitError> {
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| ArtifactCommitError::Git(error.to_string()))?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("piped Git stdin")
            .write_all(input)
            .map_err(|error| ArtifactCommitError::Git(error.to_string()))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| ArtifactCommitError::Git(error.to_string()))?;
    if !output.status.success() {
        return Err(ArtifactCommitError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn domain_error(error: agl_artifact::ArtifactCommitError) -> ArtifactCommitError {
    ArtifactCommitError::Repository(error.to_string())
}

impl From<ArtifactBindingError> for ArtifactCommitError {
    fn from(error: ArtifactBindingError) -> Self {
        Self::Git(error.to_string())
    }
}
