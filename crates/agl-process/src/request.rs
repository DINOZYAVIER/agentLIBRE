use std::path::{Path, PathBuf};

use agl_ids::{RunId, SessionId, StepId};
use serde::{Deserialize, Serialize};

use crate::{ProcessBytes, ProcessError, ProcessErrorCode, Result};
pub use agl_exec::{
    EnvironmentOverride, ExecutionAuthorization, ExecutionGrantLease, ExecutionIo, ExecutionKind,
    ExecutionLeaseOrigin, ExecutionLimits, ExecutionProfile,
    LOCAL_OPERATOR_TERMINAL_LEASE_DURATION, ShellProfileSnapshot, TerminalSize,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOwner {
    Session {
        session_id: SessionId,
        root_run_id: RunId,
    },
    Run {
        run_id: RunId,
        root_run_id: RunId,
    },
}

impl ExecutionOwner {
    pub fn may_access(&self, requester: &Self) -> bool {
        match (self, requester) {
            (
                Self::Session { session_id, .. },
                Self::Session {
                    session_id: requester,
                    ..
                },
            ) => session_id == requester,
            (
                Self::Run { run_id, .. },
                Self::Run {
                    run_id: requester, ..
                },
            ) => run_id == requester,
            _ => false,
        }
    }

    pub fn root_run_id(&self) -> &RunId {
        match self {
            Self::Session { root_run_id, .. } | Self::Run { root_run_id, .. } => root_run_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRequest {
    pub owner: ExecutionOwner,
    pub creating_run_id: RunId,
    pub creating_step_id: StepId,
    pub kind: ExecutionKind,
    pub program: PathBuf,
    pub argv0: String,
    pub program_digest: Option<String>,
    pub args: Vec<String>,
    pub workspace_root: PathBuf,
    pub cwd: PathBuf,
    pub read_only_roots: Vec<PathBuf>,
    pub environment: EnvironmentOverride,
    pub stdin: Option<ProcessBytes>,
    pub close_stdin_after_initial: bool,
    pub io: ExecutionIo,
    pub terminal_size: Option<TerminalSize>,
    pub profile: ExecutionProfile,
    pub authorization: ExecutionAuthorization,
    pub grant_lease: Option<ExecutionGrantLease>,
    pub limits: ExecutionLimits,
}

impl ExecutionRequest {
    pub fn validate(&self) -> Result<()> {
        validate_program(&self.program)?;
        if self.argv0.is_empty() || self.argv0.contains('\0') {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "process argv0 must be nonempty and contain no NUL",
            ));
        }
        match (self.kind, self.program_digest.as_deref()) {
            (ExecutionKind::Shell, Some(digest)) => validate_sha256_digest(digest)?,
            (ExecutionKind::Shell, None) => {
                return Err(ProcessError::new(
                    ProcessErrorCode::InvalidRequest,
                    "shell execution requires its frozen executable digest",
                ));
            }
            (ExecutionKind::Argv, Some(_)) => {
                return Err(ProcessError::new(
                    ProcessErrorCode::InvalidRequest,
                    "exact argv execution must not carry a shell executable digest",
                ));
            }
            (ExecutionKind::Argv, None) => {}
        }
        validate_argv(&self.args)?;
        validate_canonical_directory_path(&self.workspace_root, "workspace root")?;
        validate_canonical_directory_path(&self.cwd, "working directory")?;
        for root in &self.read_only_roots {
            validate_canonical_path(root, "read-only runtime root")?;
        }
        self.environment.validate()?;
        self.limits.validate()?;
        if self.profile == ExecutionProfile::Host && !self.authorization.host_process_execution {
            return Err(ProcessError::new(
                ProcessErrorCode::HostAuthorityRequired,
                "host execution requires admitted host_process_execution authority",
            ));
        }
        if self.authorization.shell_login_startup
            && (!self.authorization.host_process_execution
                || self.profile != ExecutionProfile::Host)
        {
            return Err(ProcessError::new(
                ProcessErrorCode::LoginAuthorityRequired,
                "login startup requires an authorized host execution profile",
            ));
        }
        if let Some(lease) = &self.grant_lease {
            lease.validate()?;
        }
        if self.profile == ExecutionProfile::Host && self.grant_lease.is_none() {
            return Err(ProcessError::new(
                ProcessErrorCode::HostAuthorityRequired,
                "host execution requires a captured grant lease",
            ));
        }
        if let Some(stdin) = &self.stdin {
            let maximum = usize::try_from(self.limits.max_input_bytes).map_err(|_| {
                ProcessError::new(
                    ProcessErrorCode::InvalidRequest,
                    "execution input limit does not fit this platform",
                )
            })?;
            stdin.decode(maximum)?;
        }
        match (self.io, self.terminal_size) {
            (ExecutionIo::Pipes, Some(_)) => {
                return Err(ProcessError::new(
                    ProcessErrorCode::IoModeMismatch,
                    "terminal size is valid only for PTY execution",
                ));
            }
            (ExecutionIo::Pty, size) => {
                size.unwrap_or_default().validate()?;
            }
            (ExecutionIo::Pipes, None) => {}
        }
        Ok(())
    }
}

fn validate_program(program: &Path) -> Result<()> {
    let value = program.as_os_str().to_string_lossy();
    if value.is_empty() || value.contains('\0') || !program.is_absolute() {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "admitted program must be an absolute path containing no NUL",
        ));
    }
    Ok(())
}

fn validate_argv(args: &[String]) -> Result<()> {
    if args.iter().any(|argument| argument.contains('\0')) {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "process arguments must contain no NUL",
        ));
    }
    Ok(())
}

fn validate_sha256_digest(digest: &str) -> Result<()> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "shell executable digest must use the sha256 format",
        ));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "shell executable digest must contain 64 lowercase hexadecimal digits",
        ));
    }
    Ok(())
}

fn validate_canonical_directory_path(path: &Path, label: &str) -> Result<()> {
    validate_canonical_path(path, label)?;
    if !path.is_dir() {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            format!("{label} must be an existing directory"),
        ));
    }
    Ok(())
}

fn validate_canonical_path(path: &Path, label: &str) -> Result<()> {
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
    if canonical != path {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            format!("{label} must already be canonical"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use agl_exec::ExecutionId;

    use super::*;

    fn owner() -> ExecutionOwner {
        ExecutionOwner::Run {
            run_id: RunId::generate(),
            root_run_id: RunId::generate(),
        }
    }

    fn workspace() -> PathBuf {
        std::env::temp_dir().canonicalize().unwrap()
    }

    fn workspace_lease() -> Option<ExecutionGrantLease> {
        None
    }

    #[test]
    fn owner_access_is_exact_and_never_uses_execution_identity_as_pid_authority() {
        let execution_owner = owner();
        assert!(execution_owner.may_access(&execution_owner));
        assert!(!execution_owner.may_access(&owner()));
        assert!(ExecutionId::generate().as_str().starts_with("exec_"));
    }

    #[test]
    fn exact_argv_metacharacters_are_accepted_as_data() {
        let request = ExecutionRequest {
            owner: owner(),
            creating_run_id: RunId::generate(),
            creating_step_id: StepId::generate(),
            kind: ExecutionKind::Argv,
            program: PathBuf::from("/bin/echo"),
            argv0: "/bin/echo".to_owned(),
            program_digest: None,
            args: vec![
                "space value".to_owned(),
                ";".to_owned(),
                "|".to_owned(),
                "$()".to_owned(),
                "*.rs".to_owned(),
            ],
            workspace_root: workspace(),
            cwd: workspace(),
            read_only_roots: Vec::new(),
            environment: EnvironmentOverride {
                values: BTreeMap::new(),
            },
            stdin: None,
            close_stdin_after_initial: true,
            io: ExecutionIo::Pipes,
            terminal_size: None,
            profile: ExecutionProfile::Workspace,
            authorization: ExecutionAuthorization::default(),
            grant_lease: workspace_lease(),
            limits: ExecutionLimits {
                timeout_ms: Some(1_000),
                max_input_bytes: 65_536,
                max_output_bytes: 65_536,
            },
        };

        request.validate().unwrap();
        assert_eq!(request.args[3], "$()");

        let mut noncanonical = request.clone();
        noncanonical.cwd = request
            .cwd
            .join("..")
            .join(request.cwd.file_name().unwrap());
        assert_eq!(
            noncanonical.validate().unwrap_err().code(),
            ProcessErrorCode::InvalidRequest
        );
    }

    #[cfg(unix)]
    #[test]
    fn shell_snapshot_keeps_canonical_target_and_rejects_identity_replacement() {
        use std::fmt::Write as _;
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        use sha2::{Digest as _, Sha256};

        let root = std::env::temp_dir().join(format!(
            "agl-shell-snapshot-{}-{}",
            std::process::id(),
            ExecutionId::generate()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let first = root.join("first-shell");
        let second = root.join("second-shell");
        let configured = root.join("configured-shell");
        std::fs::write(&first, b"first admitted executable").unwrap();
        std::fs::write(&second, b"second executable").unwrap();
        std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&second, std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&first, &configured).unwrap();

        let mut executable_digest = String::from("sha256:");
        for byte in Sha256::digest(std::fs::read(&first).unwrap()) {
            write!(&mut executable_digest, "{byte:02x}").unwrap();
        }
        let snapshot = ShellProfileSnapshot {
            program: configured.canonicalize().unwrap(),
            command_args: vec!["-c".to_owned()],
            login_command_args: None,
            environment_names: vec!["PATH".to_owned()],
            executable_digest,
            config_digest: "sha256:test-config".to_owned(),
        };

        std::fs::remove_file(&configured).unwrap();
        symlink(&second, &configured).unwrap();
        snapshot.verify_executable().unwrap();

        std::fs::write(&first, b"replaced executable").unwrap();
        assert_eq!(
            snapshot.verify_executable().unwrap_err().code(),
            ProcessErrorCode::SandboxExecutableUnavailable
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shell_snapshot_rejects_invalid_environment_names() {
        let snapshot = ShellProfileSnapshot {
            program: PathBuf::from("/bin/sh"),
            command_args: vec!["-c".to_owned()],
            login_command_args: None,
            environment_names: vec!["LD\0PRELOAD".to_owned()],
            executable_digest: "sha256:test-shell".to_owned(),
            config_digest: "sha256:test-config".to_owned(),
        };

        assert_eq!(
            snapshot.validate().unwrap_err().code(),
            ProcessErrorCode::InvalidRequest
        );
    }

    #[test]
    fn terminal_size_is_rejected_for_pipes() {
        let mut request = ExecutionRequest {
            owner: owner(),
            creating_run_id: RunId::generate(),
            creating_step_id: StepId::generate(),
            kind: ExecutionKind::Argv,
            program: PathBuf::from("/bin/echo"),
            argv0: "/bin/echo".to_owned(),
            program_digest: None,
            args: Vec::new(),
            workspace_root: workspace(),
            cwd: workspace(),
            read_only_roots: Vec::new(),
            environment: EnvironmentOverride {
                values: BTreeMap::new(),
            },
            stdin: None,
            close_stdin_after_initial: false,
            io: ExecutionIo::Pipes,
            terminal_size: Some(TerminalSize::default()),
            profile: ExecutionProfile::Workspace,
            authorization: ExecutionAuthorization::default(),
            grant_lease: workspace_lease(),
            limits: ExecutionLimits {
                timeout_ms: None,
                max_input_bytes: 1,
                max_output_bytes: 1,
            },
        };
        assert_eq!(
            request.validate().unwrap_err().code(),
            ProcessErrorCode::IoModeMismatch
        );

        request.io = ExecutionIo::Pty;
        request.terminal_size = Some(TerminalSize {
            columns: 0,
            rows: 24,
        });
        assert_eq!(
            request.validate().unwrap_err().code(),
            ProcessErrorCode::InvalidTerminalSize
        );
    }

    #[test]
    fn host_profile_requires_explicit_typed_authority() {
        let mut request = ExecutionRequest {
            owner: owner(),
            creating_run_id: RunId::generate(),
            creating_step_id: StepId::generate(),
            kind: ExecutionKind::Argv,
            program: PathBuf::from("/bin/echo"),
            argv0: "/bin/echo".to_owned(),
            program_digest: None,
            args: Vec::new(),
            workspace_root: workspace(),
            cwd: workspace(),
            read_only_roots: Vec::new(),
            environment: EnvironmentOverride {
                values: BTreeMap::new(),
            },
            stdin: None,
            close_stdin_after_initial: false,
            io: ExecutionIo::Pipes,
            terminal_size: None,
            profile: ExecutionProfile::Host,
            authorization: ExecutionAuthorization::default(),
            grant_lease: None,
            limits: ExecutionLimits {
                timeout_ms: None,
                max_input_bytes: 1,
                max_output_bytes: 1,
            },
        };
        assert_eq!(
            request.validate().unwrap_err().code(),
            ProcessErrorCode::HostAuthorityRequired
        );

        request.authorization.host_process_execution = true;
        request.grant_lease = Some(ExecutionGrantLease {
            origin: ExecutionLeaseOrigin::ToolGrant,
            grant_id: "grant-test".to_owned(),
            duration: "one_turn".to_owned(),
            scope_digest: "sha256:test".to_owned(),
        });
        request.validate().unwrap();
    }

    #[test]
    fn authority_lease_origin_is_explicit_and_local_operator_duration_is_fixed() {
        let local = ExecutionGrantLease {
            origin: ExecutionLeaseOrigin::LocalOperatorTerminal,
            grant_id: "private-operator-authority".to_owned(),
            duration: LOCAL_OPERATOR_TERMINAL_LEASE_DURATION.to_owned(),
            scope_digest: "sha256:terminal-scope".to_owned(),
        };
        local.validate().unwrap();
        assert!(!local.is_capability_grant());

        let encoded = serde_json::to_value(&local).unwrap();
        assert_eq!(encoded["origin"], "local_operator_terminal");
        assert_eq!(
            serde_json::from_value::<ExecutionGrantLease>(encoded).unwrap(),
            local
        );

        let mut wrong_duration = local;
        wrong_duration.duration = "session".to_owned();
        assert_eq!(
            wrong_duration.validate().unwrap_err().code(),
            ProcessErrorCode::InvalidRequest
        );
    }
}
