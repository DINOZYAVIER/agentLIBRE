#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use agl_exec::ExecutionId;
#[cfg(target_os = "linux")]
use serde::{Deserialize, Serialize};

use crate::{PrivateLaunchEnvironment, ProcessPlatformDiagnostics};
#[cfg(target_os = "linux")]
use agl_exec::{ExecutionIo, ExecutionRequest, ProcessError, ProcessErrorCode, Result};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod unsupported;

#[cfg(target_os = "linux")]
const LAUNCHER_PROTOCOL_VERSION: &str =
    concat!("agl-process-launcher.v2/", env!("CARGO_PKG_VERSION"));
#[cfg(target_os = "linux")]
const LAUNCHER_BUILD_ID: &str = env!("AGL_PROCESS_BUILD_ID");

#[cfg(target_os = "linux")]
#[doc(hidden)]
pub struct LaunchDirectories {
    pub execution_root: PathBuf,
    pub private_home: PathBuf,
    pub private_tmp: PathBuf,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LauncherRequest {
    protocol_version: String,
    build_id: String,
    execution_id: ExecutionId,
    request: ExecutionRequest,
    execution_root: PathBuf,
    private_home: PathBuf,
    private_tmp: PathBuf,
    has_private_environment: bool,
    has_shell_integration: bool,
    setup_timeout_ms: u64,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LauncherResponse {
    protocol_version: String,
    build_id: String,
    ok: bool,
    io: Option<ExecutionIo>,
    error_code: Option<String>,
    message: Option<String>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LauncherDiagnosticsEnvelope {
    protocol_version: String,
    build_id: String,
    diagnostics: ProcessPlatformDiagnostics,
}

#[cfg(target_os = "linux")]
impl LauncherRequest {
    fn validate_launcher_identity(&self) -> Result<()> {
        validate_launcher_identity(&self.protocol_version, &self.build_id)
    }
}

#[cfg(target_os = "linux")]
impl LauncherResponse {
    fn validate_launcher_identity(&self) -> Result<()> {
        validate_launcher_identity(&self.protocol_version, &self.build_id)
    }
}

#[cfg(target_os = "linux")]
impl LauncherDiagnosticsEnvelope {
    fn current(diagnostics: ProcessPlatformDiagnostics) -> Self {
        Self {
            protocol_version: LAUNCHER_PROTOCOL_VERSION.to_owned(),
            build_id: LAUNCHER_BUILD_ID.to_owned(),
            diagnostics,
        }
    }

    fn validate_identity(&self) -> Result<()> {
        validate_launcher_identity(&self.protocol_version, &self.build_id)
    }
}

#[cfg(target_os = "linux")]
fn validate_launcher_identity(protocol_version: &str, build_id: &str) -> Result<()> {
    if protocol_version != LAUNCHER_PROTOCOL_VERSION {
        return Err(ProcessError::new(
            ProcessErrorCode::LauncherProtocol,
            format!(
                "process launcher protocol mismatch: expected {LAUNCHER_PROTOCOL_VERSION}, received {protocol_version}"
            ),
        ));
    }
    if build_id != LAUNCHER_BUILD_ID {
        return Err(ProcessError::new(
            ProcessErrorCode::LauncherProtocol,
            format!(
                "process launcher build identity mismatch: expected {LAUNCHER_BUILD_ID}, received {build_id}"
            ),
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub use linux::LaunchedProcess;

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
#[doc(hidden)]
pub fn launch(
    launcher_path: &Path,
    execution_id: &ExecutionId,
    request: &ExecutionRequest,
    directories: &LaunchDirectories,
    private_environment: Option<PrivateLaunchEnvironment>,
    shell_integration_relay: Option<OwnedFd>,
    setup_timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<LaunchedProcess> {
    let has_private_environment = private_environment
        .as_ref()
        .is_some_and(|environment| !environment.is_empty());
    let wire = LauncherRequest {
        protocol_version: LAUNCHER_PROTOCOL_VERSION.to_owned(),
        build_id: LAUNCHER_BUILD_ID.to_owned(),
        execution_id: execution_id.clone(),
        request: request.clone(),
        execution_root: directories.execution_root.clone(),
        private_home: directories.private_home.clone(),
        private_tmp: directories.private_tmp.clone(),
        has_private_environment,
        has_shell_integration: shell_integration_relay.is_some(),
        setup_timeout_ms: u64::try_from(setup_timeout.as_millis()).unwrap_or(u64::MAX),
    };
    linux::launch(
        launcher_path,
        &wire,
        private_environment,
        shell_integration_relay,
        cancelled,
    )
}

pub fn diagnostics(launcher_path: &Path) -> ProcessPlatformDiagnostics {
    platform_diagnostics(launcher_path)
}

pub fn verify_launcher_binary_identity(launcher_path: &Path) -> Result<()> {
    platform_verify_launcher_binary_identity(launcher_path)
}

#[cfg(target_os = "linux")]
fn platform_diagnostics(launcher_path: &Path) -> ProcessPlatformDiagnostics {
    linux::diagnostics(launcher_path)
}

#[cfg(target_os = "linux")]
fn platform_verify_launcher_binary_identity(launcher_path: &Path) -> Result<()> {
    linux::verify_launcher_identity(launcher_path)
}

#[cfg(not(target_os = "linux"))]
fn platform_diagnostics(launcher_path: &Path) -> ProcessPlatformDiagnostics {
    unsupported::diagnostics(launcher_path)
}

#[cfg(not(target_os = "linux"))]
fn platform_verify_launcher_binary_identity(_launcher_path: &Path) -> Result<()> {
    Err(ProcessError::new(
        ProcessErrorCode::PlatformUnsupported,
        "process launcher identity verification is supported only on Linux",
    ))
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
