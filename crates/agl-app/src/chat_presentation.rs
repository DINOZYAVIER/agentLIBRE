use std::sync::{Arc, OnceLock};

use agl_chat::{
    ModelAttemptOutcome, PresentationDelivery, ToolActionOutcome, TurnPresentationEvent,
    TurnPresentationOutcome, TurnPresentationSink,
};

use crate::{
    ActionItemState, ApplicationError, ApplicationErrorCode, ApplicationService,
    AssistantItemState, SessionPresentationEvent, SessionPresentationItem, Severity,
};

const MAX_ACTION_SUMMARY_BYTES: usize = 1024;

#[derive(Clone, Default)]
pub struct TurnPresentationProxy {
    target: Arc<OnceLock<ApplicationService>>,
}

impl TurnPresentationProxy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn connect(&self, service: ApplicationService) -> Result<(), ApplicationError> {
        self.target.set(service).map_err(|_| {
            ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "turn presentation proxy is already connected",
            )
        })
    }

    pub fn is_connected(&self) -> bool {
        self.target.get().is_some()
    }
}

impl TurnPresentationSink for TurnPresentationProxy {
    fn try_publish(&self, event: TurnPresentationEvent) -> PresentationDelivery {
        self.target
            .get()
            .map_or(PresentationDelivery::Closed, |service| {
                service.try_publish(event)
            })
    }
}

impl TurnPresentationSink for ApplicationService {
    fn try_publish(&self, event: TurnPresentationEvent) -> PresentationDelivery {
        let (session_id, events) = application_events(event);
        match self.try_publish_batch_nonblocking(&session_id, events) {
            Ok(true) => PresentationDelivery::Delivered,
            Ok(false) => PresentationDelivery::Closed,
            Err(error)
                if matches!(
                    error.code,
                    ApplicationErrorCode::InputBackpressure | ApplicationErrorCode::ResyncRequired
                ) =>
            {
                PresentationDelivery::Lagged
            }
            Err(_) => PresentationDelivery::Closed,
        }
    }
}

fn application_events(
    event: TurnPresentationEvent,
) -> (agl_ids::SessionId, Vec<SessionPresentationEvent>) {
    match event {
        TurnPresentationEvent::ModelAttemptStarted {
            session_id,
            run_id,
            attempt_id,
            provisional_message_id,
            ..
        } => (
            session_id,
            vec![
                SessionPresentationEvent::PromptActivated { run_id },
                SessionPresentationEvent::ItemRemoved {
                    item_key: provisional_message_id.to_string(),
                },
                SessionPresentationEvent::Notice {
                    severity: Severity::Info,
                    code: "model_attempt_started".to_owned(),
                    message: format!(
                        "model attempt {attempt_id} started for assistant item {provisional_message_id}"
                    ),
                },
            ],
        ),
        TurnPresentationEvent::AssistantTextDelta {
            session_id,
            run_id,
            turn_id,
            provisional_message_id,
            sequence,
            text,
            ..
        } => (
            session_id,
            vec![SessionPresentationEvent::AssistantTextDelta {
                run_id,
                turn_id,
                provisional_message_id,
                sequence,
                text,
            }],
        ),
        TurnPresentationEvent::AssistantMessageFinal {
            session_id,
            message_id,
            content,
            ..
        } => (
            session_id,
            vec![SessionPresentationEvent::ItemUpsert {
                item: SessionPresentationItem::AssistantMessage {
                    message_id,
                    content,
                    state: AssistantItemState::Final,
                },
            }],
        ),
        TurnPresentationEvent::ModelAttemptFinished {
            session_id,
            attempt_id,
            outcome,
            ..
        } => {
            let (severity, outcome) = match outcome {
                ModelAttemptOutcome::Completed => (Severity::Info, "completed"),
                ModelAttemptOutcome::Failed => (Severity::Warning, "failed"),
            };
            (
                session_id,
                vec![SessionPresentationEvent::Notice {
                    severity,
                    code: "model_attempt_finished".to_owned(),
                    message: format!("model attempt {attempt_id} {outcome}"),
                }],
            )
        }
        TurnPresentationEvent::ToolActionStarted {
            session_id,
            run_id,
            step_id,
            capability_id,
            ..
        } => (
            session_id,
            vec![SessionPresentationEvent::ItemUpsert {
                item: SessionPresentationItem::AgentAction {
                    run_id,
                    step_id,
                    capability_id: Some(capability_id.to_string()),
                    summary: bounded_summary(capability_id.as_str()),
                    state: ActionItemState::Running,
                },
            }],
        ),
        TurnPresentationEvent::ToolActionFinished {
            session_id,
            run_id,
            step_id,
            capability_id,
            outcome,
            ..
        } => (
            session_id,
            vec![SessionPresentationEvent::ItemUpsert {
                item: SessionPresentationItem::AgentAction {
                    run_id,
                    step_id,
                    capability_id: Some(capability_id.to_string()),
                    summary: bounded_summary(capability_id.as_str()),
                    state: match outcome {
                        ToolActionOutcome::Succeeded => ActionItemState::Succeeded,
                        ToolActionOutcome::Failed => ActionItemState::Failed,
                    },
                },
            }],
        ),
        TurnPresentationEvent::TurnFinished {
            session_id,
            run_id,
            outcome,
            ..
        } => (
            session_id,
            vec![SessionPresentationEvent::PromptFinished {
                run_id,
                state: match outcome {
                    TurnPresentationOutcome::Answered => "answered",
                    TurnPresentationOutcome::Stopped => "stopped",
                    TurnPresentationOutcome::Failed => "failed",
                    TurnPresentationOutcome::Cancelled => "cancelled",
                }
                .to_owned(),
            }],
        ),
    }
}

fn bounded_summary(value: &str) -> String {
    if value.len() <= MAX_ACTION_SUMMARY_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_ACTION_SUMMARY_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}
