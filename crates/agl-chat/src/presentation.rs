use std::sync::Arc;

use agl_content::Content;
use agl_ids::{AttemptId, MessageId, RunId, SessionId, StepId, TurnId};
use agl_inference::{
    InferenceOutputEvent, InferenceOutputSink, InferenceStageEvent, OutputDelivery,
};
use agl_kernel::IncompleteOutputReason;
use agl_kernel::ToolId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildRunPresentation {
    pub parent_run_id: RunId,
    pub spawned_by_step_id: StepId,
    pub subagent_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolPresentationCompleteness {
    Complete,
    Truncated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolPresentationExecutionProfile {
    Workspace,
    Host,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolPresentationDetail {
    FilesystemList {
        path: String,
        entries: u32,
        completeness: ToolPresentationCompleteness,
    },
    FilesystemRead {
        path: String,
        bytes: u64,
    },
    RepositorySearch {
        scope: String,
        matches: u32,
        complete: bool,
    },
    ProcessExecution {
        profile: ToolPresentationExecutionProfile,
        exit_status: Option<i32>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyPresentationOutcome {
    Allowed,
    Denied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationDelivery {
    Delivered,
    Lagged,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelAttemptOutcome {
    Completed,
    Incomplete,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolActionOutcome {
    Succeeded,
    Waiting,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnPresentationOutcome {
    Answered,
    IncompleteOutput,
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
        child_run: Option<ChildRunPresentation>,
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
    InferenceStage {
        session_id: SessionId,
        run_id: RunId,
        turn_id: TurnId,
        event: InferenceStageEvent,
    },
    AssistantMessageFinal {
        session_id: SessionId,
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: Option<AttemptId>,
        message_id: MessageId,
        content: Content,
    },
    AssistantMessageIncomplete {
        session_id: SessionId,
        run_id: RunId,
        turn_id: TurnId,
        message_id: MessageId,
        content: Content,
        source_attempt_id: AttemptId,
        reason: IncompleteOutputReason,
        continuation_index: u16,
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
        tool_id: ToolId,
    },
    ToolActionFinished {
        session_id: SessionId,
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: Option<AttemptId>,
        provisional_message_id: Option<MessageId>,
        step_id: StepId,
        tool_id: ToolId,
        outcome: ToolActionOutcome,
        detail: Option<ToolPresentationDetail>,
    },
    PolicyCheck {
        session_id: SessionId,
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: Option<AttemptId>,
        step_id: StepId,
        tool_id: ToolId,
        outcome: PolicyPresentationOutcome,
    },
    TurnFinished {
        session_id: SessionId,
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: Option<AttemptId>,
        provisional_message_id: Option<MessageId>,
        outcome: TurnPresentationOutcome,
        child_run: Option<ChildRunPresentation>,
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
    publish_text: bool,
}

pub(crate) struct InferencePresentationTarget {
    pub(crate) session_id: SessionId,
    pub(crate) run_id: RunId,
    pub(crate) turn_id: TurnId,
    pub(crate) attempt_id: AttemptId,
    pub(crate) provisional_message_id: MessageId,
}

impl InferencePresentationSink {
    pub(crate) fn new(
        sink: Arc<dyn TurnPresentationSink>,
        target: InferencePresentationTarget,
        enabled: bool,
        publish_text: bool,
    ) -> Self {
        Self {
            sink,
            session_id: target.session_id,
            run_id: target.run_id,
            turn_id: target.turn_id,
            attempt_id: target.attempt_id,
            provisional_message_id: target.provisional_message_id,
            enabled,
            publish_text,
        }
    }
}

impl InferenceOutputSink for InferencePresentationSink {
    fn try_emit(&self, event: InferenceOutputEvent) -> OutputDelivery {
        if !self.enabled {
            return OutputDelivery::Closed;
        }
        if matches!(&event, InferenceOutputEvent::TextDelta { .. }) && !self.publish_text {
            return OutputDelivery::Delivered;
        }
        let event = match event {
            InferenceOutputEvent::TextDelta {
                attempt_id,
                sequence,
                text,
            } if attempt_id == self.attempt_id => TurnPresentationEvent::AssistantTextDelta {
                session_id: self.session_id.clone(),
                run_id: self.run_id.clone(),
                turn_id: self.turn_id.clone(),
                attempt_id,
                provisional_message_id: self.provisional_message_id.clone(),
                sequence,
                text,
            },
            InferenceOutputEvent::Stage(event) if event.attempt_id == self.attempt_id => {
                TurnPresentationEvent::InferenceStage {
                    session_id: self.session_id.clone(),
                    run_id: self.run_id.clone(),
                    turn_id: self.turn_id.clone(),
                    event,
                }
            }
            InferenceOutputEvent::TextDelta { .. } | InferenceOutputEvent::Stage(_) => {
                return OutputDelivery::Closed;
            }
        };
        match self.sink.try_publish(event) {
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
                InferencePresentationTarget {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                    attempt_id: attempt_id.clone(),
                    provisional_message_id: provisional_message_id.clone(),
                },
                true,
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

    #[test]
    fn inference_adapter_suppresses_private_child_text_but_keeps_stages_open() {
        let sink = Arc::new(Sink {
            events: Mutex::new(Vec::new()),
            delivery: PresentationDelivery::Delivered,
        });
        let adapter = InferencePresentationSink::new(
            sink.clone(),
            InferencePresentationTarget {
                session_id: SessionId::generate(),
                run_id: RunId::generate(),
                turn_id: TurnId::generate(),
                attempt_id: AttemptId::generate(),
                provisional_message_id: MessageId::generate(),
            },
            true,
            false,
        );
        let attempt_id = adapter.attempt_id.clone();

        assert_eq!(
            adapter.try_emit(InferenceOutputEvent::TextDelta {
                attempt_id: attempt_id.clone(),
                sequence: 1,
                text: "PRIVATE_CHILD_SENTINEL".to_owned(),
            }),
            OutputDelivery::Delivered
        );
        assert!(sink.events.lock().unwrap().is_empty());

        let stage = InferenceStageEvent {
            attempt_id,
            stage_sequence: 1,
            stage: agl_inference::InferenceProductStage::Queued,
            completed: None,
            total: None,
            unit: None,
        };
        assert_eq!(
            adapter.try_emit(InferenceOutputEvent::Stage(stage.clone())),
            OutputDelivery::Delivered
        );
        assert!(matches!(
            sink.events.lock().unwrap().as_slice(),
            [TurnPresentationEvent::InferenceStage { event, .. }] if event == &stage
        ));
    }
}
