use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use crate::evidence::{InferenceAttemptId, InferenceFinishStatus, InferenceRunId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InferenceAttemptPhase {
    Initialized,
    Started,
    RequestRecorded,
    RuntimeGenerating,
    RuntimeLogRecorded,
    ResponseRecorded,
    Failed,
    Finished,
}

impl InferenceAttemptPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            InferenceAttemptPhase::Initialized => "initialized",
            InferenceAttemptPhase::Started => "started",
            InferenceAttemptPhase::RequestRecorded => "request_recorded",
            InferenceAttemptPhase::RuntimeGenerating => "runtime_generating",
            InferenceAttemptPhase::RuntimeLogRecorded => "runtime_log_recorded",
            InferenceAttemptPhase::ResponseRecorded => "response_recorded",
            InferenceAttemptPhase::Failed => "failed",
            InferenceAttemptPhase::Finished => "finished",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InferenceAttemptTransition {
    StartAttempt {
        backend: String,
        request_path: PathBuf,
    },
    RecordRequest {
        path: PathBuf,
    },
    StartRuntime,
    RecordRuntimeLog {
        path: PathBuf,
    },
    RecordResponse {
        path: PathBuf,
    },
    FailAttempt {
        message: String,
    },
    FinishAttempt {
        status: InferenceFinishStatus,
    },
}

impl InferenceAttemptTransition {
    pub fn as_str(&self) -> &'static str {
        match self {
            InferenceAttemptTransition::StartAttempt { .. } => "start_attempt",
            InferenceAttemptTransition::RecordRequest { .. } => "record_request",
            InferenceAttemptTransition::StartRuntime => "start_runtime",
            InferenceAttemptTransition::RecordRuntimeLog { .. } => "record_runtime_log",
            InferenceAttemptTransition::RecordResponse { .. } => "record_response",
            InferenceAttemptTransition::FailAttempt { .. } => "fail_attempt",
            InferenceAttemptTransition::FinishAttempt { .. } => "finish_attempt",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferenceAttemptTransitionRecord {
    pub run_id: InferenceRunId,
    pub attempt_id: InferenceAttemptId,
    pub sequence: usize,
    pub from: InferenceAttemptPhase,
    pub to: InferenceAttemptPhase,
    pub transition: InferenceAttemptTransition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferenceAttemptMachine {
    run_id: InferenceRunId,
    attempt_id: InferenceAttemptId,
    phase: InferenceAttemptPhase,
    sequence: usize,
}

impl InferenceAttemptMachine {
    pub fn new(run_id: InferenceRunId, attempt_id: InferenceAttemptId) -> Self {
        Self {
            run_id,
            attempt_id,
            phase: InferenceAttemptPhase::Initialized,
            sequence: 0,
        }
    }

    pub fn phase(&self) -> InferenceAttemptPhase {
        self.phase
    }

    pub fn sequence(&self) -> usize {
        self.sequence
    }

    pub fn apply(
        &mut self,
        transition: InferenceAttemptTransition,
    ) -> Result<InferenceAttemptTransitionRecord, InferenceAttemptTransitionError> {
        let from = self.phase;
        let Some(to) = next_phase(from, &transition) else {
            return Err(InferenceAttemptTransitionError {
                phase: from,
                transition: transition.as_str(),
            });
        };

        self.sequence += 1;
        self.phase = to;
        Ok(InferenceAttemptTransitionRecord {
            run_id: self.run_id.clone(),
            attempt_id: self.attempt_id.clone(),
            sequence: self.sequence,
            from,
            to,
            transition,
        })
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
        (RequestRecorded, StartRuntime) => Some(RuntimeGenerating),
        (RuntimeGenerating, RecordRuntimeLog { .. }) => Some(RuntimeLogRecorded),
        (RuntimeGenerating | RuntimeLogRecorded, FailAttempt { .. }) => Some(Failed),
        (RuntimeLogRecorded, RecordResponse { .. }) => Some(ResponseRecorded),
        (
            ResponseRecorded,
            FinishAttempt {
                status: InferenceFinishStatus::Succeeded,
            },
        ) => Some(Finished),
        (
            Failed,
            FinishAttempt {
                status: InferenceFinishStatus::Failed,
            },
        ) => Some(Finished),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> InferenceAttemptMachine {
        InferenceAttemptMachine::new(
            InferenceRunId::new("run-001").unwrap(),
            InferenceAttemptId::new("attempt-001").unwrap(),
        )
    }

    #[test]
    fn accepts_success_path() {
        let mut machine = machine();

        assert_eq!(
            machine
                .apply(InferenceAttemptTransition::StartAttempt {
                    backend: "llama_cpp".to_string(),
                    request_path: PathBuf::from("request.json"),
                })
                .unwrap()
                .to,
            InferenceAttemptPhase::Started
        );
        machine
            .apply(InferenceAttemptTransition::RecordRequest {
                path: PathBuf::from("request.json"),
            })
            .unwrap();
        machine
            .apply(InferenceAttemptTransition::StartRuntime)
            .unwrap();
        machine
            .apply(InferenceAttemptTransition::RecordRuntimeLog {
                path: PathBuf::from("runtime.log"),
            })
            .unwrap();
        machine
            .apply(InferenceAttemptTransition::RecordResponse {
                path: PathBuf::from("response.json"),
            })
            .unwrap();
        let record = machine
            .apply(InferenceAttemptTransition::FinishAttempt {
                status: InferenceFinishStatus::Succeeded,
            })
            .unwrap();

        assert_eq!(record.to, InferenceAttemptPhase::Finished);
        assert_eq!(record.sequence, 6);
    }

    #[test]
    fn accepts_failure_path() {
        let mut machine = machine();
        machine
            .apply(InferenceAttemptTransition::StartAttempt {
                backend: "llama_cpp".to_string(),
                request_path: PathBuf::from("request.json"),
            })
            .unwrap();
        machine
            .apply(InferenceAttemptTransition::RecordRequest {
                path: PathBuf::from("request.json"),
            })
            .unwrap();
        machine
            .apply(InferenceAttemptTransition::StartRuntime)
            .unwrap();
        machine
            .apply(InferenceAttemptTransition::FailAttempt {
                message: "runtime failed".to_string(),
            })
            .unwrap();
        let record = machine
            .apply(InferenceAttemptTransition::FinishAttempt {
                status: InferenceFinishStatus::Failed,
            })
            .unwrap();

        assert_eq!(record.to, InferenceAttemptPhase::Finished);
    }

    #[test]
    fn rejects_illegal_transition_and_finished_is_terminal() {
        let mut machine = machine();
        let err = machine
            .apply(InferenceAttemptTransition::RecordResponse {
                path: PathBuf::from("response.json"),
            })
            .unwrap_err();
        assert_eq!(err.phase, InferenceAttemptPhase::Initialized);
        assert_eq!(err.transition, "record_response");

        machine
            .apply(InferenceAttemptTransition::StartAttempt {
                backend: "llama_cpp".to_string(),
                request_path: PathBuf::from("request.json"),
            })
            .unwrap();
        machine
            .apply(InferenceAttemptTransition::RecordRequest {
                path: PathBuf::from("request.json"),
            })
            .unwrap();
        machine
            .apply(InferenceAttemptTransition::StartRuntime)
            .unwrap();
        machine
            .apply(InferenceAttemptTransition::FailAttempt {
                message: "runtime failed".to_string(),
            })
            .unwrap();
        machine
            .apply(InferenceAttemptTransition::FinishAttempt {
                status: InferenceFinishStatus::Failed,
            })
            .unwrap();

        let err = machine
            .apply(InferenceAttemptTransition::StartRuntime)
            .unwrap_err();
        assert_eq!(err.phase, InferenceAttemptPhase::Finished);
    }
}
