use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use agl_ids::ExecutionId;
#[cfg(target_os = "linux")]
use serde::{Deserialize, Serialize};

use crate::ProcessPlatformDiagnostics;
#[cfg(target_os = "linux")]
use crate::{ExecutionIo, ExecutionRequest, Result};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod unsupported;

#[cfg(target_os = "linux")]
pub(crate) struct LaunchDirectories {
    pub execution_root: PathBuf,
    pub private_home: PathBuf,
    pub private_tmp: PathBuf,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LauncherRequest {
    execution_id: ExecutionId,
    request: ExecutionRequest,
    execution_root: PathBuf,
    private_home: PathBuf,
    private_tmp: PathBuf,
    setup_timeout_ms: u64,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LauncherResponse {
    ok: bool,
    io: Option<ExecutionIo>,
    error_code: Option<String>,
    message: Option<String>,
}

#[cfg(target_os = "linux")]
pub(crate) use linux::LaunchedProcess;

#[cfg(target_os = "linux")]
pub(crate) fn launch(
    launcher_path: &Path,
    execution_id: &ExecutionId,
    request: &ExecutionRequest,
    directories: &LaunchDirectories,
    setup_timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<LaunchedProcess> {
    let wire = LauncherRequest {
        execution_id: execution_id.clone(),
        request: request.clone(),
        execution_root: directories.execution_root.clone(),
        private_home: directories.private_home.clone(),
        private_tmp: directories.private_tmp.clone(),
        setup_timeout_ms: u64::try_from(setup_timeout.as_millis()).unwrap_or(u64::MAX),
    };
    linux::launch(launcher_path, &wire, cancelled)
}

pub fn diagnostics(launcher_path: &Path) -> ProcessPlatformDiagnostics {
    platform_diagnostics(launcher_path)
}

#[cfg(target_os = "linux")]
fn platform_diagnostics(launcher_path: &Path) -> ProcessPlatformDiagnostics {
    linux::diagnostics(launcher_path)
}

#[cfg(not(target_os = "linux"))]
fn platform_diagnostics(launcher_path: &Path) -> ProcessPlatformDiagnostics {
    unsupported::diagnostics(launcher_path)
}

#[doc(hidden)]
pub fn launcher_main() -> i32 {
    #[cfg(target_os = "linux")]
    {
        linux::launcher_main()
    }
    #[cfg(not(target_os = "linux"))]
    {
        unsupported::launcher_main()
    }
}
