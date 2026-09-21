mod effect_result_tests {
    use super::*;

    struct CommittingTool {
        calls: Arc<AtomicUsize>,
        valid_receipt: bool,
        bound_handler_output: bool,
    }

    impl ToolHandler for CommittingTool {
        fn call(&self, context: ToolContext, _input: serde_json::Value) -> ToolFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let valid_receipt = self.valid_receipt;
            let bound_handler_output = self.bound_handler_output;
            Box::pin(async move {
                std::fs::write(
                    context.workspace.root.as_path().join("committed"),
                    b"effect",
                )
                .unwrap();
                let grant = &context.authority.0[0];
                let receipt = agl_core::agent::EffectReceipt {
                    effect: grant.effect.clone(),
                    scope: if valid_receipt {
                        grant.scope.clone()
                    } else {
                        CanonicalJson::new(json!({"root":"wrong"})).unwrap()
                    },
                };
                if bound_handler_output {
                    crate::tools::committed_result(
                        "x".repeat(agl_core::MAX_TEXT_BYTES + 1),
                        receipt,
                        context.result_bytes,
                    )
                } else {
                    Ok(ToolResult {
                        content: Content::text("x".repeat(4096)).unwrap(),
                        effect_receipts: vec![receipt],
                    })
                }
            })
        }
    }

    #[test]
    fn committed_receipt_survives_output_limit_and_reopen_without_reexecution() {
        for (limit, valid_receipt, multiple_scopes, bound_handler_output) in [
            (1024, true, false, false),
            (1024, false, false, false),
            (128, true, false, false),
            (1024, true, true, false),
            (1024, true, false, true),
            (1024, false, false, true),
        ] {
            let root =
                std::env::temp_dir().join(format!("agl-committed-result-{}", uuid::Uuid::now_v7()));
            let store = StoreHandle::open_at(&root).unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let (mut bindings, mut admitted) = test_extension_with_handler(
                "committed result fixture",
                agl_core::agent::DeliveryClass::AtMostOnce,
                Arc::new(CommittingTool {
                    calls: calls.clone(),
                    valid_receipt,
                    bound_handler_output,
                }),
            );
            let effect = EffectId::new("test.extension:write").unwrap();
            admitted.definition.required_effects = vec![effect.clone()];
            admitted.definition_digest = ToolDefinitionDigest::from_bytes(sha256(
                &serde_json::to_vec(&admitted.definition).unwrap(),
            ));
            bindings.definition.tools = vec![admitted.definition.clone()];
            bindings.tools[0].definition_digest = admitted.definition_digest;
            bindings.definition.effects = vec![agl_core::EffectDefinition {
                id: effect.clone(), description: "write the fixture marker".into(),
                scope_schema: JsonSchema::new(json!({"type":"object","required":["root"],"additionalProperties":false,"properties":{"root":{"const":"workspace"},"label":{"type":"string"}}})).unwrap(),
            }];
            admitted.extension.definition_digest = ExtensionDefinitionDigest::from_bytes(sha256(
                &serde_json::to_vec(&bindings.definition).unwrap(),
            ));
            let mut configured = snapshot(&root);
            configured.limits.tool_result_bytes = limit;
            configured.tools = vec![admitted];
            let scope =
                CanonicalJson::new(json!({"root":root.to_string_lossy(), "label":"a"})).unwrap();
            configured.authority = AuthorityGrantSet(vec![agl_core::AuthorityGrant {
                effect: effect.clone(),
                scope: scope.clone(),
            }]);
            if multiple_scopes {
                configured.authority.0.push(agl_core::AuthorityGrant {
                    effect: effect.clone(),
                    scope: CanonicalJson::new(
                        json!({"root":root.to_string_lossy(),"label":"z".repeat(4096)}),
                    )
                    .unwrap(),
                });
            }
            let conversation_id = ConversationId::generate();
            bind_conversation(&store, conversation_id, &configured);
            let (inference_service, inference) = InferenceService::start(
                InferenceConfig::custom(Arc::new(ToolCallingGenerator(AtomicUsize::new(0)))),
                RestoredInferenceHealth::default(),
            )
            .unwrap();
            let (service, handle) = AgentService::start(
                AgentDependencies {
                    store: store.clone(),
                    inference: Arc::new(inference),
                },
                vec![bindings],
            )
            .unwrap();
            let run_id = handle
                .start_run(AgentRunSpec {
                    reasoning: None,
                    origin: AgentRunOrigin::User {
                        conversation_id,
                        message_id: MessageId::generate(),
                    },
                    input: Content::text("commit once").unwrap(),
                })
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let view = loop {
                let view = store.agent_run_view(run_id).unwrap();
                if view.status.is_terminal() {
                    break view;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "fixture stalled: {view:?}"
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            };
            let dispatched = limit == 1024 && !multiple_scopes;
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(dispatched));
            assert_eq!(root.join("committed").exists(), dispatched);
            let key = agl_core::agent::AgentOperationKey {
                run_id,
                ordinal: NonZeroU32::new(2).unwrap(),
            };
            let operation = store.agent_operation(&key).unwrap();
            if dispatched && valid_receipt {
                assert_eq!(view.status, AgentRunStatus::Completed, "{view:?}");
                let Some(AgentOperationResult::Tool(result)) = &operation.result else {
                    panic!("receipt not accepted")
                };
                assert_eq!(
                    result.effect_receipts,
                    vec![agl_core::agent::EffectReceipt { effect, scope }]
                );
                assert!(serde_json::to_vec(result).unwrap().len() <= limit as usize);
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(result.content.as_text()).unwrap()["effect"],
                    "committed"
                );
                assert_eq!(view.usage.tool_calls, 1);
                let exact = store
                    .compaction_exact_state(&agl_core::agent::AgentOperationKey {
                        run_id,
                        ordinal: NonZeroU32::new(4).unwrap(),
                    })
                    .unwrap();
                let fact = exact
                    .operations
                    .iter()
                    .find(|group| group.fact.kind == agl_core::agent::AgentOperationKind::Tool)
                    .unwrap();
                assert_eq!(fact.fact.effect_receipts, result.effect_receipts);
                assert_eq!(fact.sources, vec![key.clone()]);
            } else if multiple_scopes {
                assert_eq!(view.status, AgentRunStatus::Completed, "{view:?}");
                let Some(AgentOperationResult::Tool(result)) = &operation.result else {
                    panic!("expected pre-dispatch correction")
                };
                assert!(result.effect_receipts.is_empty());
                let diagnostic: serde_json::Value =
                    serde_json::from_str(result.content.as_text()).unwrap();
                assert_eq!(diagnostic["effect"], "none");
                assert_eq!(diagnostic["field"], "tool_result_bytes");
            } else {
                assert_eq!(view.status, AgentRunStatus::Failed, "{view:?}");
                assert_eq!(
                    operation.failure.as_ref().unwrap().kind,
                    if dispatched {
                        AgentOperationFailureKind::OutcomeUnknown
                    } else {
                        AgentOperationFailureKind::ResultTooLarge
                    }
                );
            }
            service.shutdown();
            inference_service.shutdown();
            drop(handle);
            drop(store);
            let reopened = StoreHandle::open_at(&root).unwrap();
            assert_eq!(reopened.agent_operation(&key).unwrap(), operation);
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(dispatched));
            drop(reopened);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
