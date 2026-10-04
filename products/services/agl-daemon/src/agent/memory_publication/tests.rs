use super::*;
use std::time::Duration;

struct Fixture {
    root: PathBuf,
    git: SourceGit,
    journal_path: PathBuf,
    journal: PublicationJournal,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "agl-memory-publication-{}",
            agl_core::RequestId::generate()
        ));
        fs::create_dir_all(root.join("repo/memory/entries")).unwrap();
        let git = SourceGit {
            root: root.join("repo"),
            runner: Arc::new(ayeque_forge_core::foreground),
            deadline: Instant::now() + Duration::from_secs(30),
        };
        git.text(&["init", "-q"]).unwrap();
        git.text(&["config", "user.name", "Memory Author"]).unwrap();
        git.text(&["config", "user.email", "memory@example.invalid"])
            .unwrap();
        fs::write(git.root.join("memory/entries/existing.md"), "previous\n").unwrap();
        fs::write(git.root.join("unrelated"), "untouched\n").unwrap();
        fs::write(git.root.join(".gitattributes"), "*.md text eol=crlf\n").unwrap();
        git.text(&["add", "."]).unwrap();
        git.text(&["commit", "-qm", "initial"]).unwrap();
        let revision = git.text(&["rev-parse", "HEAD"]).unwrap();
        let journal_path = root.join("journal.json");
        let journal = PublicationJournal {
            workspace: root.clone(),
            git_root: git.root.clone(),
            source: agl_runtime::ForgeDeclaration {
                id: "memory".into(),
                kind: "memory".into(),
                schema: "memory".into(),
                git: git.root.to_str().unwrap().to_owned(),
                revision,
                path: "memory".into(),
            },
            origin: PublicationOrigin::Run {
                run_id: AgentRunId::generate(),
            },
            maintenance: None,
            head_ref: git.current_ref().unwrap(),
            old_tree: git.text(&["rev-parse", "HEAD^{tree}"]).unwrap(),
            new_tree: None,
            commit: None,
            changes: vec![
                SourceChange {
                    path: "memory/entries/existing.md".into(),
                    before: Some(b"previous\n".to_vec()),
                    after: Some(b"updated\n".to_vec()),
                    mode: 0o644,
                },
                SourceChange {
                    path: "memory/entries/new.md".into(),
                    before: None,
                    after: Some(b"new sourced fact\n".to_vec()),
                    mode: 0o644,
                },
            ],
            pending: vec![],
        };
        write_journal(&journal_path, &journal).unwrap();
        Self {
            root,
            git,
            journal_path,
            journal,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn recorded_commit_resumes_before_head_move_and_preserves_unrelated_tree() {
    let mut fixture = Fixture::new();
    prepare_commit(&fixture.git, &fixture.journal_path, &mut fixture.journal).unwrap();
    let commit = fixture.journal.commit.clone().unwrap();
    assert_eq!(
        fixture.git.text(&["rev-parse", "HEAD"]).unwrap(),
        fixture.journal.source.revision
    );
    let mut recovered: PublicationJournal =
        serde_json::from_slice(&fs::read(&fixture.journal_path).unwrap()).unwrap();
    prepare_commit(&fixture.git, &fixture.journal_path, &mut recovered).unwrap();
    assert_eq!(recovered.commit.as_ref(), Some(&commit));
    apply_source_commit(&fixture.git, &recovered).unwrap();
    assert_eq!(fixture.git.text(&["rev-parse", "HEAD"]).unwrap(), commit);
    assert_eq!(
        fs::read(fixture.git.root.join("unrelated")).unwrap(),
        b"untouched\n"
    );
    assert_eq!(
        fixture
            .git
            .run(
                ["show", &format!("{commit}:memory/entries/new.md")].map(OsString::from),
                None
            )
            .unwrap(),
        b"new sourced fact\n"
    );
    assert_eq!(
        fixture
            .git
            .text(&["log", "-1", "--format=%an <%ae>%n%B"])
            .unwrap(),
        format!(
            "Memory Author <memory@example.invalid>\n{}",
            recovered.origin.message()
        )
    );
    apply_source_commit(&fixture.git, &recovered).unwrap();
    fixture.git.clean().unwrap();
}

#[test]
fn partial_staging_and_index_update_resume_without_another_commit() {
    let mut fixture = Fixture::new();
    prepare_commit(&fixture.git, &fixture.journal_path, &mut fixture.journal).unwrap();
    let temporary =
        temporary_path(&fixture.journal.changes[0], fixture.journal.origin.key()).unwrap();
    fs::write(fixture.git.root.join(temporary), b"upda").unwrap();
    fixture
        .git
        .text(&[
            "read-tree",
            "--reset",
            fixture.journal.commit.as_ref().unwrap(),
        ])
        .unwrap();
    apply_source_commit(&fixture.git, &fixture.journal).unwrap();
    fixture.git.clean().unwrap();
    assert_eq!(
        fs::read(fixture.git.root.join("memory/entries/existing.md")).unwrap(),
        b"updated\n"
    );
}

#[test]
fn foreign_head_dirty_file_and_index_are_refused() {
    let mut fixture = Fixture::new();
    prepare_commit(&fixture.git, &fixture.journal_path, &mut fixture.journal).unwrap();
    fs::write(fixture.git.root.join("unrelated"), "foreign\n").unwrap();
    assert!(
        apply_source_commit(&fixture.git, &fixture.journal)
            .unwrap_err()
            .to_string()
            .contains("unrelated")
    );
    fs::write(fixture.git.root.join("unrelated"), "untouched\n").unwrap();
    fs::write(
        fixture.git.root.join("memory/entries/existing.md"),
        "foreign\n",
    )
    .unwrap();
    assert!(
        apply_source_commit(&fixture.git, &fixture.journal)
            .unwrap_err()
            .to_string()
            .contains("foreign changes")
    );
    fixture.git.text(&["add", "."]).unwrap();
    assert!(
        apply_source_commit(&fixture.git, &fixture.journal)
            .unwrap_err()
            .to_string()
            .contains("index")
    );
    fixture.git.text(&["commit", "-qm", "foreign"]).unwrap();
    assert!(
        apply_source_commit(&fixture.git, &fixture.journal)
            .unwrap_err()
            .to_string()
            .contains("HEAD is foreign")
    );
}

#[test]
fn authoring_lock_is_per_git_root_and_released_on_drop() {
    let fixture = Fixture::new();
    let guard = AuthoringGuard::acquire(&fixture.root, &fixture.git.root).unwrap();
    assert!(
        AuthoringGuard::acquire(&fixture.root, &fixture.git.root)
            .err()
            .unwrap()
            .is::<ayeque_forge_core::Busy>()
    );
    let other = AuthoringGuard::acquire(&fixture.root, &fixture.root).unwrap();
    drop(other);
    drop(guard);
    AuthoringGuard::acquire(&fixture.root, &fixture.git.root).unwrap();
}

#[test]
fn source_writes_refuse_symlink_parent_and_special_leaf() {
    let fixture = Fixture::new();
    let root = source_root(&fixture.git.root).unwrap();
    std::os::unix::fs::symlink(&fixture.root, fixture.git.root.join("external")).unwrap();
    assert!(read_source(&root, "external/entry.md").is_err());
    let fifo = name(fixture.git.root.join("fifo").as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(
        read_source(&root, "fifo")
            .unwrap_err()
            .to_string()
            .contains("regular file")
    );
}

fn publisher_fixture(
    fixture: &mut Fixture,
) -> (MemoryPublisher, crate::store::StoreHandle, PathBuf) {
    let workspace = fixture.root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::remove_file(fixture.git.root.join("memory/entries/existing.md")).unwrap();
    fs::write(fixture.git.root.join("memory/.keep"), "").unwrap();
    fixture.git.text(&["add", "."]).unwrap();
    fixture
        .git
        .text(&["commit", "-qm", "empty corpus"])
        .unwrap();
    fixture.journal.source.revision = fixture.git.text(&["rev-parse", "HEAD"]).unwrap();
    let source = fixture.journal.source.clone();
    fs::write(
        workspace.join(agl_runtime::WORKSPACE_CONFIG_FILE_NAME),
        agl_runtime::serialize_workspace_forge_declarations(&workspace, vec![source]).unwrap(),
    )
    .unwrap();
    let data = fixture.root.join("data");
    let storage = fixture.root.join("forge");
    let store = crate::store::StoreHandle::open_at(data.join("store")).unwrap();
    let publisher = MemoryPublisher::new(
        data,
        storage,
        Arc::new(ayeque_forge_core::foreground),
        Duration::from_secs(30),
    )
    .unwrap();
    (publisher, store, workspace)
}

fn append_topic(store: &crate::store::StoreHandle, workspace: &Path) {
    let key = agl_core::agent::AgentOperationKey {
        run_id: AgentRunId::generate(),
        ordinal: std::num::NonZeroU32::new(1).unwrap(),
    };
    let topic = agl_core::agent::MemoryTopic {
        slug: "daemon-state".into(),
        claims: vec![agl_core::agent::MemoryClaim {
            text: "The daemon stores accepted pending claims.".into(),
            sources: vec![agl_core::MessageId::generate()],
            files: vec![],
            supersedes: None,
        }],
    };
    store
        .append_pending_memory(workspace, &key, &[topic])
        .unwrap();
}

#[test]
fn publication_retries_own_commit_after_sync_failure_then_updates_verified_copy() {
    let mut fixture = Fixture::new();
    let (mut publisher, store, workspace) = publisher_fixture(&mut fixture);
    append_topic(&store, &workspace);
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fault = failed.clone();
    publisher.runner = Arc::new(move |command| {
        if command.args.iter().any(|arg| arg == "clone")
            && !fault.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            anyhow::bail!("injected sync failure after authoring commit");
        }
        ayeque_forge_core::foreground(command)
    });
    let run_id = AgentRunId::generate();
    assert!(
        publisher
            .publish_at(&store, &workspace, run_id)
            .unwrap_err()
            .to_string()
            .contains("injected")
    );
    assert_eq!(store.pending_memory(&workspace).unwrap().len(), 1);
    fixture.git.clean().unwrap();
    let commit = fixture.git.text(&["rev-parse", "HEAD"]).unwrap();
    let declared = agl_runtime::read_workspace_forge_declarations_at(&workspace).unwrap();
    assert_eq!(declared[0].revision, commit);
    append_topic(&store, &workspace);
    publisher
        .publish_at(&store, &workspace, AgentRunId::generate())
        .unwrap();
    assert_eq!(fixture.git.text(&["rev-parse", "HEAD"]).unwrap(), commit);
    assert_eq!(store.pending_memory(&workspace).unwrap().len(), 1);
    let sources = agl_runtime::workspace_backend_declarations(&declared).unwrap();
    let reader =
        ayeque_forge_core::ForgeWorkspace::open(&publisher.storage, &workspace, &sources).unwrap();
    let (mut entry, guard, status) = reader
        .open_file("memory", Path::new("entries/daemon-state.md"))
        .unwrap();
    assert_eq!(status, ayeque_forge_core::VerificationStatus::Verified);
    let mut contents = String::new();
    entry.read_to_string(&mut contents).unwrap();
    assert!(contents.contains("The daemon stores accepted pending claims."));
    drop((entry, guard, reader));
    publisher
        .publish_at(&store, &workspace, AgentRunId::generate())
        .unwrap();
    assert!(store.pending_memory(&workspace).unwrap().is_empty());
    assert_eq!(fixture.git.text(&["rev-parse", "HEAD"]).unwrap(), commit);
    let mut mutation =
        ayeque_forge_core::ForgeMutation::begin(&publisher.storage, &workspace).unwrap();
    mutation.sync(&[]).unwrap();
}

#[test]
fn dirty_remote_and_same_repository_leave_claims_pending() {
    let mut fixture = Fixture::new();
    let (publisher, store, workspace) = publisher_fixture(&mut fixture);
    append_topic(&store, &workspace);
    fs::write(fixture.git.root.join("foreign"), "human edit").unwrap();
    let error = publisher
        .publish_at(&store, &workspace, AgentRunId::generate())
        .unwrap_err();
    assert!(error.to_string().contains("tracked or untracked"));
    assert_eq!(store.pending_memory(&workspace).unwrap().len(), 1);
    let mut sources = agl_runtime::read_workspace_forge_declarations_at(&workspace).unwrap();
    sources[0].git = "https://example.invalid/memory.git".into();
    fs::write(
        workspace.join(agl_runtime::WORKSPACE_CONFIG_FILE_NAME),
        agl_runtime::serialize_workspace_forge_declarations(&workspace, sources.clone()).unwrap(),
    )
    .unwrap();
    assert!(
        publisher
            .publish_at(&store, &workspace, AgentRunId::generate())
            .unwrap_err()
            .to_string()
            .contains("local authoring")
    );
    let same = fixture.git.root.clone();
    sources[0].git = same.to_str().unwrap().into();
    fs::write(
        same.join(agl_runtime::WORKSPACE_CONFIG_FILE_NAME),
        agl_runtime::serialize_workspace_forge_declarations(&same, sources).unwrap(),
    )
    .unwrap();
    append_topic(&store, &same);
    assert!(
        publisher
            .publish_at(&store, &same, AgentRunId::generate())
            .unwrap_err()
            .to_string()
            .contains("separate")
    );
    assert_eq!(store.pending_memory(&same).unwrap().len(), 1);
}

#[test]
fn source_commit_moves_an_entry_and_recovers_after_deletion() {
    let mut fixture = Fixture::new();
    fixture.journal.changes = vec![
        SourceChange {
            path: "memory/entries/existing.md".into(),
            before: Some(b"previous\n".to_vec()),
            after: None,
            mode: 0o644,
        },
        SourceChange {
            path: "memory/archive/existing.md".into(),
            before: None,
            after: Some(b"previous\n".to_vec()),
            mode: 0o644,
        },
    ];
    write_journal(&fixture.journal_path, &fixture.journal).unwrap();
    prepare_commit(&fixture.git, &fixture.journal_path, &mut fixture.journal).unwrap();
    fs::remove_file(fixture.git.root.join("memory/entries/existing.md")).unwrap();
    apply_source_commit(&fixture.git, &fixture.journal).unwrap();
    assert!(!fixture.git.root.join("memory/entries/existing.md").exists());
    assert_eq!(
        fs::read(fixture.git.root.join("memory/archive/existing.md")).unwrap(),
        b"previous\n"
    );
    fixture.git.clean().unwrap();
    let recovered: PublicationJournal =
        serde_json::from_slice(&fs::read(&fixture.journal_path).unwrap()).unwrap();
    apply_source_commit(&fixture.git, &recovered).unwrap();
    fixture.git.clean().unwrap();
}

#[test]
fn maintenance_without_claims_persists_stale_and_keeps_later_request() {
    let mut fixture = Fixture::new();
    let (publisher, store, workspace) = publisher_fixture(&mut fixture);
    let run = AgentRunId::generate();
    let message = agl_core::MessageId::generate();
    let entry = format!(
        "# Missing citation\n\n- slug: missing-citation\n- created: {run}\n- updated: {run}\n\n## Claims\n\n- A sourced fact. [src: {message}] [file: missing.rs@0123456789ab]\n"
    );
    fs::write(
        fixture.git.root.join("memory/entries/missing-citation.md"),
        &entry,
    )
    .unwrap();
    fixture.git.text(&["add", "."]).unwrap();
    fixture
        .git
        .text(&["commit", "-qm", "citation fixture"])
        .unwrap();
    let mut declarations = agl_runtime::read_workspace_forge_declarations_at(&workspace).unwrap();
    declarations[0].revision = fixture.git.text(&["rev-parse", "HEAD"]).unwrap();
    fs::write(
        workspace.join("agentLIBRE.toml"),
        agl_runtime::serialize_workspace_forge_declarations(&workspace, declarations).unwrap(),
    )
    .unwrap();
    store.request_memory_maintenance(&workspace).unwrap();
    let first = store.memory_maintenance(&workspace).unwrap().unwrap();
    store.request_memory_maintenance(&workspace).unwrap();
    store
        .complete_memory_maintenance(&workspace, &first)
        .unwrap();
    assert!(store.memory_maintenance(&workspace).unwrap().is_some());
    publisher
        .publish_at(&store, &workspace, AgentRunId::generate())
        .unwrap();
    assert!(store.memory_maintenance(&workspace).unwrap().is_none());
    let text =
        fs::read_to_string(fixture.git.root.join("memory/entries/missing-citation.md")).unwrap();
    assert_eq!(
        agl_core::agent::parse_entry(&text).unwrap().claims[0].status,
        Some(agl_core::agent::ClaimStatus::Stale)
    );
    let commit = fixture.git.text(&["rev-parse", "HEAD"]).unwrap();
    store.request_memory_maintenance(&workspace).unwrap();
    publisher
        .publish_at(&store, &workspace, AgentRunId::generate())
        .unwrap();
    assert_eq!(fixture.git.text(&["rev-parse", "HEAD"]).unwrap(), commit);
}

#[test]
fn archive_recovers_own_commit_and_refuses_a_different_request() {
    let mut fixture = Fixture::new();
    let (mut publisher, store, workspace) = publisher_fixture(&mut fixture);
    append_topic(&store, &workspace);
    publisher
        .publish_at(&store, &workspace, AgentRunId::generate())
        .unwrap();
    let original = fs::read(fixture.git.root.join("memory/entries/daemon-state.md")).unwrap();
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fault = failed.clone();
    publisher.runner = Arc::new(move |command| {
        if command.args.iter().any(|arg| arg == "fetch")
            && !fault.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            anyhow::bail!("injected archive sync failure");
        }
        ayeque_forge_core::foreground(command)
    });
    assert!(
        publisher
            .archive(
                &store,
                &workspace,
                "daemon-state",
                agl_core::RequestId::generate()
            )
            .is_err()
    );
    let commit = fixture.git.text(&["rev-parse", "HEAD"]).unwrap();
    assert_eq!(
        fixture.git.text(&["log", "-1", "--format=%s"]).unwrap(),
        "agl: archive memory daemon-state"
    );
    let busy = publisher
        .archive(&store, &workspace, "other", agl_core::RequestId::generate())
        .unwrap_err();
    assert!(busy.to_string().contains("Busy"));
    let revision = publisher
        .archive(
            &store,
            &workspace,
            "daemon-state",
            agl_core::RequestId::generate(),
        )
        .unwrap();
    assert_eq!(revision, commit);
    assert!(
        !fixture
            .git
            .root
            .join("memory/entries/daemon-state.md")
            .exists()
    );
    assert_eq!(
        fs::read(fixture.git.root.join("memory/archive/daemon-state.md")).unwrap(),
        original
    );
    assert!(
        !fs::read_to_string(fixture.git.root.join("memory/INDEX.md"))
            .unwrap()
            .contains("daemon-state")
    );
    assert_eq!(fixture.git.text(&["rev-parse", "HEAD"]).unwrap(), commit);
    fixture.git.clean().unwrap();
}
