#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use agl_core::Content;
    use agl_core::agent::{
        AbsolutePath, AgentDefinitionRef, AgentRunLimits, AgentRunOrigin, AgentRunSnapshot,
        AgentRunSpec, AgentRunStatus, AuthorityGrantSet, InferenceEngineBuildDigest,
        InferenceRealizationRef, InferenceRuntimeProfileDigest, InstructionSet, ModelDefinitionRef,
        ModelFinishReason, ModelGenerationOutput, ModelGenerationResult, ModelSelection,
        ModelUsage, PackageDigest, PhysicalResourceDigest, RelativePath, WorkspaceScope,
    };
    use agl_core::{ConversationId, MessageId};
    use agl_daemon_api::AgentClient;
    use agl_runtime::inference::{
        InferenceGenerateRequest, InferenceGenerateResult, InferenceGenerator,
        InferenceServiceError,
    };
    use agl_runtime::package::{PackageId, PackageVersion};

    use super::*;

    fn root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agl-daemon-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn model_runtime() -> agl_core::agent::ModelRuntimeSelection {
        use agl_core::agent::{
            GenerationSettings, GpuLayerSelection, KvCacheType, ModelArtifactKind,
            ModelArtifactRef, ModelLoadSelection, ModelRuntimeSelection, ModelServiceSelection,
            SplitMode,
        };
        ModelRuntimeSelection {
            artifact: ModelArtifactRef {
                kind: ModelArtifactKind::Gguf,
                url: "https://example.invalid/model.gguf".into(),
                digest: test_model_digest(),
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
                engine_build_digest: PackageDigest::from_bytes([7; 32]),
            },
            adapters: vec![],
            service: ModelServiceSelection {
                key: PackageDigest::from_bytes([8; 32]),
                slots: 1,
                queue_capacity: 32,
                continuous_batching: true,
                idle_timeout_ms: 900_000,
            },
        }
    }

    fn test_model_digest() -> PackageDigest {
        "sha256:b83633aa785344791618f2fddf131b010ea04912a60430760b070bad293f65bd"
            .parse()
            .unwrap()
    }

    fn install_test_model(root: &Path) {
        let models = root.join("data/runtime/models");
        std::fs::create_dir_all(&models).unwrap();
        let digest = test_model_digest().to_string();
        let digest = digest.strip_prefix("sha256:").unwrap();
        let path = models.join(format!("{digest}.gguf"));
        std::fs::write(&path, b"GGUF").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    struct GatedGenerator {
        release: Arc<AtomicBool>,
    }

    impl InferenceGenerator for GatedGenerator {
        fn measure(
            &self,
            request: InferenceGenerateRequest,
        ) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError> {
            // This fake model has an empty token stream; no production tokenizer is substituted.
            Ok(agl_core::agent::ContextCapacity::new(
                0,
                request.generation.max_output_tokens,
                request.runtime.load.context_tokens,
            ))
        }

        fn generate(
            &self,
            request: InferenceGenerateRequest,
        ) -> Result<InferenceGenerateResult, InferenceServiceError> {
            while !self.release.load(Ordering::Acquire) {
                if request.cancellation.is_cancelled() {
                    return Err(InferenceServiceError::Cancelled);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(InferenceGenerateResult {
                operation: request.operation,
                delivery_attempt: request.delivery_attempt,
                result: ModelGenerationResult {
                    private_reasoning: None,
                    output: ModelGenerationOutput::Assistant(Content::text("complete").unwrap()),
                    finish_reason: ModelFinishReason::Stop,
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    realization: InferenceRealizationRef {
                        runtime_profile_digest: InferenceRuntimeProfileDigest::from_bytes([1; 32]),
                        engine_build_digest: InferenceEngineBuildDigest::from_bytes([2; 32]),
                        physical_resource_digest: PhysicalResourceDigest::from_bytes([3; 32]),
                    },
                    correction: None,
                },
            })
        }
    }

    fn snapshot(root: &Path) -> AgentRunSnapshot {
        AgentRunSnapshot {
            presentation: Default::default(),
            agent: AgentDefinitionRef {
                id: PackageId::new("test-agent").unwrap(),
                version: PackageVersion::new("1.0.0").unwrap(),
                digest: PackageDigest::from_bytes([4; 32]),
            },
            model: ModelSelection {
                reasoning_efforts: vec![],
                model: ModelDefinitionRef {
                    id: PackageId::new("test-model").unwrap(),
                    version: PackageVersion::new("1.0.0").unwrap(),
                    digest: PackageDigest::from_bytes([5; 32]),
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

    #[tokio::test]
    async fn manual_socket_is_private_and_only_a_stale_socket_is_replaced() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};

        let root = root("socket");
        let path = root.join("private/agl.sock");
        let listener = bind_listener(&path).await.unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.mode() & 0o777, 0o600);
        // SAFETY: geteuid has no preconditions and does not mutate process state.
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert!(bind_listener(&path).await.is_err());
        drop(listener);

        let rebound = bind_listener(&path).await.unwrap();
        drop(rebound);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"preserve").unwrap();
        assert!(bind_listener(&path).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"preserve");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscription_resynchronizes_from_durable_cursor_without_progress_replay() {
        let root = root("subscription");
        std::fs::create_dir_all(&root).unwrap();
        install_test_model(&root);
        let socket = root.join("state/daemon/agl.sock");
        let release = Arc::new(AtomicBool::new(false));
        let server = DaemonServer::start(DaemonConfig {
            data_root: root.join("data"),
            execution_socket: root.join("state/execd/execd.sock"),
            inference: InferenceConfig::custom(Arc::new(GatedGenerator {
                release: release.clone(),
            })),
            extensions: vec![],
            searxng: None,
            max_resident_bytes: None,
        })
        .unwrap();
        let conversation_id = ConversationId::generate();
        server
            .handle
            .store
            .create_conversation(
                conversation_id,
                &ExactPackageRef {
                    id: PackageId::new("test-function").unwrap(),
                    version: PackageVersion::new("1.0.0").unwrap(),
                    digest: PackageDigest::from_bytes([9; 32]),
                },
                &snapshot(&root),
            )
            .unwrap();
        let server_task = tokio::spawn(server.serve(ListenerSource::Bind(socket.clone())));
        for _ in 0..1_000 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(socket.exists());

        let client = AgentClient::new(&socket);
        let run_id = client
            .start_run(AgentRunSpec {
                reasoning: None,
                origin: AgentRunOrigin::User {
                    conversation_id,
                    message_id: MessageId::generate(),
                },
                input: Content::text("resynchronize").unwrap(),
            })
            .await
            .unwrap();
        let mut subscription = client.subscribe(run_id).await.unwrap();
        let initial_cursor = subscription.cursor();
        release.store(true, Ordering::Release);
        for _ in 0..1_000 {
            if client.run_view(run_id).await.unwrap().status == AgentRunStatus::Completed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }

        let snapshot = subscription.resynchronize(&client).await.unwrap();
        assert_eq!(snapshot.view.status, AgentRunStatus::Completed);
        assert!(subscription.cursor() > initial_cursor);
        assert!(!snapshot.events.is_empty());
        assert!(
            snapshot
                .events
                .iter()
                .all(|event| { event.agent_run_id == run_id && event.id > initial_cursor })
        );

        let mut terminal = client.subscribe(run_id).await.unwrap();
        assert_eq!(terminal.initial_view().status, AgentRunStatus::Completed);
        assert!(matches!(
            terminal.next().await.unwrap(),
            agl_daemon_api::AgentSubscriptionFrame::Ended { run_view, .. }
                if run_view.status == AgentRunStatus::Completed
        ));

        server_task.abort();
        let _ = server_task.await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn optional_searxng_errors_degrade_without_preventing_daemon_start() {
        let root = root("searxng-invalid");
        let release = Arc::new(AtomicBool::new(true));
        let credentials = root.join("bad.pem");
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(&credentials, b"not-a-certificate").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let _server = DaemonServer::start(DaemonConfig {
            data_root: root.join("data"),
            execution_socket: root.join("state/execd/execd.sock"),
            inference: InferenceConfig::custom(Arc::new(GatedGenerator {
                release: release.clone(),
            })),
            extensions: vec![],
            searxng: Some(crate::IntegrationConfig {
                required: false,
                binding: crate::SearxngConfig {
                    client_certificate: credentials.clone(),
                    client_private_key: credentials.clone(),
                    private_ca: credentials.clone(),
                },
            }),
            max_resident_bytes: None,
        })
        .expect("daemon should ignore invalid searxng credentials");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn required_searxng_error_is_a_permanent_startup_fault() {
        let root = root("searxng-required-invalid");
        let credentials = root.join("bad.pem");
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(&credentials, b"not-a-certificate").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let error = DaemonServer::start(DaemonConfig {
            data_root: root.join("data"),
            execution_socket: root.join("state/execd/execd.sock"),
            inference: InferenceConfig::custom(Arc::new(GatedGenerator {
                release: Arc::new(AtomicBool::new(true)),
            })),
            extensions: vec![],
            searxng: Some(crate::IntegrationConfig {
                required: true,
                binding: crate::SearxngConfig {
                    client_certificate: credentials.clone(),
                    client_private_key: credentials.clone(),
                    private_ca: credentials.clone(),
                },
            }),
            max_resident_bytes: None,
        })
        .err()
        .expect("required invalid integration must fail startup");
        assert!(is_permanent_startup_error(&error));
        assert!(error.to_string().contains("required integration search"));

        std::fs::remove_dir_all(root).unwrap();
    }
}
