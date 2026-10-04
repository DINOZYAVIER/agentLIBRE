//! Execution state decisions from manager/helper observations; no host I/O.
use agl_core::execution::*;
use agl_core::{Fsm, Transition};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct MainOutcome {
    pub outcome: Option<ExecutionOutcome>,
    pub result: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Streams {
    pub stdout: Option<ExecutionStreamStatus>,
    pub stderr: Option<ExecutionStreamStatus>,
    pub terminal: Option<ExecutionStreamStatus>,
}
impl From<&ExecutionStatus> for Streams {
    fn from(status: &ExecutionStatus) -> Self {
        Self {
            stdout: status.stdout,
            stderr: status.stderr,
            terminal: status.terminal,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Completion {
    pub main: MainOutcome,
    pub streams: Streams,
    pub started: bool,
    pub launch_failed: bool,
    pub timeout_configured: bool,
    pub stop_cause: Option<ExecutionStopCause>,
    pub owner_lost: bool,
}

pub(crate) enum ExecutionInput {
    Complete(Completion),
    Observe {
        unit_live: bool,
        completion: Option<ExecutionStatus>,
        streams: Streams,
        stop_cause: Option<ExecutionStopCause>,
    },
    OutputsRemoved,
}

pub(crate) struct ExecutionFsm;
impl ExecutionFsm {
    pub(crate) fn initial(execution_id: ExecutionId, io: ExecutionIo) -> ExecutionStatus {
        let stream = ExecutionStreamStatus::default();
        ExecutionStatus {
            execution_id,
            state: ExecutionState::Running,
            outcome: None,
            stopped_by: None,
            stdout: (io == ExecutionIo::Pipes).then_some(stream),
            stderr: (io == ExecutionIo::Pipes).then_some(stream),
            terminal: (io == ExecutionIo::Pty).then_some(stream),
            outputs_removed: false,
        }
    }
}
impl Fsm for ExecutionFsm {
    type State = ExecutionStatus;
    type Input = ExecutionInput;
    type Output = ();
    type Error = &'static str;

    fn transition(
        &self,
        state: &ExecutionStatus,
        input: ExecutionInput,
    ) -> Result<Transition<ExecutionStatus, ()>, Self::Error> {
        let mut next = state.clone();
        match input {
            ExecutionInput::OutputsRemoved => {
                if state.state == ExecutionState::Running {
                    return Err("cannot clean running execution");
                }
                next.outputs_removed = true;
            }
            ExecutionInput::Complete(completion)
                if state.state == ExecutionState::Running && !state.outputs_removed =>
            {
                let complete = if state.terminal.is_some() {
                    completion.streams.terminal.is_some()
                } else {
                    completion.streams.stdout.is_some() && completion.streams.stderr.is_some()
                };
                let known = complete
                    && completion.main.outcome.is_some()
                    && completion.started
                    && !completion.launch_failed;
                next.state = if known {
                    ExecutionState::Exited
                } else {
                    ExecutionState::OutcomeUnknown
                };
                next.outcome = known.then_some(completion.main.outcome).flatten();
                let mut cause = completion.stop_cause;
                if cause.is_none() && completion.main.result == "timeout" {
                    cause = Some(ExecutionStopCause::Timeout);
                }
                let signalled = matches!(next.outcome, Some(ExecutionOutcome::Signal { .. }));
                if cause == Some(ExecutionStopCause::Timeout)
                    && (!signalled || !completion.timeout_configured)
                {
                    cause = None;
                }
                if cause.is_none() && signalled && completion.owner_lost {
                    cause = Some(ExecutionStopCause::TmuxLost);
                }
                next.stopped_by = known.then_some(cause).flatten();
                next.stdout = completion.streams.stdout;
                next.stderr = completion.streams.stderr;
                next.terminal = completion.streams.terminal;
            }
            ExecutionInput::Observe {
                unit_live,
                completion,
                streams,
                stop_cause,
            } if state.state == ExecutionState::Running && !state.outputs_removed => {
                if let Some(completion) = completion {
                    next = completion;
                } else if !unit_live {
                    next.state = ExecutionState::OutcomeUnknown;
                }
                for (target, observed) in [
                    (&mut next.stdout, streams.stdout),
                    (&mut next.stderr, streams.stderr),
                    (&mut next.terminal, streams.terminal),
                ] {
                    if target.is_some() && observed.is_some() {
                        *target = observed;
                    }
                }
                if next.state == ExecutionState::OutcomeUnknown {
                    next.outcome = None;
                    next.stopped_by = None;
                } else {
                    next.stopped_by = stop_cause.or(next.stopped_by);
                }
            }
            ExecutionInput::Observe { .. } | ExecutionInput::Complete(_) => {}
        }
        Ok(Transition {
            state: next,
            output: (),
        })
    }
}
