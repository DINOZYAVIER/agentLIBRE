//! Agent-side adapter for the independently owned terminal service.
//!
//! This crate owns no process runtime, PTY, registry, spool, or terminal
//! persistence. It validates one exact service identity document and creates
//! cancellation-aware clients carrying the dispatch-time authority result.

use std::path::{Path, PathBuf};

use agl_exec::AuthorityFingerprint;
pub use agl_exec::{ExecutionContextSnapshot, ShellProfileSnapshot};
use agl_terminal_client::{ClientError, TerminalClient, UnixTerminalTransport};
use agl_terminal_protocol::{ServiceIdentity, TerminalGenerationIdentity};
use thiserror::Error;

#[doc(hidden)]
pub mod test_support;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalEndpoint {
    socket_path: PathBuf,
    runtime_projection: PathBuf,
    expected_generation: TerminalGenerationIdentity,
}

impl TerminalEndpoint {
    pub fn new(
        socket_path: PathBuf,
        runtime_projection: PathBuf,
        expected_generation: TerminalGenerationIdentity,
    ) -> Result<Self, TerminalEndpointError> {
        if !socket_path.is_absolute() || !runtime_projection.is_absolute() {
            return Err(TerminalEndpointError::RelativePath);
        }
        expected_generation
            .validate()
            .map_err(|error| TerminalEndpointError::InvalidGeneration(error.to_string()))?;
        Ok(Self {
            socket_path,
            runtime_projection,
            expected_generation,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn runtime_projection(&self) -> &Path {
        &self.runtime_projection
    }

    pub fn source_revision(&self) -> &str {
        self.expected_generation.source_revision()
    }

    pub fn expected_generation(&self) -> &TerminalGenerationIdentity {
        &self.expected_generation
    }

    pub fn connect(
        &self,
        authority: AuthorityFingerprint,
    ) -> Result<TerminalClient<UnixTerminalTransport>, TerminalEndpointError> {
        let transport =
            UnixTerminalTransport::new(self.socket_path.clone()).map_err(ClientError::Transport)?;
        TerminalClient::for_generation_with_runtime_projection(
            transport,
            self.expected_generation.clone(),
            Some(authority),
            self.runtime_projection.clone(),
        )
        .map_err(Into::into)
    }

    pub fn connect_readiness(
        &self,
    ) -> Result<TerminalClient<UnixTerminalTransport>, TerminalEndpointError> {
        let transport =
            UnixTerminalTransport::new(self.socket_path.clone()).map_err(ClientError::Transport)?;
        TerminalClient::for_generation_with_runtime_projection(
            transport,
            self.expected_generation.clone(),
            None,
            self.runtime_projection.clone(),
        )
        .map_err(Into::into)
    }

    pub async fn bootstrap(
        &self,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<ServiceIdentity, TerminalEndpointError> {
        let identity = self
            .connect_readiness()?
            .bootstrap_identity(cancellation)
            .await
            .map_err(TerminalEndpointError::from)?;
        let _process_generation_id = identity.process_generation_id();
        Ok(identity)
    }
}

#[derive(Debug, Error)]
pub enum TerminalEndpointError {
    #[error("terminal endpoint paths must be absolute")]
    RelativePath,
    #[error("terminal installed generation identity is invalid: {0}")]
    InvalidGeneration(String),
    #[error(transparent)]
    Client(#[from] ClientError),
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use agl_terminal_protocol::TERMINAL_PROTOCOL_VERSION;

    use super::*;

    fn build(byte: char) -> AuthorityFingerprint {
        AuthorityFingerprint::new(format!("sha256:{}", byte.to_string().repeat(64))).unwrap()
    }

    fn temporary_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "agl-process-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn generation() -> TerminalGenerationIdentity {
        TerminalGenerationIdentity::new(
            build('a'),
            "b".repeat(40),
            build('c'),
            TERMINAL_PROTOCOL_VERSION,
        )
        .unwrap()
    }

    #[test]
    fn endpoint_requires_absolute_paths_and_exact_generation() {
        assert!(matches!(
            TerminalEndpoint::new(
                PathBuf::from("relative"),
                PathBuf::from("relative"),
                generation(),
            ),
            Err(TerminalEndpointError::RelativePath)
        ));
        let root = temporary_root("endpoint");
        let endpoint = TerminalEndpoint::new(
            root.join("terminal.sock"),
            root.join("service-identity.json"),
            generation(),
        )
        .unwrap();
        assert_eq!(endpoint.source_revision(), "b".repeat(40));
        assert_eq!(endpoint.expected_generation(), &generation());
    }
}
