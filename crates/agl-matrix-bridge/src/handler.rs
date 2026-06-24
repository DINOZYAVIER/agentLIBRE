use std::collections::BTreeSet;

use anyhow::{Result, anyhow, bail};

use crate::{
    AccessDecision, AccessPolicy, AgentClient, BindingKey, BridgeCommand, BridgeState,
    MatrixConfig, ThreadBindingStore,
};

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
    pub thread_root_event_id: Option<String>,
    pub body: String,
    pub encryption: EncryptionState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BridgeOutboundAction {
    Ignore { reason: &'static str },
    ReplyInThread { body: String },
    NoticeInThread { body: String },
    MarkProcessed { event_id: String },
    PersistBinding { key: BindingKey, session_id: String },
    RemoveBinding { key: BindingKey },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BridgeProcessedEvents {
    event_ids: BTreeSet<String>,
}

impl BridgeProcessedEvents {
    pub fn mark(&mut self, event_id: impl Into<String>) -> bool {
        self.event_ids.insert(event_id.into())
    }

    pub fn contains(&self, event_id: &str) -> bool {
        self.event_ids.contains(event_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &String> {
        self.event_ids.iter()
    }
}

pub struct BridgeEventHandler {
    matrix: MatrixConfig,
    access: AccessPolicy,
    bindings: ThreadBindingStore,
    processed: BridgeProcessedEvents,
}

impl BridgeEventHandler {
    pub fn new(
        matrix: MatrixConfig,
        access: AccessPolicy,
        bindings: ThreadBindingStore,
        processed: BridgeProcessedEvents,
    ) -> Self {
        Self {
            matrix,
            access,
            bindings,
            processed,
        }
    }

    pub fn bindings(&self) -> &ThreadBindingStore {
        &self.bindings
    }

    pub fn processed(&self) -> &BridgeProcessedEvents {
        &self.processed
    }

    pub fn state(&self) -> BridgeState {
        BridgeState::from_parts(&self.bindings, &self.processed)
    }

    pub fn handle<C: AgentClient>(
        &mut self,
        event: BridgeInboundEvent,
        client: &mut C,
    ) -> Result<Vec<BridgeOutboundAction>> {
        if self.processed.contains(&event.event_id) {
            return Ok(vec![BridgeOutboundAction::Ignore {
                reason: "event already processed",
            }]);
        }

        if event.encryption == EncryptionState::Undecryptable {
            return Ok(vec![BridgeOutboundAction::Ignore {
                reason: "event is undecryptable",
            }]);
        }

        if event.encryption == EncryptionState::Decrypted
            && self.matrix.encrypted_rooms == crate::config::EncryptedRoomPolicy::Reject
        {
            return Ok(vec![BridgeOutboundAction::Ignore {
                reason: "encrypted rooms are disabled",
            }]);
        }

        match self.access.evaluate(&event.room_id, &event.sender_user_id) {
            AccessDecision::Allowed => {}
            AccessDecision::Denied { reason } => {
                return Ok(vec![BridgeOutboundAction::Ignore { reason }]);
            }
        }

        let command = BridgeCommand::parse(&event.body, self.matrix.command_prefix())
            .map_err(|err| anyhow!("{err:?}"))?;
        let key = event.binding_key();
        let mut actions = match command {
            Some(BridgeCommand::Help) => vec![BridgeOutboundAction::NoticeInThread {
                body: "!agl help | status | bind <session-id> | unbind | send <message>"
                    .to_string(),
            }],
            Some(BridgeCommand::Status) => vec![BridgeOutboundAction::NoticeInThread {
                body: client.daemon_status()?,
            }],
            Some(BridgeCommand::Bind { session_id }) => {
                client.validate_session(&session_id)?;
                self.bindings.bind(key.clone(), session_id.clone());
                vec![
                    BridgeOutboundAction::PersistBinding { key, session_id },
                    BridgeOutboundAction::NoticeInThread {
                        body: "binding=ok".to_string(),
                    },
                ]
            }
            Some(BridgeCommand::Unbind) => {
                self.bindings.unbind(&key);
                vec![
                    BridgeOutboundAction::RemoveBinding { key },
                    BridgeOutboundAction::NoticeInThread {
                        body: "binding=removed".to_string(),
                    },
                ]
            }
            Some(BridgeCommand::Message { body }) => {
                self.send_turn(event.event_id.as_str(), key, body.as_str(), client)?
            }
            None if self.matrix.normal_chat => {
                self.send_turn(event.event_id.as_str(), key, event.body.as_str(), client)?
            }
            None => vec![BridgeOutboundAction::Ignore {
                reason: "normal chat is disabled",
            }],
        };

        if should_mark_processed(&actions) {
            self.processed.mark(event.event_id.clone());
            actions.push(BridgeOutboundAction::MarkProcessed {
                event_id: event.event_id,
            });
        }
        Ok(actions)
    }

    fn send_turn<C: AgentClient>(
        &mut self,
        event_id: &str,
        key: BindingKey,
        body: &str,
        client: &mut C,
    ) -> Result<Vec<BridgeOutboundAction>> {
        if body.trim().is_empty() {
            bail!("matrix message body is empty");
        }

        let (session_id, mut actions) = if let Some(session_id) = self.bindings.session_for(&key) {
            (session_id.to_string(), Vec::new())
        } else {
            let session_id = client.open_session()?;
            self.bindings.bind(key.clone(), session_id.clone());
            (
                session_id.clone(),
                vec![BridgeOutboundAction::PersistBinding {
                    key: key.clone(),
                    session_id,
                }],
            )
        };
        let reply = client.send_message(&session_id, body, event_id)?;
        actions.push(BridgeOutboundAction::ReplyInThread { body: reply });
        Ok(actions)
    }
}

impl BridgeInboundEvent {
    pub fn binding_key(&self) -> BindingKey {
        BindingKey::new(
            self.room_id.clone(),
            Some(
                self.thread_root_event_id
                    .clone()
                    .unwrap_or_else(|| self.event_id.clone()),
            ),
        )
    }
}

fn should_mark_processed(actions: &[BridgeOutboundAction]) -> bool {
    !matches!(
        actions,
        [BridgeOutboundAction::Ignore {
            reason: "event already processed"
        }]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EncryptedRoomPolicy, MatrixConfig};

    #[derive(Default)]
    struct FakeClient {
        opened_sessions: usize,
        sent: Vec<(String, String, String)>,
        validated: Vec<String>,
    }

    impl AgentClient for FakeClient {
        fn daemon_status(&mut self) -> Result<String> {
            Ok("state=running".to_string())
        }

        fn validate_session(&mut self, session_id: &str) -> Result<()> {
            self.validated.push(session_id.to_string());
            Ok(())
        }

        fn open_session(&mut self) -> Result<String> {
            self.opened_sessions += 1;
            Ok(format!("session-{}", self.opened_sessions))
        }

        fn send_message(
            &mut self,
            session_id: &str,
            message: &str,
            idempotency_key: &str,
        ) -> Result<String> {
            self.sent.push((
                session_id.to_string(),
                message.to_string(),
                idempotency_key.to_string(),
            ));
            Ok("assistant response".to_string())
        }
    }

    fn matrix(normal_chat: bool) -> MatrixConfig {
        MatrixConfig {
            homeserver_url: "https://matrix.example".to_string(),
            user_id: "@agl:example".to_string(),
            access_token: "token".to_string(),
            device_id: None,
            command_prefix: "!agl".to_string(),
            normal_chat,
            encrypted_rooms: EncryptedRoomPolicy::Reject,
        }
    }

    fn access() -> AccessPolicy {
        AccessPolicy {
            allowed_rooms: vec!["!room:example".to_string()],
            allowed_users: vec!["@user:example".to_string()],
        }
    }

    fn event(body: &str) -> BridgeInboundEvent {
        BridgeInboundEvent {
            event_id: "$event".to_string(),
            room_id: "!room:example".to_string(),
            sender_user_id: "@user:example".to_string(),
            thread_root_event_id: Some("$thread".to_string()),
            body: body.to_string(),
            encryption: EncryptionState::Plaintext,
        }
    }

    #[test]
    fn denied_event_does_not_call_client_or_mark_processed() {
        let mut handler = BridgeEventHandler::new(
            matrix(true),
            AccessPolicy::default(),
            ThreadBindingStore::default(),
            BridgeProcessedEvents::default(),
        );
        let mut client = FakeClient::default();

        let actions = handler.handle(event("hello"), &mut client).unwrap();

        assert_eq!(
            actions,
            vec![BridgeOutboundAction::Ignore {
                reason: "no access policy configured"
            }]
        );
        assert!(client.sent.is_empty());
        assert!(!handler.processed().contains("$event"));
    }

    #[test]
    fn send_command_opens_session_and_uses_event_id_as_idempotency_key() {
        let mut handler = BridgeEventHandler::new(
            matrix(false),
            access(),
            ThreadBindingStore::default(),
            BridgeProcessedEvents::default(),
        );
        let mut client = FakeClient::default();

        let actions = handler
            .handle(event("!agl send hello"), &mut client)
            .unwrap();

        assert_eq!(
            client.sent,
            vec![(
                "session-1".to_string(),
                "hello".to_string(),
                "$event".to_string()
            )]
        );
        assert!(actions.contains(&BridgeOutboundAction::ReplyInThread {
            body: "assistant response".to_string()
        }));
        assert!(actions.contains(&BridgeOutboundAction::MarkProcessed {
            event_id: "$event".to_string()
        }));
        assert_eq!(
            handler.bindings().session_for(&BindingKey::new(
                "!room:example",
                Some("$thread".to_string())
            )),
            Some("session-1")
        );
    }

    #[test]
    fn duplicate_event_is_ignored_before_client_dispatch() {
        let mut processed = BridgeProcessedEvents::default();
        processed.mark("$event");
        let mut handler = BridgeEventHandler::new(
            matrix(true),
            access(),
            ThreadBindingStore::default(),
            processed,
        );
        let mut client = FakeClient::default();

        let actions = handler.handle(event("hello"), &mut client).unwrap();

        assert_eq!(
            actions,
            vec![BridgeOutboundAction::Ignore {
                reason: "event already processed"
            }]
        );
        assert!(client.sent.is_empty());
    }

    #[test]
    fn normal_chat_must_be_enabled_explicitly() {
        let mut handler = BridgeEventHandler::new(
            matrix(false),
            access(),
            ThreadBindingStore::default(),
            BridgeProcessedEvents::default(),
        );
        let mut client = FakeClient::default();

        let actions = handler.handle(event("hello"), &mut client).unwrap();

        assert_eq!(
            actions,
            vec![
                BridgeOutboundAction::Ignore {
                    reason: "normal chat is disabled"
                },
                BridgeOutboundAction::MarkProcessed {
                    event_id: "$event".to_string()
                },
            ]
        );
        assert!(client.sent.is_empty());
    }

    #[test]
    fn bind_validates_session_before_persisting_binding() {
        let mut handler = BridgeEventHandler::new(
            matrix(false),
            access(),
            ThreadBindingStore::default(),
            BridgeProcessedEvents::default(),
        );
        let mut client = FakeClient::default();

        let actions = handler
            .handle(event("!agl bind session-existing"), &mut client)
            .unwrap();

        assert_eq!(client.validated, vec!["session-existing".to_string()]);
        assert!(actions.contains(&BridgeOutboundAction::PersistBinding {
            key: BindingKey::new("!room:example", Some("$thread".to_string())),
            session_id: "session-existing".to_string(),
        }));
    }

    #[test]
    fn undecryptable_event_is_rejected_before_command_parsing() {
        let mut handler = BridgeEventHandler::new(
            matrix(true),
            access(),
            ThreadBindingStore::default(),
            Default::default(),
        );
        let mut client = FakeClient::default();
        let mut inbound = event("!agl send hello");
        inbound.encryption = EncryptionState::Undecryptable;

        let actions = handler.handle(inbound, &mut client).unwrap();

        assert_eq!(
            actions,
            vec![BridgeOutboundAction::Ignore {
                reason: "event is undecryptable"
            }]
        );
        assert!(client.sent.is_empty());
    }
}
