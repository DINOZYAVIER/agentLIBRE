use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use agl_ids::{AttemptId, RunId, TurnId};

use crate::evidence::InferenceFinishStatus;

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
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub attempt_id: AttemptId,
    pub sequence: usize,
    pub from: InferenceAttemptPhase,
    pub to: InferenceAttemptPhase,
    pub transition: InferenceAttemptTransition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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
            turn_id: self.turn_id.clone(),
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
            RunId::parse("run_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b31").unwrap(),
            TurnId::parse("turn_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b32").unwrap(),
            AttemptId::parse("attempt_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b33").unwrap(),
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
