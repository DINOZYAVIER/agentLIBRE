// Native launcher fixture; never installed with the private launcher.
#[cfg(target_os = "linux")]
mod linux {
    use std::env;
    use std::fs::{self, OpenOptions};
    use std::io::{self, Read as _, Write as _};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
    use std::path::Path;
    use std::process;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use serde_json::json;

    static RESIZED: AtomicBool = AtomicBool::new(false);
    static RECEIVED_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

    pub(super) fn main() {
        if let Err(error) = run() {
            eprintln!("agl-process-test-helper: {error}");
            process::exit(2);
        }
    }

    fn run() -> Result<(), Box<dyn std::error::Error>> {
        let mut arguments = env::args().skip(1);
        let mode = arguments.next().ok_or("missing helper mode")?;
        let arguments = arguments.collect::<Vec<_>>();
        match mode.as_str() {
            "argv-echo" => println!("{}", serde_json::to_string(&arguments)?),
            "binary-stdio" => {
                io::stdout().write_all(&[b'o', 0, 0xff, b'\n'])?;
                io::stderr().write_all(&[b'e', 0xfe, 0, b'\n'])?;
            }
            "stdin-echo" => {
                let mut input = Vec::new();
                io::stdin().read_to_end(&mut input)?;
                io::stdout().write_all(&input)?;
            }
            "drain-input-chunks" => {
                drain_input_chunks(parse_usize(&arguments, 0, "chunk byte count")?)?
            }
            "interactive-lines" => interactive_lines()?,
            "tty-info" => print_tty_info()?,
            "resize-wait" => resize_wait()?,
            "signal-eof" => signal_eof()?,
            "long-output" => long_output(parse_usize(&arguments, 0, "byte count")?)?,
            "sleep-ms" => std::thread::sleep(Duration::from_millis(parse_u64(
                &arguments,
                0,
                "sleep duration",
            )?)),
            "ignore-term" => {
                ignore_termination_signals();
                std::thread::sleep(Duration::from_millis(parse_u64(
                    &arguments,
                    0,
                    "sleep duration",
                )?));
            }
            "touch" => fs::write(argument(&arguments, 0, "touch path")?, b"executed")?,
            "close-stdout" => {
                unsafe { libc::close(libc::STDOUT_FILENO) };
                io::stderr().write_all(b"stderr-after-stdout-close\n")?;
            }
            "exit-code" => process::exit(parse_i32(&arguments, 0, "exit code")?),
            "tcp-connect" => tcp_connect(parse_u16(&arguments, 0, "TCP port")?)?,
            "sandbox-probe" => sandbox_probe(&arguments)?,
            "fork-tree" => fork_tree(argument(&arguments, 0, "evidence path")?)?,
            other => return Err(format!("unknown helper mode `{other}`").into()),
        }
        Ok(())
    }

    fn print_tty_info() -> io::Result<()> {
        let size = terminal_size(libc::STDOUT_FILENO);
        let pid = unsafe { libc::getpid() };
        let session = unsafe { libc::getsid(0) };
        let process_group = unsafe { libc::getpgrp() };
        let terminal_group = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
        println!(
            "{}",
            json!({
                "stdin_tty": unsafe { libc::isatty(libc::STDIN_FILENO) } == 1,
                "stdout_tty": unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1,
                "stderr_tty": unsafe { libc::isatty(libc::STDERR_FILENO) } == 1,
                "session_leader": session == pid,
                "controlling_terminal": terminal_group == process_group,
                "columns": size.map(|value| value.0),
                "rows": size.map(|value| value.1),
            })
        );
        io::stdout().flush()
    }

    fn interactive_lines() -> io::Result<()> {
        unsafe { libc::signal(libc::SIGWINCH, resized as *const () as libc::sighandler_t) };
        io::stdout().write_all(b"ready\n")?;
        io::stdout().flush()?;
        let mut input = String::new();
        loop {
            if RESIZED.swap(false, Ordering::AcqRel) {
                let size = terminal_size(libc::STDOUT_FILENO).unwrap_or_default();
                writeln!(io::stdout(), "resized={}x{}", size.0, size.1)?;
                io::stdout().flush()?;
            }
            let mut descriptor = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            let poll_result = unsafe { libc::poll(&mut descriptor, 1, 50) };
            if poll_result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if poll_result == 0 {
                continue;
            }
            if descriptor.revents & (libc::POLLIN | libc::POLLHUP) == 0 {
                continue;
            }
            input.clear();
            if io::stdin().read_line(&mut input)? == 0 {
                return Ok(());
            }
            if input == "emit-terminal-effects\n" {
                io::stdout().write_all(
                    b"filter-before\x1b]52;c;PRIVATE_CLIPBOARD\x07\
                      \x1b]0;PRIVATE_TITLE\x1b\\\
                      \x1bPPRIVATE_DCS\x1b\\\
                      \x1b_PRIVATE_APC\x1b\\\
                      \x1b^PRIVATE_PM\x1b\\filter-after\n",
                )?;
            } else {
                write!(io::stdout(), "reply:{input}")?;
            }
            io::stdout().flush()?;
        }
    }

    extern "C" fn resized(_: libc::c_int) {
        RESIZED.store(true, Ordering::Release);
    }

    fn resize_wait() -> io::Result<()> {
        unsafe { libc::signal(libc::SIGWINCH, resized as *const () as libc::sighandler_t) };
        let initial = terminal_size(libc::STDOUT_FILENO).unwrap_or_default();
        println!("initial={}x{}", initial.0, initial.1);
        io::stdout().flush()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !RESIZED.swap(false, Ordering::AcqRel) {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "SIGWINCH was not received",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let resized = terminal_size(libc::STDOUT_FILENO).unwrap_or_default();
        println!("resized={}x{}", resized.0, resized.1);
        io::stdout().flush()
    }

    extern "C" fn received_signal(signal: libc::c_int) {
        RECEIVED_SIGNAL.store(signal, Ordering::Release);
    }

    fn signal_eof() -> io::Result<()> {
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            unsafe { libc::signal(signal, received_signal as *const () as libc::sighandler_t) };
        }
        io::stdout().write_all(b"ready\n")?;
        io::stdout().flush()?;
        let mut input = Vec::new();
        loop {
            let mut buffer = [0_u8; 1024];
            match io::stdin().read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => input.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    if RECEIVED_SIGNAL.load(Ordering::Acquire) != 0 {
                        break;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        println!(
            "{}",
            json!({
                "eof": input.is_empty() || RECEIVED_SIGNAL.load(Ordering::Acquire) == 0,
                "input_bytes": input.len(),
                "signal": RECEIVED_SIGNAL.load(Ordering::Acquire),
            })
        );
        Ok(())
    }

    fn terminal_size(descriptor: libc::c_int) -> Option<(u16, u16)> {
        let mut size = unsafe { std::mem::zeroed::<libc::winsize>() };
        (unsafe { libc::ioctl(descriptor, libc::TIOCGWINSZ, &mut size) } == 0)
            .then_some((size.ws_col, size.ws_row))
    }

    fn long_output(bytes: usize) -> io::Result<()> {
        let chunk = [b'x'; 8192];
        let mut remaining = bytes;
        while remaining > 0 {
            let count = remaining.min(chunk.len());
            io::stdout().write_all(&chunk[..count])?;
            remaining -= count;
        }
        io::stdout().flush()
    }

    fn drain_input_chunks(chunk_bytes: usize) -> io::Result<()> {
        if chunk_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "chunk byte count must be nonzero",
            ));
        }
        let mut chunk = vec![0_u8; chunk_bytes];
        let mut drained = 0_u64;
        loop {
            match io::stdin().read_exact(&mut chunk) {
                Ok(()) => {
                    drained += 1;
                    writeln!(io::stdout(), "drained={drained}")?;
                    io::stdout().flush()?;
                }
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }

    fn tcp_connect(port: u16) -> io::Result<()> {
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let result = TcpStream::connect_timeout(&address, Duration::from_millis(250));
        println!(
            "{}",
            json!({
                "connected": result.is_ok(),
                "error_kind": result.err().map(|error| format!("{:?}", error.kind())),
            })
        );
        Ok(())
    }

    fn sandbox_probe(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
        let workspace_file = Path::new(argument(arguments, 0, "workspace file")?);
        let sibling_file = Path::new(argument(arguments, 1, "sibling file")?);
        let runtime_file = Path::new(argument(arguments, 2, "runtime file")?);
        let port = parse_u16(arguments, 3, "TCP port")?;
        fs::write(workspace_file, b"workspace-ok")
            .map_err(|error| format!("workspace write failed: {error}"))?;
        let home_file = Path::new(&env::var("HOME")?).join("home-write");
        let tmp_file = Path::new(&env::var("TMPDIR")?).join("tmp-write");
        fs::write(&home_file, b"home-ok").map_err(|error| format!("HOME write failed: {error}"))?;
        fs::write(&tmp_file, b"tmp-ok").map_err(|error| format!("TMPDIR write failed: {error}"))?;

        let sibling_read_denied = fs::read(sibling_file).is_err();
        let sibling_write_denied = OpenOptions::new().write(true).open(sibling_file).is_err();
        let runtime_read = fs::read(runtime_file).is_ok();
        let runtime_write_denied = OpenOptions::new().append(true).open(runtime_file).is_err();
        let dev_null_write = OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .and_then(|mut null| null.write_all(b"sandbox-null-probe"))
            .is_ok();
        let thread_spawn = matches!(
            std::thread::Builder::new()
                .name("sandbox-probe".to_owned())
                .spawn(|| 73)
                .and_then(|thread| {
                    thread
                        .join()
                        .map_err(|_| io::Error::other("sandbox probe thread panicked"))
                }),
            Ok(73)
        );
        let clone3_result =
            unsafe { libc::syscall(libc::SYS_clone3, std::ptr::null::<libc::c_void>(), 0) };
        let clone3_unavailable =
            clone3_result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOSYS);
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let network_denied =
            TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_err();
        let visible_pids = fs::read_dir("/proc")?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
            .collect::<Vec<_>>();
        println!(
            "{}",
            json!({
                "workspace_write": workspace_file.is_file(),
                "home_write": home_file.is_file(),
                "tmp_write": tmp_file.is_file(),
                "sibling_read_denied": sibling_read_denied,
                "sibling_write_denied": sibling_write_denied,
                "runtime_read": runtime_read,
                "runtime_write_denied": runtime_write_denied,
                "dev_null_write": dev_null_write,
                "thread_spawn": thread_spawn,
                "clone3_unavailable": clone3_unavailable,
                "network_denied": network_denied,
                "visible_pids": visible_pids,
            })
        );
        Ok(())
    }

    fn fork_tree(evidence: &str) -> Result<(), Box<dyn std::error::Error>> {
        ignore_termination_signals();
        append_process_evidence(evidence, "target")?;
        let child = unsafe { libc::fork() };
        if child < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if child == 0 {
            ignore_termination_signals();
            if unsafe { libc::setsid() } < 0 {
                process::exit(3);
            }
            append_process_evidence(evidence, "child")?;
            let grandchild = unsafe { libc::fork() };
            if grandchild < 0 {
                process::exit(4);
            }
            if grandchild == 0 {
                ignore_termination_signals();
                append_process_evidence(evidence, "grandchild")?;
                wait_forever();
            }
            wait_forever();
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let contents = fs::read_to_string(evidence).unwrap_or_default();
            if ["target", "child", "grandchild"]
                .iter()
                .all(|role| contents.lines().any(|line| line.starts_with(role)))
            {
                append_line(evidence, "READY")?;
                break;
            }
            if Instant::now() >= deadline {
                return Err("fork tree did not become ready".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        wait_forever();
    }

    fn append_process_evidence(path: &str, role: &str) -> io::Result<()> {
        let namespace = fs::read_link("/proc/self/ns/pid")?;
        append_line(
            path,
            &format!(
                "{role} inner_pid={} pid_namespace={}",
                unsafe { libc::getpid() },
                namespace.display()
            ),
        )
    }

    fn append_line(path: &str, line: &str) -> io::Result<()> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(path)?;
        file.write_all(format!("{line}\n").as_bytes())?;
        file.sync_data()
    }

    fn ignore_termination_signals() {
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }
    }

    fn wait_forever() -> ! {
        loop {
            unsafe { libc::pause() };
        }
    }

    fn argument<'a>(arguments: &'a [String], index: usize, label: &str) -> Result<&'a str, String> {
        arguments
            .get(index)
            .map(String::as_str)
            .ok_or_else(|| format!("missing {label}"))
    }

    fn parse_usize(arguments: &[String], index: usize, label: &str) -> Result<usize, String> {
        argument(arguments, index, label)?
            .parse()
            .map_err(|error| format!("invalid {label}: {error}"))
    }

    fn parse_i32(arguments: &[String], index: usize, label: &str) -> Result<i32, String> {
        argument(arguments, index, label)?
            .parse()
            .map_err(|error| format!("invalid {label}: {error}"))
    }

    fn parse_u16(arguments: &[String], index: usize, label: &str) -> Result<u16, String> {
        argument(arguments, index, label)?
            .parse()
            .map_err(|error| format!("invalid {label}: {error}"))
    }

    fn parse_u64(arguments: &[String], index: usize, label: &str) -> Result<u64, String> {
        argument(arguments, index, label)?
            .parse()
            .map_err(|error| format!("invalid {label}: {error}"))
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("platform_unsupported: native process test helper is Linux-only");
    std::process::exit(2);
}
