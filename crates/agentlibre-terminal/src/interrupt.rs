//! Ctrl-C: the first one asks the running step to stop, a second one exits
//! at once (D111, D113). In the REPL the terminal is raw, Ctrl-C arrives as
//! a key and the REPL decides about the second one itself (D120, D131).
//! No turn runs in this process (AGL-076, D5), so there is no thread to
//! wake: the stop is a flag of its own, which the forwarding thread reads
//! and sends to the daemon (AGL-131, D7).

use std::sync::atomic::{AtomicBool, Ordering};

pub use agentlibre_common::tty::INTERRUPTED_EXIT as EXIT_CODE;

/// A request to stop: asked once, and told when it was asked already.
struct Flag(AtomicBool);

impl Flag {
    const fn new() -> Flag {
        Flag(AtomicBool::new(false))
    }

    /// Asks; true when it was already asked.
    fn request(&self) -> bool {
        self.0.swap(true, Ordering::SeqCst)
    }

    fn requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Whether a Ctrl-C asked the running step to stop.
static REQUESTED: Flag = Flag::new();

extern "C" fn on_sigint(_: libc::c_int) {
    // Only async-signal-safe calls here: an atomic and `_exit`.
    if REQUESTED.request() {
        unsafe { libc::_exit(EXIT_CODE as libc::c_int) };
    }
}

extern "C" fn on_sigint_request(_: libc::c_int) {
    REQUESTED.request();
}

/// Installs a SIGINT handler that only asks the running step to stop; the
/// REPL exits on its own after restoring the terminal (D135).
pub fn install_request_only() {
    install_handler(on_sigint_request);
}

/// Asks the running step to stop, as Ctrl-C does; true when it was already
/// asked.
pub fn request() -> bool {
    REQUESTED.request()
}

/// Whether the running step was asked to stop.
pub fn requested() -> bool {
    REQUESTED.requested()
}

/// Forgets an earlier Ctrl-C: the next turn starts clean (D131).
pub fn reset() {
    REQUESTED.reset();
}

/// Installs the SIGINT handler. Without `SA_RESTART`, a blocking read
/// returns early, so a request waiting for the daemon stops too.
pub fn install() {
    install_handler(on_sigint);
}

fn install_handler(handler: extern "C" fn(libc::c_int)) {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as *const () as libc::sighandler_t;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_second_request_is_told_to_have_been_asked_before() {
        // A flag of its own: the process's is the REPL's tests' too.
        let flag = Flag::new();
        assert!(!flag.requested());
        assert!(!flag.request(), "the first asks");
        assert!(flag.requested());
        assert!(flag.request(), "the second finds it asked");
        flag.reset();
        assert!(!flag.requested(), "the next turn starts clean");
    }
}
