//! Matrix bridge scaffolding.
//!
//! This crate intentionally owns only Matrix-facing bridge concerns. The daemon
//! boundary goes through `agl-client`; [`AgentClient`] is the bridge-level
//! contract used by Matrix event handling code.

pub mod access;
pub mod command;
pub mod config;
pub mod thread_binding;

use anyhow::Result;

pub use access::{AccessDecision, AccessPolicy};
pub use agl_client::{AgentLibreClient, ClientError};
pub use command::{BridgeCommand, CommandParseError};
pub use config::{BridgeConfig, MatrixConfig};
pub use thread_binding::{BindingKey, ThreadBinding, ThreadBindingStore};

/// Minimal daemon boundary expected by Matrix-facing bridge code.
pub trait AgentClient {
    fn send_message(&self, session_id: &str, message: &str) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingClient;

    impl AgentClient for RecordingClient {
        fn send_message(&self, session_id: &str, message: &str) -> Result<()> {
            assert_eq!(session_id, "matrix:!room:example/$thread");
            assert_eq!(message, "hello");
            Ok(())
        }
    }

    #[test]
    fn client_trait_covers_daemon_boundary() {
        let client = RecordingClient;
        client
            .send_message("matrix:!room:example/$thread", "hello")
            .expect("message should be accepted");
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
