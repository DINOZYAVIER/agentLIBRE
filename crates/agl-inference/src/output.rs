use agl_ids::AttemptId;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum InferenceOutputEvent {
    TextDelta {
        attempt_id: AttemptId,
        sequence: u64,
        text: String,
    },
    Stage(InferenceStageEvent),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceStageEvent {
    pub attempt_id: AttemptId,
    pub stage_sequence: u64,
    pub stage: InferenceProductStage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<InferenceProgressUnit>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceProductStage {
    Queued,
    Admission,
    ModelLoad,
    ModelReuse,
    ContextReuse,
    ContextRebuild,
    Prefill,
    Generation,
    OutputParse,
    Completed,
    Incomplete,
    Cancelled,
    Failed,
    BackendLost,
}

impl InferenceProductStage {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Incomplete | Self::Cancelled | Self::Failed | Self::BackendLost
        )
    }

    pub fn is_worker_owned(self) -> bool {
        matches!(
            self,
            Self::ModelLoad
                | Self::ModelReuse
                | Self::ContextReuse
                | Self::ContextRebuild
                | Self::Prefill
                | Self::Generation
                | Self::OutputParse
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceProgressUnit {
    Tokens,
    Chunks,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InferenceStageAuthority {
    Host,
    Worker,
}

#[derive(Clone, Debug)]
pub struct InferenceStageValidator {
    attempt_id: AttemptId,
    authority: InferenceStageAuthority,
    last_sequence: u64,
    last_stage: Option<InferenceProductStage>,
    completed: Option<u64>,
    total: Option<u64>,
    unit: Option<InferenceProgressUnit>,
}

impl InferenceStageValidator {
    pub fn host(attempt_id: AttemptId) -> Self {
        Self::new(attempt_id, InferenceStageAuthority::Host)
    }

    pub fn worker(attempt_id: AttemptId) -> Self {
        Self::new(attempt_id, InferenceStageAuthority::Worker)
    }

    fn new(attempt_id: AttemptId, authority: InferenceStageAuthority) -> Self {
        Self {
            attempt_id,
            authority,
            last_sequence: 0,
            last_stage: None,
            completed: None,
            total: None,
            unit: None,
        }
    }

    pub fn accept(&mut self, event: &InferenceStageEvent) -> Result<(), InferenceStageError> {
        if event.attempt_id != self.attempt_id {
            return Err(InferenceStageError::WrongAttempt);
        }
        let expected = self
            .last_sequence
            .checked_add(1)
            .ok_or(InferenceStageError::SequenceExhausted)?;
        if event.stage_sequence != expected {
            return Err(InferenceStageError::SequenceGap {
                expected,
                actual: event.stage_sequence,
            });
        }
        if self.authority == InferenceStageAuthority::Worker && !event.stage.is_worker_owned() {
            return Err(InferenceStageError::StageAuthorityViolation { stage: event.stage });
        }
        validate_counter_shape(event)?;

        match self.last_stage {
            None => self.validate_first_stage(event.stage)?,
            Some(previous) if previous.is_terminal() => {
                return Err(InferenceStageError::AfterTerminal { previous });
            }
            Some(previous) if previous == event.stage => {
                self.validate_repeated_progress(event)?;
                self.last_sequence = event.stage_sequence;
                self.completed = event.completed;
                self.total = event.total;
                self.unit = event.unit;
                return Ok(());
            }
            Some(previous) => validate_transition(self.authority, previous, event.stage)?,
        }

        self.last_sequence = event.stage_sequence;
        self.last_stage = Some(event.stage);
        self.completed = event.completed;
        self.total = event.total;
        self.unit = event.unit;
        Ok(())
    }

    pub fn last_stage(&self) -> Option<InferenceProductStage> {
        self.last_stage
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    fn validate_first_stage(
        &self,
        stage: InferenceProductStage,
    ) -> Result<(), InferenceStageError> {
        let allowed = match self.authority {
            InferenceStageAuthority::Host => stage == InferenceProductStage::Queued,
            InferenceStageAuthority::Worker => {
                matches!(
                    stage,
                    InferenceProductStage::ModelLoad | InferenceProductStage::ModelReuse
                )
            }
        };
        if allowed {
            Ok(())
        } else {
            Err(InferenceStageError::InvalidInitialStage { stage })
        }
    }

    fn validate_repeated_progress(
        &self,
        event: &InferenceStageEvent,
    ) -> Result<(), InferenceStageError> {
        if event.stage.is_terminal() {
            return Err(InferenceStageError::DuplicateTerminal { stage: event.stage });
        }
        if let (Some(previous), Some(current)) = (self.completed, event.completed)
            && current < previous
        {
            return Err(InferenceStageError::ProgressRegressed { previous, current });
        }
        if self.completed.is_some() && event.completed.is_none() {
            return Err(InferenceStageError::ProgressDisappeared);
        }
        if let Some(previous) = self.total
            && event.total != Some(previous)
        {
            return Err(InferenceStageError::TotalChanged {
                previous,
                current: event.total,
            });
        }
        if let Some(previous) = self.unit
            && event.unit != Some(previous)
        {
            return Err(InferenceStageError::UnitChanged {
                previous,
                current: event.unit,
            });
        }
        Ok(())
    }
}

fn validate_counter_shape(event: &InferenceStageEvent) -> Result<(), InferenceStageError> {
    if (event.completed.is_some() || event.total.is_some()) && event.unit.is_none() {
        return Err(InferenceStageError::CounterWithoutUnit);
    }
    if let (Some(completed), Some(total)) = (event.completed, event.total)
        && completed > total
    {
        return Err(InferenceStageError::ProgressExceedsTotal { completed, total });
    }
    Ok(())
}

fn validate_transition(
    authority: InferenceStageAuthority,
    previous: InferenceProductStage,
    next: InferenceProductStage,
) -> Result<(), InferenceStageError> {
    use InferenceProductStage as Stage;

    if matches!(next, Stage::Cancelled | Stage::Failed)
        || (next == Stage::BackendLost && previous.is_worker_owned())
    {
        return Ok(());
    }
    let allowed = match (previous, next) {
        (Stage::Queued, Stage::Admission) => true,
        (Stage::Admission, Stage::ModelLoad | Stage::ModelReuse) => true,
        (Stage::ModelLoad | Stage::ModelReuse, Stage::ContextReuse | Stage::ContextRebuild) => true,
        (Stage::ContextReuse, Stage::Prefill | Stage::Generation) => true,
        (Stage::ContextRebuild, Stage::Prefill) => true,
        (Stage::Prefill, Stage::Generation) => true,
        (Stage::Generation, Stage::OutputParse) => true,
        (Stage::OutputParse, Stage::Completed | Stage::Incomplete) => {
            authority == InferenceStageAuthority::Host
        }
        _ => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(InferenceStageError::InvalidTransition { previous, next })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InferenceStageError {
    WrongAttempt,
    SequenceExhausted,
    SequenceGap {
        expected: u64,
        actual: u64,
    },
    StageAuthorityViolation {
        stage: InferenceProductStage,
    },
    InvalidInitialStage {
        stage: InferenceProductStage,
    },
    InvalidTransition {
        previous: InferenceProductStage,
        next: InferenceProductStage,
    },
    AfterTerminal {
        previous: InferenceProductStage,
    },
    DuplicateTerminal {
        stage: InferenceProductStage,
    },
    CounterWithoutUnit,
    ProgressExceedsTotal {
        completed: u64,
        total: u64,
    },
    ProgressRegressed {
        previous: u64,
        current: u64,
    },
    ProgressDisappeared,
    TotalChanged {
        previous: u64,
        current: Option<u64>,
    },
    UnitChanged {
        previous: InferenceProgressUnit,
        current: Option<InferenceProgressUnit>,
    },
}

impl InferenceStageError {
    pub fn code(&self) -> &'static str {
        "inference_stage_protocol_violation"
    }
}

impl std::fmt::Display for InferenceStageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "invalid inference stage event: {self:?}")
    }
}

impl std::error::Error for InferenceStageError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputDelivery {
    Delivered,
    Lagged,
    Closed,
}

pub trait InferenceOutputSink: Send + Sync + 'static {
    fn try_emit(&self, event: InferenceOutputEvent) -> OutputDelivery;
}

/// Host-owned merger for the public inference stage stream.
///
/// The worker owns a private, attempt-local stage sequence beginning at one.
/// The model manager owns `Queued`, `Admission`, and the terminal outcome. This
/// adapter validates the private stream, assigns one contiguous public
/// sequence, and shields inference from presentation lag or disconnects.
pub(crate) struct PublicInferenceOutputBroker {
    attempt_id: AttemptId,
    downstream: Arc<dyn InferenceOutputSink>,
    state: Mutex<PublicInferenceOutputState>,
}

struct PublicInferenceOutputState {
    private_validator: InferenceStageValidator,
    public_validator: InferenceStageValidator,
    delivery_suspended: bool,
    last_delta_sequence: u64,
}

impl PublicInferenceOutputBroker {
    pub(crate) fn new(attempt_id: AttemptId, downstream: Arc<dyn InferenceOutputSink>) -> Self {
        Self {
            state: Mutex::new(PublicInferenceOutputState {
                private_validator: InferenceStageValidator::worker(attempt_id.clone()),
                public_validator: InferenceStageValidator::host(attempt_id.clone()),
                delivery_suspended: false,
                last_delta_sequence: 0,
            }),
            attempt_id,
            downstream,
        }
    }

    pub(crate) fn emit_host_stage(&self, stage: InferenceProductStage) {
        debug_assert!(!stage.is_worker_owned());
        let mut state = self.lock_state();
        let Some(event) = next_public_event(&mut state, &self.attempt_id, stage, None, None, None)
        else {
            return;
        };
        self.deliver(&mut state, InferenceOutputEvent::Stage(event));
    }

    pub(crate) fn emit_engine_stage(&self, stage: InferenceProductStage) {
        debug_assert!(stage.is_worker_owned());
        let mut state = self.lock_state();
        let private = InferenceStageEvent {
            attempt_id: self.attempt_id.clone(),
            stage_sequence: state.private_validator.last_sequence() + 1,
            stage,
            completed: None,
            total: None,
            unit: None,
        };
        if state.private_validator.accept(&private).is_err() {
            return;
        }
        let Some(public) = next_public_event(&mut state, &self.attempt_id, stage, None, None, None)
        else {
            return;
        };
        self.deliver(&mut state, InferenceOutputEvent::Stage(public));
    }

    pub(crate) fn last_public_stage(&self) -> Option<InferenceProductStage> {
        self.lock_state().public_validator.last_stage()
    }

    pub(crate) fn emit_text_delta(&self, sequence: u64, text: String) -> OutputDelivery {
        self.try_emit(InferenceOutputEvent::TextDelta {
            attempt_id: self.attempt_id.clone(),
            sequence,
            text,
        })
    }

    #[cfg(test)]
    pub(crate) fn delivery_suspended(&self) -> bool {
        self.lock_state().delivery_suspended
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, PublicInferenceOutputState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn deliver(&self, state: &mut PublicInferenceOutputState, event: InferenceOutputEvent) {
        if state.delivery_suspended {
            return;
        }
        if self.downstream.try_emit(event) != OutputDelivery::Delivered {
            state.delivery_suspended = true;
        }
    }
}

impl InferenceOutputSink for PublicInferenceOutputBroker {
    fn try_emit(&self, event: InferenceOutputEvent) -> OutputDelivery {
        let mut state = self.lock_state();
        match event {
            InferenceOutputEvent::TextDelta {
                attempt_id,
                sequence,
                text,
            } => {
                if attempt_id != self.attempt_id {
                    return OutputDelivery::Closed;
                }
                let Some(expected) = state.last_delta_sequence.checked_add(1) else {
                    return OutputDelivery::Closed;
                };
                if sequence != expected
                    || text.is_empty()
                    || state.public_validator.last_stage().is_none_or(|stage| {
                        stage != InferenceProductStage::Generation || stage.is_terminal()
                    })
                {
                    return OutputDelivery::Closed;
                }
                state.last_delta_sequence = sequence;
                self.deliver(
                    &mut state,
                    InferenceOutputEvent::TextDelta {
                        attempt_id,
                        sequence,
                        text,
                    },
                );
            }
            InferenceOutputEvent::Stage(private) => {
                if state.private_validator.accept(&private).is_err() {
                    return OutputDelivery::Closed;
                }
                let Some(public) = next_public_event(
                    &mut state,
                    &self.attempt_id,
                    private.stage,
                    private.completed,
                    private.total,
                    private.unit,
                ) else {
                    return OutputDelivery::Closed;
                };
                self.deliver(&mut state, InferenceOutputEvent::Stage(public));
            }
        }

        // Lag and closure belong to the presentation subscriber. The native
        // attempt remains authoritative and completes normally; its final
        // response reconciles the durable projection.
        OutputDelivery::Delivered
    }
}

fn next_public_event(
    state: &mut PublicInferenceOutputState,
    attempt_id: &AttemptId,
    stage: InferenceProductStage,
    completed: Option<u64>,
    total: Option<u64>,
    unit: Option<InferenceProgressUnit>,
) -> Option<InferenceStageEvent> {
    let stage_sequence = state.public_validator.last_sequence().checked_add(1)?;
    let event = InferenceStageEvent {
        attempt_id: attempt_id.clone(),
        stage_sequence,
        stage,
        completed,
        total,
        unit,
    };
    state.public_validator.accept(&event).ok()?;
    Some(event)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopInferenceOutputSink;

impl InferenceOutputSink for NoopInferenceOutputSink {
    fn try_emit(&self, _event: InferenceOutputEvent) -> OutputDelivery {
        OutputDelivery::Delivered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingSink {
        events: Mutex<Vec<InferenceOutputEvent>>,
        delivery: OutputDelivery,
    }

    impl InferenceOutputSink for RecordingSink {
        fn try_emit(&self, event: InferenceOutputEvent) -> OutputDelivery {
            self.events.lock().unwrap().push(event);
            self.delivery
        }
    }

    fn event(
        attempt_id: &AttemptId,
        sequence: u64,
        stage: InferenceProductStage,
        completed: Option<u64>,
        total: Option<u64>,
    ) -> InferenceStageEvent {
        InferenceStageEvent {
            attempt_id: attempt_id.clone(),
            stage_sequence: sequence,
            stage,
            completed,
            total,
            unit: completed.or(total).map(|_| InferenceProgressUnit::Tokens),
        }
    }

    #[test]
    fn host_stage_machine_accepts_the_complete_selected_path() {
        let attempt_id = AttemptId::generate();
        let mut validator = InferenceStageValidator::host(attempt_id.clone());
        let stages = [
            InferenceProductStage::Queued,
            InferenceProductStage::Admission,
            InferenceProductStage::ModelReuse,
            InferenceProductStage::ContextRebuild,
            InferenceProductStage::Prefill,
            InferenceProductStage::Generation,
            InferenceProductStage::OutputParse,
            InferenceProductStage::Completed,
        ];
        for (index, stage) in stages.into_iter().enumerate() {
            validator
                .accept(&event(
                    &attempt_id,
                    u64::try_from(index + 1).unwrap(),
                    stage,
                    None,
                    None,
                ))
                .unwrap();
        }
        assert_eq!(
            validator.last_stage(),
            Some(InferenceProductStage::Completed)
        );
    }

    #[test]
    fn host_broker_merges_private_stages_into_one_public_sequence() {
        let attempt_id = AttemptId::generate();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
            delivery: OutputDelivery::Delivered,
        });
        let broker = PublicInferenceOutputBroker::new(attempt_id.clone(), sink.clone());

        broker.emit_host_stage(InferenceProductStage::Queued);
        broker.emit_host_stage(InferenceProductStage::Admission);
        for (sequence, stage) in [
            InferenceProductStage::ModelReuse,
            InferenceProductStage::ContextReuse,
            InferenceProductStage::Generation,
            InferenceProductStage::OutputParse,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                broker.try_emit(InferenceOutputEvent::Stage(event(
                    &attempt_id,
                    u64::try_from(sequence + 1).unwrap(),
                    stage,
                    None,
                    None,
                ))),
                OutputDelivery::Delivered
            );
        }
        broker.emit_host_stage(InferenceProductStage::Completed);

        let events = sink.events.lock().unwrap();
        let stages = events
            .iter()
            .filter_map(|event| match event {
                InferenceOutputEvent::Stage(stage) => Some((stage.stage_sequence, stage.stage)),
                InferenceOutputEvent::TextDelta { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            stages,
            vec![
                (1, InferenceProductStage::Queued),
                (2, InferenceProductStage::Admission),
                (3, InferenceProductStage::ModelReuse),
                (4, InferenceProductStage::ContextReuse),
                (5, InferenceProductStage::Generation),
                (6, InferenceProductStage::OutputParse),
                (7, InferenceProductStage::Completed),
            ]
        );
    }

    #[test]
    fn presentation_lag_suspends_delivery_without_failing_inference() {
        let attempt_id = AttemptId::generate();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
            delivery: OutputDelivery::Lagged,
        });
        let broker = PublicInferenceOutputBroker::new(attempt_id.clone(), sink.clone());

        broker.emit_host_stage(InferenceProductStage::Queued);
        broker.emit_host_stage(InferenceProductStage::Admission);
        assert_eq!(
            broker.try_emit(InferenceOutputEvent::Stage(event(
                &attempt_id,
                1,
                InferenceProductStage::ModelReuse,
                None,
                None,
            ))),
            OutputDelivery::Delivered
        );

        assert!(broker.delivery_suspended());
        assert_eq!(sink.events.lock().unwrap().len(), 1);
    }

    #[test]
    fn malformed_private_stage_stream_is_rejected_at_the_host_boundary() {
        let attempt_id = AttemptId::generate();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
            delivery: OutputDelivery::Delivered,
        });
        let broker = PublicInferenceOutputBroker::new(attempt_id.clone(), sink);
        broker.emit_host_stage(InferenceProductStage::Queued);
        broker.emit_host_stage(InferenceProductStage::Admission);

        assert_eq!(
            broker.try_emit(InferenceOutputEvent::Stage(event(
                &attempt_id,
                2,
                InferenceProductStage::ModelReuse,
                None,
                None,
            ))),
            OutputDelivery::Closed
        );
    }

    // MIW-PROTO-001 and MIW-ENG-005.
    #[test]
    fn text_deltas_require_exact_attempt_order_and_generation_stage() {
        let attempt_id = AttemptId::generate();
        let foreign_attempt = AttemptId::generate();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
            delivery: OutputDelivery::Delivered,
        });
        let broker = PublicInferenceOutputBroker::new(attempt_id.clone(), sink.clone());
        assert_eq!(
            broker.emit_text_delta(1, "early".to_owned()),
            OutputDelivery::Closed
        );
        broker.emit_host_stage(InferenceProductStage::Queued);
        broker.emit_host_stage(InferenceProductStage::Admission);
        broker.emit_engine_stage(InferenceProductStage::ModelReuse);
        broker.emit_engine_stage(InferenceProductStage::ContextReuse);
        broker.emit_engine_stage(InferenceProductStage::Generation);
        assert_eq!(
            broker.emit_text_delta(1, "one".to_owned()),
            OutputDelivery::Delivered
        );
        assert_eq!(
            broker.emit_text_delta(1, "duplicate".to_owned()),
            OutputDelivery::Closed
        );
        assert_eq!(
            broker.emit_text_delta(3, "gap".to_owned()),
            OutputDelivery::Closed
        );
        assert_eq!(
            broker.try_emit(InferenceOutputEvent::TextDelta {
                attempt_id: foreign_attempt,
                sequence: 2,
                text: "foreign".to_owned(),
            }),
            OutputDelivery::Closed
        );
        assert_eq!(
            broker.emit_text_delta(2, "two".to_owned()),
            OutputDelivery::Delivered
        );
    }

    #[test]
    fn worker_cannot_author_terminal_or_host_stages() {
        let attempt_id = AttemptId::generate();
        for stage in [
            InferenceProductStage::Queued,
            InferenceProductStage::Admission,
            InferenceProductStage::Completed,
            InferenceProductStage::BackendLost,
        ] {
            let mut validator = InferenceStageValidator::worker(attempt_id.clone());
            assert!(matches!(
                validator.accept(&event(&attempt_id, 1, stage, None, None)),
                Err(InferenceStageError::StageAuthorityViolation { .. })
                    | Err(InferenceStageError::InvalidInitialStage { .. })
            ));
        }
    }

    #[test]
    fn repeated_progress_is_monotonic_and_bounded() {
        let attempt_id = AttemptId::generate();
        let mut validator = InferenceStageValidator::worker(attempt_id.clone());
        validator
            .accept(&event(
                &attempt_id,
                1,
                InferenceProductStage::ModelReuse,
                None,
                None,
            ))
            .unwrap();
        validator
            .accept(&event(
                &attempt_id,
                2,
                InferenceProductStage::ContextReuse,
                None,
                None,
            ))
            .unwrap();
        validator
            .accept(&event(
                &attempt_id,
                3,
                InferenceProductStage::Generation,
                Some(2),
                Some(10),
            ))
            .unwrap();
        validator
            .accept(&event(
                &attempt_id,
                4,
                InferenceProductStage::Generation,
                Some(8),
                Some(10),
            ))
            .unwrap();
        assert!(matches!(
            validator.accept(&event(
                &attempt_id,
                5,
                InferenceProductStage::Generation,
                Some(7),
                Some(10),
            )),
            Err(InferenceStageError::ProgressRegressed { .. })
        ));
    }

    #[test]
    fn stage_gap_backwards_transition_and_post_terminal_event_fail() {
        let attempt_id = AttemptId::generate();
        let mut gap = InferenceStageValidator::host(attempt_id.clone());
        assert!(matches!(
            gap.accept(&event(
                &attempt_id,
                2,
                InferenceProductStage::Queued,
                None,
                None,
            )),
            Err(InferenceStageError::SequenceGap { .. })
        ));

        let mut backwards = InferenceStageValidator::worker(attempt_id.clone());
        backwards
            .accept(&event(
                &attempt_id,
                1,
                InferenceProductStage::ModelLoad,
                None,
                None,
            ))
            .unwrap();
        assert!(matches!(
            backwards.accept(&event(
                &attempt_id,
                2,
                InferenceProductStage::ModelReuse,
                None,
                None,
            )),
            Err(InferenceStageError::InvalidTransition { .. })
        ));

        let mut terminal = InferenceStageValidator::host(attempt_id.clone());
        for (sequence, stage) in [
            InferenceProductStage::Queued,
            InferenceProductStage::Cancelled,
        ]
        .into_iter()
        .enumerate()
        {
            terminal
                .accept(&event(
                    &attempt_id,
                    u64::try_from(sequence + 1).unwrap(),
                    stage,
                    None,
                    None,
                ))
                .unwrap();
        }
        assert!(matches!(
            terminal.accept(&event(
                &attempt_id,
                3,
                InferenceProductStage::Cancelled,
                None,
                None,
            )),
            Err(InferenceStageError::AfterTerminal { .. })
        ));
    }
}
