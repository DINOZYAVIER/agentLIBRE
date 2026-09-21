use std::fmt;
use std::path::PathBuf;

use crate::{
    AGENT_PROTOCOL_SCHEMA, AgentCommand, AgentProtocolRequest, AgentProtocolResponse,
    AgentProtocolStreamFrame, AgentReply, AgentResponse, AgentSubscriptionFrame,
    FunctionActivationView, MAX_JSONL_FRAME_BYTES, PlanArtifactView, ProtocolError,
};
use agl_core::agent::{
    AgentEvent, AgentEventId, AgentEventPage, AgentMessagePage, AgentOperationKey,
    AgentOperationView, AgentRunSpec, AgentRunView, ConversationView,
};
use agl_core::implementation_plan::{PlanDigest, PlanId};
use agl_core::{AgentRunId, Content, ConversationId, MessageId, RequestId};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Clone, Debug)]
pub struct AgentClient {
    socket_path: PathBuf,
}

impl AgentClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub async fn open_conversation(
        &self,
        conversation_id: ConversationId,
        function_path: impl Into<String>,
        workspace_path: impl Into<String>,
    ) -> Result<(ConversationView, FunctionActivationView), ClientError> {
        match self
            .request(AgentCommand::OpenConversation {
                conversation_id,
                function_path: function_path.into(),
                workspace_path: workspace_path.into(),
            })
            .await?
        {
            AgentResponse::ConversationOpened {
                conversation,
                activation,
            } => Ok((conversation, *activation)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn create_plan(
        &self,
        function_path: impl Into<String>,
        workspace_path: impl Into<String>,
        prompt: Content,
    ) -> Result<(PlanArtifactView, ConversationView), ClientError> {
        match self
            .request(AgentCommand::PlanCreate {
                function_path: function_path.into(),
                workspace_path: workspace_path.into(),
                prompt,
            })
            .await?
        {
            AgentResponse::PlanCreated { plan, conversation } => Ok((*plan, conversation)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn view_plan(&self, plan_id: PlanId) -> Result<PlanArtifactView, ClientError> {
        match self.request(AgentCommand::PlanView { plan_id }).await? {
            AgentResponse::PlanViewed { plan } => Ok(*plan),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn approve_plan(
        &self,
        plan_id: PlanId,
        expected_digest: PlanDigest,
    ) -> Result<PlanArtifactView, ClientError> {
        match self
            .request(AgentCommand::PlanApprove {
                plan_id,
                expected_digest,
            })
            .await?
        {
            AgentResponse::PlanApproved { plan } => Ok(*plan),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn implement_plan(
        &self,
        plan_id: PlanId,
        expected_digest: PlanDigest,
        function_path: impl Into<String>,
    ) -> Result<PlanArtifactView, ClientError> {
        match self
            .request(AgentCommand::PlanImplement {
                plan_id,
                expected_digest,
                function_path: function_path.into(),
            })
            .await?
        {
            AgentResponse::PlanImplementStarted { plan } => Ok(*plan),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn plan_status(&self, plan_id: PlanId) -> Result<PlanArtifactView, ClientError> {
        match self.request(AgentCommand::PlanStatus { plan_id }).await? {
            AgentResponse::PlanStatus { plan } => Ok(*plan),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn conversations(
        &self,
        workspace_path: Option<String>,
        limit: u16,
    ) -> Result<Vec<ConversationView>, ClientError> {
        match self
            .request(AgentCommand::Conversations {
                workspace_path,
                limit,
            })
            .await?
        {
            AgentResponse::Conversations { conversations } => Ok(conversations),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn resolve_conversation(
        &self,
        selector: impl Into<String>,
    ) -> Result<ConversationView, ClientError> {
        match self
            .request(AgentCommand::ResolveConversation {
                selector: selector.into(),
            })
            .await?
        {
            AgentResponse::ConversationResolved { conversation } => Ok(conversation),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn rename_conversation(
        &self,
        selector: impl Into<String>,
        display_name: impl Into<String>,
    ) -> Result<ConversationView, ClientError> {
        match self
            .request(AgentCommand::RenameConversation {
                selector: selector.into(),
                display_name: display_name.into(),
            })
            .await?
        {
            AgentResponse::ConversationRenamed { conversation } => Ok(conversation),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn start_run(&self, spec: AgentRunSpec) -> Result<AgentRunId, ClientError> {
        match self.request(AgentCommand::StartRun { spec }).await? {
            AgentResponse::RunStarted { run_id } => Ok(run_id),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn cancel(&self, run_id: AgentRunId) -> Result<(), ClientError> {
        match self.request(AgentCommand::CancelRun { run_id }).await? {
            AgentResponse::RunCancelled { run_id: cancelled } if cancelled == run_id => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn run_view(&self, run_id: AgentRunId) -> Result<AgentRunView, ClientError> {
        match self.request(AgentCommand::RunView { run_id }).await? {
            AgentResponse::RunView { view } => Ok(view),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn operation_view(
        &self,
        key: AgentOperationKey,
    ) -> Result<AgentOperationView, ClientError> {
        match self.request(AgentCommand::OperationView { key }).await? {
            AgentResponse::OperationView { operation } => Ok(operation),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn events(
        &self,
        after: Option<AgentEventId>,
        limit: u16,
    ) -> Result<AgentEventPage, ClientError> {
        match self.request(AgentCommand::Events { after, limit }).await? {
            AgentResponse::Events { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn messages(
        &self,
        conversation_id: ConversationId,
        after: Option<MessageId>,
        limit: u16,
    ) -> Result<AgentMessagePage, ClientError> {
        match self
            .request(AgentCommand::Messages {
                conversation_id,
                after,
                limit,
            })
            .await?
        {
            AgentResponse::Messages { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn subscribe(&self, run_id: AgentRunId) -> Result<AgentSubscription, ClientError> {
        let request_id = RequestId::generate();
        let request =
            AgentProtocolRequest::new(request_id.clone(), AgentCommand::Subscribe { run_id });
        request.validate().map_err(ClientError::Protocol)?;
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(ClientError::Io)?;
        verify_peer(&stream)?;
        let mut bytes = serde_json::to_vec(&request).map_err(ClientError::Json)?;
        bytes.push(b'\n');
        stream.write_all(&bytes).await.map_err(ClientError::Io)?;
        let mut reader = BufReader::new(stream);
        let first = read_stream_frame(&mut reader, &request_id).await?;
        let AgentSubscriptionFrame::Subscribed { run_view, cursor } = first else {
            return Err(ClientError::UnexpectedResponse);
        };
        if run_view.id != run_id {
            return Err(ClientError::RequestMismatch);
        }
        Ok(AgentSubscription {
            reader,
            request_id,
            run_id,
            run_view,
            cursor,
        })
    }

    async fn request(&self, command: AgentCommand) -> Result<AgentResponse, ClientError> {
        let request_id = RequestId::generate();
        let request = AgentProtocolRequest::new(request_id.clone(), command);
        request.validate().map_err(ClientError::Protocol)?;
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(ClientError::Io)?;
        verify_peer(&stream)?;
        let mut bytes = serde_json::to_vec(&request).map_err(ClientError::Json)?;
        bytes.push(b'\n');
        stream.write_all(&bytes).await.map_err(ClientError::Io)?;

        let response = read_bounded_line(&mut BufReader::new(stream))
            .await?
            .ok_or(ClientError::InvalidFrame)?;
        let response: AgentProtocolResponse =
            serde_json::from_slice(&response).map_err(ClientError::Json)?;
        if response.schema != AGENT_PROTOCOL_SCHEMA || response.request_id != request_id {
            return Err(ClientError::RequestMismatch);
        }
        match response.reply {
            AgentReply::Ok { response } => Ok(*response),
            AgentReply::Error { error } => Err(ClientError::Protocol(error)),
        }
    }
}

pub struct AgentSubscription {
    reader: BufReader<UnixStream>,
    request_id: RequestId,
    run_id: AgentRunId,
    run_view: AgentRunView,
    cursor: AgentEventId,
}

impl AgentSubscription {
    pub fn initial_view(&self) -> &AgentRunView {
        &self.run_view
    }

    pub fn cursor(&self) -> AgentEventId {
        self.cursor
    }

    pub async fn next(&mut self) -> Result<AgentSubscriptionFrame, ClientError> {
        loop {
            let frame = read_stream_frame(&mut self.reader, &self.request_id).await?;
            match &frame {
                AgentSubscriptionFrame::Event { event } => {
                    if event.agent_run_id != self.run_id {
                        return Err(ClientError::InvalidFrame);
                    }
                    if event.id <= self.cursor {
                        continue;
                    }
                    self.cursor = event.id;
                }
                AgentSubscriptionFrame::Progress { progress } => {
                    let operation = match progress {
                        crate::AgentProgress::ModelOutputDelta { operation, .. }
                        | crate::AgentProgress::OperationStatus { operation, .. } => operation,
                    };
                    if operation.run_id != self.run_id {
                        return Err(ClientError::InvalidFrame);
                    }
                }
                AgentSubscriptionFrame::Lagged { .. } => {}
                AgentSubscriptionFrame::Ended { run_view, cursor } => {
                    if run_view.id != self.run_id || *cursor < self.cursor {
                        return Err(ClientError::InvalidFrame);
                    }
                    self.cursor = *cursor;
                    self.run_view = run_view.clone();
                }
                AgentSubscriptionFrame::Subscribed { .. } => {
                    return Err(ClientError::InvalidFrame);
                }
            }
            return Ok(frame);
        }
    }

    pub async fn resynchronize(
        &mut self,
        client: &AgentClient,
    ) -> Result<SubscriptionSnapshot, ClientError> {
        let view = client.run_view(self.run_id).await?;
        let mut after = self.cursor;
        let mut events = Vec::new();
        while after < view.last_event_id {
            let page = client.events(Some(after), 1_000).await?;
            let Some(last) = page.events.last().map(|event| event.id) else {
                return Err(ClientError::InvalidFrame);
            };
            for event in page.events {
                if event.agent_run_id == self.run_id {
                    events.push(event);
                }
            }
            after = last;
        }
        self.cursor = after;
        self.run_view = view.clone();
        Ok(SubscriptionSnapshot { view, events })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubscriptionSnapshot {
    pub view: AgentRunView,
    pub events: Vec<AgentEvent>,
}

async fn read_stream_frame(
    reader: &mut BufReader<UnixStream>,
    request_id: &RequestId,
) -> Result<AgentSubscriptionFrame, ClientError> {
    let bytes = read_bounded_line(reader)
        .await?
        .ok_or(ClientError::InvalidFrame)?;
    let frame: AgentProtocolStreamFrame =
        serde_json::from_slice(&bytes).map_err(ClientError::Json)?;
    if frame.schema != AGENT_PROTOCOL_SCHEMA || &frame.request_id != request_id {
        return Err(ClientError::RequestMismatch);
    }
    Ok(frame.frame)
}

async fn read_bounded_line<R>(reader: &mut R) -> Result<Option<Vec<u8>>, ClientError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(ClientError::Io)?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(ClientError::InvalidFrame)
            };
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(consumed) > MAX_JSONL_FRAME_BYTES + 1 {
            return Err(ClientError::InvalidFrame);
        }
        line.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if line.last() == Some(&b'\n') {
            return Ok(Some(line));
        }
    }
}

fn verify_peer(stream: &UnixStream) -> Result<(), ClientError> {
    let peer = stream.peer_cred().map_err(ClientError::Io)?;
    // SAFETY: geteuid has no preconditions and does not mutate process state.
    let expected = unsafe { libc::geteuid() };
    if peer.uid() != expected {
        return Err(ClientError::PeerIdentity);
    }
    Ok(())
}

#[derive(Debug)]
pub enum ClientError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Protocol(ProtocolError),
    PeerIdentity,
    InvalidFrame,
    RequestMismatch,
    UnexpectedResponse,
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "daemon I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "daemon frame is invalid: {error}"),
            Self::Protocol(error) => {
                write!(formatter, "daemon rejected request: {}", error.message)
            }
            Self::PeerIdentity => formatter.write_str("daemon peer is not owned by this user"),
            Self::InvalidFrame => {
                formatter.write_str("daemon returned an empty or oversized frame")
            }
            Self::RequestMismatch => {
                formatter.write_str("daemon response request identity differs")
            }
            Self::UnexpectedResponse => {
                formatter.write_str("daemon returned a different response kind")
            }
        }
    }
}

impl ClientError {
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Protocol(error) if error.code == crate::ProtocolErrorCode::NotFound
        )
    }
}

impl std::error::Error for ClientError {}
