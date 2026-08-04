use std::path::Path;

use crate::ProcessPlatformDiagnostics;
use agl_exec::ProcessErrorCode;

pub(crate) fn diagnostics(_launcher_path: &Path) -> ProcessPlatformDiagnostics {
    ProcessPlatformDiagnostics {
        platform: std::env::consts::OS.to_owned(),
        supported: false,
        launcher: false,
        user_namespace: false,
        pid_namespace: false,
        mount_namespace: false,
        network_namespace: false,
        landlock_abi: None,
        seccomp: false,
        pidfd: false,
        pty: false,
        error_code: Some(ProcessErrorCode::PlatformUnsupported.as_str().to_owned()),
        remediation: Some("run process execution on the supported Linux backend".to_owned()),
    }
}

pub(crate) fn launcher_main() -> i32 {
    eprintln!("platform_unsupported: process launcher is Linux-only");
    78
}
