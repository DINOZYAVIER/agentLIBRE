use super::*;

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    repo: PathBuf,
    storage: PathBuf,
    index: MemoryIndex,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "agl-memory-index-{}",
            agl_core::RequestId::generate()
        ));
        let workspace = root.join("workspace");
        let repo = root.join("repo");
        let storage = root.join("forge");
        fs::create_dir_all(repo.join("memory/entries")).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let index = MemoryIndex::new(root.join("data"), storage.clone());
        let fixture = Self {
            root,
            workspace,
            repo,
            storage,
            index,
        };
        fixture.git(&["init", "-q"]);
        fixture.git(&["config", "user.name", "Index Test"]);
        fixture.git(&["config", "user.email", "index@example.invalid"]);
        fs::write(fixture.repo.join("memory/INDEX.md"), "# Memory index\n").unwrap();
        fixture.entry("scheduler", "HTTPServer scheduler uses fixed timer ticks. [file: missing.rs@0123456789ab] [superseded-by: absent#claim-1]");
        fixture.entry("runtime", "HTTPServer runtime shares timer scheduling.");
        fs::write(
            fixture.repo.join("memory/entries/broken.md"),
            "invalid entry",
        )
        .unwrap();
        fixture.sync();
        fixture
    }
    fn git(&self, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&self.repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    fn entry(&self, slug: &str, claim: &str) {
        let run = AgentRunId::generate();
        let message = agl_core::MessageId::generate();
        let text = format!(
            "# {slug}\n\n- slug: {slug}\n- created: {run}\n- updated: {run}\n\n## Claims\n\n- {claim} [src: {message}]\n"
        );
        parse_entry(&text).unwrap();
        fs::write(self.repo.join(format!("memory/entries/{slug}.md")), text).unwrap();
    }
    fn sync(&self) {
        self.git(&["add", "."]);
        self.git(&["commit", "-qm", "index fixture"]);
        let declaration = agl_runtime::ForgeDeclaration {
            id: "project.memory".into(),
            kind: "memory".into(),
            schema: "memory".into(),
            git: self.repo.to_str().unwrap().into(),
            revision: self.git(&["rev-parse", "HEAD"]),
            path: "memory".into(),
        };
        let mut mutation =
            ayeque_forge_core::ForgeMutation::begin(&self.storage, &self.workspace).unwrap();
        fs::write(
            self.workspace.join("agentLIBRE.toml"),
            agl_runtime::serialize_workspace_forge_declarations(
                &self.workspace,
                vec![declaration.clone()],
            )
            .unwrap(),
        )
        .unwrap();
        mutation
            .sync(&agl_runtime::workspace_backend_declarations(&[declaration]).unwrap())
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn real_fts_rebuild_query_corruption_and_source_binding() {
    let fixture = Fixture::new();
    let run = AgentRunId::generate();
    assert_eq!(
        fixture
            .index
            .memory_source_id(&fixture.workspace)
            .as_deref(),
        Some("project.memory")
    );
    let page = fixture
        .index
        .search(&fixture.workspace, run, "HTTPServer", 10)
        .unwrap();
    assert_eq!(page.hits.len(), 2);
    assert_eq!(page.total, 2);
    assert_eq!(page.skipped, ["entries/broken.md"]);
    let scheduler = page
        .hits
        .iter()
        .find(|hit| hit.slug == "scheduler")
        .unwrap();
    assert!(scheduler.stale);
    assert!(!scheduler.superseded);
    assert_eq!(scheduler.supersession.as_deref(), Some("absent#claim-1"));
    assert!(scheduler.sources.contains("file:missing.rs@0123456789ab"));
    assert!(page.unchecked.is_empty());
    let db = index_db_path(&fixture.index.data, &fixture.workspace);
    let connection = Connection::open(&db).unwrap();
    let line: usize = connection
        .query_row(
            "SELECT line_no FROM claims WHERE slug='scheduler'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(line, 9);
    drop(connection);
    let fallback = fixture
        .index
        .search(&fixture.workspace, run, "***", 10)
        .unwrap();
    assert!(fallback.hits.is_empty());
    assert_eq!(fallback.fallback, "# Memory index\n");
    assert_eq!(fallback.index_path, "forge:project.memory/INDEX.md");
    fs::write(&db, "corrupt sqlite").unwrap();
    assert_eq!(
        fixture
            .index
            .search(&fixture.workspace, run, "HTTPServer", 10)
            .unwrap()
            .hits
            .len(),
        2
    );
    let corpus =
        super::super::memory_io::MemoryWorkspace::new_forge(&fixture.workspace, &fixture.storage)
            .unwrap()
            .load_corpus();
    assert!(
        corpus.is_err(),
        "malformed authoring entries prevent compaction publication"
    );
    let source = MemorySource::open(&fixture.storage, &fixture.workspace).unwrap();
    assert!(ayeque_forge_core::ForgeMutation::begin(&fixture.storage, &fixture.workspace).is_err());
    drop(source);
    fixture.entry("additional", "HTTPServer new source binding.");
    fixture.sync();
    assert_eq!(
        fixture
            .index
            .search(&fixture.workspace, run, "HTTPServer", 10)
            .unwrap()
            .hits
            .len(),
        3
    );
    fixture.index.run_finished(run);
    assert!(
        !fixture
            .index
            .workspaces
            .lock()
            .unwrap()
            .get(&fixture.workspace)
            .unwrap()
            .lock()
            .unwrap()
            .searched_runs
            .contains(&run)
    );
}
