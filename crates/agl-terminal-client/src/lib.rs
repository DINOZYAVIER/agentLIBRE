use std::fmt::{self, Display, Formatter};
use std::sync::Arc;

use agl_exec::{ProcessBytes, TerminalSize};
use agl_terminal::{TerminalDescriptor, TerminalId, TerminalStreamId};
use agl_terminal_protocol::{
    ProtocolValidationError, ServiceIdentity, TerminalAdmission, TerminalEventBatch,
    TerminalFailure, TerminalRequest, TerminalRequestKind, TerminalResponse, TerminalResponseKind,
};
use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TransportError {
    #[error("terminal transport was cancelled")]
    Cancelled,
    #[error("terminal transport is unavailable: {0}")]
    Unavailable(String),
    #[error("terminal transport rejected the frame: {0}")]
    InvalidFrame(String),
}

pub trait TerminalTransport: Send + Sync {
    fn exchange<'a>(
        &'a self,
        request: TerminalRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<TerminalResponse, TransportError>>;
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error(transparent)]
    Protocol(#[from] ProtocolValidationError),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("terminal request was cancelled")]
    Cancelled,
    #[error("terminal response request identity does not match")]
    CorrelationMismatch,
    #[error("terminal service returned an unexpected response kind")]
    UnexpectedResponse,
    #[error("terminal service rejected the operation: {0}")]
    Remote(TerminalFailureDisplay),
}

#[derive(Clone, Debug)]
pub struct TerminalFailureDisplay(pub TerminalFailure);

impl Display for TerminalFailureDisplay {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.0.code, self.0.message)
    }
}

pub struct TerminalClient<T> {
    transport: T,
    expected_service: ServiceIdentity,
}

impl<T> TerminalClient<T>
where
    T: TerminalTransport,
{
    pub fn new(transport: T, expected_service: ServiceIdentity) -> Result<Self, ClientError> {
        expected_service.validate()?;
        Ok(Self {
            transport,
            expected_service,
        })
    }

    pub fn expected_service(&self) -> &ServiceIdentity {
        &self.expected_service
    }

    pub async fn hello(&self, cancellation: CancellationToken) -> Result<(), ClientError> {
        match self.call(TerminalRequestKind::Hello, cancellation).await? {
            TerminalResponseKind::Hello => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn ensure(
        &self,
        admission: TerminalAdmission,
        cancellation: CancellationToken,
    ) -> Result<TerminalDescriptor, ClientError> {
        match self
            .call(
                TerminalRequestKind::Ensure {
                    admission: Box::new(admission),
                },
                cancellation,
            )
            .await?
        {
            TerminalResponseKind::Terminal { descriptor } => Ok(descriptor),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn inspect(
        &self,
        terminal_id: TerminalId,
        cancellation: CancellationToken,
    ) -> Result<TerminalDescriptor, ClientError> {
        match self
            .call(TerminalRequestKind::Inspect { terminal_id }, cancellation)
            .await?
        {
            TerminalResponseKind::Terminal { descriptor } => Ok(descriptor),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn attach(
        &self,
        terminal_id: TerminalId,
        after_sequence: u64,
        writable: bool,
        cancellation: CancellationToken,
    ) -> Result<TerminalAttachment, ClientError> {
        match self
            .call(
                TerminalRequestKind::Attach {
                    terminal_id,
                    after_sequence,
                    writable,
                },
                cancellation,
            )
            .await?
        {
            TerminalResponseKind::Attached {
                descriptor,
                stream_id,
                next_sequence,
                writable,
            } => Ok(TerminalAttachment {
                descriptor,
                stream_id,
                next_sequence,
                writable,
            }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn read_events(
        &self,
        stream_id: TerminalStreamId,
        after_sequence: u64,
        maximum_events: u16,
        cancellation: CancellationToken,
    ) -> Result<TerminalEventBatch, ClientError> {
        match self
            .call(
                TerminalRequestKind::ReadEvents {
                    stream_id,
                    after_sequence,
                    maximum_events,
                },
                cancellation,
            )
            .await?
        {
            TerminalResponseKind::Events { batch } => Ok(batch),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn input(
        &self,
        terminal_id: TerminalId,
        stream_id: TerminalStreamId,
        bytes: ProcessBytes,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        self.expect_ack(
            TerminalRequestKind::Input {
                terminal_id,
                stream_id,
                bytes,
            },
            cancellation,
        )
        .await
    }

    pub async fn resize(
        &self,
        terminal_id: TerminalId,
        size: TerminalSize,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        self.expect_ack(
            TerminalRequestKind::Resize { terminal_id, size },
            cancellation,
        )
        .await
    }

    pub async fn detach(
        &self,
        stream_id: TerminalStreamId,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        self.expect_ack(TerminalRequestKind::Detach { stream_id }, cancellation)
            .await
    }

    pub async fn terminate(
        &self,
        terminal_id: TerminalId,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        self.expect_ack(TerminalRequestKind::Terminate { terminal_id }, cancellation)
            .await
    }

    async fn expect_ack(
        &self,
        request: TerminalRequestKind,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        match self.call(request, cancellation).await? {
            TerminalResponseKind::Ack => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    async fn call(
        &self,
        request: TerminalRequestKind,
        cancellation: CancellationToken,
    ) -> Result<TerminalResponseKind, ClientError> {
        if cancellation.is_cancelled() {
            return Err(ClientError::Cancelled);
        }
        let request = TerminalRequest::new(self.expected_service.clone(), request)?;
        let request_id = request.request_id.clone();
        let response = self
            .transport
            .exchange(request, cancellation.clone())
            .await?;
        if cancellation.is_cancelled() {
            return Err(ClientError::Cancelled);
        }
        response.validate()?;
        if response.request_id != request_id {
            return Err(ClientError::CorrelationMismatch);
        }
        self.expected_service.require_exact(&response.service)?;
        match response.response {
            TerminalResponseKind::Failure { failure } => {
                Err(ClientError::Remote(TerminalFailureDisplay(failure)))
            }
            response => Ok(response),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalAttachment {
    pub descriptor: TerminalDescriptor,
    pub stream_id: TerminalStreamId,
    pub next_sequence: u64,
    pub writable: bool,
}

pub trait EmbeddedTerminalService: Send + Sync {
    fn handle<'a>(
        &'a self,
        request: TerminalRequest,
    ) -> BoxFuture<'a, Result<TerminalResponse, TransportError>>;
}

pub struct EmbeddedTransport<S> {
    service: Arc<S>,
}

impl<S> EmbeddedTransport<S> {
    pub fn new(service: Arc<S>) -> Self {
        Self { service }
    }
}

impl<S> Clone for EmbeddedTransport<S> {
    fn clone(&self) -> Self {
        Self {
            service: Arc::clone(&self.service),
        }
    }
}

impl<S> TerminalTransport for EmbeddedTransport<S>
where
    S: EmbeddedTerminalService + 'static,
{
    fn exchange<'a>(
        &'a self,
        request: TerminalRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<TerminalResponse, TransportError>> {
        async move {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => Err(TransportError::Cancelled),
                response = self.service.handle(request) => response,
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use agl_exec::{AuthorityFingerprint, ServiceGenerationId};
    use agl_terminal_protocol::{
        TERMINAL_PROTOCOL_VERSION, TERMINAL_RESPONSE_SCHEMA, TerminalFailureCode,
    };

    use super::*;

    fn service_identity() -> ServiceIdentity {
        ServiceIdentity {
            protocol_version: TERMINAL_PROTOCOL_VERSION,
            crate_version: env!("CARGO_PKG_VERSION").to_owned(),
            build_id: AuthorityFingerprint::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
            generation_id: ServiceGenerationId::generate(),
        }
    }

    struct EchoService {
        identity: ServiceIdentity,
        seen: Mutex<Vec<TerminalRequestKind>>,
    }

    impl EmbeddedTerminalService for EchoService {
        fn handle<'a>(
            &'a self,
            request: TerminalRequest,
        ) -> BoxFuture<'a, Result<TerminalResponse, TransportError>> {
            async move {
                self.seen.lock().unwrap().push(request.request);
                Ok(TerminalResponse {
                    schema: TERMINAL_RESPONSE_SCHEMA.to_owned(),
                    request_id: request.request_id,
                    service: self.identity.clone(),
                    response: TerminalResponseKind::Hello,
                })
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn embedded_transport_uses_the_same_exact_protocol() {
        let identity = service_identity();
        let service = Arc::new(EchoService {
            identity: identity.clone(),
            seen: Mutex::new(Vec::new()),
        });
        let client =
            TerminalClient::new(EmbeddedTransport::new(service.clone()), identity).unwrap();
        client.hello(CancellationToken::new()).await.unwrap();
        assert_eq!(
            *service.seen.lock().unwrap(),
            vec![TerminalRequestKind::Hello]
        );
    }

    struct BlockingService;

    impl EmbeddedTerminalService for BlockingService {
        fn handle<'a>(
            &'a self,
            _request: TerminalRequest,
        ) -> BoxFuture<'a, Result<TerminalResponse, TransportError>> {
            async move {
                std::future::pending::<()>().await;
                unreachable!()
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn cancellation_interrupts_an_in_flight_embedding_call() {
        let client = TerminalClient::new(
            EmbeddedTransport::new(Arc::new(BlockingService)),
            service_identity(),
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let child = cancellation.clone();
        let operation = async move { client.hello(child).await };
        tokio::pin!(operation);
        tokio::time::sleep(Duration::from_millis(1)).await;
        cancellation.cancel();
        assert!(matches!(operation.await, Err(ClientError::Cancelled)));
    }

    struct FailureService {
        identity: ServiceIdentity,
    }

    impl EmbeddedTerminalService for FailureService {
        fn handle<'a>(
            &'a self,
            request: TerminalRequest,
        ) -> BoxFuture<'a, Result<TerminalResponse, TransportError>> {
            async move {
                Ok(TerminalResponse {
                    schema: TERMINAL_RESPONSE_SCHEMA.to_owned(),
                    request_id: request.request_id,
                    service: self.identity.clone(),
                    response: TerminalResponseKind::Failure {
                        failure: TerminalFailure {
                            code: TerminalFailureCode::AuthorityDenied,
                            message: "authority does not admit this operation".to_owned(),
                            retryable: false,
                        },
                    },
                })
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn typed_remote_failures_do_not_become_transport_success() {
        let identity = service_identity();
        let client = TerminalClient::new(
            EmbeddedTransport::new(Arc::new(FailureService {
                identity: identity.clone(),
            })),
            identity,
        )
        .unwrap();
        assert!(matches!(
            client.hello(CancellationToken::new()).await,
            Err(ClientError::Remote(_))
        ));
    }
}
