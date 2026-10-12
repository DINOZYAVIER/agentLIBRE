//! Ctrl-C: the first one asks the running step to stop, a second one exits
//! at once (D111, D113).

use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Exit status after Ctrl-C (D112).
pub const EXIT_CODE: u8 = 130;

extern "C" fn on_sigint(_: libc::c_int) {
    if INTERRUPTED.swap(true, Ordering::SeqCst) {
        // Only async-signal-safe calls here.
        unsafe { libc::_exit(EXIT_CODE as libc::c_int) };
    }
}

/// Installs the SIGINT handler. Without `SA_RESTART`, a blocking read
/// returns early, so a request waiting for llama-server stops too.
pub fn install() {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigint as *const () as libc::sighandler_t;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
}

/// Whether Ctrl-C was pressed.
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Sends `signal` to the process group `group`; an empty group is fine.
pub fn signal_group(group: u32, signal: libc::c_int) {
    unsafe {
        libc::kill(-(group as libc::pid_t), signal);
    }
}
