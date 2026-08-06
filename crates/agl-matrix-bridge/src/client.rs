#[cfg(unix)]
use std::future::Future;
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
            self.inner = Some(self.block_on(async move {
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
        let status = self.block_on(client.session_status(SessionStatusRequest {
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
        let opened = self.block_on(client.open_session(SessionOpenRequest {
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
        self.block_on(async move {
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

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use agl_ids::{DaemonInstanceId, RequestId};
    use agl_protocol::{
        DaemonEvent, DaemonEventKind, DaemonRequest, HelloEvent, PROTOCOL_VERSION,
        RuntimeGenerationIdentity, RuntimeGenerationKind,
    };
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::*;

    #[test]
    fn daemon_status_is_safe_inside_multithread_tokio_runtime() {
        let outer = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        outer.block_on(async {
            let socket_path = std::env::temp_dir().join(format!(
                "agl-matrix-client-runtime-{}.sock",
                RequestId::generate()
            ));
            let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                let request: DaemonRequest =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                let response = DaemonEvent::new(
                    Some(request.request_id),
                    DaemonEventKind::Hello(HelloEvent {
                        protocol_version: PROTOCOL_VERSION.to_owned(),
                        product_version: "runtime-boundary-test".to_owned(),
                        daemon_instance_id: DaemonInstanceId::generate(),
                        daemon_runtime: RuntimeGenerationIdentity {
                            kind: RuntimeGenerationKind::Development,
                            generation_id: format!("sha256:{}", "a".repeat(64)),
                            builtin_catalog_digest: format!("sha256:{}", "b".repeat(64)),
                            executable_digest: format!("sha256:{}", "c".repeat(64)),
                        },
                        worker_build_id: format!("sha256:{}", "d".repeat(64)),
                        native_bundle_id: None,
                        composite_worker_build_id: None,
                        tools: Vec::new(),
                    }),
                );
                writer
                    .write_all(
                        format!("{}\n", serde_json::to_string(&response).unwrap()).as_bytes(),
                    )
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(10)).await;
            });

            let mut client = LazyDaemonClient::new(socket_path.clone());
            assert_eq!(
                client.daemon_status().unwrap(),
                format!(
                    "state=running protocol_version={} product_version=runtime-boundary-test",
                    PROTOCOL_VERSION
                )
            );
            drop(client);
            server.await.unwrap();
            std::fs::remove_file(socket_path).unwrap();
        });
    }
}
