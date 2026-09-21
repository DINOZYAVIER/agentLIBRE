use std::os::unix::process::CommandExt as _;
use std::process::Command;
use std::time::{Duration, Instant};

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("agl-execd-launcher: {error}");
            std::process::exit(126);
        }
    }
}

fn run() -> Result<i32, String> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next().ok_or("missing launcher mode")?;
    if mode != "--pipes" && mode != "--pty" {
        return Err("invalid launcher mode".to_owned());
    }
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
        return Err("missing launcher argument separator".to_owned());
    }
    let program = args.next().ok_or("missing target program")?;
    if !std::path::Path::new(&program).is_absolute() {
        return Err("target program must be absolute".to_owned());
    }
    let target_args = args.collect::<Vec<_>>();
    let signals = blocked_signals()?;
    // SAFETY: prctl affects only this launcher and requests notification if its
    // execd parent dies.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGUSR1) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: getppid has no preconditions.
    if unsafe { libc::getppid() } == 1 {
        return Err("execd owner died during launch".to_owned());
    }
    // SAFETY: the launcher is single-threaded and immediately separates parent
    // supervision from the child pre-exec path.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if child == 0 {
        launch_target(mode == "--pty", program, target_args, &signals);
    }
    supervise(child, &signals)
}

fn blocked_signals() -> Result<libc::sigset_t, String> {
    // SAFETY: signal-set functions initialize and mutate only this local value.
    unsafe {
        let mut signals = std::mem::zeroed();
        libc::sigemptyset(&mut signals);
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGWINCH, libc::SIGUSR1] {
            libc::sigaddset(&mut signals, signal);
        }
        if libc::sigprocmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(signals)
    }
}

fn launch_target(
    pty: bool,
    program: std::ffi::OsString,
    arguments: Vec<std::ffi::OsString>,
    signals: &libc::sigset_t,
) -> ! {
    // SAFETY: this is the isolated child before exec; failures terminate only it.
    unsafe {
        if libc::sigprocmask(libc::SIG_UNBLOCK, signals, std::ptr::null_mut()) != 0
            || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
            || libc::setsid() < 0
            || (pty && libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0) != 0)
        {
            eprintln!("agl-execd-launcher: {}", std::io::Error::last_os_error());
            libc::_exit(126);
        }
    }
    let error = Command::new(program).args(arguments).exec();
    eprintln!("agl-execd-launcher: {error}");
    // SAFETY: exec failed and this child must not run launcher cleanup.
    unsafe { libc::_exit(126) }
}

fn supervise(child: libc::pid_t, signals: &libc::sigset_t) -> Result<i32, String> {
    let mut terminate_deadline = None;
    loop {
        let mut status = 0;
        // SAFETY: child is the one process forked above and status is writable.
        let waited = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
        if waited == child {
            return propagate_status(status);
        }
        if waited < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        if terminate_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            signal_group(child, libc::SIGKILL);
            terminate_deadline = None;
        }
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 20_000_000,
        };
        // SAFETY: signals points to an initialized blocked set and timeout is valid.
        let signal = unsafe { libc::sigtimedwait(signals, std::ptr::null_mut(), &timeout) };
        match signal {
            libc::SIGINT => signal_group(child, libc::SIGINT),
            libc::SIGWINCH => signal_group(child, libc::SIGWINCH),
            libc::SIGTERM => {
                signal_group(child, libc::SIGTERM);
                terminate_deadline = Some(Instant::now() + Duration::from_secs(2));
            }
            libc::SIGUSR1 => signal_group(child, libc::SIGKILL),
            -1 => {
                let error = std::io::Error::last_os_error();
                if !matches!(error.raw_os_error(), Some(libc::EAGAIN | libc::EINTR)) {
                    return Err(error.to_string());
                }
            }
            _ => {}
        }
    }
}

fn signal_group(child: libc::pid_t, signal: i32) {
    // SAFETY: negative pid addresses the target's session-leading process group.
    let _ = unsafe { libc::kill(-child, signal) };
}

fn propagate_status(status: i32) -> Result<i32, String> {
    if libc::WIFEXITED(status) {
        return Ok(libc::WEXITSTATUS(status));
    }
    if libc::WIFSIGNALED(status) {
        let signal = libc::WTERMSIG(status);
        // SAFETY: restore and raise the target signal so execd observes the same outcome.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            let mut set = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, signal);
            libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::raise(signal);
        }
        return Ok(128 + signal);
    }
    Err("target returned an unsupported wait status".to_owned())
}
