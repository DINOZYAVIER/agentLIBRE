//! Matrix bridge scaffolding.
//!
//! This crate intentionally owns only Matrix-facing bridge concerns. The daemon
//! boundary goes through `agl-client`; [`AgentClient`] is the bridge-level
//! contract used by Matrix event handling code.

pub mod access;
pub mod command;
pub mod config;
pub mod handler;
pub mod thread_binding;

use anyhow::Result;

pub use access::{AccessDecision, AccessPolicy};
pub use agl_client::{AgentLibreClient, ClientError};
pub use command::{BridgeCommand, CommandParseError};
pub use config::{AglConfig, BindingConfig, BridgeConfig, MatrixConfig};
pub use handler::{
    BridgeEventHandler, BridgeInboundEvent, BridgeOutboundAction, BridgeProcessedEvents,
    EncryptionState,
};
pub use thread_binding::{BindingKey, ThreadBinding, ThreadBindingStore};

/// Minimal daemon boundary expected by Matrix-facing bridge code.
pub trait AgentClient {
    fn daemon_status(&mut self) -> Result<String>;
    fn validate_session(&mut self, session_id: &str) -> Result<()>;
    fn open_session(&mut self) -> Result<String>;
    fn send_message(
        &mut self,
        session_id: &str,
        message: &str,
        idempotency_key: &str,
    ) -> Result<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingClient {
        messages: Vec<(String, String, String)>,
    }

    impl AgentClient for RecordingClient {
        fn daemon_status(&mut self) -> Result<String> {
            Ok("running".to_string())
        }

        fn validate_session(&mut self, _session_id: &str) -> Result<()> {
            Ok(())
        }

        fn open_session(&mut self) -> Result<String> {
            Ok("session-1".to_string())
        }

        fn send_message(
            &mut self,
            session_id: &str,
            message: &str,
            idempotency_key: &str,
        ) -> Result<String> {
            self.messages.push((
                session_id.to_string(),
                message.to_string(),
                idempotency_key.to_string(),
            ));
            Ok("assistant reply".to_string())
        }
    }

    #[test]
    fn client_trait_covers_daemon_boundary() {
        let mut client = RecordingClient {
            messages: Vec::new(),
        };
        let session_id = client.open_session().unwrap();
        let reply = client
            .send_message(&session_id, "hello", "$event")
            .expect("message should be accepted");
        assert_eq!(reply, "assistant reply");
        assert_eq!(
            client.messages,
            vec![(
                "session-1".to_string(),
                "hello".to_string(),
                "$event".to_string()
            )]
        );
    }

    #[test]
    fn bridge_manifest_uses_client_boundary_only() {
        let manifest = include_str!("../Cargo.toml");

        assert!(manifest.contains("agl-client.workspace = true"));
        for forbidden in ["agl-chat", "agl-loop", "agl-inference", "agl-cli"] {
            assert!(
                !has_dependency(manifest, forbidden),
                "agl-matrix-bridge must not depend on {forbidden}"
            );
        }
    }

    fn has_dependency(manifest: &str, crate_name: &str) -> bool {
        manifest.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with(&format!("{crate_name}."))
                || line.starts_with(&format!("{crate_name} ="))
        })
    }
}
