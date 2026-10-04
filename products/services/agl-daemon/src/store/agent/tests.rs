use std::sync::{Arc, Barrier};

use agl_core::Content;
use agl_core::Fsm;
use agl_core::agent::{
    AbsolutePath, AgentDefinitionRef, AgentFsm, AgentFsmInput, AgentFsmState, AgentOperationFsm,
    AgentOperationFsmInput, AgentOperationResult, AgentRunLimits, AgentRunOrigin,
    AuthorityGrantSet, InferenceRealizationRef, InstructionSet, ModelDefinitionRef,
    ModelFinishReason, ModelGenerationOutput, ModelGenerationResult, ModelSelection, ModelUsage,
    PackageDigest, RelativePath, WorkspaceScope,
};
use agl_core::{ConversationId, MessageId};
use agl_runtime::package::{PackageId, PackageVersion};

use super::*;

fn model_runtime() -> agl_core::agent::ModelRuntimeSelection {
    use agl_core::agent::{
        GenerationSettings, GpuLayerSelection, KvCacheType, ModelArtifactKind, ModelArtifactRef,
        ModelLoadSelection, ModelRuntimeSelection, ModelServiceSelection, SplitMode,
    };
    ModelRuntimeSelection {
        mmproj: None,
        artifact: ModelArtifactRef {
            kind: ModelArtifactKind::Gguf,
            url: "https://example.invalid/model.gguf".into(),
            digest: PackageDigest::from_bytes([3; 32]),
            bytes: 4,
        },
        dialect: agl_core::agent::ModelDialect::Generic,
        tool_call_format: agl_core::agent::ToolCallFormat::HermesJson,
        generation: GenerationSettings {
            max_output_tokens: 32,
            seed: 1,
            temperature: 0.0,
            top_k: 1,
            top_p: 1.0,
            min_p: 0.0,
            typical_p: 1.0,
            repeat_last_n: 64,
            repeat_penalty: 1.0,
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            stop: vec![],
        },
        reasoning: agl_core::agent::ReasoningSelection::Disabled,
        speculative: None,
        load: ModelLoadSelection {
            context_tokens: 128,
            batch_size: 8,
            ubatch_size: 8,
            threads: 1,
            threads_batch: 1,
            gpu_layers: GpuLayerSelection::Count(0),
            devices: vec![],
            split_mode: SplitMode::None,
            main_gpu: None,
            tensor_split: vec![],
            mmap: true,
            mlock: false,
            flash_attention: false,
            kv_cache_type_k: KvCacheType::F16,
            kv_cache_type_v: KvCacheType::F16,
            engine_build_digest: PackageDigest::from_bytes([4; 32]),
        },
        adapters: vec![],
        service: ModelServiceSelection {
            key: PackageDigest::from_bytes([5; 32]),
            slots: 1,
            queue_capacity: 32,
            continuous_batching: true,
            idle_timeout_ms: 900_000,
        },
    }
}

fn snapshot(root: &std::path::Path) -> AgentRunSnapshot {
    let package = || AgentDefinitionRef {
        id: PackageId::new("test-agent").unwrap(),
        version: PackageVersion::new("1.0.0").unwrap(),
        digest: PackageDigest::from_bytes([1; 32]),
    };
    AgentRunSnapshot {
        presentation: Default::default(),
        agent: package(),
        model: ModelSelection {
            reasoning_efforts: vec![],
            model: ModelDefinitionRef {
                id: PackageId::new("test-model").unwrap(),
                version: PackageVersion::new("1.0.0").unwrap(),
                digest: PackageDigest::from_bytes([2; 32]),
            },
            runtime: model_runtime(),
        },
        invalid_model_output_recovery: None,
        instructions: InstructionSet::new(vec![]).unwrap(),
        workspace: WorkspaceScope {
            root: AbsolutePath::try_from(root.to_string_lossy().into_owned()).unwrap(),
            working_directory: RelativePath::try_from(".".to_owned()).unwrap(),
        },
        tools: vec![],
        authority: AuthorityGrantSet::default(),
        limits: AgentRunLimits {
            deadline_ms: 10_000,
            model_input_tokens: Some(100),
            model_output_tokens: 100,
            model_calls: 2,
            correction_input_tokens: 100,
            correction_output_tokens: 100,
            correction_calls: 2,
            tool_calls: 2,
            tool_result_bytes: 65_536,
        },
        response_format: None,
        planner_read_only: false,
    }
}

fn function_ref() -> ExactPackageRef {
    ExactPackageRef {
        id: PackageId::new("test-function").unwrap(),
        version: PackageVersion::new("1.0.0").unwrap(),
        digest: PackageDigest::from_bytes([9; 32]),
    }
}

fn bind_conversation(store: &StoreHandle, root: &std::path::Path, conversation_id: ConversationId) {
    store
        .create_conversation(conversation_id, &function_ref(), &snapshot(root))
        .unwrap();
}

fn concurrently_admit(
    store: &StoreHandle,
    root: &std::path::Path,
    spec: &AgentRunSpec,
    admitted_snapshot: &AgentRunSnapshot,
) -> Vec<AgentRunId> {
    let barrier = Arc::new(Barrier::new(8));
    let mut workers = Vec::new();
    for _ in 0..8 {
        let store = store.clone();
        let root = root.to_path_buf();
        let spec = spec.clone();
        let admitted_snapshot = admitted_snapshot.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            assert!(root.is_absolute());
            barrier.wait();
            match store.admit_agent_run(&spec, admitted_snapshot).unwrap() {
                AgentRunAdmission::Created(run) | AgentRunAdmission::Replayed(run) => run.id,
            }
        }));
    }
    workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect()
}

#[test]
fn conversation_binding_is_immutable_and_workspace_queries_are_exact() {
    let root =
        std::env::temp_dir().join(format!("agl-conversation-binding-{}", uuid::Uuid::now_v7()));
    let first_workspace = root.join("first");
    let second_workspace = root.join("second");
    std::fs::create_dir_all(&first_workspace).unwrap();
    std::fs::create_dir_all(&second_workspace).unwrap();
    let store = StoreHandle::open_at(root.join("store")).unwrap();
    let first_id = ConversationId::generate();
    let second_id = ConversationId::generate();
    let first_snapshot = snapshot(&first_workspace);
    store
        .create_conversation(first_id, &function_ref(), &first_snapshot)
        .unwrap();
    store
        .create_conversation(second_id, &function_ref(), &snapshot(&second_workspace))
        .unwrap();

    let mut changed = first_snapshot.clone();
    changed.model.model.digest = PackageDigest::from_bytes([42; 32]);
    assert!(
        store
            .create_conversation(first_id, &function_ref(), &changed)
            .is_err()
    );
    assert_eq!(
        store.conversation_binding(first_id).unwrap().snapshot,
        first_snapshot
    );
    assert_eq!(
        store.conversations(Some(&first_workspace), 10).unwrap(),
        vec![store.conversation_binding(first_id).unwrap().view]
    );
    assert_eq!(store.conversations(None, 10).unwrap().len(), 2);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn conversation_names_are_trimmed_unique_and_resolve_exactly() {
    let root =
        std::env::temp_dir().join(format!("agl-conversation-names-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&root).unwrap();
    let store = StoreHandle::open_at(root.join("store")).unwrap();
    let first_id = ConversationId::generate();
    let second_id = ConversationId::generate();
    bind_conversation(&store, &root, first_id);
    bind_conversation(&store, &root, second_id);

    let renamed = store
        .rename_conversation(&first_id.to_string(), "  Coder  ")
        .unwrap();
    assert_eq!(renamed.display_name.as_deref(), Some("Coder"));
    assert_eq!(store.resolve_conversation("Coder").unwrap().id, first_id);
    assert_eq!(
        store
            .resolve_conversation(&second_id.to_string())
            .unwrap()
            .id,
        second_id
    );
    assert!(
        store
            .rename_conversation(&second_id.to_string(), "Coder")
            .is_err()
    );
    assert!(
        store
            .rename_conversation(&second_id.to_string(), &first_id.to_string())
            .is_err()
    );
    assert_eq!(
        store
            .conversation_binding(second_id)
            .unwrap()
            .view
            .display_name,
        None
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn agent_admission_is_atomic_replay_safe_and_uses_blob_identity() {
    let root = std::env::temp_dir().join(format!("agl-daemon-store-{}", uuid::Uuid::now_v7()));
    let store = StoreHandle::open_at(&root).unwrap();
    let spec = AgentRunSpec {
        reasoning: None,
        origin: AgentRunOrigin::User {
            conversation_id: ConversationId::generate(),
            message_id: MessageId::generate(),
        },
        input: Content::text("hello").unwrap(),
    };
    let conversation_id = &spec.origin.conversation_id();
    bind_conversation(&store, &root, *conversation_id);
    let created = store.admit_agent_run(&spec, snapshot(&root)).unwrap();
    let AgentRunAdmission::Created(created) = created else {
        panic!("first admission must create")
    };
    let replayed = store.admit_agent_run(&spec, snapshot(&root)).unwrap();
    let AgentRunAdmission::Replayed(replayed) = replayed else {
        panic!("second admission must replay")
    };
    assert_eq!(created.id, replayed.id);
    let storage = store.lock().unwrap();
    let (kind, length): (String, i64) = storage
        .connection()
        .query_row(
            "SELECT typeof(agent_run_id), length(agent_run_id) FROM agent_runs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((kind.as_str(), length), ("blob", 16));
    let digest_shape: (String, i64, String, i64, String, i64) = storage
        .connection()
        .query_row(
            "SELECT typeof(agent_package_digest), length(agent_package_digest),
                        typeof(model_package_digest), length(model_package_digest),
                        typeof(instruction_digest), length(instruction_digest)
                 FROM agent_runs",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        digest_shape,
        ("blob".into(), 32, "blob".into(), 32, "blob".into(), 32)
    );
    drop(storage);

    let fsm = AgentFsm::new(
        created.id,
        Some(created.origin.conversation_id()),
        created.snapshot.limits,
        created.snapshot.model.runtime.generation.max_output_tokens,
        &created.snapshot.tools,
    );
    let transition = fsm
        .transition(
            &AgentFsmState {
                status: created.status,
                checkpoint: created.checkpoint.clone(),
                usage: created.usage,
            },
            AgentFsmInput::Drive,
        )
        .unwrap();
    let events = store
        .commit_agent_transition(created.id, 0, &transition.state, &transition.output)
        .unwrap();
    assert!(!events.is_empty());
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].id.0 + 1 == pair[1].id.0)
    );
    assert!(
        store
            .commit_agent_transition(created.id, 0, &transition.state, &transition.output)
            .is_err()
    );
    let operation_count: i64 = store
        .lock()
        .unwrap()
        .connection()
        .query_row("SELECT count(*) FROM agent_operations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(operation_count, 1);
    let first_page = store.agent_event_page(None, 1).unwrap();
    assert_eq!(first_page.events.len(), 1);
    assert_eq!(first_page.events[0].agent_run_id, created.id);
    assert!(matches!(
        first_page.events[0].data,
        AgentEventData::RunAdmitted { .. }
    ));
    let second_page = store.agent_event_page(first_page.next_cursor, 100).unwrap();
    assert!(!second_page.events.is_empty());
    assert!(
        second_page
            .events
            .iter()
            .all(|event| event.agent_run_id == created.id)
    );

    let conflict = AgentRunSpec {
        reasoning: None,
        origin: spec.origin,
        input: Content::text("changed").unwrap(),
    };
    assert!(store.admit_agent_run(&conflict, snapshot(&root)).is_err());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn accepted_content_is_stored_once_and_reconstructed_for_the_operation() {
    let root = std::env::temp_dir().join(format!("agl-daemon-content-{}", uuid::Uuid::now_v7()));
    let store = StoreHandle::open_at(&root).unwrap();
    let conversation_id = ConversationId::generate();
    let spec = AgentRunSpec {
        reasoning: None,
        origin: AgentRunOrigin::User {
            conversation_id,
            message_id: MessageId::generate(),
        },
        input: Content::text("input").unwrap(),
    };
    bind_conversation(&store, &root, conversation_id);
    let AgentRunAdmission::Created(run) = store.admit_agent_run(&spec, snapshot(&root)).unwrap()
    else {
        panic!("first admission must create")
    };
    let fsm = AgentFsm::new(
        run.id,
        Some(run.origin.conversation_id()),
        run.snapshot.limits,
        run.snapshot.model.runtime.generation.max_output_tokens,
        &run.snapshot.tools,
    );
    let initial = AgentFsmState {
        status: run.status,
        checkpoint: run.checkpoint,
        usage: run.usage,
    };
    let driven = fsm.transition(&initial, AgentFsmInput::Drive).unwrap();
    store
        .commit_agent_transition(run.id, 0, &driven.state, &driven.output)
        .unwrap();
    let operation = driven.output.operation.unwrap();
    let machine = AgentOperationFsm::for_operation(&operation);
    let started = machine
        .transition(
            &operation.state,
            AgentOperationFsmInput::Start {
                key: operation.key.clone(),
                delivery_attempt: operation.delivery_attempt,
                request: operation.request.clone(),
            },
        )
        .unwrap();
    let started_operation = operation
        .apply_fsm_transition(started.state, &started.output)
        .unwrap();
    store
        .commit_agent_operation_transition(&operation, &started_operation, &started.output)
        .unwrap();
    let content = Content::text("unique-result-content").unwrap();
    let private_reasoning = Content::text("durable-private-reasoning").unwrap();
    let finished = machine
        .transition(
            &started_operation.state,
            AgentOperationFsmInput::Result {
                key: started_operation.key.clone(),
                delivery_attempt: started_operation.delivery_attempt,
                result: AgentOperationResult::ModelGeneration(ModelGenerationResult {
                    private_reasoning: Some(private_reasoning.clone()),
                    output: ModelGenerationOutput::Assistant(content.clone()),
                    finish_reason: ModelFinishReason::Stop,
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    realization: InferenceRealizationRef {
                        runtime_profile_digest:
                            agl_core::agent::InferenceRuntimeProfileDigest::from_bytes([1; 32]),
                        engine_build_digest:
                            agl_core::agent::InferenceEngineBuildDigest::from_bytes([2; 32]),
                        physical_resource_digest:
                            agl_core::agent::PhysicalResourceDigest::from_bytes([3; 32]),
                    },
                    correction: None,
                }),
            },
        )
        .unwrap();
    let finished_operation = started_operation
        .apply_fsm_transition(finished.state, &finished.output)
        .unwrap();
    let message_id = MessageId::generate();
    let resumed = fsm
        .transition(
            &driven.state,
            AgentFsmInput::OperationFinished {
                operation: finished_operation.clone(),
                message_id: Some(message_id.clone()),
            },
        )
        .unwrap();
    store
        .commit_agent_cycle(
            &started_operation,
            &finished_operation,
            &finished.output,
            1,
            &resumed.state,
            &resumed.output,
        )
        .unwrap();

    let reconstructed = store.agent_operation(&operation.key).unwrap();
    assert_eq!(reconstructed.result, finished_operation.result);
    let storage = store.lock().unwrap();
    let (result_json, message_copies): (String, u32) = storage
            .connection()
            .query_row(
                "SELECT o.result_json,
                        (SELECT count(*) FROM agent_messages WHERE content_json LIKE '%unique-result-content%')
                 FROM agent_operations o WHERE agent_run_id=?1 AND ordinal=?2",
                params![run.id.as_bytes().as_slice(), operation.key.ordinal.get()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
    assert!(!result_json.contains("unique-result-content"));
    assert!(result_json.contains("durable-private-reasoning"));
    assert_eq!(message_copies, 1);
    drop(storage);
    assert_eq!(
        store
            .conversation_messages(conversation_id, None, 10)
            .unwrap()
            .messages
            .len(),
        2
    );
    let second_input_id = MessageId::generate();
    let second_spec = AgentRunSpec {
        reasoning: None,
        origin: AgentRunOrigin::User {
            conversation_id,
            message_id: second_input_id.clone(),
        },
        input: Content::text("follow-up").unwrap(),
    };
    let AgentRunAdmission::Created(second) = store
        .admit_agent_run(&second_spec, snapshot(&root))
        .unwrap()
    else {
        panic!("second admission must create")
    };
    let AgentCheckpoint::Ready { context, .. } = second.checkpoint else {
        panic!("new Run must start ready")
    };
    let first_input_id = spec.origin.user_message_id().unwrap();
    assert_eq!(
        context,
        [
            first_input_id.clone(),
            message_id.clone(),
            second_input_id.clone()
        ]
    );
    let entries = store.agent_context(&context).unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.message.role)
            .collect::<Vec<_>>(),
        [MessageRole::User, MessageRole::Assistant, MessageRole::User]
    );
    assert_eq!(entries[1].private_reasoning, Some(private_reasoning));
    assert!(
        store
            .conversation_messages(conversation_id, None, 10)
            .unwrap()
            .messages
            .iter()
            .all(|message| !message
                .content
                .as_text()
                .contains("durable-private-reasoning"))
    );
    store
        .lock()
        .unwrap()
        .connection()
        .execute(
            "UPDATE agent_operations SET request_json=?3
                 WHERE agent_run_id=?1 AND ordinal=?2",
            params![
                operation.key.run_id.as_bytes().as_slice(),
                operation.key.ordinal.get(),
                strict_json(&AgentOperationRequest::Tool(agl_core::agent::ToolRequest {
                    tool_id: agl_core::ToolId::new("test.extension:forged").unwrap(),
                    input: serde_json::json!({}),
                },))
                .unwrap(),
            ],
        )
        .unwrap();
    assert!(
        store
            .conversation_messages(conversation_id, None, 10)
            .is_err()
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn reasoning_override_is_run_local_validated_and_immutable_on_replay() {
    use agl_core::agent::{ReasoningEffort, ReasoningSelection};

    let root = std::env::temp_dir().join(format!("agl-run-reasoning-{}", uuid::Uuid::now_v7()));
    let store = StoreHandle::open_at(&root).unwrap();
    let conversation_id = ConversationId::generate();
    let mut configured = snapshot(&root);
    configured.model.reasoning_efforts = vec![ReasoningEffort::Low, ReasoningEffort::Xhigh];
    configured.model.runtime.reasoning = ReasoningSelection::Enabled {
        max_tokens: 16,
        effort: Some(ReasoningEffort::Xhigh),
        preserve: true,
    };
    store
        .create_conversation(conversation_id, &function_ref(), &configured)
        .unwrap();
    let mut spec = AgentRunSpec {
        origin: AgentRunOrigin::User {
            conversation_id,
            message_id: MessageId::generate(),
        },
        input: Content::text("per-Run reasoning").unwrap(),
        reasoning: Some(ReasoningEffort::Medium),
    };
    assert!(store.admit_agent_run(&spec, configured.clone()).is_err());
    assert!(
        store
            .conversation_messages(conversation_id, None, 100)
            .unwrap()
            .messages
            .is_empty()
    );
    spec.reasoning = Some(ReasoningEffort::Low);
    let AgentRunAdmission::Created(run) = store.admit_agent_run(&spec, configured.clone()).unwrap()
    else {
        panic!("expected creation")
    };
    assert_eq!(
        run.snapshot.model.runtime.reasoning,
        ReasoningSelection::Enabled {
            max_tokens: 16,
            effort: Some(ReasoningEffort::Low),
            preserve: true,
        }
    );
    assert!(
        run.snapshot
            .model
            .runtime
            .shares_service_with(&configured.model.runtime)
    );
    assert_eq!(
        store
            .conversation_binding(conversation_id)
            .unwrap()
            .snapshot,
        configured
    );
    assert_eq!(
        store.conversations(None, 100).unwrap()[0].reasoning,
        configured.model.runtime.reasoning
    );
    spec.reasoning = Some(ReasoningEffort::Medium);
    drop(store);
    let store = StoreHandle::open_at(&root).unwrap();
    let AgentRunAdmission::Replayed(replay) = store.admit_agent_run(&spec, configured).unwrap()
    else {
        panic!("expected replay")
    };
    assert_eq!(replay.snapshot, run.snapshot);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn absent_aggregate_input_limit_round_trips_as_null_not_a_numeric_sentinel() {
    let root = std::env::temp_dir().join(format!("agl-unlimited-input-{}", uuid::Uuid::now_v7()));
    let store = StoreHandle::open_at(&root).unwrap();
    let conversation_id = ConversationId::generate();
    let mut configured = snapshot(&root);
    configured.limits.model_input_tokens = None;
    store
        .create_conversation(conversation_id, &function_ref(), &configured)
        .unwrap();
    let spec = AgentRunSpec {
        reasoning: None,
        origin: AgentRunOrigin::User {
            conversation_id,
            message_id: MessageId::generate(),
        },
        input: Content::text("unlimited aggregate input").unwrap(),
    };
    let AgentRunAdmission::Created(run) = store.admit_agent_run(&spec, configured).unwrap() else {
        panic!("expected creation")
    };
    let stored: Option<u64> = store
        .read(|connection| {
            Ok(connection.query_row(
                "SELECT limit_model_input_tokens FROM agent_runs WHERE agent_run_id=?1",
                [run.id.as_bytes().as_slice()],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(stored, None);
    drop(store);
    let restored = StoreHandle::open_at(&root).unwrap();
    assert_eq!(
        restored
            .replay_agent_run(&spec)
            .unwrap()
            .unwrap()
            .snapshot
            .limits
            .model_input_tokens,
        None
    );
    drop(restored);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn independent_conversation_admission_is_exclusive_but_replay_is_allowed() {
    let root = std::env::temp_dir().join(format!("agl-conversation-busy-{}", uuid::Uuid::now_v7()));
    let store = StoreHandle::open_at(&root).unwrap();
    let conversation_id = ConversationId::generate();
    bind_conversation(&store, &root, conversation_id);
    let barrier = Arc::new(Barrier::new(8));
    let mut workers = Vec::new();
    for _ in 0..8 {
        let store = store.clone();
        let root = root.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            let spec = AgentRunSpec {
                reasoning: None,
                origin: AgentRunOrigin::User {
                    conversation_id,
                    message_id: MessageId::generate(),
                },
                input: Content::text("independent input").unwrap(),
            };
            barrier.wait();
            store.admit_agent_run(&spec, snapshot(&root))
        }));
    }
    let mut created = None;
    let mut busy = 0;
    for worker in workers {
        match worker.join().unwrap() {
            Ok(AgentRunAdmission::Created(run)) => assert!(created.replace(run).is_none()),
            Err(StoreError::ConversationBusy {
                conversation_id: rejected,
            }) => {
                assert_eq!(rejected, conversation_id);
                busy += 1;
            }
            result => panic!("unexpected admission result: {result:?}"),
        }
    }
    assert_eq!(busy, 7);
    let created = created.unwrap();
    let replay = AgentRunSpec {
        reasoning: None,
        origin: created.origin.clone(),
        input: Content::text("independent input").unwrap(),
    };
    let independent = AgentRunSpec {
        reasoning: None,
        origin: AgentRunOrigin::User {
            conversation_id,
            message_id: MessageId::generate(),
        },
        input: Content::text("next input").unwrap(),
    };
    for status in ["pending", "running"] {
        store
            .lock()
            .unwrap()
            .connection()
            .execute(
                "UPDATE agent_runs SET status=?1 WHERE agent_run_id=?2",
                params![status, created.id.as_bytes().as_slice()],
            )
            .unwrap();
        assert!(
            matches!(store.admit_agent_run(&replay, snapshot(&root)).unwrap(), AgentRunAdmission::Replayed(run) if run.id == created.id)
        );
        assert!(matches!(
            store.admit_agent_run(&independent, snapshot(&root)),
            Err(StoreError::ConversationBusy { .. })
        ));
        assert_eq!(
            store
                .conversation_messages(conversation_id, None, 100)
                .unwrap()
                .messages
                .len(),
            1
        );
    }
    let other_id = ConversationId::generate();
    bind_conversation(&store, &root, other_id);
    assert!(matches!(
        store
            .admit_agent_run(
                &AgentRunSpec {
                    reasoning: None,
                    origin: AgentRunOrigin::User {
                        conversation_id: other_id,
                        message_id: MessageId::generate()
                    },
                    input: Content::text("other Conversation remains independent").unwrap(),
                },
                snapshot(&root)
            )
            .unwrap(),
        AgentRunAdmission::Created(_)
    ));
    store
        .lock()
        .unwrap()
        .connection()
        .execute(
            "UPDATE agent_runs SET status='completed' WHERE agent_run_id=?1",
            [created.id.as_bytes().as_slice()],
        )
        .unwrap();
    assert!(matches!(
        store
            .admit_agent_run(&independent, snapshot(&root))
            .unwrap(),
        AgentRunAdmission::Created(_)
    ));
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn concurrent_user_origin_admission_has_one_natural_identity() {
    let root =
        std::env::temp_dir().join(format!("agl-daemon-user-origin-{}", uuid::Uuid::now_v7()));
    let store = StoreHandle::open_at(&root).unwrap();
    let spec = AgentRunSpec {
        reasoning: None,
        origin: AgentRunOrigin::User {
            conversation_id: ConversationId::generate(),
            message_id: MessageId::generate(),
        },
        input: Content::text("user prompt").unwrap(),
    };
    let conversation_id = &spec.origin.conversation_id();
    bind_conversation(&store, &root, *conversation_id);
    let ids = concurrently_admit(&store, &root, &spec, &snapshot(&root));
    assert!(ids.iter().all(|id| id == &ids[0]));
    assert!(
        store
            .admit_agent_run(
                &AgentRunSpec {
                    reasoning: None,
                    origin: spec.origin,
                    input: Content::text("changed user prompt").unwrap(),
                },
                snapshot(&root),
            )
            .is_err()
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn internal_message_content_never_crosses_a_conversation_query() {
    let root = std::env::temp_dir().join(format!(
        "agl-daemon-internal-message-{}",
        uuid::Uuid::now_v7()
    ));
    let store = StoreHandle::open_at(&root).unwrap();
    let conversation_id = ConversationId::generate();
    let spec = AgentRunSpec {
        reasoning: None,
        origin: AgentRunOrigin::User {
            conversation_id,
            message_id: MessageId::generate(),
        },
        input: Content::text("visible user message").unwrap(),
    };
    bind_conversation(&store, &root, conversation_id);
    let AgentRunAdmission::Created(run) = store.admit_agent_run(&spec, snapshot(&root)).unwrap()
    else {
        panic!("first admission must create")
    };
    let operation = AgentOperationKey {
        run_id: run.id,
        ordinal: NonZeroU32::MIN,
    };
    let internal = AgentMessage {
        id: MessageId::generate(),
        conversation_id: Some(conversation_id),
        run_id: Some(run.id),
        source_operation: Some(operation),
        role: agl_core::agent::MessageRole::Assistant,
        visibility: agl_core::agent::MessageVisibility::Internal,
        content: Content::text("must-never-cross-query").unwrap(),
    };
    store
        .lock()
        .unwrap()
        .transaction(|transaction| insert_agent_message(transaction, &internal))
        .unwrap();

    let page = store
        .conversation_messages(conversation_id, None, 100)
        .unwrap();
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].content.as_text(), "visible user message");
    let encoded = serde_json::to_string(&page).unwrap();
    assert!(!encoded.contains("must-never-cross-query"));
    let _ = std::fs::remove_dir_all(root);
}

fn running_operation(root: &std::path::Path) -> (StoreHandle, AgentOperation) {
    let store = StoreHandle::open_at(root).unwrap();
    let conversation_id = ConversationId::generate();
    let spec = AgentRunSpec {
        reasoning: None,
        origin: AgentRunOrigin::User {
            conversation_id,
            message_id: MessageId::generate(),
        },
        input: Content::text("input").unwrap(),
    };
    bind_conversation(&store, root, conversation_id);
    let AgentRunAdmission::Created(run) = store.admit_agent_run(&spec, snapshot(root)).unwrap()
    else {
        panic!("first admission must create")
    };
    let fsm = AgentFsm::new(
        run.id,
        Some(conversation_id),
        run.snapshot.limits,
        run.snapshot.model.runtime.generation.max_output_tokens,
        &run.snapshot.tools,
    );
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
        .commit_agent_transition(run.id, 0, &driven.state, &driven.output)
        .unwrap();
    let operation = driven.output.operation.unwrap();
    let machine = AgentOperationFsm::for_operation(&operation);
    let started = machine
        .transition(
            &operation.state,
            AgentOperationFsmInput::Start {
                key: operation.key.clone(),
                delivery_attempt: operation.delivery_attempt,
                request: operation.request.clone(),
            },
        )
        .unwrap();
    let started_operation = operation
        .apply_fsm_transition(started.state, &started.output)
        .unwrap();
    store
        .commit_agent_operation_transition(&operation, &started_operation, &started.output)
        .unwrap();
    (store, started_operation)
}

#[test]
fn record_memory_claim_rejection_appends_event_without_touching_operation() {
    let root = std::env::temp_dir().join(format!("agl-memory-claim-{}", uuid::Uuid::now_v7()));
    let (store, operation) = running_operation(&root);
    assert_eq!(
        operation.state,
        agl_core::agent::AgentOperationDeliveryState::Running
    );
    let source = MessageId::generate();
    let slug = Some("repl".to_owned());
    let reason = agl_core::agent::MemoryClaimRejection::MissingSources;
    let text = Some("The REPL is the default mode.".to_owned());
    let sources = vec![source];
    let expected = AgentEventData::MemoryClaimRejected {
        key: operation.key.clone(),
        slug: slug.clone(),
        reason,
        text: text.clone(),
        sources: sources.clone(),
    };
    store
        .record_memory_claim_rejection(&operation, slug, reason, text, sources)
        .unwrap();

    let stored_operation = store.agent_operation(&operation.key).unwrap();
    assert_eq!(stored_operation, operation);

    let page = store.agent_event_page(None, 100).unwrap();
    let event = page
        .events
        .iter()
        .find(|event| matches!(event.data, AgentEventData::MemoryClaimRejected { .. }))
        .expect("memory claim rejection event must be stored");
    assert_eq!(event.data, expected);
    assert_eq!(event.agent_run_id, operation.key.run_id);
    assert_eq!(event.operation, Some(operation.key.clone()));
    assert_eq!(event.operation_revision, Some(operation.revision));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn record_memory_claim_rejection_rejects_a_stale_operation_revision() {
    let root =
        std::env::temp_dir().join(format!("agl-memory-claim-stale-{}", uuid::Uuid::now_v7()));
    let (store, operation) = running_operation(&root);
    let mut stale = operation.clone();
    stale.revision += 1;
    let error = store
        .record_memory_claim_rejection(
            &stale,
            None,
            agl_core::agent::MemoryClaimRejection::NotDurable,
            None,
            vec![],
        )
        .unwrap_err();
    assert!(
        matches!(error, StoreError::TransitionRejected { .. }),
        "stale revision must be rejected, got {error:?}"
    );
    assert_rejection_left_no_trace(&store, &operation);
    let _ = std::fs::remove_dir_all(root);
}

fn assert_rejection_left_no_trace(store: &StoreHandle, operation: &AgentOperation) {
    assert_eq!(store.agent_operation(&operation.key).unwrap(), *operation);
    let page = store.agent_event_page(None, 1_000).unwrap();
    assert!(
        !page
            .events
            .iter()
            .any(|event| matches!(event.data, AgentEventData::MemoryClaimRejected { .. }))
    );
}

#[test]
fn record_memory_claim_rejection_rejects_a_non_running_operation() {
    let root = std::env::temp_dir().join(format!(
        "agl-memory-claim-terminal-{}",
        uuid::Uuid::now_v7()
    ));
    let (store, operation) = running_operation(&root);
    let machine = AgentOperationFsm::for_operation(&operation);
    let failed = machine
        .transition(
            &operation.state,
            AgentOperationFsmInput::Failure {
                key: operation.key.clone(),
                delivery_attempt: operation.delivery_attempt,
                failure: AgentOperationFailure {
                    kind: AgentOperationFailureKind::Unavailable,
                },
                retry_at_ms: None,
            },
        )
        .unwrap();
    let failed_operation = operation
        .apply_fsm_transition(failed.state, &failed.output)
        .unwrap();
    store
        .commit_agent_operation_transition(&operation, &failed_operation, &failed.output)
        .unwrap();
    let stored = store.agent_operation(&operation.key).unwrap();
    assert_eq!(
        stored.state,
        agl_core::agent::AgentOperationDeliveryState::Failed
    );
    let error = store
        .record_memory_claim_rejection(
            &stored,
            None,
            agl_core::agent::MemoryClaimRejection::NotDurable,
            None,
            vec![],
        )
        .unwrap_err();
    assert!(
        matches!(error, StoreError::TransitionRejected { .. }),
        "non-running operation must be rejected, got {error:?}"
    );
    assert_rejection_left_no_trace(&store, &stored);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn record_memory_claim_rejection_rejects_a_mismatched_delivery_attempt() {
    let root =
        std::env::temp_dir().join(format!("agl-memory-claim-attempt-{}", uuid::Uuid::now_v7()));
    let (store, operation) = running_operation(&root);
    let mut mismatched = operation.clone();
    mismatched.delivery_attempt = NonZeroU32::new(2).unwrap();
    let error = store
        .record_memory_claim_rejection(
            &mismatched,
            None,
            agl_core::agent::MemoryClaimRejection::NotDurable,
            None,
            vec![],
        )
        .unwrap_err();
    assert!(
        matches!(error, StoreError::TransitionRejected { .. }),
        "mismatched delivery attempt must be rejected, got {error:?}"
    );
    assert_rejection_left_no_trace(&store, &operation);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn pending_memory_preserves_later_identical_claims_and_audits_once() {
    let root = std::env::temp_dir().join(format!("agl-pending-memory-{}", uuid::Uuid::now_v7()));
    let (store, operation) = running_operation(&root);
    let transition = AgentOperationFsm::for_operation(&operation)
        .transition(
            &operation.state,
            AgentOperationFsmInput::Failure {
                key: operation.key.clone(),
                delivery_attempt: operation.delivery_attempt,
                failure: AgentOperationFailure {
                    kind: AgentOperationFailureKind::Unavailable,
                },
                retry_at_ms: None,
            },
        )
        .unwrap();
    let terminal = operation
        .apply_fsm_transition(transition.state, &transition.output)
        .unwrap();
    store
        .commit_agent_operation_transition(&operation, &terminal, &transition.output)
        .unwrap();
    let operation = terminal;
    let topic = MemoryTopic {
        slug: "daemon-state".into(),
        claims: vec![],
    };
    store
        .append_pending_memory(&root, &operation.key, std::slice::from_ref(&topic))
        .unwrap();
    let snapshot = store.pending_memory(&root).unwrap();
    store
        .append_pending_memory(&root, &operation.key, std::slice::from_ref(&topic))
        .unwrap();
    let completed = vec![(
        snapshot[0].clone(),
        vec![(0, agl_core::agent::MemoryClaimRejection::IndexCapReached)],
    )];
    store.complete_pending_memory(&root, &completed).unwrap();
    let remaining = store.pending_memory(&root).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_ne!(remaining[0].id, snapshot[0].id);
    assert_eq!(remaining[0].topic, topic);
    store.complete_pending_memory(&root, &completed).unwrap();
    let events = store.agent_event_page(None, 100).unwrap().events;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.data, AgentEventData::MemoryClaimRejected { .. }))
            .count(),
        1
    );
    assert_eq!(store.agent_operation(&operation.key).unwrap(), operation);
    store
        .complete_pending_memory(&root, &[(remaining[0].clone(), vec![])])
        .unwrap();
    assert!(store.pending_memory(&root).unwrap().is_empty());
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn scheduled_origins_replay_without_user_messages_and_reuse_history_after_restart() {
    let root = std::env::temp_dir().join(format!("agl-schedule-history-{}", uuid::Uuid::now_v7()));
    let store = StoreHandle::open_at(&root).unwrap();
    let conversation_id = enable_test_schedule(&store, &root).conversation_id;
    let spec = AgentRunSpec {
        reasoning: None,
        origin: AgentRunOrigin::Timer {
            conversation_id,
            function_id: function_ref().id,
            tick_at_ms: 1000,
        },
        input: Content::text("scheduled task").unwrap(),
    };
    let AgentRunAdmission::Created(run) = store.admit_agent_run(&spec, snapshot(&root)).unwrap()
    else {
        panic!("first tick must create")
    };
    assert_eq!(
        run.snapshot.instructions.blocks.last().unwrap().content,
        spec.input
    );
    assert!(
        matches!(&run.checkpoint, AgentCheckpoint::Ready { context, .. } if context.is_empty())
    );
    let fsm = AgentFsm::new(
        run.id,
        Some(conversation_id),
        run.snapshot.limits,
        run.snapshot.model.runtime.generation.max_output_tokens,
        &run.snapshot.tools,
    );
    let initial = AgentFsmState {
        status: run.status,
        checkpoint: run.checkpoint,
        usage: run.usage,
    };
    let driven = fsm.transition(&initial, AgentFsmInput::Drive).unwrap();
    store
        .commit_agent_transition(run.id, 0, &driven.state, &driven.output)
        .unwrap();
    let operation = driven.output.operation.unwrap();
    let machine = AgentOperationFsm::for_operation(&operation);
    let started = machine
        .transition(
            &operation.state,
            AgentOperationFsmInput::Start {
                key: operation.key.clone(),
                delivery_attempt: operation.delivery_attempt,
                request: operation.request.clone(),
            },
        )
        .unwrap();
    let started_operation = operation
        .apply_fsm_transition(started.state, &started.output)
        .unwrap();
    store
        .commit_agent_operation_transition(&operation, &started_operation, &started.output)
        .unwrap();
    let content = Content::text("previous scheduled result").unwrap();
    let private_reasoning = Content::text("durable-private-reasoning").unwrap();
    let finished = machine
        .transition(
            &started_operation.state,
            AgentOperationFsmInput::Result {
                key: started_operation.key.clone(),
                delivery_attempt: started_operation.delivery_attempt,
                result: AgentOperationResult::ModelGeneration(ModelGenerationResult {
                    private_reasoning: Some(private_reasoning.clone()),
                    output: ModelGenerationOutput::Assistant(content.clone()),
                    finish_reason: ModelFinishReason::Stop,
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    realization: InferenceRealizationRef {
                        runtime_profile_digest:
                            agl_core::agent::InferenceRuntimeProfileDigest::from_bytes([1; 32]),
                        engine_build_digest:
                            agl_core::agent::InferenceEngineBuildDigest::from_bytes([2; 32]),
                        physical_resource_digest:
                            agl_core::agent::PhysicalResourceDigest::from_bytes([3; 32]),
                    },
                    correction: None,
                }),
            },
        )
        .unwrap();
    let finished_operation = started_operation
        .apply_fsm_transition(finished.state, &finished.output)
        .unwrap();
    let message_id = MessageId::generate();
    let resumed = fsm
        .transition(
            &driven.state,
            AgentFsmInput::OperationFinished {
                operation: finished_operation.clone(),
                message_id: Some(message_id.clone()),
            },
        )
        .unwrap();
    store
        .commit_agent_cycle(
            &started_operation,
            &finished_operation,
            &finished.output,
            1,
            &resumed.state,
            &resumed.output,
        )
        .unwrap();

    drop(store);
    let store = StoreHandle::open_at(&root).unwrap();
    assert!(
        matches!(store.admit_agent_run(&spec, snapshot(&root)).unwrap(), AgentRunAdmission::Replayed(replayed) if replayed.id == run.id)
    );
    let event = AgentRunSpec {
        origin: AgentRunOrigin::FileEvent {
            conversation_id,
            function_id: function_ref().id,
            event_id: agl_core::RequestId::generate(),
            paths: vec![RelativePath::try_from("source.txt".to_owned()).unwrap()],
        },
        ..spec
    };
    let AgentRunAdmission::Created(event_run) =
        store.admit_agent_run(&event, snapshot(&root)).unwrap()
    else {
        panic!("file event must create")
    };
    assert!(
        matches!(event_run.checkpoint, AgentCheckpoint::Ready { context, .. } if context == vec![message_id.clone()])
    );
    assert!(
        matches!(store.admit_agent_run(&event, snapshot(&root)).unwrap(), AgentRunAdmission::Replayed(replayed) if replayed.id == event_run.id)
    );
    let messages = store
        .conversation_messages(conversation_id, None, 100)
        .unwrap();
    assert_eq!(messages.messages.len(), 1);
    assert_eq!(messages.messages[0].id, message_id);
    assert_eq!(messages.messages[0].role, MessageRole::Assistant);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn scheduled_admission_blocks_same_function_in_same_workspace_only() {
    let root = std::env::temp_dir().join(format!("agl-schedule-overlap-{}", uuid::Uuid::now_v7()));
    let workspace_a = root.join("a");
    let workspace_b = root.join("b");
    std::fs::create_dir_all(&workspace_a).unwrap();
    std::fs::create_dir_all(&workspace_b).unwrap();
    let store = StoreHandle::open_at(root.join("store")).unwrap();
    let manual_conversation = ConversationId::generate();
    let schedule_a = enable_test_schedule(&store, &workspace_a).conversation_id;
    let schedule_b = enable_test_schedule(&store, &workspace_b).conversation_id;
    bind_conversation(&store, &workspace_a, manual_conversation);

    let manual = AgentRunSpec {
        reasoning: None,
        origin: AgentRunOrigin::User {
            conversation_id: manual_conversation,
            message_id: MessageId::generate(),
        },
        input: Content::text("manual task").unwrap(),
    };
    store
        .admit_agent_run(&manual, snapshot(&workspace_a))
        .unwrap();
    let scheduled = |conversation_id| AgentRunSpec {
        reasoning: None,
        origin: AgentRunOrigin::Timer {
            conversation_id,
            function_id: function_ref().id,
            tick_at_ms: 1000,
        },
        input: Content::text("scheduled task").unwrap(),
    };
    assert!(
        matches!(store.admit_agent_run(&scheduled(schedule_a), snapshot(&workspace_a)),
        Err(StoreError::ConversationBusy { conversation_id }) if conversation_id == manual_conversation)
    );
    assert!(matches!(
        store
            .admit_agent_run(&scheduled(schedule_b), snapshot(&workspace_b))
            .unwrap(),
        AgentRunAdmission::Created(_)
    ));
    let manual_b = ConversationId::generate();
    bind_conversation(&store, &workspace_b, manual_b);
    let manual_b_spec = AgentRunSpec {
        origin: AgentRunOrigin::User {
            conversation_id: manual_b,
            message_id: MessageId::generate(),
        },
        ..manual.clone()
    };
    assert!(
        matches!(store.admit_agent_run(&manual_b_spec, snapshot(&workspace_b)),
        Err(StoreError::ConversationBusy { conversation_id }) if conversation_id == schedule_b)
    );
    let another_manual_a = ConversationId::generate();
    bind_conversation(&store, &workspace_a, another_manual_a);
    let another_manual_spec = AgentRunSpec {
        origin: AgentRunOrigin::User {
            conversation_id: another_manual_a,
            message_id: MessageId::generate(),
        },
        ..manual
    };
    assert!(matches!(
        store
            .admit_agent_run(&another_manual_spec, snapshot(&workspace_a))
            .unwrap(),
        AgentRunAdmission::Created(_)
    ));
    let invalid = AgentRunSpec {
        origin: AgentRunOrigin::Timer {
            conversation_id: schedule_a,
            function_id: PackageId::new("another-function").unwrap(),
            tick_at_ms: 1000,
        },
        ..scheduled(schedule_a)
    };
    assert!(matches!(
        store.admit_agent_run(&invalid, snapshot(&workspace_a)),
        Err(StoreError::InvalidValue { .. })
    ));
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

fn enable_test_schedule(store: &StoreHandle, root: &std::path::Path) -> schedules::ScheduleRecord {
    store.enable_schedule(&function_ref(), &snapshot(root), &agl_runtime::function::FunctionSchedule {
        interval_seconds: 1, max_run_seconds: 5, prompt: "scheduled task".into(), events: vec!["file:source.txt".into()],
    }, &serde_json::json!({"files": "none", "commands": [], "terminal": false, "search": false})).unwrap()
}

#[test]
fn schedule_enable_disable_restart_and_updated_snapshot_preserve_the_selected_history() {
    let root = std::env::temp_dir().join(format!("agl-schedule-enable-{}", uuid::Uuid::now_v7()));
    let store = StoreHandle::open_at(&root).unwrap();
    let enabled = enable_test_schedule(&store, &root);
    let again = enable_test_schedule(&store, &root);
    assert_eq!(enabled.conversation_id, again.conversation_id);
    assert_eq!(enabled.enabled_at_ms, again.enabled_at_ms);
    assert_eq!(enabled.generation, again.generation);
    let (disabled, cancelled) = store.disable_schedule(&root, &function_ref().id).unwrap();
    assert!(!disabled.enabled);
    assert!(cancelled.is_empty());
    let spec = AgentRunSpec {
        reasoning: None,
        input: Content::text("scheduled task").unwrap(),
        origin: AgentRunOrigin::Timer {
            conversation_id: enabled.conversation_id,
            function_id: function_ref().id,
            tick_at_ms: 1000,
        },
    };
    assert!(matches!(
        store.admit_agent_run(&spec, snapshot(&root)),
        Err(StoreError::InvalidValue { .. })
    ));
    let resumed = enable_test_schedule(&store, &root);
    assert_eq!(resumed.conversation_id, enabled.conversation_id);
    assert!(resumed.enabled_at_ms >= enabled.enabled_at_ms);
    assert_ne!(resumed.generation, enabled.generation);
    assert!(matches!(
        store.admit_scheduled_run(&spec, snapshot(&root), &enabled.generation),
        Err(StoreError::InvalidValue { .. })
    ));
    let user = AgentRunSpec {
        origin: AgentRunOrigin::User {
            conversation_id: resumed.conversation_id,
            message_id: MessageId::generate(),
        },
        ..spec.clone()
    };
    assert!(matches!(
        store.admit_agent_run(&user, snapshot(&root)),
        Err(StoreError::InvalidValue { .. })
    ));
    let AgentRunAdmission::Created(run) = store.admit_agent_run(&spec, snapshot(&root)).unwrap()
    else {
        panic!("tick must create")
    };
    assert_eq!(run.snapshot.limits.deadline_ms, 5000);
    let (disabled, cancelled) = store.disable_schedule(&root, &function_ref().id).unwrap();
    assert_eq!(cancelled, [run.id]);
    let mut updated_snapshot = snapshot(&root);
    updated_snapshot.instructions = InstructionSet::new(vec![agl_core::agent::InstructionBlock {
        source: agl_core::agent::InstructionSource::Agent,
        content: Content::text("new dependency instructions").unwrap(),
    }])
    .unwrap();
    let updated = store
        .enable_schedule(
            &function_ref(),
            &updated_snapshot,
            &disabled.config,
            &disabled.permissions,
        )
        .unwrap();
    assert_ne!(updated.conversation_id, enabled.conversation_id);
    assert!(store.conversation_binding(enabled.conversation_id).is_ok());
    drop(store);
    let store = StoreHandle::open_at(&root).unwrap();
    let records = store.schedules(Some(&root)).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].conversation_id, updated.conversation_id);
    assert_eq!(records[0].snapshot, updated_snapshot);
    assert!(records[0].enabled);
    let (_, active_old_generation) = store.disable_schedule(&root, &function_ref().id).unwrap();
    assert_eq!(active_old_generation, [run.id]);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn schedule_next_tick_keeps_enable_anchor_and_skips_missed_ticks() {
    assert_eq!(schedules::next_tick_after(1000, 2, 1000), Some(3000));
    assert_eq!(schedules::next_tick_after(1000, 2, 3000), Some(5000));
    assert_eq!(schedules::next_tick_after(1000, 2, 100_000), Some(101_000));
    assert_eq!(schedules::next_tick_after(1000, 2, 999), Some(3000));
    assert_eq!(
        schedules::next_tick_after(i64::MAX - 10, 1, i64::MAX - 10),
        None
    );
    assert_eq!(schedules::next_tick_after(1000, 0, 1000), None);
}
