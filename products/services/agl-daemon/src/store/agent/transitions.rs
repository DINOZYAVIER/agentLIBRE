use super::*;
impl StoreHandle {
    pub fn admit_agent_run(
        &self,
        spec: &AgentRunSpec,
        mut snapshot: AgentRunSnapshot,
    ) -> crate::store::Result<AgentRunAdmission> {
        spec.input.validate().map_err(StoreError::Content)?;
        let store = self.lock()?;
        store.transaction(|tx| {
            create_agent_schema(tx)?;
            if let Some(run) = existing_agent_run(tx, spec)? {
                return Ok(AgentRunAdmission::Replayed(run));
            }
            if let Some(selected) = spec.reasoning {
                let agl_core::agent::ReasoningSelection::Enabled { effort, .. } =
                    &mut snapshot.model.runtime.reasoning
                else {
                    return Err(StoreError::InvalidValue {
                        field: "Run reasoning",
                        value: format!("{selected:?}"),
                        reason: "Function has disabled reasoning and provides no reasoning budget",
                    });
                };
                if !snapshot.model.reasoning_efforts.contains(&selected) {
                    return Err(StoreError::InvalidValue {
                        field: "Run reasoning",
                        value: format!("{selected:?}"),
                        reason: "Model does not declare this reasoning effort",
                    });
                }
                *effort = Some(selected);
            }
            let origin = strict_json(&spec.origin)?;
            let input = strict_json(&spec.input)?;
            let agl_core::agent::AgentRunOrigin::User {
                conversation_id,
                message_id,
            } = &spec.origin;
            let busy: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_runs
                 WHERE origin_conversation_id=?1 AND status IN ('pending','running'))",
                [conversation_id.as_bytes().as_slice()],
                |row| row.get(0),
            )?;
            if busy {
                return Err(StoreError::ConversationBusy {
                    conversation_id: *conversation_id,
                });
            }
            let id = AgentRunId::generate();
            let admitted_at_ms = unix_ms();
            let touched = tx.execute(
                "UPDATE agent_conversations SET last_active_at_ms=?1
                 WHERE conversation_id=?2",
                params![admitted_at_ms, conversation_id.as_bytes().as_slice()],
            )?;
            if touched != 1 {
                return Err(StoreError::NotFound {
                    resource: format!("Conversation {conversation_id}"),
                });
            }
            let deadline_at_ms = if snapshot.limits.deadline_ms == i64::MAX {
                i64::MAX
            } else {
                admitted_at_ms
                    .checked_add(snapshot.limits.deadline_ms)
                    .ok_or(StoreError::InvalidValue {
                        field: "deadline_ms",
                        value: snapshot.limits.deadline_ms.to_string(),
                        reason: "deadline overflows UTC milliseconds",
                    })?
            };
            let input_message_id = message_id.clone();
            let input_visibility = agl_core::agent::MessageVisibility::Conversation;
            let mut context = compaction::conversation_context(tx, *conversation_id)?;
            context.push(input_message_id.clone());
            let checkpoint = AgentCheckpoint::Ready {
                context,
                next_operation_ordinal: NonZeroU32::MIN,
            };
            let usage = AgentRunUsage::default();
            tx.execute(
                "INSERT INTO agent_runs (
                    agent_run_id, origin_json, origin_conversation_id,
                    origin_message_id,
                    input_json, snapshot_json, agent_package_digest,
                    model_package_digest, instruction_digest, status,
                    checkpoint_json, deadline_ms, admitted_at_ms, deadline_at_ms,
                    limit_model_input_tokens,
                    limit_model_output_tokens, limit_model_calls, limit_tool_calls,
                    limit_tool_result_bytes,
                    usage_model_input_tokens, usage_model_output_tokens,
                    usage_model_calls, usage_correction_input_tokens,
                    usage_correction_output_tokens, usage_correction_calls,
                    usage_tool_calls, revision
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                           ?8, ?9, 'pending', ?10, ?11, ?12, ?13, ?14,
                           ?15, ?16, ?17, ?18, 0, 0, 0, 0, 0, 0, 0, 0)",
                params![
                    id.as_bytes().as_slice(),
                    origin,
                    conversation_id.as_bytes().as_slice(),
                    message_id.as_str(),
                    input,
                    strict_json(&snapshot)?,
                    snapshot.agent.digest.as_bytes().as_slice(),
                    snapshot.model.model.digest.as_bytes().as_slice(),
                    snapshot.instructions.digest.as_bytes().as_slice(),
                    strict_json(&checkpoint)?,
                    snapshot.limits.deadline_ms,
                    admitted_at_ms,
                    deadline_at_ms,
                    snapshot.limits.model_input_tokens,
                    snapshot.limits.model_output_tokens,
                    snapshot.limits.model_calls,
                    snapshot.limits.tool_calls,
                    snapshot.limits.tool_result_bytes,
                ],
            )?;
            tx.execute(
                "INSERT INTO agent_messages (
                    message_id, conversation_id, agent_run_id, role, visibility, content_json
                 ) VALUES (?1, ?2, ?3, 'user', ?4, ?5)",
                params![
                    input_message_id.as_str(),
                    origin_conversation_bytes(&spec.origin),
                    id.as_bytes().as_slice(),
                    enum_text(&input_visibility)?,
                    strict_json(&spec.input)?,
                ],
            )?;
            append_events(
                tx,
                id,
                &[
                    AgentEventData::RunAdmitted {
                        origin: spec.origin.clone(),
                    },
                    AgentEventData::MessageAppended {
                        message_id: input_message_id,
                        role: agl_core::agent::MessageRole::User,
                        visibility: input_visibility,
                    },
                ],
                admitted_at_ms,
                0,
                &[],
                None,
            )?;
            Ok(AgentRunAdmission::Created(AgentRun {
                id,
                origin: spec.origin.clone(),
                snapshot,
                status: AgentRunStatus::Pending,
                checkpoint,
                usage,
                revision: 0,
            }))
        })
    }

    pub fn commit_agent_transition(
        &self,
        run_id: AgentRunId,
        expected_revision: u64,
        state: &AgentFsmState,
        output: &AgentFsmOutput,
    ) -> crate::store::Result<Vec<AgentEvent>> {
        if output.message.is_some() {
            return Err(StoreError::InvalidValue {
                field: "Agent transition",
                value: run_id.to_string(),
                reason: "generated messages must be committed with their operation result",
            });
        }
        let store = self.lock()?;
        store.transaction(|tx| {
            create_agent_schema(tx)?;
            let changed = tx.execute(
                "UPDATE agent_runs SET status = ?1, checkpoint_json = ?2,
                    usage_model_input_tokens=?3, usage_model_output_tokens=?4,
                    usage_model_calls=?5, usage_tool_calls=?6,
                    usage_correction_input_tokens=?7,
                    usage_correction_output_tokens=?8, usage_correction_calls=?9,
                    revision = revision + 1
                 WHERE agent_run_id = ?10 AND revision = ?11
                   AND usage_model_input_tokens <= ?3
                   AND usage_model_output_tokens <= ?4
                   AND usage_model_calls <= ?5
                   AND usage_tool_calls <= ?6
                   AND usage_correction_input_tokens <= ?7
                   AND usage_correction_output_tokens <= ?8
                   AND usage_correction_calls <= ?9",
                params![
                    enum_text(&state.status)?,
                    strict_json(&state.checkpoint)?,
                    state.usage.model_input_tokens,
                    state.usage.model_output_tokens,
                    state.usage.model_calls,
                    state.usage.tool_calls,
                    state.usage.correction_input_tokens,
                    state.usage.correction_output_tokens,
                    state.usage.correction_calls,
                    run_id.as_bytes().as_slice(),
                    expected_revision,
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::TransitionRejected {
                    resource: run_id.to_string(),
                    from: expected_revision.to_string(),
                    to: expected_revision.saturating_add(1).to_string(),
                });
            }
            if let Some(operation) = &output.operation {
                let (kind, tool_id) = match &operation.request {
                    AgentOperationRequest::ModelGeneration(_) => ("model_generation", None),
                    AgentOperationRequest::Compaction(_) => ("compaction", None),
                    AgentOperationRequest::Tool(request) => {
                        ("tool", Some(request.tool_id.as_str()))
                    }
                };
                tx.execute(
                    "INSERT INTO agent_operations (
                        agent_run_id, ordinal, kind, tool_id, request_json, delivery,
                        delivery_attempt, state, result_json, result_message_id, failure_json,
                        retry_at_ms, revision
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, NULL, NULL, ?9)",
                    params![
                        run_id.as_bytes().as_slice(),
                        operation.key.ordinal.get(),
                        kind,
                        tool_id,
                        strict_json(&operation.request)?,
                        enum_text(&operation.delivery)?,
                        operation.delivery_attempt.get(),
                        enum_text(&operation.state)?,
                        operation.revision,
                    ],
                )?;
            }
            let operation_revisions = output
                .operation
                .as_ref()
                .map(|operation| vec![(&operation.key, operation.revision)])
                .unwrap_or_default();
            append_events(
                tx,
                run_id,
                &output.events,
                unix_ms(),
                expected_revision.saturating_add(1),
                &operation_revisions,
                None,
            )
        })
    }

    pub(crate) fn record_model_output_rejection(
        &self,
        operation: &AgentOperation,
        correction_attempt: u32,
        raw_output: Option<&Content>,
        diagnostic: agl_core::agent::ModelOutputDiagnostic,
        usage: agl_core::agent::ModelUsage,
        realization: Option<agl_core::agent::InferenceRealizationRef>,
    ) -> crate::store::Result<()> {
        let store = self.lock()?;
        store.transaction(|tx| {
            let current: (String, u64, u32) = tx.query_row(
                "SELECT state, revision, delivery_attempt FROM agent_operations WHERE agent_run_id=?1 AND ordinal=?2",
                params![operation.key.run_id.as_bytes().as_slice(), operation.key.ordinal.get()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            if current != ("running".to_owned(), operation.revision, operation.delivery_attempt.get()) {
                return Err(StoreError::TransitionRejected { resource: format!("{:?}", operation.key), from: current.0, to: "model_output_rejected".into() });
            }
            let events = append_events(tx, operation.key.run_id, &[AgentEventData::ModelOutputRejected {
                key: operation.key.clone(), delivery_attempt: operation.delivery_attempt, correction_attempt,
                diagnostic, usage, realization,
            }], unix_ms(), stored_run_revision(tx, operation.key.run_id)?, &[(&operation.key, operation.revision)], Some((&operation.key, operation.revision)))?;
            let event = events.first().ok_or(StoreError::InvalidValue {
                field: "Model output rejection",
                value: format!("{:?}", operation.key),
                reason: "rejection event was not stored",
            })?;
            tx.execute(
                "INSERT INTO agent_model_output_rejections (
                    agent_event_id, agent_run_id, operation_ordinal, delivery_attempt,
                    correction_attempt, raw_output_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    event.id.0,
                    operation.key.run_id.as_bytes().as_slice(),
                    operation.key.ordinal.get(),
                    operation.delivery_attempt.get(),
                    correction_attempt,
                    raw_output.map(strict_json).transpose()?,
                ],
            )?;
            Ok(())
        })
    }

    pub(crate) fn record_memory_claim_rejection(
        &self,
        operation: &AgentOperation,
        slug: Option<String>,
        reason: agl_core::agent::MemoryClaimRejection,
        text: Option<String>,
        sources: Vec<MessageId>,
    ) -> crate::store::Result<()> {
        // Append-only audit event for a rejected memory claim (M1 D4). It is
        // tied to the extracting operation and never changes operation state;
        // callers record best-effort, so a failure here must not fail the
        // compaction that produced the claim.
        let store = self.lock()?;
        store.transaction(|tx| {
            let current: (String, u64, u32) = tx.query_row(
                "SELECT state, revision, delivery_attempt FROM agent_operations WHERE agent_run_id=?1 AND ordinal=?2",
                params![operation.key.run_id.as_bytes().as_slice(), operation.key.ordinal.get()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            if current != ("running".to_owned(), operation.revision, operation.delivery_attempt.get()) {
                return Err(StoreError::TransitionRejected {
                    resource: format!("{:?}", operation.key),
                    from: current.0,
                    to: "memory_claim_rejected".into(),
                });
            }
            append_events(
                tx,
                operation.key.run_id,
                &[AgentEventData::MemoryClaimRejected {
                    key: operation.key.clone(),
                    slug,
                    reason,
                    text,
                    sources,
                }],
                unix_ms(),
                stored_run_revision(tx, operation.key.run_id)?,
                &[(&operation.key, operation.revision)],
                Some((&operation.key, operation.revision)),
            )
            .map(|_| ())
        })
    }

    #[cfg(test)]
    pub(crate) fn model_output_rejection_content(
        &self,
        event_id: AgentEventId,
    ) -> crate::store::Result<Option<Content>> {
        let store = self.lock()?;
        let raw: Option<Option<String>> = store
            .conn
            .query_row(
                "SELECT raw_output_json FROM agent_model_output_rejections WHERE agent_event_id=?1",
                params![event_id.0],
                |row| row.get(0),
            )
            .optional()?;
        raw.flatten()
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    pub fn commit_agent_operation_transition(
        &self,
        previous: &AgentOperation,
        next: &AgentOperation,
        output: &AgentOperationFsmOutput,
    ) -> crate::store::Result<Vec<AgentEvent>> {
        if next.result.is_some() {
            return Err(StoreError::InvalidValue {
                field: "Agent operation transition",
                value: format!("{:?}", next.key),
                reason: "successful result must be committed with its Agent transition",
            });
        }
        if previous.key != next.key
            || previous.request != next.request
            || previous.delivery != next.delivery
        {
            return Err(StoreError::TransitionRejected {
                resource: "agent operation identity".into(),
                from: format!("{:?}", previous.key),
                to: format!("{:?}", next.key),
            });
        }
        let store = self.lock()?;
        store.transaction(|tx| {
            create_agent_schema(tx)?;
            let retry_at_ms = operation_retry_at_ms(next, output)?;
            let changed = tx.execute(
                "UPDATE agent_operations SET delivery_attempt=?1, state=?2, result_json=NULL,
                    result_message_id=NULL, failure_json=?3, retry_at_ms=?4, revision=?5
                 WHERE agent_run_id=?6 AND ordinal=?7 AND revision=?8
                   AND request_json=?9 AND delivery=?10
                   AND delivery_attempt=?11 AND state=?12",
                params![
                    next.delivery_attempt.get(),
                    enum_text(&next.state)?,
                    next.failure.as_ref().map(strict_json).transpose()?,
                    retry_at_ms,
                    next.revision,
                    next.key.run_id.as_bytes().as_slice(),
                    next.key.ordinal.get(),
                    previous.revision,
                    strict_json(&previous.request)?,
                    enum_text(&previous.delivery)?,
                    previous.delivery_attempt.get(),
                    enum_text(&previous.state)?,
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::TransitionRejected {
                    resource: format!("{:?}", next.key),
                    from: enum_text(&previous.state)?,
                    to: enum_text(&next.state)?,
                });
            }
            let run_revision = stored_run_revision(tx, next.key.run_id)?;
            append_events(
                tx,
                next.key.run_id,
                &output.events,
                unix_ms(),
                run_revision,
                &[(&next.key, next.revision)],
                Some((&next.key, next.revision)),
            )
        })
    }

    pub fn commit_agent_cycle(
        &self,
        previous_operation: &AgentOperation,
        next_operation: &AgentOperation,
        operation_output: &AgentOperationFsmOutput,
        expected_run_revision: u64,
        run_state: &AgentFsmState,
        agent_output: &AgentFsmOutput,
    ) -> crate::store::Result<Vec<AgentEvent>> {
        if previous_operation.key != next_operation.key
            || previous_operation.request != next_operation.request
            || previous_operation.delivery != next_operation.delivery
        {
            return Err(StoreError::TransitionRejected {
                resource: "agent operation identity".into(),
                from: format!("{:?}", previous_operation.key),
                to: format!("{:?}", next_operation.key),
            });
        }
        let run_id = next_operation.key.run_id;
        if operation_retry_at_ms(next_operation, operation_output)?.is_some() {
            return Err(StoreError::InvalidValue {
                field: "Agent operation cycle",
                value: format!("{:?}", next_operation.key),
                reason: "Run resume requires a terminal operation, not a scheduled retry",
            });
        }
        let (result_json, result_message_id) =
            encode_operation_result(next_operation, agent_output.message.as_ref())?;
        let store = self.lock()?;
        let events = store.transaction(|tx| {
            create_agent_schema(tx)?;
            if let Some(message) = &agent_output.message {
                insert_agent_message(tx, message)?;
            }
            let operation_changed = tx.execute(
                "UPDATE agent_operations SET delivery_attempt=?1, state=?2, result_json=?3,
                    result_message_id=?4, failure_json=?5, retry_at_ms=NULL, revision=?6
                 WHERE agent_run_id=?7 AND ordinal=?8 AND revision=?9
                   AND request_json=?10 AND delivery=?11
                   AND delivery_attempt=?12 AND state=?13",
                params![
                    next_operation.delivery_attempt.get(),
                    enum_text(&next_operation.state)?,
                    result_json,
                    result_message_id.as_ref().map(MessageId::as_str),
                    next_operation
                        .failure
                        .as_ref()
                        .map(strict_json)
                        .transpose()?,
                    next_operation.revision,
                    run_id.as_bytes().as_slice(),
                    next_operation.key.ordinal.get(),
                    previous_operation.revision,
                    strict_json(&previous_operation.request)?,
                    enum_text(&previous_operation.delivery)?,
                    previous_operation.delivery_attempt.get(),
                    enum_text(&previous_operation.state)?,
                ],
            )?;
            if operation_changed != 1 {
                return Err(StoreError::TransitionRejected {
                    resource: format!("{:?}", next_operation.key),
                    from: enum_text(&previous_operation.state)?,
                    to: enum_text(&next_operation.state)?,
                });
            }
            let run_changed = tx.execute(
                "UPDATE agent_runs SET status=?1, checkpoint_json=?2,
                    usage_model_input_tokens=?3, usage_model_output_tokens=?4,
                    usage_model_calls=?5, usage_tool_calls=?6,
                    usage_correction_input_tokens=?7,
                    usage_correction_output_tokens=?8, usage_correction_calls=?9,
                    revision=revision+1
                 WHERE agent_run_id=?10 AND revision=?11
                   AND usage_model_input_tokens <= ?3
                   AND usage_model_output_tokens <= ?4
                   AND usage_model_calls <= ?5
                   AND usage_tool_calls <= ?6
                   AND usage_correction_input_tokens <= ?7
                   AND usage_correction_output_tokens <= ?8
                   AND usage_correction_calls <= ?9",
                params![
                    enum_text(&run_state.status)?,
                    strict_json(&run_state.checkpoint)?,
                    run_state.usage.model_input_tokens,
                    run_state.usage.model_output_tokens,
                    run_state.usage.model_calls,
                    run_state.usage.tool_calls,
                    run_state.usage.correction_input_tokens,
                    run_state.usage.correction_output_tokens,
                    run_state.usage.correction_calls,
                    run_id.as_bytes().as_slice(),
                    expected_run_revision,
                ],
            )?;
            if run_changed != 1 {
                return Err(StoreError::TransitionRejected {
                    resource: run_id.to_string(),
                    from: expected_run_revision.to_string(),
                    to: expected_run_revision.saturating_add(1).to_string(),
                });
            }
            if let Some(operation) = &agent_output.operation {
                insert_agent_operation(tx, operation)?;
            }
            let mut events = operation_output.events.clone();
            events.extend(agent_output.events.clone());
            let mut operation_revisions = vec![(&next_operation.key, next_operation.revision)];
            if let Some(operation) = &agent_output.operation {
                operation_revisions.push((&operation.key, operation.revision));
            }
            append_events(
                tx,
                run_id,
                &events,
                unix_ms(),
                expected_run_revision.saturating_add(1),
                &operation_revisions,
                Some((&next_operation.key, next_operation.revision)),
            )
        })?;
        if let Some(message) = agent_output.message.as_ref() {
            tracing::debug!(
                operation = ?next_operation.key,
                message_id = %message.id,
                role = ?message.role,
                visibility = ?message.visibility,
                content_bytes = message.content.as_text().len(),
                "Agent message accepted and stored in SQLite"
            );
        }
        match next_operation.result.as_ref() {
            Some(AgentOperationResult::ModelGeneration(result)) => {
                let (output_kind, tool_id) = match &result.output {
                    ModelGenerationOutput::Assistant(_) => ("assistant", None),
                    ModelGenerationOutput::ToolCall(call) => {
                        ("tool_call", Some(call.tool_id.as_str()))
                    }
                    ModelGenerationOutput::ToolCalls(calls) => (
                        "tool_calls",
                        calls.first().map(|call| call.tool_id.as_str()),
                    ),
                    ModelGenerationOutput::AssistantToolCall(mixed) => {
                        ("assistant_tool_call", Some(mixed.call.tool_id.as_str()))
                    }
                    ModelGenerationOutput::AssistantToolCalls { calls, .. } => (
                        "assistant_tool_calls",
                        calls.first().map(|call| call.tool_id.as_str()),
                    ),
                };
                tracing::debug!(
                    operation = ?next_operation.key,
                    output_kind,
                    tool_id,
                    finish_reason = ?result.finish_reason,
                    input_tokens = result.usage.input_tokens,
                    output_tokens = result.usage.output_tokens,
                    corrected = result.correction.is_some(),
                    "model generation accepted and stored in SQLite"
                );
            }
            Some(AgentOperationResult::Compaction(result)) => tracing::debug!(
                operation = ?next_operation.key,
                content_bytes = result.content.render().map(|content| content.as_text().len()).ok(),
                "compaction accepted and stored in SQLite"
            ),
            Some(AgentOperationResult::Tool(result)) => tracing::debug!(
                operation = ?next_operation.key,
                content_bytes = result.content.as_text().len(),
                effect_receipts = result.effect_receipts.len(),
                "tool result accepted and stored in SQLite"
            ),
            None => {}
        }
        Ok(events)
    }
}
