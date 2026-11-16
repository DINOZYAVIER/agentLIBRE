//! Ctrl-C: the first one asks the running step to stop, a second one exits
//! at once (D111, D113). In the REPL the terminal is raw, Ctrl-C arrives as
//! a key and the REPL decides about the second one itself (D120, D131).
//! What stops is the stop of the terminal's runs (AGL-075): one for the
//! process, as a terminal runs one turn at a time.

use std::sync::OnceLock;

pub use crate::stop::EXIT_CODE;
use crate::stop::Stop;

static STOP: OnceLock<Stop> = OnceLock::new();

/// The stop of the terminal's runs.
pub fn stop() -> &'static Stop {
    STOP.get_or_init(Stop::new)
}

extern "C" fn on_sigint(_: libc::c_int) {
    // Only async-signal-safe calls here: `request` is atomics and
    // `pthread_kill`.
    if STOP.get().is_some_and(Stop::request) {
        unsafe { libc::_exit(EXIT_CODE as libc::c_int) };
    }
}

extern "C" fn on_sigint_request(_: libc::c_int) {
    if let Some(stop) = STOP.get() {
        stop.request();
    }
}

/// Installs a SIGINT handler that only asks the running step to stop; the
/// REPL exits on its own after restoring the terminal (D135).
pub fn install_request_only() {
    install_handler(on_sigint_request);
}

/// Asks the running step to stop, as Ctrl-C does, and wakes the thread of
/// its turn; true when it was already asked.
pub fn request() -> bool {
    stop().request()
}

/// Forgets an earlier Ctrl-C: the next turn starts clean (D131).
pub fn reset() {
    stop().reset();
}

/// Installs the SIGINT handler. Without `SA_RESTART`, a blocking read
/// returns early, so a request waiting for llama-server stops too.
pub fn install() {
    install_handler(on_sigint);
}

fn install_handler(handler: extern "C" fn(libc::c_int)) {
    // The stop exists before a signal can ask it.
    stop();
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as *const () as libc::sighandler_t;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
}
