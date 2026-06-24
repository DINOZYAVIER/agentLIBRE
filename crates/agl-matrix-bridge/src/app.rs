use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::{
    AgentClient, BridgeConfig, BridgeEventHandler, BridgeInboundEvent, BridgeOutboundAction,
    BridgeState,
};

pub struct BridgeApp {
    config: BridgeConfig,
    state_path: Option<PathBuf>,
    state: BridgeState,
}

impl BridgeApp {
    pub fn from_config(config: BridgeConfig) -> Result<Self> {
        config
            .validate()
            .map_err(|err| anyhow::anyhow!("bridge config is invalid: {err:?}"))?;
        let state_path = config.bindings.path.clone().map(PathBuf::from);
        let state = if let Some(path) = &state_path {
            BridgeState::load(path)
                .with_context(|| format!("failed to load bridge state {}", path.display()))?
        } else {
            BridgeState::default()
        };
        Ok(Self {
            config,
            state_path,
            state,
        })
    }

    pub fn state(&self) -> &BridgeState {
        &self.state
    }

    pub fn handle_event<C: AgentClient>(
        &mut self,
        event: BridgeInboundEvent,
        client: &mut C,
    ) -> Result<Vec<BridgeOutboundAction>> {
        let (bindings, processed) = self.state.clone().into_parts();
        let mut handler = BridgeEventHandler::new(
            self.config.matrix.clone(),
            self.config.access.clone(),
            bindings,
            processed,
        );
        let actions = handler.handle(event, client)?;
        self.state = handler.state();
        if state_should_be_saved(&actions)
            && let Some(path) = &self.state_path
        {
            self.state
                .save(path)
                .with_context(|| format!("failed to save bridge state {}", path.display()))?;
        }
        Ok(actions)
    }
}

fn state_should_be_saved(actions: &[BridgeOutboundAction]) -> bool {
    actions.iter().any(|action| {
        matches!(
            action,
            BridgeOutboundAction::MarkProcessed { .. }
                | BridgeOutboundAction::PersistBinding { .. }
                | BridgeOutboundAction::RemoveBinding { .. }
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AccessPolicy, AglConfig, BindingConfig, BindingKey, EncryptedRoomPolicy, MatrixConfig,
    };

    #[derive(Default)]
    struct FakeClient {
        opened_sessions: usize,
        sent: Vec<(String, String, String)>,
    }

    impl AgentClient for FakeClient {
        fn daemon_status(&mut self) -> Result<String> {
            Ok("state=running".to_string())
        }

        fn validate_session(&mut self, _session_id: &str) -> Result<()> {
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

    fn config(path: Option<PathBuf>) -> BridgeConfig {
        BridgeConfig {
            matrix: MatrixConfig {
                homeserver_url: "https://matrix.example".to_string(),
                user_id: "@agl:example".to_string(),
                access_token: Some("token".to_string()),
                device_id: None,
                session_path: None,
                store_path: None,
                command_prefix: "!agl".to_string(),
                normal_chat: false,
                encrypted_rooms: EncryptedRoomPolicy::Reject,
            },
            agl: AglConfig::default(),
            access: AccessPolicy {
                allowed_rooms: vec!["!room:example".to_string()],
                allowed_users: vec!["@user:example".to_string()],
            },
            bindings: BindingConfig {
                path: path.map(|path| path.display().to_string()),
            },
        }
    }

    fn event(id: &str, body: &str) -> BridgeInboundEvent {
        BridgeInboundEvent {
            event_id: id.to_string(),
            room_id: "!room:example".to_string(),
            sender_user_id: "@user:example".to_string(),
            thread_root_event_id: Some("$thread".to_string()),
            body: body.to_string(),
            encryption: crate::EncryptionState::Plaintext,
        }
    }

    #[test]
    fn app_persists_binding_and_processed_event_state() {
        let path = std::env::temp_dir().join(format!(
            "agl-matrix-bridge-app-{}-state.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut app = BridgeApp::from_config(config(Some(path.clone()))).unwrap();
        let mut client = FakeClient::default();

        let actions = app
            .handle_event(event("$event", "!agl send hello"), &mut client)
            .unwrap();

        assert!(matches!(
            actions.as_slice(),
            [
                BridgeOutboundAction::PersistBinding { .. },
                BridgeOutboundAction::ReplyInThread { .. },
                BridgeOutboundAction::MarkProcessed { .. }
            ]
        ));
        let state = BridgeState::load(&path).unwrap();
        assert!(state.processed_event_ids.contains("$event"));
        assert_eq!(
            state.bindings,
            vec![crate::ThreadBinding {
                key: BindingKey::new("!room:example", Some("$thread".to_string())),
                session_id: "session-1".to_string(),
            }]
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn app_reuses_persisted_processed_state_to_skip_duplicate_events() {
        let path = std::env::temp_dir().join(format!(
            "agl-matrix-bridge-app-{}-duplicate.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut state = BridgeState::default();
        state.mark_processed("$event");
        state.save(&path).unwrap();
        let mut app = BridgeApp::from_config(config(Some(path.clone()))).unwrap();
        let mut client = FakeClient::default();

        let actions = app
            .handle_event(event("$event", "!agl send hello"), &mut client)
            .unwrap();

        assert_eq!(
            actions,
            vec![BridgeOutboundAction::Ignore {
                reason: "event already processed",
            }]
        );
        assert!(client.sent.is_empty());
        std::fs::remove_file(path).unwrap();
    }
}
