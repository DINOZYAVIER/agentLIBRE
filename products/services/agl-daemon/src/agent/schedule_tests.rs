struct ScheduleWritingGenerator {
    inner: FakeGenerator,
    file: std::path::PathBuf,
}

impl InferenceGenerator for ScheduleWritingGenerator {
        fn count_text_tokens(&self, _: agl_runtime::inference::InferenceTokenizeRequest) -> Result<u64, InferenceServiceError> { Err(InferenceServiceError::Unavailable) }

    fn measure(
        &self,
        request: InferenceGenerateRequest,
    ) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError> {
        self.inner.measure(request)
    }
    fn generate(
        &self,
        request: InferenceGenerateRequest,
    ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError> {
        if request
            .instructions
            .blocks
            .iter()
            .any(|b| b.content.as_text() == "scheduled task")
        {
            assert!(
                request
                    .context
                    .iter()
                    .all(|entry| entry.message.role != agl_core::agent::MessageRole::User)
            );
        }
        std::fs::write(&self.file, "self write during Run").unwrap();
        self.inner.generate(request)
    }
}

fn schedule_record(
    store: &StoreHandle,
    root: &std::path::Path,
    interval: u64,
) -> crate::store::ScheduleRecord {
    store.enable_schedule(&function_ref(), &snapshot(root), &agl_runtime::function::FunctionSchedule {
        interval_seconds: interval, max_run_seconds: 5, prompt: "scheduled task".into(), events: vec!["file:source.txt".into()],
    }, &serde_json::json!({"files": "none", "commands": [], "terminal": false, "search": false})).unwrap()
}

fn wait_schedule_run(store: &StoreHandle, run_id: AgentRunId) {
    for _ in 0..200 {
        if store.agent_run_view(run_id).unwrap().status.is_terminal() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        store.agent_run_view(run_id).unwrap().status,
        AgentRunStatus::Completed
    );
}

#[test]
fn schedule_file_events_do_not_loop_on_own_writes_or_fast_manual_runs() {
    let root =
        std::env::temp_dir().join(format!("agl-schedule-suppression-{}", uuid::Uuid::now_v7()));
    let store = StoreHandle::open_at(&root).unwrap();
    std::fs::write(root.join("source.txt"), "baseline").unwrap();
    let record = schedule_record(&store, &root, 60);
    let generator = Arc::new(ScheduleWritingGenerator {
        inner: FakeGenerator(AtomicUsize::new(0)),
        file: root.join("source.txt"),
    });
    let (inference_service, inference) = InferenceService::start(
        InferenceConfig::custom(generator.clone()),
        RestoredInferenceHealth::default(),
    )
    .unwrap();
    let (service, agent) = AgentService::start(
        AgentDependencies { memory: None, memory_index: None,
            store: store.clone(),
            inference: Arc::new(inference),
        },
        vec![],
    )
    .unwrap();
    let mut triggers = super::schedules::TriggerState::new(record.clone(), record.enabled_at_ms);
    triggers.poll(&store, &agent, record.enabled_at_ms).unwrap();
    std::fs::write(root.join("source.txt"), "external change").unwrap();
    triggers
        .poll(&store, &agent, record.enabled_at_ms + 1)
        .unwrap();
    let first = store.schedules(Some(&root)).unwrap()[0].last_run.unwrap();
    wait_schedule_run(&store, first);
    for time in 2..5 {
        triggers
            .poll(&store, &agent, record.enabled_at_ms + time)
            .unwrap();
    }
    assert_eq!(generator.inner.0.load(Ordering::SeqCst), 1);
    let manual_conversation = ConversationId::generate();
    bind_conversation(&store, manual_conversation, &snapshot(&root));
    let manual = agent
        .start_run(AgentRunSpec {
            reasoning: None,
            origin: AgentRunOrigin::User {
                conversation_id: manual_conversation,
                message_id: MessageId::generate(),
            },
            input: Content::text("manual task").unwrap(),
        })
        .unwrap();
    wait_schedule_run(&store, manual);
    triggers
        .poll(&store, &agent, record.enabled_at_ms + 5)
        .unwrap();
    triggers
        .poll(&store, &agent, record.enabled_at_ms + 6)
        .unwrap();
    assert_eq!(generator.inner.0.load(Ordering::SeqCst), 2);
    std::fs::write(root.join("source.txt"), "another external change").unwrap();
    triggers
        .poll(&store, &agent, record.enabled_at_ms + 7)
        .unwrap();
    let last = store.schedules(Some(&root)).unwrap()[0].last_run.unwrap();
    assert_ne!(first, last);
    wait_schedule_run(&store, last);
    assert_eq!(generator.inner.0.load(Ordering::SeqCst), 3);
    let messages = store
        .conversation_messages(record.conversation_id, None, 100)
        .unwrap()
        .messages;
    assert_eq!(messages.len(), 2);
    assert!(
        messages
            .iter()
            .all(|message| message.role == agl_core::agent::MessageRole::Assistant)
    );
    drop(triggers);
    service.shutdown();
    inference_service.shutdown();
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn schedule_restart_skips_elapsed_ticks_and_offline_file_changes_then_runs_next_tick() {
    let root = std::env::temp_dir().join(format!(
        "agl-schedule-restart-ticks-{}",
        uuid::Uuid::now_v7()
    ));
    let store = StoreHandle::open_at(&root).unwrap();
    std::fs::write(root.join("source.txt"), "before restart").unwrap();
    let record = schedule_record(&store, &root, 1);
    drop(store);
    std::fs::write(root.join("source.txt"), "during offline").unwrap();
    let store = StoreHandle::open_at(&root).unwrap();
    let generator = Arc::new(FakeGenerator(AtomicUsize::new(0)));
    let (inference_service, inference) = InferenceService::start(
        InferenceConfig::custom(generator.clone()),
        RestoredInferenceHealth::default(),
    )
    .unwrap();
    let (service, agent) = AgentService::start(
        AgentDependencies { memory: None, memory_index: None,
            store: store.clone(),
            inference: Arc::new(inference),
        },
        vec![],
    )
    .unwrap();
    let restored = store.schedules(Some(&root)).unwrap().remove(0);
    let mut triggers = super::schedules::TriggerState::new(restored, record.enabled_at_ms + 5500);
    triggers
        .poll(&store, &agent, record.enabled_at_ms + 5500)
        .unwrap();
    triggers
        .poll(&store, &agent, record.enabled_at_ms + 5999)
        .unwrap();
    assert!(store.schedules(Some(&root)).unwrap()[0].last_run.is_none());
    assert_eq!(generator.0.load(Ordering::SeqCst), 0);
    triggers
        .poll(&store, &agent, record.enabled_at_ms + 6000)
        .unwrap();
    let run = store.schedules(Some(&root)).unwrap()[0].last_run.unwrap();
    wait_schedule_run(&store, run);
    assert!(
        matches!(store.agent_run(run).unwrap().origin, AgentRunOrigin::Timer { tick_at_ms, .. } if tick_at_ms == record.enabled_at_ms + 6000)
    );
    assert_eq!(generator.0.load(Ordering::SeqCst), 1);
    drop(triggers);
    service.shutdown();
    inference_service.shutdown();
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
