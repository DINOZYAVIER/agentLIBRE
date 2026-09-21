use super::*;
impl StoreHandle {
    pub(crate) fn pending_memory(
        &self,
        workspace_root: &Path,
    ) -> crate::store::Result<Vec<MemoryTopic>> {
        self.read(|connection| {
            let value: Option<String> = connection
                .query_row(
                    "SELECT claims_json FROM agent_memory_pending WHERE workspace_root=?1",
                    [workspace_root.to_string_lossy().as_ref()],
                    |row| row.get(0),
                )
                .optional()?;
            value
                .map(|value| serde_json::from_str(&value).map_err(Into::into))
                .transpose()
                .map(|value| value.unwrap_or_default())
        })
    }

    pub(crate) fn replace_pending_memory(
        &self,
        workspace_root: &Path,
        claims: &[MemoryTopic],
    ) -> crate::store::Result<()> {
        let store = self.lock()?;
        store.transaction(|tx| {
            create_agent_schema(tx)?;
            if claims.is_empty() {
                tx.execute(
                    "DELETE FROM agent_memory_pending WHERE workspace_root=?1",
                    [workspace_root.to_string_lossy().as_ref()],
                )?;
            } else {
                let json = serde_json::to_string(claims)?;
                tx.execute(
                    "INSERT INTO agent_memory_pending(workspace_root, claims_json)
                     VALUES (?1, ?2)
                     ON CONFLICT(workspace_root) DO UPDATE SET claims_json=excluded.claims_json",
                    params![workspace_root.to_string_lossy().as_ref(), json],
                )?;
            }
            Ok(())
        })
    }

    pub fn agent_run(&self, run_id: AgentRunId) -> crate::store::Result<AgentRun> {
        self.read(|connection| {
            let stored = connection
                .query_row(
                    "SELECT origin_json, snapshot_json, agent_package_digest,
                            model_package_digest, instruction_digest, status, checkpoint_json,
                            usage_model_input_tokens, usage_model_output_tokens,
                            usage_model_calls, usage_correction_input_tokens,
                            usage_correction_output_tokens, usage_correction_calls,
                            usage_tool_calls, revision
                     FROM agent_runs WHERE agent_run_id=?1",
                    [run_id.as_bytes().as_slice()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            (
                                run_id.as_bytes().to_vec(),
                                row.get::<_, String>(1)?,
                                row.get::<_, Vec<u8>>(2)?,
                                row.get::<_, Vec<u8>>(3)?,
                                row.get::<_, Vec<u8>>(4)?,
                                row.get::<_, String>(5)?,
                                row.get::<_, String>(6)?,
                                AgentRunUsage {
                                    model_input_tokens: row.get(7)?,
                                    model_output_tokens: row.get(8)?,
                                    model_calls: row.get(9)?,
                                    correction_input_tokens: row.get(10)?,
                                    correction_output_tokens: row.get(11)?,
                                    correction_calls: row.get(12)?,
                                    tool_calls: row.get(13)?,
                                },
                                row.get::<_, u64>(14)?,
                            ),
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound {
                    resource: format!("AgentRun {run_id}"),
                })?;
            let origin = serde_json::from_str(&stored.0)?;
            decode_run(&origin, stored.1)
        })
    }

    pub fn recoverable_agent_runs(
        &self,
        offset: usize,
        limit: usize,
    ) -> crate::store::Result<Vec<AgentRun>> {
        if !(1..=1_000).contains(&limit) {
            return Err(StoreError::InvalidValue {
                field: "recoverable AgentRun page limit",
                value: limit.to_string(),
                reason: "limit must be between 1 and 1000",
            });
        }
        self.read(|connection| {
            let mut statement = connection.prepare(
                "SELECT agent_run_id, origin_json, snapshot_json,
                        agent_package_digest, model_package_digest, instruction_digest,
                        status, checkpoint_json, usage_model_input_tokens,
                        usage_model_output_tokens, usage_model_calls,
                        usage_correction_input_tokens, usage_correction_output_tokens,
                        usage_correction_calls, usage_tool_calls, revision
                 FROM agent_runs
                 WHERE status IN ('pending','running')
                 ORDER BY rowid LIMIT ?1 OFFSET ?2",
            )?;
            let rows = statement.query_map(params![limit as u64, offset as u64], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    AgentRunUsage {
                        model_input_tokens: row.get(8)?,
                        model_output_tokens: row.get(9)?,
                        model_calls: row.get(10)?,
                        correction_input_tokens: row.get(11)?,
                        correction_output_tokens: row.get(12)?,
                        correction_calls: row.get(13)?,
                        tool_calls: row.get(14)?,
                    },
                    row.get::<_, u64>(15)?,
                ))
            })?;
            let mut runs = Vec::new();
            for row in rows {
                let (
                    id,
                    origin,
                    snapshot,
                    agent_digest,
                    model_digest,
                    instruction_digest,
                    status,
                    checkpoint,
                    usage,
                    revision,
                ) = row?;
                let origin: agl_core::agent::AgentRunOrigin = serde_json::from_str(&origin)?;
                runs.push(decode_run(
                    &origin,
                    (
                        id,
                        snapshot,
                        agent_digest,
                        model_digest,
                        instruction_digest,
                        status,
                        checkpoint,
                        usage,
                        revision,
                    ),
                )?);
            }
            Ok(runs)
        })
    }

    pub fn agent_operation(&self, key: &AgentOperationKey) -> crate::store::Result<AgentOperation> {
        self.read(|connection| {
            let stored = connection
                .query_row(
                    "SELECT request_json, delivery, delivery_attempt, state,
                            result_json, result_message_id, failure_json, revision
                     FROM agent_operations
                     WHERE agent_run_id=?1 AND ordinal=?2",
                    params![key.run_id.as_bytes().as_slice(), key.ordinal.get()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, u32>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, Option<String>>(6)?,
                            row.get::<_, u64>(7)?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound {
                    resource: format!("Agent operation {key:?}"),
                })?;
            let delivery_attempt = NonZeroU32::new(stored.2).ok_or(StoreError::InvalidValue {
                field: "Agent operation delivery attempt",
                value: stored.2.to_string(),
                reason: "delivery attempt must be positive",
            })?;
            let request = serde_json::from_str(&stored.0)?;
            let failure = stored
                .6
                .map(|value| serde_json::from_str(&value))
                .transpose()?;
            let result = decode_operation_result(
                connection,
                key,
                &request,
                failure.as_ref(),
                stored.4.as_deref(),
                stored.5.as_deref(),
            )?;
            Ok(AgentOperation {
                key: key.clone(),
                request,
                delivery: serde_json::from_value(serde_json::Value::String(stored.1))?,
                delivery_attempt,
                state: serde_json::from_value(serde_json::Value::String(stored.3))?,
                result,
                failure,
                revision: stored.7,
            })
        })
    }

    pub fn preceding_tool_operation(
        &self,
        key: &AgentOperationKey,
    ) -> crate::store::Result<Option<AgentOperation>> {
        let ordinal = self.read(|connection| {
            Ok(connection
                .query_row(
                    "SELECT ordinal FROM agent_operations
                     WHERE agent_run_id=?1 AND ordinal<?2 AND kind='tool'
                     ORDER BY ordinal DESC LIMIT 1",
                    params![key.run_id.as_bytes().as_slice(), key.ordinal.get()],
                    |row| row.get::<_, u32>(0),
                )
                .optional()?)
        })?;
        let Some(ordinal) = ordinal else {
            return Ok(None);
        };
        let ordinal = NonZeroU32::new(ordinal).ok_or_else(|| StoreError::InvalidValue {
            field: "preceding Tool operation ordinal",
            value: ordinal.to_string(),
            reason: "operation ordinal must be positive",
        })?;
        let previous = self.agent_operation(&AgentOperationKey {
            run_id: key.run_id,
            ordinal,
        })?;
        if !matches!(previous.request, AgentOperationRequest::Tool(_)) {
            return Err(StoreError::InvalidValue {
                field: "preceding Tool operation",
                value: format!("{:?}", previous.key),
                reason: "the preceding operation is not a Tool operation",
            });
        }
        Ok(Some(previous))
    }

    pub fn agent_operation_retry_at_ms(
        &self,
        key: &AgentOperationKey,
    ) -> crate::store::Result<Option<i64>> {
        self.read(|connection| {
            connection
                .query_row(
                    "SELECT retry_at_ms FROM agent_operations
                     WHERE agent_run_id=?1 AND ordinal=?2",
                    params![key.run_id.as_bytes().as_slice(), key.ordinal.get()],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound {
                    resource: format!("Agent operation {key:?}"),
                })
        })
    }

    pub fn agent_context(
        &self,
        message_ids: &[MessageId],
    ) -> crate::store::Result<Vec<AgentContextEntry>> {
        if message_ids.len() > 10_000 {
            return Err(StoreError::InvalidValue {
                field: "Agent context",
                value: message_ids.len().to_string(),
                reason: "context exceeds 10000 messages",
            });
        }
        self.read(|connection| {
            let mut statement = connection.prepare(
                "SELECT m.conversation_id,m.agent_run_id,m.source_operation_ordinal,
                        m.role,m.visibility,m.content_json,o.request_json,
                        o.result_json,previous.result_json
                 FROM agent_messages m
                 LEFT JOIN agent_operations o
                   ON o.agent_run_id=m.agent_run_id
                  AND o.ordinal=m.source_operation_ordinal
                 LEFT JOIN agent_operations previous
                   ON previous.agent_run_id=m.agent_run_id
                  AND previous.ordinal=m.source_operation_ordinal-1
                 WHERE m.message_id=?1",
            )?;
            let mut context = Vec::with_capacity(message_ids.len());
            for message_id in message_ids {
                let row = statement
                    .query_row([message_id.as_str()], |row| {
                        Ok((
                            row.get::<_, Option<Vec<u8>>>(0)?,
                            row.get::<_, Option<Vec<u8>>>(1)?,
                            row.get::<_, Option<u32>>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, Option<String>>(6)?,
                            row.get::<_, Option<String>>(7)?,
                            row.get::<_, Option<String>>(8)?,
                        ))
                    })
                    .optional()?
                    .ok_or_else(|| StoreError::NotFound {
                        resource: format!("Agent message {message_id}"),
                    })?;
                let conversation_id =
                    decode_optional_id(row.0, "ConversationId", ConversationId::from_bytes)?;
                let run_id = decode_optional_id(row.1, "AgentRunId", AgentRunId::from_bytes)?;
                let source_operation = match (run_id, row.2) {
                    (Some(run_id), Some(ordinal)) => Some(AgentOperationKey {
                        run_id,
                        ordinal: NonZeroU32::new(ordinal).ok_or(StoreError::InvalidValue {
                            field: "Agent operation ordinal",
                            value: ordinal.to_string(),
                            reason: "operation ordinal must be positive",
                        })?,
                    }),
                    (None, None) | (Some(_), None) => None,
                    (None, Some(_)) => {
                        return Err(StoreError::InvalidValue {
                            field: "Agent context",
                            value: message_id.to_string(),
                            reason: "operation ordinal has no AgentRunId",
                        });
                    }
                };
                let role = serde_json::from_value(serde_json::Value::String(row.3))?;
                let entry = AgentContextEntry {
                    message: AgentMessage {
                        id: message_id.clone(),
                        conversation_id,
                        run_id,
                        source_operation: source_operation.clone(),
                        role,
                        visibility: serde_json::from_value(serde_json::Value::String(row.4))?,
                        content: serde_json::from_str(&row.5)?,
                    },
                    source_request: row.6.map(|json| serde_json::from_str(&json)).transpose()?,
                    private_reasoning: context_private_reasoning(
                        role,
                        row.7.as_deref(),
                        row.8.as_deref(),
                    )?,
                };
                entry
                    .validate()
                    .map_err(|reason| StoreError::InvalidValue {
                        field: "Agent context",
                        value: message_id.to_string(),
                        reason,
                    })?;
                context.push(entry);
            }
            Ok(context)
        })
    }

    pub fn agent_run_view(&self, run_id: AgentRunId) -> crate::store::Result<AgentRunView> {
        self.read(|connection| {
        let (origin, status, usage, checkpoint, last_event): (String, String, AgentRunUsage, String, u64) = connection.query_row(
            "SELECT r.origin_json, r.status,
                    r.usage_model_input_tokens, r.usage_model_output_tokens,
                    r.usage_model_calls, r.usage_correction_input_tokens,
                    r.usage_correction_output_tokens, r.usage_correction_calls,
                    r.usage_tool_calls,
                    r.checkpoint_json,
                    (SELECT max(agent_event_id) FROM agent_events e WHERE e.agent_run_id=r.agent_run_id)
             FROM agent_runs r WHERE r.agent_run_id=?1",
            [run_id.as_bytes().as_slice()],
            |row| Ok((
                row.get(0)?,
                row.get(1)?,
                AgentRunUsage {
                    model_input_tokens: row.get(2)?,
                    model_output_tokens: row.get(3)?,
                    model_calls: row.get(4)?,
                    correction_input_tokens: row.get(5)?,
                    correction_output_tokens: row.get(6)?,
                    correction_calls: row.get(7)?,
                    tool_calls: row.get(8)?,
                },
                row.get(9)?,
                row.get(10)?,
            )),
        )?;
        let checkpoint: AgentCheckpoint = serde_json::from_str(&checkpoint)?;
        let current_operation = match &checkpoint {
            AgentCheckpoint::Waiting { operation, .. } => Some(operation.clone()),
            AgentCheckpoint::Ready { .. } => None,
        };
        let failure = current_operation
            .as_ref()
            .map(|operation| {
                connection
                    .query_row(
                        "SELECT request_json, failure_json FROM agent_operations
                         WHERE agent_run_id=?1 AND ordinal=?2 AND failure_json IS NOT NULL",
                        params![run_id.as_bytes().as_slice(), operation.ordinal.get()],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
                    .map_err(StoreError::from)
                    .and_then(|row| {
                        row.map(|(request, failure)| {
                            let request: AgentOperationRequest = serde_json::from_str(&request)?;
                            let failure: AgentOperationFailure = serde_json::from_str(&failure)?;
                            Ok(AgentRunFailureView::Operation {
                                operation: operation.clone(),
                                kind: failure.kind,
                                tool_id: match request {
                                    AgentOperationRequest::Tool(request) => Some(request.tool_id),
                                    AgentOperationRequest::ModelGeneration(_) | AgentOperationRequest::Compaction(_) => None,
                                },
                            })
                        })
                        .transpose()
                    })
            })
            .transpose()?
            .flatten();
        let failure = if failure.is_some() || status != "failed" {
            failure
        } else {
            connection
                .query_row(
                    "SELECT data_json FROM agent_events
                     WHERE agent_run_id=?1
                       AND json_extract(data_json, '$.type')='run_status_changed'
                       AND json_extract(data_json, '$.failure_kind') IS NOT NULL
                     ORDER BY agent_event_id DESC LIMIT 1",
                    [run_id.as_bytes().as_slice()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .map(|json| -> crate::store::Result<_> {
                    let event: AgentEventData = serde_json::from_str(&json)?;
                    let AgentEventData::RunStatusChanged {
                        failure_kind: Some(kind),
                        ..
                    } = event
                    else {
                        return Err(StoreError::InvalidValue {
                            field: "Agent Run failure event",
                            value: json,
                            reason: "event has no Run failure kind",
                        });
                    };
                    Ok(AgentRunFailureView::Run { kind })
                })
                .transpose()?
        };
        Ok(AgentRunView {
            id: run_id,
            origin: serde_json::from_str(&origin)?,
            status: serde_json::from_value(serde_json::Value::String(status))?,
            usage,
            current_operation,
            failure,
            last_event_id: AgentEventId(last_event),
        })
        })
    }

    pub fn agent_run_deadline_at_ms(&self, run_id: AgentRunId) -> crate::store::Result<i64> {
        self.read(|connection| {
            connection
                .query_row(
                    "SELECT deadline_at_ms FROM agent_runs WHERE agent_run_id=?1",
                    [run_id.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .map_err(StoreError::from)
        })
    }

    pub fn agent_event_page(
        &self,
        after: Option<AgentEventId>,
        limit: usize,
    ) -> crate::store::Result<AgentEventPage> {
        if !(1..=1_000).contains(&limit) {
            return Err(StoreError::InvalidValue {
                field: "agent event page limit",
                value: limit.to_string(),
                reason: "limit must be between 1 and 1000",
            });
        }
        self.read(|connection| {
            let mut statement = connection.prepare(
                "SELECT agent_event_id, agent_run_id, operation_ordinal, run_revision,
                        operation_revision, committed_at_ms, data_json
             FROM agent_events WHERE agent_event_id > ?1 ORDER BY agent_event_id LIMIT ?2",
            )?;
            let rows = statement.query_map(
                params![after.map_or(0, |id| id.0), limit as u64 + 1],
                |row| {
                    Ok((
                        row.get::<_, u64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Option<u32>>(2)?,
                        row.get::<_, u64>(3)?,
                        row.get::<_, Option<u64>>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )?;
            let mut events = Vec::with_capacity(limit + 1);
            for row in rows {
                let (
                    id,
                    run_id,
                    operation_ordinal,
                    run_revision,
                    operation_revision,
                    committed_at_ms,
                    data,
                ) = row?;
                let bytes: [u8; 16] = run_id.try_into().map_err(|_| StoreError::InvalidValue {
                    field: "agent event run ID",
                    value: "invalid blob".into(),
                    reason: "AgentRunId must contain 16 bytes",
                })?;
                let agent_run_id =
                    AgentRunId::from_bytes(bytes).map_err(|_| StoreError::InvalidValue {
                        field: "agent event run ID",
                        value: "invalid UUID".into(),
                        reason: "AgentRunId must be UUIDv7",
                    })?;
                events.push(AgentEvent {
                    id: AgentEventId(id),
                    agent_run_id,
                    operation: operation_ordinal.and_then(NonZeroU32::new).map(|ordinal| {
                        AgentOperationKey {
                            run_id: agent_run_id,
                            ordinal,
                        }
                    }),
                    run_revision,
                    operation_revision,
                    committed_at_ms,
                    data: serde_json::from_str(&data)?,
                });
            }
            let next_cursor = (events.len() > limit).then(|| events[limit - 1].id);
            events.truncate(limit);
            Ok(AgentEventPage {
                events,
                next_cursor,
            })
        })
    }

    pub fn conversation_messages(
        &self,
        conversation_id: ConversationId,
        after: Option<&MessageId>,
        limit: usize,
    ) -> crate::store::Result<AgentMessagePage> {
        if !(1..=1_000).contains(&limit) {
            return Err(StoreError::InvalidValue {
                field: "Agent message page limit",
                value: limit.to_string(),
                reason: "limit must be between 1 and 1000",
            });
        }
        self.read(|connection| {
            let after_sequence = message_cursor_sequence(connection, conversation_id, after)?;
            let mut statement = connection.prepare(
                "SELECT m.message_id, m.agent_run_id, m.source_operation_ordinal,
                        m.role, m.content_json, o.request_json
                 FROM agent_messages m
                 LEFT JOIN agent_operations o
                   ON o.agent_run_id=m.agent_run_id
                  AND o.ordinal=m.source_operation_ordinal
                 WHERE m.conversation_id=?1 AND m.visibility='conversation'
                   AND m.message_sequence > ?2
                 ORDER BY m.message_sequence LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![
                    conversation_id.as_bytes().as_slice(),
                    after_sequence,
                    limit as u64 + 1,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                        row.get::<_, Option<u32>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )?;
            let mut messages = Vec::with_capacity(limit + 1);
            for row in rows {
                messages.push(decode_conversation_message(conversation_id, row?)?);
            }
            let next_cursor = (messages.len() > limit).then(|| messages[limit - 1].id.clone());
            messages.truncate(limit);
            Ok(AgentMessagePage {
                messages,
                next_cursor,
            })
        })
    }

    pub fn create_conversation(
        &self,
        conversation_id: ConversationId,
        function: &ExactPackageRef,
        snapshot: &AgentRunSnapshot,
    ) -> crate::store::Result<ConversationView> {
        let workspace_root = snapshot.workspace.root.as_path().to_string_lossy();
        if !snapshot.workspace.root.as_path().is_absolute() {
            return Err(StoreError::InvalidValue {
                field: "Conversation workspace",
                value: workspace_root.into_owned(),
                reason: "workspace root must be absolute",
            });
        }
        let store = self.lock()?;
        store.transaction(|tx| {
            create_agent_schema(tx)?;
            let now = unix_ms();
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO agent_conversations (
                    conversation_id, display_name, function_json, workspace_root,
                    snapshot_json, created_at_ms, last_active_at_ms
                 ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?5)",
                params![
                    conversation_id.as_bytes().as_slice(),
                    strict_json(function)?,
                    workspace_root.as_ref(),
                    strict_json(snapshot)?,
                    now,
                ],
            )?;
            if inserted != 1 {
                return Err(StoreError::TransitionRejected {
                    resource: format!("Conversation {conversation_id}"),
                    from: "bound".into(),
                    to: "rebound".into(),
                });
            }
            Ok(ConversationView {
                id: conversation_id,
                display_name: None,
                function: function.clone(),
                reasoning: snapshot.model.runtime.reasoning,
                presentation: snapshot.presentation.clone(),
                created_at_ms: now,
                last_active_at_ms: now,
            })
        })
    }

    pub fn conversation_binding(
        &self,
        conversation_id: ConversationId,
    ) -> crate::store::Result<ConversationBinding> {
        self.read(|connection| load_conversation_binding(connection, conversation_id))
    }

    pub fn resolve_conversation(&self, selector: &str) -> crate::store::Result<ConversationView> {
        if selector.is_empty() || selector.len() > 256 {
            return Err(StoreError::InvalidValue {
                field: "Conversation selector",
                value: selector.to_owned(),
                reason: "selector must contain 1 to 256 UTF-8 bytes",
            });
        }
        self.read(|connection| {
            let id = conversation_id_for_selector(connection, selector)?;
            Ok(load_conversation_binding(connection, id)?.view)
        })
    }

    pub fn conversations(
        &self,
        workspace_root: Option<&std::path::Path>,
        limit: usize,
    ) -> crate::store::Result<Vec<ConversationView>> {
        if !(1..=1_000).contains(&limit) {
            return Err(StoreError::InvalidValue {
                field: "Conversation page limit",
                value: limit.to_string(),
                reason: "limit must be between 1 and 1000",
            });
        }
        self.read(|connection| {
            let sql = "SELECT conversation_id, display_name, function_json,
                              created_at_ms, last_active_at_ms,
                              json_extract(snapshot_json, '$.model.runtime.reasoning'),
                              json_extract(snapshot_json, '$.presentation')
                       FROM agent_conversations
                       WHERE (?1 IS NULL OR workspace_root=?1)
                       ORDER BY last_active_at_ms DESC, conversation_id DESC LIMIT ?2";
            let workspace = workspace_root.map(|path| path.to_string_lossy().into_owned());
            let mut statement = connection.prepare(sql)?;
            let mut rows = statement.query(params![workspace, limit as u64])?;
            let mut conversations = Vec::new();
            while let Some(row) = rows.next()? {
                conversations.push(decode_conversation_view(
                    (
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ),
                    serde_json::from_str(&row.get::<_, String>(5)?)?,
                    serde_json::from_str(&row.get::<_, String>(6)?)?,
                )?);
            }
            Ok(conversations)
        })
    }

    pub fn rename_conversation(
        &self,
        selector: &str,
        display_name: &str,
    ) -> crate::store::Result<ConversationView> {
        let display_name = display_name.trim();
        if display_name.is_empty() || display_name.len() > 256 {
            return Err(StoreError::InvalidValue {
                field: "Conversation display name",
                value: display_name.to_owned(),
                reason: "name must contain 1 to 256 UTF-8 bytes after trimming",
            });
        }
        if ConversationId::parse(display_name).is_ok() {
            return Err(StoreError::InvalidValue {
                field: "Conversation display name",
                value: display_name.to_owned(),
                reason: "name must not be a Conversation ID",
            });
        }
        let store = self.lock()?;
        store.transaction(|tx| {
            create_agent_schema(tx)?;
            let id = conversation_id_for_selector(tx, selector)?;
            let collision = tx
                .query_row(
                    "SELECT conversation_id FROM agent_conversations
                     WHERE display_name=?1 AND conversation_id<>?2",
                    params![display_name, id.as_bytes().as_slice()],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()?;
            if collision.is_some() {
                return Err(StoreError::TransitionRejected {
                    resource: format!("Conversation name {display_name:?}"),
                    from: "occupied".into(),
                    to: "renamed".into(),
                });
            }
            tx.execute(
                "UPDATE agent_conversations SET display_name=?1 WHERE conversation_id=?2",
                params![display_name, id.as_bytes().as_slice()],
            )?;
            Ok(load_conversation_binding(tx, id)?.view)
        })
    }

    pub fn resolve_agent_run_snapshot(
        &self,
        origin: &agl_core::agent::AgentRunOrigin,
    ) -> crate::store::Result<AgentRunSnapshot> {
        self.read(|connection| {
            let agl_core::agent::AgentRunOrigin::User {
                conversation_id, ..
            } = origin;
            let snapshot = connection
                .query_row(
                    "SELECT snapshot_json FROM agent_conversations WHERE conversation_id=?1",
                    [conversation_id.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound {
                    resource: format!("Conversation {conversation_id}"),
                })?;
            decode_snapshot(snapshot)
        })
    }

    /// Return an already admitted natural-origin Run without consulting the
    /// mutable Conversation configuration used for first admission.
    pub fn replay_agent_run(&self, spec: &AgentRunSpec) -> crate::store::Result<Option<AgentRun>> {
        spec.input.validate().map_err(StoreError::Content)?;
        self.read(|connection| existing_agent_run(connection, spec))
    }
}
