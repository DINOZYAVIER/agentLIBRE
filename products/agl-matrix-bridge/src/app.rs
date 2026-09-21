use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::{
    AgentBoundary, BridgeConfig, BridgeEventHandler, BridgeInboundEvent, BridgeOutboundAction,
    BridgeState, ReplayReservation,
};

pub struct BridgeApp {
    config: BridgeConfig,
    state_path: PathBuf,
    state: BridgeState,
}

impl BridgeApp {
    pub fn from_config(config: BridgeConfig) -> Result<Self> {
        config
            .validate()
            .map_err(|err| anyhow::anyhow!("bridge config is invalid: {err:?}"))?;
        let state_path = config
            .bindings
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .context("bindings.path is required for persistent Matrix thread identity")?;
        let state = BridgeState::load(&state_path)
            .with_context(|| format!("failed to load bridge state {}", state_path.display()))?;
        Ok(Self {
            config,
            state_path,
            state,
        })
    }

    pub fn state(&self) -> &BridgeState {
        &self.state
    }

    pub fn handle_event<C: AgentBoundary>(
        &mut self,
        event: BridgeInboundEvent,
        client: &mut C,
    ) -> Result<Vec<BridgeOutboundAction>> {
        let mut handler =
            BridgeEventHandler::new(self.config.matrix.clone(), self.config.access.clone());
        if let Some(reason) = handler.rejection_reason(&event) {
            return Ok(vec![BridgeOutboundAction::Ignore { reason }]);
        }

        let event_id = event.event_id.clone();
        let message_id = match self.state.reserve_event(&event_id)? {
            ReplayReservation::Reserved(message_id) => message_id,
            ReplayReservation::AlreadyProcessed => {
                return Ok(vec![BridgeOutboundAction::Ignore {
                    reason: "event already processed",
                }]);
            }
        };

        let key = event.binding_key();
        let conversation_id = if let Some(conversation_id) = self.state.conversation_for(&key) {
            conversation_id
        } else {
            let conversation_id = agl_core::ConversationId::generate();
            self.state.bind(key, conversation_id);
            self.save_state()?;
            conversation_id
        };

        // Publish both the event origin and thread identity before daemon
        // admission. A retry resolves or activates this exact Conversation.
        self.save_state()?;
        client.ensure_conversation(
            conversation_id,
            &self.config.agl.function_path,
            &self.config.agl.workspace_path,
        )?;

        let actions = handler.handle(event, conversation_id, message_id, client)?;
        self.state.mark_processed(&event_id)?;
        self.save_state()?;
        Ok(actions)
    }

    fn save_state(&self) -> Result<()> {
        self.state.save(&self.state_path).with_context(|| {
            format!("failed to save bridge state {}", self.state_path.display())
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AccessPolicy, AglConfig, BindingConfig, EncryptedRoomPolicy, MatrixConfig,
        VerificationConfig,
    };
    use agl_core::{ConversationId, MessageId};

    #[derive(Default)]
    struct FakeClient {
        opened: Vec<ConversationId>,
        sent: Vec<(ConversationId, MessageId, String)>,
        fail_open: bool,
        fail_send: bool,
    }

    impl AgentBoundary for FakeClient {
        fn ensure_conversation(
            &mut self,
            conversation_id: ConversationId,
            _function_path: &str,
            _workspace_path: &str,
        ) -> Result<()> {
            if self.fail_open {
                anyhow::bail!("simulated Conversation activation failure");
            }
            if !self.opened.contains(&conversation_id) {
                self.opened.push(conversation_id);
            }
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
            if self.fail_send {
                anyhow::bail!("simulated reply wait failure");
            }
            Ok("assistant response".to_owned())
        }
    }

    fn config(path: Option<PathBuf>) -> BridgeConfig {
        BridgeConfig {
            matrix: MatrixConfig {
                homeserver_url: "https://matrix.example".to_owned(),
                user_id: "@agl:example".to_owned(),
                access_token: Some("token".to_owned()),
                device_id: None,
                session_path: None,
                store_path: None,
                encrypted_rooms: EncryptedRoomPolicy::Reject,
            },
            agl: AglConfig {
                socket_path: None,
                function_path: "/functions/matrix".into(),
                workspace_path: "/workspace".into(),
            },
            verification: VerificationConfig::default(),
            access: AccessPolicy {
                allowed_rooms: vec!["!room:example".to_owned()],
                allowed_users: vec!["@user:example".to_owned()],
            },
            bindings: BindingConfig {
                path: path.map(|path| path.display().to_string()),
            },
        }
    }

    fn event(event_id: &str, thread_root: &str, body: &str) -> BridgeInboundEvent {
        BridgeInboundEvent {
            event_id: event_id.to_owned(),
            room_id: "!room:example".to_owned(),
            sender_user_id: "@user:example".to_owned(),
            thread_root_event_id: thread_root.to_owned(),
            body: body.to_owned(),
            encryption: crate::EncryptionState::Plaintext,
        }
    }

    fn temp_state(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agl-matrix-bridge-app-{}-{name}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn one_thread_reuses_one_conversation_and_persists_replay() {
        let path = temp_state("one-thread");
        let mut app = BridgeApp::from_config(config(Some(path.clone()))).unwrap();
        let mut client = FakeClient::default();

        app.handle_event(event("$root", "$root", "first"), &mut client)
            .unwrap();
        app.handle_event(event("$reply", "$root", "second"), &mut client)
            .unwrap();

        assert_eq!(client.opened.len(), 1);
        assert_eq!(client.sent.len(), 2);
        assert_eq!(client.sent[0].0, client.sent[1].0);
        let state = BridgeState::load(&path).unwrap();
        assert_eq!(state.bindings.len(), 1);
        assert_eq!(state.replay.len(), 2);
        assert!(state.replay.iter().all(|entry| entry.processed));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn different_root_messages_create_different_conversations() {
        let path = temp_state("different-roots");
        let mut app = BridgeApp::from_config(config(Some(path.clone()))).unwrap();
        let mut client = FakeClient::default();

        app.handle_event(event("$one", "$one", "one"), &mut client)
            .unwrap();
        app.handle_event(event("$two", "$two", "two"), &mut client)
            .unwrap();

        assert_eq!(client.opened.len(), 2);
        assert_ne!(client.sent[0].0, client.sent[1].0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn completed_event_is_ignored_without_second_agent_call() {
        let path = temp_state("completed-event");
        let mut app = BridgeApp::from_config(config(Some(path.clone()))).unwrap();
        let mut client = FakeClient::default();
        let input = event("$event", "$event", "hello");

        app.handle_event(input.clone(), &mut client).unwrap();
        let actions = app.handle_event(input, &mut client).unwrap();

        assert_eq!(
            actions,
            vec![BridgeOutboundAction::Ignore {
                reason: "event already processed"
            }]
        );
        assert_eq!(client.sent.len(), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn crash_window_persists_origin_and_thread_binding_before_agent_reply() {
        let path = temp_state("crash-window");
        let mut app = BridgeApp::from_config(config(Some(path.clone()))).unwrap();
        let mut client = FakeClient {
            fail_send: true,
            ..FakeClient::default()
        };

        assert!(
            app.handle_event(event("$event", "$event", "hello"), &mut client)
                .is_err()
        );
        let state = BridgeState::load(&path).unwrap();
        assert_eq!(state.bindings.len(), 1);
        assert_eq!(state.replay.len(), 1);
        assert!(!state.replay[0].processed);
        assert_eq!(state.replay[0].message_id, client.sent[0].1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn activation_failure_retries_the_persisted_conversation_identity() {
        let path = temp_state("activation-retry");
        let input = event("$event", "$event", "hello");
        let mut app = BridgeApp::from_config(config(Some(path.clone()))).unwrap();
        let mut failed = FakeClient {
            fail_open: true,
            ..FakeClient::default()
        };

        assert!(app.handle_event(input.clone(), &mut failed).is_err());
        let reserved = BridgeState::load(&path).unwrap();
        let conversation_id = reserved.bindings[0].conversation_id;
        assert!(!reserved.replay[0].processed);

        let mut retried = FakeClient::default();
        app.handle_event(input, &mut retried).unwrap();
        assert_eq!(retried.opened, vec![conversation_id]);
        assert_eq!(retried.sent[0].0, conversation_id);
        std::fs::remove_file(path).unwrap();
    }
}
