#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
use agl_client::UnixTransport;
use agl_client::{AgentLibreClient, DaemonTransport};
use agl_ids::SessionId;
use agl_protocol::{
    HelloRequest, PROTOCOL_VERSION, ProtocolToolMode, SessionOpenRequest, SessionStatus,
    SessionStatusRequest, SessionTurnRequest, TurnTerminalStatus,
};
#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, bail};

use crate::AgentClient;

#[cfg(unix)]
pub struct LazyDaemonClient {
    socket_path: PathBuf,
    inner: Option<AgentLibreClient<UnixTransport>>,
}

#[cfg(unix)]
impl LazyDaemonClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            inner: None,
        }
    }

    fn inner(&mut self) -> Result<&mut AgentLibreClient<UnixTransport>> {
        if self.inner.is_none() {
            self.inner = Some(
                AgentLibreClient::connect(&self.socket_path).with_context(|| {
                    format!(
                        "failed to connect to daemon socket {}",
                        self.socket_path.display()
                    )
                })?,
            );
        }
        Ok(self.inner.as_mut().expect("client initialized"))
    }
}

#[cfg(unix)]
impl AgentClient for LazyDaemonClient {
    fn daemon_status(&mut self) -> Result<String> {
        AgentClient::daemon_status(self.inner()?)
    }

    fn validate_session(&mut self, session_id: &SessionId) -> Result<()> {
        AgentClient::validate_session(self.inner()?, session_id)
    }

    fn open_session(&mut self) -> Result<SessionId> {
        AgentClient::open_session(self.inner()?)
    }

    fn send_message(
        &mut self,
        session_id: &SessionId,
        message: &str,
        idempotency_key: &str,
    ) -> Result<String> {
        AgentClient::send_message(self.inner()?, session_id, message, idempotency_key)
    }
}

impl<T> AgentClient for AgentLibreClient<T>
where
    T: DaemonTransport,
{
    fn daemon_status(&mut self) -> Result<String> {
        let hello = self.hello(HelloRequest {
            client_name: Some("agl-matrix-bridge".to_string()),
            accepted_protocol_versions: vec![PROTOCOL_VERSION.to_string()],
        })?;
        Ok(format!(
            "state=running protocol_version={} product_version={}",
            hello.protocol_version, hello.product_version
        ))
    }

    fn validate_session(&mut self, session_id: &SessionId) -> Result<()> {
        let status = self.session_status(SessionStatusRequest {
            session_id: session_id.clone(),
        })?;
        match status.status {
            SessionStatus::Open | SessionStatus::Busy => Ok(()),
            SessionStatus::Finished | SessionStatus::Failed => {
                bail!("session {session_id} is {:?}", status.status)
            }
        }
    }

    fn open_session(&mut self) -> Result<SessionId> {
        let opened = self.open_session(SessionOpenRequest {
            session_id: None,
            new_session: true,
            workspace_root: None,
            skills: Vec::new(),
            tool_mode: ProtocolToolMode::ReadOnly,
        })?;
        Ok(opened.session_id)
    }

    fn send_message(
        &mut self,
        session_id: &SessionId,
        message: &str,
        idempotency_key: &str,
    ) -> Result<String> {
        let response = self.send_turn(SessionTurnRequest {
            session_id: session_id.clone(),
            text: message.to_string(),
            idempotency_key: Some(idempotency_key.to_string()),
        })?;
        match response.status {
            TurnTerminalStatus::Answered => Ok(response.assistant_text),
            TurnTerminalStatus::Stopped
            | TurnTerminalStatus::Failed
            | TurnTerminalStatus::Cancelled => {
                bail!("daemon turn ended with {:?}", response.status)
            }
        }
    }
}
