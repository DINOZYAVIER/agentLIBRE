//! Terminal-run memory publication. Git owns objects; the journal proves
//! which source commit belongs to this publication across daemon restarts.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agl_core::AgentRunId;
use anyhow::{Context, Result, ensure};
use ayeque_forge_core::{GitCommand, GitRunner};
use serde::{Deserialize, Serialize};

pub(crate) struct MemoryPublisher {
    data: PathBuf,
    storage: PathBuf,
    runner: Arc<GitRunner>,
    timeout: Duration,
}
impl MemoryPublisher {
    pub(crate) fn new(
        data: PathBuf,
        storage: PathBuf,
        runner: Arc<GitRunner>,
        timeout: Duration,
    ) -> Result<Self> {
        ensure!(!timeout.is_zero(), "Forge sync timeout must be positive");
        Ok(Self {
            data,
            storage,
            runner,
            timeout,
        })
    }

    pub(crate) fn after_run(&self, store: &crate::store::StoreHandle, run_id: AgentRunId) {
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.publish(store, run_id)))
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!("memory publication interrupted by a panic"))
                });
        if let Err(error) = result {
            let details = error
                .downcast_ref::<ayeque_forge_core::VerificationRequired>()
                .and_then(|request| serde_json::to_value(request).ok())
                .and_then(|value| agl_core::CanonicalJson::new(value).ok());
            tracing::warn!(%run_id, reason=%error, "memory publication deferred; claims remain pending");
            if let Err(event_error) =
                store.record_memory_publication_deferred(run_id, format!("{error:#}"), details)
            {
                tracing::warn!(%run_id, %event_error, "failed to record deferred memory publication");
            }
        }
    }

    fn publish(&self, store: &crate::store::StoreHandle, run_id: AgentRunId) -> Result<()> {
        let run = store.agent_run(run_id)?;
        ensure!(
            run.status.is_terminal(),
            "memory publication requires a terminal Run"
        );
        let workspace = run.snapshot.workspace.root.as_path().canonicalize()?;
        self.publish_at(store, &workspace, run_id)
    }
    fn publish_at(
        &self,
        store: &crate::store::StoreHandle,
        workspace: &Path,
        run_id: AgentRunId,
    ) -> Result<()> {
        self.publish_workspace(store, workspace, PublicationOrigin::Run { run_id })
            .map(|_| ())
    }
    pub(crate) fn archive(
        &self,
        store: &crate::store::StoreHandle,
        workspace: &Path,
        slug: &str,
        request_id: agl_core::RequestId,
    ) -> Result<String> {
        ensure!(agl_core::agent::is_valid_slug(slug), "invalid memory slug");
        self.publish_workspace(
            store,
            workspace,
            PublicationOrigin::Archive {
                request_id,
                slug: slug.to_owned(),
            },
        )?
        .context("archive publication returned no revision")
    }
    fn publish_workspace(
        &self,
        store: &crate::store::StoreHandle,
        workspace: &Path,
        origin: PublicationOrigin,
    ) -> Result<Option<String>> {
        let workspace = workspace.canonicalize()?;
        let path = self.data.join("forge/memory-publications").join(format!(
            "{}.json",
            ayeque_forge_core::sha256_hex(workspace.as_os_str().as_encoded_bytes())
        ));
        let pending = store.pending_memory(&workspace)?;
        let maintenance = store.memory_maintenance(&workspace)?;
        if matches!(origin, PublicationOrigin::Run { .. })
            && pending.is_empty()
            && maintenance.is_none()
            && !path.try_exists()?
        {
            return Ok(None);
        }
        // Take the workspace writer before reading/editing its declaration.
        let mut mutation = ayeque_forge_core::ForgeMutation::begin(&self.storage, &workspace)?;
        let mut declarations = agl_runtime::read_workspace_forge_declarations_at(&workspace)?;
        let memory = declarations
            .iter()
            .filter(|source| source.kind == "memory")
            .collect::<Vec<_>>();
        ensure!(
            memory.len() == 1,
            "workspace must declare exactly one memory source to publish pending claims"
        );
        let source = memory[0].clone();
        let local = PathBuf::from(&source.git);
        let local = if local.is_absolute() {
            local
        } else {
            workspace.join(local)
        };
        let local = local.canonicalize().context("memory publication requires a local authoring Git repository; URL sources stay pending")?;
        let mut git = SourceGit {
            root: local,
            runner: self.runner.clone(),
            deadline: Instant::now()
                .checked_add(self.timeout)
                .context("memory publication timeout is too large")?,
        };
        let git_root =
            PathBuf::from(git.text(&["rev-parse", "--show-toplevel"])?).canonicalize()?;
        git.root = git_root.clone();
        ensure!(
            !workspace
                .join(agl_runtime::WORKSPACE_CONFIG_FILE_NAME)
                .starts_with(&git_root),
            "memory authoring repo must be separate from the workspace manifest"
        );
        let _authoring = AuthoringGuard::acquire(&self.data, &git_root)?;
        let mut journal = if path.try_exists()? {
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)?;
            ensure!(
                file.metadata()?.is_file(),
                "memory publication journal is not a regular file"
            );
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let journal: PublicationJournal = serde_json::from_slice(&bytes)?;
            if let PublicationOrigin::Archive { slug, .. } = &origin {
                ensure!(
                    matches!(&journal.origin, PublicationOrigin::Archive {slug: existing, ..} if existing == slug),
                    "Busy: another memory publication is pending"
                );
            }
            ensure!(
                journal.workspace == workspace && journal.git_root == git_root,
                "Busy: memory journal workspace/source changed"
            );
            ensure!(
                journal.source.id == source.id
                    && journal.source.git == source.git
                    && journal.source.path == source.path
                    && journal.source.kind == source.kind
                    && journal.source.schema == source.schema,
                "Busy: memory declaration changed during publication"
            );
            ensure!(
                source.revision == journal.source.revision
                    || journal.commit.as_ref() == Some(&source.revision),
                "Busy: memory declaration revision is foreign"
            );
            journal
        } else {
            ensure!(
                git.text(&["rev-parse", "HEAD"])? == source.revision,
                "Busy: memory HEAD differs from declared revision"
            );
            git.clean()?;
            let changes = match &origin {
                PublicationOrigin::Run { run_id } => plan_publication(
                    &git,
                    &workspace,
                    &source,
                    &pending,
                    *run_id,
                    maintenance.is_some(),
                )?,
                PublicationOrigin::Archive { slug, .. } => {
                    (plan_archive(&git, &workspace, &source, slug)?, vec![])
                }
            };
            let journal = PublicationJournal {
                workspace: workspace.clone(),
                git_root: git_root.clone(),
                source: source.clone(),
                maintenance: if matches!(origin, PublicationOrigin::Run { .. }) {
                    maintenance
                } else {
                    None
                },
                origin,
                head_ref: git.current_ref()?,
                old_tree: git.text(&["rev-parse", "HEAD^{tree}"])?,
                new_tree: None,
                commit: None,
                changes: changes.0,
                pending: changes.1,
            };
            write_journal(&path, &journal)?;
            journal
        };
        if !journal.changes.is_empty() {
            prepare_commit(&git, &path, &mut journal)?;
            apply_source_commit(&git, &journal)?;
            declarations
                .iter_mut()
                .find(|entry| entry.id == source.id)
                .context("memory declaration missing")?
                .revision = journal.commit.clone().context("memory commit missing")?;
            let bytes = agl_runtime::serialize_workspace_forge_declarations(
                &workspace,
                declarations.clone(),
            )?;
            atomic_bytes(
                &workspace.join(agl_runtime::WORKSPACE_CONFIG_FILE_NAME),
                &bytes,
            )?;
            mutation.set_sync_timeout(self.timeout)?;
            let runner = self.runner.clone();
            mutation.set_git_runner(move |request| runner(request));
            let saved = agl_runtime::forge_verification::VerificationChoices::new(&self.data)?;
            let choices = declarations.clone();
            mutation.set_verification_choices(move |request| {
                let source = choices
                    .iter()
                    .find(|entry| entry.id == request.source.id())
                    .context("verification source is missing")?;
                let key = agl_runtime::forge_verification::VerificationKey::new(
                    &request.workspace,
                    source,
                    &request.relative_file_path,
                )?;
                Ok(saved.get(&key)?.map(|decision| match decision {
                    agl_runtime::forge_verification::VerificationDecision::Verify => {
                        ayeque_forge_core::VerificationDecision::Verify
                    }
                    agl_runtime::forge_verification::VerificationDecision::Skip => {
                        ayeque_forge_core::VerificationDecision::Skip
                    }
                }))
            });
            mutation.sync(&agl_runtime::workspace_backend_declarations(&declarations)?)?;
        } else {
            ensure!(
                git.text(&["rev-parse", "HEAD"])? == journal.source.revision,
                "Busy: memory HEAD changed before queue completion"
            );
            git.clean()?;
        }
        store.complete_pending_memory(&workspace, &journal.pending)?;
        if let Some(request) = &journal.maintenance {
            store.complete_memory_maintenance(&workspace, request)?;
        }
        fs::remove_file(&path)?;
        File::open(path.parent().context("memory journal parent missing")?)?.sync_all()?;
        tracing::info!(origin=?journal.origin, commit=?journal.commit, "memory publication completed");
        Ok(Some(journal.commit.unwrap_or(journal.source.revision)))
    }
}

type PendingResult = (
    crate::store::PendingMemory,
    Vec<(usize, agl_core::agent::MemoryClaimRejection)>,
);
fn plan_publication(
    git: &SourceGit,
    workspace: &Path,
    source: &agl_runtime::ForgeDeclaration,
    pending: &[crate::store::PendingMemory],
    run_id: AgentRunId,
    maintenance: bool,
) -> Result<(Vec<SourceChange>, Vec<PendingResult>)> {
    let memory = git.root.join(&source.path);
    let memory = memory
        .canonicalize()
        .context("memory authoring directory is unavailable")?;
    ensure!(
        memory.starts_with(&git.root),
        "memory authoring path escapes the Git root"
    );
    let root = source_root(&git.root)?;
    // Check components through pinned directory descriptors, not symlinks.
    let prefix = if source.path == "." {
        String::new()
    } else {
        path_components(&source.path)?;
        format!("{}/", source.path)
    };
    source_parent(&root, &format!("{prefix}INDEX.md"), false)?;
    let reader = super::memory_io::MemoryWorkspace::authoring(&memory, workspace)
        .map_err(|error| anyhow::anyhow!("memory corpus: {error:?}"))?;
    let mut corpus = reader
        .load_corpus()
        .map_err(|error| anyhow::anyhow!("memory corpus: {error:?}"))?
        .context("memory corpus is absent")?;
    let mut changed = BTreeMap::new();
    if maintenance {
        for slug in corpus.slugs().map(str::to_owned).collect::<Vec<_>>() {
            let entry = corpus.entry_mut(&slug).unwrap();
            let mut dirty = false;
            for claim in &mut entry.claims {
                if claim.status != Some(agl_core::agent::ClaimStatus::Stale)
                    && claim.files.iter().any(|file| {
                        reader
                            .stamped_file(&file.path)
                            .is_none_or(|current| current.digest != file.digest)
                    })
                {
                    claim.status = Some(agl_core::agent::ClaimStatus::Stale);
                    dirty = true;
                }
            }
            if dirty {
                entry.updated = run_id;
                changed.insert(
                    format!("entries/{slug}.md"),
                    agl_core::agent::render_entry(entry),
                );
            }
        }
    }
    let mut files = corpus
        .slugs()
        .map(|slug| {
            (
                format!("entries/{slug}.md"),
                agl_core::agent::render_entry(corpus.entry(slug).unwrap()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut index = (!changed.is_empty()).then(|| corpus.render_index());
    let mut completed = Vec::new();
    for record in pending {
        ensure!(
            Instant::now() < git.deadline,
            "memory publication deadline exceeded"
        );
        let stamped = reader.stamp_topic(&record.topic);
        let (plan, outcomes) =
            super::memory_io::plan_writes(&corpus, &record.topic.slug, &stamped, &run_id);
        let rejections = outcomes
            .iter()
            .enumerate()
            .filter_map(|(index, outcome)| match outcome {
                super::memory_io::WritePlanOutcome::Rejected { reason } => Some((index, *reason)),
                _ => None,
            })
            .collect();
        for (path, contents) in plan.entries {
            files.insert(path.clone(), contents.clone());
            changed.insert(path, contents);
        }
        if plan.index.is_some() {
            index = plan.index;
        }
        corpus = agl_core::agent::MemoryCorpus::parse(&files)?;
        completed.push((record.clone(), rejections));
    }
    if let Some(index) = index {
        changed.insert("INDEX.md".into(), index);
    }
    let changes = changed
        .into_iter()
        .map(|(path, contents)| {
            let path = format!("{prefix}{path}");
            let before = read_source(&root, &path)?;
            Ok(SourceChange {
                path,
                mode: before.as_ref().map(|(_, mode)| *mode).unwrap_or(0o644),
                before: before.map(|(bytes, _)| bytes),
                after: Some(contents.into_bytes()),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((changes, completed))
}

fn plan_archive(
    git: &SourceGit,
    workspace: &Path,
    source: &agl_runtime::ForgeDeclaration,
    slug: &str,
) -> Result<Vec<SourceChange>> {
    ensure!(agl_core::agent::is_valid_slug(slug), "invalid memory slug");
    let prefix = if source.path == "." {
        String::new()
    } else {
        path_components(&source.path)?;
        format!("{}/", source.path)
    };
    let root = source_root(&git.root)?;
    let entry_path = format!("{prefix}entries/{slug}.md");
    let archive_path = format!("{prefix}archive/{slug}.md");
    let (bytes, mode) = read_source(&root, &entry_path)?.context("memory entry does not exist")?;
    ensure!(
        read_source(&root, &archive_path)?.is_none(),
        "archive destination already exists"
    );
    let reader =
        super::memory_io::MemoryWorkspace::authoring(&git.root.join(&source.path), workspace)
            .map_err(|error| anyhow::anyhow!("memory corpus: {error:?}"))?;
    let corpus = reader
        .load_corpus()
        .map_err(|error| anyhow::anyhow!("memory corpus: {error:?}"))?
        .context("memory corpus is absent")?;
    ensure!(corpus.entry(slug).is_some(), "memory entry slug is absent");
    let remaining = corpus
        .slugs()
        .filter(|value| *value != slug)
        .map(|slug| {
            (
                format!("entries/{slug}.md"),
                agl_core::agent::render_entry(corpus.entry(slug).unwrap()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let index = agl_core::agent::MemoryCorpus::parse(&remaining)?.render_index();
    let index_path = format!("{prefix}INDEX.md");
    let before = read_source(&root, &index_path)?;
    Ok(vec![
        SourceChange {
            path: entry_path,
            before: Some(bytes.clone()),
            after: None,
            mode,
        },
        SourceChange {
            path: archive_path,
            before: None,
            after: Some(bytes),
            mode,
        },
        SourceChange {
            path: index_path,
            mode: before.as_ref().map(|(_, mode)| *mode).unwrap_or(0o644),
            before: before.map(|(bytes, _)| bytes),
            after: Some(index.into_bytes()),
        },
    ])
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceChange {
    path: String,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
    mode: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum PublicationOrigin {
    Run {
        run_id: AgentRunId,
    },
    Archive {
        request_id: agl_core::RequestId,
        slug: String,
    },
}
impl PublicationOrigin {
    fn message(&self) -> String {
        match self {
            Self::Run { run_id } => format!("agl: memory for {run_id}"),
            Self::Archive { slug, .. } => format!("agl: archive memory {slug}"),
        }
    }
    fn key(&self) -> String {
        match self {
            Self::Run { run_id } => run_id.to_string(),
            Self::Archive { request_id, .. } => request_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationJournal {
    workspace: PathBuf,
    git_root: PathBuf,
    source: agl_runtime::ForgeDeclaration,
    origin: PublicationOrigin,
    maintenance: Option<agl_core::RequestId>,
    head_ref: String,
    old_tree: String,
    new_tree: Option<String>,
    commit: Option<String>,
    changes: Vec<SourceChange>,
    pending: Vec<(
        crate::store::PendingMemory,
        Vec<(usize, agl_core::agent::MemoryClaimRejection)>,
    )>,
}

struct AuthoringGuard(File);
impl AuthoringGuard {
    fn acquire(data: &Path, root: &Path) -> Result<Self> {
        let locks = data.join("forge/authoring-locks");
        fs::create_dir_all(&locks)?;
        let name = ayeque_forge_core::sha256_hex(root.as_os_str().as_encoded_bytes());
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(locks.join(name))?;
        ensure!(
            file.metadata()?.is_file(),
            "authoring lock is not a regular file"
        );
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(ayeque_forge_core::Busy.into());
        }
        Ok(Self(file))
    }
}
impl Drop for AuthoringGuard {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

struct SourceGit {
    root: PathBuf,
    runner: Arc<GitRunner>,
    deadline: Instant,
}
impl SourceGit {
    fn run(
        &self,
        args: impl IntoIterator<Item = OsString>,
        input: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        ensure!(
            Instant::now() < self.deadline,
            "memory publication deadline exceeded"
        );
        // Harden the objects proved by the durable journal, then index/refs.
        // https://git-scm.com/docs/git-config#Documentation/git-config.txt-corefsync
        let args = [
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.fsync=objects,index,reference",
            "-c",
            "core.fsyncMethod=fsync",
        ]
        .into_iter()
        .map(OsString::from)
        .chain(args)
        .collect();
        let stdin = input.map(input_file).transpose()?;
        let output = (self.runner)(GitCommand {
            cwd: self.root.clone(),
            args,
            stdin,
            stdout: None,
            deadline: self.deadline,
        })?;
        ensure!(
            Instant::now() < self.deadline,
            "memory publication deadline exceeded"
        );
        ensure!(
            output.status.success(),
            "memory Git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(output.stdout)
    }
    fn text(&self, args: &[&str]) -> Result<String> {
        Ok(
            String::from_utf8(self.run(args.iter().map(OsString::from), None)?)?
                .trim()
                .to_owned(),
        )
    }
    fn current_ref(&self) -> Result<String> {
        // rev-parse --symbolic-full-name also handles a detached HEAD.
        self.text(&["rev-parse", "--symbolic-full-name", "HEAD"])
    }
    fn clean(&self) -> Result<()> {
        ensure!(
            self.run(
                ["status", "--porcelain=v1", "-z", "--untracked-files=all"].map(OsString::from),
                None
            )?
            .is_empty(),
            "Busy: memory Git repository has tracked or untracked changes"
        );
        Ok(())
    }
    fn object(&self, args: &[&str], input: Option<&[u8]>) -> Result<String> {
        let value = String::from_utf8(self.run(args.iter().map(OsString::from), input)?)?
            .trim()
            .to_owned();
        ensure!(
            matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Git did not return a full object id"
        );
        Ok(value)
    }

    /// Replace only declared paths, preserving every other raw tree entry.
    /// No checkout filters, hooks, shared index or shell interpretation.
    fn write_tree(
        &self,
        original: &str,
        changes: &BTreeMap<Vec<Vec<u8>>, Option<Vec<u8>>>,
    ) -> Result<String> {
        let bytes = self.run(["ls-tree", "-z", original].map(OsString::from), None)?;
        let mut entries = BTreeMap::<Vec<u8>, TreeEntry>::new();
        for record in bytes
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            let tab = record
                .iter()
                .position(|byte| *byte == b'\t')
                .context("invalid Git tree record")?;
            let fields = std::str::from_utf8(&record[..tab])?
                .split(' ')
                .collect::<Vec<_>>();
            ensure!(fields.len() == 3, "invalid Git tree header");
            entries.insert(
                record[tab + 1..].to_vec(),
                TreeEntry {
                    mode: fields[0].to_owned(),
                    kind: fields[1].to_owned(),
                    oid: fields[2].to_owned(),
                },
            );
        }
        let mut children = BTreeMap::<Vec<u8>, BTreeMap<Vec<Vec<u8>>, Option<Vec<u8>>>>::new();
        for (components, contents) in changes {
            let (first, rest) = components.split_first().context("empty memory Git path")?;
            if rest.is_empty() {
                let Some(contents) = contents else {
                    let previous = entries
                        .remove(first)
                        .context("memory delete target is absent from Git tree")?;
                    ensure!(
                        previous.kind == "blob"
                            && matches!(previous.mode.as_str(), "100644" | "100755"),
                        "memory delete target is not a regular Git file"
                    );
                    continue;
                };
                let mode = match entries.get(first) {
                    Some(entry) => {
                        ensure!(
                            entry.kind == "blob"
                                && matches!(entry.mode.as_str(), "100644" | "100755"),
                            "memory write target is not a regular Git file"
                        );
                        entry.mode.clone()
                    }
                    None => "100644".to_owned(),
                };
                let oid = self.object(
                    &["hash-object", "-w", "--stdin", "--no-filters"],
                    Some(contents),
                )?;
                entries.insert(
                    first.clone(),
                    TreeEntry {
                        mode,
                        kind: "blob".to_owned(),
                        oid,
                    },
                );
            } else {
                children
                    .entry(first.clone())
                    .or_default()
                    .insert(rest.to_vec(), contents.clone());
            }
        }
        for (name, child_changes) in children {
            let tree = match entries.get(&name) {
                Some(entry) => {
                    ensure!(entry.kind == "tree", "memory parent is not a Git directory");
                    entry.oid.clone()
                }
                None => self.object(&["mktree", "-z"], Some(&[]))?,
            };
            let oid = self.write_tree(&tree, &child_changes)?;
            entries.insert(
                name,
                TreeEntry {
                    mode: "040000".into(),
                    kind: "tree".into(),
                    oid,
                },
            );
        }
        let mut input = Vec::new();
        for (name, entry) in entries {
            input.extend_from_slice(
                format!("{} {} {}\t", entry.mode, entry.kind, entry.oid).as_bytes(),
            );
            input.extend_from_slice(&name);
            input.push(0);
        }
        self.object(&["mktree", "-z"], Some(&input))
    }
}
struct TreeEntry {
    mode: String,
    kind: String,
    oid: String,
}

fn input_file(bytes: &[u8]) -> Result<File> {
    let fd = unsafe { libc::memfd_create(c"agl-memory-git".as_ptr(), libc::MFD_CLOEXEC) };
    ensure!(
        fd >= 0,
        "failed to allocate memory Git input: {}",
        std::io::Error::last_os_error()
    );
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)?;
    file.rewind()?;
    Ok(file)
}

fn write_journal(path: &Path, journal: &PublicationJournal) -> Result<()> {
    atomic_bytes(path, &serde_json::to_vec(journal)?)
}
fn atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("publication path has no parent")?;
    durable_directories(parent)?;
    let mode = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "publication target is not a regular file"
            );
            metadata.permissions().mode() & 0o777
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0o600,
        Err(error) => return Err(error.into()),
    };
    let temporary = parent.join(format!(
        ".agl-publication-{}",
        agl_core::RequestId::generate()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn durable_directories(path: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut cursor = path;
    while !cursor.try_exists()? {
        missing.push(cursor.to_owned());
        cursor = cursor
            .parent()
            .context("publication directory has no existing ancestor")?;
    }
    for directory in missing.into_iter().rev() {
        fs::create_dir(&directory)?;
        File::open(
            directory
                .parent()
                .context("publication directory parent missing")?,
        )?
        .sync_all()?;
    }
    Ok(())
}

fn path_components(path: &str) -> Result<Vec<Vec<u8>>> {
    let components = Path::new(path)
        .components()
        .map(|part| {
            let Component::Normal(value) = part else {
                anyhow::bail!("invalid memory Git path")
            };
            Ok(value.as_encoded_bytes().to_vec())
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(!components.is_empty(), "empty memory Git path");
    Ok(components)
}

fn prepare_commit(git: &SourceGit, path: &Path, journal: &mut PublicationJournal) -> Result<()> {
    ensure!(
        git.root == journal.git_root,
        "memory journal Git root mismatch"
    );
    ensure!(
        git.current_ref()? == journal.head_ref,
        "Busy: memory branch changed during publication"
    );
    if let Some(commit) = &journal.commit {
        let tree = journal
            .new_tree
            .as_ref()
            .context("memory journal has no tree")?;
        let proof = git.text(&["show", "--no-patch", "--format=%T%n%P%n%s", commit])?;
        ensure!(
            proof
                == format!(
                    "{tree}\n{}\n{}",
                    journal.source.revision,
                    journal.origin.message()
                ),
            "Busy: journal commit proof mismatch"
        );
        return Ok(());
    }
    ensure!(
        git.text(&["rev-parse", "HEAD"])? == journal.source.revision,
        "Busy: memory HEAD differs from declared revision"
    );
    git.clean()?;
    ensure!(
        !git.text(&["config", "user.name"])?.is_empty(),
        "memory repository user.name is empty"
    );
    ensure!(
        !git.text(&["config", "user.email"])?.is_empty(),
        "memory repository user.email is empty"
    );
    let changes = journal
        .changes
        .iter()
        .map(|change| Ok((path_components(&change.path)?, change.after.clone())))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let tree = git.write_tree(&journal.old_tree, &changes)?;
    journal.new_tree = Some(tree.clone());
    write_journal(path, journal)?;
    // Commit objects are created before HEAD moves. A crash before recording
    // this id leaves only an unreachable object and can safely retry.
    let commit = git.object(
        &["commit-tree", &tree, "-p", &journal.source.revision],
        Some(format!("{}\n", journal.origin.message()).as_bytes()),
    )?;
    journal.commit = Some(commit);
    write_journal(path, journal)
}

fn source_root(root: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(root)?)
}
fn name(value: &[u8]) -> Result<std::ffi::CString> {
    Ok(std::ffi::CString::new(value)?)
}
fn source_parent(
    root: &File,
    relative: &str,
    create: bool,
) -> Result<Option<(File, std::ffi::CString)>> {
    let mut components = path_components(relative)?;
    let leaf = name(&components.pop().context("missing memory filename")?)?;
    let mut parent = root.try_clone()?;
    for component in components {
        let component = name(&component)?;
        let mut fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            if !create {
                return Ok(None);
            }
            ensure!(
                unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o755) } == 0,
                "failed to create memory directory: {}",
                std::io::Error::last_os_error()
            );
            parent.sync_all()?;
            fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    component.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
        }
        ensure!(
            fd >= 0,
            "memory directory is missing or unsafe: {}",
            std::io::Error::last_os_error()
        );
        parent = unsafe { File::from_raw_fd(fd) };
    }
    Ok(Some((parent, leaf)))
}
fn read_source(root: &File, path: &str) -> Result<Option<(Vec<u8>, u32)>> {
    let Some((parent, leaf)) = source_parent(root, path, false)? else {
        return Ok(None);
    };
    read_at(&parent, &leaf)
}
fn read_at(parent: &File, leaf: &std::ffi::CStr) -> Result<Option<(Vec<u8>, u32)>> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
        return Ok(None);
    }
    ensure!(
        fd >= 0,
        "memory file is unsafe: {}",
        std::io::Error::last_os_error()
    );
    let mut file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "memory target is not a regular file");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some((bytes, metadata.permissions().mode() & 0o777)))
}
fn temporary_path(change: &SourceChange, run: impl std::fmt::Display) -> Result<String> {
    let parent = Path::new(&change.path)
        .parent()
        .context("memory file has no parent")?;
    let digest = ayeque_forge_core::sha256_hex(change.path.as_bytes());
    Ok(parent
        .join(format!(".agl-memory-{run}-{digest}"))
        .to_str()
        .context("non-UTF8 memory path")?
        .to_owned())
}

fn apply_source_commit(git: &SourceGit, journal: &PublicationJournal) -> Result<()> {
    let commit = journal
        .commit
        .as_ref()
        .context("memory journal has no commit")?;
    let tree = journal
        .new_tree
        .as_ref()
        .context("memory journal has no tree")?;
    ensure!(
        git.current_ref()? == journal.head_ref,
        "Busy: memory branch changed"
    );
    let head = git.text(&["rev-parse", "HEAD"])?;
    ensure!(
        head == journal.source.revision || head == *commit,
        "Busy: memory HEAD is foreign"
    );
    let index = git.text(&["write-tree"])?;
    ensure!(
        index == journal.old_tree || index == *tree,
        "Busy: memory index has foreign changes"
    );
    let root = source_root(&journal.git_root)?;
    let mut owned_paths = Vec::new();
    for change in &journal.changes {
        let current = read_source(&root, &change.path)?;
        let bytes = current.as_ref().map(|(bytes, _)| bytes);
        ensure!(
            bytes == change.before.as_ref() || bytes == change.after.as_ref(),
            "Busy: memory file has foreign changes: {}",
            change.path
        );
        if let Some((_, mode)) = current {
            ensure!(
                mode == change.mode,
                "Busy: memory file mode changed: {}",
                change.path
            );
        }
        owned_paths.push(change.path.as_bytes().to_vec());
        let temporary = temporary_path(change, journal.origin.key())?;
        if let Some((bytes, _)) = read_source(&root, &temporary)? {
            ensure!(
                change
                    .after
                    .as_ref()
                    .is_some_and(|after| after.starts_with(&bytes)),
                "Busy: memory staging file is foreign"
            );
        }
        owned_paths.push(temporary.into_bytes());
    }
    let status = git.run(
        [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ]
        .map(OsString::from),
        None,
    )?;
    for record in status
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        ensure!(
            record.len() > 3 && owned_paths.iter().any(|path| path == &record[3..]),
            "Busy: unrelated memory Git changes"
        );
    }
    for change in &journal.changes {
        ensure!(
            Instant::now() < git.deadline,
            "memory publication deadline exceeded"
        );
        if read_source(&root, &change.path)?
            .as_ref()
            .map(|(bytes, _)| bytes)
            == change.after.as_ref()
        {
            continue;
        }
        let (parent, leaf) =
            source_parent(&root, &change.path, true)?.context("memory parent unavailable")?;
        let Some(after) = &change.after else {
            ensure!(
                unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), 0) } == 0,
                "failed to delete memory source entry: {}",
                std::io::Error::last_os_error()
            );
            parent.sync_all()?;
            continue;
        };
        let temp = PathBuf::from(temporary_path(change, journal.origin.key())?);
        let temp = name(
            temp.file_name()
                .context("memory staging filename missing")?
                .as_encoded_bytes(),
        )?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                temp.as_ptr(),
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_TRUNC
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC,
                0o600,
            )
        };
        ensure!(
            fd >= 0,
            "failed to open memory staging file: {}",
            std::io::Error::last_os_error()
        );
        let mut file = unsafe { File::from_raw_fd(fd) };
        ensure!(
            file.metadata()?.is_file(),
            "memory staging is not a regular file"
        );
        file.write_all(after)?;
        file.set_permissions(fs::Permissions::from_mode(change.mode))?;
        file.sync_all()?;
        ensure!(
            unsafe {
                libc::renameat(
                    parent.as_raw_fd(),
                    temp.as_ptr(),
                    parent.as_raw_fd(),
                    leaf.as_ptr(),
                )
            } == 0,
            "failed to publish memory source file: {}",
            std::io::Error::last_os_error()
        );
        parent.sync_all()?;
    }
    for change in &journal.changes {
        let temp = temporary_path(change, journal.origin.key())?;
        if let Some((parent, leaf)) = source_parent(&root, &temp, false)?
            && read_at(&parent, &leaf)?.is_some()
        {
            ensure!(
                unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), 0) } == 0,
                "failed to remove owned memory staging file"
            );
            parent.sync_all()?;
        }
    }
    git.run(["read-tree", "--reset", commit].map(OsString::from), None)?;
    if head != *commit {
        git.run(
            [
                "update-ref",
                &journal.head_ref,
                commit,
                &journal.source.revision,
            ]
            .map(OsString::from),
            None,
        )?;
    }
    git.clean()
}

#[cfg(test)]
mod tests;
