//! Agent-side adapter for the independently owned terminal service.
//!
//! This crate owns no process runtime, PTY, registry, spool, or terminal
//! persistence. It validates one exact service identity document and creates
//! cancellation-aware clients carrying the dispatch-time authority result.

use std::path::{Path, PathBuf};

use agl_exec::AuthorityFingerprint;
use agl_terminal_client::{ClientError, TerminalClient, UnixTerminalTransport};
use agl_terminal_protocol::ServiceIdentity;
use thiserror::Error;

const MAX_IDENTITY_BYTES: u64 = 4 * 1024;
pub const TERMINAL_SOURCE_REVISION: &str = "17134b9f20aa942ba1955331f6e9c9eb4706191e";
pub const TERMINAL_BUILD_ID: &str =
    "sha256:57e7080ad368f24fc63a63bbf9cd40af29cd66764df72203286304a5e9d1f760";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalEndpoint {
    socket_path: PathBuf,
    identity_path: PathBuf,
    expected_build_id: AuthorityFingerprint,
    source_revision: String,
}

impl TerminalEndpoint {
    pub fn new(
        socket_path: PathBuf,
        identity_path: PathBuf,
        expected_build_id: AuthorityFingerprint,
        source_revision: impl Into<String>,
    ) -> Result<Self, TerminalEndpointError> {
        if !socket_path.is_absolute() || !identity_path.is_absolute() {
            return Err(TerminalEndpointError::RelativePath);
        }
        let source_revision = source_revision.into();
        if source_revision.len() != 40
            || !source_revision
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(TerminalEndpointError::InvalidSourceRevision);
        }
        Ok(Self {
            socket_path,
            identity_path,
            expected_build_id,
            source_revision,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn identity_path(&self) -> &Path {
        &self.identity_path
    }

    pub fn source_revision(&self) -> &str {
        &self.source_revision
    }

    pub fn connect(
        &self,
        authority: AuthorityFingerprint,
    ) -> Result<TerminalClient<UnixTerminalTransport>, TerminalEndpointError> {
        let identity = self.read_identity()?;
        let transport =
            UnixTerminalTransport::new(self.socket_path.clone()).map_err(ClientError::Transport)?;
        TerminalClient::authorized(transport, identity, authority).map_err(Into::into)
    }

    pub fn connect_readiness(
        &self,
    ) -> Result<TerminalClient<UnixTerminalTransport>, TerminalEndpointError> {
        let identity = self.read_identity()?;
        let transport =
            UnixTerminalTransport::new(self.socket_path.clone()).map_err(ClientError::Transport)?;
        TerminalClient::new(transport, identity).map_err(Into::into)
    }

    pub fn read_identity(&self) -> Result<ServiceIdentity, TerminalEndpointError> {
        let metadata = std::fs::symlink_metadata(&self.identity_path).map_err(identity_io)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(TerminalEndpointError::UnsafeIdentityFile);
        }
        if metadata.len() == 0 || metadata.len() > MAX_IDENTITY_BYTES {
            return Err(TerminalEndpointError::IdentitySize);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(TerminalEndpointError::UnsafeIdentityPermissions);
            }
        }
        let bytes = std::fs::read(&self.identity_path).map_err(identity_io)?;
        let identity: ServiceIdentity = serde_json::from_slice(&bytes)
            .map_err(|error| TerminalEndpointError::InvalidIdentity(error.to_string()))?;
        identity
            .validate()
            .map_err(|error| TerminalEndpointError::InvalidIdentity(error.to_string()))?;
        if identity.build_id != self.expected_build_id {
            return Err(TerminalEndpointError::BuildMismatch);
        }
        Ok(identity)
    }
}

#[derive(Debug, Error)]
pub enum TerminalEndpointError {
    #[error("terminal endpoint paths must be absolute")]
    RelativePath,
    #[error("terminal source revision must be one exact lowercase 40-hex commit")]
    InvalidSourceRevision,
    #[error("terminal identity file is unavailable: {0}")]
    IdentityIo(String),
    #[error("terminal identity path must be a regular non-symlink file")]
    UnsafeIdentityFile,
    #[error("terminal identity file must be private")]
    UnsafeIdentityPermissions,
    #[error("terminal identity file is empty or exceeds four KiB")]
    IdentitySize,
    #[error("terminal identity is invalid: {0}")]
    InvalidIdentity(String),
    #[error("terminal service build identity does not match the configured generation")]
    BuildMismatch,
    #[error(transparent)]
    Client(#[from] ClientError),
}

fn identity_io(error: std::io::Error) -> TerminalEndpointError {
    TerminalEndpointError::IdentityIo(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use agl_exec::ServiceGenerationId;
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

    #[test]
    fn endpoint_requires_absolute_paths_and_exact_source_revision() {
        assert!(matches!(
            TerminalEndpoint::new(
                PathBuf::from("relative"),
                PathBuf::from("relative"),
                build('a'),
                "b".repeat(40)
            ),
            Err(TerminalEndpointError::RelativePath)
        ));
        assert!(matches!(
            TerminalEndpoint::new(
                PathBuf::from("/socket"),
                PathBuf::from("/identity"),
                build('a'),
                "main"
            ),
            Err(TerminalEndpointError::InvalidSourceRevision)
        ));
    }

    #[test]
    fn identity_is_exact_private_and_not_a_symlink() {
        let root = temporary_root("identity");
        std::fs::create_dir_all(&root).unwrap();
        let identity_path = root.join("identity.json");
        let identity = ServiceIdentity {
            protocol_version: TERMINAL_PROTOCOL_VERSION,
            crate_version: "1.0.0-alpha.1".to_owned(),
            build_id: build('a'),
            generation_id: ServiceGenerationId::generate(),
        };
        std::fs::write(&identity_path, serde_json::to_vec(&identity).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&identity_path, std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let endpoint = TerminalEndpoint::new(
            root.join("terminal.sock"),
            identity_path.clone(),
            build('a'),
            "b".repeat(40),
        )
        .unwrap();
        assert_eq!(endpoint.read_identity().unwrap(), identity);
        let mismatch = TerminalEndpoint::new(
            root.join("terminal.sock"),
            identity_path.clone(),
            build('c'),
            "b".repeat(40),
        )
        .unwrap();
        assert!(matches!(
            mismatch.read_identity(),
            Err(TerminalEndpointError::BuildMismatch)
        ));
        #[cfg(unix)]
        {
            let link = root.join("identity-link.json");
            std::os::unix::fs::symlink(&identity_path, &link).unwrap();
            let linked =
                TerminalEndpoint::new(root.join("terminal.sock"), link, build('a'), "b".repeat(40))
                    .unwrap();
            assert!(matches!(
                linked.read_identity(),
                Err(TerminalEndpointError::UnsafeIdentityFile)
            ));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
