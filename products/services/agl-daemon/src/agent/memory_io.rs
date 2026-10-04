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
//!   containment (mirroring the daemon `agentlibre.files:file.read` rules) — and stamped with
//!   `sha256-12`, the first 12 hex characters of the file's SHA-256 (M1 D3);
//! - unreadable or escaping file paths are dropped, not errors (D3); under
//!   the accepted S1=A source rule a claim survives when it keeps a valid
//!   message source OR at least one readable contained file source after the
//!   drops.
//!
//! Pure write plans feed memory_publication. Source commits, durable recovery
//! and Forge sync are owned by that publisher.

use std::collections::BTreeMap;
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
    memory_enabled: bool,
    _forge_memory: Option<(ayeque_forge_core::ForgeWorkspace, String)>,
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
                memory_enabled: true,
                _forge_memory: None,
            })
        }
        #[cfg(not(test))]
        {
            Self::new_forge(
                &workspace_root,
                &ayeque_forge_core::data_root()
                    .map_err(|error| CorpusLoadError::Forge(error.to_string()))?,
            )
        }
    }

    pub(crate) fn new_forge(root: &Path, storage: &Path) -> Result<Self, CorpusLoadError> {
        let workspace_root = root
            .canonicalize()
            .map_err(|_| CorpusLoadError::RootUnavailable)?;
        let declarations = agl_runtime::read_workspace_forge_declarations_at(&workspace_root)
            .map_err(|error| CorpusLoadError::Forge(error.to_string()))?;
        let mut memory = declarations.iter().filter(|source| source.kind == "memory");
        let Some(source) = memory.next() else {
            return Ok(Self {
                root: workspace_root.clone(),
                workspace_root,
                forge_owned: true,
                memory_enabled: false,
                _forge_memory: None,
            });
        };
        if memory.next().is_some() {
            return Err(CorpusLoadError::Forge("multiple memory sources".into()));
        }
        let sources = agl_runtime::workspace_backend_declarations(&declarations)
            .map_err(|error| CorpusLoadError::Forge(error.to_string()))?;
        let reader = ayeque_forge_core::ForgeWorkspace::open(storage, &workspace_root, &sources)
            .map_err(|error| CorpusLoadError::Forge(error.to_string()))?;
        let materialized = reader
            .materialized_path(&source.id)
            .map_err(|error| CorpusLoadError::Forge(error.to_string()))?;
        Ok(Self {
            root: materialized.to_path_buf(),
            workspace_root,
            forge_owned: true,
            memory_enabled: true,
            _forge_memory: Some((reader, source.id.clone())),
        })
    }

    pub(crate) fn authoring(memory: &Path, workspace: &Path) -> Result<Self, CorpusLoadError> {
        let root = memory
            .canonicalize()
            .map_err(|_| CorpusLoadError::RootUnavailable)?;
        let workspace_root = workspace
            .canonicalize()
            .map_err(|_| CorpusLoadError::RootUnavailable)?;
        if !root.is_dir() || !workspace_root.is_dir() {
            return Err(CorpusLoadError::RootUnavailable);
        }
        Ok(Self {
            root,
            workspace_root,
            forge_owned: true,
            memory_enabled: true,
            _forge_memory: None,
        })
    }

    pub(crate) fn load_corpus(&self) -> Result<Option<MemoryCorpus>, CorpusLoadError> {
        if !self.memory_enabled {
            return Ok(None);
        }
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
            if let Some((reader, id)) = &self._forge_memory {
                use std::io::Read as _;
                let relative = Path::new("entries").join(&name);
                let (mut file, _guard, _status) = reader
                    .open_file(id, &relative)
                    .map_err(|error| CorpusLoadError::Forge(error.to_string()))?;
                let mut content = String::new();
                file.read_to_string(&mut content)
                    .map_err(|_| CorpusLoadError::Unreadable(name.clone()))?;
                files.insert(format!("entries/{name}"), content);
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
        use std::io::Read as _;
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        use std::os::unix::fs::OpenOptionsExt as _;
        let components = rel
            .components()
            .filter_map(|part| match part {
                Component::Normal(value) => Some(value),
                _ => None,
            })
            .collect::<Vec<_>>();
        let (leaf, parents) = components.split_last()?;
        let mut directory = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.workspace_root)
            .ok()?;
        for component in parents {
            let name = std::ffi::CString::new(component.as_encoded_bytes()).ok()?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return None;
            }
            directory = unsafe { fs::File::from_raw_fd(fd) };
        }
        let name = std::ffi::CString::new(leaf.as_encoded_bytes()).ok()?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return None;
        }
        let mut file = unsafe { fs::File::from_raw_fd(fd) };
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let count = file.read(&mut buffer).ok()?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        let digest = hash.finalize();
        let digest = digest[..6]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Some(FileSource {
            path: relative.to_owned(),
            digest: Some(digest),
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
        // still rejected, mirroring the daemon agentlibre.files:file.read rules.
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
    fn cited_files_reject_special_nodes_and_parent_symlink_races() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let root = temp_root();
        let outside = root.with_extension("outside");
        fs::create_dir_all(root.join("citation")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("citation/file"), b"abc").unwrap();
        fs::write(outside.join("file"), b"outside bytes").unwrap();
        let fifo =
            std::ffi::CString::new(root.join("fifo").as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let workspace = MemoryWorkspace::new(&root).unwrap();
        assert!(workspace.stamped_file("fifo").is_none());
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_root = root.clone();
        let worker_outside = outside.clone();
        let worker = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                fs::rename(worker_root.join("citation"), worker_root.join("held")).unwrap();
                std::os::unix::fs::symlink(&worker_outside, worker_root.join("citation")).unwrap();
                fs::remove_file(worker_root.join("citation")).unwrap();
                fs::rename(worker_root.join("held"), worker_root.join("citation")).unwrap();
            }
        });
        let mut escaped = false;
        for _ in 0..1000 {
            if let Some(file) = workspace.stamped_file("citation/file") {
                escaped |= file.digest.as_deref() != Some("ba7816bf8f01");
            }
        }
        stop.store(true, Ordering::Release);
        worker.join().unwrap();
        assert!(!escaped);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
}
