use std::sync::Arc;

use agl_capabilities::CapabilityId;
use agl_content::Content;
use agl_ids::{AttemptId, MessageId, RunId, SessionId, StepId, TurnId};
use agl_inference::{InferenceOutputEvent, InferenceOutputSink, OutputDelivery};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationDelivery {
    Delivered,
    Lagged,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelAttemptOutcome {
    Completed,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolActionOutcome {
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnPresentationOutcome {
    Answered,
    Stopped,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnPresentationEvent {
    ModelAttemptStarted {
        session_id: SessionId,
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: AttemptId,
        provisional_message_id: MessageId,
    },
    AssistantTextDelta {
        session_id: SessionId,
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: AttemptId,
        provisional_message_id: MessageId,
        sequence: u64,
        text: String,
    },
    AssistantMessageFinal {
        session_id: SessionId,
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: Option<AttemptId>,
        message_id: MessageId,
        content: Content,
    },
    ModelAttemptFinished {
        session_id: SessionId,
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: AttemptId,
        provisional_message_id: MessageId,
        outcome: ModelAttemptOutcome,
    },
    ToolActionStarted {
        session_id: SessionId,
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: Option<AttemptId>,
        provisional_message_id: Option<MessageId>,
        step_id: StepId,
        capability_id: CapabilityId,
    },
    ToolActionFinished {
        session_id: SessionId,
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: Option<AttemptId>,
        provisional_message_id: Option<MessageId>,
        step_id: StepId,
        capability_id: CapabilityId,
        outcome: ToolActionOutcome,
    },
    TurnFinished {
        session_id: SessionId,
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: Option<AttemptId>,
        provisional_message_id: Option<MessageId>,
        outcome: TurnPresentationOutcome,
    },
}

pub trait TurnPresentationSink: Send + Sync + 'static {
    fn try_publish(&self, event: TurnPresentationEvent) -> PresentationDelivery;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopTurnPresentationSink;

impl TurnPresentationSink for NoopTurnPresentationSink {
    fn try_publish(&self, _event: TurnPresentationEvent) -> PresentationDelivery {
        PresentationDelivery::Delivered
    }
}

pub(crate) struct InferencePresentationSink {
    sink: Arc<dyn TurnPresentationSink>,
    session_id: SessionId,
    run_id: RunId,
    turn_id: TurnId,
    attempt_id: AttemptId,
    provisional_message_id: MessageId,
    enabled: bool,
}

impl InferencePresentationSink {
    pub(crate) fn new(
        sink: Arc<dyn TurnPresentationSink>,
        session_id: SessionId,
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: AttemptId,
        provisional_message_id: MessageId,
        enabled: bool,
    ) -> Self {
        Self {
            sink,
            session_id,
            run_id,
            turn_id,
            attempt_id,
            provisional_message_id,
            enabled,
        }
    }
}

impl InferenceOutputSink for InferencePresentationSink {
    fn try_emit(&self, event: InferenceOutputEvent) -> OutputDelivery {
        if !self.enabled {
            return OutputDelivery::Closed;
        }
        let InferenceOutputEvent::TextDelta {
            attempt_id,
            sequence,
            text,
        } = event;
        if attempt_id != self.attempt_id {
            return OutputDelivery::Closed;
        }
        match self
            .sink
            .try_publish(TurnPresentationEvent::AssistantTextDelta {
                session_id: self.session_id.clone(),
                run_id: self.run_id.clone(),
                turn_id: self.turn_id.clone(),
                attempt_id,
                provisional_message_id: self.provisional_message_id.clone(),
                sequence,
                text,
            }) {
            PresentationDelivery::Delivered => OutputDelivery::Delivered,
            PresentationDelivery::Lagged => OutputDelivery::Lagged,
            PresentationDelivery::Closed => OutputDelivery::Closed,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct Sink {
        events: Mutex<Vec<TurnPresentationEvent>>,
        delivery: PresentationDelivery,
    }

    impl TurnPresentationSink for Sink {
        fn try_publish(&self, event: TurnPresentationEvent) -> PresentationDelivery {
            self.events.lock().unwrap().push(event);
            self.delivery
        }
    }

    #[test]
    fn inference_adapter_preserves_identity_and_maps_nonblocking_delivery() {
        for (presentation, output) in [
            (PresentationDelivery::Delivered, OutputDelivery::Delivered),
            (PresentationDelivery::Lagged, OutputDelivery::Lagged),
            (PresentationDelivery::Closed, OutputDelivery::Closed),
        ] {
            let sink = Arc::new(Sink {
                events: Mutex::new(Vec::new()),
                delivery: presentation,
            });
            let session_id = SessionId::generate();
            let run_id = RunId::generate();
            let turn_id = TurnId::generate();
            let attempt_id = AttemptId::generate();
            let provisional_message_id = MessageId::generate();
            let adapter = InferencePresentationSink::new(
                sink.clone(),
                session_id.clone(),
                run_id.clone(),
                turn_id.clone(),
                attempt_id.clone(),
                provisional_message_id.clone(),
                true,
            );

            assert_eq!(
                adapter.try_emit(InferenceOutputEvent::TextDelta {
                    attempt_id: attempt_id.clone(),
                    sequence: 7,
                    text: "привет".to_string(),
                }),
                output
            );
            assert_eq!(
                sink.events.lock().unwrap().as_slice(),
                [TurnPresentationEvent::AssistantTextDelta {
                    session_id,
                    run_id,
                    turn_id,
                    attempt_id,
                    provisional_message_id,
                    sequence: 7,
                    text: "привет".to_string(),
                }]
            );
        }
    }
}
