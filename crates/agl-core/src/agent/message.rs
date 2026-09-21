use crate::Content;
use crate::{AgentRunId, ConversationId, MessageId};
use serde::{Deserialize, Serialize};

use super::{AgentOperationKey, AgentOperationRequest};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageVisibility {
    Conversation,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMessage {
    pub id: MessageId,
    pub conversation_id: Option<ConversationId>,
    pub run_id: Option<AgentRunId>,
    pub source_operation: Option<AgentOperationKey>,
    pub role: MessageRole,
    pub visibility: MessageVisibility,
    pub content: Content,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMessagePage {
    pub messages: Vec<AgentMessage>,
    pub next_cursor: Option<MessageId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentContextEntry {
    pub message: AgentMessage,
    pub source_request: Option<AgentOperationRequest>,
    pub private_reasoning: Option<Content>,
}

impl AgentContextEntry {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.message.validate()?;
        if let Some(reasoning) = &self.private_reasoning {
            reasoning
                .validate()
                .map_err(|_| "invalid private reasoning content")?;
            if self.message.role == MessageRole::User {
                return Err("user context message cannot have private reasoning");
            }
        }
        match (&self.message.role, &self.source_request) {
            (MessageRole::User, None) => Ok(()),
            (MessageRole::User, Some(_)) => {
                Err("user context message cannot have a source request")
            }
            (MessageRole::Assistant, Some(AgentOperationRequest::ModelGeneration(_))) => Ok(()),
            (MessageRole::Assistant, Some(AgentOperationRequest::Compaction(_))) => {
                if self.message.visibility != MessageVisibility::Internal
                    || self.private_reasoning.is_some()
                {
                    return Err("compaction context must be internal without private reasoning");
                }
                Ok(())
            }
            (MessageRole::Tool, Some(AgentOperationRequest::Tool(_))) => Ok(()),
            (MessageRole::Assistant | MessageRole::Tool, None) => {
                Err("generated context message requires its source request")
            }
            (MessageRole::Assistant, Some(AgentOperationRequest::Tool(_))) => {
                Err("assistant context message requires a model source request")
            }
            (
                MessageRole::Tool,
                Some(
                    AgentOperationRequest::ModelGeneration(_)
                    | AgentOperationRequest::Compaction(_),
                ),
            ) => Err("tool context message requires a Tool source request"),
        }
    }
}

impl AgentMessage {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.visibility == MessageVisibility::Conversation && self.conversation_id.is_none() {
            return Err("conversation-visible message requires a conversation");
        }
        if let Some(operation) = &self.source_operation
            && self.run_id.as_ref() != Some(&operation.run_id)
        {
            return Err("source operation must belong to the message run");
        }
        match self.role {
            MessageRole::User if self.source_operation.is_some() => {
                Err("user message cannot have a source operation")
            }
            MessageRole::Assistant | MessageRole::Tool if self.source_operation.is_none() => {
                Err("generated message requires a source operation")
            }
            _ => self
                .content
                .validate()
                .map_err(|_| "invalid message content"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

    #[test]
    fn visibility_and_operation_correlation_are_fail_closed() {
        let run_id = AgentRunId::generate();
        let operation = AgentOperationKey {
            run_id,
            ordinal: NonZeroU32::MIN,
        };
        let generated = AgentMessage {
            id: MessageId::generate(),
            conversation_id: None,
            run_id: Some(run_id),
            source_operation: Some(operation),
            role: MessageRole::Assistant,
            visibility: MessageVisibility::Internal,
            content: Content::text("private result").unwrap(),
        };
        assert_eq!(generated.validate(), Ok(()));

        let mut invalid = generated.clone();
        invalid.visibility = MessageVisibility::Conversation;
        assert_eq!(
            invalid.validate(),
            Err("conversation-visible message requires a conversation")
        );
        invalid.visibility = MessageVisibility::Internal;
        invalid.run_id = Some(AgentRunId::generate());
        assert_eq!(
            invalid.validate(),
            Err("source operation must belong to the message run")
        );
    }

    #[test]
    fn context_role_must_match_the_persisted_operation_request() {
        let run_id = AgentRunId::generate();
        let operation = AgentOperationKey {
            run_id,
            ordinal: NonZeroU32::MIN,
        };
        let entry = AgentContextEntry {
            message: AgentMessage {
                id: MessageId::generate(),
                conversation_id: None,
                run_id: Some(run_id),
                source_operation: Some(operation),
                role: MessageRole::Tool,
                visibility: MessageVisibility::Internal,
                content: Content::text("result").unwrap(),
            },
            source_request: Some(AgentOperationRequest::ModelGeneration(
                super::super::ModelGenerationRequest {
                    context: vec![],
                    max_output_tokens: 1,
                },
            )),
            private_reasoning: None,
        };
        assert_eq!(
            entry.validate(),
            Err("tool context message requires a Tool source request")
        );
    }
}
