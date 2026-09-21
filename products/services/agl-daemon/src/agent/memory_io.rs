//! Contained reads and digest-stamped memory claims (task 4a of
//! `reports/m1-implementation-tasks.md`).
//!
//! Companion to the core `memory_entry` codec (task 3):
//!
//! - the opt-in corpus below the workspace root is loaded through the
//!   task-3 codec; only an absent `memory/` directory is the opt-out and
//!   yields `None`, a present but entry-less `memory/` yields an empty
//!   corpus, and a present `memory` or `memory/entries` that is not a
//!   directory is a load error;
//! - cited workspace files are read only inside the workspace root — relative
//!   path, no parent components, no symlink component, canonicalized
//!   containment (mirroring the daemon `fs_read` rules) — and stamped with
//!   `sha256-12`, the first 12 hex characters of the file's SHA-256 (M1 D3);
//! - unreadable or escaping file paths are dropped, not errors (D3); under
//!   the accepted S1=A source rule a claim survives when it keeps a valid
//!   message source OR at least one readable contained file source after the
//!   drops.
//!
//! Workspace-bound plans below perform guarded entry/INDEX writes. Coordination
//! and conflict retry belong to task 4b.3, and compaction wiring to task 5.

// Consumed by task 4b (digest-checked writes) and task 5 (compaction
// wiring); its public surface is intentionally unused until then.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use agl_core::AgentRunId;
use agl_core::MessageId;
use agl_core::agent::{
    CorpusParseError, FileSource, MemoryClaimRejection, MemoryCorpus, MemoryEntry, MemoryTopic,
    StoredClaim, is_valid_slug, render_entry, title_from_slug,
};
use sha2::{Digest as _, Sha256};

/// Failure to load the opt-in corpus. Only an absent `memory/` directory is
/// the opt-out and is not an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CorpusLoadError {
    /// The workspace root is missing, not a directory, or not canonicalizable.
    RootUnavailable,
    /// A `memory` or `memory/entries` path component is a symlink and could
    /// escape the root.
    EscapingPath,
    /// `memory` or `memory/entries` exists but is not a directory.
    NotADirectory(String),
    /// A corpus file could not be read.
    Unreadable(String),
    /// An entry file failed the task-3 codec.
    Parse(CorpusParseError),
    /// The required workspace declaration or Forge lock/materialization is invalid.
    Forge(String),
}

/// One claim of a topic after its file sources were read and stamped. The
/// claim's wording, message sources and supersession references are preserved
/// verbatim from the validated payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StampedClaim {
    pub text: String,
    pub sources: Vec<MessageId>,
    /// Readable, contained file sources carrying their `sha256-12` stamps.
    pub files: Vec<FileSource>,
    pub supersedes: Option<Vec<String>>,
}

impl StampedClaim {
    /// The stored form for the task 4b writer: new claims carry no `status:`
    /// tag and are not superseded themselves.
    pub(crate) fn into_stored(self) -> StoredClaim {
        StoredClaim {
            text: self.text,
            sources: self.sources,
            files: self.files,
            status: None,
            superseded_by: None,
        }
    }
}

/// Per-claim outcome of `stamp_topic` (per-claim, like task 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StampedOutcome {
    Stamped {
        index: usize,
        claim: StampedClaim,
    },
    /// The claim keeps neither a message source nor a readable contained
    /// file source after dropping bad paths (S1=A); it is not written (D3).
    Dropped {
        index: usize,
        reason: MemoryClaimRejection,
    },
}

/// Read surface for the Forge-owned workspace memory role.
///
/// `root` is the verified Forge materialization. `workspace_root` remains the
/// agentLIBRE workspace because claim file citations refer to source files
/// there, not to files inside the memory entity.
pub(crate) struct MemoryWorkspace {
    root: PathBuf,
    workspace_root: PathBuf,
    forge_owned: bool,
}

impl MemoryWorkspace {
    /// Resolve the required workspace declaration and verified Forge memory
    /// materialization. Production callers never fall back to workspace-local
    /// memory files.
    pub(crate) fn new(root: &Path) -> Result<Self, CorpusLoadError> {
        let workspace_root = root
            .canonicalize()
            .map_err(|_| CorpusLoadError::RootUnavailable)?;
        if !workspace_root.is_dir() {
            return Err(CorpusLoadError::RootUnavailable);
        }
        #[cfg(test)]
        {
            // Codec and transaction tests use a private local fixture. The
            // production constructor below is the only runtime path.
            Ok(Self {
                root: workspace_root.clone(),
                workspace_root,
                forge_owned: false,
            })
        }
        #[cfg(not(test))]
        {
            let artifacts = agl_runtime::resolve_workspace_artifacts(&workspace_root)
                .map_err(|error| CorpusLoadError::Forge(error.to_string()))?;
            let materialized = artifacts.memory.materialized_path;
            let metadata = fs::symlink_metadata(&materialized).map_err(|_| {
                CorpusLoadError::Unreadable("Forge memory materialization".to_owned())
            })?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(CorpusLoadError::EscapingPath);
            }
            let materialized = materialized.canonicalize().map_err(|_| {
                CorpusLoadError::Unreadable("Forge memory materialization".to_owned())
            })?;
            Ok(Self {
                root: materialized,
                workspace_root,
                forge_owned: true,
            })
        }
    }

    /// Plan and capture the complete filesystem evidence used by a memory
    /// write. The pure [`plan_writes`] function remains available for codec
    /// tests, but a plan intended for `apply_write_plan` must come from this
    /// workspace-bound method.
    pub(crate) fn plan_writes(
        &self,
        corpus: &MemoryCorpus,
        topic_slug: &str,
        stamped: &[StampedOutcome],
        current_run: &AgentRunId,
    ) -> Result<(WritePlan, Vec<WritePlanOutcome>), MemoryWriteError> {
        let evidence = snapshot_memory(&self.root)?;
        let (mut plan, outcomes) = plan_writes(corpus, topic_slug, stamped, current_run);
        plan.evidence = Some(evidence);
        Ok((plan, outcomes))
    }

    /// Apply a workspace-bound plan as one contained, rollback-capable file
    /// transaction. Forge-owned materializations are read-only; this method is
    /// retained only for the local codec transaction tests.
    pub(crate) fn apply_write_plan(&self, plan: &WritePlan) -> Result<(), MemoryWriteError> {
        if self.forge_owned {
            return Err(MemoryWriteError::ForgeOwnedMaterialization);
        }
        let lock = crate::tools::workspace_mutation(&self.root);
        let _guard = lock.lock().map_err(|_| MemoryWriteError::LockPoisoned)?;
        apply_write_plan_inner(&self.root, plan, None)
    }

    /// Apply a plan and, once only, ask the caller for a fresh plan after a
    /// digest conflict. The callback runs after the lock is released, so its
    /// load/plan operation can take the same lock if needed. A callback that
    /// returns the original stale plan remains a conflict.
    pub(crate) fn apply_write_plan_with_retry<F>(
        &self,
        plan: &WritePlan,
        mut replan: F,
    ) -> Result<(), MemoryWriteError>
    where
        F: FnMut() -> Result<WritePlan, MemoryWriteError>,
    {
        match self.apply_write_plan(plan) {
            Ok(()) => Ok(()),
            Err(MemoryWriteError::Stale { .. } | MemoryWriteError::UnexpectedPath { .. }) => {
                let retry = replan()?;
                self.apply_write_plan(&retry)
            }
            Err(error) => Err(error),
        }
    }

    /// Load the opt-in corpus from `memory/entries/` through the task-3
    /// codec. `Ok(None)` is the opt-out and only when `memory/` is absent;
    /// a present `memory/` without an `entries/` directory yields an empty
    /// corpus; a present `memory` or `entries` that is not a directory is a
    /// load error (`CorpusLoadError::NotADirectory`); one invalid entry file
    /// fails the whole load (`CorpusLoadError::Parse`).
    pub(crate) fn load_corpus(&self) -> Result<Option<MemoryCorpus>, CorpusLoadError> {
        let memory = self.memory_root();
        if self.contained_dir(&memory, "memory")?.is_none() {
            return Ok(None);
        }
        let entries = memory.join("entries");
        if self.contained_dir(&entries, "memory/entries")?.is_none() {
            return Ok(Some(
                MemoryCorpus::parse(&BTreeMap::new()).map_err(CorpusLoadError::Parse)?,
            ));
        }

        let mut files = BTreeMap::new();
        let read_dir = fs::read_dir(&entries)
            .map_err(|_| CorpusLoadError::Unreadable("memory/entries".to_owned()))?;
        for dir_entry in read_dir {
            let dir_entry =
                dir_entry.map_err(|_| CorpusLoadError::Unreadable("memory/entries".to_owned()))?;
            let name = dir_entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".md") {
                continue;
            }
            let path = entries.join(&name);
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| CorpusLoadError::Unreadable(name.clone()))?;
            if metadata.file_type().is_symlink() {
                return Err(CorpusLoadError::EscapingPath);
            }
            if !metadata.is_file() {
                continue;
            }
            let canonical = path
                .canonicalize()
                .map_err(|_| CorpusLoadError::EscapingPath)?;
            if !canonical.starts_with(&self.root) {
                return Err(CorpusLoadError::EscapingPath);
            }
            let content =
                fs::read_to_string(&path).map_err(|_| CorpusLoadError::Unreadable(name.clone()))?;
            files.insert(format!("entries/{name}"), content);
        }

        Ok(Some(
            MemoryCorpus::parse(&files).map_err(CorpusLoadError::Parse)?,
        ))
    }

    fn memory_root(&self) -> PathBuf {
        if self.forge_owned {
            self.root.clone()
        } else {
            self.root.join("memory")
        }
    }

    /// Read one cited file by workspace-relative path and stamp it with the
    /// `sha256-12` digest (M1 D3). Returns `None` — the path is dropped, not
    /// an error (D3) — for malformed relative paths, symlink components,
    /// root escapes, missing paths, and read failures.
    pub(crate) fn stamped_file(&self, relative: &str) -> Option<FileSource> {
        let rel = Path::new(relative);
        if rel.is_absolute()
            || rel
                .components()
                .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
        {
            return None;
        }
        let path = self.workspace_root.join(rel);
        if !path.starts_with(&self.workspace_root) || !reject_symlinks(&self.workspace_root, &path)
        {
            return None;
        }
        let canonical = path.canonicalize().ok()?;
        if !canonical.starts_with(&self.workspace_root) || !canonical.is_file() {
            return None;
        }
        let bytes = fs::read(&canonical).ok()?;
        Some(FileSource {
            path: relative.to_owned(),
            digest: Some(sha256_12(&bytes)),
        })
    }

    /// Stamp every claim of a validated topic under the accepted S1=A source
    /// rule.
    ///
    /// Precondition: the topic already passed `MemoryTopic::validate` against
    /// the summarized source, so slug form, claim shape, source membership
    /// and supersession references are enforced. Bad file paths are dropped
    /// while readable ones keep their `sha256-12` stamps; a claim is kept
    /// when it keeps a message source OR at least one stamped file source,
    /// and dropped with `UnreadableOrEscapingPath` when it keeps neither.
    pub(crate) fn stamp_topic(&self, topic: &MemoryTopic) -> Vec<StampedOutcome> {
        topic
            .claims
            .iter()
            .enumerate()
            .map(|(index, claim)| {
                let files = claim
                    .files
                    .iter()
                    .filter_map(|path| self.stamped_file(path))
                    .collect::<Vec<_>>();
                if claim.sources.is_empty() && files.is_empty() {
                    StampedOutcome::Dropped {
                        index,
                        reason: MemoryClaimRejection::UnreadableOrEscapingPath,
                    }
                } else {
                    StampedOutcome::Stamped {
                        index,
                        claim: StampedClaim {
                            text: claim.text.clone(),
                            sources: claim.sources.clone(),
                            files,
                            supersedes: claim.supersedes.clone(),
                        },
                    }
                }
            })
            .collect()
    }

    /// `Some(())` when `path` exists directly as a non-symlink directory
    /// inside the root, `None` when it is absent. A symlink at this position
    /// is a load failure (the corpus must not escape the workspace root),
    /// and an existing non-directory is a load error: only an absent
    /// `memory/` is the opt-out.
    fn contained_dir(&self, path: &Path, label: &str) -> Result<Option<()>, CorpusLoadError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(CorpusLoadError::EscapingPath);
                }
                if metadata.is_dir() {
                    Ok(Some(()))
                } else {
                    Err(CorpusLoadError::NotADirectory(label.to_owned()))
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(CorpusLoadError::Unreadable(label.to_owned())),
        }
    }
}

/// Walk each path component between `root` and `target` (which must sit
/// lexically inside `root`) and reject any symlink component, mirroring the
/// daemon `fs_read` containment rules.
fn reject_symlinks(root: &Path, target: &Path) -> bool {
    let Ok(relative) = target.strip_prefix(root) else {
        return false;
    };
    let mut cursor = root.to_path_buf();
    for part in relative.components() {
        let Component::Normal(part) = part else {
            continue;
        };
        cursor.push(part);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => return false,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    true
}

/// `sha256-12`: the first 12 hex characters of the file's SHA-256 (M1 D3).
fn sha256_12(bytes: &[u8]) -> String {
    let value = Sha256::digest(bytes);
    value
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// Task 4b.1: plan writes (no filesystem mutation)
// ---------------------------------------------------------------------------

/// The deterministic write plan: which entry files changed and whether the
/// INDEX must be regenerated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WritePlan {
    /// Changed entry files: `entries/<slug>.md` → rendered text.
    pub entries: BTreeMap<String, String>,
    /// The regenerated INDEX.md, present only when at least one entry changed.
    pub index: Option<String>,
    /// Snapshot captured before planning. It is intentionally not populated
    /// by the pure planner, because only a workspace can observe absence,
    /// bytes, permissions, and unexpected files.
    evidence: Option<MemorySnapshot>,
}

/// A filesystem transaction failure. `RollbackFailed` is deliberately
/// distinct from an ordinary I/O failure: the caller cannot assume that the
/// workspace was restored when it is returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MemoryWriteError {
    ForgeOwnedMaterialization,
    MissingEvidence,
    EscapingPath,
    Stale { path: String },
    UnexpectedPath { path: String },
    InvalidPlan(String),
    Io { operation: String, path: String },
    LockPoisoned,
    RollbackFailed { operation: String, path: String },
}

impl fmt::Display for MemoryWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForgeOwnedMaterialization => {
                write!(formatter, "Forge-owned memory materialization is read-only")
            }
            Self::MissingEvidence => {
                write!(formatter, "memory write plan has no filesystem evidence")
            }
            Self::EscapingPath => write!(formatter, "memory write path escapes the workspace"),
            Self::Stale { path } => write!(formatter, "memory write precondition changed: {path}"),
            Self::UnexpectedPath { path } => write!(formatter, "unexpected memory path: {path}"),
            Self::InvalidPlan(reason) => write!(formatter, "invalid memory write plan: {reason}"),
            Self::Io { operation, path } => {
                write!(formatter, "memory write {operation} failed: {path}")
            }
            Self::LockPoisoned => write!(formatter, "memory workspace lock is poisoned"),
            Self::RollbackFailed { operation, path } => {
                write!(
                    formatter,
                    "memory write rollback {operation} failed: {path}"
                )
            }
        }
    }
}

impl std::error::Error for MemoryWriteError {}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MemorySnapshot(BTreeMap<String, MemoryFileState>);

#[derive(Clone, Debug, PartialEq, Eq)]
enum MemoryFileState {
    File {
        bytes: Vec<u8>,
        digest: String,
        readonly: bool,
    },
    Directory,
    Absent,
}

/// Per-claim outcome of `plan_writes` (per-claim, like task 1 and 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WritePlanOutcome {
    /// The claim was appended; `ordinal` is its 1-based position.
    Appended { ordinal: usize },
    /// Exact-text duplicate already stored; no change, no rejection.
    Duplicate { ordinal: usize },
    /// The claim was not written.
    Rejected { reason: MemoryClaimRejection },
}

/// Plan the deterministic writes for one topic's stamped claims against the
/// current corpus. No filesystem mutation, no lock, no retry, no model calls,
/// no compaction.
///
/// The plan is computed by cloning the corpus, applying each stamped claim in
/// order (exact-text dedup, supersession validation, INDEX cap), then diffing
/// the rendered output before and after to produce the changed-file set.
pub(crate) fn plan_writes(
    corpus: &MemoryCorpus,
    topic_slug: &str,
    stamped: &[StampedOutcome],
    current_run: &AgentRunId,
) -> (WritePlan, Vec<WritePlanOutcome>) {
    let before = rendered_entries(corpus);
    let mut working = corpus.clone();

    // Reject everything on invalid slug or INDEX cap for a new topic.
    if !is_valid_slug(topic_slug) {
        let outcomes = stamped
            .iter()
            .map(|_| WritePlanOutcome::Rejected {
                reason: MemoryClaimRejection::InvalidSlug,
            })
            .collect();
        return (
            WritePlan {
                entries: BTreeMap::new(),
                index: None,
                evidence: None,
            },
            outcomes,
        );
    }
    let existing = working.entry(topic_slug).is_some();
    if !existing && !working.fits_new_topic() {
        let outcomes = stamped
            .iter()
            .map(|_| WritePlanOutcome::Rejected {
                reason: MemoryClaimRejection::IndexCapReached,
            })
            .collect();
        return (
            WritePlan {
                entries: BTreeMap::new(),
                index: None,
                evidence: None,
            },
            outcomes,
        );
    }

    let mut entry = working
        .entry(topic_slug)
        .cloned()
        .unwrap_or_else(|| MemoryEntry {
            title: title_from_slug(topic_slug),
            slug: topic_slug.to_owned(),
            created: *current_run,
            updated: *current_run,
            artifact: None,
            claims: Vec::new(),
        });

    let mut outcomes = Vec::with_capacity(stamped.len());
    for outcome in stamped {
        match outcome {
            StampedOutcome::Dropped { reason, .. } => {
                outcomes.push(WritePlanOutcome::Rejected { reason: *reason });
                continue;
            }
            StampedOutcome::Stamped { claim, .. } => {
                // Exact-text dedup.
                if let Some(position) = entry.claims.iter().position(|c| c.text == claim.text) {
                    outcomes.push(WritePlanOutcome::Duplicate {
                        ordinal: position + 1,
                    });
                    continue;
                }
                // Validate supersession references.
                let mut supersession_valid = true;
                let mut local: Vec<usize> = Vec::new();
                let mut cross: Vec<(String, usize)> = Vec::new();
                for reference in claim.supersedes.iter().flatten() {
                    let Some((ref_slug, ordinal)) = reference.split_once('#') else {
                        supersession_valid = false;
                        break;
                    };
                    let Some(number) = ordinal.strip_prefix("claim-") else {
                        supersession_valid = false;
                        break;
                    };
                    let Some(position) =
                        number.parse::<usize>().ok().and_then(|v| v.checked_sub(1))
                    else {
                        supersession_valid = false;
                        break;
                    };
                    if ref_slug == topic_slug {
                        if position >= entry.claims.len() {
                            supersession_valid = false;
                            break;
                        }
                        local.push(position);
                    } else if let Some(target) = working.entry(ref_slug) {
                        if position >= target.claims.len() {
                            supersession_valid = false;
                            break;
                        }
                        cross.push((ref_slug.to_owned(), position));
                    } else {
                        supersession_valid = false;
                        break;
                    }
                }
                if !supersession_valid {
                    outcomes.push(WritePlanOutcome::Rejected {
                        reason: MemoryClaimRejection::InvalidSupersession,
                    });
                    continue;
                }

                // Apply the claim with its pre-stamped file digests.
                let ordinal = entry.claims.len() + 1;
                for position in &local {
                    if entry.claims[*position].superseded_by.is_none() {
                        entry.claims[*position].superseded_by =
                            Some(format!("{}#claim-{}", topic_slug, ordinal));
                    }
                }
                for (ref_slug, position) in &cross {
                    if let Some(target) = working
                        .entry_mut(ref_slug)
                        .filter(|t| t.claims[*position].superseded_by.is_none())
                    {
                        target.claims[*position].superseded_by =
                            Some(format!("{}#claim-{}", topic_slug, ordinal));
                        target.updated = *current_run;
                    }
                }
                entry.claims.push(StoredClaim {
                    text: claim.text.clone(),
                    sources: claim.sources.clone(),
                    files: claim.files.clone(),
                    status: None,
                    superseded_by: None,
                });
                entry.updated = *current_run;
                outcomes.push(WritePlanOutcome::Appended { ordinal });
            }
        }
    }

    // Materialize the entry only when at least one claim was appended or it
    // already existed.
    let appended = outcomes
        .iter()
        .any(|o| matches!(o, WritePlanOutcome::Appended { .. }));
    if existing || appended {
        working.insert(entry);
    }

    // Diff rendered output to produce the deterministic changed-file set.
    let after = rendered_entries(&working);
    let mut changed: BTreeMap<String, String> = BTreeMap::new();
    for (path, text) in &after {
        if before.get(path).map(|old| old != text).unwrap_or(true) {
            changed.insert(path.clone(), text.clone());
        }
    }
    let index = if changed.is_empty() {
        None
    } else {
        Some(working.render_index())
    };
    (
        WritePlan {
            entries: changed,
            index,
            evidence: None,
        },
        outcomes,
    )
}

/// Render all entries into a `BTreeMap` keyed by `entries/<slug>.md`.
fn rendered_entries(corpus: &MemoryCorpus) -> BTreeMap<String, String> {
    corpus
        .slugs()
        .map(|slug| {
            (
                format!("entries/{slug}.md"),
                render_entry(corpus.entry(slug).unwrap()),
            )
        })
        .collect()
}

fn sha256_full(bytes: &[u8]) -> String {
    let value = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(7 + value.len() * 2);
    encoded.push_str("sha256:");
    for byte in value {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn snapshot_memory(root: &Path) -> Result<MemorySnapshot, MemoryWriteError> {
    let mut files = BTreeMap::new();
    capture_memory_path(&root.join("memory"), "memory", &mut files)?;
    Ok(MemorySnapshot(files))
}

fn capture_memory_path(
    path: &Path,
    relative: &str,
    files: &mut BTreeMap<String, MemoryFileState>,
) -> Result<(), MemoryWriteError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            files.insert(relative.to_owned(), MemoryFileState::Absent);
            return Ok(());
        }
        Err(_) => {
            return Err(MemoryWriteError::Io {
                operation: "inspect".to_owned(),
                path: relative.to_owned(),
            });
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(MemoryWriteError::EscapingPath);
    }
    if metadata.is_dir() {
        files.insert(relative.to_owned(), MemoryFileState::Directory);
        let mut entries = fs::read_dir(path)
            .map_err(|_| MemoryWriteError::Io {
                operation: "read directory".to_owned(),
                path: relative.to_owned(),
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| MemoryWriteError::Io {
                operation: "read directory".to_owned(),
                path: relative.to_owned(),
            })?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_relative = format!("{relative}/{name}");
            capture_memory_path(&entry.path(), &child_relative, files)?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Err(MemoryWriteError::Io {
            operation: "inspect special file".to_owned(),
            path: relative.to_owned(),
        });
    }
    let bytes = fs::read(path).map_err(|_| MemoryWriteError::Io {
        operation: "read".to_owned(),
        path: relative.to_owned(),
    })?;
    files.insert(
        relative.to_owned(),
        MemoryFileState::File {
            digest: sha256_full(&bytes),
            bytes,
            readonly: metadata.permissions().readonly(),
        },
    );
    Ok(())
}

fn state_at(snapshot: &MemorySnapshot, path: &str) -> MemoryFileState {
    snapshot
        .0
        .get(path)
        .cloned()
        .unwrap_or(MemoryFileState::Absent)
}

fn validate_memory_target(relative: &str) -> Result<(), MemoryWriteError> {
    let path = Path::new(relative);
    if relative == "INDEX.md" {
        return Ok(());
    }
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return Err(MemoryWriteError::InvalidPlan(relative.to_owned()));
    };
    let Some(Component::Normal(name)) = components.next() else {
        return Err(MemoryWriteError::InvalidPlan(relative.to_owned()));
    };
    if first != "entries"
        || components.next().is_some()
        || !name
            .to_string_lossy()
            .strip_suffix(".md")
            .is_some_and(is_valid_slug)
    {
        return Err(MemoryWriteError::InvalidPlan(relative.to_owned()));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct TransactionFault {
    fail_after: Option<usize>,
    fail_rollback: bool,
}

fn apply_write_plan_inner(
    root: &Path,
    plan: &WritePlan,
    fault: Option<TransactionFault>,
) -> Result<(), MemoryWriteError> {
    let evidence = plan
        .evidence
        .as_ref()
        .ok_or(MemoryWriteError::MissingEvidence)?;
    if plan.entries.is_empty() && plan.index.is_none() {
        return Ok(());
    }
    if plan.entries.is_empty() && plan.index.is_some() {
        return Err(MemoryWriteError::InvalidPlan(
            "INDEX cannot change without an entry change".to_owned(),
        ));
    }
    if plan.index.is_none() && !plan.entries.is_empty() {
        return Err(MemoryWriteError::InvalidPlan(
            "entry changes require a regenerated INDEX".to_owned(),
        ));
    }

    let mut changes = Vec::<(String, Vec<u8>)>::new();
    for (relative, text) in &plan.entries {
        validate_memory_target(relative)?;
        changes.push((format!("memory/{relative}"), text.as_bytes().to_vec()));
    }
    if let Some(index) = &plan.index {
        changes.push(("memory/INDEX.md".to_owned(), index.as_bytes().to_vec()));
    }

    let observed = snapshot_memory(root)?;
    if observed.0.len() != evidence.0.len() {
        let path = observed
            .0
            .keys()
            .find(|path| !evidence.0.contains_key(*path))
            .or_else(|| {
                evidence
                    .0
                    .keys()
                    .find(|path| !observed.0.contains_key(*path))
            })
            .cloned()
            .unwrap_or_else(|| "memory".to_owned());
        return Err(MemoryWriteError::UnexpectedPath { path });
    }
    for path in evidence.0.keys().chain(observed.0.keys()) {
        let expected = state_at(evidence, path);
        let actual = state_at(&observed, path);
        if expected != actual {
            return match (&expected, &actual) {
                (
                    MemoryFileState::File { digest: old, .. },
                    MemoryFileState::File { digest: new, .. },
                ) if old != new => Err(MemoryWriteError::Stale { path: path.clone() }),
                (MemoryFileState::Absent, _) => {
                    Err(MemoryWriteError::UnexpectedPath { path: path.clone() })
                }
                _ => Err(MemoryWriteError::Stale { path: path.clone() }),
            };
        }
    }
    for (path, _) in &changes {
        match state_at(evidence, path) {
            MemoryFileState::Absent | MemoryFileState::File { .. } => {}
            _ => return Err(MemoryWriteError::InvalidPlan(path.clone())),
        }
    }

    let transaction = root.join(format!(
        ".agentlibre-memory-transaction-{}",
        uuid::Uuid::now_v7()
    ));
    let staged = transaction.join("staged");
    let backups = transaction.join("backups");
    fs::create_dir(&transaction)
        .and_then(|_| fs::create_dir(&staged))
        .and_then(|_| fs::create_dir(&backups))
        .map_err(|_| MemoryWriteError::Io {
            operation: "create transaction".to_owned(),
            path: transaction.display().to_string(),
        })?;

    let result = (|| {
        // Stage the complete changed file set before touching the workspace.
        for (index, (_, bytes)) in changes.iter().enumerate() {
            fs::write(staged.join(index.to_string()), bytes).map_err(|_| MemoryWriteError::Io {
                operation: "stage".to_owned(),
                path: staged.join(index.to_string()).display().to_string(),
            })?;
        }
        let mut created_directories = Vec::new();
        let mut applied = Vec::<(String, usize)>::new();
        let apply_result = (|| {
            for (index, (relative, _)) in changes.iter().enumerate() {
                let destination = root.join(relative);
                if let Some(parent) = destination.parent() {
                    create_memory_directories(root, parent, &mut created_directories)?;
                }
                if matches!(state_at(evidence, relative), MemoryFileState::File { .. }) {
                    fs::rename(&destination, backups.join(index.to_string())).map_err(|_| {
                        MemoryWriteError::Io {
                            operation: "backup".to_owned(),
                            path: relative.clone(),
                        }
                    })?;
                }
                fs::rename(staged.join(index.to_string()), &destination).map_err(|_| {
                    MemoryWriteError::Io {
                        operation: "apply".to_owned(),
                        path: relative.clone(),
                    }
                })?;
                applied.push((relative.clone(), index));
                if fault.is_some_and(|fault| fault.fail_after == Some(applied.len())) {
                    return Err(MemoryWriteError::Io {
                        operation: "injected apply failure".to_owned(),
                        path: relative.clone(),
                    });
                }
            }
            Ok::<_, MemoryWriteError>(())
        })();
        if let Err(error) = apply_result {
            if fault.is_some_and(|fault| fault.fail_rollback) {
                return Err(MemoryWriteError::RollbackFailed {
                    operation: "injected rollback failure".to_owned(),
                    path: "memory".to_owned(),
                });
            }
            rollback_memory(root, evidence, &changes, &created_directories, &applied)?;
            return Err(error);
        }
        Ok::<_, MemoryWriteError>(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&transaction);
        return result;
    }
    fs::remove_dir_all(&transaction).map_err(|_| MemoryWriteError::Io {
        operation: "cleanup transaction".to_owned(),
        path: transaction.display().to_string(),
    })
}

fn create_memory_directories(
    root: &Path,
    directory: &Path,
    created: &mut Vec<PathBuf>,
) -> Result<(), MemoryWriteError> {
    let relative = directory
        .strip_prefix(root)
        .map_err(|_| MemoryWriteError::EscapingPath)?;
    let mut cursor = root.to_path_buf();
    for part in relative.components() {
        let Component::Normal(part) = part else {
            return Err(MemoryWriteError::EscapingPath);
        };
        cursor.push(part);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(MemoryWriteError::EscapingPath),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&cursor).map_err(|_| MemoryWriteError::Io {
                    operation: "create directory".to_owned(),
                    path: cursor.display().to_string(),
                })?;
                created.push(cursor.clone());
            }
            Err(_) => {
                return Err(MemoryWriteError::Io {
                    operation: "inspect directory".to_owned(),
                    path: cursor.display().to_string(),
                });
            }
        }
    }
    Ok(())
}

fn rollback_memory(
    root: &Path,
    evidence: &MemorySnapshot,
    changes: &[(String, Vec<u8>)],
    created_directories: &[PathBuf],
    applied: &[(String, usize)],
) -> Result<(), MemoryWriteError> {
    for (relative, _) in applied.iter().rev() {
        let destination = root.join(relative);
        match fs::remove_file(&destination) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(MemoryWriteError::RollbackFailed {
                    operation: "remove applied file".to_owned(),
                    path: relative.clone(),
                });
            }
        }
    }
    for (relative, _) in changes {
        if let MemoryFileState::File {
            bytes, readonly, ..
        } = state_at(evidence, relative)
        {
            let destination = root.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|_| MemoryWriteError::RollbackFailed {
                    operation: "restore parent".to_owned(),
                    path: relative.clone(),
                })?;
            }
            fs::write(&destination, bytes).map_err(|_| MemoryWriteError::RollbackFailed {
                operation: "restore file".to_owned(),
                path: relative.clone(),
            })?;
            let mut permissions = fs::metadata(&destination)
                .map_err(|_| MemoryWriteError::RollbackFailed {
                    operation: "inspect restored file".to_owned(),
                    path: relative.clone(),
                })?
                .permissions();
            permissions.set_readonly(readonly);
            fs::set_permissions(&destination, permissions).map_err(|_| {
                MemoryWriteError::RollbackFailed {
                    operation: "restore permissions".to_owned(),
                    path: relative.clone(),
                }
            })?;
        }
    }
    for directory in created_directories.iter().rev() {
        match fs::remove_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(_) => {
                return Err(MemoryWriteError::RollbackFailed {
                    operation: "remove created directory".to_owned(),
                    path: directory.display().to_string(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agl_core::agent::MemoryClaim;

    fn msg(value: &str) -> MessageId {
        MessageId::parse(value).unwrap()
    }

    fn temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("agl-memory-io-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
    }

    const SEED_ENTRY: &str = "# Seeded topic\n\n- slug: seeded\n- created: run_01a090d4-5628-77d3-8f53-1abe34dbda5d\n- updated: run_01a090d4-5628-77d3-8f53-1abe34dbda5d\n- artifact: reports/seeded.md\n\n## Claims\n\n- A durable fact. [src: msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9] [file: notes/a.md@0123456789ab]\n";

    fn claim(text: &str, sources: Vec<MessageId>, files: Vec<&str>) -> MemoryClaim {
        MemoryClaim {
            text: text.to_owned(),
            sources,
            files: files.into_iter().map(str::to_owned).collect(),
            supersedes: None,
        }
    }

    #[test]
    fn absent_memory_directory_is_the_opt_out() {
        let root = temp_root();
        write(&root, "README.md", "# root\n");
        let workspace = MemoryWorkspace::new(&root).unwrap();
        assert_eq!(workspace.load_corpus(), Ok(None));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_without_entries_is_an_empty_corpus() {
        let root = temp_root();
        write(&root, "memory/INDEX.md", "# Memory Index\n");
        let workspace = MemoryWorkspace::new(&root).unwrap();
        let corpus = workspace.load_corpus().unwrap().expect("opt-in corpus");
        assert!(corpus.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_as_a_file_is_a_load_error() {
        let root = temp_root();
        write(&root, "memory", "not a directory\n");
        let workspace = MemoryWorkspace::new(&root).unwrap();
        assert_eq!(
            workspace.load_corpus(),
            Err(CorpusLoadError::NotADirectory("memory".to_owned()))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn entries_as_a_file_is_a_load_error() {
        let root = temp_root();
        write(&root, "memory/INDEX.md", "# Memory Index\n");
        write(&root, "memory/entries", "not a directory\n");
        let workspace = MemoryWorkspace::new(&root).unwrap();
        assert_eq!(
            workspace.load_corpus(),
            Err(CorpusLoadError::NotADirectory("memory/entries".to_owned()))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn seeded_corpus_loads_with_preserved_stamps() {
        let root = temp_root();
        write(&root, "memory/INDEX.md", "# Memory Index\n");
        write(&root, "memory/entries/seeded.md", SEED_ENTRY);
        write(&root, "memory/entries-notes.txt", "not an entry");

        let workspace = MemoryWorkspace::new(&root).unwrap();
        let corpus = workspace.load_corpus().unwrap().expect("opt-in corpus");
        assert_eq!(corpus.len(), 1);
        let entry = corpus.entry("seeded").expect("seeded entry");
        assert_eq!(entry.title, "Seeded topic");
        assert_eq!(entry.claims.len(), 1);
        // Existing source stamps are preserved verbatim by the load.
        assert_eq!(
            entry.claims[0].files,
            vec![FileSource {
                path: "notes/a.md".to_owned(),
                digest: Some("0123456789ab".to_owned()),
            }]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn repo_seeded_corpus_loads() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("repo root above the daemon crate");
        let workspace = MemoryWorkspace::new(repo_root).unwrap();
        let corpus = workspace
            .load_corpus()
            .expect("seed load")
            .expect("this repo opts in");
        assert!(!corpus.is_empty(), "the seeded corpus must not be empty");
        assert!(corpus.entry("repl").is_some());
    }

    #[test]
    fn malformed_entry_fails_the_load() {
        let root = temp_root();
        write(&root, "memory/entries/broken.md", "# Broken\n\n## Claims\n");
        let workspace = MemoryWorkspace::new(&root).unwrap();
        let error = workspace.load_corpus().unwrap_err();
        assert!(
            matches!(error, CorpusLoadError::Parse(_)),
            "expected a parse error, got {error:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn symlinked_memory_directory_rejects_the_load() {
        let root = temp_root();
        let outside = root.join("outside");
        fs::create_dir_all(outside.join("memory/entries")).unwrap();
        fs::write(outside.join("memory/entries/seeded.md"), SEED_ENTRY).unwrap();
        std::os::unix::fs::symlink(outside.join("memory"), root.join("memory")).unwrap();

        let workspace = MemoryWorkspace::new(&root).unwrap();
        assert_eq!(workspace.load_corpus(), Err(CorpusLoadError::EscapingPath));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stamped_file_uses_the_first_12_hex_chars() {
        let root = temp_root();
        write(&root, "notes/a.md", "hello stamp\n");
        let value = Sha256::digest(b"hello stamp\n");
        let expected: String = value
            .iter()
            .take(6)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let workspace = MemoryWorkspace::new(&root).unwrap();
        assert_eq!(
            workspace.stamped_file("notes/a.md"),
            Some(FileSource {
                path: "notes/a.md".to_owned(),
                digest: Some(expected),
            })
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stamped_file_drops_malformed_and_missing_paths() {
        let root = temp_root();
        write(&root, "notes/a.md", "hello\n");
        write(&root, "notes/child.md", "inner\n");
        let workspace = MemoryWorkspace::new(&root).unwrap();
        for relative in [
            "",
            "/etc/passwd",
            "notes/../child.md",
            "missing.md",
            "notes",
            "notes/missing.md",
        ] {
            assert_eq!(
                workspace.stamped_file(relative),
                None,
                "path {relative:?} must be dropped"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stamped_file_never_follows_workspace_symlinks() {
        let root = temp_root();
        write(&root, "notes/a.md", "hello\n");
        // A symlink inside the root pointing at a readable contained file is
        // still rejected, mirroring the daemon fs_read rules.
        std::os::unix::fs::symlink(root.join("notes/a.md"), root.join("link-inside.md")).unwrap();
        // A symlink escaping the root is dropped as an escape.
        let outside_file =
            std::env::temp_dir().join(format!("agl-memory-io-outside-{}", uuid::Uuid::now_v7()));
        fs::write(&outside_file, "outside\n").unwrap();
        std::os::unix::fs::symlink(&outside_file, root.join("link-out.md")).unwrap();

        let workspace = MemoryWorkspace::new(&root).unwrap();
        assert_eq!(workspace.stamped_file("link-inside.md"), None);
        assert_eq!(workspace.stamped_file("link-out.md"), None);
        let _ = fs::remove_file(&outside_file);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stamp_topic_applies_the_selected_source_rule() {
        let root = temp_root();
        write(&root, "notes/a.md", "hello\n");
        let value = Sha256::digest(b"hello\n");
        let stamp: String = value
            .iter()
            .take(6)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let workspace = MemoryWorkspace::new(&root).unwrap();
        let topic = MemoryTopic {
            slug: "repl".to_owned(),
            claims: vec![
                // Message source plus a readable file: kept, file stamped.
                claim(
                    "Kept with both.",
                    vec![msg("msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9")],
                    vec!["notes/a.md"],
                ),
                // Message source plus an unreadable file: kept, path dropped.
                claim(
                    "Kept on its source.",
                    vec![msg("msg_01a090d5-4b9b-71b0-aae8-d5421f5c3513")],
                    vec!["notes/missing.md"],
                ),
                // File-only claim with a readable contained file: kept.
                claim("Kept as file-only.", vec![], vec!["notes/a.md"]),
                // File-only claim whose only file is unreadable: dropped (S1=A).
                claim("Dropped.", vec![], vec!["notes/missing.md"]),
            ],
        };

        let outcomes = workspace.stamp_topic(&topic);
        assert_eq!(outcomes.len(), 4);
        let first = match &outcomes[0] {
            StampedOutcome::Stamped { claim, .. } => claim,
            other => panic!("claim 0 must be stamped, got {other:?}"),
        };
        assert_eq!(
            first.files,
            vec![FileSource {
                path: "notes/a.md".to_owned(),
                digest: Some(stamp.clone()),
            }]
        );
        assert_eq!(
            first.sources,
            vec![msg("msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9")]
        );
        let second = match &outcomes[1] {
            StampedOutcome::Stamped { claim, .. } => claim,
            other => panic!("claim 1 must be stamped, got {other:?}"),
        };
        assert!(second.files.is_empty());
        let third = match &outcomes[2] {
            StampedOutcome::Stamped { claim, .. } => claim,
            other => panic!("claim 2 must be stamped, got {other:?}"),
        };
        assert_eq!(third.files.len(), 1);
        assert_eq!(
            outcomes[3],
            StampedOutcome::Dropped {
                index: 3,
                reason: MemoryClaimRejection::UnreadableOrEscapingPath,
            }
        );
        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // Task 4b.1: plan_writes tests
    // -----------------------------------------------------------------------

    fn seeded_corpus() -> MemoryCorpus {
        let mut files = BTreeMap::new();
        files.insert("entries/seeded.md".to_owned(), SEED_ENTRY.to_owned());
        MemoryCorpus::parse(&files).unwrap()
    }

    fn run_a() -> AgentRunId {
        AgentRunId::parse("run_01a090d4-5628-77d3-8f53-1abe34dbda5d").unwrap()
    }

    fn run_b() -> AgentRunId {
        AgentRunId::parse("run_01a09292-e4ab-7e11-a6ee-1221312293c1").unwrap()
    }

    fn stamped(text: &str, sources: Vec<MessageId>, files: Vec<FileSource>) -> StampedOutcome {
        StampedOutcome::Stamped {
            index: 0,
            claim: StampedClaim {
                text: text.to_owned(),
                sources,
                files,
                supersedes: None,
            },
        }
    }

    #[test]
    fn plan_writes_new_topic_creates_entry_and_index() {
        let corpus = MemoryCorpus::parse(&BTreeMap::new()).unwrap();
        let run = run_a();
        let claims = vec![stamped(
            "Fresh fact.",
            vec![msg("msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9")],
            vec![],
        )];
        let (plan, outcomes) = plan_writes(&corpus, "fresh-topic", &claims, &run);

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0], WritePlanOutcome::Appended { ordinal: 1 });
        assert_eq!(plan.entries.len(), 1);
        let path = "entries/fresh-topic.md";
        let rendered = plan.entries.get(path).expect("entry changed");
        assert!(rendered.contains("# Fresh topic"));
        assert!(rendered.contains("Fresh fact."));
        assert!(rendered.contains(&run.to_string()));
        assert!(plan.index.is_some(), "INDEX must be regenerated");
        let index = plan.index.as_ref().unwrap();
        assert!(index.contains("fresh-topic"));
        assert!(index.contains("1 claims"));
    }

    #[test]
    fn plan_writes_append_to_existing_topic() {
        let corpus = seeded_corpus();
        let run = run_b();
        let claims = vec![stamped(
            "A new durable fact.",
            vec![msg("msg_01a090d5-4b9b-71b0-aae8-d5421f5c3513")],
            vec![],
        )];
        let (plan, outcomes) = plan_writes(&corpus, "seeded", &claims, &run);

        assert_eq!(outcomes[0], WritePlanOutcome::Appended { ordinal: 2 });
        assert_eq!(plan.entries.len(), 1);
        let rendered = plan.entries.get("entries/seeded.md").unwrap();
        assert!(rendered.contains("A new durable fact."));
        assert!(rendered.contains(&run.to_string()), "updated bumped");
        assert!(plan.index.is_some());
    }

    #[test]
    fn plan_writes_exact_duplicate_no_change() {
        let corpus = seeded_corpus();
        let run = run_b();
        let claims = vec![stamped(
            "A durable fact.",
            vec![msg("msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9")],
            vec![FileSource {
                path: "notes/a.md".to_owned(),
                digest: Some("0123456789ab".to_owned()),
            }],
        )];
        let (plan, outcomes) = plan_writes(&corpus, "seeded", &claims, &run);

        assert_eq!(outcomes[0], WritePlanOutcome::Duplicate { ordinal: 1 });
        assert!(plan.entries.is_empty(), "no entry changed");
        assert!(plan.index.is_none(), "no INDEX change");
    }

    #[test]
    fn plan_writes_supersession_same_topic() {
        // Corpus has one claim: "A durable fact."
        let corpus = seeded_corpus();
        let run = run_b();
        // New claim supersedes claim-1 of the same topic.
        let superseding = StampedOutcome::Stamped {
            index: 0,
            claim: StampedClaim {
                text: "Updated fact.".to_owned(),
                sources: vec![msg("msg_01a090d5-4b9b-71b0-aae8-d5421f5c3513")],
                files: vec![],
                supersedes: Some(vec!["seeded#claim-1".to_owned()]),
            },
        };
        let (plan, outcomes) = plan_writes(&corpus, "seeded", &[superseding], &run);

        assert_eq!(outcomes[0], WritePlanOutcome::Appended { ordinal: 2 });
        let rendered = plan.entries.get("entries/seeded.md").unwrap();
        assert!(rendered.contains("Updated fact."));
        assert!(
            rendered.contains("[superseded-by: seeded#claim-2]"),
            "old claim tagged"
        );
        assert!(plan.index.is_some());
    }

    #[test]
    fn plan_writes_supersession_cross_topic() {
        // Two entries: "seeded" and "other".
        let mut files = BTreeMap::new();
        files.insert("entries/seeded.md".to_owned(), SEED_ENTRY.to_owned());
        files.insert(
            "entries/other.md".to_owned(),
            "# Other topic\n\n- slug: other\n- created: run_01a090d4-5628-77d3-8f53-1abe34dbda5d\n- updated: run_01a090d4-5628-77d3-8f53-1abe34dbda5d\n\n## Claims\n\n- Old cross-topic fact. [src: msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9]\n".to_owned(),
        );
        let corpus = MemoryCorpus::parse(&files).unwrap();
        let run = run_b();
        let superseding = StampedOutcome::Stamped {
            index: 0,
            claim: StampedClaim {
                text: "New cross-topic replacement.".to_owned(),
                sources: vec![msg("msg_01a090d5-4b9b-71b0-aae8-d5421f5c3513")],
                files: vec![],
                supersedes: Some(vec!["other#claim-1".to_owned()]),
            },
        };
        let (plan, outcomes) = plan_writes(&corpus, "seeded", &[superseding], &run);

        assert_eq!(outcomes[0], WritePlanOutcome::Appended { ordinal: 2 });
        // Both entries changed: seeded (new claim) and other (superseded tag).
        assert_eq!(plan.entries.len(), 2);
        let other = plan.entries.get("entries/other.md").unwrap();
        assert!(
            other.contains("[superseded-by: seeded#claim-2]"),
            "cross-topic claim tagged"
        );
        assert!(other.contains(&run.to_string()), "target updated bumped");
        assert!(plan.index.is_some());
    }

    #[test]
    fn plan_writes_index_cap_rejects_new_topic() {
        // Build a corpus with 38 entries (2 + 38 = 40, at the cap).
        let mut files = BTreeMap::new();
        for i in 0..38 {
            let slug = format!("topic-{i:02}");
            files.insert(
                format!("entries/{slug}.md"),
                format!(
                    "# Title {i}\n\n- slug: {slug}\n- created: run_01a090d4-5628-77d3-8f53-1abe34dbda5d\n- updated: run_01a090d4-5628-77d3-8f53-1abe34dbda5d\n\n## Claims\n\n- Fact {i}. [src: msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9]\n"
                ),
            );
        }
        let corpus = MemoryCorpus::parse(&files).unwrap();
        assert_eq!(corpus.index_lines(), 40);
        let run = run_a();
        let claims = vec![stamped(
            "Would overflow.",
            vec![msg("msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9")],
            vec![],
        )];
        let (plan, outcomes) = plan_writes(&corpus, "new-topic", &claims, &run);

        assert_eq!(
            outcomes[0],
            WritePlanOutcome::Rejected {
                reason: MemoryClaimRejection::IndexCapReached,
            }
        );
        assert!(plan.entries.is_empty());
        assert!(plan.index.is_none());
    }

    #[test]
    fn plan_writes_preserves_existing_stamps() {
        let corpus = seeded_corpus();
        let run = run_b();
        let claims = vec![stamped(
            "Brand new claim with a stamped file.",
            vec![],
            vec![FileSource {
                path: "notes/new.md".to_owned(),
                digest: Some("abcdef123456".to_owned()),
            }],
        )];
        let (plan, _) = plan_writes(&corpus, "seeded", &claims, &run);

        let rendered = plan.entries.get("entries/seeded.md").unwrap();
        // Existing stamp preserved verbatim.
        assert!(rendered.contains("notes/a.md@0123456789ab"));
        // New stamp present.
        assert!(rendered.contains("notes/new.md@abcdef123456"));
    }

    #[test]
    fn plan_writes_dropped_claim_emits_rejection() {
        let corpus = seeded_corpus();
        let run = run_b();
        let dropped = StampedOutcome::Dropped {
            index: 0,
            reason: MemoryClaimRejection::UnreadableOrEscapingPath,
        };
        let (plan, outcomes) = plan_writes(&corpus, "seeded", &[dropped], &run);

        assert_eq!(
            outcomes[0],
            WritePlanOutcome::Rejected {
                reason: MemoryClaimRejection::UnreadableOrEscapingPath,
            }
        );
        assert!(plan.entries.is_empty());
        assert!(plan.index.is_none());
    }

    #[test]
    fn plan_writes_invalid_supersession_rejects() {
        let corpus = seeded_corpus();
        let run = run_b();
        // Reference a non-existent ordinal.
        let bad = StampedOutcome::Stamped {
            index: 0,
            claim: StampedClaim {
                text: "Bad ref.".to_owned(),
                sources: vec![msg("msg_01a090d5-4b9b-71b0-aae8-d5421f5c3513")],
                files: vec![],
                supersedes: Some(vec!["seeded#claim-99".to_owned()]),
            },
        };
        let (plan, outcomes) = plan_writes(&corpus, "seeded", &[bad], &run);

        assert_eq!(
            outcomes[0],
            WritePlanOutcome::Rejected {
                reason: MemoryClaimRejection::InvalidSupersession,
            }
        );
        assert!(plan.entries.is_empty());
        assert!(plan.index.is_none());
    }

    #[test]
    fn plan_writes_deterministic_changed_file_set() {
        // Two identical plans from the same inputs produce identical results.
        let corpus = seeded_corpus();
        let run = run_b();
        let claims = vec![
            stamped(
                "First.",
                vec![msg("msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9")],
                vec![],
            ),
            stamped(
                "Second.",
                vec![msg("msg_01a090d5-4b9b-71b0-aae8-d5421f5c3513")],
                vec![],
            ),
        ];
        let (plan1, outcomes1) = plan_writes(&corpus, "seeded", &claims, &run);
        let (plan2, outcomes2) = plan_writes(&corpus, "seeded", &claims, &run);

        assert_eq!(plan1, plan2);
        assert_eq!(outcomes1, outcomes2);
        // Only the seeded entry changed.
        assert_eq!(plan1.entries.len(), 1);
        assert!(plan1.entries.contains_key("entries/seeded.md"));
    }

    #[test]
    fn workspace_write_creates_entry_and_keeps_index_consistent() {
        let root = temp_root();
        let workspace = MemoryWorkspace::new(&root).unwrap();
        let corpus = MemoryCorpus::parse(&BTreeMap::new()).unwrap();
        let claims = vec![stamped(
            "Created on disk.",
            vec![msg("msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9")],
            vec![],
        )];
        let (plan, outcomes) = workspace
            .plan_writes(&corpus, "created", &claims, &run_a())
            .unwrap();
        assert_eq!(outcomes, vec![WritePlanOutcome::Appended { ordinal: 1 }]);
        workspace.apply_write_plan(&plan).unwrap();

        let loaded = workspace.load_corpus().unwrap().unwrap();
        assert_eq!(
            loaded.entry("created").unwrap().claims[0].text,
            "Created on disk."
        );
        assert_eq!(
            fs::read_to_string(root.join("memory/INDEX.md")).unwrap(),
            loaded.render_index()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_write_rejects_stale_plan_evidence() {
        let root = temp_root();
        write(&root, "memory/INDEX.md", "# Memory Index\n");
        write(&root, "memory/entries/seeded.md", SEED_ENTRY);
        let workspace = MemoryWorkspace::new(&root).unwrap();
        let corpus = workspace.load_corpus().unwrap().unwrap();
        let (plan, _) = workspace
            .plan_writes(
                &corpus,
                "seeded",
                &[stamped(
                    "Planned before another writer.",
                    vec![msg("msg_01a090d5-4b9b-71b0-aae8-d5421f5c3513")],
                    vec![],
                )],
                &run_b(),
            )
            .unwrap();
        write(
            &root,
            "memory/entries/seeded.md",
            "changed after planning\n",
        );
        assert!(matches!(
            workspace.apply_write_plan(&plan),
            Err(MemoryWriteError::Stale { path }) if path == "memory/entries/seeded.md"
        ));
        assert_eq!(
            fs::read_to_string(root.join("memory/entries/seeded.md")).unwrap(),
            "changed after planning\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_write_rejects_unexpected_files_and_symlink_paths() {
        let root = temp_root();
        write(&root, "memory/INDEX.md", "# Memory Index\n");
        let workspace = MemoryWorkspace::new(&root).unwrap();
        let corpus = MemoryCorpus::parse(&BTreeMap::new()).unwrap();
        let (plan, _) = workspace
            .plan_writes(
                &corpus,
                "created",
                &[stamped(
                    "A planned claim.",
                    vec![msg("msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9")],
                    vec![],
                )],
                &run_a(),
            )
            .unwrap();
        write(&root, "memory/entries/unexpected.md", "unexpected\n");
        assert!(matches!(
            workspace.apply_write_plan(&plan),
            Err(MemoryWriteError::UnexpectedPath { .. })
        ));

        let outside = root.join("outside");
        fs::create_dir_all(outside.join("entries")).unwrap();
        fs::remove_file(root.join("memory/entries/unexpected.md")).unwrap();
        fs::remove_dir_all(root.join("memory")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("memory")).unwrap();
        assert_eq!(
            workspace.apply_write_plan(&plan),
            Err(MemoryWriteError::EscapingPath)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_write_rolls_back_after_partial_io_failure() {
        let root = temp_root();
        let mut files = BTreeMap::new();
        files.insert("entries/seeded.md".to_owned(), SEED_ENTRY.to_owned());
        files.insert(
            "entries/other.md".to_owned(),
            "# Other topic\n\n- slug: other\n- created: run_01a090d4-5628-77d3-8f53-1abe34dbda5d\n- updated: run_01a090d4-5628-77d3-8f53-1abe34dbda5d\n\n## Claims\n\n- Old cross-topic fact. [src: msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9]\n".to_owned(),
        );
        let corpus = MemoryCorpus::parse(&files).unwrap();
        write(&root, "memory/INDEX.md", &corpus.render_index());
        for (path, text) in &files {
            write(&root, &format!("memory/{path}"), text);
        }
        let workspace = MemoryWorkspace::new(&root).unwrap();
        let superseding = StampedOutcome::Stamped {
            index: 0,
            claim: StampedClaim {
                text: "Replacement fact.".to_owned(),
                sources: vec![msg("msg_01a090d5-4b9b-71b0-aae8-d5421f5c3513")],
                files: vec![],
                supersedes: Some(vec!["other#claim-1".to_owned()]),
            },
        };
        let (plan, _) = workspace
            .plan_writes(&corpus, "seeded", &[superseding], &run_b())
            .unwrap();
        let before_seeded = fs::read(root.join("memory/entries/seeded.md")).unwrap();
        let before_other = fs::read(root.join("memory/entries/other.md")).unwrap();
        let error = apply_write_plan_inner(
            &root,
            &plan,
            Some(TransactionFault {
                fail_after: Some(1),
                fail_rollback: false,
            }),
        )
        .unwrap_err();
        assert!(matches!(error, MemoryWriteError::Io { .. }));
        assert_eq!(
            fs::read(root.join("memory/entries/seeded.md")).unwrap(),
            before_seeded
        );
        assert_eq!(
            fs::read(root.join("memory/entries/other.md")).unwrap(),
            before_other
        );
        assert_eq!(
            fs::read_to_string(root.join("memory/INDEX.md")).unwrap(),
            corpus.render_index()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_write_surfaces_rollback_failure() {
        let root = temp_root();
        let workspace = MemoryWorkspace::new(&root).unwrap();
        let corpus = MemoryCorpus::parse(&BTreeMap::new()).unwrap();
        let (plan, _) = workspace
            .plan_writes(
                &corpus,
                "created",
                &[stamped(
                    "A planned claim.",
                    vec![msg("msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9")],
                    vec![],
                )],
                &run_a(),
            )
            .unwrap();
        assert!(matches!(
            apply_write_plan_inner(
                &root,
                &plan,
                Some(TransactionFault {
                    fail_after: Some(1),
                    fail_rollback: true,
                }),
            ),
            Err(MemoryWriteError::RollbackFailed { .. })
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_lock_is_shared_by_canonical_root() {
        let root = temp_root();
        let first = crate::tools::workspace_mutation(&root);
        let second = crate::tools::workspace_mutation(&root);
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_write_retries_once_with_fresh_evidence() {
        let root = temp_root();
        write(&root, "memory/INDEX.md", "# Memory Index\n");
        write(&root, "memory/entries/seeded.md", SEED_ENTRY);
        let workspace = MemoryWorkspace::new(&root).unwrap();
        let corpus = workspace.load_corpus().unwrap().unwrap();
        let (stale, _) = workspace
            .plan_writes(
                &corpus,
                "seeded",
                &[stamped(
                    "Retried claim.",
                    vec![msg("msg_01a090d5-4b9b-71b0-aae8-d5421f5c3513")],
                    vec![],
                )],
                &run_b(),
            )
            .unwrap();
        write(&root, "memory/INDEX.md", "# intervening writer\n");
        let mut attempts = 0;
        workspace
            .apply_write_plan_with_retry(&stale, || {
                attempts += 1;
                let corpus = workspace.load_corpus().unwrap().unwrap();
                workspace
                    .plan_writes(
                        &corpus,
                        "seeded",
                        &[stamped(
                            "Retried claim.",
                            vec![msg("msg_01a090d5-4b9b-71b0-aae8-d5421f5c3513")],
                            vec![],
                        )],
                        &run_b(),
                    )
                    .map(|(plan, _)| plan)
            })
            .unwrap();
        assert_eq!(attempts, 1);
        assert!(
            fs::read_to_string(root.join("memory/entries/seeded.md"))
                .unwrap()
                .contains("Retried claim.")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_write_does_not_retry_stale_plan_without_new_evidence() {
        let root = temp_root();
        write(&root, "memory/INDEX.md", "# Memory Index\n");
        write(&root, "memory/entries/seeded.md", SEED_ENTRY);
        let workspace = MemoryWorkspace::new(&root).unwrap();
        let corpus = workspace.load_corpus().unwrap().unwrap();
        let (stale, _) = workspace
            .plan_writes(
                &corpus,
                "seeded",
                &[stamped(
                    "Must not overwrite.",
                    vec![msg("msg_01a090d5-4b9b-71b0-aae8-d5421f5c3513")],
                    vec![],
                )],
                &run_b(),
            )
            .unwrap();
        let intervening = "intervening writer\n";
        write(&root, "memory/entries/seeded.md", intervening);
        let mut attempts = 0;
        let error = workspace
            .apply_write_plan_with_retry(&stale, || {
                attempts += 1;
                Ok(stale.clone())
            })
            .unwrap_err();
        assert_eq!(attempts, 1);
        assert!(matches!(error, MemoryWriteError::Stale { .. }));
        assert_eq!(
            fs::read_to_string(root.join("memory/entries/seeded.md")).unwrap(),
            intervening
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn forge_owned_materialization_rejects_in_place_writes() {
        let root = temp_root();
        fs::create_dir_all(root.join("materialized")).unwrap();
        let workspace = MemoryWorkspace {
            root: root.join("materialized"),
            workspace_root: root.clone(),
            forge_owned: true,
        };
        let plan = WritePlan {
            entries: [("entries/topic.md".to_owned(), "new\n".to_owned())]
                .into_iter()
                .collect(),
            index: Some("# Memory Index\n".to_owned()),
            evidence: None,
        };
        assert_eq!(
            workspace.apply_write_plan(&plan),
            Err(MemoryWriteError::ForgeOwnedMaterialization)
        );
        assert!(!workspace.root.join("entries/topic.md").exists());
        let _ = fs::remove_dir_all(root);
    }
}
