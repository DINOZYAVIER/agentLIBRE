#[cfg(unix)]
use std::future::Future;
#[cfg(unix)]
use std::path::PathBuf;

use agl_core::Content;
use agl_core::agent::{AgentRunOrigin, AgentRunSpec, AgentRunStatus, MessageRole};
use agl_core::{ConversationId, MessageId};
#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, bail};

use crate::AgentBoundary;

#[cfg(unix)]
pub struct LazyDaemonClient {
    socket_path: PathBuf,
    runtime: Option<tokio::runtime::Runtime>,
    inner: agl_daemon_api::AgentClient,
}

#[cfg(unix)]
impl LazyDaemonClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            inner: agl_daemon_api::AgentClient::new(socket_path.clone()),
            socket_path,
            runtime: Some(tokio::runtime::Runtime::new().expect("matrix daemon client runtime")),
        }
    }

    fn runtime(&self) -> &tokio::runtime::Runtime {
        self.runtime.as_ref().expect("client runtime initialized")
    }

    fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        if tokio::runtime::Handle::try_current().is_ok_and(|handle| {
            handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
        }) {
            tokio::task::block_in_place(|| self.runtime().block_on(future))
        } else {
            self.runtime().block_on(future)
        }
    }

    pub fn daemon_status(&self) -> Result<String> {
        let client = self.inner.clone();
        let socket_path = self.socket_path.display().to_string();
        self.block_on(async move {
            client
                .events(None, 1)
                .await
                .context("failed to connect to daemon")?;
            Ok(format!("state=running socket={socket_path}"))
        })
    }

    async fn wait_for_reply(
        client: agl_daemon_api::AgentClient,
        conversation_id: ConversationId,
        message_id: MessageId,
        message: String,
    ) -> Result<String> {
        let run_id = client
            .start_run(AgentRunSpec {
                reasoning: None,
                origin: AgentRunOrigin::User {
                    conversation_id,
                    message_id: message_id.clone(),
                },
                input: Content::text(message)?,
            })
            .await
            .context("daemon rejected Matrix AgentRun")?;
        let mut subscription = client
            .subscribe(run_id)
            .await
            .context("failed to subscribe to Matrix AgentRun")?;
        let mut status = subscription.initial_view().status;
        while !status.is_terminal() {
            subscription
                .next()
                .await
                .context("AgentRun subscription ended before terminal state")?;
            status = client
                .run_view(run_id)
                .await
                .context("failed to refresh Matrix AgentRun")?
                .status;
        }
        match status {
            AgentRunStatus::Completed => {}
            AgentRunStatus::Failed | AgentRunStatus::Cancelled => {
                bail!("Matrix AgentRun {run_id} ended with {status:?}")
            }
            AgentRunStatus::Pending | AgentRunStatus::Running => {
                unreachable!("terminal status checked above")
            }
        }

        let page = client
            .messages(conversation_id, Some(message_id), 1_000)
            .await
            .context("failed to read Matrix conversation reply")?;
        page.messages
            .into_iter()
            .rev()
            .find_map(|entry| {
                (entry.run_id == Some(run_id) && entry.role == MessageRole::Assistant)
                    .then(|| entry.content.into_text())
            })
            .ok_or_else(|| anyhow::anyhow!("Matrix AgentRun produced no text assistant message"))
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
impl AgentBoundary for LazyDaemonClient {
    fn ensure_conversation(
        &mut self,
        conversation_id: ConversationId,
        function_path: &str,
        workspace_path: &str,
    ) -> Result<()> {
        let function_path = PathBuf::from(function_path)
            .canonicalize()
            .context("failed to canonicalize Matrix Function path")?;
        let workspace_path = PathBuf::from(workspace_path)
            .canonicalize()
            .context("failed to canonicalize Matrix workspace path")?;
        let function_path = function_path
            .to_str()
            .context("Matrix Function path is not valid UTF-8")?
            .to_owned();
        let workspace_path = workspace_path
            .to_str()
            .context("Matrix workspace path is not valid UTF-8")?
            .to_owned();
        let client = self.inner.clone();
        self.block_on(async move {
            match client
                .resolve_conversation(conversation_id.to_string())
                .await
            {
                Ok(existing) if existing.id == conversation_id => return Ok(()),
                Ok(_) => anyhow::bail!("daemon resolved a different Matrix Conversation"),
                Err(error) if error.is_not_found() => {}
                Err(error) => {
                    return Err(error).context("failed to resolve Matrix Conversation");
                }
            }
            client
                .open_conversation(conversation_id, function_path, workspace_path)
                .await
                .context("daemon rejected Matrix Conversation activation")?;
            Ok(())
        })
    }

    fn send_message(
        &mut self,
        conversation_id: ConversationId,
        message_id: MessageId,
        message: &str,
    ) -> Result<String> {
        let client = self.inner.clone();
        let message = message.to_owned();
        self.block_on(Self::wait_for_reply(
            client,
            conversation_id,
            message_id,
            message,
        ))
    }
}
