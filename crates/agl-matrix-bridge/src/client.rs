use agl_client::{AgentLibreClient, DaemonTransport};
use agl_protocol::{
    HelloRequest, PROTOCOL_VERSION, ProtocolToolMode, SessionOpenRequest, SessionStatus,
    SessionStatusRequest, SessionTurnRequest, TurnTerminalStatus,
};
use anyhow::{Result, bail};

use crate::AgentClient;

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

    fn validate_session(&mut self, session_id: &str) -> Result<()> {
        let status = self.session_status(SessionStatusRequest {
            session_id: session_id.to_string(),
        })?;
        match status.status {
            SessionStatus::Open | SessionStatus::Busy => Ok(()),
            SessionStatus::Finished | SessionStatus::Failed => {
                bail!("session {session_id} is {:?}", status.status)
            }
        }
    }

    fn open_session(&mut self) -> Result<String> {
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
        session_id: &str,
        message: &str,
        idempotency_key: &str,
    ) -> Result<String> {
        let response = self.send_turn(SessionTurnRequest {
            session_id: session_id.to_string(),
            text: message.to_string(),
            idempotency_key: Some(idempotency_key.to_string()),
        })?;
        match response.status {
            TurnTerminalStatus::Answered => Ok(response.assistant_text),
            TurnTerminalStatus::Stopped | TurnTerminalStatus::Failed => {
                bail!("daemon turn ended with {:?}", response.status)
            }
        }
    }
}
