use std::num::NonZeroU32;
use std::path::Path;

use agl_core::Content;
use agl_core::agent::{
    AgentCheckpoint, AgentContextEntry, AgentEvent, AgentEventData, AgentEventId, AgentEventPage,
    AgentFsmOutput, AgentFsmState, AgentMessage, AgentMessagePage, AgentOperation,
    AgentOperationFailure, AgentOperationFailureKind, AgentOperationFsmOutput, AgentOperationKey,
    AgentOperationRequest, AgentOperationResult, AgentRun, AgentRunFailureView, AgentRunSnapshot,
    AgentRunSpec, AgentRunStatus, AgentRunUsage, AgentRunView, ConversationBinding,
    ConversationView, EffectReceipt, ExactPackageRef, INVALID_MODEL_OUTPUT_CORRECTION,
    InferenceRealizationRef, MemoryTopic, MessageRole, MessageVisibility, ModelFinishReason,
    ModelGenerationOutput, ModelGenerationResult, ModelUsage, ToolCall, ToolResult,
};
use agl_core::{AgentRunId, ConversationId, MessageId};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

const MAX_STORED_JSON_BYTES: usize = 72 * 1024 * 1024;
const MAX_STORED_JSON_DEPTH: usize = 64;

use crate::store::{StoreError, StoreHandle};
mod compaction;

#[derive(Clone, Debug, PartialEq)]
pub enum AgentRunAdmission {
    Created(AgentRun),
    Replayed(AgentRun),
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "result",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum StoredAgentOperationResult {
    ModelGeneration(StoredModelGenerationResult),
    Compaction(Box<agl_core::agent::CompactionMetadata>),
    Tool(StoredToolResult),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredModelGenerationResult {
    output: StoredModelGenerationOutput,
    private_reasoning: Option<Content>,
    finish_reason: ModelFinishReason,
    usage: ModelUsage,
    realization: InferenceRealizationRef,
    correction: Option<Box<agl_core::agent::ModelCorrectionRecord>>,
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum StoredModelGenerationOutput {
    Assistant,
    ToolCall(ToolCall),
    ToolCalls(Vec<ToolCall>),
    AssistantToolCall(ToolCall),
    AssistantToolCalls(Vec<ToolCall>),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredToolResult {
    effect_receipts: Vec<EffectReceipt>,
}

fn decode_operation_result(
    connection: &rusqlite::Connection,
    key: &AgentOperationKey,
    request: &AgentOperationRequest,
    failure: Option<&AgentOperationFailure>,
    result_json: Option<&str>,
    result_message_id: Option<&str>,
) -> crate::store::Result<Option<AgentOperationResult>> {
    let Some(result_json) = result_json else {
        if let Some(message_id) = result_message_id
            && (!matches!(
                (request, failure),
                (
                    AgentOperationRequest::ModelGeneration(_),
                    Some(AgentOperationFailure {
                        kind: AgentOperationFailureKind::InvalidResult
                            | AgentOperationFailureKind::InvalidModelOutput { .. }
                            | AgentOperationFailureKind::CorrectionFailed { .. }
                    })
                )
            ) || load_operation_message_content(
                connection,
                key,
                Some(message_id),
                MessageRole::Assistant,
            )?
            .as_text()
                != INVALID_MODEL_OUTPUT_CORRECTION)
        {
            return Err(StoreError::InvalidValue {
                field: "Agent operation result",
                value: format!("{key:?}"),
                reason: "message reference without result metadata must be an invalid model output correction",
            });
        }
        return Ok(None);
    };
    let stored: StoredAgentOperationResult = serde_json::from_str(result_json)?;
    let result = match stored {
        StoredAgentOperationResult::Compaction(metadata) => {
            let content = load_operation_message_content(
                connection,
                key,
                result_message_id,
                MessageRole::Assistant,
            )?;
            AgentOperationResult::Compaction(Box::new(agl_core::agent::CompactionResult {
                content: serde_json::from_str(content.as_text())?,
                metadata: *metadata,
            }))
        }
        StoredAgentOperationResult::ModelGeneration(result) => {
            let output = match result.output {
                StoredModelGenerationOutput::Assistant => {
                    ModelGenerationOutput::Assistant(load_operation_message_content(
                        connection,
                        key,
                        result_message_id,
                        MessageRole::Assistant,
                    )?)
                }
                StoredModelGenerationOutput::ToolCall(call) => {
                    if result_message_id.is_some() {
                        return Err(StoreError::InvalidValue {
                            field: "Agent operation result",
                            value: format!("{key:?}"),
                            reason: "Tool call result cannot reference a message",
                        });
                    }
                    ModelGenerationOutput::ToolCall(call)
                }
                StoredModelGenerationOutput::ToolCalls(calls) => {
                    if result_message_id.is_some() {
                        return Err(StoreError::InvalidValue {
                            field: "Agent operation result",
                            value: format!("{key:?}"),
                            reason: "Tool call result cannot reference a message",
                        });
                    }
                    ModelGenerationOutput::ToolCalls(calls)
                }
                StoredModelGenerationOutput::AssistantToolCall(call) => {
                    let content = load_operation_message_content(
                        connection,
                        key,
                        result_message_id,
                        MessageRole::Assistant,
                    )?;
                    ModelGenerationOutput::AssistantToolCall(agl_core::agent::AssistantToolCall {
                        content,
                        call,
                    })
                }
                StoredModelGenerationOutput::AssistantToolCalls(calls) => {
                    let content = load_operation_message_content(
                        connection,
                        key,
                        result_message_id,
                        MessageRole::Assistant,
                    )?;
                    ModelGenerationOutput::AssistantToolCalls { content, calls }
                }
            };
            AgentOperationResult::ModelGeneration(ModelGenerationResult {
                output,
                private_reasoning: result.private_reasoning,
                finish_reason: result.finish_reason,
                usage: result.usage,
                realization: result.realization,
                correction: result.correction,
            })
        }
        StoredAgentOperationResult::Tool(result) => AgentOperationResult::Tool(ToolResult {
            content: load_operation_message_content(
                connection,
                key,
                result_message_id,
                MessageRole::Tool,
            )?,
            effect_receipts: result.effect_receipts,
        }),
    };
    Ok(Some(result))
}

fn context_private_reasoning(
    role: MessageRole,
    current_result: Option<&str>,
    previous_result: Option<&str>,
) -> crate::store::Result<Option<Content>> {
    let encoded = match role {
        MessageRole::User => return Ok(None),
        MessageRole::Assistant => current_result,
        MessageRole::Tool => previous_result,
    };
    let Some(encoded) = encoded else {
        return Ok(None);
    };
    match serde_json::from_str::<StoredAgentOperationResult>(encoded)? {
        StoredAgentOperationResult::ModelGeneration(result) => Ok(result.private_reasoning),
        StoredAgentOperationResult::Tool(_) => Ok(None),
        StoredAgentOperationResult::Compaction(_) => Ok(None),
    }
}

fn load_operation_message_content(
    connection: &rusqlite::Connection,
    key: &AgentOperationKey,
    result_message_id: Option<&str>,
    expected_role: MessageRole,
) -> crate::store::Result<Content> {
    let message_id = result_message_id.ok_or(StoreError::InvalidValue {
        field: "Agent operation result",
        value: format!("{key:?}"),
        reason: "content result requires a message reference",
    })?;
    let (run_id, ordinal, role, content): (Vec<u8>, u32, String, String) = connection.query_row(
        "SELECT agent_run_id, source_operation_ordinal, role, content_json
         FROM agent_messages WHERE message_id=?1",
        [message_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    if run_id.as_slice() != key.run_id.as_bytes()
        || ordinal != key.ordinal.get()
        || serde_json::from_value::<MessageRole>(serde_json::Value::String(role))? != expected_role
    {
        return Err(StoreError::InvalidValue {
            field: "Agent operation result message",
            value: message_id.to_owned(),
            reason: "message correlation does not match its operation result",
        });
    }
    serde_json::from_str(&content).map_err(StoreError::from)
}

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

fn encode_operation_result(
    operation: &AgentOperation,
    message: Option<&AgentMessage>,
) -> crate::store::Result<(Option<String>, Option<MessageId>)> {
    let Some(result) = &operation.result else {
        if let Some(message) = message {
            return Ok((
                None,
                Some(validate_invalid_model_output_message(operation, message)?),
            ));
        }
        return Ok((None, None));
    };
    let (stored, message_id) = match result {
        AgentOperationResult::Compaction(result) => (
            StoredAgentOperationResult::Compaction(Box::new(result.metadata.clone())),
            Some(validate_result_message(
                operation,
                message,
                MessageRole::Assistant,
                &result
                    .content
                    .render()
                    .map_err(|_| StoreError::InvalidValue {
                        field: "Compaction content",
                        value: format!("{:?}", operation.key),
                        reason: "invalid compaction content",
                    })?,
            )?),
        ),
        AgentOperationResult::ModelGeneration(result) => {
            let (output, message_id) = match &result.output {
                ModelGenerationOutput::Assistant(content) => (
                    StoredModelGenerationOutput::Assistant,
                    Some(validate_result_message(
                        operation,
                        message,
                        MessageRole::Assistant,
                        content,
                    )?),
                ),
                ModelGenerationOutput::ToolCall(call) => {
                    if message.is_some() {
                        return Err(StoreError::InvalidValue {
                            field: "Agent operation result",
                            value: format!("{:?}", operation.key),
                            reason: "Tool call result cannot create a message",
                        });
                    }
                    (StoredModelGenerationOutput::ToolCall(call.clone()), None)
                }
                ModelGenerationOutput::ToolCalls(calls) => {
                    if message.is_some() || calls.is_empty() {
                        return Err(StoreError::InvalidValue {
                            field: "Agent operation result",
                            value: format!("{:?}", operation.key),
                            reason: "invalid tool call batch message",
                        });
                    }
                    (StoredModelGenerationOutput::ToolCalls(calls.clone()), None)
                }
                ModelGenerationOutput::AssistantToolCall(mixed) => (
                    StoredModelGenerationOutput::AssistantToolCall(mixed.call.clone()),
                    Some(validate_result_message(
                        operation,
                        message,
                        MessageRole::Assistant,
                        &mixed.content,
                    )?),
                ),
                ModelGenerationOutput::AssistantToolCalls { content, calls } => {
                    if calls.is_empty() {
                        return Err(StoreError::InvalidValue {
                            field: "Agent operation result",
                            value: format!("{:?}", operation.key),
                            reason: "empty assistant tool call batch",
                        });
                    }
                    (
                        StoredModelGenerationOutput::AssistantToolCalls(calls.clone()),
                        Some(validate_result_message(
                            operation,
                            message,
                            MessageRole::Assistant,
                            content,
                        )?),
                    )
                }
            };
            (
                StoredAgentOperationResult::ModelGeneration(StoredModelGenerationResult {
                    output,
                    private_reasoning: result.private_reasoning.clone(),
                    finish_reason: result.finish_reason,
                    usage: result.usage,
                    realization: result.realization.clone(),
                    correction: result.correction.clone(),
                }),
                message_id,
            )
        }
        AgentOperationResult::Tool(result) => (
            StoredAgentOperationResult::Tool(StoredToolResult {
                effect_receipts: result.effect_receipts.clone(),
            }),
            Some(validate_result_message(
                operation,
                message,
                MessageRole::Tool,
                &result.content,
            )?),
        ),
    };
    Ok((Some(strict_json(&stored)?), message_id))
}

fn validate_invalid_model_output_message(
    operation: &AgentOperation,
    message: &AgentMessage,
) -> crate::store::Result<MessageId> {
    let valid_operation = matches!(
        (&operation.request, operation.failure.as_ref()),
        (
            AgentOperationRequest::ModelGeneration(_),
            Some(AgentOperationFailure {
                kind: AgentOperationFailureKind::InvalidResult
                    | AgentOperationFailureKind::InvalidModelOutput { .. }
                    | AgentOperationFailureKind::CorrectionFailed { .. }
            })
        )
    );
    if !valid_operation
        || message.run_id != Some(operation.key.run_id)
        || message.source_operation.as_ref() != Some(&operation.key)
        || message.role != MessageRole::Assistant
        || message.visibility != MessageVisibility::Internal
        || message.content.as_text() != INVALID_MODEL_OUTPUT_CORRECTION
    {
        return Err(StoreError::InvalidValue {
            field: "Agent operation correction message",
            value: message.id.to_string(),
            reason: "message does not exactly describe an invalid model output",
        });
    }
    message
        .validate()
        .map_err(|reason| StoreError::InvalidValue {
            field: "Agent operation correction message",
            value: message.id.to_string(),
            reason,
        })?;
    Ok(message.id.clone())
}

fn validate_result_message(
    operation: &AgentOperation,
    message: Option<&AgentMessage>,
    role: MessageRole,
    content: &Content,
) -> crate::store::Result<MessageId> {
    let message = message.ok_or(StoreError::InvalidValue {
        field: "Agent operation result",
        value: format!("{:?}", operation.key),
        reason: "content result requires a message",
    })?;
    if message.run_id != Some(operation.key.run_id)
        || message.source_operation.as_ref() != Some(&operation.key)
        || message.role != role
        || &message.content != content
    {
        return Err(StoreError::InvalidValue {
            field: "Agent operation result message",
            value: message.id.to_string(),
            reason: "message does not exactly match its operation result",
        });
    }
    Ok(message.id.clone())
}

fn insert_agent_message(
    tx: &rusqlite::Transaction<'_>,
    message: &AgentMessage,
) -> crate::store::Result<()> {
    message
        .validate()
        .map_err(|reason| StoreError::InvalidValue {
            field: "agent message",
            value: message.id.to_string(),
            reason,
        })?;
    tx.execute(
        "INSERT INTO agent_messages (
            message_id, conversation_id, agent_run_id, source_operation_ordinal,
            role, visibility, content_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            message.id.as_str(),
            message
                .conversation_id
                .as_ref()
                .map(|id| id.as_bytes().as_slice()),
            message.run_id.as_ref().map(|id| id.as_bytes().as_slice()),
            message
                .source_operation
                .as_ref()
                .map(|key| key.ordinal.get()),
            enum_text(&message.role)?,
            enum_text(&message.visibility)?,
            strict_json(&message.content)?,
        ],
    )?;
    Ok(())
}

type StoredConversationView = (Vec<u8>, Option<String>, String, i64, i64);

fn decode_conversation_view(
    stored: StoredConversationView,
    reasoning: agl_core::agent::ReasoningSelection,
    presentation: agl_core::agent::AgentPresentation,
) -> crate::store::Result<ConversationView> {
    let id = decode_optional_id(Some(stored.0), "ConversationId", ConversationId::from_bytes)?
        .expect("stored Conversation ID is not optional");
    Ok(ConversationView {
        id,
        display_name: stored.1,
        function: serde_json::from_str(&stored.2)?,
        reasoning,
        presentation,
        created_at_ms: stored.3,
        last_active_at_ms: stored.4,
    })
}

fn load_conversation_binding(
    connection: &rusqlite::Connection,
    conversation_id: ConversationId,
) -> crate::store::Result<ConversationBinding> {
    let stored = connection
        .query_row(
            "SELECT conversation_id, display_name, function_json,
                    created_at_ms, last_active_at_ms, snapshot_json
             FROM agent_conversations WHERE conversation_id=?1",
            [conversation_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound {
            resource: format!("Conversation {conversation_id}"),
        })?;
    let snapshot = decode_snapshot(stored.5)?;
    Ok(ConversationBinding {
        view: decode_conversation_view(
            (stored.0, stored.1, stored.2, stored.3, stored.4),
            snapshot.model.runtime.reasoning,
            snapshot.presentation.clone(),
        )?,
        snapshot,
    })
}

fn conversation_id_for_selector(
    connection: &rusqlite::Connection,
    selector: &str,
) -> crate::store::Result<ConversationId> {
    if let Ok(id) = ConversationId::parse(selector) {
        let exists = connection
            .query_row(
                "SELECT 1 FROM agent_conversations WHERE conversation_id=?1",
                [id.as_bytes().as_slice()],
                |_| Ok(()),
            )
            .optional()?;
        return exists.map(|()| id).ok_or_else(|| StoreError::NotFound {
            resource: format!("Conversation {selector}"),
        });
    }
    let bytes = connection
        .query_row(
            "SELECT conversation_id FROM agent_conversations WHERE display_name=?1",
            [selector],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound {
            resource: format!("Conversation {selector:?}"),
        })?;
    decode_optional_id(Some(bytes), "ConversationId", ConversationId::from_bytes)?.ok_or_else(
        || StoreError::InvalidValue {
            field: "ConversationId",
            value: selector.to_owned(),
            reason: "stored Conversation ID is missing",
        },
    )
}

type StoredConversationMessage = (
    String,
    Option<Vec<u8>>,
    Option<u32>,
    String,
    String,
    Option<String>,
);

fn decode_conversation_message(
    conversation_id: ConversationId,
    stored: StoredConversationMessage,
) -> crate::store::Result<AgentMessage> {
    let (message_id, run_id, ordinal, role, content, source_request) = stored;
    let run_id = decode_optional_id(run_id, "Agent message run ID", AgentRunId::from_bytes)?;
    let source_operation = match (run_id, ordinal) {
        (Some(run_id), Some(ordinal)) => Some(AgentOperationKey {
            run_id,
            ordinal: NonZeroU32::new(ordinal).ok_or(StoreError::InvalidValue {
                field: "Agent message operation ordinal",
                value: ordinal.to_string(),
                reason: "operation ordinal must be positive",
            })?,
        }),
        (_, None) => None,
        (None, Some(ordinal)) => {
            return Err(StoreError::InvalidValue {
                field: "Agent message operation ordinal",
                value: ordinal.to_string(),
                reason: "operation ordinal requires an AgentRunId",
            });
        }
    };
    let message = AgentMessage {
        id: MessageId::parse(&message_id).map_err(|_| StoreError::InvalidValue {
            field: "Agent message ID",
            value: message_id,
            reason: "message ID is invalid",
        })?,
        conversation_id: Some(conversation_id),
        run_id,
        source_operation,
        role: serde_json::from_value(serde_json::Value::String(role))?,
        visibility: agl_core::agent::MessageVisibility::Conversation,
        content: serde_json::from_str(&content)?,
    };
    let entry = AgentContextEntry {
        message,
        source_request: source_request
            .map(|request| serde_json::from_str(&request))
            .transpose()?,
        private_reasoning: None,
    };
    entry
        .validate()
        .map_err(|reason| StoreError::InvalidValue {
            field: "Agent message",
            value: entry.message.id.to_string(),
            reason,
        })?;
    Ok(entry.message)
}

fn message_cursor_sequence(
    connection: &rusqlite::Connection,
    conversation_id: ConversationId,
    cursor: Option<&MessageId>,
) -> crate::store::Result<i64> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    connection
        .query_row(
            "SELECT message_sequence FROM agent_messages
             WHERE conversation_id=?1 AND visibility='conversation' AND message_id=?2",
            params![conversation_id.as_bytes().as_slice(), cursor.as_str()],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound {
            resource: format!("conversation message {cursor}"),
        })
}

fn insert_agent_operation(
    tx: &rusqlite::Transaction<'_>,
    operation: &AgentOperation,
) -> crate::store::Result<()> {
    let (kind, tool_id) = match &operation.request {
        AgentOperationRequest::ModelGeneration(_) => ("model_generation", None),
        AgentOperationRequest::Compaction(_) => ("compaction", None),
        AgentOperationRequest::Tool(request) => ("tool", Some(request.tool_id.as_str())),
    };
    tx.execute(
        "INSERT INTO agent_operations (
            agent_run_id, ordinal, kind, tool_id, request_json, delivery,
            delivery_attempt, state, result_json, result_message_id, failure_json, revision
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, NULL, ?9)",
        params![
            operation.key.run_id.as_bytes().as_slice(),
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
    Ok(())
}

fn decode_optional_id<T>(
    value: Option<Vec<u8>>,
    field: &'static str,
    decode: impl FnOnce([u8; 16]) -> Result<T, agl_core::ParseIdError>,
) -> crate::store::Result<Option<T>> {
    value
        .map(|bytes| {
            let bytes: [u8; 16] = bytes.try_into().map_err(|_| StoreError::InvalidValue {
                field,
                value: "invalid blob".into(),
                reason: "compact ID must contain 16 bytes",
            })?;
            decode(bytes).map_err(|_| StoreError::InvalidValue {
                field,
                value: "invalid UUID".into(),
                reason: "compact ID must be UUIDv7",
            })
        })
        .transpose()
}

fn create_agent_schema(tx: &rusqlite::Transaction<'_>) -> crate::store::Result<()> {
    create_agent_schema_connection(tx)
}

pub(crate) fn create_agent_schema_connection(
    connection: &rusqlite::Connection,
) -> crate::store::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_runs (
            agent_run_id BLOB PRIMARY KEY CHECK(length(agent_run_id) = 16),
            origin_json TEXT NOT NULL CHECK(json_valid(origin_json) AND length(origin_json)<=75497472),
            origin_conversation_id BLOB NOT NULL CHECK(length(origin_conversation_id)=16),
            origin_message_id TEXT NOT NULL,
            input_json TEXT NOT NULL CHECK(json_valid(input_json) AND length(input_json)<=75497472),
            snapshot_json TEXT NOT NULL CHECK(json_valid(snapshot_json) AND length(snapshot_json)<=75497472),
            agent_package_digest BLOB NOT NULL CHECK(length(agent_package_digest)=32),
            model_package_digest BLOB NOT NULL CHECK(length(model_package_digest)=32),
            instruction_digest BLOB NOT NULL CHECK(length(instruction_digest)=32),
            status TEXT NOT NULL CHECK(status IN ('pending','running','completed','failed','cancelled')),
            checkpoint_json TEXT NOT NULL CHECK(json_valid(checkpoint_json) AND length(checkpoint_json)<=75497472),
            deadline_ms INTEGER NOT NULL CHECK(deadline_ms > 0),
            admitted_at_ms INTEGER NOT NULL CHECK(admitted_at_ms >= 0),
            deadline_at_ms INTEGER NOT NULL CHECK(deadline_at_ms > admitted_at_ms),
            limit_model_input_tokens INTEGER CHECK(limit_model_input_tokens > 0),
            limit_model_output_tokens INTEGER NOT NULL CHECK(limit_model_output_tokens >= 0),
            limit_model_calls INTEGER NOT NULL CHECK(limit_model_calls >= 0),
            limit_tool_calls INTEGER NOT NULL CHECK(limit_tool_calls >= 0),
            limit_tool_result_bytes INTEGER NOT NULL CHECK(limit_tool_result_bytes > 0 AND limit_tool_result_bytes <= 65536),
            usage_model_input_tokens INTEGER NOT NULL CHECK(usage_model_input_tokens >= 0),
            usage_model_output_tokens INTEGER NOT NULL CHECK(usage_model_output_tokens >= 0),
            usage_model_calls INTEGER NOT NULL CHECK(usage_model_calls >= 0),
            usage_correction_input_tokens INTEGER NOT NULL CHECK(usage_correction_input_tokens >= 0),
            usage_correction_output_tokens INTEGER NOT NULL CHECK(usage_correction_output_tokens >= 0),
            usage_correction_calls INTEGER NOT NULL CHECK(usage_correction_calls >= 0),
            usage_tool_calls INTEGER NOT NULL CHECK(usage_tool_calls >= 0),
            revision INTEGER NOT NULL CHECK(revision >= 0),
            FOREIGN KEY(origin_conversation_id) REFERENCES agent_conversations(conversation_id) ON DELETE CASCADE
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS agent_runs_origin_user
            ON agent_runs(origin_conversation_id, origin_message_id);
        CREATE TABLE IF NOT EXISTS agent_messages (
            message_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            message_id TEXT NOT NULL UNIQUE,
            conversation_id BLOB CHECK(conversation_id IS NULL OR length(conversation_id) = 16),
            agent_run_id BLOB CHECK(agent_run_id IS NULL OR length(agent_run_id) = 16),
            source_operation_ordinal INTEGER CHECK(source_operation_ordinal IS NULL OR source_operation_ordinal > 0),
            role TEXT NOT NULL CHECK(role IN ('user','assistant','tool')),
            visibility TEXT NOT NULL CHECK(visibility IN ('conversation','internal')),
            content_json TEXT NOT NULL CHECK(json_valid(content_json) AND length(content_json)<=75497472),
            FOREIGN KEY(conversation_id) REFERENCES agent_conversations(conversation_id) ON DELETE CASCADE,
            FOREIGN KEY(agent_run_id) REFERENCES agent_runs(agent_run_id) ON DELETE CASCADE
        ) STRICT;
        CREATE INDEX IF NOT EXISTS agent_messages_conversation_visibility_sequence
            ON agent_messages(conversation_id, visibility, message_sequence);
        CREATE TABLE IF NOT EXISTS agent_operations (
            agent_run_id BLOB NOT NULL CHECK(length(agent_run_id) = 16),
            ordinal INTEGER NOT NULL CHECK(ordinal > 0),
            kind TEXT NOT NULL CHECK(kind IN ('model_generation','compaction','tool')),
            tool_id TEXT,
            request_json TEXT NOT NULL CHECK(json_valid(request_json) AND length(request_json)<=75497472),
            delivery TEXT NOT NULL CHECK(delivery IN ('retryable','at_most_once')),
            delivery_attempt INTEGER NOT NULL CHECK(delivery_attempt > 0),
            state TEXT NOT NULL CHECK(state IN ('pending','running','retry_scheduled','succeeded','failed','outcome_unknown','cancelled')),
            result_json TEXT CHECK(result_json IS NULL OR (json_valid(result_json) AND length(result_json)<=75497472)),
            result_message_id TEXT UNIQUE,
            failure_json TEXT CHECK(failure_json IS NULL OR (json_valid(failure_json) AND length(failure_json)<=75497472)),
            retry_at_ms INTEGER CHECK(retry_at_ms IS NULL OR retry_at_ms >= 0),
            revision INTEGER NOT NULL CHECK(revision >= 0),
            CHECK((state='retry_scheduled') = (retry_at_ms IS NOT NULL)),
            PRIMARY KEY(agent_run_id, ordinal),
            FOREIGN KEY(agent_run_id) REFERENCES agent_runs(agent_run_id) ON DELETE CASCADE,
            FOREIGN KEY(result_message_id) REFERENCES agent_messages(message_id)
        ) STRICT;
        CREATE TABLE IF NOT EXISTS agent_events (
            agent_event_id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_run_id BLOB NOT NULL CHECK(length(agent_run_id) = 16),
            operation_ordinal INTEGER CHECK(operation_ordinal IS NULL OR operation_ordinal > 0),
            run_revision INTEGER NOT NULL CHECK(run_revision >= 0),
            operation_revision INTEGER CHECK(operation_revision IS NULL OR operation_revision >= 0),
            committed_at_ms INTEGER NOT NULL CHECK(committed_at_ms >= 0),
            data_json TEXT NOT NULL CHECK(json_valid(data_json) AND length(data_json)<=75497472),
            FOREIGN KEY(agent_run_id) REFERENCES agent_runs(agent_run_id) ON DELETE CASCADE
        ) STRICT;
        CREATE INDEX IF NOT EXISTS agent_events_run_cursor
            ON agent_events(agent_run_id, agent_event_id);
        CREATE TABLE IF NOT EXISTS agent_model_output_rejections (
            agent_event_id INTEGER PRIMARY KEY,
            agent_run_id BLOB NOT NULL CHECK(length(agent_run_id) = 16),
            operation_ordinal INTEGER NOT NULL CHECK(operation_ordinal > 0),
            delivery_attempt INTEGER NOT NULL CHECK(delivery_attempt > 0),
            correction_attempt INTEGER NOT NULL CHECK(correction_attempt >= 0),
            raw_output_json TEXT CHECK(raw_output_json IS NULL OR (json_valid(raw_output_json) AND length(raw_output_json)<=75497472)),
            FOREIGN KEY(agent_event_id) REFERENCES agent_events(agent_event_id) ON DELETE CASCADE,
            FOREIGN KEY(agent_run_id, operation_ordinal)
                REFERENCES agent_operations(agent_run_id, ordinal) ON DELETE CASCADE
        ) STRICT;
        CREATE INDEX IF NOT EXISTS agent_model_output_rejections_operation
            ON agent_model_output_rejections(agent_run_id, operation_ordinal, correction_attempt);
        CREATE TABLE IF NOT EXISTS agent_conversations (
            conversation_id BLOB PRIMARY KEY CHECK(length(conversation_id) = 16),
            display_name TEXT UNIQUE CHECK(
                display_name IS NULL OR (
                    length(CAST(display_name AS BLOB)) BETWEEN 1 AND 256
                    AND display_name=trim(display_name)
                )
            ),
            function_json TEXT NOT NULL CHECK(json_valid(function_json) AND length(function_json)<=4194304),
            workspace_root TEXT NOT NULL CHECK(length(workspace_root) BETWEEN 1 AND 32768),
            snapshot_json TEXT NOT NULL CHECK(json_valid(snapshot_json) AND length(snapshot_json)<=75497472),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            last_active_at_ms INTEGER NOT NULL CHECK(last_active_at_ms >= created_at_ms)
        ) STRICT;
        CREATE INDEX IF NOT EXISTS agent_conversations_workspace_activity
            ON agent_conversations(workspace_root, last_active_at_ms DESC);
        CREATE TABLE IF NOT EXISTS implementation_plans (
            plan_id BLOB PRIMARY KEY CHECK(length(plan_id)=16),
            state TEXT NOT NULL CHECK(state IN ('draft','awaiting_decisions','ready_for_approval','approved','implementing','completed','failed','stale')),
            draft_digest BLOB NOT NULL CHECK(length(draft_digest)=32),
            approved_digest BLOB CHECK(approved_digest IS NULL OR length(approved_digest)=32),
            workspace_root TEXT NOT NULL CHECK(length(workspace_root) BETWEEN 1 AND 32768),
            planner_conversation_id BLOB NOT NULL CHECK(length(planner_conversation_id)=16),
            initial_workspace_json TEXT CHECK(initial_workspace_json IS NULL OR (json_valid(initial_workspace_json) AND length(initial_workspace_json)<=75497472)),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
            CHECK((state IN ('approved','implementing','completed','failed','stale')) = (approved_digest IS NOT NULL)),
            FOREIGN KEY(planner_conversation_id) REFERENCES agent_conversations(conversation_id) ON DELETE CASCADE
        ) STRICT;
        CREATE INDEX IF NOT EXISTS implementation_plans_state_updated
            ON implementation_plans(state, updated_at_ms DESC);
        CREATE TABLE IF NOT EXISTS implementation_plan_slices (
            plan_id BLOB NOT NULL CHECK(length(plan_id)=16),
            slice_id TEXT NOT NULL CHECK(length(CAST(slice_id AS BLOB)) BETWEEN 1 AND 128),
            state TEXT NOT NULL CHECK(state IN ('pending','running','completed','failed','stale')),
            result_json TEXT CHECK(result_json IS NULL OR (json_valid(result_json) AND length(result_json)<=75497472)),
            stale_paths_json TEXT NOT NULL CHECK(json_valid(stale_paths_json) AND length(stale_paths_json)<=4194304),
            updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0),
            PRIMARY KEY(plan_id, slice_id),
            FOREIGN KEY(plan_id) REFERENCES implementation_plans(plan_id) ON DELETE CASCADE,
            CHECK((state='completed' OR state='failed') = (result_json IS NOT NULL))
        ) STRICT;
        CREATE INDEX IF NOT EXISTS implementation_plan_slices_state
            ON implementation_plan_slices(plan_id, state);
        CREATE TABLE IF NOT EXISTS agent_memory_pending (
            workspace_root TEXT PRIMARY KEY CHECK(length(workspace_root) BETWEEN 1 AND 32768),
            claims_json TEXT NOT NULL CHECK(json_valid(claims_json) AND length(claims_json)<=75497472)
        ) STRICT;",
    )?;
    Ok(())
}

fn append_events(
    tx: &rusqlite::Transaction<'_>,
    run_id: AgentRunId,
    events: &[AgentEventData],
    timestamp_ms: i64,
    run_revision: u64,
    operation_revisions: &[(&AgentOperationKey, u64)],
    default_operation: Option<(&AgentOperationKey, u64)>,
) -> crate::store::Result<Vec<AgentEvent>> {
    if timestamp_ms < 0 {
        return Err(StoreError::InvalidValue {
            field: "timestamp_ms",
            value: timestamp_ms.to_string(),
            reason: "timestamp must be nonnegative",
        });
    }
    let mut stored = Vec::with_capacity(events.len());
    for data in events {
        let explicit_operation = match data {
            AgentEventData::OperationCreated { key, .. }
            | AgentEventData::OperationStarted { key, .. }
            | AgentEventData::OperationRetryScheduled { key, .. }
            | AgentEventData::OperationCompleted { key, .. }
            | AgentEventData::ModelOutputRejected { key, .. }
            | AgentEventData::MemoryClaimRejected { key, .. }
            | AgentEventData::OperationOutcomeUnknown { key, .. } => Some(key),
            _ => None,
        };
        let operation = if let Some(key) = explicit_operation {
            if key.run_id != run_id {
                return Err(StoreError::InvalidValue {
                    field: "Agent event operation",
                    value: format!("{key:?}"),
                    reason: "event operation belongs to another AgentRun",
                });
            }
            Some(
                operation_revisions
                    .iter()
                    .find(|(candidate, _)| *candidate == key)
                    .map(|(_, revision)| (key, *revision))
                    .ok_or_else(|| StoreError::InvalidValue {
                        field: "Agent event operation",
                        value: format!("{key:?}"),
                        reason: "event operation has no matching committed revision",
                    })?,
            )
        } else {
            default_operation
        };
        tx.execute(
            "INSERT INTO agent_events (
                agent_run_id, operation_ordinal, run_revision, operation_revision,
                committed_at_ms, data_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run_id.as_bytes().as_slice(),
                operation.map(|(key, _)| key.ordinal.get()),
                run_revision,
                operation.map(|(_, revision)| revision),
                timestamp_ms,
                strict_json(data)?
            ],
        )?;
        stored.push(AgentEvent {
            id: AgentEventId(tx.last_insert_rowid().try_into().map_err(|_| {
                StoreError::InvalidValue {
                    field: "agent_event_id",
                    value: tx.last_insert_rowid().to_string(),
                    reason: "event ID must be positive",
                }
            })?),
            agent_run_id: run_id,
            operation: operation.map(|(key, _)| key.clone()),
            run_revision,
            operation_revision: operation.map(|(_, revision)| revision),
            committed_at_ms: timestamp_ms,
            data: data.clone(),
        });
    }
    Ok(stored)
}

fn stored_run_revision(
    tx: &rusqlite::Transaction<'_>,
    run_id: AgentRunId,
) -> crate::store::Result<u64> {
    tx.query_row(
        "SELECT revision FROM agent_runs WHERE agent_run_id=?1",
        [run_id.as_bytes().as_slice()],
        |row| row.get(0),
    )
    .map_err(StoreError::from)
}

fn operation_retry_at_ms(
    operation: &AgentOperation,
    output: &AgentOperationFsmOutput,
) -> crate::store::Result<Option<i64>> {
    let retries = output
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEventData::OperationRetryScheduled {
                key,
                delivery_attempt,
                retry_at_ms,
            } => Some((key, delivery_attempt, retry_at_ms)),
            _ => None,
        })
        .collect::<Vec<_>>();
    match operation.state {
        agl_core::agent::AgentOperationDeliveryState::RetryScheduled => {
            if retries.len() != 1 {
                return Err(StoreError::InvalidValue {
                    field: "Agent operation retry",
                    value: format!("{:?}", operation.key),
                    reason: "RetryScheduled requires exactly one retry event",
                });
            }
            let (key, delivery_attempt, retry_at_ms) = retries[0];
            if key != &operation.key
                || *delivery_attempt != operation.delivery_attempt
                || *retry_at_ms < 0
            {
                return Err(StoreError::InvalidValue {
                    field: "Agent operation retry",
                    value: format!("{:?}", operation.key),
                    reason: "retry event identity, attempt, or timestamp differs",
                });
            }
            Ok(Some(*retry_at_ms))
        }
        _ if retries.is_empty() => Ok(None),
        _ => Err(StoreError::InvalidValue {
            field: "Agent operation retry",
            value: format!("{:?}", operation.key),
            reason: "non-retry operation cannot carry a retry event",
        }),
    }
}

fn enum_text<T: serde::Serialize>(value: &T) -> crate::store::Result<String> {
    let serde_json::Value::String(value) = serde_json::to_value(value)? else {
        return Err(StoreError::InvalidValue {
            field: "enum",
            value: "non-string".into(),
            reason: "enum must use a string representation",
        });
    };
    Ok(value)
}

fn origin_conversation_bytes(origin: &agl_core::agent::AgentRunOrigin) -> Option<&[u8]> {
    let agl_core::agent::AgentRunOrigin::User {
        conversation_id, ..
    } = origin;
    Some(conversation_id.as_bytes().as_slice())
}

fn existing_agent_run(
    connection: &rusqlite::Connection,
    spec: &AgentRunSpec,
) -> crate::store::Result<Option<AgentRun>> {
    let origin = strict_json(&spec.origin)?;
    let input = strict_json(&spec.input)?;
    let agl_core::agent::AgentRunOrigin::User {
        conversation_id,
        message_id,
    } = &spec.origin;
    let stored = connection
        .query_row(
            "SELECT origin_json, input_json, agent_run_id, snapshot_json,
                    agent_package_digest, model_package_digest, instruction_digest,
                    status, checkpoint_json,
                    usage_model_input_tokens, usage_model_output_tokens,
                    usage_model_calls, usage_correction_input_tokens,
                    usage_correction_output_tokens, usage_correction_calls,
                    usage_tool_calls, revision
             FROM agent_runs
             WHERE origin_conversation_id=?1
               AND origin_message_id=?2",
            params![conversation_id.as_bytes().as_slice(), message_id.as_str(),],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    (
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Vec<u8>>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                        row.get::<_, Vec<u8>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        AgentRunUsage {
                            model_input_tokens: row.get(9)?,
                            model_output_tokens: row.get(10)?,
                            model_calls: row.get(11)?,
                            correction_input_tokens: row.get(12)?,
                            correction_output_tokens: row.get(13)?,
                            correction_calls: row.get(14)?,
                            tool_calls: row.get(15)?,
                        },
                        row.get::<_, u64>(16)?,
                    ),
                ))
            },
        )
        .optional()?;
    let Some((stored_origin, stored_input, run)) = stored else {
        return Ok(None);
    };
    if stored_origin != origin || stored_input != input {
        return Err(StoreError::InvalidValue {
            field: "agent run origin",
            value: origin,
            reason: "natural origin key is already admitted with a different spec",
        });
    }
    decode_run(&spec.origin, run).map(Some)
}

fn strict_json<T: serde::Serialize>(value: &T) -> crate::store::Result<String> {
    let value = serde_json::to_value(value)?;
    let value = canonical_json(value, 0)?;
    let encoded = serde_json::to_string(&value)?;
    if encoded.len() > MAX_STORED_JSON_BYTES {
        return Err(StoreError::InvalidValue {
            field: "stored JSON",
            value: encoded.len().to_string(),
            reason: "stored JSON exceeds 72 MiB",
        });
    }
    Ok(encoded)
}

fn canonical_json(
    value: serde_json::Value,
    depth: usize,
) -> crate::store::Result<serde_json::Value> {
    if depth > MAX_STORED_JSON_DEPTH {
        return Err(StoreError::InvalidValue {
            field: "stored JSON",
            value: depth.to_string(),
            reason: "stored JSON exceeds 64 nesting levels",
        });
    }
    Ok(match value {
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .into_iter()
                .map(|value| canonical_json(value, depth + 1))
                .collect::<crate::store::Result<_>>()?,
        ),
        serde_json::Value::Object(values) => {
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut canonical = serde_json::Map::new();
            for (key, value) in entries {
                canonical.insert(key, canonical_json(value, depth + 1)?);
            }
            serde_json::Value::Object(canonical)
        }
        value => value,
    })
}

fn decode_snapshot(json: String) -> crate::store::Result<AgentRunSnapshot> {
    if json.len() > MAX_STORED_JSON_BYTES {
        return Err(StoreError::InvalidValue {
            field: "AgentRun snapshot",
            value: json.len().to_string(),
            reason: "stored snapshot exceeds 72 MiB",
        });
    }
    Ok(serde_json::from_str(&json)?)
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

type StoredRun = (
    Vec<u8>,
    String,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    String,
    String,
    AgentRunUsage,
    u64,
);

fn decode_run(
    origin: &agl_core::agent::AgentRunOrigin,
    stored: StoredRun,
) -> crate::store::Result<AgentRun> {
    let (
        id,
        snapshot_json,
        agent_digest,
        model_digest,
        instruction_digest,
        status,
        checkpoint,
        usage,
        revision,
    ) = stored;
    let bytes: [u8; 16] = id.try_into().map_err(|_| StoreError::InvalidValue {
        field: "agent_run_id",
        value: "invalid blob length".into(),
        reason: "AgentRunId must contain 16 bytes",
    })?;
    let snapshot: AgentRunSnapshot = serde_json::from_str(&snapshot_json)?;
    if digest_blob(agent_digest, "agent package digest")? != *snapshot.agent.digest.as_bytes()
        || digest_blob(model_digest, "model package digest")?
            != *snapshot.model.model.digest.as_bytes()
        || digest_blob(instruction_digest, "instruction digest")?
            != *snapshot.instructions.digest.as_bytes()
    {
        return Err(StoreError::InvalidValue {
            field: "AgentRun snapshot digests",
            value: "mismatch".into(),
            reason: "indexed digest projection does not match snapshot JSON",
        });
    }
    Ok(AgentRun {
        id: AgentRunId::from_bytes(bytes).map_err(|_| StoreError::InvalidValue {
            field: "agent_run_id",
            value: "invalid UUID".into(),
            reason: "AgentRunId must be UUIDv7",
        })?,
        origin: origin.clone(),
        snapshot,
        status: serde_json::from_value(serde_json::Value::String(status))?,
        checkpoint: serde_json::from_str(&checkpoint)?,
        usage,
        revision,
    })
}

fn digest_blob(value: Vec<u8>, field: &'static str) -> crate::store::Result<[u8; 32]> {
    value
        .try_into()
        .map_err(|value: Vec<u8>| StoreError::InvalidValue {
            field,
            value: format!("{} bytes", value.len()),
            reason: "digest BLOB must contain exactly 32 bytes",
        })
}

#[cfg(test)]
mod tests;
