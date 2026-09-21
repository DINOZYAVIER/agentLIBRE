use std::num::NonZeroU32;

use crate::model::{ModelConfig, ModelDialect, ToolCallFormat};
use agl_core::Content;
use agl_core::agent::{
    AdmittedTool, AgentContextEntry, AgentOperationKey, AgentOperationRequest,
    INVALID_MODEL_OUTPUT_CORRECTION, MessageRole, MessageVisibility,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::inference::InferenceGenerateRequest;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedModelRequest {
    pub operation: AgentOperationKey,
    pub delivery_attempt: NonZeroU32,
    pub dialect: ModelDialect,
    pub tool_call_format: ToolCallFormat,
    pub max_output_tokens: u64,
    pub messages: Vec<RenderedMessage>,
    pub tools: Vec<RenderedTool>,
    pub response_format: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedMessage {
    pub role: RenderedMessageRole,
    pub content: Option<Content>,
    pub private_reasoning: Option<Content>,
    pub name: Option<String>,
    pub tool_call: Option<RenderedToolCall>,
    #[serde(default)]
    pub tool_calls: Vec<RenderedToolCall>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderedMessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

pub fn render_model_request(
    request: &InferenceGenerateRequest,
    config: &ModelConfig,
) -> Result<RenderedModelRequest> {
    config.validate()?;
    ensure!(
        request.generation.context.len() == request.context.len()
            && request
                .generation
                .context
                .iter()
                .zip(&request.context)
                .all(|(expected, entry)| expected == &entry.message.id),
        "materialized context does not match ModelGenerationRequest"
    );
    let mut messages = Vec::new();
    for block in &request.instructions.blocks {
        push_message(
            &mut messages,
            RenderedMessage {
                role: RenderedMessageRole::System,
                content: Some(block.content.clone()),
                private_reasoning: None,
                name: None,
                tool_call: None,
                tool_calls: Vec::new(),
            },
        );
    }
    let mut index = 0;
    while index < request.context.len() {
        if request.context[index].message.role == MessageRole::Tool {
            let start = index;
            while index < request.context.len()
                && request.context[index].message.role == MessageRole::Tool
            {
                index += 1;
            }
            render_tool_context_group(&request.context[start..index], &mut messages)?;
        } else {
            render_context(&request.context[index], &mut messages)?;
            index += 1;
        }
    }
    Ok(RenderedModelRequest {
        operation: request.operation.clone(),
        delivery_attempt: request.delivery_attempt,
        dialect: config.dialect,
        tool_call_format: config.tool_call_format,
        max_output_tokens: request.generation.max_output_tokens,
        messages,
        tools: request.tools.iter().map(render_tool).collect(),
        response_format: request.response_format.clone(),
    })
}

fn render_tool_context_group(
    entries: &[AgentContextEntry],
    output: &mut Vec<RenderedMessage>,
) -> Result<()> {
    let mut calls = Vec::with_capacity(entries.len());
    for entry in entries {
        entry.validate().map_err(anyhow::Error::msg)?;
        let Some(AgentOperationRequest::Tool(request)) = &entry.source_request else {
            anyhow::bail!("Tool message source is not a Tool request");
        };
        calls.push(RenderedToolCall {
            name: request.tool_id.to_string(),
            arguments: request.input.clone(),
        });
    }
    output.push(RenderedMessage {
        role: RenderedMessageRole::Assistant,
        content: None,
        private_reasoning: entries[0].private_reasoning.clone(),
        name: None,
        tool_call: None,
        tool_calls: calls,
    });
    for entry in entries {
        let Some(AgentOperationRequest::Tool(request)) = &entry.source_request else {
            unreachable!();
        };
        output.push(RenderedMessage {
            role: RenderedMessageRole::Tool,
            content: Some(entry.message.content.clone()),
            private_reasoning: None,
            name: Some(request.tool_id.to_string()),
            tool_call: None,
            tool_calls: Vec::new(),
        });
    }
    Ok(())
}

fn render_context(entry: &AgentContextEntry, output: &mut Vec<RenderedMessage>) -> Result<()> {
    entry.validate().map_err(anyhow::Error::msg)?;
    if entry.message.role == MessageRole::Tool {
        let Some(AgentOperationRequest::Tool(request)) = &entry.source_request else {
            anyhow::bail!("Tool message source is not a Tool request");
        };
        output.push(RenderedMessage {
            role: RenderedMessageRole::Assistant,
            content: None,
            private_reasoning: entry.private_reasoning.clone(),
            name: None,
            tool_call: Some(RenderedToolCall {
                name: request.tool_id.to_string(),
                arguments: request.input.clone(),
            }),
            tool_calls: Vec::new(),
        });
        output.push(RenderedMessage {
            role: RenderedMessageRole::Tool,
            content: Some(entry.message.content.clone()),
            private_reasoning: None,
            name: Some(request.tool_id.to_string()),
            tool_call: None,
            tool_calls: Vec::new(),
        });
        return Ok(());
    }
    push_message(
        output,
        RenderedMessage {
            role: if entry.message.role == MessageRole::Assistant
                && entry.message.visibility == MessageVisibility::Internal
                && entry.message.content.as_text() == INVALID_MODEL_OUTPUT_CORRECTION
            {
                // This durable assistant message records a failed generation. Render it as a
                // user correction so the next generation receives an instruction, not an
                // assistant prefix. Consecutive failures are coalesced by push_message.
                RenderedMessageRole::User
            } else {
                match entry.message.role {
                    MessageRole::User => RenderedMessageRole::User,
                    MessageRole::Assistant => RenderedMessageRole::Assistant,
                    MessageRole::Tool => unreachable!(),
                }
            },
            content: Some(entry.message.content.clone()),
            private_reasoning: entry.private_reasoning.clone(),
            name: None,
            tool_call: None,
            tool_calls: Vec::new(),
        },
    );
    Ok(())
}

fn push_message(output: &mut Vec<RenderedMessage>, next: RenderedMessage) {
    if let Some(previous) = output.last()
        && previous.role == next.role
        && previous.name.is_none()
        && previous.tool_call.is_none()
        && previous.tool_calls.is_empty()
        && previous.private_reasoning.is_none()
        && next.name.is_none()
        && next.tool_call.is_none()
        && next.tool_calls.is_empty()
        && next.private_reasoning.is_none()
        && let (Some(left), Some(right)) = (&previous.content, &next.content)
        && left == right
    {
        return;
    }
    output.push(next);
}

fn render_tool(tool: &AdmittedTool) -> RenderedTool {
    RenderedTool {
        name: tool.definition.id.to_string(),
        description: tool.definition.description.clone(),
        input_schema: tool.definition.input_schema.as_value().clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use crate::package::{PackageId, PackageVersion};
    use agl_core::ToolId;
    use agl_core::agent::{
        AgentMessage, AgentOperationKey, InstructionBlock, InstructionSet, InstructionSource,
        MessageVisibility, ModelDefinitionRef, ModelGenerationRequest, PackageDigest, ToolRequest,
    };
    use agl_core::{AgentRunId, MessageId};

    use super::*;
    use crate::inference::{InferenceCancellation, InferenceGenerateRequest};

    fn request() -> InferenceGenerateRequest {
        let run_id = AgentRunId::generate();
        let user_id = MessageId::generate();
        let tool_id = MessageId::generate();
        let operation = AgentOperationKey {
            run_id,
            ordinal: NonZeroU32::new(2).unwrap(),
        };
        InferenceGenerateRequest {
            operation: AgentOperationKey {
                run_id,
                ordinal: NonZeroU32::new(3).unwrap(),
            },
            delivery_attempt: NonZeroU32::MIN,
            model: ModelDefinitionRef {
                id: PackageId::new("test-model").unwrap(),
                version: PackageVersion::new("1.0.0").unwrap(),
                digest: PackageDigest::from_bytes([1; 32]),
            },
            runtime: crate::inference::service::test_runtime_selection(),
            generation: ModelGenerationRequest {
                context: vec![user_id.clone(), tool_id.clone()],
                max_output_tokens: 32,
            },
            instructions: InstructionSet::new(vec![InstructionBlock {
                source: InstructionSource::Agent,
                content: Content::text("system").unwrap(),
            }])
            .unwrap(),
            context: vec![
                AgentContextEntry {
                    message: AgentMessage {
                        id: user_id,
                        conversation_id: None,
                        run_id: Some(run_id),
                        source_operation: None,
                        role: MessageRole::User,
                        visibility: MessageVisibility::Internal,
                        content: Content::text("question").unwrap(),
                    },
                    source_request: None,
                    private_reasoning: None,
                },
                AgentContextEntry {
                    message: AgentMessage {
                        id: tool_id,
                        conversation_id: None,
                        run_id: Some(run_id),
                        source_operation: Some(operation.clone()),
                        role: MessageRole::Tool,
                        visibility: MessageVisibility::Internal,
                        content: Content::text("result").unwrap(),
                    },
                    source_request: Some(AgentOperationRequest::Tool(ToolRequest {
                        tool_id: ToolId::new("test.extension:read").unwrap(),
                        input: serde_json::json!({"path":"README.md"}),
                    })),
                    private_reasoning: None,
                },
            ],
            tools: vec![],
            response_format: None,
            deadline_at_ms: i64::MAX,
            cancellation: InferenceCancellation::new(),
            progress: None,
            health: None,
        }
    }

    #[test]
    fn tool_context_reconstructs_the_native_assistant_and_tool_pair_once() {
        let mut request = request();
        request.context[1].private_reasoning = Some(Content::text("private plan").unwrap());
        let rendered = render_model_request(
            &request,
            &ModelConfig {
                dialect: ModelDialect::Gemma4,
                tool_call_format: ToolCallFormat::GemmaAgentCall,
            },
        )
        .unwrap();

        assert_eq!(
            rendered
                .messages
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            [
                RenderedMessageRole::System,
                RenderedMessageRole::User,
                RenderedMessageRole::Assistant,
                RenderedMessageRole::Tool,
            ]
        );
        assert_eq!(rendered.messages[2].tool_calls.len(), 1);
        let call = &rendered.messages[2].tool_calls[0];
        assert_eq!(call.name, "test.extension:read");
        assert_eq!(call.arguments, serde_json::json!({"path":"README.md"}));
        assert_eq!(
            rendered.messages[2]
                .private_reasoning
                .as_ref()
                .unwrap()
                .as_text(),
            "private plan"
        );
        assert_eq!(
            rendered.messages[3].content.as_ref().unwrap().as_text(),
            "result"
        );
    }

    #[test]
    fn materialized_context_identity_is_exact() {
        let mut request = request();
        request.generation.context[0] = MessageId::generate();
        assert!(render_model_request(&request, &ModelConfig::default()).is_err());
    }

    #[test]
    fn consecutive_invalid_output_corrections_render_as_one_user_turn() {
        let mut request = request();
        for ordinal in [4, 5] {
            let id = MessageId::generate();
            let source_operation = AgentOperationKey {
                run_id: request.operation.run_id,
                ordinal: NonZeroU32::new(ordinal).unwrap(),
            };
            request.generation.context.push(id.clone());
            request.context.push(AgentContextEntry {
                message: AgentMessage {
                    id,
                    conversation_id: None,
                    run_id: Some(request.operation.run_id),
                    source_operation: Some(source_operation),
                    role: MessageRole::Assistant,
                    visibility: MessageVisibility::Internal,
                    content: Content::text(INVALID_MODEL_OUTPUT_CORRECTION).unwrap(),
                },
                source_request: Some(AgentOperationRequest::ModelGeneration(
                    ModelGenerationRequest {
                        context: vec![],
                        max_output_tokens: 32,
                    },
                )),
                private_reasoning: None,
            });
        }

        let rendered = render_model_request(&request, &ModelConfig::default()).unwrap();
        assert_eq!(
            rendered
                .messages
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            [
                RenderedMessageRole::System,
                RenderedMessageRole::User,
                RenderedMessageRole::Assistant,
                RenderedMessageRole::Tool,
                RenderedMessageRole::User,
            ]
        );
        assert_eq!(
            rendered
                .messages
                .last()
                .unwrap()
                .content
                .as_ref()
                .unwrap()
                .as_text(),
            INVALID_MODEL_OUTPUT_CORRECTION
        );
    }

    #[test]
    fn structured_response_format_is_carried_to_the_native_request() {
        let mut request = request();
        request.response_format = Some(serde_json::json!({
            "type": "json_schema",
            "json_schema": {"name": "semantic_summary", "schema": {"type": "object"}}
        }));
        let rendered = render_model_request(
            &request,
            &ModelConfig {
                dialect: ModelDialect::Gemma4,
                tool_call_format: ToolCallFormat::GemmaAgentCall,
            },
        )
        .unwrap();
        assert_eq!(rendered.response_format, request.response_format);
    }
}
