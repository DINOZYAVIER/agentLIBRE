use std::collections::BTreeMap;
use std::fmt::{self, Debug, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use agl_exec::{ExecutionProfile, ProcessError, ProcessErrorCode, Result};

use crate::environment::PrivateTerminalEnvironment;
use crate::history::TerminalHistorySeed;
use crate::{AdmittedShellProfile, HostStartupPolicy, ShellIntegrationToken, ShellStartupPaths};

const PRIVATE_HOME_IN_SANDBOX: &str = "/.agl-private/home";

/// Complete private startup input for one managed shell. Debug output keeps
/// history commands and private environment values redacted by their types.
pub struct ManagedShellStartup {
    pub shell: AdmittedShellProfile,
    pub host_startup: HostStartupPolicy,
    pub history_seed: TerminalHistorySeed,
    pub integration_token: ShellIntegrationToken,
    pub private_environment: PrivateTerminalEnvironment,
}

pub struct ManagedShellIntegrationTransport {
    pub supervisor_socket: OwnedFd,
    pub relay_socket: Option<OwnedFd>,
    pub event_fifo_guard: OwnedFd,
    pub event_fifo_path: PathBuf,
    pub control_fifo_path: PathBuf,
}

pub struct MaterializedManagedShell {
    pub transport: ManagedShellIntegrationTransport,
    pub private_environment: PrivateTerminalEnvironment,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
}

impl Debug for ManagedShellStartup {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedShellStartup")
            .field("shell", &self.shell)
            .field("host_startup", &self.host_startup)
            .field("history_seed", &self.history_seed)
            .field("integration_token", &self.integration_token)
            .field("private_environment", &self.private_environment)
            .finish()
    }
}

impl ManagedShellStartup {
    pub fn materialize(
        self,
        profile: ExecutionProfile,
        private_home: &Path,
    ) -> Result<MaterializedManagedShell> {
        // Reject a non-conforming adapter before creating any private runtime state.
        self.shell.validate()?;
        self.host_startup.validate(profile)?;
        let private = private_home.join("agl-terminal");
        ensure_private_directory(&private)?;

        let visible_home = if profile == ExecutionProfile::Workspace {
            PathBuf::from(PRIVATE_HOME_IN_SANDBOX).join("agl-terminal")
        } else {
            private.clone()
        };
        let seed_host = private.join("history.seed");
        let seed_visible = visible_home.join("history.seed");
        let event_host = private.join("integration.events.fifo");
        let event_visible = visible_home.join("integration.events.fifo");
        let control_host = private.join("integration.controls.fifo");
        let control_visible = visible_home.join("integration.controls.fifo");
        let integration = agl_pty::create_shell_integration_transport(&event_host, &control_host)?;
        let plan = self.shell.render_startup(
            &self.host_startup,
            &self.history_seed,
            &ShellStartupPaths {
                startup_directory: visible_home,
                history_seed: seed_visible,
                event_fifo: event_visible,
                control_fifo: control_visible,
            },
            &self.integration_token,
            profile,
        )?;
        write_private_file(&private.join(plan.startup_name), plan.startup.as_bytes())?;
        write_private_file(&seed_host, &plan.history)?;

        let mut environment = plan.environment;
        environment.insert("HISTFILE".to_owned(), "/dev/null".to_owned());
        Ok(MaterializedManagedShell {
            transport: ManagedShellIntegrationTransport {
                supervisor_socket: integration.supervisor,
                relay_socket: Some(integration.relay),
                event_fifo_guard: integration.event_guard,
                event_fifo_path: event_host,
                control_fifo_path: control_host,
            },
            private_environment: self.private_environment,
            args: plan.args,
            environment,
        })
    }
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| shell_io("failed to protect managed shell directory", error))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(shell_io("failed to create managed shell directory", error));
        }
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| shell_io("failed to inspect managed shell directory", error))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "managed shell directory must be owned, private, and not a symlink",
        ));
    }
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| shell_io("failed to create managed shell file", error))?;
    validate_private_file(&file)?;
    file.write_all(bytes)
        .map_err(|error| shell_io("failed to write managed shell file", error))?;
    file.sync_all()
        .map_err(|error| shell_io("failed to sync managed shell file", error))
}

fn validate_private_file(file: &File) -> Result<()> {
    let metadata = file
        .metadata()
        .map_err(|error| shell_io("failed to inspect managed shell file", error))?;
    if !metadata.is_file()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
    {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "managed shell files must be owned private regular files",
        ));
    }
    Ok(())
}

fn shell_io(context: &str, error: std::io::Error) -> ProcessError {
    ProcessError::new(ProcessErrorCode::Internal, format!("{context}: {error}"))
}
