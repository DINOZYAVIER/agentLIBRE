use std::fmt::{self, Debug, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use crate::platform::LaunchDirectories;
use crate::terminal::environment::PrivateTerminalEnvironment;
use crate::terminal::history::TerminalHistorySeed;
use crate::{ExecutionProfile, ExecutionRequest, ProcessError, ProcessErrorCode, Result};

pub(crate) use agl_terminal::ShellIntegrationToken;
pub use agl_terminal::{
    AdmittedShellKind, AdmittedShellProfile, BoundedShellIntegration, CommandBoundary,
    HostStartupPolicy, IntegrationBatch, MAX_SHELL_INTEGRATION_FRAME_BYTES, ShellExit,
    ShellIntegrationControl, ShellIntegrationEvent, ShellIntegrationHealth, ShellIntegrationNotice,
    ShellIntegrationState, ShellStartupPaths, TerminalPromptState, TypedCommandAbortReason,
    TypedCommandTransactionId,
};

const PRIVATE_HOME_IN_SANDBOX: &str = "/.agl-private/home";

/// Private materialization input consumed only inside `ProcessSupervisor`
/// after it has allocated the execution-private home. Debug never renders the
/// history seed, whose exact commands may contain secrets.
pub(crate) struct ManagedShellStartup {
    pub shell: AdmittedShellProfile,
    pub host_startup: HostStartupPolicy,
    pub history_seed: TerminalHistorySeed,
    pub integration_token: ShellIntegrationToken,
    pub private_environment: PrivateTerminalEnvironment,
}

pub(crate) struct ManagedShellIntegrationTransport {
    pub supervisor_socket: OwnedFd,
    pub relay_socket: Option<OwnedFd>,
    pub event_fifo_guard: OwnedFd,
    pub event_fifo_path: PathBuf,
    pub control_fifo_path: PathBuf,
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
    pub(crate) fn materialize(
        self,
        request: &mut ExecutionRequest,
        directories: &LaunchDirectories,
    ) -> Result<(ManagedShellIntegrationTransport, PrivateTerminalEnvironment)> {
        // Reject a non-conforming adapter before creating any private runtime state.
        self.shell.validate()?;
        self.host_startup.validate(request.profile)?;
        let private = directories.private_home.join("agl-terminal");
        ensure_private_directory(&private)?;

        let visible_home = if request.profile == ExecutionProfile::Workspace {
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
        let integration =
            crate::platform::create_shell_integration_transport(&event_host, &control_host)?;
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
            request.profile,
        )?;
        write_private_file(&private.join(plan.startup_name), plan.startup.as_bytes())?;
        write_private_file(&seed_host, &plan.history)?;

        request.args = plan.args;
        request.environment.values.extend(plan.environment);
        request
            .environment
            .values
            .insert("HISTFILE".to_owned(), "/dev/null".to_owned());
        request.validate()?;
        Ok((
            ManagedShellIntegrationTransport {
                supervisor_socket: integration.supervisor,
                relay_socket: Some(integration.relay),
                event_fifo_guard: integration.event_guard,
                event_fifo_path: event_host,
                control_fifo_path: control_host,
            },
            self.private_environment,
        ))
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs::File;
    use std::io::Write as _;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::fs::FileTypeExt as _;
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use agl_ids::{RunId, SessionId, StepId};

    use super::*;
    use crate::{
        EnvironmentOverride, ExecutionAuthorization, ExecutionIo, ExecutionKind, ExecutionLimits,
        ExecutionOwner, ExecutionProfile, ShellProfileSnapshot,
    };

    fn snapshot(kind: AdmittedShellKind) -> AdmittedShellProfile {
        let executable = match kind {
            AdmittedShellKind::Bash => "/bin/bash",
            AdmittedShellKind::Zsh => "/bin/zsh",
        };
        AdmittedShellProfile {
            kind,
            snapshot: ShellProfileSnapshot {
                program: PathBuf::from(executable),
                command_args: vec!["-c".to_owned()],
                login_command_args: None,
                environment_names: vec!["PATH".to_owned()],
                executable_digest:
                    "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                        .to_owned(),
                config_digest: "sha256:test-shell".to_owned(),
            },
        }
    }

    fn request(workspace: &Path, shell: &AdmittedShellProfile) -> ExecutionRequest {
        ExecutionRequest {
            owner: ExecutionOwner::Session {
                session_id: SessionId::generate(),
                root_run_id: RunId::generate(),
            },
            creating_run_id: RunId::generate(),
            creating_step_id: StepId::generate(),
            kind: ExecutionKind::Shell,
            program: shell.snapshot.program.clone(),
            argv0: shell.snapshot.program.display().to_string(),
            program_digest: Some(shell.snapshot.executable_digest.clone()),
            args: Vec::new(),
            workspace_root: workspace.to_path_buf(),
            cwd: workspace.to_path_buf(),
            read_only_roots: Vec::new(),
            environment: EnvironmentOverride {
                values: BTreeMap::new(),
            },
            stdin: None,
            close_stdin_after_initial: false,
            io: ExecutionIo::Pty,
            terminal_size: Some(crate::TerminalSize::default()),
            profile: ExecutionProfile::Workspace,
            authorization: ExecutionAuthorization::default(),
            grant_lease: None,
            limits: ExecutionLimits {
                timeout_ms: None,
                max_input_bytes: 1024,
                max_output_bytes: 1024,
            },
        }
    }

    #[test]
    fn supervisor_materialization_uses_private_files_and_managed_non_login_args() {
        let base = std::env::temp_dir().join(format!(
            "agl-managed-shell-{}-{}",
            std::process::id(),
            RunId::generate()
        ));
        let workspace = base.join("workspace");
        let execution_root = base.join("execution");
        let private_home = execution_root.join("home");
        let private_tmp = execution_root.join("tmp");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&private_home).unwrap();
        fs::create_dir_all(&private_tmp).unwrap();
        fs::set_permissions(&private_home, fs::Permissions::from_mode(0o700)).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let directories = LaunchDirectories {
            execution_root,
            private_home: private_home.clone(),
            private_tmp,
        };
        let shell = snapshot(AdmittedShellKind::Bash);
        let startup = ManagedShellStartup {
            shell: shell.clone(),
            host_startup: HostStartupPolicy::ManagedOnly,
            history_seed: TerminalHistorySeed::from_commands(vec![
                "cd one".to_owned(),
                "printf 'two\\nlines'".to_owned(),
            ])
            .unwrap(),
            integration_token: ShellIntegrationToken::generate().unwrap(),
            private_environment: PrivateTerminalEnvironment::default(),
        };
        let mut request = request(&workspace, &shell);
        let (integration_transport, private_environment) =
            startup.materialize(&mut request, &directories).unwrap();
        assert!(private_environment.is_empty());

        assert_eq!(request.args[0], "--noprofile");
        assert_eq!(request.args[1], "--rcfile");
        assert_eq!(request.args[3], "-i");
        assert!(request.args[2].starts_with("/.agl-private/home/"));
        assert_eq!(request.environment.values["HISTFILE"], "/dev/null");
        let managed = private_home.join("agl-terminal");
        for name in ["bashrc", "history.seed"] {
            assert_eq!(
                fs::metadata(managed.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(
            fs::metadata(managed.join("integration.events.fifo"))
                .unwrap()
                .file_type()
                .is_fifo()
        );
        let rc = fs::read_to_string(managed.join("bashrc")).unwrap();
        assert!(!rc.contains(".bashrc"));
        assert!(rc.contains("history -r '/.agl-private/home/agl-terminal/history.seed'"));
        assert!(rc.contains("'/.agl-private/home/agl-terminal/integration.events.fifo'"));
        assert!(rc.contains("'/.agl-private/home/agl-terminal/integration.controls.fifo'"));
        drop(integration_transport);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn native_bash_hooks_emit_authenticated_boundaries_without_leaking_writer_fd() {
        let executable = find_shell("bash").expect("managed Bash is required on Linux");
        run_native_shell_integration(AdmittedShellKind::Bash, &executable);
    }

    #[test]
    fn native_zsh_hooks_emit_authenticated_boundaries_when_zsh_is_installed() {
        let Some(executable) = find_shell("zsh") else {
            return;
        };
        run_native_shell_integration(AdmittedShellKind::Zsh, &executable);
    }

    fn find_shell(name: &str) -> Option<PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|root| root.join(name))
            .find(|candidate| candidate.is_file())
            .and_then(|candidate| candidate.canonicalize().ok())
    }

    fn quote_shell_path(path: &Path) -> String {
        let value = path.as_os_str().to_string_lossy();
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    fn run_native_shell_integration(kind: AdmittedShellKind, executable: &Path) {
        let root = std::env::temp_dir().join(format!(
            "agl-native-managed-shell-{kind:?}-{}-{}",
            std::process::id(),
            RunId::generate()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let root = root.canonicalize().unwrap();
        let event_fifo = root.join("integration.events.fifo");
        let control_fifo = root.join("integration.controls.fifo");
        let transport =
            crate::platform::create_shell_integration_transport(&event_fifo, &control_fifo)
                .unwrap();
        let token = ShellIntegrationToken::generate().unwrap();
        let seed = root.join("history.seed");
        write_private_file(&seed, b"").unwrap();
        let shell = snapshot(kind);
        let plan = shell
            .render_startup(
                &HostStartupPolicy::ManagedOnly,
                &TerminalHistorySeed::empty(),
                &ShellStartupPaths {
                    startup_directory: root.clone(),
                    history_seed: seed.clone(),
                    event_fifo: event_fifo.clone(),
                    control_fifo: control_fifo.clone(),
                },
                &token,
                ExecutionProfile::Workspace,
            )
            .unwrap();
        let startup = root.join(plan.startup_name);
        write_private_file(&startup, plan.startup.as_bytes()).unwrap();

        let (mut master, slave) = open_test_pty();
        let relay_slave = slave.try_clone().unwrap();
        let relay_event_fifo = event_fifo.clone();
        let relay_control_fifo = control_fifo.clone();
        let relay = std::thread::spawn(move || {
            crate::platform::run_shell_integration_relay(
                transport.relay,
                relay_slave.as_raw_fd(),
                &relay_event_fifo,
                &relay_control_fifo,
                MAX_SHELL_INTEGRATION_FRAME_BYTES,
            )
        });
        let monitor_token = token.clone();
        let monitor = std::thread::spawn(move || {
            let mut integration = BoundedShellIntegration::new(monitor_token);
            let mut events = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                match crate::platform::receive_shell_integration_event(
                    &transport.supervisor,
                    MAX_SHELL_INTEGRATION_FRAME_BYTES,
                )
                .unwrap()
                {
                    crate::platform::ShellIntegrationReceive::Empty => {
                        if Instant::now() >= deadline {
                            panic!("managed {kind:?} integration monitor timed out");
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    crate::platform::ShellIntegrationReceive::Closed => break,
                    crate::platform::ShellIntegrationReceive::Event(frame) => {
                        let batch = integration.push_packet(&frame);
                        assert_eq!(batch.notice, None, "{kind:?} emitted an invalid frame");
                        if let Some(ShellIntegrationEvent::PromptReady {
                            sequence,
                            input_pending,
                            ..
                        }) = batch.events.first()
                        {
                            let shell_sequence = integration.last_shell_sequence().unwrap();
                            let control = integration
                                .encode_control(&ShellIntegrationControl::PromptReadyAck {
                                    event_sequence: shell_sequence,
                                    prompt_generation: (!input_pending).then_some(*sequence),
                                })
                                .unwrap();
                            crate::platform::send_shell_integration_control(
                                &transport.supervisor,
                                &control,
                                Duration::from_secs(1),
                            )
                            .unwrap();
                        }
                        events.extend(batch.events);
                    }
                }
            }
            (integration, events)
        });
        let slave_stdout = slave.try_clone().unwrap();
        let slave_stderr = slave.try_clone().unwrap();
        let mut command = Command::new(executable);
        match kind {
            AdmittedShellKind::Bash => {
                command
                    .arg("--noprofile")
                    .arg("--rcfile")
                    .arg(&startup)
                    .arg("-i");
            }
            AdmittedShellKind::Zsh => {
                command.arg("-d").arg("-i").env("ZDOTDIR", &root);
            }
        }
        command
            .env("HISTFILE", "/dev/null")
            .current_dir(&root)
            .stdin(Stdio::from(slave))
            .stdout(Stdio::from(slave_stdout))
            .stderr(Stdio::from(slave_stderr));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let shell_process_group = i32::try_from(child.id()).unwrap();
        let terminal_observer = OwnedFd::from(master.try_clone().unwrap());

        master.write_all(b"sleep 1\n").unwrap();
        master.flush().unwrap();
        let foreground_deadline = Instant::now() + Duration::from_secs(5);
        let observed_foreground = loop {
            match crate::platform::terminal_foreground_process_group(
                &terminal_observer,
                shell_process_group,
            ) {
                Ok(Some(process_group)) => break process_group,
                Ok(None) => {}
                Err(error) => panic!(
                    "managed {kind:?} foreground observation failed: {}",
                    error.message()
                ),
            }
            if Instant::now() >= foreground_deadline {
                let _ = child.kill();
                panic!("managed {kind:?} never transferred PTY foreground ownership");
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_ne!(observed_foreground, shell_process_group);
        let prompt_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match crate::platform::terminal_foreground_process_group(
                &terminal_observer,
                shell_process_group,
            ) {
                Ok(None) => break,
                Ok(Some(_)) => {}
                Err(error) => panic!(
                    "managed {kind:?} prompt foreground observation failed: {}",
                    error.message()
                ),
            }
            if Instant::now() >= prompt_deadline {
                let _ = child.kill();
                panic!("managed {kind:?} did not restore shell foreground ownership");
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        let child_probe = format!(
            "{} -c 'for __fd in /proc/self/fd/*; do __target=$(readlink \"$__fd\" 2>/dev/null); case \"$__target\" in *integration.*.fifo*) printf \"__AGL_%s__=%s\\n\" FD_LEAK \"$__target\";; esac; done; if [[ -n ${{__agl_integration_token+x}} ]]; then printf \"__AGL_%s__\\n\" TOKEN_ENV_LEAK; fi'",
            quote_shell_path(executable)
        );
        let long_command = format!(": '{}'", "x".repeat(9 * 1024));
        let commands = format!(
            "false\n\
             printf '__AGL_%s__=%s\\n' STATUS \"$?\"\n\
             printf 'one\\n' | tr o O\n\
             if true; then\n\
               printf '__AGL_%s__\\n' MULTI\n\
             fi\n\
             {child_probe}\n\
             {long_command}\n\
             printf 'AGL2\\0%s\\0%s\\0prompt_ready\\0%s\\0%s\\0%s\\0' fake 1 /pty-spoof - 0\n\
             rm -f -- {}; printf '__AGL_%s__\\n' ALIVE_AFTER_CHANNEL_LOSS\n",
            quote_shell_path(&event_fifo),
        );
        master.write_all(commands.as_bytes()).unwrap();
        master.write_all(b"exit\n").unwrap();
        master.flush().unwrap();
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK,) },
            0
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut terminal_output = Vec::new();
        let status = loop {
            drain_test_pty(master.as_raw_fd(), &mut terminal_output);
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("managed {kind:?} native integration fixture timed out");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        for _ in 0..20 {
            if drain_test_pty(master.as_raw_fd(), &mut terminal_output) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        drop(master);
        assert!(status.success(), "managed {kind:?} exited as {status:?}");
        assert!(
            terminal_output
                .windows(b"__AGL_STATUS__=1".len())
                .any(|window| window == b"__AGL_STATUS__=1"),
            "{kind:?} hook did not preserve the prior exit status: {}",
            String::from_utf8_lossy(&terminal_output)
        );
        assert!(
            terminal_output
                .windows(b"__AGL_MULTI__".len())
                .any(|window| window == b"__AGL_MULTI__")
        );
        assert!(
            terminal_output
                .windows(b"__AGL_ALIVE_AFTER_CHANNEL_LOSS__".len())
                .any(|window| window == b"__AGL_ALIVE_AFTER_CHANNEL_LOSS__")
        );
        assert!(
            !terminal_output
                .windows(b"__AGL_FD_LEAK__".len())
                .any(|window| window == b"__AGL_FD_LEAK__"),
            "{kind:?} leaked its integration writer: {}",
            String::from_utf8_lossy(&terminal_output)
        );
        assert!(
            !terminal_output
                .windows(b"__AGL_TOKEN_ENV_LEAK__".len())
                .any(|window| window == b"__AGL_TOKEN_ENV_LEAK__")
        );
        assert!(
            !terminal_output
                .windows(token.expose_to_managed_startup().len())
                .any(|window| window == token.expose_to_managed_startup().as_bytes())
        );
        assert!(!startup.exists());
        assert!(!seed.exists());
        let relay_status = relay.join().unwrap();
        assert_eq!(
            relay_status, 125,
            "relay must reject the replaced FIFO path"
        );
        let (mut integration, events) = monitor.join().unwrap();
        let starts = events
            .iter()
            .filter_map(|event| match event {
                ShellIntegrationEvent::CommandStarted { command, .. } => Some(command),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            starts
                .iter()
                .filter(|command| command.contains("printf 'one\\n' | tr o O"))
                .count(),
            1
        );
        assert_eq!(
            starts
                .iter()
                .filter(|command| command.as_str() == long_command)
                .count(),
            1,
            "managed {kind:?} hook truncated a command above the former 8 KiB limit"
        );
        assert_eq!(
            starts
                .iter()
                .filter(|command| command.contains("if true;") && command.contains("MULTI"))
                .count(),
            1
        );
        let false_position = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ShellIntegrationEvent::CommandStarted { command, .. }
                        if command.trim() == "false"
                )
            })
            .unwrap();
        assert!(matches!(
            events.get(false_position + 1),
            Some(ShellIntegrationEvent::CommandFinished {
                exit: ShellExit::Code { code: 1 },
                ..
            })
        ));
        assert!(matches!(
            events.get(false_position + 2),
            Some(ShellIntegrationEvent::PromptReady {
                last_exit: Some(1),
                ..
            })
        ));
        assert_eq!(integration.state().cwd(), Some(root.as_path()));
        let closed = integration.channel_closed();
        assert_eq!(closed.notice.unwrap().code, "shell_integration_degraded");
        assert_eq!(
            integration.state().health(),
            ShellIntegrationHealth::Degraded
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn open_test_pty() -> (File, File) {
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(
            unsafe { libc::tcgetattr(slave, attributes.as_mut_ptr()) },
            0
        );
        let mut attributes = unsafe { attributes.assume_init() };
        attributes.c_lflag &= !libc::ECHO;
        assert_eq!(
            unsafe { libc::tcsetattr(slave, libc::TCSANOW, &attributes) },
            0
        );
        unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
    }

    fn drain_test_pty(descriptor: i32, output: &mut Vec<u8>) -> bool {
        let mut buffer = [0u8; 4096];
        loop {
            let read = unsafe { libc::read(descriptor, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read > 0 {
                output.extend_from_slice(&buffer[..read as usize]);
                continue;
            }
            if read == 0 {
                return true;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EIO) {
                return true;
            }
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return false;
            }
            panic!("failed to read native shell PTY: {error}");
        }
    }
}
