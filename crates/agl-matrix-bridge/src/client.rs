#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
use agl_client::{AgentLibreClient, RunSubscriptionEvent};
use agl_content::Content;
use agl_ids::SessionId;
use agl_protocol::{
    ProtocolRunState, ProtocolToolMode, RunBudgetRequest, RunSubmitRequest, RunSubscribeRequest,
    SessionOpenRequest, SessionStatus, SessionStatusRequest, SessionTranscriptRequest,
    TranscriptEvent,
};
#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, bail};

use crate::AgentClient;

#[cfg(unix)]
pub struct LazyDaemonClient {
    socket_path: PathBuf,
    runtime: Option<tokio::runtime::Runtime>,
    inner: Option<AgentLibreClient>,
}

#[cfg(unix)]
impl LazyDaemonClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            runtime: Some(tokio::runtime::Runtime::new().expect("matrix daemon client runtime")),
            inner: None,
        }
    }

    fn ensure_connected(&mut self) -> Result<()> {
        if self.inner.is_none() {
            let socket_path = self.socket_path.clone();
            self.inner = Some(self.runtime().block_on(async move {
                AgentLibreClient::connect(&socket_path)
                    .await
                    .with_context(|| {
                        format!(
                            "failed to connect to daemon socket {}",
                            socket_path.display()
                        )
                    })
            })?);
        }
        Ok(())
    }

    fn client(&self) -> &AgentLibreClient {
        self.inner.as_ref().expect("client initialized")
    }

    fn runtime(&self) -> &tokio::runtime::Runtime {
        self.runtime.as_ref().expect("client runtime initialized")
    }
}

#[cfg(unix)]
impl Drop for LazyDaemonClient {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

#[cfg(unix)]
impl AgentClient for LazyDaemonClient {
    fn daemon_status(&mut self) -> Result<String> {
        self.ensure_connected()?;
        let hello = self.client().hello()?;
        Ok(format!(
            "state=running protocol_version={} product_version={}",
            hello.protocol_version, hello.product_version
        ))
    }

    fn validate_session(&mut self, session_id: &SessionId) -> Result<()> {
        self.ensure_connected()?;
        let client = self.client().clone();
        let status = self
            .runtime()
            .block_on(client.session_status(SessionStatusRequest {
                session_id: session_id.clone(),
            }))?;
        match status.status {
            SessionStatus::Open | SessionStatus::Busy => Ok(()),
            SessionStatus::Finished | SessionStatus::Failed => {
                bail!("session {session_id} is {:?}", status.status)
            }
        }
    }

    fn open_session(&mut self) -> Result<SessionId> {
        self.ensure_connected()?;
        let client = self.client().clone();
        let opened = self
            .runtime()
            .block_on(client.open_session(SessionOpenRequest {
                session_id: None,
                new_session: true,
                workspace_root: None,
                function_ref: None,
                skills: Vec::new(),
                tool_mode: ProtocolToolMode::ReadOnly,
            }))?;
        Ok(opened.session_id)
    }

    fn send_message(
        &mut self,
        session_id: &SessionId,
        message: &str,
        idempotency_key: &str,
    ) -> Result<String> {
        self.ensure_connected()?;
        let client = self.client().clone();
        let session_id = session_id.clone();
        let message = message.to_owned();
        let idempotency_key = idempotency_key.to_owned();
        self.runtime().block_on(async move {
            let accepted = client
                .submit_run(RunSubmitRequest {
                    session_id: session_id.clone(),
                    content: Content::text(message)?,
                    client_submission_id: idempotency_key,
                    budget: RunBudgetRequest::default(),
                })
                .await?;
            let mut subscription = client
                .subscribe_run(RunSubscribeRequest {
                    run_id: accepted.run_id.clone(),
                    after_sequence: 0,
                })
                .await?;
            let terminal = loop {
                match subscription.next().await? {
                    Some(RunSubscriptionEvent::Event(_)) => {}
                    Some(RunSubscriptionEvent::Finished(event)) => break event,
                    None => bail!("daemon run subscription ended without terminal state"),
                }
            };
            if terminal.state != ProtocolRunState::Succeeded {
                bail!("daemon turn ended with {:?}", terminal.state);
            }
            let transcript = client
                .read_transcript(SessionTranscriptRequest {
                    session_id,
                    include_content: true,
                })
                .await?;
            transcript
                .events
                .into_iter()
                .rev()
                .find_map(|event| match event {
                    TranscriptEvent::AssistantMessage {
                        run_id, content, ..
                    } if run_id == accepted.run_id => {
                        content.and_then(|content| content.text_only())
                    }
                    _ => None,
                })
                .ok_or_else(|| anyhow::anyhow!("daemon turn produced no assistant message"))
        })
    }
}
