use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{ProcessError, ProcessErrorCode, Result};

pub const WRITABLE_INPUT_LEASE_TTL: Duration = Duration::from_secs(30);
pub const WRITABLE_INPUT_LEASE_HEARTBEAT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSupervisorOptions {
    pub launcher_path: PathBuf,
    pub data_root: PathBuf,
    pub state_root: PathBuf,
    pub max_active: usize,
    pub command_capacity: usize,
    pub poll_interval: Duration,
    pub setup_timeout: Duration,
    pub termination_grace: Duration,
    pub max_input_bytes: usize,
    pub max_result_bytes: usize,
    pub max_spool_bytes: u64,
    pub termination_output_headroom_bytes: u64,
    pub finished_retention: Duration,
    pub runtime_read_only_roots: Vec<PathBuf>,
}

impl ProcessSupervisorOptions {
    pub fn validate(&self) -> Result<()> {
        debug_assert!(WRITABLE_INPUT_LEASE_HEARTBEAT < WRITABLE_INPUT_LEASE_TTL);
        if self.launcher_path.as_os_str().is_empty() {
            return Err(ProcessError::new(
                ProcessErrorCode::LauncherUnavailable,
                "process launcher path must be configured",
            ));
        }
        if self.max_active == 0
            || self.command_capacity == 0
            || self.poll_interval.is_zero()
            || self.setup_timeout.is_zero()
            || self.termination_grace.is_zero()
            || self.max_input_bytes == 0
            || self.max_result_bytes == 0
            || self.max_spool_bytes == 0
            || self.termination_output_headroom_bytes == 0
            || self.finished_retention.is_zero()
        {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "process supervisor limits and durations must be nonzero",
            ));
        }
        for root in &self.runtime_read_only_roots {
            validate_canonical_directory(root, "configured process runtime read-only root")?;
        }
        Ok(())
    }

    pub(crate) fn admits_runtime_read_only_root(&self, requested: &Path) -> bool {
        self.runtime_read_only_roots
            .iter()
            .any(|configured| requested.starts_with(configured))
    }
}

fn validate_canonical_directory(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute() || path.as_os_str().to_string_lossy().contains('\0') {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            format!("{label} must be an absolute path containing no NUL"),
        ));
    }
    let canonical = path.canonicalize().map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            format!("{label} cannot be canonicalized: {error}"),
        )
    })?;
    if canonical != path || !path.is_dir() {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            format!("{label} must be an existing canonical directory"),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessPlatformDiagnostics {
    pub platform: String,
    pub supported: bool,
    pub launcher: bool,
    pub user_namespace: bool,
    pub pid_namespace: bool,
    pub mount_namespace: bool,
    pub network_namespace: bool,
    pub landlock_abi: Option<u32>,
    pub seccomp: bool,
    pub pidfd: bool,
    pub pty: bool,
    pub error_code: Option<String>,
    pub remediation: Option<String>,
}
