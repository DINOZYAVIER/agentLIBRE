use std::sync::{Arc, Weak};

use agl_app::{
    ApplicationBackend, ApplicationCallContext, ApplicationError, ApplicationErrorCode,
    ApplicationService, CommandContext, CommandId, HumanTerminalCommandAdmission,
    HumanTerminalCommandSubmit, HumanTerminalEnsure, PresentationSubscribe, PromptAdmission,
    PromptAdmissionState, PromptBudget, PromptSubmission, SessionOpen, SessionOpened,
    SessionPresentationEventEnvelope, SessionPresentationSnapshot, SuggestionPage,
    SuggestionRequest, TerminalEnsured,
};
use agl_ids::{DaemonInstanceId, RequestId, SessionId};
use agl_protocol::{
    ApplicationToolResultEvent, CommandCatalogEvent, CommandCatalogRequest, DaemonEvent,
    DaemonEventKind, DaemonRequestKind, HumanHostTerminalEnsureRequest, HumanTerminalEnsuredEvent,
    ProtocolError, ProtocolErrorCode, ProtocolRunState, RunAcceptedEvent, RunSubmitRequest,
};
use serde::{Serialize, de::DeserializeOwned};

use crate::state::{
    DaemonState, DaemonStateExecutor, SharedDaemonState, daemon_state_application_error,
};

pub(crate) fn application_service(
    daemon_instance_id: DaemonInstanceId,
    state: Weak<DaemonStateExecutor>,
) -> ApplicationService {
    ApplicationService::new(
        daemon_instance_id,
        Arc::new(DaemonApplicationBackend { state }),
    )
}

pub(crate) async fn handle_prompt_submit_request(
    application: &ApplicationService,
    request_id: RequestId,
    request: RunSubmitRequest,
) -> DaemonEvent {
    let result = application
        .submit_prompt(PromptSubmission {
            session_id: request.session_id,
            client_submission_id: request.client_submission_id,
            content: request.content,
            budget: PromptBudget {
                wall_time_ms: request.budget.wall_time_ms,
                model_input_tokens: request.budget.model_input_tokens,
                model_output_tokens: request.budget.model_output_tokens,
                model_attempts: request.budget.model_attempts,
                capability_calls: request.budget.capability_calls,
            },
        })
        .await
        .map(|admission| {
            DaemonEventKind::RunAccepted(RunAcceptedEvent {
                session_id: admission.session_id,
                run_id: admission.run_id,
                turn_id: admission.turn_id,
                state: match admission.state {
                    PromptAdmissionState::Queued => ProtocolRunState::Queued,
                    PromptAdmissionState::Running => ProtocolRunState::Running,
                    PromptAdmissionState::Waiting => ProtocolRunState::Waiting,
                    PromptAdmissionState::Succeeded => ProtocolRunState::Succeeded,
                    PromptAdmissionState::Incomplete => ProtocolRunState::Incomplete,
                    PromptAdmissionState::Failed => ProtocolRunState::Failed,
                    PromptAdmissionState::Cancelled => ProtocolRunState::Cancelled,
                },
                replayed: admission.replayed,
            })
        });
    DaemonEvent::new(
        Some(request_id),
        result.unwrap_or_else(|error| DaemonEventKind::Error(protocol_error(error))),
    )
}

struct DaemonApplicationBackend {
    state: Weak<DaemonStateExecutor>,
}

impl DaemonApplicationBackend {
    fn with_state<T>(
        &self,
        context: ApplicationCallContext,
        operation: impl FnOnce(&mut DaemonState, &ApplicationCallContext) -> Result<T, ApplicationError>
        + Send
        + 'static,
    ) -> Result<T, ApplicationError>
    where
        T: Send + 'static,
    {
        let state = self.state.upgrade().ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::OutcomeUnknown,
                "daemon is shutting down",
            )
        })?;
        state
            .call(context, operation)
            .map_err(daemon_state_application_error)?
    }
}

impl ApplicationBackend for DaemonApplicationBackend {
    fn open_session(
        &self,
        context: ApplicationCallContext,
        request: SessionOpen,
    ) -> Result<SessionOpened, ApplicationError> {
        self.with_state(context, move |state, _| {
            state.application_open_session(request)
        })
    }

    fn snapshot_page(
        &self,
        context: ApplicationCallContext,
        session_id: &SessionId,
        page_cursor: Option<&str>,
    ) -> Result<agl_app::PresentationSnapshotPage, ApplicationError> {
        let session_id = session_id.clone();
        let page_cursor = page_cursor.map(str::to_owned);
        self.with_state(context, move |state, _| {
            state.application_snapshot_page(&session_id, page_cursor.as_deref())
        })
    }

    fn invoke(
        &self,
        context: ApplicationCallContext,
        request: agl_app::ApplicationActionRequest,
    ) -> Result<agl_app::ApplicationToolResult, ApplicationError> {
        let state = self.state.upgrade().ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::OutcomeUnknown,
                "daemon is shutting down",
            )
        })?;
        state.invoke_application(context, request)
    }

    fn submit_prompt(
        &self,
        context: ApplicationCallContext,
        request: PromptSubmission,
    ) -> Result<PromptAdmission, ApplicationError> {
        self.with_state(context, move |state, _| {
            state.application_submit_prompt(request)
        })
    }

    fn ensure_human_terminal(
        &self,
        context: ApplicationCallContext,
        request: HumanTerminalEnsure,
    ) -> Result<TerminalEnsured, ApplicationError> {
        self.with_state(context, move |state, _| {
            state.application_ensure_human_terminal(request)
        })
    }

    fn submit_human_terminal_command(
        &self,
        context: ApplicationCallContext,
        request: HumanTerminalCommandSubmit,
    ) -> Result<HumanTerminalCommandAdmission, ApplicationError> {
        self.with_state(context, move |state, _| {
            state.application_submit_human_terminal_command(request)
        })
    }

    fn suggestions(
        &self,
        context: ApplicationCallContext,
        request: SuggestionRequest,
    ) -> Result<SuggestionPage, ApplicationError> {
        self.with_state(context, move |state, _| {
            state.application_suggestions(request)
        })
    }
}

pub(crate) async fn handle_finite_request(
    application: &ApplicationService,
    request_id: RequestId,
    request: DaemonRequestKind,
    _operator_uid: u32,
) -> DaemonEvent {
    let result = match request {
        DaemonRequestKind::CommandCatalog(request) => command_catalog(application, request).await,
        DaemonRequestKind::CommandSuggestions(request) => {
            let request = (|| {
                Ok(SuggestionRequest {
                    session_id: request.session_id,
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
                    DaemonEventKind::ApplicationToolResult(ApplicationToolResultEvent { result })
                }),
            Err(error) => Err(error),
        },
        DaemonRequestKind::HumanTerminalEnsure(request) => match wire_convert(request) {
            Ok(request) => application
                .ensure_human_terminal(request)
                .await
                .and_then(wire_convert::<_, HumanTerminalEnsuredEvent>)
                .map(DaemonEventKind::HumanTerminalEnsured),
            Err(error) => Err(error),
        },
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

pub(crate) async fn handle_human_host_terminal_request(
    state: &SharedDaemonState,
    application: &ApplicationService,
    request_id: RequestId,
    request: HumanHostTerminalEnsureRequest,
    operator_uid: u32,
) -> DaemonEvent {
    let session_id = request.terminal.session_id.clone();
    let confirmed = request.confirm_host_authority;
    let result = match wire_convert(request.terminal) {
        Ok(request) => {
            match state
                .operator_ensure_human_host_terminal(request, operator_uid, confirmed)
                .await
            {
                Ok(ensured) => {
                    let result = ensured
                        .validate_for_session(&session_id)
                        .and_then(|_| wire_convert::<_, HumanTerminalEnsuredEvent>(ensured));
                    match result {
                        Ok(event) => application
                            .refresh(&session_id)
                            .await
                            .map(|_| DaemonEventKind::HumanTerminalEnsured(event)),
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
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
    event.descriptors.retain(|descriptor| {
        client_effect_is_admitted(descriptor.action_kind, &request.client_effects)
    });
    event
        .descriptors
        .sort_by(|left, right| left.id.cmp(&right.id));
    Ok(DaemonEventKind::CommandCatalog(event))
}

fn client_effect_is_admitted(
    action: agl_protocol::ApplicationActionKind,
    effects: &[agl_protocol::ClientEffectKind],
) -> bool {
    match action {
        agl_protocol::ApplicationActionKind::ClientHelp => {
            effects.contains(&agl_protocol::ClientEffectKind::Help)
        }
        agl_protocol::ApplicationActionKind::ClientDisconnect => {
            effects.contains(&agl_protocol::ClientEffectKind::Disconnect)
        }
        _ => true,
    }
}

fn application_action_result(
    result: agl_app::ApplicationToolResult,
) -> Result<agl_protocol::ApplicationToolResult, ApplicationError> {
    match result {
        agl_app::ApplicationToolResult::SessionOpened { opened } => {
            Ok(agl_protocol::ApplicationToolResult::SessionOpened {
                session_id: opened.session_id,
                resumed: opened.resumed,
            })
        }
        agl_app::ApplicationToolResult::SessionExited {
            session_id,
            cancelled_runs,
            terminated_terminals,
            terminated_executions: _,
        } => Ok(agl_protocol::ApplicationToolResult::SessionExited {
            session_id,
            cancelled_runs,
            terminated_terminals,
        }),
        other => wire_convert(other),
    }
}

pub(crate) fn presentation_subscribe(session_id: SessionId) -> PresentationSubscribe {
    PresentationSubscribe { session_id }
}

pub(crate) fn presentation_event(
    event: SessionPresentationEventEnvelope,
) -> Result<agl_protocol::SessionPresentationEventEnvelope, ApplicationError> {
    if matches!(
        &event.event,
        agl_app::SessionPresentationEvent::SnapshotReplaced { .. }
    ) {
        return Err(ApplicationError::new(
            ApplicationErrorCode::Internal,
            "snapshot replacement must use the bounded transfer adapter",
        ));
    }
    wire_convert(event)
}

pub(crate) fn presentation_snapshot(
    snapshot: SessionPresentationSnapshot,
    older_page_cursor: Option<String>,
) -> Result<agl_protocol::SessionPresentationSnapshot, ApplicationError> {
    let mut value = serde_json::to_value(snapshot).map_err(conversion_error)?;
    let object = value.as_object_mut().ok_or_else(|| {
        ApplicationError::new(
            ApplicationErrorCode::Internal,
            "application snapshot is not a JSON object",
        )
    })?;
    object.insert(
        "older_page_cursor".to_owned(),
        serde_json::to_value(older_page_cursor).map_err(conversion_error)?,
    );
    serde_json::from_value(value).map_err(conversion_error)
}

pub(crate) fn protocol_error(error: ApplicationError) -> ProtocolError {
    let (code, retryable) = match error.code {
        ApplicationErrorCode::InvalidArguments => (ProtocolErrorCode::InvalidArguments, false),
        ApplicationErrorCode::CommandUnavailable => (ProtocolErrorCode::CommandUnavailable, false),
        ApplicationErrorCode::SessionBusy => (ProtocolErrorCode::SessionBusy, true),
        ApplicationErrorCode::NotFound => (ProtocolErrorCode::NotFound, false),
        ApplicationErrorCode::NotAuthorized => (ProtocolErrorCode::NotAuthorized, false),
        ApplicationErrorCode::AuthorizationRequired => {
            (ProtocolErrorCode::AuthorizationRequired, false)
        }
        ApplicationErrorCode::ConfirmationRequired => {
            (ProtocolErrorCode::ConfirmationRequired, false)
        }
        ApplicationErrorCode::StaleContextRevision => {
            (ProtocolErrorCode::StaleContextRevision, false)
        }
        ApplicationErrorCode::TerminalOwnerMismatch => {
            (ProtocolErrorCode::TerminalOwnerMismatch, false)
        }
        ApplicationErrorCode::WriterLeaseBusy => (ProtocolErrorCode::WriterLeaseBusy, true),
        ApplicationErrorCode::ModelNotInstalled => (ProtocolErrorCode::ModelNotInstalled, false),
        ApplicationErrorCode::ModelContextTooSmall => {
            (ProtocolErrorCode::ModelContextTooSmall, false)
        }
        ApplicationErrorCode::SkillNotAdmitted => (ProtocolErrorCode::SkillNotAdmitted, false),
        ApplicationErrorCode::IncompleteOutputNotFound => {
            (ProtocolErrorCode::IncompleteOutputNotFound, false)
        }
        ApplicationErrorCode::ContinuationAlreadyClaimed => {
            (ProtocolErrorCode::ContinuationAlreadyClaimed, false)
        }
        ApplicationErrorCode::StaleContinuationContext => {
            (ProtocolErrorCode::StaleContinuationContext, false)
        }
        ApplicationErrorCode::InputBackpressure => (ProtocolErrorCode::InputBackpressure, true),
        ApplicationErrorCode::ActivityCapacityExceeded => {
            (ProtocolErrorCode::ActivityCapacityExceeded, false)
        }
        ApplicationErrorCode::ResyncRequired => (ProtocolErrorCode::ResyncRequired, true),
        ApplicationErrorCode::OutcomeUnknown => (ProtocolErrorCode::OutcomeUnknown, false),
        ApplicationErrorCode::Internal => (ProtocolErrorCode::Internal, false),
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

#[cfg(test)]
mod tests {
    use super::client_effect_is_admitted;
    use agl_protocol::{ApplicationActionKind, ClientEffectKind};

    #[test]
    fn client_catalog_entries_are_negotiated_without_synthetic_duplicates() {
        let effects = [ClientEffectKind::Help];
        assert!(client_effect_is_admitted(
            ApplicationActionKind::ClientHelp,
            &effects
        ));
        assert!(!client_effect_is_admitted(
            ApplicationActionKind::ClientDisconnect,
            &effects
        ));
        assert!(client_effect_is_admitted(
            ApplicationActionKind::SessionStatus,
            &[]
        ));
    }
}
