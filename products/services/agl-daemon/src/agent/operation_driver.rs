use super::*;
use crate::tools::COMMITTED_RESULT_LIMIT;

pub(crate) struct OperationExecution<'a> {
    pub(crate) dependencies: &'a AgentDependencies,
    pub(crate) tools: &'a BTreeMap<String, PreparedTool>,
    pub(crate) runtime: Option<&'a tokio::runtime::Runtime>,
    pub(crate) progress: &'a ProgressSubscribers,
    pub(crate) cancellation: &'a RunCancellation,
    pub(crate) snapshot: &'a agl_core::agent::AgentRunSnapshot,
    pub(crate) conversation_id: Option<agl_core::ConversationId>,
    pub(crate) deadline_at_ms: i64,
}

impl OperationExecution<'_> {
    pub(crate) fn complete(
        &self,
        mut operation: AgentOperation,
    ) -> Result<
        (
            AgentOperation,
            AgentOperation,
            agl_core::agent::AgentOperationFsmOutput,
        ),
        AgentServiceError,
    > {
        let dependencies = self.dependencies;
        let progress = self.progress;
        let cancellation = self.cancellation;
        let deadline_at_ms = self.deadline_at_ms;
        let machine = AgentOperationFsm::for_operation(&operation);
        if operation.state == agl_core::agent::AgentOperationDeliveryState::Running {
            let recovered = machine.transition(
                &operation.state,
                AgentOperationFsmInput::Recover {
                    key: operation.key.clone(),
                    delivery_attempt: operation.delivery_attempt,
                },
            )?;
            let recovered_operation = operation
                .apply_fsm_transition(recovered.state, &recovered.output)
                .map_err(|_| AgentServiceError::InvalidTransition)?;
            if recovered_operation.state.is_terminal() {
                return Ok((operation, recovered_operation, recovered.output));
            }
            dependencies.store.commit_agent_operation_transition(
                &operation,
                &recovered_operation,
                &recovered.output,
            )?;
            emit_durable(progress, operation.key.run_id);
            operation = recovered_operation;
        }
        loop {
            if operation.state.is_terminal() {
                return Err(AgentServiceError::InvalidTransition);
            }
            if operation.state == agl_core::agent::AgentOperationDeliveryState::RetryScheduled {
                let retry_at_ms = dependencies
                    .store
                    .agent_operation_retry_at_ms(&operation.key)?
                    .ok_or(AgentServiceError::InvalidTransition)?;
                wait_until(retry_at_ms, deadline_at_ms, cancellation);
            }
            if cancellation.is_cancelled() {
                let cancelled = machine.transition(
                    &operation.state,
                    AgentOperationFsmInput::Cancel {
                        key: operation.key.clone(),
                        delivery_attempt: operation.delivery_attempt,
                    },
                )?;
                let cancelled_operation = operation
                    .apply_fsm_transition(cancelled.state, &cancelled.output)
                    .map_err(|_| AgentServiceError::InvalidTransition)?;
                return Ok((operation, cancelled_operation, cancelled.output));
            }
            let input = match operation.state {
                agl_core::agent::AgentOperationDeliveryState::Pending => {
                    AgentOperationFsmInput::Start {
                        key: operation.key.clone(),
                        delivery_attempt: operation.delivery_attempt,
                        request: operation.request.clone(),
                    }
                }
                agl_core::agent::AgentOperationDeliveryState::RetryScheduled => {
                    AgentOperationFsmInput::RetryDue {
                        key: operation.key.clone(),
                        delivery_attempt: operation.delivery_attempt,
                        request: operation.request.clone(),
                    }
                }
                _ => return Err(AgentServiceError::InvalidTransition),
            };
            let started = machine.transition(&operation.state, input)?;
            let started_operation = operation
                .apply_fsm_transition(started.state, &started.output)
                .map_err(|_| AgentServiceError::InvalidTransition)?;
            dependencies.store.commit_agent_operation_transition(
                &operation,
                &started_operation,
                &started.output,
            )?;
            tracing::debug!(
                operation = ?started_operation.key,
                kind = ?started_operation.kind(),
                delivery_attempt = started_operation.delivery_attempt.get(),
                tool_id = match &started_operation.request {
                    AgentOperationRequest::Tool(request) => Some(request.tool_id.as_str()),
                    _ => None,
                },
                "Agent operation started"
            );
            emit_durable(progress, operation.key.run_id);
            emit_progress(
                progress,
                AgentProgress::OperationStatus {
                    operation: started_operation.key.clone(),
                    status: AgentProgressStatus::Running,
                },
            );
            let result = if now_ms() >= deadline_at_ms {
                Err(AgentOperationFailure {
                    kind: AgentOperationFailureKind::Deadline,
                })
            } else {
                self.dispatch(&started_operation)
            };
            let transition_input = match result {
                Ok(result) => AgentOperationFsmInput::Result {
                    key: started_operation.key.clone(),
                    delivery_attempt: started_operation.delivery_attempt,
                    result,
                },
                Err(failure) if failure.kind == AgentOperationFailureKind::Cancelled => {
                    AgentOperationFsmInput::Cancel {
                        key: started_operation.key.clone(),
                        delivery_attempt: started_operation.delivery_attempt,
                    }
                }
                Err(failure) => {
                    let retry_at_ms = retry_at_ms(&started_operation, &failure, deadline_at_ms);
                    AgentOperationFsmInput::Failure {
                        key: started_operation.key.clone(),
                        delivery_attempt: started_operation.delivery_attempt,
                        failure,
                        retry_at_ms,
                    }
                }
            };
            let finished = machine.transition(&started_operation.state, transition_input)?;
            let finished_operation = started_operation
                .apply_fsm_transition(finished.state, &finished.output)
                .map_err(|_| AgentServiceError::InvalidTransition)?;
            tracing::debug!(
                operation = ?finished_operation.key,
                kind = ?finished_operation.kind(),
                delivery_attempt = finished_operation.delivery_attempt.get(),
                state = ?finished_operation.state,
                failure = ?finished_operation.failure.as_ref().map(|failure| &failure.kind),
                "Agent operation finished"
            );
            if finished_operation.state
                == agl_core::agent::AgentOperationDeliveryState::RetryScheduled
            {
                dependencies.store.commit_agent_operation_transition(
                    &started_operation,
                    &finished_operation,
                    &finished.output,
                )?;
                emit_durable(progress, operation.key.run_id);
                operation = finished_operation;
                continue;
            }
            let terminal = match finished_operation.state {
                agl_core::agent::AgentOperationDeliveryState::Succeeded => {
                    agl_core::agent::AgentOperationTerminalStatus::Succeeded
                }
                agl_core::agent::AgentOperationDeliveryState::Cancelled => {
                    agl_core::agent::AgentOperationTerminalStatus::Cancelled
                }
                _ => agl_core::agent::AgentOperationTerminalStatus::Failed,
            };
            emit_progress(
                progress,
                AgentProgress::OperationStatus {
                    operation: finished_operation.key.clone(),
                    status: AgentProgressStatus::Terminal(terminal),
                },
            );
            return Ok((started_operation, finished_operation, finished.output));
        }
    }

    fn dispatch(
        &self,
        operation: &AgentOperation,
    ) -> Result<AgentOperationResult, AgentOperationFailure> {
        let dependencies = self.dependencies;
        let tools = self.tools;
        let runtime = self.runtime;
        let progress = self.progress;
        let cancellation = self.cancellation;
        let snapshot = self.snapshot;
        let deadline_at_ms = self.deadline_at_ms;
        match &operation.request {
            AgentOperationRequest::Compaction(request) => {
                super::compaction::execute(self, operation, request)
                    .map(|result| AgentOperationResult::Compaction(Box::new(result)))
            }
            AgentOperationRequest::ModelGeneration(generation) => dependencies
                .store
                .agent_context(&generation.context)
                .map_err(|_| AgentOperationFailure {
                    kind: AgentOperationFailureKind::Execution,
                })
                .and_then(|context| {
                    let progress_sink = {
                        let subscribers = progress.clone();
                        let operation = operation.key.clone();
                        InferenceProgressSink::new(move |content| {
                            emit_progress(
                                &subscribers,
                                AgentProgress::ModelOutputDelta {
                                    operation: operation.clone(),
                                    content,
                                },
                            );
                        })
                    };
                    let health_sink = {
                        let store = dependencies.store.clone();
                        InferenceHealthSink::new(move |updates| {
                            store.put_inference_health_updates(&updates).map_err(|_| ())
                        })
                    };
                    let planner_correction = is_planner_correction(snapshot, &context);
                    let inference_request = InferenceGenerateRequest {
                        operation: operation.key.clone(),
                        delivery_attempt: operation.delivery_attempt,
                        model: snapshot.model.model.clone(),
                        runtime: snapshot.model.runtime.clone(),
                        generation: generation.clone(),
                        instructions: snapshot.instructions.clone(),
                        context: context.clone(),
                        tools: if planner_correction {
                            Vec::new()
                        } else {
                            snapshot.tools.clone()
                        },
                        response_format: planner_correction
                            .then(|| snapshot.response_format.clone())
                            .flatten(),
                        deadline_at_ms,
                        cancellation: cancellation.inference.clone(),
                        progress: Some(progress_sink),
                        health: Some(health_sink),
                    };
                    let capacity = dependencies
                        .inference
                        .measure(inference_request.clone())
                        .map_err(map_inference_failure)?;
                    if capacity.needs_compaction() {
                        return Err(AgentOperationFailure {
                            kind: AgentOperationFailureKind::CompactionRequired(capacity),
                        });
                    }
                    let generated = dependencies.inference.generate(inference_request);
                    match generated {
                        Ok(result) => validate_model_result(snapshot, tools, generation, &result)
                            .map(|()| result)
                            .map_err(map_inference_failure),
                        Err(InferenceServiceError::InvalidModelOutput(output)) => {
                            record_model_output_rejection(dependencies, operation, 0, &output)?;
                            let (Some(raw_output), Some(realization)) =
                                (output.raw_output, output.realization)
                            else {
                                return Err(AgentOperationFailure {
                                    kind: AgentOperationFailureKind::InvalidModelOutput {
                                        usage: output.usage,
                                    },
                                });
                            };
                            correct_invalid_model_output(
                                dependencies,
                                tools,
                                operation,
                                snapshot,
                                context,
                                raw_output,
                                output.usage,
                                realization,
                                deadline_at_ms,
                                cancellation,
                            )
                        }
                        Err(error) => Err(map_inference_failure(error)),
                    }
                })
                .map(AgentOperationResult::ModelGeneration),
            AgentOperationRequest::Tool(request) => {
                let binding = tools
                    .get(request.tool_id.as_str())
                    .ok_or(AgentOperationFailure {
                        kind: AgentOperationFailureKind::Unavailable,
                    });
                let result =
                    binding.and_then(|prepared| {
                        let admitted = snapshot
                            .tools
                            .iter()
                            .find(|tool| {
                                tool.definition.id == request.tool_id
                                    && tool.definition_digest == prepared.binding.definition_digest
                            })
                            .ok_or(AgentOperationFailure {
                                kind: AgentOperationFailureKind::Unauthorized,
                            })?;
                        if admitted.definition != prepared.definition {
                            return Err(AgentOperationFailure {
                                kind: AgentOperationFailureKind::Unauthorized,
                            });
                        }
                        if snapshot.planner_read_only
                            && prepared.definition.required_effects.iter().any(|effect| {
                                matches!(
                                    effect.as_str(),
                                    "agentlibre.builtins:filesystem_write"
                                        | "agentlibre.execution:terminal.control"
                                )
                            })
                        {
                            return Err(AgentOperationFailure {
                                kind: AgentOperationFailureKind::Unauthorized,
                            });
                        }
                        let input = CanonicalJson::new(request.input.clone()).map_err(|_| {
                            AgentOperationFailure {
                                kind: AgentOperationFailureKind::InvalidInput,
                            }
                        })?;
                        prepared
                            .input_validator
                            .validate(input.as_value())
                            .map_err(|_| AgentOperationFailure {
                                kind: AgentOperationFailureKind::InvalidInput,
                            })?;
                        if prepared.definition.required_effects.iter().any(|effect| {
                            !snapshot
                                .authority
                                .0
                                .iter()
                                .any(|grant| &grant.effect == effect)
                        }) {
                            return Err(AgentOperationFailure {
                                kind: AgentOperationFailureKind::Unauthorized,
                            });
                        }
                        if let Some(result) = guarded_duplicate_result(dependencies, operation)? {
                            validate_tool_result(prepared, snapshot, &result)?;
                            return Ok(AgentOperationResult::Tool(result));
                        }
                        reserve_effect_receipt_result(prepared, snapshot)?;
                        let runtime = runtime.ok_or(AgentOperationFailure {
                            kind: AgentOperationFailureKind::Unavailable,
                        })?;
                        let mut tool_context = ToolContext::new(
                            operation.key.clone(),
                            self.conversation_id,
                            snapshot.workspace.clone(),
                            snapshot.authority.clone(),
                            deadline_at_ms,
                            snapshot.limits.tool_result_bytes,
                            cancellation.tool.clone(),
                        );
                        tool_context.read_only_workspace = snapshot.planner_read_only;
                        let delivered = execute_tool_future(
                            runtime,
                            prepared
                                .binding
                                .handler
                                .call(tool_context, request.input.clone()),
                            deadline_at_ms,
                            cancellation,
                        );
                        match delivered {
                            Ok(result) => {
                                // A receipt is checked after the handler ran. Invalid
                                // output cannot establish that its effect was absent.
                                let result = accept_tool_result(prepared, snapshot, result)
                                    .map_err(|_| AgentOperationFailure {
                                        kind: AgentOperationFailureKind::OutcomeUnknown,
                                    })?;
                                Ok(AgentOperationResult::Tool(result))
                            }
                            Err(failure) => {
                                let correction = recoverable_tool_failure_result(
                                    dependencies,
                                    operation,
                                    &failure,
                                    snapshot.limits.tool_result_bytes,
                                )?;
                                correction
                                    .map(AgentOperationResult::Tool)
                                    .ok_or_else(|| failure.into())
                            }
                        }
                    });
                match result {
                    Ok(result) => Ok(result),
                    Err(failure) => {
                        let kind = match failure.kind {
                            AgentOperationFailureKind::InvalidInput => {
                                ToolFailureKind::InvalidInput
                            }
                            AgentOperationFailureKind::Unauthorized => {
                                ToolFailureKind::Unauthorized
                            }
                            AgentOperationFailureKind::Unavailable => ToolFailureKind::Unavailable,
                            AgentOperationFailureKind::ResultTooLarge => {
                                ToolFailureKind::ResultTooLarge
                            }
                            _ => return Err(failure),
                        };
                        let rejection = agl_core::agent::ToolFailure::no_effect(
                            kind,
                            Some(if kind == ToolFailureKind::ResultTooLarge {
                                "tool_result_bytes"
                            } else {
                                "input"
                            }),
                            &[if kind == ToolFailureKind::ResultTooLarge {
                                "The configured tool_result_bytes cannot hold the effect receipts. Use another admitted Tool or ask the human to increase the Function result budget."
                            } else {
                                "Inspect the admitted Tool schema and authority; submit a corrected call or use another admitted Tool."
                            }],
                        );
                        recoverable_tool_failure_result(
                            dependencies,
                            operation,
                            &rejection,
                            snapshot.limits.tool_result_bytes,
                        )?
                        .map(AgentOperationResult::Tool)
                        .ok_or(failure)
                    }
                }
            }
        }
    }
}

fn is_planner_correction(
    snapshot: &agl_core::agent::AgentRunSnapshot,
    context: &[agl_core::agent::AgentContextEntry],
) -> bool {
    snapshot.planner_read_only
        && context.iter().rev().any(|entry| {
            entry.message.role == agl_core::agent::MessageRole::User
                && entry
                    .message
                    .content
                    .as_text()
                    .starts_with(agl_runtime::planner::PLANNER_CORRECTION_PREFIX)
        })
}

pub(super) fn record_model_output_rejection(
    dependencies: &AgentDependencies,
    operation: &AgentOperation,
    correction_attempt: u32,
    output: &agl_runtime::inference::InvalidModelOutput,
) -> Result<(), AgentOperationFailure> {
    dependencies
        .store
        .record_model_output_rejection(
            operation,
            correction_attempt,
            output.raw_output.as_ref(),
            output.diagnostic.clone(),
            output.usage,
            output.realization.clone(),
        )
        .map_err(|_| AgentOperationFailure {
            kind: AgentOperationFailureKind::Unavailable,
        })?;
    tracing::warn!(
        operation = ?operation.key,
        delivery_attempt = operation.delivery_attempt.get(),
        correction_attempt,
        class = ?output.diagnostic.class,
        field = output.diagnostic.field.as_deref(),
        finish_reason = ?output.diagnostic.finish_reason,
        output_bytes = output.diagnostic.output_bytes,
        output_digest = %output.diagnostic.output_digest,
        "model output rejected; payload stored in SQLite"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn correct_invalid_model_output(
    dependencies: &AgentDependencies,
    prepared_tools: &BTreeMap<String, PreparedTool>,
    operation: &AgentOperation,
    snapshot: &agl_core::agent::AgentRunSnapshot,
    mut context: Vec<agl_core::agent::AgentContextEntry>,
    raw_output: Content,
    primary_usage: agl_core::agent::ModelUsage,
    primary_realization: agl_core::agent::InferenceRealizationRef,
    deadline_at_ms: i64,
    cancellation: &RunCancellation,
) -> Result<agl_core::agent::ModelGenerationResult, AgentOperationFailure> {
    let Some(recovery) = &snapshot.invalid_model_output_recovery else {
        return Err(AgentOperationFailure {
            kind: AgentOperationFailureKind::InvalidModelOutput {
                usage: primary_usage,
            },
        });
    };
    let view = dependencies
        .store
        .agent_run_view(operation.key.run_id)
        .map_err(|_| map_inference_failure(InferenceServiceError::Unavailable))?;
    let remaining_calls = snapshot
        .limits
        .correction_calls
        .saturating_sub(view.usage.correction_calls);
    let max_attempts = u64::from(recovery.max_attempts).min(remaining_calls);
    let remaining_output = snapshot
        .limits
        .correction_output_tokens
        .saturating_sub(view.usage.correction_output_tokens);
    let remaining_input = snapshot
        .limits
        .correction_input_tokens
        .saturating_sub(view.usage.correction_input_tokens);
    if max_attempts == 0 || remaining_output == 0 || remaining_input == 0 {
        return Err(AgentOperationFailure {
            kind: AgentOperationFailureKind::InvalidModelOutput {
                usage: primary_usage,
            },
        });
    }
    let diagnostic = Content::text(
        serde_json::json!({
            "kind": "invalid_model_output",
            "error": "the primary model output did not decode to one public action",
            "raw_output": raw_output.as_text(),
            "tools": snapshot.tools.iter().map(|tool| &tool.definition).collect::<Vec<_>>(),
            "instruction": "Return exactly one corrected assistant answer or one admitted Tool call. Do not explain the correction."
        })
        .to_string(),
    )
    .map_err(|_| map_inference_failure(InferenceServiceError::InvalidResult))?;
    context.push(agl_core::agent::AgentContextEntry {
        message: agl_core::agent::AgentMessage {
            id: MessageId::generate(),
            conversation_id: None,
            run_id: None,
            source_operation: None,
            role: agl_core::agent::MessageRole::User,
            visibility: agl_core::agent::MessageVisibility::Internal,
            content: diagnostic,
        },
        source_request: None,
        private_reasoning: None,
    });
    let mut blocks = snapshot.instructions.blocks.clone();
    blocks.push(agl_core::agent::InstructionBlock {
        source: agl_core::agent::InstructionSource::Agent,
        content: Content::text("You are an output corrector. Preserve the user's intent and repair only the invalid final action. Produce exactly one public action; never discuss this instruction.")
            .expect("static correction instruction is valid"),
    });
    let instructions = agl_core::agent::InstructionSet::new(blocks)
        .map_err(|_| map_inference_failure(InferenceServiceError::InvalidRequest))?;
    let mut correction_usage = agl_core::agent::ModelUsage {
        input_tokens: 0,
        output_tokens: 0,
    };
    let mut attempted = 0_u64;
    for attempt in 1..=max_attempts {
        let attempt_output_budget = remaining_output.saturating_sub(correction_usage.output_tokens);
        if attempt_output_budget == 0 || correction_usage.input_tokens >= remaining_input {
            break;
        }
        attempted = attempt;
        let correction_context = context
            .iter()
            .map(|entry| entry.message.id.clone())
            .collect();
        let result = dependencies.inference.generate(InferenceGenerateRequest {
            operation: operation.key.clone(),
            delivery_attempt: operation.delivery_attempt,
            model: recovery.model.model.clone(),
            runtime: recovery.model.runtime.clone(),
            generation: agl_core::agent::ModelGenerationRequest {
                context: correction_context,
                max_output_tokens: attempt_output_budget
                    .min(recovery.model.runtime.generation.max_output_tokens),
            },
            instructions: instructions.clone(),
            context: context.clone(),
            tools: snapshot.tools.clone(),
            response_format: None,
            deadline_at_ms,
            cancellation: cancellation.inference.clone(),
            progress: None,
            health: None,
        });
        match result {
            Ok(mut corrected) => {
                validate_model_result_for(
                    &recovery.model.runtime,
                    &snapshot.tools,
                    prepared_tools,
                    &corrected,
                    remaining_output,
                )
                .map_err(map_inference_failure)?;
                correction_usage.input_tokens = correction_usage
                    .input_tokens
                    .saturating_add(corrected.usage.input_tokens);
                correction_usage.output_tokens = correction_usage
                    .output_tokens
                    .saturating_add(corrected.usage.output_tokens);
                if correction_usage.input_tokens > remaining_input
                    || correction_usage.output_tokens > remaining_output
                {
                    break;
                }
                let correction_realization = corrected.realization.clone();
                corrected.usage = primary_usage;
                corrected.realization = primary_realization;
                corrected.correction = Some(Box::new(agl_core::agent::ModelCorrectionRecord {
                    model: recovery.model.model.clone(),
                    attempts: attempt as u32,
                    usage: correction_usage,
                    realization: correction_realization,
                }));
                corrected.private_reasoning = None;
                return Ok(corrected);
            }
            Err(InferenceServiceError::InvalidModelOutput(output)) => {
                record_model_output_rejection(dependencies, operation, attempt as u32, &output)?;
                correction_usage.input_tokens = correction_usage
                    .input_tokens
                    .saturating_add(output.usage.input_tokens);
                correction_usage.output_tokens = correction_usage
                    .output_tokens
                    .saturating_add(output.usage.output_tokens);
                if attempt < max_attempts {
                    let Some(raw_output) = output.raw_output else {
                        break;
                    };
                    let content = Content::text(
                        serde_json::json!({
                            "kind": "invalid_correction_output",
                            "attempt": attempt,
                            "raw_output": raw_output.as_text(),
                            "instruction": "Repair this output and return exactly one public action."
                        })
                        .to_string(),
                    )
                    .map_err(|_| map_inference_failure(InferenceServiceError::InvalidResult))?;
                    context.push(agl_core::agent::AgentContextEntry {
                        message: agl_core::agent::AgentMessage {
                            id: MessageId::generate(),
                            conversation_id: None,
                            run_id: None,
                            source_operation: None,
                            role: agl_core::agent::MessageRole::User,
                            visibility: agl_core::agent::MessageVisibility::Internal,
                            content,
                        },
                        source_request: None,
                        private_reasoning: None,
                    });
                }
            }
            Err(_) => break,
        }
    }
    Err(AgentOperationFailure {
        kind: AgentOperationFailureKind::CorrectionFailed {
            primary_usage,
            calls: attempted,
            input_tokens: correction_usage.input_tokens,
            output_tokens: correction_usage.output_tokens,
        },
    })
}

const GUARDED_OBSERVATION_TOOLS: [&str; 2] = [
    "agentlibre.builtins:fs_read",
    "agentlibre.builtins:forge_read",
];
const DUPLICATE_TOOL_CALL_KIND: &str = "duplicate_tool_call";
const TOOL_REQUEST_FAILURE_KIND: &str = "tool_request_failure";
const MAX_CONSECUTIVE_TOOL_REQUEST_FAILURES: u64 = 3;

fn recoverable_tool_failure_result(
    dependencies: &AgentDependencies,
    operation: &AgentOperation,
    failure: &agl_core::agent::ToolFailure,
    result_bytes: u64,
) -> Result<Option<ToolResult>, AgentOperationFailure> {
    let agl_core::agent::ToolFailureEffect::None {
        field,
        next_actions,
        details,
    } = &failure.effect
    else {
        return Ok(None);
    };
    if matches!(
        failure.kind,
        ToolFailureKind::Cancelled | ToolFailureKind::Deadline | ToolFailureKind::OutcomeUnknown
    ) {
        return Ok(None);
    }
    if next_actions.is_empty()
        || next_actions.len() > 8
        || next_actions
            .iter()
            .any(|value| value.is_empty() || value.len() > 1024)
        || field.as_ref().is_some_and(|value| value.len() > 128)
    {
        return Err(AgentOperationFailure {
            kind: AgentOperationFailureKind::InvalidResult,
        });
    }
    let previous_attempt = dependencies
        .store
        .preceding_tool_operation(&operation.key)
        .map_err(|_| AgentOperationFailure {
            kind: AgentOperationFailureKind::Execution,
        })?
        .as_ref()
        .and_then(tool_request_failure_attempt)
        .unwrap_or(0);
    if previous_attempt >= MAX_CONSECUTIVE_TOOL_REQUEST_FAILURES {
        return Err(AgentOperationFailure {
            kind: AgentOperationFailureKind::ToolLoopDetected,
        });
    }
    let instruction = match (&operation.request, failure.kind) {
        (AgentOperationRequest::Tool(request), ToolFailureKind::InvalidInput)
            if request.tool_id.as_str() == "agentlibre.builtins:fs_apply_patch" =>
        {
            "The patch made no change. Do not repeat it. To create a new file, use op=create with a workspace-relative path, content and expected_absent=true; no digest is required. For update/delete, call fs_read for the existing target and copy its digest exactly. For move, read the existing source and copy its digest; do not move /dev/null to create a file."
        }
        _ => {
            "The Tool made no change. Do not repeat the same request. Inspect the admitted inputs and authority, then issue a corrected Tool call."
        }
    };
    let content = Content::text(
        serde_json::json!({
            "kind": TOOL_REQUEST_FAILURE_KIND,
            "failure": failure.kind,
            "effect": "none",
            "field": field,
            "details": details,
            "next_actions": next_actions,
            "attempt": previous_attempt + 1,
            "instruction": instruction,
        })
        .to_string(),
    )
    .map_err(|_| AgentOperationFailure {
        kind: AgentOperationFailureKind::InvalidResult,
    })?;
    let result = ToolResult {
        content,
        effect_receipts: Vec::new(),
    };
    let size = serde_json::to_vec(&result)
        .map_err(|_| AgentOperationFailure {
            kind: AgentOperationFailureKind::InvalidResult,
        })?
        .len() as u64;
    if size > result_bytes.min(agl_core::agent::MAX_TOOL_RESULT_BYTES) {
        return Err(AgentOperationFailure {
            kind: AgentOperationFailureKind::ResultTooLarge,
        });
    }
    Ok(Some(result))
}

fn tool_request_failure_attempt(operation: &AgentOperation) -> Option<u64> {
    let AgentOperationResult::Tool(result) = operation.result.as_ref()? else {
        return None;
    };
    let value = serde_json::from_str::<serde_json::Value>(result.content.as_text()).ok()?;
    (value.get("kind").and_then(serde_json::Value::as_str) == Some(TOOL_REQUEST_FAILURE_KIND))
        .then(|| value.get("attempt").and_then(serde_json::Value::as_u64))
        .flatten()
}

fn guarded_duplicate_result(
    dependencies: &AgentDependencies,
    operation: &AgentOperation,
) -> Result<Option<ToolResult>, AgentOperationFailure> {
    let AgentOperationRequest::Tool(request) = &operation.request else {
        return Ok(None);
    };
    if !GUARDED_OBSERVATION_TOOLS.contains(&request.tool_id.as_str()) {
        return Ok(None);
    }
    let previous = dependencies
        .store
        .preceding_tool_operation(&operation.key)
        .map_err(|_| AgentOperationFailure {
            kind: AgentOperationFailureKind::Execution,
        })?;
    let Some(previous) = previous else {
        return Ok(None);
    };
    if previous.request != operation.request {
        return Ok(None);
    }
    let Some(AgentOperationResult::Tool(previous_result)) = &previous.result else {
        return Err(AgentOperationFailure {
            kind: AgentOperationFailureKind::Execution,
        });
    };
    if is_duplicate_correction(previous_result) {
        return Err(AgentOperationFailure {
            kind: AgentOperationFailureKind::ToolLoopDetected,
        });
    }

    let previous_value =
        serde_json::from_str::<serde_json::Value>(previous_result.content.as_text()).ok();
    let mut correction = serde_json::Map::from_iter([
        (
            "kind".to_owned(),
            serde_json::Value::String(DUPLICATE_TOOL_CALL_KIND.to_owned()),
        ),
        (
            "previous_operation".to_owned(),
            serde_json::json!(previous.key.ordinal.get()),
        ),
        (
            "instruction".to_owned(),
            serde_json::Value::String(
                "The exact read-only Tool call already succeeded. Change the arguments; for pagination use the previous result's next_cursor."
                    .to_owned(),
            ),
        ),
    ]);
    if let Some(next_cursor) = previous_value
        .as_ref()
        .and_then(|value| value.get("next_cursor"))
        .filter(|value| !value.is_null())
    {
        correction.insert("next_cursor".to_owned(), next_cursor.clone());
    }
    let content = serde_json::to_string(&serde_json::Value::Object(correction))
        .ok()
        .and_then(|value| Content::text(value).ok())
        .ok_or(AgentOperationFailure {
            kind: AgentOperationFailureKind::InvalidResult,
        })?;
    Ok(Some(ToolResult {
        content,
        effect_receipts: Vec::new(),
    }))
}

fn is_duplicate_correction(result: &ToolResult) -> bool {
    serde_json::from_str::<serde_json::Value>(result.content.as_text())
        .ok()
        .and_then(|value| {
            value
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|kind| kind == DUPLICATE_TOOL_CALL_KIND)
}

fn retry_at_ms(
    operation: &AgentOperation,
    failure: &AgentOperationFailure,
    deadline_at_ms: i64,
) -> Option<i64> {
    const MAX_DELIVERY_ATTEMPTS: u32 = 3;
    if matches!(operation.request, AgentOperationRequest::Tool(_))
        || operation.delivery != agl_core::agent::DeliveryClass::Retryable
        || operation.delivery_attempt.get() >= MAX_DELIVERY_ATTEMPTS
        || !matches!(
            failure.kind,
            AgentOperationFailureKind::Unavailable | AgentOperationFailureKind::Execution
        )
    {
        return None;
    }
    let exponent = operation.delivery_attempt.get().saturating_sub(1).min(5);
    let delay_ms = 25_i64.checked_shl(exponent).unwrap_or(800).min(800);
    let retry_at_ms = now_ms().checked_add(delay_ms)?;
    (retry_at_ms < deadline_at_ms).then_some(retry_at_ms)
}

fn wait_until(retry_at_ms: i64, deadline_at_ms: i64, cancellation: &RunCancellation) {
    loop {
        let now = now_ms();
        if cancellation.is_cancelled() || now >= retry_at_ms || now >= deadline_at_ms {
            return;
        }
        let remaining = retry_at_ms.min(deadline_at_ms).saturating_sub(now) as u64;
        thread::sleep(std::time::Duration::from_millis(remaining.clamp(1, 10)));
    }
}

fn execute_tool_future(
    runtime: &tokio::runtime::Runtime,
    mut future: ToolFuture,
    deadline_at_ms: i64,
    cancellation: &RunCancellation,
) -> Result<agl_core::agent::ToolResult, agl_core::agent::ToolFailure> {
    runtime.block_on(async move {
        loop {
            let now = now_ms();
            let deadline_elapsed = now >= deadline_at_ms;
            let cancelled = cancellation.is_cancelled();
            if deadline_elapsed || cancelled {
                cancellation.tool.cancel();
                return match tokio::time::timeout(
                    std::time::Duration::from_millis(250),
                    &mut future,
                )
                .await
                {
                    Ok(Err(mut failure)) if failure.kind == ToolFailureKind::Cancelled => {
                        if deadline_elapsed {
                            failure.kind = ToolFailureKind::Deadline;
                        }
                        Err(failure)
                    }
                    Ok(Ok(result)) => Ok(result),
                    Ok(Err(failure)) => Err(failure),
                    Err(_) => Err(agl_core::agent::ToolFailure::unknown(
                        ToolFailureKind::OutcomeUnknown,
                    )),
                };
            }
            let wait_ms = deadline_at_ms.saturating_sub(now).clamp(1, 10) as u64;
            tokio::select! {
                result = &mut future => return result,
                _ = tokio::time::sleep(std::time::Duration::from_millis(wait_ms)) => {}
            }
        }
    })
}

pub(crate) fn operation_message_id(operation: &AgentOperation) -> Option<MessageId> {
    match operation.result.as_ref() {
        Some(AgentOperationResult::ModelGeneration(result))
            if matches!(
                result.output,
                agl_core::agent::ModelGenerationOutput::Assistant(_)
                    | agl_core::agent::ModelGenerationOutput::AssistantToolCall(_)
                    | agl_core::agent::ModelGenerationOutput::AssistantToolCalls { .. }
            ) =>
        {
            Some(MessageId::generate())
        }
        Some(AgentOperationResult::Tool(_)) => Some(MessageId::generate()),
        Some(AgentOperationResult::Compaction(_)) => Some(MessageId::generate()),
        _ if matches!(
            (&operation.request, operation.failure.as_ref()),
            (
                AgentOperationRequest::ModelGeneration(_),
                Some(AgentOperationFailure {
                    kind: AgentOperationFailureKind::InvalidResult
                        | AgentOperationFailureKind::InvalidModelOutput { .. }
                        | AgentOperationFailureKind::CorrectionFailed { .. }
                })
            )
        ) =>
        {
            Some(MessageId::generate())
        }
        _ => None,
    }
}

fn validate_model_result(
    snapshot: &agl_core::agent::AgentRunSnapshot,
    prepared_tools: &BTreeMap<String, PreparedTool>,
    request: &agl_core::agent::ModelGenerationRequest,
    result: &agl_core::agent::ModelGenerationResult,
) -> Result<(), InferenceServiceError> {
    validate_model_result_for(
        &snapshot.model.runtime,
        &snapshot.tools,
        prepared_tools,
        result,
        request.max_output_tokens,
    )
}

fn validate_model_result_for(
    runtime: &agl_core::agent::ModelRuntimeSelection,
    tools: &[agl_core::agent::AdmittedTool],
    prepared_tools: &BTreeMap<String, PreparedTool>,
    result: &agl_core::agent::ModelGenerationResult,
    max_output_tokens: u64,
) -> Result<(), InferenceServiceError> {
    if result.correction.is_some() {
        return Err(InferenceServiceError::InvalidResult);
    }
    if let Some(reasoning) = &result.private_reasoning
        && (!matches!(
            runtime.reasoning,
            agl_core::agent::ReasoningSelection::Enabled { preserve: true, .. }
        ) || reasoning.validate().is_err())
    {
        return Err(InferenceServiceError::InvalidResult);
    }
    if result.usage.output_tokens > max_output_tokens {
        return Err(InferenceServiceError::InvalidResult);
    }
    let validate_call = |call: &agl_core::agent::ToolCall| {
        let input = CanonicalJson::new(call.input.clone())
            .map_err(|_| InferenceServiceError::InvalidResult)?;
        let tool = tools
            .iter()
            .find(|tool| tool.definition.id == call.tool_id)
            .ok_or(InferenceServiceError::InvalidResult)?;
        let prepared = prepared_tools
            .get(tool.definition.id.as_str())
            .filter(|prepared| prepared.definition == tool.definition)
            .ok_or(InferenceServiceError::InvalidResult)?;
        prepared
            .input_validator
            .validate(input.as_value())
            .map_err(|_| InferenceServiceError::InvalidResult)
    };
    match &result.output {
        agl_core::agent::ModelGenerationOutput::Assistant(content) => {
            if result.finish_reason == agl_core::agent::ModelFinishReason::ToolCall
                || content.validate().is_err()
            {
                return Err(InferenceServiceError::InvalidResult);
            }
        }
        agl_core::agent::ModelGenerationOutput::ToolCall(call) => {
            if result.finish_reason != agl_core::agent::ModelFinishReason::ToolCall {
                return Err(InferenceServiceError::InvalidResult);
            }
            validate_call(call)?;
        }
        agl_core::agent::ModelGenerationOutput::ToolCalls(calls) => {
            if result.finish_reason != agl_core::agent::ModelFinishReason::ToolCall
                || calls.is_empty()
            {
                return Err(InferenceServiceError::InvalidResult);
            }
            for call in calls {
                validate_call(call)?;
            }
        }
        agl_core::agent::ModelGenerationOutput::AssistantToolCall(mixed) => {
            if result.finish_reason != agl_core::agent::ModelFinishReason::ToolCall
                || mixed.content.validate().is_err()
            {
                return Err(InferenceServiceError::InvalidResult);
            }
            validate_call(&mixed.call)?;
        }
        agl_core::agent::ModelGenerationOutput::AssistantToolCalls { content, calls } => {
            if result.finish_reason != agl_core::agent::ModelFinishReason::ToolCall
                || content.validate().is_err()
                || calls.is_empty()
            {
                return Err(InferenceServiceError::InvalidResult);
            }
            for call in calls {
                validate_call(call)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_tool_result(
    tool: &PreparedTool,
    snapshot: &agl_core::agent::AgentRunSnapshot,
    result: &agl_core::agent::ToolResult,
) -> Result<(), AgentOperationFailure> {
    let serialized_bytes = serde_json::to_vec(result)
        .map_err(|_| AgentOperationFailure {
            kind: AgentOperationFailureKind::InvalidResult,
        })?
        .len() as u64;
    if serialized_bytes > snapshot.limits.tool_result_bytes
        || serialized_bytes > agl_core::agent::MAX_TOOL_RESULT_BYTES
    {
        return Err(AgentOperationFailure {
            kind: AgentOperationFailureKind::ResultTooLarge,
        });
    }
    validate_tool_receipts(tool, snapshot, result)
}

fn validate_tool_receipts(
    tool: &PreparedTool,
    snapshot: &agl_core::agent::AgentRunSnapshot,
    result: &ToolResult,
) -> Result<(), AgentOperationFailure> {
    if result.effect_receipts.len() != tool.definition.required_effects.len() {
        return Err(AgentOperationFailure {
            kind: AgentOperationFailureKind::InvalidResult,
        });
    }
    for effect in &tool.definition.required_effects {
        let receipts = result
            .effect_receipts
            .iter()
            .filter(|receipt| &receipt.effect == effect)
            .collect::<Vec<_>>();
        if receipts.len() != 1 {
            return Err(AgentOperationFailure {
                kind: AgentOperationFailureKind::InvalidResult,
            });
        }
        let receipt = receipts[0];
        let validator = tool
            .effect_validators
            .get(effect)
            .ok_or(AgentOperationFailure {
                kind: AgentOperationFailureKind::InvalidResult,
            })?;
        if !super::run_driver::valid_realized_scope(
            validator,
            &receipt.scope,
            snapshot.workspace.root.as_path(),
        ) {
            return Err(AgentOperationFailure {
                kind: AgentOperationFailureKind::InvalidResult,
            });
        }
        if !snapshot
            .authority
            .0
            .iter()
            .any(|grant| grant.effect == *effect && grant.scope == receipt.scope)
        {
            return Err(AgentOperationFailure {
                kind: AgentOperationFailureKind::Unauthorized,
            });
        }
    }
    Ok(())
}

/// Before dispatch, prove that the exact possible receipts and the bounded
/// committed-result notice fit. An insufficient budget must not cause effects.
fn reserve_effect_receipt_result(
    tool: &PreparedTool,
    snapshot: &agl_core::agent::AgentRunSnapshot,
) -> Result<(), AgentOperationFailure> {
    if tool.definition.required_effects.is_empty() {
        return Ok(());
    }
    let effect_receipts = tool
        .definition
        .required_effects
        .iter()
        .map(|effect| {
            let grant = snapshot
                .authority
                .0
                .iter()
                .filter(|grant| &grant.effect == effect)
                .max_by_key(|grant| {
                    serde_json::to_vec(&grant.scope)
                        .expect("canonical scope")
                        .len()
                })
                .ok_or(AgentOperationFailure {
                    kind: AgentOperationFailureKind::Unauthorized,
                })?;
            Ok(agl_core::agent::EffectReceipt {
                effect: effect.clone(),
                scope: grant.scope.clone(),
            })
        })
        .collect::<Result<Vec<_>, AgentOperationFailure>>()?;
    validate_tool_result(
        tool,
        snapshot,
        &ToolResult {
            content: Content::text(COMMITTED_RESULT_LIMIT)
                .expect("bounded committed-result notice"),
            effect_receipts,
        },
    )
}

fn accept_tool_result(
    tool: &PreparedTool,
    snapshot: &agl_core::agent::AgentRunSnapshot,
    mut result: ToolResult,
) -> Result<ToolResult, AgentOperationFailure> {
    validate_tool_receipts(tool, snapshot, &result)?;
    match validate_tool_result(tool, snapshot, &result) {
        Ok(()) => Ok(result),
        Err(AgentOperationFailure {
            kind: AgentOperationFailureKind::ResultTooLarge,
        }) if !result.effect_receipts.is_empty() => {
            result.content =
                Content::text(COMMITTED_RESULT_LIMIT).expect("bounded committed-result notice");
            validate_tool_result(tool, snapshot, &result)?;
            Ok(result)
        }
        Err(error) => Err(error),
    }
}
