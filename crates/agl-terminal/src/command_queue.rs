use std::collections::VecDeque;
use std::fmt::{self, Debug, Formatter};
use std::path::PathBuf;
use std::time::Instant;

use agl_exec::{ExecutionId, ProcessBytes, ProcessError, ProcessErrorCode, Result};
use serde::{Deserialize, Serialize};

use crate::{
    MAX_TYPED_TERMINAL_COMMAND_BYTES, ShellExit, TerminalId, TerminalPromptState,
    human_terminal_command_submission,
};

pub const DEFAULT_AGENT_TERMINAL_QUEUE_CAPACITY: usize = 32;
pub const MAX_AGENT_TERMINAL_COMMAND_BYTES: usize = MAX_TYPED_TERMINAL_COMMAND_BYTES;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanTerminalCommandAdmission {
    pub terminal_id: TerminalId,
    pub execution_id: ExecutionId,
    pub command_sequence: u64,
    pub output_after_sequence: u64,
    pub submission: ProcessBytes,
}

pub struct QueuedTerminalCommand {
    pub command_sequence: u64,
    command: String,
    pub deadline: Option<Instant>,
}

impl QueuedTerminalCommand {
    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn submission(&self) -> ProcessBytes {
        human_terminal_command_submission(&self.command)
            .expect("queued agent commands passed the shared typed-command validator")
    }
}

impl Debug for QueuedTerminalCommand {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueuedTerminalCommand")
            .field("command_sequence", &self.command_sequence)
            .field("command_bytes", &self.command.len())
            .field("deadline", &self.deadline)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalCommandOutputRange {
    pub after_sequence: u64,
    pub through_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalCommandResult {
    pub terminal_id: TerminalId,
    pub execution_id: ExecutionId,
    pub command_sequence: u64,
    pub cwd: PathBuf,
    pub exit: ShellExit,
    pub output: TerminalCommandOutputRange,
}

struct ActiveCommand {
    command: QueuedTerminalCommand,
    integration_start_sequence: Option<u64>,
    output_after_sequence: u64,
    submission_reserved: bool,
    submitted: bool,
}

pub struct AgentTerminalCommandQueue {
    capacity: usize,
    next_sequence: u64,
    queued: VecDeque<QueuedTerminalCommand>,
    active: Option<ActiveCommand>,
}

impl AgentTerminalCommandQueue {
    pub fn new(capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "agent terminal command queue capacity must be nonzero",
            ));
        }
        Ok(Self {
            capacity,
            next_sequence: 1,
            queued: VecDeque::new(),
            active: None,
        })
    }

    pub fn enqueue(&mut self, command: String, deadline: Option<Instant>) -> Result<u64> {
        human_terminal_command_submission(&command)?;
        if self.queued.len() >= self.capacity {
            return Err(ProcessError::new(
                ProcessErrorCode::InputBackpressure,
                "agent terminal command queue is full",
            ));
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.checked_add(1).ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "agent terminal command sequence overflowed",
            )
        })?;
        self.queued.push_back(QueuedTerminalCommand {
            command_sequence: sequence,
            command,
            deadline,
        });
        Ok(sequence)
    }

    pub fn begin_next(
        &mut self,
        prompt: &TerminalPromptState,
        output_after_sequence: u64,
    ) -> Result<Option<&QueuedTerminalCommand>> {
        if self.active.is_some() {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "agent terminal already has an active command",
            ));
        }
        if !prompt.is_trusted_ready() {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "agent terminal command requires a trusted fresh prompt",
            ));
        }
        let Some(command) = self.queued.pop_front() else {
            return Ok(None);
        };
        self.active = Some(ActiveCommand {
            command,
            integration_start_sequence: None,
            output_after_sequence,
            submission_reserved: false,
            submitted: false,
        });
        Ok(self.active.as_ref().map(|active| &active.command))
    }

    pub fn mark_started(
        &mut self,
        integration_sequence: u64,
        output_after_sequence: u64,
    ) -> Result<()> {
        let active = self.active.as_mut().ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "shell integration started a command with no active queue item",
            )
        })?;
        if integration_sequence == 0
            || active
                .integration_start_sequence
                .replace(integration_sequence)
                .is_some()
        {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "active agent command has an invalid duplicate integration start",
            ));
        }
        active.output_after_sequence = output_after_sequence;
        Ok(())
    }

    pub fn reserve_submission(&mut self) -> Result<ProcessBytes> {
        let active = self.active.as_mut().ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "agent terminal has no active command to submit",
            )
        })?;
        if active.submitted || active.submission_reserved {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "active agent command submission is already reserved or complete",
            ));
        }
        active.submission_reserved = true;
        Ok(active.command.submission())
    }

    pub fn complete_submission(&mut self) -> Result<()> {
        let active = self.active.as_mut().ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "agent terminal has no active command submission",
            )
        })?;
        if !active.submission_reserved || active.submitted {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "active agent command submission reservation is invalid",
            ));
        }
        active.submission_reserved = false;
        active.submitted = true;
        Ok(())
    }

    pub fn abandon_submission(&mut self) {
        if let Some(active) = self.active.as_mut()
            && !active.submitted
        {
            active.submission_reserved = false;
        }
    }

    pub fn finish(
        &mut self,
        terminal_id: TerminalId,
        execution_id: ExecutionId,
        integration_finish_sequence: u64,
        exit: ShellExit,
        cwd: PathBuf,
        output_through_sequence: u64,
    ) -> Result<TerminalCommandResult> {
        let active = self.active.take().ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "shell integration finished a command with no active queue item",
            )
        })?;
        let start = active.integration_start_sequence.ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "shell integration finished before command_started",
            )
        })?;
        if integration_finish_sequence <= start
            || output_through_sequence < active.output_after_sequence
        {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "agent terminal command boundary is not monotonic",
            ));
        }
        Ok(TerminalCommandResult {
            terminal_id,
            execution_id,
            command_sequence: active.command.command_sequence,
            cwd,
            exit,
            output: TerminalCommandOutputRange {
                after_sequence: active.output_after_sequence,
                through_sequence: output_through_sequence,
            },
        })
    }

    pub fn cancel_active(&mut self) -> Option<u64> {
        self.active
            .take()
            .map(|active| active.command.command_sequence)
    }

    pub fn cancel_queued(&mut self, command_sequence: u64) -> bool {
        let Some(position) = self
            .queued
            .iter()
            .position(|command| command.command_sequence == command_sequence)
        else {
            return false;
        };
        self.queued.remove(position);
        true
    }

    pub fn cancel_all(&mut self) -> Vec<u64> {
        let mut cancelled = self
            .active
            .take()
            .map(|active| vec![active.command.command_sequence])
            .unwrap_or_default();
        cancelled.extend(
            self.queued
                .drain(..)
                .map(|command| command.command_sequence),
        );
        cancelled
    }

    pub fn is_queued(&self, command_sequence: u64) -> bool {
        self.queued
            .iter()
            .any(|command| command.command_sequence == command_sequence)
    }

    pub fn active_deadline(&self) -> Option<Instant> {
        self.active
            .as_ref()
            .and_then(|active| active.command.deadline)
    }

    pub fn active_is_submitted(&self) -> bool {
        self.active.as_ref().is_some_and(|active| active.submitted)
    }

    pub fn queued_len(&self) -> usize {
        self.queued.len()
    }

    pub fn active_sequence(&self) -> Option<u64> {
        self.active
            .as_ref()
            .map(|active| active.command.command_sequence)
    }

    pub fn active_command(&self) -> Option<&str> {
        self.active
            .as_ref()
            .map(|active| active.command.command.as_str())
    }
}

impl Default for AgentTerminalCommandQueue {
    fn default() -> Self {
        Self::new(DEFAULT_AGENT_TERMINAL_QUEUE_CAPACITY)
            .expect("default terminal queue capacity is nonzero")
    }
}

impl Debug for AgentTerminalCommandQueue {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentTerminalCommandQueue")
            .field("capacity", &self.capacity)
            .field("next_sequence", &self.next_sequence)
            .field("queued", &self.queued.len())
            .field("active_sequence", &self.active_sequence())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_fifo_exact_and_never_interleave() {
        let mut queue = AgentTerminalCommandQueue::new(2).unwrap();
        assert_eq!(
            queue
                .enqueue("cd dir && printf '%s' x".to_owned(), None)
                .unwrap(),
            1
        );
        assert_eq!(queue.enqueue("pwd".to_owned(), None).unwrap(), 2);
        assert_eq!(
            queue.enqueue("third".to_owned(), None).unwrap_err().code(),
            ProcessErrorCode::InputBackpressure
        );
        let prompt = TerminalPromptState::Ready {
            sequence: 1,
            last_exit: None,
        };
        let first = queue.begin_next(&prompt, 8).unwrap().unwrap();
        assert_eq!(first.command(), "cd dir && printf '%s' x");
        assert_eq!(
            first.submission().decode(1024).unwrap(),
            b"\x1b[200~cd dir && printf '%s' x\x1b[201~\n"
        );
        assert_eq!(
            queue.begin_next(&prompt, 8).unwrap_err().code(),
            ProcessErrorCode::StateConflict
        );
        queue.mark_started(2, 11).unwrap();
        let result = queue
            .finish(
                TerminalId::generate(),
                ExecutionId::generate(),
                3,
                ShellExit::Code { code: 0 },
                PathBuf::from("/workspace/dir"),
                12,
            )
            .unwrap();
        assert_eq!(result.command_sequence, 1);
        assert_eq!(result.output.after_sequence, 11);
        assert_eq!(result.output.through_sequence, 12);
        assert_eq!(queue.queued_len(), 1);
    }

    #[test]
    fn degraded_or_busy_prompt_cannot_submit_agent_bytes() {
        let mut queue = AgentTerminalCommandQueue::new(1).unwrap();
        queue.enqueue("true".to_owned(), None).unwrap();
        assert_eq!(
            queue
                .begin_next(&TerminalPromptState::Degraded, 0)
                .unwrap_err()
                .code(),
            ProcessErrorCode::StateConflict
        );
        assert_eq!(queue.queued_len(), 1);
    }
}
