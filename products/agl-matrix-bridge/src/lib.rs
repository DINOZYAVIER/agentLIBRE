//! Matrix thread-chat bridge.
//!
//! This crate intentionally owns only Matrix-facing bridge concerns. The daemon
//! boundary goes through `agl-daemon-api`; [`AgentClient`] is the bridge-level
//! interface used by Matrix event handling code.

pub mod access;
pub mod app;
pub mod client;
pub mod config;
pub mod handler;
#[cfg(unix)]
pub mod runtime;
pub mod state;
#[cfg(unix)]
mod sync_retry;
#[cfg(unix)]
pub use sync_retry::PermanentMatrixFault;

#[cfg(unix)]
pub fn is_permanent_matrix_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<PermanentMatrixFault>().is_some()
}

use agl_core::{ConversationId, MessageId};
use anyhow::Result;

pub use access::{AccessDecision, AccessPolicy};
pub use agl_daemon_api::{AgentClient, ClientError};
pub use app::BridgeApp;
#[cfg(unix)]
pub use client::LazyDaemonClient;
pub use config::{
    AglConfig, BindingConfig, BridgeConfig, BridgeConfigError, EncryptedRoomPolicy, MatrixConfig,
    VerificationConfig,
};
pub use handler::{BridgeEventHandler, BridgeInboundEvent, BridgeOutboundAction, EncryptionState};
#[cfg(unix)]
pub use runtime::{
    MatrixDeviceVerificationRequest, MatrixDeviceVerificationResult,
    MatrixDeviceVerificationStatus, MatrixLoginResult, MatrixPasswordLogin, MatrixRuntime,
    MatrixSasEmoji, MatrixSasPresentation, MatrixUserDevice,
};
pub use state::{
    BindingKey, BridgeReplayEntry, BridgeState, REPLAY_JOURNAL_LIMIT, ReplayReservation,
    ThreadBinding,
};

/// Minimal daemon boundary expected by Matrix-facing bridge code.
pub trait AgentBoundary {
    fn ensure_conversation(
        &mut self,
        conversation_id: ConversationId,
        function_path: &str,
        workspace_path: &str,
    ) -> Result<()>;
    fn send_message(
        &mut self,
        conversation_id: ConversationId,
        message_id: MessageId,
        message: &str,
    ) -> Result<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingClient {
        messages: Vec<(String, String, String)>,
    }

    impl AgentBoundary for RecordingClient {
        fn ensure_conversation(
            &mut self,
            _conversation_id: ConversationId,
            _function_path: &str,
            _workspace_path: &str,
        ) -> Result<()> {
            Ok(())
        }

        fn send_message(
            &mut self,
            conversation_id: ConversationId,
            _message_id: MessageId,
            message: &str,
        ) -> Result<String> {
            self.messages.push((
                conversation_id.to_string(),
                message.to_string(),
                "accepted".to_string(),
            ));
            Ok("assistant reply".to_string())
        }
    }

    #[test]
    fn client_trait_covers_daemon_boundary() {
        let mut client = RecordingClient {
            messages: Vec::new(),
        };
        let conversation_id =
            ConversationId::parse("conv_01890f17-4a00-7000-8000-000000000001").unwrap();
        client
            .ensure_conversation(conversation_id, "/functions/matrix", "/workspace")
            .unwrap();
        let reply = client
            .send_message(conversation_id, MessageId::generate(), "hello")
            .expect("message should be accepted");
        assert_eq!(reply, "assistant reply");
        assert_eq!(
            client.messages,
            vec![(
                "conv_01890f17-4a00-7000-8000-000000000001".to_string(),
                "hello".to_string(),
                "accepted".to_string()
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

        assert!(dependencies.contains("agl-daemon-api"));
        assert!(dependencies.contains("matrix-sdk"));
        for forbidden in ["agl-daemon", "agl-runtime", "agl-execd", "agl-cli"] {
            assert!(
                !dependencies.contains(forbidden),
                "agl-matrix-bridge must not depend on {forbidden}"
            );
        }
    }
}
