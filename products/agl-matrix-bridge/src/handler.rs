use agl_core::{ConversationId, MessageId};
use anyhow::{Context, Result};

use crate::{AccessDecision, AccessPolicy, AgentBoundary, BindingKey, MatrixConfig};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncryptionState {
    Plaintext,
    Decrypted,
    Undecryptable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeInboundEvent {
    pub event_id: String,
    pub room_id: String,
    pub sender_user_id: String,
    pub thread_root_event_id: String,
    pub body: String,
    pub encryption: EncryptionState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BridgeOutboundAction {
    Ignore { reason: &'static str },
    ReplyInThread { body: String },
}

pub struct BridgeEventHandler {
    matrix: MatrixConfig,
    access: AccessPolicy,
}

impl BridgeEventHandler {
    pub fn new(matrix: MatrixConfig, access: AccessPolicy) -> Self {
        Self { matrix, access }
    }

    pub fn handle<C: AgentBoundary>(
        &mut self,
        event: BridgeInboundEvent,
        conversation_id: ConversationId,
        message_id: MessageId,
        client: &mut C,
    ) -> Result<Vec<BridgeOutboundAction>> {
        if let Some(reason) = self.rejection_reason(&event) {
            return Ok(vec![BridgeOutboundAction::Ignore { reason }]);
        }

        let reply = client
            .send_message(conversation_id, message_id, event.body.as_str())
            .context("failed to send Matrix text as an AgentRun")?;
        Ok(vec![BridgeOutboundAction::ReplyInThread { body: reply }])
    }

    pub(crate) fn rejection_reason(&self, event: &BridgeInboundEvent) -> Option<&'static str> {
        if event.encryption == EncryptionState::Undecryptable {
            return Some("event is undecryptable");
        }
        if event.encryption == EncryptionState::Decrypted
            && self.matrix.encrypted_rooms == crate::config::EncryptedRoomPolicy::Reject
        {
            return Some("encrypted rooms are disabled");
        }
        match self.access.evaluate(&event.room_id, &event.sender_user_id) {
            AccessDecision::Allowed => {}
            AccessDecision::Denied { reason } => return Some(reason),
        }
        if event.body.trim().is_empty() {
            return Some("message body is empty");
        }
        None
    }
}

impl BridgeInboundEvent {
    pub fn binding_key(&self) -> BindingKey {
        BindingKey::new(self.room_id.clone(), self.thread_root_event_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EncryptedRoomPolicy;

    const CONVERSATION_ID: &str = "conv_01890f17-4a00-7000-8000-000000000001";

    #[derive(Default)]
    struct FakeClient {
        opened_conversations: usize,
        sent: Vec<(ConversationId, MessageId, String)>,
    }

    impl AgentBoundary for FakeClient {
        fn ensure_conversation(
            &mut self,
            _conversation_id: ConversationId,
            _function_path: &str,
            _workspace_path: &str,
        ) -> Result<()> {
            self.opened_conversations += 1;
            Ok(())
        }

        fn send_message(
            &mut self,
            conversation_id: ConversationId,
            message_id: MessageId,
            message: &str,
        ) -> Result<String> {
            self.sent
                .push((conversation_id, message_id, message.to_owned()));
            Ok("assistant response".to_owned())
        }
    }

    fn matrix() -> MatrixConfig {
        MatrixConfig {
            homeserver_url: "https://matrix.example".to_owned(),
            user_id: "@agl:example".to_owned(),
            access_token: Some("token".to_owned()),
            device_id: None,
            session_path: None,
            store_path: None,
            encrypted_rooms: EncryptedRoomPolicy::Reject,
        }
    }

    fn access() -> AccessPolicy {
        AccessPolicy {
            allowed_rooms: vec!["!room:example".to_owned()],
            allowed_users: vec!["@user:example".to_owned()],
        }
    }

    fn event(event_id: &str, thread_root: &str, body: &str) -> BridgeInboundEvent {
        BridgeInboundEvent {
            event_id: event_id.to_owned(),
            room_id: "!room:example".to_owned(),
            sender_user_id: "@user:example".to_owned(),
            thread_root_event_id: thread_root.to_owned(),
            body: body.to_owned(),
            encryption: EncryptionState::Plaintext,
        }
    }

    #[test]
    fn denied_event_does_not_call_agent() {
        let mut handler = BridgeEventHandler::new(matrix(), access());
        let mut client = FakeClient::default();
        let mut denied = event("$event", "$event", "hello");
        denied.sender_user_id = "@denied:example".to_owned();

        let actions = handler
            .handle(
                denied,
                ConversationId::parse(CONVERSATION_ID).unwrap(),
                MessageId::generate(),
                &mut client,
            )
            .unwrap();

        assert_eq!(
            actions,
            vec![BridgeOutboundAction::Ignore {
                reason: "user is not allowed"
            }]
        );
        assert!(client.sent.is_empty());
    }

    #[test]
    fn every_allowed_text_is_an_agent_prompt_without_command_mode() {
        let mut handler = BridgeEventHandler::new(matrix(), access());
        let mut client = FakeClient::default();

        let actions = handler
            .handle(
                event("$root", "$root", "!agl send is ordinary text"),
                ConversationId::parse(CONVERSATION_ID).unwrap(),
                MessageId::generate(),
                &mut client,
            )
            .unwrap();

        assert_eq!(client.opened_conversations, 0);
        assert_eq!(client.sent[0].2, "!agl send is ordinary text");
        assert!(matches!(
            actions.as_slice(),
            [BridgeOutboundAction::ReplyInThread { .. }]
        ));
    }

    #[test]
    fn different_roots_create_different_binding_keys() {
        assert_ne!(
            event("$one", "$one", "one").binding_key(),
            event("$two", "$two", "two").binding_key()
        );
    }

    #[test]
    fn decrypted_event_obeys_encrypted_room_policy() {
        let mut handler = BridgeEventHandler::new(matrix(), access());
        let mut client = FakeClient::default();
        let mut encrypted = event("$event", "$event", "hello");
        encrypted.encryption = EncryptionState::Decrypted;

        let actions = handler
            .handle(
                encrypted,
                ConversationId::parse(CONVERSATION_ID).unwrap(),
                MessageId::generate(),
                &mut client,
            )
            .unwrap();

        assert_eq!(
            actions,
            vec![BridgeOutboundAction::Ignore {
                reason: "encrypted rooms are disabled"
            }]
        );
    }
}
