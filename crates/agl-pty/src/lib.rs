use serde::{Deserialize, Serialize};

pub const MAX_SHELL_INTEGRATION_FRAME_BYTES: usize = 80 * 1024;

#[cfg(test)]
mod test_support {
    use agl_exec::{
        CallerNamespace, CallerOwner, CallerOwnerId, CallerOwnerKind, CallerRole,
        CorrelationGroupId, CorrelationOperationId, ExecutionCorrelation, ExecutionOwner,
        ExecutionRequestId, LifecycleScopeId,
    };

    pub(crate) type RunId = ExecutionRequestId;
    pub(crate) type SessionId = ExecutionRequestId;
    pub(crate) type StepId = ExecutionRequestId;

    fn namespace() -> CallerNamespace {
        CallerNamespace::new("terminal-test", 1).expect("static caller namespace is valid")
    }

    pub(crate) fn session_owner(
        session_id: &SessionId,
        authority_id: &RunId,
        role: CallerRole,
    ) -> ExecutionOwner {
        ExecutionOwner::new(
            CallerOwner::new(
                namespace(),
                CallerOwnerId::new(session_id.as_str()).unwrap(),
                CallerOwnerKind::Persistent,
                role,
            ),
            LifecycleScopeId::new(authority_id.as_str()).unwrap(),
        )
    }

    pub(crate) fn run_owner(run_id: &RunId, authority_id: &RunId) -> ExecutionOwner {
        ExecutionOwner::new(
            CallerOwner::new(
                namespace(),
                CallerOwnerId::new(run_id.as_str()).unwrap(),
                CallerOwnerKind::Ephemeral,
                CallerRole::Agent,
            ),
            LifecycleScopeId::new(authority_id.as_str()).unwrap(),
        )
    }

    pub(crate) fn correlation(run_id: &RunId, step_id: &StepId) -> ExecutionCorrelation {
        ExecutionCorrelation::new(
            namespace(),
            CorrelationGroupId::new(run_id.as_str()).unwrap(),
            CorrelationOperationId::new(step_id.as_str()).unwrap(),
        )
    }
}

#[cfg(target_os = "linux")]
mod linux;
#[doc(hidden)]
pub mod platform;
mod private_environment;
#[cfg(target_os = "linux")]
mod runtime_roots;
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub mod wire;

#[cfg(target_os = "linux")]
pub use linux::{
    ShellIntegrationReceive, ShellIntegrationSocketPair, create_shell_integration_transport,
    interrupt_terminal_foreground, notify_terminal_resize, receive_shell_integration_event,
    run_shell_integration_relay, send_shell_integration_control, terminal_foreground_process_group,
};
#[doc(hidden)]
pub use platform::{
    LaunchDirectories, LaunchedProcess, diagnostics, launch, launcher_main,
    verify_launcher_binary_identity,
};
pub use private_environment::{
    MAX_PRIVATE_ENVIRONMENT_BYTES, MAX_PRIVATE_ENVIRONMENT_ENTRIES,
    MAX_PRIVATE_ENVIRONMENT_NAME_BYTES, MAX_PRIVATE_ENVIRONMENT_VALUE_BYTES,
    PrivateEnvironmentValue, PrivateLaunchEnvironment, zeroize_private_bytes,
};
#[cfg(target_os = "linux")]
pub use runtime_roots::{STANDARD_RUNTIME_ROOTS, standard_runtime_roots};

/// Portable capability report for the platform-specific process launcher.
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

#[cfg(test)]
mod tests {
    use super::ProcessPlatformDiagnostics;

    #[test]
    fn diagnostics_round_trip_and_reject_unknown_fields() {
        let diagnostics = ProcessPlatformDiagnostics {
            platform: "linux".to_owned(),
            supported: true,
            launcher: true,
            user_namespace: true,
            pid_namespace: true,
            mount_namespace: true,
            network_namespace: true,
            landlock_abi: Some(6),
            seccomp: true,
            pidfd: true,
            pty: true,
            error_code: None,
            remediation: None,
        };

        let mut value = serde_json::to_value(&diagnostics).expect("diagnostics serialize");
        assert_eq!(
            serde_json::from_value::<ProcessPlatformDiagnostics>(value.clone())
                .expect("diagnostics deserialize"),
            diagnostics
        );

        value
            .as_object_mut()
            .expect("diagnostics encode as an object")
            .insert("legacy_supported".to_owned(), serde_json::Value::Bool(true));
        assert!(serde_json::from_value::<ProcessPlatformDiagnostics>(value).is_err());
    }
}
