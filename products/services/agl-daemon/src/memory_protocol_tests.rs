struct MemoryTokenRoute {
    model: agl_core::agent::ModelDefinitionRef,
    runtime: agl_core::agent::ModelRuntimeSelection,
    calls: std::sync::atomic::AtomicUsize,
}
impl agl_runtime::inference::InferenceRoute for MemoryTokenRoute {
    fn count_text_tokens(&self, request: agl_runtime::inference::InferenceTokenizeRequest) -> Result<u64, InferenceServiceError> {
        assert_eq!(request.model, self.model);
        assert_eq!(request.runtime, self.runtime);
        assert!(request.text.contains("Total claims: 2"));
        assert!(request.text.contains("[src:"));
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(42)
    }
    fn measure(&self, _:InferenceGenerateRequest) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError> {panic!("search cannot measure a model prompt")}
    fn generate(&self, _:InferenceGenerateRequest) -> Result<agl_core::agent::ModelGenerationResult, InferenceServiceError> {panic!("search cannot generate")}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_publication_search_and_archive_through_unix_protocol() {
    use agl_tool_api::ToolHandler as _;
    use std::io::Read as _;
    let root = root("memproto");
    std::fs::create_dir_all(root.join("repo/memory/entries")).unwrap();
    install_test_model(&root);
    let repo = root.join("repo");
    let git = |args: &[&str]| {
        let result = std::process::Command::new("git").args(args).current_dir(&repo).output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        String::from_utf8(result.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q"]); git(&["config", "user.name", "Memory Test"]); git(&["config", "user.email", "memory@example.invalid"]);
    std::fs::write(repo.join("memory/INDEX.md"), "# Memory\n").unwrap();
    git(&["add", "."]); git(&["commit", "-qm", "empty memory"]);
    let source = agl_runtime::ForgeDeclaration {id:"fixture.memory".into(),kind:"memory".into(),schema:"memory".into(),git:repo.to_str().unwrap().into(),revision:git(&["rev-parse", "HEAD"]),path:"memory".into()};
    std::fs::write(root.join("agentLIBRE.toml"), agl_runtime::serialize_workspace_forge_declarations(&root, vec![source]).unwrap()).unwrap();
    let storage = root.join("forge");
    let publisher = Arc::new(agent::memory_publication::MemoryPublisher::new(root.join("data"), storage.clone(), Arc::new(ayeque_forge_core::foreground), std::time::Duration::from_secs(30)).unwrap());
    let mut server = DaemonServer::start(DaemonConfig {
        data_root: root.join("data"), state_root:root.join("state"), catalog_root:root.join("source"),service_git:Default::default(),forge_sync_timeout_seconds:300,
        inference:InferenceConfig::custom(Arc::new(GatedGenerator {release:Arc::new(AtomicBool::new(true))})), extensions:vec![],searxng:None,max_resident_bytes:None,
    }).unwrap();
    server.handle.memory = publisher.clone();
    let store = server.handle.store.clone();
    let conversation_id = ConversationId::generate();
    store.create_conversation(conversation_id, &ExactPackageRef {id:PackageId::new("test-function").unwrap(),version:PackageVersion::new("1.0.0").unwrap(),digest:PackageDigest::from_bytes([9;32])}, &snapshot(&root)).unwrap();
    let socket = root.join("state/daemon/agl.sock");
    let task = tokio::spawn(server.serve(ListenerSource::Bind(socket.clone())));
    for _ in 0..200 { if socket.exists() {break;} tokio::time::sleep(std::time::Duration::from_millis(5)).await; }
    let client = AgentClient::new(&socket);
    let message_id = MessageId::generate();
    let run_id = client.start_run(AgentRunSpec {reasoning:None,origin:AgentRunOrigin::User {conversation_id,message_id:message_id.clone()},input:Content::text("finish").unwrap()}).await.unwrap();
    for _ in 0..200 {if store.agent_run(run_id).unwrap().status.is_terminal() {break;} tokio::time::sleep(std::time::Duration::from_millis(5)).await;}
    let before = store.agent_run(run_id).unwrap();
    assert!(before.status.is_terminal());
    let operation = agl_core::agent::AgentOperationKey {run_id, ordinal:std::num::NonZeroU32::MIN};
    store.append_pending_memory(&root, &operation, &[agl_core::agent::MemoryTopic {slug:"scheduler".into(),claims:vec![agl_core::agent::MemoryClaim {text:"The scheduler persists fixed timer ticks.".into(),sources:vec![message_id.clone()],files:vec![],supersedes:None}]}]).unwrap();
    publisher.after_run(&store, run_id);
    assert!(store.pending_memory(&root).unwrap().is_empty());
    assert_eq!(store.agent_run(run_id).unwrap().status, before.status);
    std::fs::write(repo.join("foreign-change"), "human change").unwrap();
    store.append_pending_memory(&root, &operation, &[agl_core::agent::MemoryTopic {slug:"other".into(),claims:vec![agl_core::agent::MemoryClaim {text:"Another pending fact.".into(),sources:vec![message_id.clone()],files:vec![],supersedes:None}]}]).unwrap();
    publisher.after_run(&store, run_id);
    assert_eq!(store.agent_run(run_id).unwrap().status, before.status);
    assert_eq!(store.pending_memory(&root).unwrap().len(),1);
    assert!(store.agent_event_page(None,1000).unwrap().events.iter().any(|event| matches!(&event.data, agl_core::agent::AgentEventData::MemoryPublicationDeferred {reason, ..} if reason.contains("Busy"))));
    std::fs::remove_file(repo.join("foreign-change")).unwrap();
    publisher.after_run(&store,run_id);
    assert!(store.pending_memory(&root).unwrap().is_empty());
    let index = Arc::new(agent::memory_index::MemoryIndex::new(root.join("data"),storage.clone()));
    let tools = crate::tools::memory::MemoryTools::new(index,store.clone());
    let route = Arc::new(MemoryTokenRoute {model:before.snapshot.model.model.clone(),runtime:before.snapshot.model.runtime.clone(),calls:std::sync::atomic::AtomicUsize::new(0)});
    tools.initialize(route.clone()).unwrap();
    let context = agl_tool_api::ToolContext::new(operation,Some(conversation_id),before.snapshot.workspace.clone(),AuthorityGrantSet(vec![]),i64::MAX,65536,agl_tool_api::ToolCancellation::new());
    let result = tools.call(context.clone(),serde_json::json!({"query":"scheduler","limit":5})).await.unwrap();
    assert!(result.content.as_text().contains("scheduler persists fixed timer ticks"));
    assert!(result.content.as_text().len() <= 600);
    assert_eq!(route.calls.load(Ordering::SeqCst),1);
    let response = client.memory_archive(root.to_str().unwrap(),"scheduler").await.unwrap();
    let revision = match response {agl_daemon_api::AgentResponse::MemoryArchived {slug, revision} => {assert_eq!(slug,"scheduler"); revision}, other=>panic!("{other:?}")};
    assert_eq!(git(&["rev-parse", "HEAD"]),revision);
    let sources = agl_runtime::read_workspace_forge_declarations_at(&root).unwrap();
    let reader = ayeque_forge_core::ForgeWorkspace::open(&storage,&root,&agl_runtime::workspace_backend_declarations(&sources).unwrap()).unwrap();
    assert!(reader.open_file("fixture.memory",Path::new("entries/scheduler.md")).is_err());
    let (mut file, guard, status) = reader.open_file("fixture.memory",Path::new("archive/scheduler.md")).unwrap();
    assert_eq!(status,ayeque_forge_core::VerificationStatus::Verified);
    let mut contents=String::new();file.read_to_string(&mut contents).unwrap();assert!(contents.contains("timer ticks"));
    drop(file); drop(guard); drop(reader);
    let fallback = tools.call(context,serde_json::json!({"query":"scheduler","limit":5})).await.unwrap();
    assert!(fallback.content.as_text().contains("No matching claims. forge:fixture.memory/INDEX.md"));
    assert!(!fallback.content.as_text().contains("fixed timer ticks"));
    let deferred = client.memory_archive(root.to_str().unwrap(),"absent").await.unwrap();
    assert!(matches!(deferred,agl_daemon_api::AgentResponse::MemoryArchiveDeferred {..}));
    assert!(client.memory_archive(root.to_str().unwrap(),"../escape").await.is_err());
    task.abort();let _=task.await;
    drop(tools);drop(publisher);drop(store);
    let _=std::fs::remove_dir_all(root);
}
