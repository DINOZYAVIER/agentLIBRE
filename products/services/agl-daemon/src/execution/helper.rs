use super::*;
use agl_core::Fsm;
use std::os::unix::process::CommandExt;

pub fn run_helper(mode: &str, path: &Path) -> Result<()> {
    if mode == "session-closed"
        && (path
            .file_name()
            .is_none_or(|name| ExecutionId::parse(&name.to_string_lossy()).is_err())
            || !path.join("launch.json").try_exists()?)
    {
        return Ok(());
    }
    let launch: Launch = read_json(&path.join("launch.json"))?;
    let result = match mode {
        "session-closed" => {
            if path.join("outcome.json").exists() || path.join("result.json").exists() {
                return Ok(());
            }
            let manager = &launch.systemctl;
            let state = Command::new(manager)
                .args([
                    "--user",
                    "show",
                    "-p",
                    "ActiveState",
                    "--value",
                    &launch.unit,
                ])
                .output()?;
            if state.status.success()
                && matches!(
                    String::from_utf8_lossy(&state.stdout).trim(),
                    "active" | "activating"
                )
            {
                if !path.join("stop.json").exists() {
                    atomic_json(&path.join("stop.json"), &ExecutionStopCause::TmuxLost)?;
                }
                checked(Command::new(manager).args([
                    "--user",
                    "stop",
                    "--no-block",
                    &launch.unit,
                ]))?;
            }
            Ok(())
        }
        "launch" => exec_payload(path, launch),
        "terminal" => {
            let tty = fs::read_link("/proc/self/fd/0")?;
            ensure!(
                tty.starts_with("/dev/pts"),
                "terminal helper has no tmux PTY"
            );
            // tmux already allocated the PTY. Detach the short-lived launcher
            // session so the user manager can acquire this same slave without
            // privileged stealing; systemd supplies the payload's session.
            unsafe {
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
            }
            ensure!(
                unsafe { libc::ioctl(0, libc::TIOCNOTTY) } == 0,
                "cannot release tmux controlling TTY: {}",
                std::io::Error::last_os_error()
            );
            let error = run_command(&launch, path, Some(&tty)).exec();
            Err(error.into())
        }
        "capture-command" => {
            let limit = launch.request.max_output_bytes;
            // Open both readers before spawning threads. A failed open cannot
            // strand another thread and the parent at an unreachable barrier.
            let inputs = ["stdout", "stderr"].map(|stream| -> Result<_> {
                let input = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(path.join(format!("{stream}.fifo")))?;
                let fd = std::os::fd::AsRawFd::as_raw_fd(&input);
                ensure!(
                    unsafe { libc::fcntl(fd, libc::F_SETFL, 0) } == 0,
                    "cannot enable blocking capture"
                );
                Ok((stream, input))
            });
            let [stdout, stderr] = inputs;
            let inputs = [stdout?, stderr?];
            let handles = inputs.map(|(stream, input)| {
                let path = path.to_owned();
                std::thread::spawn(move || -> Result<()> {
                    while !path.join("payload-ready.json").exists()
                        && !path.join("outcome.json").exists()
                    {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    capture(&path, stream, input, limit)
                })
            });
            atomic_json(&path.join("capture-ready.json"), &true)?;
            for handle in handles {
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("capture thread panicked"))??;
            }
            Ok(())
        }
        "capture-terminal" => {
            File::create(path.join("terminal"))?
                .set_permissions(fs::Permissions::from_mode(0o600))?;
            atomic_json(&path.join("capture-ready.json"), &true)?;
            capture(
                path,
                "terminal",
                std::io::stdin().lock(),
                launch.request.max_output_bytes,
            )?;
            publish_result(path, &launch)
        }
        "outcome" => {
            let outcome = match (
                std::env::var("EXIT_CODE").as_deref(),
                std::env::var("EXIT_STATUS"),
            ) {
                (Ok("exited"), Ok(value)) => value
                    .parse()
                    .ok()
                    .map(|code| ExecutionOutcome::Exit { code }),
                (Ok("killed" | "dumped"), Ok(value)) => {
                    signal_number(&value).map(|signal| ExecutionOutcome::Signal { signal })
                }
                _ => None,
            };
            atomic_json(
                &path.join("outcome.json"),
                &MainOutcome {
                    outcome,
                    result: std::env::var("SERVICE_RESULT").unwrap_or_default(),
                },
            )?;
            if launch.request.io == ExecutionIo::Pipes {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !(path.join("stdout.json").exists() && path.join("stderr.json").exists())
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                publish_result(path, &launch)?;
            }
            // Planner scratch belongs to the unit, including failed startup.
            let scratch = path
                .parent()
                .unwrap()
                .join("planner-scratch")
                .join(path.file_name().unwrap());
            if scratch.exists() {
                fs::remove_dir_all(scratch)?;
            }
            Ok(())
        }
        _ => anyhow::bail!("invalid execution helper mode"),
    };
    if mode == "launch"
        && let Err(error) = &result
    {
        // The manager observes this helper's exit on preparation/exec failure.
        // It is not evidence that the requested payload returned that code.
        atomic_json(&path.join("launch-error.json"), &error.to_string())?;
    }
    result
}
fn exec_payload(path: &Path, launch: Launch) -> Result<()> {
    ensure_session(&launch, path)?;
    ensure!(
        read_json::<ExecutionStopCause>(&path.join("stop.json")).ok()
            != Some(ExecutionStopCause::TmuxLost),
        "tmux session was lost before payload preparation"
    );
    let mut command = Command::new(&launch.request.argv[0]);
    command
        .args(&launch.request.argv[1..])
        .current_dir(&launch.request.cwd);
    if launch.request.clear_environment {
        command.env_clear();
    }
    command.envs(&launch.request.environment);
    if launch.request.io == ExecutionIo::Pty {
        command.env(
            "TERM",
            std::env::var("TERM").unwrap_or_else(|_| "screen".into()),
        );
    }
    if launch.request.io == ExecutionIo::Pipes {
        if let Some(input) = &launch.input_fd_path {
            let file = File::open(input)?;
            ensure!(file.metadata()?.is_file(), "Forge stdin FD is not regular");
            command.stdin(file);
        } else if let Some(input) = &launch.input_file {
            command.stdin(open_regular(input)?);
        } else {
            command.stdin(Stdio::null());
        }
        let capture_output = OpenOptions::new()
            .write(true)
            .open(path.join("stdout.fifo"))?;
        if let Some(output) = &launch.output_fd_path {
            let file = OpenOptions::new().write(true).open(output)?;
            ensure!(file.metadata()?.is_file(), "Forge stdout FD is not regular");
            command.stdout(file);
            // Open the capture FIFO writer once so its reader can complete;
            // payload data goes only to the trusted caller's open file.
            drop(capture_output);
        } else {
            command.stdout(capture_output);
        }
        command.stderr(
            OpenOptions::new()
                .write(true)
                .open(path.join("stderr.fifo"))?,
        );
    }
    atomic_json(&path.join("payload-ready.json"), &true)?;
    while !path.join("capture-ready.json").exists() {
        std::thread::sleep(Duration::from_millis(5));
    }
    ensure!(
        launch
            .deadline_at_ms
            .is_none_or(|deadline| now_ms() < deadline),
        "execution deadline expired before payload exec"
    );
    ensure!(
        read_json::<ExecutionStopCause>(&path.join("stop.json")).ok()
            != Some(ExecutionStopCause::TmuxLost),
        "tmux session was lost before payload exec"
    );
    ensure_session(&launch, path)?;
    if let Some(private) = launch.private {
        return super::private::exec(command, &launch.request, &private, path);
    }
    if let ExecutionIsolation::ReadOnlyWorkspace { workspace_root, .. } = &launch.request.isolation
    {
        let workspace = Path::new(workspace_root).canonicalize()?;
        let id = ExecutionId::parse(&path.file_name().unwrap().to_string_lossy())?;
        let scratch = readonly::Scratch::create(path.parent().unwrap(), &workspace, id)?;
        let (argv, cache) = scratch.command(
            &launch.request,
            &workspace,
            launch
                .bubblewrap
                .as_deref()
                .context("missing pinned bubblewrap executable")?,
            launch
                .planner_cache
                .as_ref()
                .context("missing planner cache")?,
        )?;
        // ExecStopPost owns scratch cleanup after successful exec.
        std::mem::forget(scratch);
        let mut sandbox = Command::new(&argv[0]);
        sandbox.args(&argv[1..]);
        if launch.request.clear_environment {
            sandbox.env_clear();
        }
        sandbox.envs(&launch.request.environment);
        // The FIFO descriptors of command have not been installed yet.
        sandbox
            .stdin(Stdio::null())
            .stdout(
                OpenOptions::new()
                    .write(true)
                    .open(path.join("stdout.fifo"))?,
            )
            .stderr(
                OpenOptions::new()
                    .write(true)
                    .open(path.join("stderr.fifo"))?,
            );
        super::private::planner_filter(&mut sandbox, cache)?;
        ensure!(
            launch
                .deadline_at_ms
                .is_none_or(|deadline| now_ms() < deadline),
            "planner deadline expired before exec"
        );
        ensure_session(&launch, path)?;
        atomic_json(&path.join("started.json"), &true)?;
        return Err(sandbox.exec().into());
    }
    atomic_json(&path.join("started.json"), &true)?;
    Err(command.exec().into())
}

fn ensure_session(launch: &Launch, path: &Path) -> Result<()> {
    checked(
        Command::new(&launch.tmux)
            .arg("-S")
            .arg(&launch.socket)
            .args(["has-session", "-t"])
            .arg(path.file_name().context("execution directory has no id")?),
    )?;
    Ok(())
}
pub(super) fn run_command(launch: &Launch, path: &Path, terminal: Option<&Path>) -> Command {
    let mut command = Command::new(&launch.systemd_run);
    command
        .args([
            "--user",
            "--quiet",
            "--collect",
            "--service-type=exec",
            "--expand-environment=no",
        ])
        .arg(format!("--unit={}", launch.unit))
        .args([
            "--property=ExitType=main",
            "--property=KillMode=control-group",
            "--property=KillSignal=SIGTERM",
            "--property=TimeoutStopSec=2s",
            "--property=Restart=no",
            "--property=UMask=0077",
        ])
        .arg(format!(
            "--property=BindsTo={} {}",
            launch.owner_unit, launch.tmux_unit
        ))
        .arg(format!(
            "--property=After={} {}",
            launch.owner_unit, launch.tmux_unit
        ));
    let stop = format!(
        "{} __execution outcome {}",
        systemd_quote(&launch.helper.to_string_lossy()),
        systemd_quote(&path.to_string_lossy())
    );
    command.arg(format!("--property=ExecStopPost={stop}"));
    let timeout = match (launch.request.timeout_ms, launch.deadline_at_ms) {
        (0, None) => 0,
        (timeout, None) => timeout,
        (timeout, Some(deadline)) => {
            let remaining = deadline.saturating_sub(now_ms()).max(1) as u64;
            if timeout == 0 {
                remaining
            } else {
                remaining.min(timeout)
            }
        }
    };
    if timeout > 0 {
        command.arg(format!("--property=RuntimeMaxSec={timeout}ms"));
    }
    if let Some(limit) = launch.memory_max {
        command.arg(format!("--property=MemoryMax={limit}"));
    }
    if let Some(tty) = terminal {
        command.arg(format!("--property=TTYPath={}", tty.display()));
        command.args([
            "--wait",
            "--property=StandardInput=tty-force",
            "--property=StandardOutput=tty",
            "--property=StandardError=tty",
            "--property=TTYReset=no",
            "--property=TTYVHangup=no",
        ]);
        if let Ok(term) = std::env::var("TERM") {
            command.arg(format!("--setenv=TERM={term}"));
        }
    } else {
        command.args([
            "--property=StandardOutput=null",
            "--property=StandardError=journal",
        ]);
    }
    command
        .arg("--")
        .arg(&launch.helper)
        .args(["__execution", "launch"])
        .arg(path);
    command
}
fn systemd_quote(value: &str) -> String {
    let mut result = String::from("\"");
    for byte in value.bytes() {
        match byte {
            b'%' => result.push_str("%%"),
            b'$' => result.push_str("$$"),
            b'"' | b'\\' => {
                result.push('\\');
                result.push(byte as char);
            }
            32..=126 => result.push(byte as char),
            _ => result.push_str(&format!("\\x{byte:02x}")),
        }
    }
    result.push('"');
    result
}
fn capture(path: &Path, stream: &str, mut input: impl Read, limit: u64) -> Result<()> {
    let mut output = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path.join(stream))?;
    let mut meta = ExecutionStreamStatus::default();
    let mut buffer = [0; 16384];
    loop {
        let count = match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        let keep = (limit.saturating_sub(meta.bytes)).min(count as u64) as usize;
        output.write_all(&buffer[..keep])?;
        meta.bytes += keep as u64;
        if keep < count && !meta.truncated {
            meta.truncated = true;
            atomic_json(&path.join(format!("{stream}.live.json")), &meta)?;
        }
    }
    output.sync_all()?;
    atomic_json(&path.join(format!("{stream}.json")), &meta)
}
fn publish_result(path: &Path, launch: &Launch) -> Result<()> {
    let main: MainOutcome = read_json(&path.join("outcome.json"))?;
    let stop_cause = read_json(&path.join("stop.json")).ok();
    let owner_lost = if stop_cause.is_none()
        && matches!(main.outcome, Some(ExecutionOutcome::Signal { .. }))
        && matches!(launch.request.owner, ExecutionOwner::Agent { .. })
        && matches!(launch.request.isolation, ExecutionIsolation::Standard)
    {
        let output = Command::new(&launch.systemctl)
            .args([
                "--user",
                "show",
                "--property=ActiveState",
                "--value",
                &launch.owner_unit,
            ])
            .output()?;
        output.status.success()
            && !matches!(
                String::from_utf8_lossy(&output.stdout).trim(),
                "active" | "activating"
            )
    } else {
        false
    };
    let completion = Completion {
        main,
        stop_cause,
        owner_lost,
        streams: Streams {
            stdout: read_json(&path.join("stdout.json")).ok(),
            stderr: read_json(&path.join("stderr.json")).ok(),
            terminal: read_json(&path.join("terminal.json")).ok(),
        },
        started: path.join("started.json").exists(),
        launch_failed: path.join("launch-error.json").exists(),
        timeout_configured: launch.request.timeout_ms != 0 || launch.deadline_at_ms.is_some(),
    };
    let initial = ExecutionFsm::initial(
        ExecutionId::parse(&path.file_name().unwrap().to_string_lossy())?,
        launch.request.io,
    );
    let result = ExecutionFsm
        .transition(&initial, ExecutionInput::Complete(completion))
        .map_err(anyhow::Error::msg)?;
    atomic_json(&path.join("result.json"), &result.state)
}
fn signal_number(value: &str) -> Option<i32> {
    if let Ok(number) = value.parse() {
        return Some(number);
    }
    let signals = [
        ("HUP", libc::SIGHUP),
        ("INT", libc::SIGINT),
        ("QUIT", libc::SIGQUIT),
        ("ILL", libc::SIGILL),
        ("ABRT", libc::SIGABRT),
        ("FPE", libc::SIGFPE),
        ("KILL", libc::SIGKILL),
        ("SEGV", libc::SIGSEGV),
        ("PIPE", libc::SIGPIPE),
        ("ALRM", libc::SIGALRM),
        ("TERM", libc::SIGTERM),
        ("USR1", libc::SIGUSR1),
        ("USR2", libc::SIGUSR2),
        ("BUS", libc::SIGBUS),
        ("XCPU", libc::SIGXCPU),
        ("XFSZ", libc::SIGXFSZ),
        ("SYS", libc::SIGSYS),
    ];
    signals.into_iter().find_map(|(name, number)| {
        (value.strip_prefix("SIG").unwrap_or(value) == name).then_some(number)
    })
}
