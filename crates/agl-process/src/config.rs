use serde::{Deserialize, Serialize};

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
