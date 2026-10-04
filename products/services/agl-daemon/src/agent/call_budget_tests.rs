// Simulate a crash after durable dispatch admission, before any result commit.
use agl_core::agent::AgentOperationKey;
#[test]
fn interrupted_primary_and_correction_reservations_survive_store_reopen() {
    for correction in [false, true] {
        let root = std::env::temp_dir().join(format!("agl-call-budget-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation = ConversationId::generate();
        let mut configured = snapshot(&root);
        configured.limits.model_calls = if correction { 2 } else { 1 };
        configured.limits.correction_calls = 1;
        configured.response_format = Some(string_response_format());
        let mut corrector = configured.model.clone();
        corrector.model.id = PackageId::new("corrector").unwrap();
        configured.invalid_model_output_recovery =
            Some(agl_core::agent::InvalidModelOutputRecoverySelection {
                model: corrector,
                max_attempts: 2,
            });
        bind_conversation(&store, conversation, &configured);
        let spec = AgentRunSpec {
            reasoning: None,
            origin: AgentRunOrigin::User {
                conversation_id: conversation,
                message_id: MessageId::generate(),
            },
            input: Content::text("interrupted call").unwrap(),
        };
        let crate::store::AgentRunAdmission::Created(run) =
            store.admit_agent_run(&spec, configured.clone()).unwrap()
        else {
            panic!("expected new Run")
        };
        let fsm = AgentFsm::new(run.id, Some(conversation), configured.limits, 32, &[]);
        let driven = fsm
            .transition(
                &AgentFsmState {
                    status: run.status,
                    checkpoint: run.checkpoint,
                    usage: run.usage,
                },
                AgentFsmInput::Drive,
            )
            .unwrap();
        store
            .commit_agent_transition(run.id, run.revision, &driven.state, &driven.output)
            .unwrap();
        let operation = driven.output.operation.unwrap();
        let started = AgentOperationFsm::for_operation(&operation)
            .transition(
                &operation.state,
                AgentOperationFsmInput::Start {
                    key: operation.key.clone(),
                    delivery_attempt: operation.delivery_attempt,
                    request: operation.request.clone(),
                },
            )
            .unwrap();
        let running = operation
            .apply_fsm_transition(started.state, &started.output)
            .unwrap();
        store
            .commit_agent_operation_transition(&operation, &running, &started.output)
            .unwrap();
        let mut state = driven.state;
        for (index, correction_attempt) in (0..=u32::from(correction)).enumerate() {
            let reserved = fsm
                .transition(
                    &state,
                    AgentFsmInput::ModelCallStarted {
                        key: running.key.clone(),
                        delivery_attempt: running.delivery_attempt,
                        correction_attempt,
                    },
                )
                .unwrap();
            store
                .commit_agent_transition(
                    run.id,
                    run.revision + 1 + index as u64,
                    &reserved.state,
                    &reserved.output,
                )
                .unwrap();
            state = reserved.state;
        }
        drop(store);
        let store = StoreHandle::open_at(&root).unwrap();
        assert_eq!(store.agent_run_view(run.id).unwrap().usage.model_calls, 1);
        assert_eq!(
            store.agent_run_view(run.id).unwrap().usage.correction_calls,
            u64::from(correction)
        );
        let generator = Arc::new(OutputCorrectingGenerator(AtomicUsize::new(0)));
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(generator.clone()),
            RestoredInferenceHealth::default(),
        )
        .unwrap();
        let (service, _) = AgentService::start(
            AgentDependencies {
                store: store.clone(),
                inference: Arc::new(inference),
                memory: None,
                memory_index: None,
            },
            vec![],
        )
        .unwrap();
        for _ in 0..2_000 {
            if store.agent_run_view(run.id).unwrap().status.is_terminal() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }
        let view = store.agent_run_view(run.id).unwrap();
        assert_eq!(view.status, AgentRunStatus::Failed);
        assert_eq!(view.usage.model_calls, configured.limits.model_calls);
        assert_eq!(view.usage.correction_calls, u64::from(correction));
        assert_eq!(generator.0.load(Ordering::SeqCst), usize::from(correction));
        let events = store.agent_event_page(None, 100).unwrap().events;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event.data,
                    agl_core::agent::AgentEventData::ModelCallStarted { .. }
                ))
                .count(),
            configured.limits.model_calls as usize + usize::from(correction)
        );
        assert!(
            events
                .iter()
                .filter(|event| matches!(
                    event.data,
                    agl_core::agent::AgentEventData::ModelCallStarted { .. }
                ))
                .all(|event| event.operation.is_some() && event.operation_revision.is_some())
        );
        service.shutdown();
        inference_service.shutdown();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn transport_retries_spend_the_call_cap_before_a_successful_result() {
    let root = std::env::temp_dir().join(format!("agl-call-cap-{}", uuid::Uuid::now_v7()));
    let store = StoreHandle::open_at(&root).unwrap();
    let conversation = ConversationId::generate();
    let mut configured = snapshot(&root);
    configured.limits.model_calls = 1;
    bind_conversation(&store, conversation, &configured);
    let generator = Arc::new(FlakyGenerator(AtomicUsize::new(0)));
    let (inference_service, inference) = InferenceService::start(
        InferenceConfig::custom(generator.clone()),
        RestoredInferenceHealth::default(),
    )
    .unwrap();
    let (service, handle) = AgentService::start(
        AgentDependencies {
            store: store.clone(),
            inference: Arc::new(inference),
            memory: None,
            memory_index: None,
        },
        vec![],
    )
    .unwrap();
    let run_id = handle
        .start_run(AgentRunSpec {
            reasoning: None,
            origin: AgentRunOrigin::User {
                conversation_id: conversation,
                message_id: MessageId::generate(),
            },
            input: Content::text("bounded retries").unwrap(),
        })
        .unwrap();
    for _ in 0..2_000 {
        if store.agent_run_view(run_id).unwrap().status.is_terminal() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(1));
    }
    let view = store.agent_run_view(run_id).unwrap();
    assert_eq!(view.status, AgentRunStatus::Failed);
    assert_eq!(view.usage.model_calls, 1);
    assert_eq!(view.usage.model_input_tokens, 0);
    assert_eq!(generator.0.load(Ordering::SeqCst), 1);
    let operation = store
        .agent_operation(&AgentOperationKey {
            run_id,
            ordinal: NonZeroU32::MIN,
        })
        .unwrap();
    assert_eq!(
        operation.failure.unwrap().kind,
        AgentOperationFailureKind::LimitsExceeded
    );
    service.shutdown();
    inference_service.shutdown();
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn blocked_publication_does_not_hold_any_active_run_slots() {
    let root = std::env::temp_dir().join(format!("agl-publication-slots-{}", uuid::Uuid::now_v7()));
    let store = StoreHandle::open_at(&root).unwrap();
    let (inference_service, inference) = InferenceService::start(
        InferenceConfig::custom(Arc::new(FakeGenerator(AtomicUsize::new(0)))),
        RestoredInferenceHealth::default(),
    )
    .unwrap();
    let (entered, observed) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let (publication, queue) = super::memory_worker::PublicationWorker::start(move |run_id| {
        entered.send(run_id).unwrap();
        blocked.recv().unwrap();
    })
    .unwrap();
    let (sender, receiver) = mpsc::sync_channel(SCHEDULER_QUEUE_CAPACITY);
    let progress = Arc::new(Mutex::new(Vec::new()));
    let scheduler = sender.clone();
    let dependencies = AgentDependencies {
        store: store.clone(),
        inference: Arc::new(inference),
        memory: None,
        memory_index: None,
    };
    let worker_progress = progress.clone();
    let worker = thread::spawn(move || {
        worker_loop(
            receiver,
            scheduler,
            dependencies,
            BTreeMap::new(),
            worker_progress,
            vec![],
            Some(queue),
        )
    });
    let mut runs = Vec::new();
    // More completed Runs than the whole active capacity while publication is blocked.
    for _ in 0..=MAX_ACTIVE_RUNS {
        let conversation = ConversationId::generate();
        bind_conversation(&store, conversation, &snapshot(&root));
        let (reply, result) = mpsc::sync_channel(1);
        sender
            .send(Command::StartRun {
                schedule_generation: None,
                reply,
                spec: AgentRunSpec {
                    reasoning: None,
                    origin: AgentRunOrigin::User {
                        conversation_id: conversation,
                        message_id: MessageId::generate(),
                    },
                    input: Content::text("finish independently of publication").unwrap(),
                },
            })
            .unwrap();
        runs.push(result.recv().unwrap().unwrap());
    }
    observed
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    for _ in 0..2_000 {
        if runs
            .iter()
            .all(|id| store.agent_run_view(*id).unwrap().status.is_terminal())
        {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        runs.iter()
            .all(|id| store.agent_run_view(*id).unwrap().status == AgentRunStatus::Completed)
    );
    publication.stop_accepting();
    sender.send(Command::Shutdown).unwrap();
    worker.join().unwrap();
    release.send(()).unwrap();
    drop(publication);
    assert!(observed.try_recv().is_err());
    inference_service.shutdown();
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
