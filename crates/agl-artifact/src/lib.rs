//! Runtime implementation boundary for kernel-declared file-tree Artifacts.
//!
//! AGL-171 introduces the opaque handle boundary and an in-memory fixture.
//! Git binding, concrete file mutation, commit, and recovery belong to
//! AGL-172 and are intentionally absent here.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};
use std::str::FromStr;
use std::sync::Arc;

use agl_kernel::{ArtifactAccess, ArtifactDeclaration, ArtifactId};

const FILE_TREE_KIND: &str = "agl.file-tree";

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

#[derive(Clone, Debug, Default)]
pub struct FixtureArtifactTree {
    files: BTreeMap<ArtifactPath, Arc<[u8]>>,
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
    tree: Arc<FixtureArtifactTree>,
}

impl ArtifactHandle {
    pub fn fixture(
        declaration: ArtifactDeclaration,
        tree: FixtureArtifactTree,
    ) -> Result<Self, ArtifactHandleError> {
        if declaration.kind.as_str() != FILE_TREE_KIND {
            return Err(ArtifactHandleError::UnsupportedKind {
                artifact_id: declaration.id,
                kind: declaration.kind.to_string(),
            });
        }
        Ok(Self {
            declaration,
            tree: Arc::new(tree),
        })
    }

    pub fn id(&self) -> &ArtifactId {
        &self.declaration.id
    }

    pub fn declaration(&self) -> &ArtifactDeclaration {
        &self.declaration
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
        self.tree
            .files
            .get(&path)
            .map(|bytes| bytes.to_vec())
            .ok_or_else(|| ArtifactHandleError::NotFound {
                artifact_id: self.declaration.id.clone(),
                path: path.to_string(),
            })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ArtifactHandleError {
    #[error("invalid Artifact-relative path `{path}`")]
    InvalidRelativePath { path: String },
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
    #[error("Artifact `{artifact_id}` has no file `{path}`")]
    NotFound {
        artifact_id: ArtifactId,
        path: String,
    },
}
