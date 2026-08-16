use super::*;

#[cfg(target_os = "linux")]
const PANIC_GUARD_CHILD_ENV: &str = "AGL_INTERNAL_TUI_PANIC_GUARD_CHILD";

#[cfg(target_os = "linux")]
#[test]
fn panic_hook_restores_parent_terminal_in_native_pty() {
    if std::env::var_os(PANIC_GUARD_CHILD_ENV).is_some() {
        let _terminal_mode = TuiTerminalMode::enter().unwrap();
        io::stdout().write_all(b"AGL_TUI_PANIC_READY\n").unwrap();
        io::stdout().flush().unwrap();
        let mut trigger = [0_u8; 1];
        io::stdin().read_exact(&mut trigger).unwrap();
        panic!("intentional TUI terminal-guard panic fixture");
    }

    let mut fixture = PanicGuardParentTerminal::spawn();
    fixture.wait_for(b"AGL_TUI_PANIC_READY");
    fixture.assert_raw();
    fixture.write(b"x");
    let status = fixture.finish();
    assert!(!status.success(), "induced panic unexpectedly succeeded");
    fixture.assert_restored();
    let enable = fixture
        .output
        .windows(b"\x1b[?2004h".len())
        .position(|candidate| candidate == b"\x1b[?2004h")
        .expect("panic fixture never enabled bracketed paste");
    let disable = fixture
        .output
        .windows(b"\x1b[?2004l".len())
        .rposition(|candidate| candidate == b"\x1b[?2004l")
        .expect("panic hook never disabled bracketed paste");
    assert!(disable > enable);
    assert!(
        fixture.output[disable..]
            .windows(b"\x1b[?25h".len())
            .any(|candidate| candidate == b"\x1b[?25h")
    );
}

#[cfg(target_os = "linux")]
struct PanicGuardParentTerminal {
    master: std::fs::File,
    child: std::process::Child,
    output: Vec<u8>,
    original: libc::termios,
}

#[cfg(target_os = "linux")]
impl PanicGuardParentTerminal {
    fn spawn() -> Self {
        use std::os::fd::FromRawFd as _;
        use std::process::Stdio;

        let mut master = -1;
        let mut slave = -1;
        let size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            },
            0
        );
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut original) }, 0);
        let duplicate = |descriptor| {
            let found = unsafe { libc::dup(descriptor) };
            assert!(found >= 0);
            unsafe { std::fs::File::from_raw_fd(found) }
        };
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::terminal_mode::panic_hook_restores_parent_terminal_in_native_pty",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PANIC_GUARD_CHILD_ENV, "1")
            .env("RUST_BACKTRACE", "0")
            .stdin(Stdio::from(duplicate(slave)))
            .stdout(Stdio::from(duplicate(slave)))
            .stderr(Stdio::from(duplicate(slave)))
            .spawn()
            .unwrap();
        unsafe { libc::close(slave) };
        let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        Self {
            master: unsafe { std::fs::File::from_raw_fd(master) },
            child,
            output: Vec::new(),
            original,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
        self.master.flush().unwrap();
    }

    fn wait_for(&mut self, needle: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self
            .output
            .windows(needle.len())
            .any(|candidate| candidate == needle)
        {
            self.read_available();
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "panic fixture exited before ready: {status}; output={}",
                    String::from_utf8_lossy(&self.output)
                );
            }
            assert!(Instant::now() < deadline, "panic fixture timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn finish(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.read_available();
            if let Some(status) = self.child.try_wait().unwrap() {
                self.read_available();
                return status;
            }
            assert!(Instant::now() < deadline, "panic fixture did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn assert_raw(&self) {
        let current = self.current_termios();
        assert_eq!(current.c_lflag & (libc::ICANON | libc::ECHO), 0);
    }

    fn assert_restored(&self) {
        let current = self.current_termios();
        assert_eq!(current.c_iflag, self.original.c_iflag);
        assert_eq!(current.c_oflag, self.original.c_oflag);
        assert_eq!(current.c_cflag, self.original.c_cflag);
        assert_eq!(current.c_lflag, self.original.c_lflag);
        assert_eq!(current.c_cc, self.original.c_cc);
    }

    fn current_termios(&self) -> libc::termios {
        use std::os::fd::AsRawFd as _;

        let mut current = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(
            unsafe { libc::tcgetattr(self.master.as_raw_fd(), &mut current) },
            0
        );
        current
    }

    fn read_available(&mut self) {
        let mut bytes = [0_u8; 4096];
        loop {
            match self.master.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => self.output.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => panic!("failed to read panic fixture PTY: {error}"),
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for PanicGuardParentTerminal {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
