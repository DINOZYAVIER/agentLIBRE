use serde::{Deserialize, Serialize};

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
