//! Runtime implementation boundary for kernel-declared file-tree Artifacts.
//!
//! AGL-171 introduces the opaque handle boundary and an in-memory fixture.
//! Git binding, concrete file mutation, commit, and recovery belong to
//! AGL-172 and are intentionally absent here.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use agl_kernel::{ArtifactAccess, ArtifactDeclaration, ArtifactId};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

mod commit;

pub use commit::*;

fn is_file_tree_kind(kind: &str) -> bool {
    matches!(
        kind,
        "agl.file-tree"
            | "agentlibre.file-tree"
            | "agentlibre.task-specs"
            | "agentlibre.review-pack"
    )
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactPath(String);

impl ArtifactPath {
    pub fn new(value: impl Into<String>) -> Result<Self, ArtifactHandleError> {
        let value = value.into();
        let valid = !value.is_empty()
            && !value.starts_with('/')
            && !value.ends_with('/')
            && !value.contains('\\')
            && value.split('/').all(|segment| {
                !segment.is_empty()
                    && segment != "."
                    && segment != ".."
                    && !segment.chars().any(char::is_control)
            });
        if !valid {
            return Err(ArtifactHandleError::InvalidRelativePath { path: value });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ArtifactPath {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ArtifactPath {
    type Err = ArtifactHandleError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ArtifactPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ArtifactPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Default)]
pub struct FixtureArtifactTree {
    files: BTreeMap<ArtifactPath, Arc<[u8]>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactBinding {
    artifact_id: ArtifactId,
    submodule_path: PathBuf,
    remote_url: String,
    gitlink: String,
    child_head: String,
    checkout_root: Option<PathBuf>,
    verified: bool,
}

impl ArtifactBinding {
    pub fn fixture(
        artifact_id: ArtifactId,
        submodule_path: impl Into<PathBuf>,
        remote_url: impl Into<String>,
        gitlink: impl Into<String>,
    ) -> Result<Self, ArtifactHandleError> {
        let gitlink = gitlink.into();
        let remote_url = remote_url.into();
        let path = submodule_path.into();
        let verified = remote_url != "local-state://cache"
            && gitlink != "0000000000000000000000000000000000000000"
            && path.starts_with(".agl")
            && path != Path::new(".agl/cache");
        Self::new(
            artifact_id,
            path,
            remote_url,
            gitlink.clone(),
            gitlink,
            None,
            verified,
        )
    }

    pub fn verified_fixture(
        artifact_id: ArtifactId,
        submodule_path: impl Into<PathBuf>,
        remote_url: impl Into<String>,
        gitlink: impl Into<String>,
        child_head: impl Into<String>,
    ) -> Result<Self, ArtifactHandleError> {
        Self::new(
            artifact_id,
            submodule_path.into(),
            remote_url.into(),
            gitlink.into(),
            child_head.into(),
            None,
            true,
        )
    }

    pub fn local_state_fixture(
        artifact_id: ArtifactId,
        path: impl Into<PathBuf>,
    ) -> Result<Self, ArtifactHandleError> {
        Self::new(
            artifact_id,
            path.into(),
            "local-state://cache".to_owned(),
            "0000000000000000000000000000000000000000".to_owned(),
            "0000000000000000000000000000000000000000".to_owned(),
            None,
            false,
        )
    }

    pub fn verified_checkout(
        artifact_id: ArtifactId,
        submodule_path: impl Into<PathBuf>,
        remote_url: impl Into<String>,
        gitlink: impl Into<String>,
        child_head: impl Into<String>,
        checkout_root: impl Into<PathBuf>,
    ) -> Result<Self, ArtifactHandleError> {
        Self::new(
            artifact_id,
            submodule_path.into(),
            remote_url.into(),
            gitlink.into(),
            child_head.into(),
            Some(checkout_root.into()),
            true,
        )
    }

    fn new(
        artifact_id: ArtifactId,
        submodule_path: PathBuf,
        remote_url: String,
        gitlink: String,
        child_head: String,
        checkout_root: Option<PathBuf>,
        verified: bool,
    ) -> Result<Self, ArtifactHandleError> {
        let path = submodule_path.to_string_lossy().replace('\\', "/");
        if !path.starts_with(".agl/") || ArtifactPath::new(path.clone()).is_err() {
            return Err(ArtifactHandleError::InvalidBindingPath { path });
        }
        for oid in [&gitlink, &child_head] {
            if oid.len() != 40 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(ArtifactHandleError::InvalidGitOid { oid: oid.clone() });
            }
        }
        Ok(Self {
            artifact_id,
            submodule_path,
            remote_url,
            gitlink,
            child_head,
            checkout_root,
            verified,
        })
    }

    pub fn artifact_id(&self) -> &ArtifactId {
        &self.artifact_id
    }

    pub fn submodule_path(&self) -> &Path {
        &self.submodule_path
    }

    pub fn remote_url(&self) -> &str {
        &self.remote_url
    }

    pub fn gitlink(&self) -> &str {
        &self.gitlink
    }

    pub fn child_head(&self) -> &str {
        &self.child_head
    }

    pub fn checkout_root(&self) -> Option<&Path> {
        self.checkout_root.as_deref()
    }

    pub fn is_verified(&self) -> bool {
        self.verified && self.gitlink == self.child_head
    }

    pub fn binding_identity(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.artifact_id,
            self.submodule_path.display(),
            self.gitlink,
            self.child_head
        )
    }

    pub fn into_handle(
        self,
        access: ArtifactAccess,
    ) -> Result<ArtifactHandle, ArtifactHandleError> {
        let declaration = ArtifactDeclaration::new(
            self.artifact_id.clone(),
            agl_kernel::ArtifactKindId::new("agentlibre.file-tree").unwrap(),
            [access],
        )
        .map_err(|error| ArtifactHandleError::InvalidDeclaration {
            reason: error.to_string(),
        })?;
        ArtifactHandle::bind(declaration, self)
    }
}

#[derive(Clone, Debug)]
enum ArtifactBackend {
    Fixture(Arc<RwLock<FixtureArtifactTree>>),
    Filesystem(PathBuf),
    Unavailable,
}

impl FixtureArtifactTree {
    pub fn new<'a>(
        files: impl IntoIterator<Item = (&'a str, &'a [u8])>,
    ) -> Result<Self, ArtifactHandleError> {
        let mut tree = Self::default();
        for (path, bytes) in files {
            let path = ArtifactPath::new(path)?;
            if tree.files.insert(path.clone(), Arc::from(bytes)).is_some() {
                return Err(ArtifactHandleError::DuplicatePath {
                    path: path.to_string(),
                });
            }
        }
        Ok(tree)
    }
}

#[derive(Clone, Debug)]
pub struct ArtifactHandle {
    declaration: ArtifactDeclaration,
    binding_identity: Option<String>,
    backend: ArtifactBackend,
}

impl ArtifactHandle {
    pub fn fixture(
        declaration: ArtifactDeclaration,
        tree: FixtureArtifactTree,
    ) -> Result<Self, ArtifactHandleError> {
        if !is_file_tree_kind(declaration.kind.as_str()) {
            return Err(ArtifactHandleError::UnsupportedKind {
                artifact_id: declaration.id,
                kind: declaration.kind.to_string(),
            });
        }
        Ok(Self {
            declaration,
            binding_identity: None,
            backend: ArtifactBackend::Fixture(Arc::new(RwLock::new(tree))),
        })
    }

    pub fn bind(
        declaration: ArtifactDeclaration,
        binding: ArtifactBinding,
    ) -> Result<Self, ArtifactHandleError> {
        if declaration.id != *binding.artifact_id() {
            return Err(ArtifactHandleError::BindingIdMismatch {
                declared: declaration.id,
                bound: binding.artifact_id,
            });
        }
        if !is_file_tree_kind(declaration.kind.as_str()) {
            return Err(ArtifactHandleError::UnsupportedKind {
                artifact_id: declaration.id,
                kind: declaration.kind.to_string(),
            });
        }
        if !binding.is_verified() {
            return Err(ArtifactHandleError::UnverifiedBinding {
                artifact_id: declaration.id,
            });
        }
        let identity = binding.binding_identity();
        let backend = binding
            .checkout_root
            .map(ArtifactBackend::Filesystem)
            .unwrap_or(ArtifactBackend::Unavailable);
        Ok(Self {
            declaration,
            binding_identity: Some(identity),
            backend,
        })
    }

    pub fn id(&self) -> &ArtifactId {
        &self.declaration.id
    }

    pub fn declaration(&self) -> &ArtifactDeclaration {
        &self.declaration
    }

    pub fn binding_identity(&self) -> Option<&str> {
        self.binding_identity.as_deref()
    }

    pub fn require_access(&self, access: ArtifactAccess) -> Result<(), ArtifactHandleError> {
        if self.declaration.permits(access) {
            Ok(())
        } else {
            Err(ArtifactHandleError::AccessDenied {
                artifact_id: self.declaration.id.clone(),
                requested: access,
            })
        }
    }

    pub fn read(&self, path: ArtifactPath) -> Result<Vec<u8>, ArtifactHandleError> {
        self.require_access(ArtifactAccess::ReadTree)?;
        match &self.backend {
            ArtifactBackend::Fixture(tree) => {
                let tree = tree.read().expect("fixture Artifact lock poisoned");
                reject_fixture_symlink(&tree, &path)?;
                tree.files
                    .get(&path)
                    .map(|bytes| bytes.to_vec())
                    .ok_or_else(|| ArtifactHandleError::NotFound {
                        artifact_id: self.declaration.id.clone(),
                        path: path.to_string(),
                    })
            }
            ArtifactBackend::Filesystem(root) => fs::read(checked_existing_path(root, &path)?)
                .map_err(|error| ArtifactHandleError::Io {
                    path: path.to_string(),
                    reason: error.to_string(),
                }),
            ArtifactBackend::Unavailable => Err(ArtifactHandleError::Unavailable {
                artifact_id: self.declaration.id.clone(),
            }),
        }
    }

    pub fn files(&self) -> Result<Vec<ArtifactPath>, ArtifactHandleError> {
        self.require_access(ArtifactAccess::ReadTree)?;
        match &self.backend {
            ArtifactBackend::Fixture(tree) => Ok(tree
                .read()
                .expect("fixture Artifact lock poisoned")
                .files
                .keys()
                .cloned()
                .collect()),
            ArtifactBackend::Filesystem(root) => {
                fn visit(
                    root: &Path,
                    current: &Path,
                    files: &mut Vec<ArtifactPath>,
                ) -> Result<(), ArtifactHandleError> {
                    let mut entries = fs::read_dir(current)
                        .map_err(|error| ArtifactHandleError::Io {
                            path: current.display().to_string(),
                            reason: error.to_string(),
                        })?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|error| ArtifactHandleError::Io {
                            path: current.display().to_string(),
                            reason: error.to_string(),
                        })?;
                    entries.sort_by_key(std::fs::DirEntry::file_name);
                    for entry in entries {
                        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
                            ArtifactHandleError::Io {
                                path: entry.path().display().to_string(),
                                reason: error.to_string(),
                            }
                        })?;
                        let relative = entry.path().strip_prefix(root).unwrap().to_path_buf();
                        let relative = relative.to_string_lossy().replace('\\', "/");
                        if metadata.file_type().is_symlink() {
                            return Err(ArtifactHandleError::SymlinkEscape { path: relative });
                        }
                        if metadata.is_dir() {
                            visit(root, &entry.path(), files)?;
                        } else if metadata.is_file() {
                            files.push(ArtifactPath::new(relative)?);
                        }
                    }
                    Ok(())
                }
                let root = root
                    .canonicalize()
                    .map_err(|error| ArtifactHandleError::Io {
                        path: root.display().to_string(),
                        reason: error.to_string(),
                    })?;
                let mut files = Vec::new();
                visit(&root, &root, &mut files)?;
                Ok(files)
            }
            ArtifactBackend::Unavailable => Err(ArtifactHandleError::Unavailable {
                artifact_id: self.declaration.id.clone(),
            }),
        }
    }

    pub fn write(
        &self,
        path: ArtifactPath,
        bytes: &[u8],
    ) -> Result<ArtifactMutation, ArtifactHandleError> {
        self.require_access(ArtifactAccess::MutateTree)?;
        match &self.backend {
            ArtifactBackend::Fixture(tree) => {
                let mut tree = tree.write().expect("fixture Artifact lock poisoned");
                reject_fixture_symlink(&tree, &path)?;
                let before = tree.files.insert(path.clone(), Arc::from(bytes));
                Ok(ArtifactMutation {
                    path,
                    before: before.map(|bytes| bytes.to_vec()),
                    after: Some(bytes.to_vec()),
                })
            }
            ArtifactBackend::Filesystem(root) => {
                let absolute = checked_write_path(root, &path)?;
                let before = fs::read(&absolute).ok();
                fs::write(&absolute, bytes).map_err(|error| ArtifactHandleError::Io {
                    path: path.to_string(),
                    reason: error.to_string(),
                })?;
                Ok(ArtifactMutation {
                    path,
                    before,
                    after: Some(bytes.to_vec()),
                })
            }
            ArtifactBackend::Unavailable => Err(ArtifactHandleError::Unavailable {
                artifact_id: self.declaration.id.clone(),
            }),
        }
    }

    pub fn remove(&self, path: ArtifactPath) -> Result<ArtifactMutation, ArtifactHandleError> {
        self.require_access(ArtifactAccess::MutateTree)?;
        match &self.backend {
            ArtifactBackend::Fixture(tree) => {
                let mut tree = tree.write().expect("fixture Artifact lock poisoned");
                reject_fixture_symlink(&tree, &path)?;
                let before =
                    tree.files
                        .remove(&path)
                        .ok_or_else(|| ArtifactHandleError::NotFound {
                            artifact_id: self.declaration.id.clone(),
                            path: path.to_string(),
                        })?;
                Ok(ArtifactMutation {
                    path,
                    before: Some(before.to_vec()),
                    after: None,
                })
            }
            ArtifactBackend::Filesystem(root) => {
                let absolute = checked_existing_path(root, &path)?;
                let before = fs::read(&absolute).map_err(|error| ArtifactHandleError::Io {
                    path: path.to_string(),
                    reason: error.to_string(),
                })?;
                fs::remove_file(&absolute).map_err(|error| ArtifactHandleError::Io {
                    path: path.to_string(),
                    reason: error.to_string(),
                })?;
                Ok(ArtifactMutation {
                    path,
                    before: Some(before),
                    after: None,
                })
            }
            ArtifactBackend::Unavailable => Err(ArtifactHandleError::Unavailable {
                artifact_id: self.declaration.id.clone(),
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactMutation {
    path: ArtifactPath,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}

impl ArtifactMutation {
    pub fn path(&self) -> &ArtifactPath {
        &self.path
    }
    pub fn before(&self) -> Option<&[u8]> {
        self.before.as_deref()
    }
    pub fn after(&self) -> Option<&[u8]> {
        self.after.as_deref()
    }
}

fn reject_fixture_symlink(
    tree: &FixtureArtifactTree,
    path: &ArtifactPath,
) -> Result<(), ArtifactHandleError> {
    let segments = path.as_str().split('/').collect::<Vec<_>>();
    for end in 1..=segments.len() {
        let prefix = ArtifactPath::new(segments[..end].join("/"))?;
        if tree
            .files
            .get(&prefix)
            .is_some_and(|bytes| bytes.starts_with(b"fixture-symlink:"))
        {
            return Err(ArtifactHandleError::SymlinkEscape {
                path: path.to_string(),
            });
        }
    }
    Ok(())
}

fn checked_existing_path(root: &Path, path: &ArtifactPath) -> Result<PathBuf, ArtifactHandleError> {
    let root = root
        .canonicalize()
        .map_err(|error| ArtifactHandleError::Io {
            path: root.display().to_string(),
            reason: error.to_string(),
        })?;
    let candidate = root.join(path.as_str());
    let canonical = candidate
        .canonicalize()
        .map_err(|error| ArtifactHandleError::Io {
            path: path.to_string(),
            reason: error.to_string(),
        })?;
    if !canonical.starts_with(&root) {
        return Err(ArtifactHandleError::SymlinkEscape {
            path: path.to_string(),
        });
    }
    Ok(canonical)
}

fn checked_write_path(root: &Path, path: &ArtifactPath) -> Result<PathBuf, ArtifactHandleError> {
    let root = root
        .canonicalize()
        .map_err(|error| ArtifactHandleError::Io {
            path: root.display().to_string(),
            reason: error.to_string(),
        })?;
    let candidate = root.join(path.as_str());
    let parent = candidate
        .parent()
        .ok_or_else(|| ArtifactHandleError::InvalidRelativePath {
            path: path.to_string(),
        })?;
    let parent = parent
        .canonicalize()
        .map_err(|error| ArtifactHandleError::Io {
            path: path.to_string(),
            reason: error.to_string(),
        })?;
    if !parent.starts_with(&root) {
        return Err(ArtifactHandleError::SymlinkEscape {
            path: path.to_string(),
        });
    }
    Ok(candidate)
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ArtifactHandleError {
    #[error("invalid Artifact-relative path `{path}`")]
    InvalidRelativePath { path: String },
    #[error("invalid Artifact binding path `{path}`")]
    InvalidBindingPath { path: String },
    #[error("invalid Git object ID `{oid}`")]
    InvalidGitOid { oid: String },
    #[error("duplicate fixture Artifact path `{path}`")]
    DuplicatePath { path: String },
    #[error("Artifact `{artifact_id}` kind `{kind}` is not a file tree")]
    UnsupportedKind {
        artifact_id: ArtifactId,
        kind: String,
    },
    #[error("Artifact `{artifact_id}` does not permit {requested:?}")]
    AccessDenied {
        artifact_id: ArtifactId,
        requested: ArtifactAccess,
    },
    #[error("Artifact binding ID `{bound}` does not match declaration `{declared}`")]
    BindingIdMismatch {
        declared: ArtifactId,
        bound: ArtifactId,
    },
    #[error("Artifact `{artifact_id}` binding is not Git-verified")]
    UnverifiedBinding { artifact_id: ArtifactId },
    #[error("Artifact `{artifact_id}` has no checkout backend")]
    Unavailable { artifact_id: ArtifactId },
    #[error("symlink escape from Artifact path `{path}`")]
    SymlinkEscape { path: String },
    #[error("invalid Artifact declaration: {reason}")]
    InvalidDeclaration { reason: String },
    #[error("Artifact I/O failed for `{path}`: {reason}")]
    Io { path: String, reason: String },
    #[error("Artifact `{artifact_id}` has no file `{path}`")]
    NotFound {
        artifact_id: ArtifactId,
        path: String,
    },
}
