#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agl_core::Content;
    use agl_core::ExtensionDefinition;
    use agl_core::agent::{
        AbsolutePath, AdmittedTool, AgentDefinitionRef, AgentRunLimits, AgentRunOrigin,
        AgentRunSnapshot, AuthorityGrantSet, ExactPackageRef, ExtensionDefinitionRef,
        InferenceEngineBuildDigest, InferenceRealizationRef, InferenceRuntimeProfileDigest,
        InstructionSet, ModelDefinitionRef, ModelFinishReason, ModelGenerationOutput,
        ModelGenerationResult, ModelSelection, ModelUsage, PackageDigest, PhysicalResourceDigest,
        RelativePath, WorkspaceScope,
    };
    use agl_core::{ConversationId, MessageId};
    use agl_runtime::extension::ToolHandler;
    use agl_runtime::inference::{
        InferenceConfig, InferenceGenerator, InferenceService, RestoredInferenceHealth,
    };
    use agl_runtime::package::{PackageId, PackageVersion};
    use serde_json::json;

    use super::*;
    include!("compaction_tests.rs");
    include!("effect_result_tests.rs");

    fn parse_diagnostic(raw: &str) -> agl_core::agent::ModelOutputDiagnostic {
        agl_core::agent::ModelOutputDiagnostic {
            class: agl_core::agent::ModelOutputFailureClass::Syntax,
            field: Some("raw_output.public_action".into()),
            finish_reason: Some(ModelFinishReason::Stop),
            output_bytes: raw.len() as u64,
            output_digest: PackageDigest::from_bytes(sha256(raw.as_bytes())),
        }
    }

    fn realization(byte: u8) -> InferenceRealizationRef {
        InferenceRealizationRef {
            runtime_profile_digest: InferenceRuntimeProfileDigest::from_bytes([byte; 32]),
            engine_build_digest: InferenceEngineBuildDigest::from_bytes([2; 32]),
            physical_resource_digest: PhysicalResourceDigest::from_bytes([3; 32]),
        }
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

    struct FakeGenerator(AtomicUsize);
    impl InferenceGenerator for FakeGenerator {
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
            _request: InferenceGenerateRequest,
        ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError>
        {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(agl_runtime::inference::InferenceGenerateResult {
                operation: _request.operation,
                delivery_attempt: _request.delivery_attempt,
                result: ModelGenerationResult {
                    private_reasoning: None,
                    output: ModelGenerationOutput::Assistant(Content::text("done").unwrap()),
                    finish_reason: ModelFinishReason::Stop,
                    usage: ModelUsage {
                        input_tokens: 2,
                        output_tokens: 1,
                    },
                    realization: realization(1),
                    correction: None,
                },
            })
        }
    }

    struct CancellableGenerator(AtomicBool);

    impl InferenceGenerator for CancellableGenerator {
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
        ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError>
        {
            self.0.store(true, Ordering::Release);
            for _ in 0..1_000 {
                if request.cancellation.is_cancelled() {
                    return Err(InferenceServiceError::Cancelled);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(InferenceServiceError::Unavailable)
        }
    }

    struct FlakyGenerator(AtomicUsize);

    impl InferenceGenerator for FlakyGenerator {
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
        ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError>
        {
            let attempt = self.0.fetch_add(1, Ordering::SeqCst);
            if attempt < 2 {
                return Err(InferenceServiceError::Unavailable);
            }
            Ok(agl_runtime::inference::InferenceGenerateResult {
                operation: request.operation,
                delivery_attempt: request.delivery_attempt,
                result: ModelGenerationResult {
                    private_reasoning: None,
                    output: ModelGenerationOutput::Assistant(Content::text("retried").unwrap()),
                    finish_reason: ModelFinishReason::Stop,
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    realization: realization(4),
                    correction: None,
                },
            })
        }
    }

    struct OutputCorrectingGenerator(AtomicUsize);

    impl InferenceGenerator for OutputCorrectingGenerator {
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
        ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError>
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            if request.model.id.as_str() == "model" {
                return Err(InferenceServiceError::InvalidModelOutput(Box::new(
                    agl_runtime::inference::InvalidModelOutput {
                        raw_output: Some(Content::text("<tool_call>{broken").unwrap()),
                        diagnostic: parse_diagnostic("<tool_call>{broken"),
                        usage: ModelUsage {
                            input_tokens: 3,
                            output_tokens: 4,
                        },
                        realization: Some(realization(12)),
                    },
                )));
            }
            assert_eq!(request.model.id.as_str(), "corrector");
            assert!(matches!(
                request.runtime.load.gpu_layers,
                agl_core::agent::GpuLayerSelection::Count(0)
            ));
            assert_eq!(request.generation.context.len(), request.context.len());
            assert!(
                request
                    .generation
                    .context
                    .iter()
                    .zip(&request.context)
                    .all(|(id, entry)| id == &entry.message.id)
            );
            assert!(
                request
                    .context
                    .last()
                    .unwrap()
                    .message
                    .content
                    .as_text()
                    .contains("raw_output")
            );
            Ok(agl_runtime::inference::InferenceGenerateResult {
                operation: request.operation,
                delivery_attempt: request.delivery_attempt,
                result: ModelGenerationResult {
                    private_reasoning: None,
                    output: ModelGenerationOutput::Assistant(Content::text("corrected").unwrap()),
                    finish_reason: ModelFinishReason::Stop,
                    usage: ModelUsage {
                        input_tokens: 5,
                        output_tokens: 6,
                    },
                    realization: realization(13),
                    correction: None,
                },
            })
        }
    }

    struct FailingCorrectorGenerator(AtomicUsize);

    impl InferenceGenerator for FailingCorrectorGenerator {
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
        ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError>
        {
            let call = self.0.fetch_add(1, Ordering::SeqCst);
            if request.model.id.as_str() == "corrector" {
                return Err(InferenceServiceError::InvalidModelOutput(Box::new(
                    agl_runtime::inference::InvalidModelOutput {
                        raw_output: Some(Content::text("still invalid").unwrap()),
                        diagnostic: parse_diagnostic("still invalid"),
                        usage: ModelUsage {
                            input_tokens: 2,
                            output_tokens: 3,
                        },
                        realization: Some(realization(13)),
                    },
                )));
            }
            if call == 0 {
                return Err(InferenceServiceError::InvalidModelOutput(Box::new(
                    agl_runtime::inference::InvalidModelOutput {
                        raw_output: Some(Content::text("primary invalid").unwrap()),
                        diagnostic: parse_diagnostic("primary invalid"),
                        usage: ModelUsage {
                            input_tokens: 3,
                            output_tokens: 4,
                        },
                        realization: Some(realization(12)),
                    },
                )));
            }
            assert!(
                request
                    .context
                    .last()
                    .unwrap()
                    .message
                    .content
                    .as_text()
                    .contains("invalid_model_output")
            );
            Ok(agl_runtime::inference::InferenceGenerateResult {
                operation: request.operation,
                delivery_attempt: request.delivery_attempt,
                result: ModelGenerationResult {
                    private_reasoning: None,
                    output: ModelGenerationOutput::Assistant(
                        Content::text("primary recovered").unwrap(),
                    ),
                    finish_reason: ModelFinishReason::Stop,
                    usage: ModelUsage {
                        input_tokens: 7,
                        output_tokens: 1,
                    },
                    realization: realization(1),
                    correction: None,
                },
            })
        }
    }

    struct InvalidToolGenerator;

    impl InferenceGenerator for InvalidToolGenerator {
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
        ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError>
        {
            Ok(agl_runtime::inference::InferenceGenerateResult {
                operation: request.operation,
                delivery_attempt: request.delivery_attempt,
                result: ModelGenerationResult {
                    private_reasoning: None,
                    output: ModelGenerationOutput::ToolCall(agl_core::agent::ToolCall {
                        tool_id: ToolId::new("missing.extension:tool").unwrap(),
                        input: json!({}),
                    }),
                    finish_reason: ModelFinishReason::ToolCall,
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    realization: realization(5),
                    correction: None,
                },
            })
        }
    }

    struct StreamingGenerator;

    impl InferenceGenerator for StreamingGenerator {
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
        ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError>
        {
            let progress = request.progress.as_ref().unwrap();
            progress.emit(Content::text("one").unwrap());
            progress.emit(Content::text(" two").unwrap());
            Ok(agl_runtime::inference::InferenceGenerateResult {
                operation: request.operation,
                delivery_attempt: request.delivery_attempt,
                result: ModelGenerationResult {
                    private_reasoning: None,
                    output: ModelGenerationOutput::Assistant(Content::text("one two").unwrap()),
                    finish_reason: ModelFinishReason::Stop,
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 2,
                    },
                    realization: realization(6),
                    correction: None,
                },
            })
        }
    }

    struct ToolCallingGenerator(AtomicUsize);

    impl InferenceGenerator for ToolCallingGenerator {
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
        ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError>
        {
            let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
            Ok(agl_runtime::inference::InferenceGenerateResult {
                operation: request.operation,
                delivery_attempt: request.delivery_attempt,
                result: ModelGenerationResult {
                    private_reasoning: None,
                    output: if first {
                        ModelGenerationOutput::ToolCall(agl_core::agent::ToolCall {
                            tool_id: ToolId::new("test.extension:echo").unwrap(),
                            input: json!({}),
                        })
                    } else {
                        ModelGenerationOutput::Assistant(Content::text("still alive").unwrap())
                    },
                    finish_reason: if first {
                        ModelFinishReason::ToolCall
                    } else {
                        ModelFinishReason::Stop
                    },
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    realization: realization(7),
                    correction: None,
                },
            })
        }
    }

    struct RepeatingReadGenerator(AtomicUsize);

    struct CorrectedToolGenerator {
        calls: AtomicUsize,
        corrections: usize,
        tool: agl_core::agent::ToolCall,
    }

    impl InferenceGenerator for CorrectedToolGenerator {
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
        ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError>
        {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.corrections {
                return Err(InferenceServiceError::InvalidResult);
            }
            let is_tool = call == self.corrections;
            Ok(agl_runtime::inference::InferenceGenerateResult {
                operation: request.operation,
                delivery_attempt: request.delivery_attempt,
                result: ModelGenerationResult {
                    private_reasoning: None,
                    output: if is_tool {
                        ModelGenerationOutput::ToolCall(self.tool.clone())
                    } else {
                        ModelGenerationOutput::Assistant(Content::text("recovered").unwrap())
                    },
                    finish_reason: if is_tool {
                        ModelFinishReason::ToolCall
                    } else {
                        ModelFinishReason::Stop
                    },
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    realization: realization(9),
                    correction: None,
                },
            })
        }
    }

    impl InferenceGenerator for RepeatingReadGenerator {
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
        ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError>
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(agl_runtime::inference::InferenceGenerateResult {
                operation: request.operation,
                delivery_attempt: request.delivery_attempt,
                result: ModelGenerationResult {
                    private_reasoning: None,
                    output: ModelGenerationOutput::ToolCall(agl_core::agent::ToolCall {
                        tool_id: ToolId::new("agentlibre.builtins:fs_read").unwrap(),
                        input: json!({"path":"src/lib.rs","cursor":1,"limit_lines":200}),
                    }),
                    finish_reason: ModelFinishReason::ToolCall,
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    realization: realization(9),
                    correction: None,
                },
            })
        }
    }

    struct RepeatingEchoGenerator(AtomicUsize);

    impl InferenceGenerator for RepeatingEchoGenerator {
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
        ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError>
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(agl_runtime::inference::InferenceGenerateResult {
                operation: request.operation,
                delivery_attempt: request.delivery_attempt,
                result: ModelGenerationResult {
                    private_reasoning: None,
                    output: ModelGenerationOutput::ToolCall(agl_core::agent::ToolCall {
                        tool_id: ToolId::new("test.extension:echo").unwrap(),
                        input: json!({}),
                    }),
                    finish_reason: ModelFinishReason::ToolCall,
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    realization: realization(11),
                    correction: None,
                },
            })
        }
    }

    struct CorrectingReadGenerator(AtomicUsize);

    impl InferenceGenerator for CorrectingReadGenerator {
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
        ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError>
        {
            let call = self.0.fetch_add(1, Ordering::SeqCst);
            let (output, finish_reason) = if call < 4 {
                let cursor = if call == 2 { 201 } else { 1 };
                (
                    ModelGenerationOutput::ToolCall(agl_core::agent::ToolCall {
                        tool_id: ToolId::new("agentlibre.builtins:fs_read").unwrap(),
                        input: json!({"path":"src/lib.rs","cursor":cursor,"limit_lines":200}),
                    }),
                    ModelFinishReason::ToolCall,
                )
            } else {
                (
                    ModelGenerationOutput::Assistant(Content::text("done").unwrap()),
                    ModelFinishReason::Stop,
                )
            };
            Ok(agl_runtime::inference::InferenceGenerateResult {
                operation: request.operation,
                delivery_attempt: request.delivery_attempt,
                result: ModelGenerationResult {
                    private_reasoning: None,
                    output,
                    finish_reason,
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    realization: realization(10),
                    correction: None,
                },
            })
        }
    }

    struct PreservedReasoningGenerator {
        calls: AtomicUsize,
        observed_follow_up: AtomicBool,
    }

    impl InferenceGenerator for PreservedReasoningGenerator {
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
        ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError>
        {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 2 {
                let reasoning = request
                    .context
                    .iter()
                    .filter_map(|entry| entry.private_reasoning.as_ref())
                    .map(|content| content.as_text())
                    .collect::<Vec<_>>();
                self.observed_follow_up
                    .store(reasoning == ["tool plan", "final plan"], Ordering::Release);
            }
            let (output, private_reasoning, finish_reason) = if call == 0 {
                (
                    ModelGenerationOutput::ToolCall(agl_core::agent::ToolCall {
                        tool_id: ToolId::new("test.extension:echo").unwrap(),
                        input: json!({}),
                    }),
                    Some(Content::text("tool plan").unwrap()),
                    ModelFinishReason::ToolCall,
                )
            } else {
                (
                    ModelGenerationOutput::Assistant(Content::text("done").unwrap()),
                    (call == 1).then(|| Content::text("final plan").unwrap()),
                    ModelFinishReason::Stop,
                )
            };
            Ok(agl_runtime::inference::InferenceGenerateResult {
                operation: request.operation,
                delivery_attempt: request.delivery_attempt,
                result: ModelGenerationResult {
                    output,
                    private_reasoning,
                    finish_reason,
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    realization: realization(8),
                    correction: None,
                },
            })
        }
    }

    struct EchoTool;

    impl ToolHandler for EchoTool {
        fn call(&self, _context: ToolContext, _input: serde_json::Value) -> ToolFuture {
            Box::pin(async {
                Ok(agl_core::agent::ToolResult {
                    content: Content::text("ok").unwrap(),
                    effect_receipts: vec![],
                })
            })
        }
    }

    struct CountingReadTool(Arc<AtomicUsize>);

    impl ToolHandler for CountingReadTool {
        fn call(&self, _context: ToolContext, _input: serde_json::Value) -> ToolFuture {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(agl_core::agent::ToolResult {
                    content: Content::text(
                        json!({"lines":[{"line":1,"text":"content"}],"next_cursor":201})
                            .to_string(),
                    )
                    .unwrap(),
                    effect_receipts: vec![],
                })
            })
        }
    }

    struct PanickingTool;

    impl ToolHandler for PanickingTool {
        fn call(&self, _context: ToolContext, _input: serde_json::Value) -> ToolFuture {
            panic!("intentional Tool handler panic")
        }
    }

    struct InvalidInputTool;

    struct FailingTool {
        failure: agl_core::agent::ToolFailure,
        calls: Arc<AtomicUsize>,
    }

    impl ToolHandler for FailingTool {
        fn call(&self, _context: ToolContext, _input: serde_json::Value) -> ToolFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let failure = self.failure.clone();
            Box::pin(async move { Err(failure) })
        }
    }

    impl ToolHandler for InvalidInputTool {
        fn call(&self, _context: ToolContext, _input: serde_json::Value) -> ToolFuture {
            Box::pin(async {
                Err(agl_core::agent::ToolFailure::no_effect(
                    ToolFailureKind::InvalidInput,
                    Some("input"),
                    &["Correct the input before submitting a new call."],
                ))
            })
        }
    }

    fn test_extension(description: &str) -> (ExtensionBindings, AdmittedTool) {
        test_extension_with_handler(
            description,
            agl_core::agent::DeliveryClass::Retryable,
            Arc::new(EchoTool),
        )
    }

    #[test]
    fn every_tool_result_is_rejected_above_the_run_byte_limit() {
        let root = std::env::temp_dir().join(format!("agl-tool-bytes-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        let (bindings, admitted) = test_extension("bounded result");
        let prepared = PreparedTool {
            binding: bindings.tools[0].clone(),
            definition: admitted.definition.clone(),
            input_validator: Arc::new(
                jsonschema::validator_for(admitted.definition.input_schema.as_value()).unwrap(),
            ),
            extension_id: admitted.extension.id.clone(),
            extension_version: admitted.extension.package.version.clone(),
            extension_digest: admitted.extension.definition_digest,
            effect_validators: BTreeMap::new(),
        };
        let mut configured = snapshot(&root);
        configured.limits.tool_result_bytes = 32;
        let failure = validate_tool_result(
            &prepared,
            &configured,
            &agl_core::agent::ToolResult {
                content: Content::text("x".repeat(128)).unwrap(),
                effect_receipts: vec![],
            },
        )
        .unwrap_err();
        assert_eq!(failure.kind, AgentOperationFailureKind::ResultTooLarge);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tool_receipt_accepts_only_the_exact_realized_workspace_scope() {
        let root =
            std::env::temp_dir().join(format!("agl-tool-realized-scope-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        let (bindings, mut admitted) = test_extension("realized scope");
        let effect = EffectId::new("test.extension:execute").unwrap();
        admitted.definition.required_effects = vec![effect.clone()];
        let schema = JsonSchema::new(json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["root", "executables"],
            "properties": {
                "root": {"const": "workspace"},
                "executables": {"type": "array", "items": {"type": "string"}}
            }
        }))
        .unwrap();
        let effect_validator =
            Arc::new(jsonschema::validator_for(schema.as_value()).unwrap());
        let input_validator = Arc::new(
            jsonschema::validator_for(admitted.definition.input_schema.as_value()).unwrap(),
        );
        let prepared = PreparedTool {
            binding: bindings.tools[0].clone(),
            definition: admitted.definition,
            input_validator,
            extension_id: admitted.extension.id,
            extension_version: admitted.extension.package.version,
            extension_digest: admitted.extension.definition_digest,
            effect_validators: BTreeMap::from([(effect.clone(), effect_validator)]),
        };
        let scope = CanonicalJson::new(json!({
            "root": root.to_string_lossy(),
            "executables": ["cargo"]
        }))
        .unwrap();
        let mut configured = snapshot(&root);
        configured.authority = AuthorityGrantSet(vec![agl_core::AuthorityGrant {
            effect: effect.clone(),
            scope: scope.clone(),
        }]);
        let result = agl_core::agent::ToolResult {
            content: Content::text("ok").unwrap(),
            effect_receipts: vec![agl_core::agent::EffectReceipt { effect, scope }],
        };

        validate_tool_result(&prepared, &configured, &result).unwrap();

        std::fs::remove_dir_all(root).unwrap();
    }

    fn test_extension_with_handler(
        description: &str,
        delivery: agl_core::agent::DeliveryClass,
        handler: Arc<dyn ToolHandler>,
    ) -> (ExtensionBindings, AdmittedTool) {
        let definition = ToolDefinition {
            id: ToolId::new("test.extension:echo").unwrap(),
            description: description.into(),
            input_schema: JsonSchema::new(json!({
                "type": "object",
                "additionalProperties": false
            }))
            .unwrap(),
            required_effects: vec![],
            delivery,
        };
        let extension = ExtensionDefinition {
            id: ExtensionId::new("test.extension").unwrap(),
            effects: vec![],
            tools: vec![definition.clone()],
        };
        let definition_digest =
            ToolDefinitionDigest::from_bytes(sha256(&serde_json::to_vec(&definition).unwrap()));
        let extension_digest =
            ExtensionDefinitionDigest::from_bytes(sha256(&serde_json::to_vec(&extension).unwrap()));
        (
            ExtensionBindings {
                version: PackageVersion::new("1.0.0").unwrap(),
                content_digest: agl_runtime::package::PackageTreeDigest::new(format!(
                    "sha256:{}",
                    "09".repeat(32)
                ))
                .unwrap(),
                definition: extension.clone(),
                tools: vec![ToolBinding {
                    tool_id: definition.id.clone(),
                    definition_digest,
                    handler,
                }],
                allows_authority: Arc::new(|_| true),
            },
            AdmittedTool {
                definition,
                extension: ExtensionDefinitionRef {
                    id: extension.id,
                    package: ExactPackageRef {
                        id: PackageId::new("test.extension").unwrap(),
                        version: PackageVersion::new("1.0.0").unwrap(),
                        digest: PackageDigest::from_bytes([9; 32]),
                    },
                    definition_digest: extension_digest,
                },
                definition_digest,
            },
        )
    }

    fn guarded_read_extension(handler: Arc<dyn ToolHandler>) -> (ExtensionBindings, AdmittedTool) {
        let definition = ToolDefinition {
            id: ToolId::new("agentlibre.builtins:fs_read").unwrap(),
            description: "read one explicit page".into(),
            input_schema: JsonSchema::new(json!({
                "type":"object",
                "additionalProperties":false,
                "required":["path","cursor","limit_lines"],
                "properties":{
                    "path":{"type":"string"},
                    "cursor":{"type":"integer","minimum":1},
                    "limit_lines":{"type":"integer","minimum":1,"maximum":500}
                }
            }))
            .unwrap(),
            required_effects: vec![],
            delivery: agl_core::agent::DeliveryClass::Retryable,
        };
        let extension = ExtensionDefinition {
            id: ExtensionId::new("agentlibre.builtins").unwrap(),
            effects: vec![],
            tools: vec![definition.clone()],
        };
        let definition_digest =
            ToolDefinitionDigest::from_bytes(sha256(&serde_json::to_vec(&definition).unwrap()));
        let extension_digest =
            ExtensionDefinitionDigest::from_bytes(sha256(&serde_json::to_vec(&extension).unwrap()));
        (
            ExtensionBindings {
                version: PackageVersion::new("1.0.0").unwrap(),
                content_digest: agl_runtime::package::PackageTreeDigest::new(format!(
                    "sha256:{}",
                    "10".repeat(32)
                ))
                .unwrap(),
                definition: extension.clone(),
                tools: vec![ToolBinding {
                    tool_id: definition.id.clone(),
                    definition_digest,
                    handler,
                }],
                allows_authority: Arc::new(|_| true),
            },
            AdmittedTool {
                definition,
                extension: ExtensionDefinitionRef {
                    id: extension.id,
                    package: ExactPackageRef {
                        id: PackageId::new("agentlibre.builtins").unwrap(),
                        version: PackageVersion::new("1.0.0").unwrap(),
                        digest: PackageDigest::from_bytes([10; 32]),
                    },
                    definition_digest: extension_digest,
                },
                definition_digest,
            },
        )
    }

    fn snapshot(root: &std::path::Path) -> AgentRunSnapshot {
        AgentRunSnapshot {
            presentation: Default::default(),
            agent: AgentDefinitionRef {
                id: PackageId::new("agent").unwrap(),
                version: PackageVersion::new("1.0.0").unwrap(),
                digest: PackageDigest::from_bytes([1; 32]),
            },
            model: ModelSelection {
                reasoning_efforts: vec![],
                model: ModelDefinitionRef {
                    id: PackageId::new("model").unwrap(),
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

    fn bind_conversation(
        store: &StoreHandle,
        conversation_id: ConversationId,
        snapshot: &AgentRunSnapshot,
    ) {
        store
            .create_conversation(conversation_id, &function_ref(), snapshot)
            .unwrap();
    }

    #[test]
    fn corrected_model_output_does_not_break_tool_dispatch_or_no_effect_recovery() {
        for corrections in [1, 2] {
            for read in [true, false] {
                let root = std::env::temp_dir().join(format!(
                    "agl-corrected-tool-{corrections}-{read}-{}",
                    uuid::Uuid::now_v7()
                ));
                let store = StoreHandle::open_at(&root).unwrap();
                let conversation_id = ConversationId::generate();
                let handler_calls = Arc::new(AtomicUsize::new(0));
                let (bindings, admitted) = if read {
                    guarded_read_extension(Arc::new(CountingReadTool(handler_calls.clone())))
                } else {
                    test_extension_with_handler(
                        "known no-effect failure after correction",
                        agl_core::agent::DeliveryClass::AtMostOnce,
                        Arc::new(InvalidInputTool),
                    )
                };
                let generator = Arc::new(CorrectedToolGenerator {
                    calls: AtomicUsize::new(0),
                    corrections,
                    tool: agl_core::agent::ToolCall {
                        tool_id: admitted.definition.id.clone(),
                        input: if read {
                            json!({"path":"src/lib.rs","cursor":1,"limit_lines":200})
                        } else {
                            json!({})
                        },
                    },
                });
                let mut configured = snapshot(&root);
                configured.tools.push(admitted);
                configured.limits.model_calls = corrections as u64 + 2;
                bind_conversation(&store, conversation_id, &configured);
                let (inference_service, inference) = InferenceService::start(
                    InferenceConfig::custom(generator.clone()),
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
                        input: Content::text("correct the output, then use the Tool").unwrap(),
                    })
                    .unwrap();
                for _ in 0..1_000 {
                    if store.agent_run_view(run_id).unwrap().status.is_terminal() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                let view = store.agent_run_view(run_id).unwrap();
                assert_eq!(
                    view.status,
                    AgentRunStatus::Completed,
                    "corrections={corrections} read={read}: {view:?}"
                );
                assert_eq!(view.usage.tool_calls, 1);
                assert_eq!(generator.calls.load(Ordering::SeqCst), corrections + 2);
                if read {
                    assert_eq!(handler_calls.load(Ordering::SeqCst), 1);
                }
                let tool_key = agl_core::agent::AgentOperationKey {
                    run_id,
                    ordinal: NonZeroU32::new(corrections as u32 + 2).unwrap(),
                };
                assert!(store.preceding_tool_operation(&tool_key).unwrap().is_none());
                service.shutdown();
                inference_service.shutdown();
                drop(store);
                let reopened = StoreHandle::open_at(&root).unwrap();
                assert!(
                    reopened
                        .preceding_tool_operation(&tool_key)
                        .unwrap()
                        .is_none()
                );
                assert_eq!(
                    reopened.agent_run_view(run_id).unwrap().status,
                    AgentRunStatus::Completed
                );
                drop(reopened);
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }

    #[test]
    fn consecutive_read_duplicates_get_one_correction_then_fail_durably() {
        let root =
            std::env::temp_dir().join(format!("agl-daemon-tool-loop-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        let handler_calls = Arc::new(AtomicUsize::new(0));
        let (bindings, admitted) =
            guarded_read_extension(Arc::new(CountingReadTool(handler_calls.clone())));
        let mut configured = snapshot(&root);
        configured.tools.push(admitted);
        configured.limits.model_calls = 4;
        configured.limits.tool_calls = 4;
        bind_conversation(&store, conversation_id, &configured);
        let generator = Arc::new(RepeatingReadGenerator(AtomicUsize::new(0)));
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(generator.clone()),
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
                input: Content::text("repeat the same read").unwrap(),
            })
            .unwrap();
        for _ in 0..1_000 {
            if store.agent_run_view(run_id).unwrap().status.is_terminal() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let view = store.agent_run_view(run_id).unwrap();
        assert_eq!(view.status, AgentRunStatus::Failed);
        assert_eq!(view.usage.model_calls, 3);
        assert_eq!(view.usage.tool_calls, 2);
        assert_eq!(handler_calls.load(Ordering::SeqCst), 1);
        assert_eq!(generator.0.load(Ordering::SeqCst), 3);
        assert!(matches!(
            view.failure,
            Some(agl_core::agent::AgentRunFailureView::Operation {
                kind: AgentOperationFailureKind::ToolLoopDetected,
                ..
            })
        ));

        let correction = store
            .agent_operation(&agl_core::agent::AgentOperationKey {
                run_id,
                ordinal: NonZeroU32::new(4).unwrap(),
            })
            .unwrap();
        let Some(AgentOperationResult::Tool(correction)) = correction.result else {
            panic!("first duplicate must produce a corrective Tool result");
        };
        let correction: serde_json::Value =
            serde_json::from_str(correction.content.as_text()).unwrap();
        assert_eq!(correction["kind"], "duplicate_tool_call");
        assert_eq!(correction["previous_operation"], 2);
        assert_eq!(correction["next_cursor"], 201);

        let events = store.agent_event_page(None, 1_000).unwrap();
        assert!(events.events.iter().any(|event| {
            event.agent_run_id == run_id
                && matches!(
                    event.data,
                    agl_core::agent::AgentEventData::RunStatusChanged {
                        to: AgentRunStatus::Failed,
                        failure_kind: Some(agl_core::agent::AgentRunFailureKind::ToolLoopDetected),
                        ..
                    }
                )
        }));

        service.shutdown();
        inference_service.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn side_effect_free_tool_failure_is_returned_to_the_model() {
        for kind in [
            ToolFailureKind::InvalidInput,
            ToolFailureKind::Unauthorized,
            ToolFailureKind::Unavailable,
            ToolFailureKind::ResultTooLarge,
            ToolFailureKind::Execution,
        ] {
            for no_effect in [true, false] {
                assert_effect_aware_failure(kind, no_effect);
            }
        }
    }

    fn assert_effect_aware_failure(kind: ToolFailureKind, no_effect: bool) {
        let root = std::env::temp_dir().join(format!(
            "agl-daemon-tool-request-repair-{}",
            uuid::Uuid::now_v7()
        ));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        let calls = Arc::new(AtomicUsize::new(0));
        let failure = if no_effect {
            agl_core::agent::ToolFailure::no_effect_with_details(
                kind,
                Some("fixture_field"),
                &["Submit a corrected fixture call."],
                CanonicalJson::new(json!({
                    "state": "exited",
                    "outcome": {"type": "exit", "code": 7}
                }))
                .unwrap(),
            )
        } else {
            agl_core::agent::ToolFailure::unknown(kind)
        };
        let (bindings, admitted) = test_extension_with_handler(
            "effect-aware rejection",
            agl_core::agent::DeliveryClass::Retryable,
            Arc::new(FailingTool {
                failure,
                calls: calls.clone(),
            }),
        );
        let mut configured = snapshot(&root);
        configured.tools.push(admitted);
        bind_conversation(&store, conversation_id, &configured);
        let generator = Arc::new(ToolCallingGenerator(AtomicUsize::new(0)));
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(generator),
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
                input: Content::text("repair the Tool request").unwrap(),
            })
            .unwrap();
        for _ in 0..1_000 {
            if store.agent_run_view(run_id).unwrap().status.is_terminal() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let view = store.agent_run_view(run_id).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let expected_status = if no_effect {
            AgentRunStatus::Completed
        } else {
            AgentRunStatus::Failed
        };
        assert_eq!(
            view.status, expected_status,
            "kind={kind:?} no_effect={no_effect}: {view:?}"
        );
        let correction = store
            .agent_operation(&agl_core::agent::AgentOperationKey {
                run_id,
                ordinal: NonZeroU32::new(2).unwrap(),
            })
            .unwrap();
        assert_eq!(correction.delivery_attempt.get(), 1);
        if no_effect {
            assert_eq!(view.usage.tool_calls, 1);
            let AgentOperationResult::Tool(result) = correction.result.unwrap() else {
                panic!("expected a model-visible Tool diagnostic");
            };
            let value: serde_json::Value = serde_json::from_str(result.content.as_text()).unwrap();
            assert_eq!(value["kind"], "tool_request_failure");
            assert_eq!(value["failure"], serde_json::to_value(kind).unwrap());
            assert_eq!(value["effect"], "none");
            assert_eq!(value["field"], "fixture_field");
            assert_eq!(value["details"]["state"], "exited");
            assert_eq!(value["details"]["outcome"]["code"], 7);
            assert_eq!(value["next_actions"][0], "Submit a corrected fixture call.");
            assert_eq!(value["attempt"], 1);
            assert!(
                value["instruction"]
                    .as_str()
                    .unwrap()
                    .contains("Do not repeat")
            );
            assert!(result.effect_receipts.is_empty());
        } else {
            assert!(correction.result.is_none());
            assert_eq!(
                correction.failure.unwrap().kind,
                agl_core::agent::AgentOperationFailureKind::OutcomeUnknown
            );
        }

        service.shutdown();
        inference_service.shutdown();
        drop(store);
        let reopened = StoreHandle::open_at(&root).unwrap();
        assert_eq!(
            reopened.agent_run_view(run_id).unwrap().status,
            expected_status
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn optional_cpu_model_corrects_invalid_primary_output_and_has_separate_usage() {
        let root = std::env::temp_dir().join(format!(
            "agl-daemon-output-correction-{}",
            uuid::Uuid::now_v7()
        ));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        let mut configured = snapshot(&root);
        let mut correction_runtime = model_runtime();
        correction_runtime.service.key = PackageDigest::from_bytes([14; 32]);
        configured.invalid_model_output_recovery =
            Some(agl_core::agent::InvalidModelOutputRecoverySelection {
                model: ModelSelection {
                    reasoning_efforts: vec![],
                    model: ModelDefinitionRef {
                        id: PackageId::new("corrector").unwrap(),
                        version: PackageVersion::new("1.0.0").unwrap(),
                        digest: PackageDigest::from_bytes([15; 32]),
                    },
                    runtime: correction_runtime,
                },
                max_attempts: 2,
            });
        bind_conversation(&store, conversation_id, &configured);
        let generator = Arc::new(OutputCorrectingGenerator(AtomicUsize::new(0)));
        let calls = Arc::clone(&generator);
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(generator),
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
        let run_id = handle
            .start_run(AgentRunSpec {
                reasoning: None,
                origin: AgentRunOrigin::User {
                    conversation_id,
                    message_id: MessageId::generate(),
                },
                input: Content::text("correct the malformed action").unwrap(),
            })
            .unwrap();
        for _ in 0..1_000 {
            if store.agent_run_view(run_id).unwrap().status.is_terminal() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let view = store.agent_run_view(run_id).unwrap();
        assert_eq!(view.status, AgentRunStatus::Completed);
        assert_eq!(calls.0.load(Ordering::SeqCst), 2);
        assert_eq!(view.usage.model_calls, 1);
        assert_eq!(view.usage.model_input_tokens, 3);
        assert_eq!(view.usage.model_output_tokens, 4);
        assert_eq!(view.usage.correction_calls, 1);
        assert_eq!(view.usage.correction_input_tokens, 5);
        assert_eq!(view.usage.correction_output_tokens, 6);

        let rejected = assert_parse_diagnostics(&store, run_id, &["<tool_call>{broken"]);
        let key = agl_core::agent::AgentOperationKey {
            run_id,
            ordinal: NonZeroU32::new(1).unwrap(),
        };
        let operation = store.agent_operation(&key).unwrap();
        let Some(AgentOperationResult::ModelGeneration(result)) = &operation.result else {
            panic!("missing corrected result")
        };
        assert_eq!(result.correction.as_ref().unwrap().attempts, 1);

        service.shutdown();
        inference_service.shutdown();
        drop(handle);
        drop(store);
        let reopened = StoreHandle::open_at(&root).unwrap();
        assert_eq!(
            assert_parse_diagnostics(&reopened, run_id, &["<tool_call>{broken"]),
            rejected
        );
        assert_eq!(reopened.agent_operation(&key).unwrap(), operation);
        drop(reopened);
        let _ = std::fs::remove_dir_all(root);
    }

    fn assert_parse_diagnostics(
        store: &StoreHandle,
        run_id: AgentRunId,
        raw: &[&str],
    ) -> Vec<agl_core::agent::AgentEvent> {
        let events: Vec<_> = store
            .agent_event_page(None, 1000)
            .unwrap()
            .events
            .into_iter()
            .filter(|event| {
                event.agent_run_id == run_id
                    && matches!(
                        event.data,
                        agl_core::agent::AgentEventData::ModelOutputRejected { .. }
                    )
            })
            .collect();
        assert_eq!(events.len(), raw.len());
        for (attempt, (event, raw)) in events.iter().zip(raw).enumerate() {
            let agl_core::agent::AgentEventData::ModelOutputRejected {
                key,
                delivery_attempt,
                correction_attempt,
                diagnostic,
                realization: observed,
                ..
            } = &event.data
            else {
                unreachable!()
            };
            assert_eq!(*correction_attempt, attempt as u32);
            assert_eq!(delivery_attempt.get(), 1);
            assert_eq!(key.ordinal.get(), 1);
            assert_eq!(event.operation.as_ref(), Some(key));
            assert_eq!(*diagnostic, parse_diagnostic(raw));
            assert_eq!(
                store
                    .model_output_rejection_content(event.id)
                    .unwrap()
                    .as_ref()
                    .map(Content::as_text),
                Some(*raw)
            );
            assert_eq!(
                *observed,
                Some(realization(if attempt == 0 { 12 } else { 13 }))
            );
            assert!(!serde_json::to_string(event).unwrap().contains(raw));
        }
        events
    }

    #[test]
    fn failed_corrector_is_bounded_and_falls_back_to_primary_model_recovery() {
        let root = std::env::temp_dir().join(format!(
            "agl-daemon-output-correction-fallback-{}",
            uuid::Uuid::now_v7()
        ));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        let mut configured = snapshot(&root);
        configured.limits.model_calls = 3;
        let mut correction_runtime = model_runtime();
        correction_runtime.service.key = PackageDigest::from_bytes([14; 32]);
        configured.invalid_model_output_recovery =
            Some(agl_core::agent::InvalidModelOutputRecoverySelection {
                model: ModelSelection {
                    reasoning_efforts: vec![],
                    model: ModelDefinitionRef {
                        id: PackageId::new("corrector").unwrap(),
                        version: PackageVersion::new("1.0.0").unwrap(),
                        digest: PackageDigest::from_bytes([15; 32]),
                    },
                    runtime: correction_runtime,
                },
                max_attempts: 2,
            });
        bind_conversation(&store, conversation_id, &configured);
        let generator = Arc::new(FailingCorrectorGenerator(AtomicUsize::new(0)));
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(generator),
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
        let run_id = handle
            .start_run(AgentRunSpec {
                reasoning: None,
                origin: AgentRunOrigin::User {
                    conversation_id,
                    message_id: MessageId::generate(),
                },
                input: Content::text("recover after corrector failure").unwrap(),
            })
            .unwrap();
        for _ in 0..1_000 {
            if store.agent_run_view(run_id).unwrap().status.is_terminal() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let view = store.agent_run_view(run_id).unwrap();
        assert_eq!(view.status, AgentRunStatus::Completed, "{view:?}");
        assert_eq!(view.usage.model_calls, 2);
        assert_eq!(view.usage.correction_calls, 2);
        assert_eq!(view.usage.model_input_tokens, 10);
        assert_eq!(view.usage.model_output_tokens, 5);
        assert_eq!(view.usage.correction_input_tokens, 4);
        assert_eq!(view.usage.correction_output_tokens, 6);
        let rejected = assert_parse_diagnostics(
            &store,
            run_id,
            &["primary invalid", "still invalid", "still invalid"],
        );
        let key = agl_core::agent::AgentOperationKey {
            run_id,
            ordinal: NonZeroU32::new(1).unwrap(),
        };
        let operation = store.agent_operation(&key).unwrap();
        assert!(matches!(
            operation.failure.as_ref().unwrap().kind,
            AgentOperationFailureKind::CorrectionFailed {
                calls: 2,
                primary_usage: ModelUsage {
                    input_tokens: 3,
                    output_tokens: 4
                },
                ..
            }
        ));

        service.shutdown();
        inference_service.shutdown();
        drop(handle);
        drop(store);
        let reopened = StoreHandle::open_at(&root).unwrap();
        assert_eq!(
            assert_parse_diagnostics(
                &reopened,
                run_id,
                &["primary invalid", "still invalid", "still invalid"]
            ),
            rejected
        );
        assert_eq!(reopened.agent_operation(&key).unwrap(), operation);
        drop(reopened);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn fourth_consecutive_tool_request_failure_stops_the_loop() {
        let root = std::env::temp_dir().join(format!(
            "agl-daemon-tool-request-loop-{}",
            uuid::Uuid::now_v7()
        ));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        let (bindings, admitted) = test_extension_with_handler(
            "bound invalid input",
            agl_core::agent::DeliveryClass::AtMostOnce,
            Arc::new(InvalidInputTool),
        );
        let mut configured = snapshot(&root);
        configured.tools.push(admitted);
        configured.limits.model_calls = 5;
        configured.limits.tool_calls = 5;
        bind_conversation(&store, conversation_id, &configured);
        let generator = Arc::new(RepeatingEchoGenerator(AtomicUsize::new(0)));
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(generator),
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
                input: Content::text("repeat invalid requests").unwrap(),
            })
            .unwrap();
        for _ in 0..1_000 {
            if store.agent_run_view(run_id).unwrap().status.is_terminal() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let view = store.agent_run_view(run_id).unwrap();
        assert_eq!(view.status, AgentRunStatus::Failed);
        assert_eq!(view.usage.tool_calls, 3);
        assert!(matches!(
            view.failure,
            Some(agl_core::agent::AgentRunFailureView::Operation {
                kind: AgentOperationFailureKind::ToolLoopDetected,
                ..
            })
        ));

        service.shutdown();
        inference_service.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn changed_read_cursor_resets_the_duplicate_sequence() {
        let root = std::env::temp_dir().join(format!(
            "agl-daemon-tool-loop-reset-{}",
            uuid::Uuid::now_v7()
        ));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        let handler_calls = Arc::new(AtomicUsize::new(0));
        let (bindings, admitted) =
            guarded_read_extension(Arc::new(CountingReadTool(handler_calls.clone())));
        let mut configured = snapshot(&root);
        configured.tools.push(admitted);
        configured.limits.model_calls = 6;
        configured.limits.tool_calls = 6;
        bind_conversation(&store, conversation_id, &configured);
        let generator = Arc::new(CorrectingReadGenerator(AtomicUsize::new(0)));
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(generator.clone()),
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
                input: Content::text("correct the cursor").unwrap(),
            })
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if store.agent_run_view(run_id).unwrap().status.is_terminal() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let view = store.agent_run_view(run_id).unwrap();
        assert_eq!(view.status, AgentRunStatus::Completed);
        assert_eq!(view.usage.model_calls, 5);
        assert_eq!(view.usage.tool_calls, 4);
        assert_eq!(handler_calls.load(Ordering::SeqCst), 3);

        service.shutdown();
        inference_service.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn long_lived_service_executes_and_exact_replay_does_not_regenerate() {
        let root =
            std::env::temp_dir().join(format!("agl-daemon-agent-service-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        bind_conversation(&store, conversation_id, &snapshot(&root));
        let generator = Arc::new(FakeGenerator(AtomicUsize::new(0)));
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(generator.clone()),
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
        let spec = AgentRunSpec {
            reasoning: None,
            origin: AgentRunOrigin::User {
                conversation_id,
                message_id: MessageId::generate(),
            },
            input: Content::text("hello").unwrap(),
        };
        let first = handle.start_run(spec.clone()).unwrap();
        let mut changed_source = snapshot(&root);
        changed_source.workspace.root = AbsolutePath::try_from(
            root.join("configuration-changed-after-admission")
                .to_string_lossy()
                .into_owned(),
        )
        .unwrap();
        assert!(
            store
                .create_conversation(conversation_id, &function_ref(), &changed_source)
                .is_err()
        );
        let replay = handle.start_run(spec).unwrap();
        assert_eq!(first, replay);
        for _ in 0..100 {
            if store.agent_run_view(first).unwrap().status == AgentRunStatus::Completed {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(generator.0.load(Ordering::Relaxed), 1);
        service.shutdown();
        inference_service.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn preserved_reasoning_survives_tool_loop_and_follow_up_run_privately() {
        let root =
            std::env::temp_dir().join(format!("agl-daemon-reasoning-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        let (bindings, admitted) = test_extension("reasoning context");
        let mut configured = snapshot(&root);
        configured.tools.push(admitted);
        configured.model.runtime.reasoning = agl_core::agent::ReasoningSelection::Enabled {
            max_tokens: 16,
            effort: None,
            preserve: true,
        };
        bind_conversation(&store, conversation_id, &configured);
        let generator = Arc::new(PreservedReasoningGenerator {
            calls: AtomicUsize::new(0),
            observed_follow_up: AtomicBool::new(false),
        });
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(generator.clone()),
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
        for prompt in ["first", "follow-up"] {
            let run_id = handle
                .start_run(AgentRunSpec {
                    reasoning: None,
                    origin: AgentRunOrigin::User {
                        conversation_id,
                        message_id: MessageId::generate(),
                    },
                    input: Content::text(prompt).unwrap(),
                })
                .unwrap();
            for _ in 0..1_000 {
                if store.agent_run_view(run_id).unwrap().status.is_terminal() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert_eq!(
                store.agent_run_view(run_id).unwrap().status,
                AgentRunStatus::Completed
            );
        }
        assert!(generator.observed_follow_up.load(Ordering::Acquire));
        assert!(
            store
                .conversation_messages(conversation_id, None, 100)
                .unwrap()
                .messages
                .iter()
                .all(|message| !message.content.as_text().contains("plan"))
        );
        service.shutdown();
        inference_service.shutdown();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cumulative_limit_failure_is_exposed_as_a_run_failure() {
        let root = std::env::temp_dir().join(format!("agl-daemon-limit-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        let mut configured = snapshot(&root);
        configured.limits.model_input_tokens = Some(1);
        bind_conversation(&store, conversation_id, &configured);
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(Arc::new(FakeGenerator(AtomicUsize::new(0)))),
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
        let run_id = handle
            .start_run(AgentRunSpec {
                reasoning: None,
                origin: AgentRunOrigin::User {
                    conversation_id,
                    message_id: MessageId::generate(),
                },
                input: Content::text("exceed input limit").unwrap(),
            })
            .unwrap();
        for _ in 0..1_000 {
            if store.agent_run_view(run_id).unwrap().status.is_terminal() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            store.agent_run_view(run_id).unwrap().failure,
            Some(agl_core::agent::AgentRunFailureView::Run {
                kind: agl_core::agent::AgentRunFailureKind::LimitsExceeded,
            })
        );
        service.shutdown();
        inference_service.shutdown();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_stale_and_duplicate_bindings_fail_before_admission() {
        let root =
            std::env::temp_dir().join(format!("agl-daemon-bindings-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        let (matching, admitted) = test_extension("exact definition");
        let mut configured = snapshot(&root);
        configured.tools.push(admitted);
        bind_conversation(&store, conversation_id, &configured);
        let generator = Arc::new(FakeGenerator(AtomicUsize::new(0)));
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(generator),
            RestoredInferenceHealth::default(),
        )
        .unwrap();
        let dependencies = AgentDependencies {
            store: store.clone(),
            inference: Arc::new(inference),
        };
        let spec = || AgentRunSpec {
            reasoning: None,
            origin: AgentRunOrigin::User {
                conversation_id,
                message_id: MessageId::generate(),
            },
            input: Content::text("binding check").unwrap(),
        };

        let (missing_service, missing) = AgentService::start(dependencies.clone(), vec![]).unwrap();
        assert!(matches!(
            missing.start_run(spec()),
            Err(AgentServiceError::InvalidBindings)
        ));
        missing_service.shutdown();
        assert!(store.recoverable_agent_runs(0, 1_000).unwrap().is_empty());

        let (stale, _) = test_extension("changed definition");
        let (stale_service, stale_handle) =
            AgentService::start(dependencies.clone(), vec![stale]).unwrap();
        assert!(matches!(
            stale_handle.start_run(spec()),
            Err(AgentServiceError::InvalidBindings)
        ));
        stale_service.shutdown();
        assert!(store.recoverable_agent_runs(0, 1_000).unwrap().is_empty());

        assert!(matches!(
            AgentService::start(dependencies, vec![matching.clone(), matching]),
            Err(AgentServiceError::InvalidBindings)
        ));
        assert!(store.recoverable_agent_runs(0, 1_000).unwrap().is_empty());

        inference_service.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn startup_recovers_a_running_retryable_operation_from_store() {
        let root =
            std::env::temp_dir().join(format!("agl-daemon-recovery-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        let run_snapshot = snapshot(&root);
        bind_conversation(&store, conversation_id, &run_snapshot);
        let spec = AgentRunSpec {
            reasoning: None,
            origin: AgentRunOrigin::User {
                conversation_id,
                message_id: MessageId::generate(),
            },
            input: Content::text("recover me").unwrap(),
        };
        let AgentRunAdmission::Created(run) =
            store.admit_agent_run(&spec, run_snapshot.clone()).unwrap()
        else {
            panic!("first admission must create")
        };
        let fsm = AgentFsm::new(
            run.id,
            Some(match &run.origin {
                AgentRunOrigin::User {
                    conversation_id, ..
                } => *conversation_id,
            }),
            run_snapshot.limits,
            run_snapshot.model.runtime.generation.max_output_tokens,
            &run_snapshot.tools,
        );
        let state = AgentFsmState {
            status: run.status,
            checkpoint: run.checkpoint,
            usage: run.usage,
        };
        let driven = fsm.transition(&state, AgentFsmInput::Drive).unwrap();
        store
            .commit_agent_transition(run.id, 0, &driven.state, &driven.output)
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
        let started_operation = operation
            .apply_fsm_transition(started.state, &started.output)
            .unwrap();
        store
            .commit_agent_operation_transition(&operation, &started_operation, &started.output)
            .unwrap();

        let generator = Arc::new(FakeGenerator(AtomicUsize::new(0)));
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(generator.clone()),
            RestoredInferenceHealth::default(),
        )
        .unwrap();
        let (service, _handle) = AgentService::start(
            AgentDependencies {
                store: store.clone(),
                inference: Arc::new(inference),
            },
            vec![],
        )
        .unwrap();
        for _ in 0..100 {
            if store.agent_run_view(run.id).unwrap().status == AgentRunStatus::Completed {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            store.agent_run_view(run.id).unwrap().status,
            AgentRunStatus::Completed
        );
        assert_eq!(generator.0.load(Ordering::Relaxed), 1);
        let recovered = store.agent_operation(&operation.key).unwrap();
        assert_eq!(recovered.delivery_attempt.get(), 2);
        service.shutdown();
        inference_service.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cancellation_reaches_active_inference_and_commits_cancelled_run() {
        let root = std::env::temp_dir().join(format!("agl-daemon-cancel-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        bind_conversation(&store, conversation_id, &snapshot(&root));
        let generator = Arc::new(CancellableGenerator(AtomicBool::new(false)));
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(generator.clone()),
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
        let run_id = handle
            .start_run(AgentRunSpec {
                reasoning: None,
                origin: AgentRunOrigin::User {
                    conversation_id,
                    message_id: MessageId::generate(),
                },
                input: Content::text("cancel me").unwrap(),
            })
            .unwrap();
        for _ in 0..1_000 {
            if generator.0.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        handle.cancel(run_id).unwrap();
        for _ in 0..1_000 {
            if store.agent_run_view(run_id).unwrap().status == AgentRunStatus::Cancelled {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            store.agent_run_view(run_id).unwrap().status,
            AgentRunStatus::Cancelled
        );
        service.shutdown();
        inference_service.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn retryable_model_delivery_is_bounded_and_durable() {
        let root = std::env::temp_dir().join(format!("agl-daemon-retry-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        bind_conversation(&store, conversation_id, &snapshot(&root));
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
            },
            vec![],
        )
        .unwrap();
        let run_id = handle
            .start_run(AgentRunSpec {
                reasoning: None,
                origin: AgentRunOrigin::User {
                    conversation_id,
                    message_id: MessageId::generate(),
                },
                input: Content::text("retry me").unwrap(),
            })
            .unwrap();
        for _ in 0..1_000 {
            if store.agent_run_view(run_id).unwrap().status.is_terminal() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            store.agent_run_view(run_id).unwrap().status,
            AgentRunStatus::Completed
        );
        assert_eq!(generator.0.load(Ordering::SeqCst), 3);
        let operation = store
            .agent_operation(&agl_core::agent::AgentOperationKey {
                run_id,
                ordinal: std::num::NonZeroU32::MIN,
            })
            .unwrap();
        assert_eq!(operation.delivery_attempt.get(), 3);
        let events = store.agent_event_page(None, 100).unwrap().events;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event.data,
                    agl_core::agent::AgentEventData::OperationRetryScheduled { .. }
                ))
                .count(),
            2
        );
        assert!(events.iter().all(|event| event.committed_at_ms > 0));
        service.shutdown();
        inference_service.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_model_tool_output_terminates_the_operation_and_run() {
        let root =
            std::env::temp_dir().join(format!("agl-daemon-invalid-model-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        bind_conversation(&store, conversation_id, &snapshot(&root));
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(Arc::new(InvalidToolGenerator)),
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
        let run_id = handle
            .start_run(AgentRunSpec {
                reasoning: None,
                origin: AgentRunOrigin::User {
                    conversation_id,
                    message_id: MessageId::generate(),
                },
                input: Content::text("reject invalid tool output").unwrap(),
            })
            .unwrap();
        for _ in 0..1_000 {
            if store.agent_run_view(run_id).unwrap().status.is_terminal() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            store.agent_run_view(run_id).unwrap().status,
            AgentRunStatus::Failed
        );
        let operation = store
            .agent_operation(&agl_core::agent::AgentOperationKey {
                run_id,
                ordinal: NonZeroU32::MIN,
            })
            .unwrap();
        assert_eq!(
            operation.state,
            agl_core::agent::AgentOperationDeliveryState::Failed
        );
        assert_eq!(
            operation.failure,
            Some(AgentOperationFailure {
                kind: AgentOperationFailureKind::InvalidResult,
            })
        );
        service.shutdown();
        inference_service.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn panicking_at_most_once_tool_is_recovered_as_outcome_unknown() {
        let root =
            std::env::temp_dir().join(format!("agl-daemon-tool-panic-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        let (bindings, admitted) = test_extension_with_handler(
            "panic containment",
            agl_core::agent::DeliveryClass::AtMostOnce,
            Arc::new(PanickingTool),
        );
        let mut configured = snapshot(&root);
        configured.tools.push(admitted);
        bind_conversation(&store, conversation_id, &configured);
        let generator = Arc::new(ToolCallingGenerator(AtomicUsize::new(0)));
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(generator),
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
        let failed_run = handle
            .start_run(AgentRunSpec {
                reasoning: None,
                origin: AgentRunOrigin::User {
                    conversation_id,
                    message_id: MessageId::generate(),
                },
                input: Content::text("panic in the Tool").unwrap(),
            })
            .unwrap();
        for _ in 0..1_000 {
            if store
                .agent_run_view(failed_run)
                .unwrap()
                .status
                .is_terminal()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            store.agent_run_view(failed_run).unwrap().status,
            AgentRunStatus::Failed
        );
        let failure = store
            .agent_run_view(failed_run)
            .unwrap()
            .failure
            .expect("terminal Tool diagnostic");
        let agl_core::agent::AgentRunFailureView::Operation {
            operation,
            kind,
            tool_id,
        } = failure
        else {
            panic!("expected operation failure");
        };
        assert_eq!(
            operation,
            agl_core::agent::AgentOperationKey {
                run_id: failed_run,
                ordinal: NonZeroU32::new(2).unwrap(),
            }
        );
        assert_eq!(kind, AgentOperationFailureKind::OutcomeUnknown);
        assert_eq!(tool_id, Some(ToolId::new("test.extension:echo").unwrap()));
        let tool_operation = store
            .agent_operation(&agl_core::agent::AgentOperationKey {
                run_id: failed_run,
                ordinal: NonZeroU32::new(2).unwrap(),
            })
            .unwrap();
        assert_eq!(
            tool_operation.state,
            agl_core::agent::AgentOperationDeliveryState::OutcomeUnknown
        );

        let next_run = handle
            .start_run(AgentRunSpec {
                reasoning: None,
                origin: AgentRunOrigin::User {
                    conversation_id,
                    message_id: MessageId::generate(),
                },
                input: Content::text("prove the scheduler survived").unwrap(),
            })
            .unwrap();
        for _ in 0..1_000 {
            if store.agent_run_view(next_run).unwrap().status.is_terminal() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            store.agent_run_view(next_run).unwrap().status,
            AgentRunStatus::Completed
        );
        service.shutdown();
        inference_service.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn model_deltas_are_transient_exact_progress() {
        let root =
            std::env::temp_dir().join(format!("agl-daemon-progress-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        bind_conversation(&store, conversation_id, &snapshot(&root));
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(Arc::new(StreamingGenerator)),
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
        let mut progress = handle.subscribe();
        let run_id = handle
            .start_run(AgentRunSpec {
                reasoning: None,
                origin: AgentRunOrigin::User {
                    conversation_id,
                    message_id: MessageId::generate(),
                },
                input: Content::text("stream").unwrap(),
            })
            .unwrap();
        for _ in 0..1_000 {
            if store.agent_run_view(run_id).unwrap().status.is_terminal() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let deltas = std::iter::from_fn(|| progress.receiver.try_recv().ok())
            .filter_map(|signal| match signal {
                SubscriptionSignal::Progress(AgentProgress::ModelOutputDelta {
                    content, ..
                }) => Some(content.into_text()),
                SubscriptionSignal::Progress(AgentProgress::OperationStatus { .. })
                | SubscriptionSignal::Durable => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(deltas, ["one", " two"]);
        let durable = serde_json::to_string(&store.agent_event_page(None, 100).unwrap()).unwrap();
        assert!(!durable.contains("one two"));
        service.shutdown();
        inference_service.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn run_deadline_cancels_inference_and_records_deadline_failure() {
        let root =
            std::env::temp_dir().join(format!("agl-daemon-deadline-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(&root).unwrap();
        let conversation_id = ConversationId::generate();
        let mut configured = snapshot(&root);
        configured.limits.deadline_ms = 20;
        bind_conversation(&store, conversation_id, &configured);
        let generator = Arc::new(CancellableGenerator(AtomicBool::new(false)));
        let (inference_service, inference) = InferenceService::start(
            InferenceConfig::custom(generator),
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
        let run_id = handle
            .start_run(AgentRunSpec {
                reasoning: None,
                origin: AgentRunOrigin::User {
                    conversation_id,
                    message_id: MessageId::generate(),
                },
                input: Content::text("expire me").unwrap(),
            })
            .unwrap();
        for _ in 0..1_000 {
            if store.agent_run_view(run_id).unwrap().status.is_terminal() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            store.agent_run_view(run_id).unwrap().status,
            AgentRunStatus::Failed
        );
        assert!(
            store
                .agent_event_page(None, 100)
                .unwrap()
                .events
                .iter()
                .any(|event| {
                    matches!(
                        event.data,
                        agl_core::agent::AgentEventData::RunStatusChanged {
                            failure_kind: Some(agl_core::agent::AgentRunFailureKind::Deadline),
                            ..
                        }
                    )
                })
        );
        service.shutdown();
        inference_service.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn transient_progress_overflow_sets_one_resynchronization_signal() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(PROGRESS_CAPACITY);
        let lagged = Arc::new(AtomicBool::new(false));
        let subscribers = Arc::new(Mutex::new(vec![ProgressSubscriber {
            run_id: None,
            sender,
            lagged: lagged.clone(),
        }]));
        let operation = agl_core::agent::AgentOperationKey {
            run_id: agl_core::AgentRunId::generate(),
            ordinal: NonZeroU32::MIN,
        };
        for _ in 0..=PROGRESS_CAPACITY {
            emit_progress(
                &subscribers,
                AgentProgress::OperationStatus {
                    operation: operation.clone(),
                    status: AgentProgressStatus::Running,
                },
            );
        }
        assert!(lagged.swap(false, Ordering::AcqRel));
        assert_eq!(
            std::iter::from_fn(|| receiver.try_recv().ok()).count(),
            PROGRESS_CAPACITY
        );
        assert!(!lagged.load(Ordering::Acquire));
    }
}
