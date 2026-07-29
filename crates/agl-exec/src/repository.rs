use sha2::{Digest as _, Sha256};

use crate::{
    ExecutionChannel, ExecutionExit, ExecutionId, ExecutionOutputChunk, ExecutionState,
    ProcessError, ProcessErrorCode, Result,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedOutputFrame {
    pub sequence: u64,
    pub channel: ExecutionChannel,
    pub spool_offset: u64,
    pub byte_length: u64,
    pub safe_digest: String,
}

impl CommittedOutputFrame {
    pub fn from_chunk(chunk: &ExecutionOutputChunk, spool_offset: u64) -> Result<Self> {
        let payload = chunk.bytes.decode(usize::MAX)?;
        Ok(Self {
            sequence: chunk.sequence,
            channel: chunk.channel,
            spool_offset,
            byte_length: payload.len() as u64,
            safe_digest: sha256_digest(&payload),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionTerminalUpdate {
    pub state: ExecutionState,
    pub exit: Option<ExecutionExit>,
    pub error_code: Option<String>,
    pub finished_at_unix_ms: i64,
    pub output_truncated: bool,
    pub discarded_output_bytes: u64,
}

impl ExecutionTerminalUpdate {
    pub fn validate(&self) -> Result<()> {
        if !self.state.is_terminal() {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "execution terminal update requires a terminal state",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputSpoolRead {
    pub chunks: Vec<ExecutionOutputChunk>,
    pub complete: bool,
}

/// Private bounded byte storage. Implementations append and sync before the
/// repository publishes matching metadata.
pub trait OutputSpool: Send + Sync {
    fn prepare(&self, execution_id: &ExecutionId) -> Result<()>;
    fn append(&self, execution_id: &ExecutionId, chunk: &ExecutionOutputChunk) -> Result<u64>;
    fn sync(&self, execution_id: &ExecutionId) -> Result<()>;
    fn read(
        &self,
        execution_id: &ExecutionId,
        after_sequence: u64,
        through_sequence: u64,
        maximum_bytes: usize,
    ) -> Result<OutputSpoolRead>;
    fn recover(&self, execution_id: &ExecutionId, committed: &[CommittedOutputFrame])
    -> Result<()>;
    fn remove(&self, execution_id: &ExecutionId) -> Result<()>;
}

fn sha256_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut rendered = String::with_capacity(71);
    rendered.push_str("sha256:");
    for byte in Sha256::digest(bytes) {
        write!(&mut rendered, "{byte:02x}").expect("writing to a String cannot fail");
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProcessBytes;

    #[test]
    fn committed_output_frame_is_derived_from_the_exact_payload() {
        let chunk = ExecutionOutputChunk {
            sequence: 7,
            channel: ExecutionChannel::Stdout,
            bytes: ProcessBytes::from_bytes(b"hello"),
        };

        let frame = CommittedOutputFrame::from_chunk(&chunk, 41).unwrap();

        assert_eq!(frame.sequence, 7);
        assert_eq!(frame.channel, ExecutionChannel::Stdout);
        assert_eq!(frame.spool_offset, 41);
        assert_eq!(frame.byte_length, 5);
        assert_eq!(
            frame.safe_digest,
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn terminal_update_requires_a_terminal_state() {
        let mut update = ExecutionTerminalUpdate {
            state: ExecutionState::Running,
            exit: None,
            error_code: None,
            finished_at_unix_ms: 1,
            output_truncated: false,
            discarded_output_bytes: 0,
        };

        assert_eq!(
            update.validate().unwrap_err().code(),
            ProcessErrorCode::StateConflict
        );
        update.state = ExecutionState::Exited;
        assert!(update.validate().is_ok());
    }
}
