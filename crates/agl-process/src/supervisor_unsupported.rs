use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use agl_ids::{ExecutionId, RequestId, RunId, WriterLeaseId};

use crate::terminal::shell::ManagedShellStartup;
use crate::{
    ExecutionCursor, ExecutionListFilter, ExecutionOwner, ExecutionPrivateCommand,
    ExecutionReadResult, ExecutionRepository, ExecutionRequest, ExecutionStatus, InputLease,
    KillMode, OutputSpool, ProcessBytes, ProcessError, ProcessErrorCode, ProcessSupervisorOptions,
    Result, ShellIntegrationReadResult, TerminalSize,
};

#[derive(Clone)]
pub struct ProcessHandle {
    _private: (),
}

pub struct ProcessSupervisor {
    _private: (),
}

impl ProcessSupervisor {
    pub fn start(
        _options: ProcessSupervisorOptions,
        _repository: Arc<dyn ExecutionRepository>,
        _spool: Arc<dyn OutputSpool>,
    ) -> Result<Self> {
        unsupported()
    }

    pub fn handle(&self) -> ProcessHandle {
        ProcessHandle { _private: () }
    }

    pub fn shutdown(&self) -> Result<()> {
        unsupported()
    }
}

impl ProcessHandle {
    pub fn start(&self, _request: ExecutionRequest) -> Result<ExecutionStatus> {
        unsupported()
    }

    pub fn start_cancellable(
        &self,
        _request: ExecutionRequest,
        _deadline: Option<Instant>,
        _cancelled: impl Fn() -> bool,
    ) -> Result<ExecutionStatus> {
        unsupported()
    }

    pub(crate) fn start_reserved_managed_terminal(
        &self,
        _execution_id: ExecutionId,
        _request: ExecutionRequest,
        _managed_startup: ManagedShellStartup,
    ) -> Result<ExecutionStatus> {
        unsupported()
    }

    pub fn status(
        &self,
        _execution_id: &ExecutionId,
        _owner: &ExecutionOwner,
    ) -> Result<ExecutionStatus> {
        unsupported()
    }

    pub fn operator_status(&self, _execution_id: &ExecutionId) -> Result<ExecutionStatus> {
        unsupported()
    }

    pub fn operator_private_command(
        &self,
        _execution_id: &ExecutionId,
        _maximum_bytes: usize,
    ) -> Result<ExecutionPrivateCommand> {
        unsupported()
    }

    pub fn list_owned(
        &self,
        _filter: ExecutionListFilter,
        _owner: &ExecutionOwner,
    ) -> Result<Vec<ExecutionStatus>> {
        unsupported()
    }

    pub fn operator_list(&self, _filter: ExecutionListFilter) -> Result<Vec<ExecutionStatus>> {
        unsupported()
    }

    pub fn read(
        &self,
        _execution_id: &ExecutionId,
        _owner: &ExecutionOwner,
        _cursor: ExecutionCursor,
        _maximum_bytes: usize,
    ) -> Result<ExecutionReadResult> {
        unsupported()
    }

    pub fn operator_read(
        &self,
        _execution_id: &ExecutionId,
        _cursor: ExecutionCursor,
        _maximum_bytes: usize,
    ) -> Result<ExecutionReadResult> {
        unsupported()
    }

    pub fn read_shell_integration(
        &self,
        _execution_id: &ExecutionId,
        _owner: &ExecutionOwner,
        _maximum_bytes: usize,
    ) -> Result<ShellIntegrationReadResult> {
        unsupported()
    }

    pub fn operator_read_shell_integration(
        &self,
        _execution_id: &ExecutionId,
        _maximum_bytes: usize,
    ) -> Result<ShellIntegrationReadResult> {
        unsupported()
    }

    pub fn attach(
        &self,
        _execution_id: &ExecutionId,
        _owner: &ExecutionOwner,
        _attachment_id: RequestId,
        _writable: bool,
    ) -> Result<InputLease> {
        unsupported()
    }

    pub fn operator_attach(
        &self,
        _execution_id: &ExecutionId,
        _attachment_id: RequestId,
        _writable: bool,
    ) -> Result<InputLease> {
        unsupported()
    }

    pub fn detach(
        &self,
        _execution_id: &ExecutionId,
        _owner: &ExecutionOwner,
        _lease: InputLease,
    ) -> Result<()> {
        unsupported()
    }

    pub fn operator_detach(&self, _execution_id: &ExecutionId, _lease: InputLease) -> Result<()> {
        unsupported()
    }

    pub fn renew_input_lease(
        &self,
        _execution_id: &ExecutionId,
        _owner: &ExecutionOwner,
        _lease: InputLease,
    ) -> Result<()> {
        unsupported()
    }

    pub fn operator_renew_input_lease(
        &self,
        _execution_id: &ExecutionId,
        _lease: InputLease,
    ) -> Result<()> {
        unsupported()
    }

    pub fn input_lease_active(
        &self,
        _execution_id: &ExecutionId,
        _owner: &ExecutionOwner,
        _lease: InputLease,
    ) -> Result<bool> {
        unsupported()
    }

    pub fn operator_input_lease_active(
        &self,
        _execution_id: &ExecutionId,
        _lease: InputLease,
    ) -> Result<bool> {
        unsupported()
    }

    pub fn operator_resolve_writer_lease(
        &self,
        _execution_id: &ExecutionId,
        _writer_lease_id: WriterLeaseId,
    ) -> Result<InputLease> {
        unsupported()
    }

    pub fn write(
        &self,
        _execution_id: &ExecutionId,
        _owner: &ExecutionOwner,
        _lease: InputLease,
        _bytes: ProcessBytes,
        _eof: bool,
    ) -> Result<()> {
        unsupported()
    }

    pub fn operator_write(
        &self,
        _execution_id: &ExecutionId,
        _lease: InputLease,
        _bytes: ProcessBytes,
        _eof: bool,
    ) -> Result<()> {
        unsupported()
    }

    pub fn resize(
        &self,
        _execution_id: &ExecutionId,
        _owner: &ExecutionOwner,
        _terminal_size: TerminalSize,
    ) -> Result<()> {
        unsupported()
    }

    pub fn operator_resize(
        &self,
        _execution_id: &ExecutionId,
        _terminal_size: TerminalSize,
    ) -> Result<()> {
        unsupported()
    }

    pub fn interrupt_foreground(
        &self,
        _execution_id: &ExecutionId,
        _owner: &ExecutionOwner,
    ) -> Result<()> {
        unsupported()
    }

    pub fn operator_interrupt_foreground(&self, _execution_id: &ExecutionId) -> Result<()> {
        unsupported()
    }

    pub(crate) fn operator_handoff_managed_terminal(
        &self,
        _execution_id: &ExecutionId,
        _owner: ExecutionOwner,
        _interrupt_foreground: bool,
    ) -> Result<()> {
        unsupported()
    }

    pub fn kill(
        &self,
        _execution_id: &ExecutionId,
        _owner: &ExecutionOwner,
        _mode: KillMode,
    ) -> Result<()> {
        unsupported()
    }

    pub fn operator_kill(&self, _execution_id: &ExecutionId, _mode: KillMode) -> Result<()> {
        unsupported()
    }

    pub fn terminate_owner(&self, _owner: &ExecutionOwner) -> Result<usize> {
        unsupported()
    }

    pub fn terminate_grant(&self, _grant_id: &str) -> Result<usize> {
        unsupported()
    }

    pub fn expire_run_grants(&self, _creating_run_id: &RunId, _duration: &str) -> Result<usize> {
        unsupported()
    }

    pub fn terminate_inactive_grants(&self, _live_grant_ids: BTreeSet<String>) -> Result<usize> {
        unsupported()
    }

    pub fn wait(
        &self,
        _execution_id: &ExecutionId,
        _owner: &ExecutionOwner,
        _deadline: Option<Instant>,
        _cancelled: impl Fn() -> bool,
    ) -> Result<ExecutionStatus> {
        unsupported()
    }
}

fn unsupported<T>() -> Result<T> {
    Err(ProcessError::new(
        ProcessErrorCode::PlatformUnsupported,
        "process execution is not implemented on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use crate::{
        CommittedOutputFrame, ExecutionOutputChunk, InMemoryExecutionRepository, OutputSpool,
        ProcessSupervisorOptions,
    };

    use super::*;

    struct NoopSpool;

    impl OutputSpool for NoopSpool {
        fn prepare(&self, _execution_id: &ExecutionId) -> Result<()> {
            panic!("unsupported backend touched the output spool")
        }

        fn append(
            &self,
            _execution_id: &ExecutionId,
            _chunk: &ExecutionOutputChunk,
        ) -> Result<u64> {
            panic!("unsupported backend touched the output spool")
        }

        fn sync(&self, _execution_id: &ExecutionId) -> Result<()> {
            panic!("unsupported backend touched the output spool")
        }

        fn read(
            &self,
            _execution_id: &ExecutionId,
            _after_sequence: u64,
            _through_sequence: u64,
            _maximum_bytes: usize,
        ) -> Result<Vec<ExecutionOutputChunk>> {
            panic!("unsupported backend touched the output spool")
        }

        fn recover(
            &self,
            _execution_id: &ExecutionId,
            _committed: &[CommittedOutputFrame],
        ) -> Result<()> {
            panic!("unsupported backend touched the output spool")
        }

        fn remove(&self, _execution_id: &ExecutionId) -> Result<()> {
            panic!("unsupported backend touched the output spool")
        }
    }

    #[test]
    fn unsupported_backend_fails_before_creating_private_state() {
        let root =
            std::env::temp_dir().join(format!("agl-process-unsupported-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let options = ProcessSupervisorOptions {
            launcher_path: PathBuf::from("unsupported-launcher"),
            data_root: root.join("data"),
            state_root: root.join("state"),
            max_active: 1,
            command_capacity: 1,
            poll_interval: Duration::from_millis(1),
            setup_timeout: Duration::from_millis(1),
            termination_grace: Duration::from_millis(1),
            max_input_bytes: 1,
            max_result_bytes: 1,
            max_spool_bytes: 1,
            termination_output_headroom_bytes: 1,
            finished_retention: Duration::from_secs(1),
            runtime_read_only_roots: Vec::new(),
        };
        let error = match ProcessSupervisor::start(
            options,
            Arc::new(InMemoryExecutionRepository::new()),
            Arc::new(NoopSpool),
        ) {
            Ok(_) => panic!("unsupported backend unexpectedly started"),
            Err(error) => error,
        };

        assert_eq!(error.code(), ProcessErrorCode::PlatformUnsupported);
        assert!(!root.exists());
    }
}
