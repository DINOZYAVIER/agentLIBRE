use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ExecutionProfile, ProcessError, ProcessErrorCode, Result, ShellProfileSnapshot};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContextSnapshot {
    pub workspace_root: PathBuf,
    pub working_directory: PathBuf,
    pub private_execution_roots: Vec<PathBuf>,
    pub shell: ShellProfileSnapshot,
    pub revision: u64,
    pub profile_metadata: String,
}

impl ExecutionContextSnapshot {
    pub fn validate(&self) -> Result<()> {
        validate_canonical_absolute(&self.workspace_root, "workspace root")?;
        validate_canonical_absolute(&self.working_directory, "working directory")?;
        for root in &self.private_execution_roots {
            validate_canonical_absolute(root, "private execution root")?;
        }
        self.shell.validate()?;
        if self.revision == 0 {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "execution context revision must be nonzero",
            ));
        }
        Ok(())
    }
}

pub fn resolve_execution_directory(
    snapshot: &ExecutionContextSnapshot,
    requested: &Path,
    profile: ExecutionProfile,
    host_authorized: bool,
) -> Result<PathBuf> {
    snapshot.validate()?;
    let text = requested.as_os_str().to_string_lossy();
    if text.is_empty() || text.contains('\0') {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "working directory must be nonempty and contain no NUL",
        ));
    }
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        snapshot.working_directory.join(requested)
    };
    let resolved = candidate.canonicalize().map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            format!("working directory cannot be canonicalized: {error}"),
        )
    })?;
    if !resolved.is_dir() {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "working directory must be an existing directory",
        ));
    }

    match profile {
        ExecutionProfile::Workspace => {
            let admitted = resolved.starts_with(&snapshot.workspace_root)
                || snapshot
                    .private_execution_roots
                    .iter()
                    .any(|root| resolved.starts_with(root));
            if !admitted {
                return Err(ProcessError::new(
                    ProcessErrorCode::HostAuthorityRequired,
                    "working directory is outside the workspace execution profile",
                ));
            }
        }
        ExecutionProfile::Host if !host_authorized => {
            return Err(ProcessError::new(
                ProcessErrorCode::HostAuthorityRequired,
                "host working directory requires admitted host_process_execution authority",
            ));
        }
        ExecutionProfile::Host => {}
    }
    Ok(resolved)
}

fn validate_canonical_absolute(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute() || path.as_os_str().to_string_lossy().contains('\0') {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            format!("{label} must be an absolute canonical path"),
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
    use std::fs;

    use super::*;

    fn snapshot(workspace: PathBuf, cwd: PathBuf) -> ExecutionContextSnapshot {
        ExecutionContextSnapshot {
            workspace_root: workspace,
            working_directory: cwd,
            private_execution_roots: Vec::new(),
            shell: ShellProfileSnapshot {
                program: PathBuf::from("/bin/sh"),
                command_args: vec!["-c".to_owned()],
                login_command_args: Some(vec!["-l".to_owned(), "-c".to_owned()]),
                environment_names: vec!["PATH".to_owned()],
                executable_digest: "sha256:shell".to_owned(),
                config_digest: "sha256:config".to_owned(),
            },
            revision: 1,
            profile_metadata: "workspace".to_owned(),
        }
    }

    #[test]
    fn relative_resolution_uses_logical_cwd_without_process_chdir() {
        let root = std::env::temp_dir().join(format!("agl-process-context-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/child")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        let root = root.canonicalize().unwrap();
        let before = std::env::current_dir().unwrap();
        let first = snapshot(root.clone(), root.join("a"));
        let second = snapshot(root.clone(), root.join("b"));

        assert_eq!(
            resolve_execution_directory(
                &first,
                Path::new("child"),
                ExecutionProfile::Workspace,
                false,
            )
            .unwrap(),
            root.join("a/child")
        );
        assert_eq!(
            resolve_execution_directory(
                &second,
                Path::new("."),
                ExecutionProfile::Workspace,
                false,
            )
            .unwrap(),
            root.join("b")
        );
        assert_eq!(std::env::current_dir().unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_profile_rejects_canonical_escape_and_host_requires_authority() {
        let base =
            std::env::temp_dir().join(format!("agl-process-context-policy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("workspace")).unwrap();
        fs::create_dir_all(base.join("outside")).unwrap();
        let workspace = base.join("workspace").canonicalize().unwrap();
        let outside = base.join("outside").canonicalize().unwrap();
        let snapshot = snapshot(workspace.clone(), workspace);

        assert_eq!(
            resolve_execution_directory(&snapshot, &outside, ExecutionProfile::Workspace, false,)
                .unwrap_err()
                .code(),
            ProcessErrorCode::HostAuthorityRequired
        );
        assert_eq!(
            resolve_execution_directory(&snapshot, &outside, ExecutionProfile::Host, false)
                .unwrap_err()
                .code(),
            ProcessErrorCode::HostAuthorityRequired
        );
        assert_eq!(
            resolve_execution_directory(&snapshot, &outside, ExecutionProfile::Host, true).unwrap(),
            outside
        );
        fs::remove_dir_all(base).unwrap();
    }
}
