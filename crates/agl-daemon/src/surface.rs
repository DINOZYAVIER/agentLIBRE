use std::sync::{Arc, Mutex, Weak};

use agl_app::{
    ApplicationBackend, ApplicationError, ApplicationErrorCode, ApplicationService, CommandContext,
    CommandId, LocalOperatorPrincipal, PresentationSubscribe, PromptAdmission, PromptSubmission,
    SessionOpen, SessionOpened, SessionPresentationEventEnvelope, SessionPresentationSnapshot,
    SuggestionPage, SuggestionRequest, UserShellAdmission, UserShellSubmission,
};
use agl_ids::{DaemonInstanceId, RequestId, SessionId};
use agl_protocol::{
    ApplicationActionResultEvent, CommandCatalogEvent, CommandCatalogRequest, DaemonEvent,
    DaemonEventKind, DaemonRequestKind, ProtocolError, ProtocolErrorCode, SessionPresentationEvent,
    UserShellAcceptedEvent,
};
use serde::{Serialize, de::DeserializeOwned};

use crate::state::DaemonState;

pub(crate) fn application_service(
    daemon_instance_id: DaemonInstanceId,
    state: Weak<Mutex<DaemonState>>,
) -> ApplicationService {
    ApplicationService::new(
        daemon_instance_id,
        Arc::new(DaemonApplicationBackend { state }),
    )
}

struct DaemonApplicationBackend {
    state: Weak<Mutex<DaemonState>>,
}

impl DaemonApplicationBackend {
    fn with_state<T>(
        &self,
        operation: impl FnOnce(&mut DaemonState) -> Result<T, ApplicationError>,
    ) -> Result<T, ApplicationError> {
        let state = self.state.upgrade().ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::OutcomeUnknown,
                "daemon is shutting down",
            )
        })?;
        let mut state = state.lock().map_err(|_| {
            ApplicationError::new(ApplicationErrorCode::Internal, "daemon state lock poisoned")
        })?;
        operation(&mut state)
    }
}

impl ApplicationBackend for DaemonApplicationBackend {
    fn open_session(&self, request: SessionOpen) -> Result<SessionOpened, ApplicationError> {
        self.with_state(|state| state.application_open_session(request))
    }

    fn snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionPresentationSnapshot, ApplicationError> {
        self.with_state(|state| state.application_snapshot(session_id))
    }

    fn invoke(
        &self,
        request: agl_app::ApplicationActionRequest,
    ) -> Result<agl_app::ApplicationActionResult, ApplicationError> {
        self.with_state(|state| state.application_invoke(request))
    }

    fn submit_prompt(
        &self,
        request: PromptSubmission,
    ) -> Result<PromptAdmission, ApplicationError> {
        self.with_state(|state| state.application_submit_prompt(request))
    }

    fn start_user_shell(
        &self,
        request: UserShellSubmission,
    ) -> Result<UserShellAdmission, ApplicationError> {
        self.with_state(|state| state.application_start_user_shell(request))
    }

    fn suggestions(&self, request: SuggestionRequest) -> Result<SuggestionPage, ApplicationError> {
        self.with_state(|state| state.application_suggestions(request))
    }
}

pub(crate) async fn handle_finite_request(
    application: &ApplicationService,
    request_id: RequestId,
    request: DaemonRequestKind,
    operator_uid: u32,
) -> DaemonEvent {
    let result = match request {
        DaemonRequestKind::CommandCatalog(request) => command_catalog(application, request).await,
        DaemonRequestKind::CommandSuggestions(request) => {
            let request = (|| {
                Ok(SuggestionRequest {
                    command_id: CommandId::parse(request.command_id)?,
                    argument_id: request.argument_id,
                    query: request.query,
                    cursor: request.cursor,
                })
            })();
            match request {
                Ok(request) => application
                    .command_suggestions(request)
                    .await
                    .and_then(|page| wire_convert(page).map(DaemonEventKind::CommandSuggestions)),
                Err(error) => Err(error),
            }
        }
        DaemonRequestKind::ApplicationAction(request) => match wire_convert(request) {
            Ok(request) => application
                .invoke(request)
                .await
                .and_then(application_action_result)
                .map(|result| {
                    DaemonEventKind::ApplicationActionResult(ApplicationActionResultEvent {
                        result,
                    })
                }),
            Err(error) => Err(error),
        },
        DaemonRequestKind::SessionPresentation(request) => application
            .snapshot(&request.session_id)
            .await
            .and_then(wire_convert)
            .map(|snapshot| {
                DaemonEventKind::SessionPresentation(SessionPresentationEvent { snapshot })
            }),
        DaemonRequestKind::UserShellStart(request) => {
            let submission = UserShellSubmission {
                session_id: request.session_id,
                client_submission_id: request.client_submission_id,
                command: request.command,
                execution_context_revision: request.execution_context_revision,
                profile: request.profile,
                terminal_size: request.terminal_size,
                background: request.background,
                operator: LocalOperatorPrincipal { uid: operator_uid },
            };
            application
                .start_user_shell(submission)
                .await
                .and_then(wire_convert::<_, UserShellAcceptedEvent>)
                .map(DaemonEventKind::UserShellAccepted)
        }
        _ => Err(ApplicationError::new(
            ApplicationErrorCode::InvalidArguments,
            "request is not a finite application-surface operation",
        )),
    };
    DaemonEvent::new(
        Some(request_id),
        result.unwrap_or_else(|error| DaemonEventKind::Error(protocol_error(error))),
    )
}

async fn command_catalog(
    application: &ApplicationService,
    request: CommandCatalogRequest,
) -> Result<DaemonEventKind, ApplicationError> {
    let context = if let Some(session_id) = request.session_id {
        application.snapshot(&session_id).await?.command_context
    } else {
        CommandContext::default()
    };
    let catalog = application.command_catalog(context).await?;
    let mut event: CommandCatalogEvent = wire_convert(catalog)?;
    if request
        .client_effects
        .contains(&agl_protocol::ClientEffectKind::Help)
    {
        event.descriptors.push(client_descriptor(
            "client.help",
            "help",
            "Show command help",
        ));
    }
    if request
        .client_effects
        .contains(&agl_protocol::ClientEffectKind::Disconnect)
    {
        event.descriptors.push(client_descriptor(
            "client.disconnect",
            "disconnect",
            "Disconnect this surface",
        ));
    }
    event
        .descriptors
        .sort_by(|left, right| left.id.cmp(&right.id));
    Ok(DaemonEventKind::CommandCatalog(event))
}

fn client_descriptor(id: &str, name: &str, summary: &str) -> agl_protocol::CommandDescriptor {
    agl_protocol::CommandDescriptor {
        id: id.to_owned(),
        name: name.to_owned(),
        aliases: Vec::new(),
        summary: summary.to_owned(),
        category: agl_protocol::CommandCategory::Client,
        arguments: Vec::new(),
        action_kind: agl_protocol::ApplicationActionKind::SessionStatus,
        concurrency: agl_protocol::CommandConcurrency::SurfaceLocal,
        availability: agl_protocol::CommandAvailability::Enabled,
    }
}

fn application_action_result(
    result: agl_app::ApplicationActionResult,
) -> Result<agl_protocol::ApplicationActionResult, ApplicationError> {
    match result {
        agl_app::ApplicationActionResult::SessionOpened { opened } => {
            Ok(agl_protocol::ApplicationActionResult::SessionOpened {
                session_id: opened.session_id,
                resumed: opened.resumed,
                snapshot: Box::new(wire_convert(opened.snapshot)?),
            })
        }
        other => wire_convert(other),
    }
}

pub(crate) fn presentation_subscribe(session_id: SessionId) -> PresentationSubscribe {
    PresentationSubscribe { session_id }
}

pub(crate) fn presentation_event(
    event: SessionPresentationEventEnvelope,
) -> Result<agl_protocol::SessionPresentationEventEnvelope, ApplicationError> {
    wire_convert(event)
}

pub(crate) fn presentation_snapshot(
    snapshot: SessionPresentationSnapshot,
) -> Result<agl_protocol::SessionPresentationSnapshot, ApplicationError> {
    wire_convert(snapshot)
}

pub(crate) fn protocol_error(error: ApplicationError) -> ProtocolError {
    let (code, retryable) = match error.code {
        ApplicationErrorCode::InvalidArguments | ApplicationErrorCode::StaleContextRevision => {
            (ProtocolErrorCode::InvalidRequest, false)
        }
        ApplicationErrorCode::NotFound => (ProtocolErrorCode::NotFound, false),
        ApplicationErrorCode::NotAuthorized => (ProtocolErrorCode::Unauthorized, false),
        ApplicationErrorCode::CommandUnavailable
        | ApplicationErrorCode::ModelNotInstalled
        | ApplicationErrorCode::ModelContextTooSmall
        | ApplicationErrorCode::SkillNotAdmitted => (ProtocolErrorCode::Unsupported, false),
        ApplicationErrorCode::SessionBusy | ApplicationErrorCode::InputBackpressure => {
            (ProtocolErrorCode::Busy, true)
        }
        ApplicationErrorCode::ResyncRequired
        | ApplicationErrorCode::OutcomeUnknown
        | ApplicationErrorCode::Internal => (ProtocolErrorCode::RuntimeFailure, false),
    };
    let mut protocol = ProtocolError::new(code, error.message, retryable);
    protocol.safe_metadata.insert(
        "application_code".to_owned(),
        error.code.as_str().to_owned(),
    );
    protocol
}

fn wire_convert<S, T>(source: S) -> Result<T, ApplicationError>
where
    S: Serialize,
    T: DeserializeOwned,
{
    let value = serde_json::to_value(source).map_err(conversion_error)?;
    serde_json::from_value(value).map_err(conversion_error)
}

fn conversion_error(error: serde_json::Error) -> ApplicationError {
    ApplicationError::new(
        ApplicationErrorCode::Internal,
        format!("surface protocol conversion failed: {error}"),
    )
}
