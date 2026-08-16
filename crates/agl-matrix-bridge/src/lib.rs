//! Matrix bridge scaffolding.
//!
//! This crate intentionally owns only Matrix-facing bridge concerns. The daemon
//! boundary goes through `agl-client`; [`AgentClient`] is the bridge-level
//! interface used by Matrix event handling code.

pub mod access;
pub mod app;
pub mod client;
pub mod command;
pub mod config;
pub mod handler;
pub mod outbox_delivery;
#[cfg(unix)]
pub mod runtime;
pub mod state;
pub mod thread_binding;

use agl_ids::SessionId;
use anyhow::Result;

pub use access::{AccessDecision, AccessPolicy};
pub use agl_client::{AgentLibreClient, ClientError};
pub use app::BridgeApp;
#[cfg(unix)]
pub use client::LazyDaemonClient;
pub use command::{BridgeCommand, CommandParseError};
pub use config::{
    AglConfig, BindingConfig, BridgeConfig, BridgeConfigError, EncryptedRoomPolicy, MatrixConfig,
    VerificationConfig,
};
pub use handler::{
    BridgeEventHandler, BridgeInboundEvent, BridgeOutboundAction, BridgeProcessedEvents,
    EncryptionState,
};
pub use outbox_delivery::{
    MATRIX_ROOM_NOTIFY_REF_PREFIX, MatrixOutboxDeliveryTools, MatrixOutboxTransport,
    parse_matrix_room_notify_ref,
};
#[cfg(unix)]
pub use runtime::{
    MatrixDeviceVerificationRequest, MatrixDeviceVerificationResult,
    MatrixDeviceVerificationStatus, MatrixLoginResult, MatrixOutboxDeliveryAction,
    MatrixOutboxDeliveryReport, MatrixOutboxDeliveryResult, MatrixPasswordLogin, MatrixRuntime,
    MatrixSasEmoji, MatrixSasPresentation, MatrixUserDevice,
};
pub use state::BridgeState;
pub use thread_binding::{BindingKey, ThreadBinding, ThreadBindingStore};

/// Minimal daemon boundary expected by Matrix-facing bridge code.
pub trait AgentClient {
    fn daemon_status(&mut self) -> Result<String>;
    fn validate_session(&mut self, session_id: &SessionId) -> Result<()>;
    fn open_session(&mut self) -> Result<SessionId>;
    fn send_message(
        &mut self,
        session_id: &SessionId,
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

        fn validate_session(&mut self, _session_id: &SessionId) -> Result<()> {
            Ok(())
        }

        fn open_session(&mut self) -> Result<SessionId> {
            Ok(SessionId::parse(
                "ses_01890f17-4a00-7000-8000-000000000001",
            )?)
        }

        fn send_message(
            &mut self,
            session_id: &SessionId,
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
                "ses_01890f17-4a00-7000-8000-000000000001".to_string(),
                "hello".to_string(),
                "$event".to_string()
            )]
        );
    }

    #[test]
    fn bridge_package_uses_client_boundary_only() {
        let output = std::process::Command::new(env!("CARGO"))
            .args(["metadata", "--format-version", "1", "--no-deps"])
            .current_dir(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .and_then(std::path::Path::parent)
                    .unwrap(),
            )
            .output()
            .unwrap();
        assert!(output.status.success());
        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let dependencies = metadata["packages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|package| package["name"] == "agl-matrix-bridge")
            .unwrap()["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|dependency| dependency["name"].as_str())
            .collect::<std::collections::BTreeSet<_>>();

        assert!(dependencies.contains("agl-client"));
        assert!(dependencies.contains("matrix-sdk"));
        for forbidden in ["agl-chat", "agl-loop", "agl-inference", "agl-cli"] {
            assert!(
                !dependencies.contains(forbidden),
                "agl-matrix-bridge must not depend on {forbidden}"
            );
        }
    }
}
