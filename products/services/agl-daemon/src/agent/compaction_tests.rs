mod compaction_tests {
    use super::*;
    use crate::agent::compaction::deterministic_semantic_summary;
    use agl_core::agent::{
        AgentCheckpoint, AgentContextEntry, AgentMessage, CompactionFailureStage, CompactionResult,
        InstructionBlock, InstructionSource, MessageRole, MessageVisibility,
        ModelOutputFailureClass, ReasoningEffort, ReasoningSelection,
    };
    use std::sync::Mutex;

    #[derive(Clone, Copy)]
    enum Fault {
        None,
        InvalidOnce,
        InvalidAlways,
        InvalidSourceAlways,
        OversizedSource,
        RebuiltOnce,
        RebuiltAlways,
        MeasureFailsAfterSummary,
        CheckpointOverflow,
        Cancelled,
    }

    struct CompactionModel {
        expected: agl_core::agent::ModelRuntimeSelection,
        supports_low: bool,
        fault: Fault,
        summaries: AtomicUsize,
        work: Mutex<Vec<Vec<AgentContextEntry>>>,
    }

    fn is_summary(request: &InferenceGenerateRequest) -> bool {
        request.instructions.blocks.iter().any(|block| {
            block
                .content
                .as_text()
                .starts_with("This is the Agent's context-compaction operation")
        })
    }

    impl InferenceGenerator for CompactionModel {
        fn measure(
            &self,
            request: InferenceGenerateRequest,
        ) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError> {
            // An explicit fixture token oracle, independent of text byte length:
            // summary=400, an isolated entry=64, work=128, forced expansion=900.
            if matches!(self.fault, Fault::MeasureFailsAfterSummary)
                && self.summaries.load(Ordering::SeqCst) > 0
            {
                return Err(InferenceServiceError::Unavailable);
            }
            let count = if is_summary(&request) {
                if matches!(self.fault, Fault::OversizedSource) {
                    1025
                } else {
                    400
                }
            } else if request.instructions.blocks.is_empty() {
                request.context.len() as u64 * 64
            } else if request.operation.ordinal.get() == 1
                && request
                    .context
                    .last()
                    .is_some_and(|entry| entry.message.content.as_text().starts_with("expand"))
            {
                900
            } else if (matches!(self.fault, Fault::RebuiltOnce)
                && self.summaries.load(Ordering::SeqCst) == 1)
                || (matches!(self.fault, Fault::RebuiltAlways)
                    && self.summaries.load(Ordering::SeqCst) > 0
                    && !request.context.iter().any(|entry| {
                        entry
                            .message
                            .content
                            .as_text()
                            .contains("Historical context:")
                    }))
            {
                100
            } else if matches!(self.fault, Fault::CheckpointOverflow)
                && request
                    .context
                    .iter()
                    .filter(|entry| {
                        matches!(
                            &entry.source_request,
                            Some(AgentOperationRequest::Compaction(_))
                        )
                    })
                    .count()
                    > 1
            {
                900
            } else {
                96
            };
            Ok(agl_core::agent::ContextCapacity::new(
                count,
                request.generation.max_output_tokens,
                request.runtime.load.context_tokens,
            ))
        }

        fn generate(
            &self,
            request: InferenceGenerateRequest,
        ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError>
        {
            assert!(self.expected.shares_service_with(&request.runtime));
            let summary = is_summary(&request);
            let text = if summary {
                let response_format = request
                    .response_format
                    .as_ref()
                    .expect("compaction must use schema-constrained output");
                assert_eq!(response_format["type"], "json_schema");
                let allowed = response_format["json_schema"]["schema"]["$defs"]["claim"]
                    ["properties"]["sources"]["items"]["enum"]
                    .as_array()
                    .expect("source IDs must be constrained");
                for pair in request.context.as_chunks::<2>().0 {
                    let historical = &pair[1];
                    assert!(allowed.contains(&json!(historical.message.id)));
                    let text = historical.message.content.as_text();
                    if text.starts_with("seed-long ") {
                        assert!(text.ends_with("END-OF-HISTORY"));
                        assert!(text.len() > 2048);
                    }
                    if matches!(historical.source_request, Some(AgentOperationRequest::Compaction(_))) {
                        let semantic: agl_core::agent::SemanticSummary = serde_json::from_str(text).unwrap();
                        semantic
                            .validate(std::slice::from_ref(&historical.message.id))
                            .unwrap();
                    }
                }
                assert_eq!(
                    response_format["json_schema"]["name"],
                    "semantic_summary"
                );
                assert!(
                    request
                        .context
                        .iter()
                        .all(|entry| entry.private_reasoning.is_none())
                );
                assert_eq!(request.generation.max_output_tokens, 128);
                assert_eq!(
                    request.runtime.reasoning,
                    if self.supports_low {
                        ReasoningSelection::Enabled {
                            max_tokens: 32,
                            effort: Some(ReasoningEffort::Low),
                            preserve: false,
                        }
                    } else {
                        ReasoningSelection::Disabled
                    }
                );
                let attempt = self.summaries.fetch_add(1, Ordering::SeqCst);
                if matches!(self.fault, Fault::Cancelled) {
                    return Err(InferenceServiceError::Cancelled);
                }
                if matches!(self.fault, Fault::InvalidAlways)
                    || (matches!(self.fault, Fault::InvalidOnce) && attempt == 0)
                {
                    "not a semantic summary".to_owned()
                } else if matches!(self.fault, Fault::InvalidSourceAlways) {
                    let claim = json!({
                        "text": "Continue the fixture objective",
                        "sources": [MessageId::generate().to_string()]
                    });
                    json!({
                        "objective": claim,
                        "rationale": [],
                        "decisions": [],
                        "completed": [],
                        "discoveries": [],
                        "unresolved": [],
                        "next_position": claim
                    })
                    .to_string()
                } else {
                    let marker = request
                        .context
                        .iter()
                        .find(|entry| {
                            entry
                                .message
                                .content
                                .as_text()
                                .starts_with("Source message: ")
                        })
                        .unwrap();
                    let source = marker
                        .message
                        .content
                        .as_text()
                        .split_whitespace()
                        .nth(2)
                        .unwrap();
                    let claim =
                        json!({"text":"Continue the fixture objective", "sources":[source]});
                    json!({"objective":claim, "rationale":[], "decisions":[], "completed":[], "discoveries":[], "unresolved":[], "next_position":claim}).to_string()
                }
            } else {
                assert_eq!(request.runtime.reasoning, self.expected.reasoning);
                self.work.lock().unwrap().push(request.context.clone());
                "fixture answer".to_owned()
            };
            Ok(agl_runtime::inference::InferenceGenerateResult {
                operation: request.operation,
                delivery_attempt: request.delivery_attempt,
                result: ModelGenerationResult {
                    output: ModelGenerationOutput::Assistant(Content::text(text).unwrap()),
                    private_reasoning: (!summary)
                        .then(|| Content::text("private seed reasoning").unwrap()),
                    finish_reason: ModelFinishReason::Stop,
                    usage: ModelUsage {
                        input_tokens: if summary { 400 } else { 128 },
                        output_tokens: if summary { 64 } else { 4 },
                    },
                    realization: realization(8),
                    correction: None,
                },
            })
        }
    }

    fn configuration(root: &std::path::Path, supports_low: bool) -> AgentRunSnapshot {
        let mut configured = snapshot(root);
        configured.model.runtime.load.context_tokens = 1024;
        configured.model.runtime.generation.max_output_tokens = 256;
        configured.model.runtime.reasoning = ReasoningSelection::Enabled {
            max_tokens: 128,
            effort: Some(ReasoningEffort::Xhigh),
            preserve: true,
        };
        configured.model.reasoning_efforts = vec![ReasoningEffort::Xhigh];
        if supports_low {
            configured
                .model
                .reasoning_efforts
                .push(ReasoningEffort::Low);
        }
        configured.instructions = InstructionSet::new(vec![InstructionBlock {
            source: InstructionSource::Agent,
            content: Content::text("fixture work instructions").unwrap(),
        }])
        .unwrap();
        configured.limits.model_calls = 16;
        configured.limits.model_output_tokens = 8192;
        configured.limits.model_input_tokens = None;
        configured
    }

    fn run(
        handle: &AgentHandle,
        store: &StoreHandle,
        conversation_id: ConversationId,
        text: &str,
    ) -> agl_core::agent::AgentRunView {
        let id = handle
            .start_run(AgentRunSpec {
                reasoning: None,
                origin: AgentRunOrigin::User {
                    conversation_id,
                    message_id: MessageId::generate(),
                },
                input: Content::text(text).unwrap(),
            })
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let view = store.agent_run_view(id).unwrap();
            if view.status.is_terminal() {
                return view;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fixture Run stalled: {view:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn start(
        store: &StoreHandle,
        model: Arc<CompactionModel>,
    ) -> (AgentService, AgentHandle, InferenceService) {
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(model),
            RestoredInferenceHealth::default(),
        )
        .unwrap();
        let (service, handle) = AgentService::start(
            AgentDependencies {
                store: store.clone(),
                inference: Arc::new(inference),
            },
            vec![],
        )
        .unwrap();
        (service, handle, inference_service)
    }

    fn result(store: &StoreHandle, run_id: AgentRunId, ordinal: u32) -> CompactionResult {
        let operation = store
            .agent_operation(&agl_core::agent::AgentOperationKey {
                run_id,
                ordinal: NonZeroU32::new(ordinal).unwrap(),
            })
            .unwrap();
        let Some(AgentOperationResult::Compaction(result)) = operation.result else {
            panic!("expected compaction: {operation:?}")
        };
        *result
    }

    #[test]
    fn two_generations_resume_the_committed_summary_and_preserve_original_reasoning() {
        for supports_low in [true, false] {
            let root = std::env::temp_dir()
                .join(format!("agl-compaction-reopen-{}", uuid::Uuid::now_v7()));
            let store = StoreHandle::open_at(&root).unwrap();
            let conversation = ConversationId::generate();
            let configured = configuration(&root, supports_low);
            bind_conversation(&store, conversation, &configured);
            let model = Arc::new(CompactionModel {
                expected: configured.model.runtime.clone(),
                supports_low,
                fault: Fault::None,
                summaries: AtomicUsize::new(0),
                work: Mutex::new(vec![]),
            });
            let (service, handle, inference) = start(&store, model.clone());
            assert_eq!(
                run(&handle, &store, conversation, &format!("seed-long {} END-OF-HISTORY", "x".repeat(3000))).status,
                AgentRunStatus::Completed
            );
            let originals = store
                .conversation_messages(conversation, None, 100)
                .unwrap()
                .messages;
            let original_ids: Vec<_> = originals.iter().map(|message| message.id.clone()).collect();
            let first = run(&handle, &store, conversation, "expand-1");
            assert_eq!(first.status, AgentRunStatus::Completed, "{first:?}");
            assert_eq!(first.usage.model_calls, 2);
            let first_record = result(&store, first.id, 2);
            assert!(
                first_record.metadata.after.prompt_tokens
                    <= u64::from(first_record.metadata.after.context_capacity_tokens) / 10,
                "compaction rebuilt {} tokens in a {}-token window",
                first_record.metadata.after.prompt_tokens,
                first_record.metadata.after.context_capacity_tokens,
            );
            assert!(
                first_record
                    .metadata
                    .validate_model(&configured.model)
                    .is_ok()
            );
            for mutation in 0..6 {
                let mut metadata = first_record.metadata.clone();
                match mutation {
                    0 => metadata.after.context_capacity_tokens += 1,
                    1 => metadata.after.reserved_output_tokens += 1,
                    2 => metadata.summary_request.reserved_output_tokens += 1,
                    3 => metadata.before.trigger_threshold_tokens += 1,
                    4 => metadata.usage.output_tokens = 129,
                    _ => {
                        metadata.tokenizer_artifact =
                            agl_core::agent::PackageDigest::from_bytes([9; 32])
                    }
                }
                assert!(
                    metadata.validate_model(&configured.model).is_err(),
                    "accepted metadata mutation {mutation}"
                );
            }
            assert_eq!(first_record.metadata.source, original_ids);
            assert_eq!(first_record.metadata.tail.len(), 1);
            assert_eq!(first_record.metadata.tail_tokens, 64);
            assert!(
                store
                    .agent_context(&original_ids)
                    .unwrap()
                    .iter()
                    .any(|entry| entry.private_reasoning.is_some())
            );
            service.shutdown();
            inference.shutdown();
            drop(handle);
            drop(store);
            let store = StoreHandle::open_at(&root).unwrap();
            assert_eq!(result(&store, first.id, 2), first_record);
            let (service, handle, inference) = start(&store, model.clone());
            let second = run(&handle, &store, conversation, "expand-2");
            assert_eq!(second.status, AgentRunStatus::Completed, "{second:?}");
            let second_record = result(&store, second.id, 2);
            assert!(
                second_record
                    .content
                    .exact
                    .operations
                    .iter()
                    .any(|group| group.count >= 2)
            );
            assert!(
                second_record
                    .content
                    .exact
                    .operations
                    .iter()
                    .all(|group| group.count == group.sources.len() as u64
                        && group.sources.first() == Some(&group.first)
                        && group.sources.last() == Some(&group.last))
            );
            let summaries = model.summaries.load(Ordering::SeqCst);
            assert_eq!(
                run(&handle, &store, conversation, "continue").status,
                AgentRunStatus::Completed
            );
            assert_eq!(model.summaries.load(Ordering::SeqCst), summaries);
            let work = model.work.lock().unwrap();
            let resumed = work.last().unwrap();
            assert!(matches!(
                resumed[0].source_request,
                Some(AgentOperationRequest::Compaction(_))
            ));
            assert!(
                resumed
                    .iter()
                    .all(|entry| !original_ids.contains(&entry.message.id))
            );
            drop(work);
            assert_eq!(store.agent_context(&original_ids).unwrap().len(), 2);
            service.shutdown();
            inference.shutdown();
            drop(handle);
            drop(store);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn restores_the_ten_most_recent_compaction_checkpoints() {
        let root = std::env::temp_dir()
            .join(format!("agl-compaction-checkpoints-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation = ConversationId::generate();
        let configured = configuration(&root, false);
        bind_conversation(&store, conversation, &configured);
        let model = Arc::new(CompactionModel {
            expected: configured.model.runtime.clone(),
            supports_low: false,
            fault: Fault::None,
            summaries: AtomicUsize::new(0),
            work: Mutex::new(vec![]),
        });
        let (service, handle, inference) = start(&store, model.clone());

        assert_eq!(
            run(&handle, &store, conversation, "seed").status,
            AgentRunStatus::Completed
        );
        let mut all_checkpoints = Vec::new();
        for index in 0..12 {
            let view = run(
                &handle,
                &store,
                conversation,
                &format!("expand-{index}"),
            );
            assert_eq!(view.status, AgentRunStatus::Completed, "{view:?}");
            let run = store.agent_run(view.id).unwrap();
            let AgentCheckpoint::Ready { context, .. } = run.checkpoint else {
                panic!("completed Run is not ready")
            };
            let checkpoints: Vec<_> = store
                .agent_context(&context)
                .unwrap()
                .into_iter()
                .filter(|entry| {
                    matches!(
                        &entry.source_request,
                        Some(AgentOperationRequest::Compaction(_))
                    )
                })
                .map(|entry| entry.message.id)
                .collect();
            assert_eq!(checkpoints.len(), (index + 1).min(10));
            all_checkpoints.push(checkpoints.last().unwrap().clone());
            if index >= 9 {
                assert_eq!(checkpoints, all_checkpoints[index + 1 - 10..]);
            }
        }

        service.shutdown();
        inference.shutdown();
        drop(handle);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checkpoint_overflow_drops_old_checkpoints_but_keeps_tail() {
        let root = std::env::temp_dir()
            .join(format!("agl-compaction-checkpoint-overflow-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation = ConversationId::generate();
        let configured = configuration(&root, false);
        bind_conversation(&store, conversation, &configured);
        let model = Arc::new(CompactionModel {
            expected: configured.model.runtime.clone(),
            supports_low: false,
            fault: Fault::CheckpointOverflow,
            summaries: AtomicUsize::new(0),
            work: Mutex::new(vec![]),
        });
        let (service, handle, inference) = start(&store, model.clone());

        assert_eq!(
            run(&handle, &store, conversation, "seed").status,
            AgentRunStatus::Completed
        );
        assert_eq!(
            run(&handle, &store, conversation, "expand-1").status,
            AgentRunStatus::Completed
        );
        let second = run(&handle, &store, conversation, "expand-2");
        assert_eq!(second.status, AgentRunStatus::Completed, "{second:?}");
        let compaction = result(&store, second.id, 2);
        assert!(compaction.metadata.retained_checkpoints.is_empty());
        let AgentCheckpoint::Ready { context, .. } = store.agent_run(second.id).unwrap().checkpoint
        else {
            panic!("completed Run is not ready")
        };
        let entries = store.agent_context(&context).unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|entry| {
                    matches!(
                        &entry.source_request,
                        Some(AgentOperationRequest::Compaction(_))
                    )
                })
                .count(),
            1
        );
        assert!(entries
            .iter()
            .any(|entry| entry.message.content.as_text() == "expand-2"));

        service.shutdown();
        inference.shutdown();
        drop(handle);
        drop(store);
        let store = StoreHandle::open_at(&root).unwrap();
        let (service, handle, inference) = start(&store, model);
        let third = run(&handle, &store, conversation, "expand-3");
        assert_eq!(third.status, AgentRunStatus::Completed, "{third:?}");
        assert!(result(&store, third.id, 2)
            .metadata
            .retained_checkpoints
            .is_empty());

        service.shutdown();
        inference.shutdown();
        drop(handle);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_summary_gets_one_correction_and_oversized_source_never_generates() {
        for fault in [
            Fault::InvalidOnce,
            Fault::InvalidAlways,
            Fault::OversizedSource,
            Fault::RebuiltOnce,
            Fault::RebuiltAlways,
            Fault::Cancelled,
        ] {
            let root = std::env::temp_dir()
                .join(format!("agl-compaction-failure-{}", uuid::Uuid::now_v7()));
            let store = StoreHandle::open_at(&root).unwrap();
            let conversation = ConversationId::generate();
            let configured = configuration(&root, false);
            bind_conversation(&store, conversation, &configured);
            let model = Arc::new(CompactionModel {
                expected: configured.model.runtime.clone(),
                supports_low: false,
                fault,
                summaries: AtomicUsize::new(0),
                work: Mutex::new(vec![]),
            });
            let (service, handle, inference) = start(&store, model.clone());
            assert_eq!(
                run(&handle, &store, conversation, "seed").status,
                AgentRunStatus::Completed
            );
            let view = run(&handle, &store, conversation, "expand");
            if matches!(fault, Fault::Cancelled) {
                assert_eq!(view.status, AgentRunStatus::Cancelled, "{view:?}");
            } else if matches!(fault, Fault::InvalidOnce) {
                assert_eq!(view.status, AgentRunStatus::Completed, "{view:?}");
                assert_eq!(view.usage.model_calls, 3);
                assert!(result(&store, view.id, 3).metadata.correction_of.is_some());
            } else if matches!(fault, Fault::RebuiltOnce | Fault::RebuiltAlways) {
                assert_eq!(view.status, AgentRunStatus::Completed, "{view:?}");
                assert_eq!(view.usage.model_calls, 2);
                assert!(result(&store, view.id, 2).metadata.correction_of.is_none());
            } else if matches!(fault, Fault::InvalidAlways) {
                assert_eq!(view.status, AgentRunStatus::Completed, "{view:?}");
                assert_eq!(view.usage.model_calls, 3);
                let fallback = result(&store, view.id, 3);
                assert!(fallback.metadata.correction_of.is_some());
                assert_eq!(fallback.content.semantic.rationale.len(), 0);
                assert!(fallback
                    .content
                    .semantic
                    .objective
                    .text
                    .starts_with("Current user objective:"));
            } else {
                assert_eq!(view.status, AgentRunStatus::Failed, "{view:?}");
                let operation = store
                    .agent_operation(view.current_operation.as_ref().unwrap())
                    .unwrap();
                let AgentOperationFailureKind::ContextExhausted(
                    agl_core::agent::ContextExhaustion {
                        compaction: Some(failure),
                        ..
                    },
                ) = operation.failure.unwrap().kind
                else {
                    panic!("expected structural compaction failure")
                };
                assert_eq!(
                    failure.stage,
                    if matches!(fault, Fault::OversizedSource) {
                        CompactionFailureStage::SummarySource
                    } else {
                        CompactionFailureStage::SemanticOutput
                    }
                );
                assert!(failure.summary_source_start.is_some());
                assert!(failure.summary_source_end.is_some());
                if matches!(fault, Fault::OversizedSource) {
                    assert!(!failure.source_contributions.is_empty());
                    assert!(failure.source_contributions.len() <= 8);
                    assert!(
                        failure
                            .source_contributions
                            .iter()
                            .all(|range| range.isolated_prompt_tokens == 1025)
                    );
                }
                assert_eq!(
                    store
                        .conversation_messages(conversation, None, 100)
                        .unwrap()
                        .messages
                        .len(),
                    3
                );
            }
            assert_eq!(
                model.summaries.load(Ordering::SeqCst),
                if matches!(fault, Fault::OversizedSource) {
                    0
                } else if matches!(
                    fault,
                    Fault::Cancelled | Fault::RebuiltOnce | Fault::RebuiltAlways
                ) {
                    1
                } else {
                    2
                }
            );
            service.shutdown();
            inference.shutdown();
            drop(handle);
            drop(store);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn rejected_semantic_outputs_record_their_diagnostic_class() {
        for (fault, class, field) in [
            (Fault::InvalidAlways, ModelOutputFailureClass::Syntax, None::<&str>),
            (
                Fault::InvalidSourceAlways,
                ModelOutputFailureClass::InvalidContent,
                Some("sources"),
            ),
        ] {
            let root = std::env::temp_dir()
                .join(format!("agl-compaction-rejection-{}", uuid::Uuid::now_v7()));
            let store = StoreHandle::open_at(&root).unwrap();
            let conversation = ConversationId::generate();
            let configured = configuration(&root, false);
            bind_conversation(&store, conversation, &configured);
            let model = Arc::new(CompactionModel {
                expected: configured.model.runtime.clone(),
                supports_low: false,
                fault,
                summaries: AtomicUsize::new(0),
                work: Mutex::new(vec![]),
            });
            let (service, handle, inference) = start(&store, model.clone());
            assert_eq!(
                run(&handle, &store, conversation, "seed").status,
                AgentRunStatus::Completed
            );
            let view = run(&handle, &store, conversation, "expand");
            assert_eq!(view.status, AgentRunStatus::Completed, "{view:?}");
            assert_eq!(view.usage.model_calls, 3);
            assert!(result(&store, view.id, 3).metadata.correction_of.is_some());
            let events: Vec<_> = store
                .agent_event_page(None, 1000)
                .unwrap()
                .events
                .into_iter()
                .filter(|event| {
                    event.agent_run_id == view.id
                        && matches!(
                            event.data,
                            agl_core::agent::AgentEventData::ModelOutputRejected { .. }
                        )
                })
                .collect();
            assert_eq!(events.len(), 2, "{events:?}");
            for (attempt, event) in events.iter().enumerate() {
                let agl_core::agent::AgentEventData::ModelOutputRejected {
                    key,
                    delivery_attempt,
                    correction_attempt,
                    diagnostic,
                    usage,
                    realization,
                } = &event.data
                else {
                    unreachable!()
                };
                assert_eq!(key.ordinal.get(), attempt as u32 + 2);
                assert_eq!(delivery_attempt.get(), 1);
                assert_eq!(*correction_attempt, attempt as u32);
                assert_eq!(event.operation.as_ref(), Some(key));
                assert_eq!(diagnostic.class, class);
                assert_eq!(diagnostic.field.as_deref(), field);
                assert_eq!(diagnostic.finish_reason, Some(ModelFinishReason::Stop));
                assert!(diagnostic.output_bytes > 0);
                let raw = store
                    .model_output_rejection_content(event.id)
                    .unwrap()
                    .expect("rejected compaction output");
                assert!(!raw.as_text().is_empty());
                assert!(!serde_json::to_string(event)
                    .unwrap()
                    .contains(raw.as_text()));
                assert_eq!(usage.output_tokens, 64);
                assert!(realization.is_some());
            }
            service.shutdown();
            inference.shutdown();
            drop(handle);
            drop(store);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn rebuilt_measurement_failure_surfaces_the_inference_error() {
        let root = std::env::temp_dir()
            .join(format!("agl-compaction-measure-failure-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation = ConversationId::generate();
        let configured = configuration(&root, false);
        bind_conversation(&store, conversation, &configured);
        let model = Arc::new(CompactionModel {
            expected: configured.model.runtime.clone(),
            supports_low: false,
            fault: Fault::MeasureFailsAfterSummary,
            summaries: AtomicUsize::new(0),
            work: Mutex::new(vec![]),
        });
        let (service, handle, inference) = start(&store, model.clone());
        assert_eq!(
            run(&handle, &store, conversation, "seed").status,
            AgentRunStatus::Completed
        );
        let view = run(&handle, &store, conversation, "expand");
        assert_eq!(view.status, AgentRunStatus::Failed, "{view:?}");
        assert_eq!(model.summaries.load(Ordering::SeqCst), 1);
        let operation = store
            .agent_operation(view.current_operation.as_ref().unwrap())
            .unwrap();
        assert_eq!(
            operation.failure.unwrap().kind,
            AgentOperationFailureKind::Unavailable
        );
        service.shutdown();
        inference.shutdown();
        drop(handle);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deterministic_fallback_uses_the_latest_user_objective() {
        let conversation_id = ConversationId::generate();
        let entry = |role, text| AgentContextEntry {
            message: AgentMessage {
                id: MessageId::generate(),
                conversation_id: Some(conversation_id),
                run_id: None,
                source_operation: None,
                role,
                visibility: MessageVisibility::Conversation,
                content: Content::text(text).unwrap(),
            },
            source_request: None,
            private_reasoning: None,
        };
        let context = vec![
            entry(MessageRole::User, "old objective"),
            entry(MessageRole::Assistant, "old answer"),
            entry(MessageRole::User, "new objective after redirection"),
        ];
        let summary = deterministic_semantic_summary(&context);
        assert_eq!(
            summary.objective.text,
            "Current user objective: new objective after redirection"
        );
        assert_eq!(
            summary.objective.sources,
            vec![context[2].message.id.clone()]
        );
    }
}
