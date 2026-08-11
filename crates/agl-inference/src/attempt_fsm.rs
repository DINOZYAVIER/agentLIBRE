use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use agl_ids::{AttemptId, RunId, TurnId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceAttemptOutcome {
    Succeeded,
    IncompleteOutput,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceRejectionStage {
    Plan,
    Content,
    Descriptor,
    Lease,
    Admission,
    Queue,
    Dispatch,
    Engine,
    Evidence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceAttemptFailure {
    pub code: String,
    pub stage: InferenceRejectionStage,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceAttemptCancellation {
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferencePlanEvidence {
    pub plan_digest: String,
    pub package_refs: Vec<String>,
    pub profile_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceContentEvidence {
    pub content_digest: String,
    pub resolved_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceAdmissionEvidence {
    pub reservation_id: String,
    pub resource_components: Vec<(String, u64)>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceDispatchEvidence {
    pub descriptor_set_id: String,
    pub engine_generation: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceRuntimeEvidence {
    pub allocation_receipt_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceAttemptPhase {
    Initialized,
    Started,
    RequestRecorded,
    Planned,
    ContentReady,
    Admitted,
    DispatchRecorded,
    RuntimeGenerating,
    RuntimeLogRecorded,
    ResponseRecorded,
    FailureRecorded,
    CancellationRecorded,
    Succeeded,
    IncompleteOutput,
    Failed,
    Cancelled,
}

impl InferenceAttemptPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initialized => "initialized",
            Self::Started => "started",
            Self::RequestRecorded => "request_recorded",
            Self::Planned => "planned",
            Self::ContentReady => "content_ready",
            Self::Admitted => "admitted",
            Self::DispatchRecorded => "dispatch_recorded",
            Self::RuntimeGenerating => "runtime_generating",
            Self::RuntimeLogRecorded => "runtime_log_recorded",
            Self::ResponseRecorded => "response_recorded",
            Self::FailureRecorded => "failure_recorded",
            Self::CancellationRecorded => "cancellation_recorded",
            Self::Succeeded => "succeeded",
            Self::IncompleteOutput => "incomplete_output",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::IncompleteOutput | Self::Failed | Self::Cancelled
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InferenceAttemptTransition {
    StartAttempt {
        backend: String,
        request_path: PathBuf,
    },
    RecordRequest {
        path: PathBuf,
    },
    RecordPlan {
        plan: InferencePlanEvidence,
    },
    RecordContentReady {
        content: InferenceContentEvidence,
    },
    RecordAdmissionGrant {
        admission: InferenceAdmissionEvidence,
    },
    RecordDispatch {
        dispatch: InferenceDispatchEvidence,
    },
    RecordRuntimeStarted {
        runtime: InferenceRuntimeEvidence,
    },
    RecordRuntimeLog {
        path: PathBuf,
    },
    RecordResponse {
        path: PathBuf,
    },
    RecordFailure {
        failure: InferenceAttemptFailure,
    },
    RecordCancellation {
        cancellation: InferenceAttemptCancellation,
    },
    FinishAttempt {
        outcome: InferenceAttemptOutcome,
    },
}

impl InferenceAttemptTransition {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::StartAttempt { .. } => "start_attempt",
            Self::RecordRequest { .. } => "record_request",
            Self::RecordPlan { .. } => "record_plan",
            Self::RecordContentReady { .. } => "record_content_ready",
            Self::RecordAdmissionGrant { .. } => "record_admission_grant",
            Self::RecordDispatch { .. } => "record_dispatch",
            Self::RecordRuntimeStarted { .. } => "record_runtime_started",
            Self::RecordRuntimeLog { .. } => "record_runtime_log",
            Self::RecordResponse { .. } => "record_response",
            Self::RecordFailure { .. } => "record_failure",
            Self::RecordCancellation { .. } => "record_cancellation",
            Self::FinishAttempt { .. } => "finish_attempt",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceAttemptTransitionRecord {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub attempt_id: AttemptId,
    pub sequence: usize,
    pub from: InferenceAttemptPhase,
    pub to: InferenceAttemptPhase,
    pub transition: InferenceAttemptTransition,
}

impl InferenceAttemptTransitionRecord {
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    pub fn transition(&self) -> &InferenceAttemptTransition {
        &self.transition
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceAttemptMachine {
    run_id: RunId,
    turn_id: TurnId,
    attempt_id: AttemptId,
    phase: InferenceAttemptPhase,
    sequence: usize,
}

impl InferenceAttemptMachine {
    pub fn new(run_id: RunId, turn_id: TurnId, attempt_id: AttemptId) -> Self {
        Self {
            run_id,
            turn_id,
            attempt_id,
            phase: InferenceAttemptPhase::Initialized,
            sequence: 0,
        }
    }

    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    pub fn phase(&self) -> InferenceAttemptPhase {
        self.phase
    }

    pub fn sequence(&self) -> usize {
        self.sequence
    }

    pub fn preview(
        &self,
        transition: InferenceAttemptTransition,
    ) -> Result<InferenceAttemptTransitionRecord, InferenceAttemptTransitionError> {
        let from = self.phase;
        let Some(to) = next_phase(from, &transition) else {
            return Err(InferenceAttemptTransitionError {
                phase: from,
                transition: transition.as_str(),
            });
        };
        Ok(InferenceAttemptTransitionRecord {
            run_id: self.run_id.clone(),
            turn_id: self.turn_id.clone(),
            attempt_id: self.attempt_id.clone(),
            sequence: self.sequence + 1,
            from,
            to,
            transition,
        })
    }

    pub fn commit(
        &mut self,
        record: &InferenceAttemptTransitionRecord,
    ) -> Result<(), InferenceAttemptTransitionError> {
        if record.run_id != self.run_id
            || record.turn_id != self.turn_id
            || record.attempt_id != self.attempt_id
            || record.sequence != self.sequence + 1
            || record.from != self.phase
            || next_phase(self.phase, &record.transition) != Some(record.to)
        {
            return Err(InferenceAttemptTransitionError {
                phase: self.phase,
                transition: record.transition.as_str(),
            });
        }
        self.sequence = record.sequence;
        self.phase = record.to;
        Ok(())
    }

    pub fn apply(
        &mut self,
        transition: InferenceAttemptTransition,
    ) -> Result<InferenceAttemptTransitionRecord, InferenceAttemptTransitionError> {
        let record = self.preview(transition)?;
        self.commit(&record)?;
        Ok(record)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferenceAttemptTransitionError {
    pub phase: InferenceAttemptPhase,
    pub transition: &'static str,
}

impl fmt::Display for InferenceAttemptTransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "illegal inference attempt transition `{}` from phase `{}`",
            self.transition,
            self.phase.as_str()
        )
    }
}

impl Error for InferenceAttemptTransitionError {}

fn next_phase(
    from: InferenceAttemptPhase,
    transition: &InferenceAttemptTransition,
) -> Option<InferenceAttemptPhase> {
    use InferenceAttemptPhase::*;
    use InferenceAttemptTransition::*;

    match (from, transition) {
        (Initialized, StartAttempt { .. }) => Some(Started),
        (Started, RecordRequest { .. }) => Some(RequestRecorded),
        (RequestRecorded, RecordPlan { .. }) => Some(Planned),
        (Planned, RecordContentReady { .. }) => Some(ContentReady),
        (ContentReady, RecordAdmissionGrant { .. }) => Some(Admitted),
        (Admitted, RecordDispatch { .. }) => Some(DispatchRecorded),
        (DispatchRecorded, RecordRuntimeStarted { .. }) => Some(RuntimeGenerating),
        (RuntimeGenerating, RecordRuntimeLog { .. }) => Some(RuntimeLogRecorded),
        (RuntimeLogRecorded, RecordResponse { .. }) => Some(ResponseRecorded),
        (phase, RecordFailure { .. }) if is_failure_eligible(phase) => Some(FailureRecorded),
        (phase, RecordCancellation { .. }) if is_failure_eligible(phase) => {
            Some(CancellationRecorded)
        }
        (
            ResponseRecorded,
            FinishAttempt {
                outcome: InferenceAttemptOutcome::Succeeded,
            },
        ) => Some(Succeeded),
        (
            ResponseRecorded,
            FinishAttempt {
                outcome: InferenceAttemptOutcome::IncompleteOutput,
            },
        ) => Some(IncompleteOutput),
        (
            FailureRecorded,
            FinishAttempt {
                outcome: InferenceAttemptOutcome::Failed,
            },
        ) => Some(Failed),
        (
            CancellationRecorded,
            FinishAttempt {
                outcome: InferenceAttemptOutcome::Cancelled,
            },
        ) => Some(Cancelled),
        _ => None,
    }
}

fn is_failure_eligible(phase: InferenceAttemptPhase) -> bool {
    !phase.is_terminal()
        && !matches!(
            phase,
            InferenceAttemptPhase::Initialized
                | InferenceAttemptPhase::FailureRecorded
                | InferenceAttemptPhase::CancellationRecorded
        )
}
