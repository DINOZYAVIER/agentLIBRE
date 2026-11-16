//! The stop of a turn (AGL-075): each run has its own, so that a turn of
//! one session is stopped while a turn of another goes on. A turn runs in
//! [`Stop::scope`] on its thread; the agent, the backends, the tools and the
//! search ask [`requested`], which reads the stop of the turn on the thread
//! that asks. A request wakes that thread's blocking read with a signal of
//! its own, as Ctrl-C woke the main thread before (D111).

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Once};

/// Exit status after Ctrl-C (D112).
pub const EXIT_CODE: u8 = 130;

/// The signal that wakes the thread of a turn; its handler does nothing.
const WAKE: libc::c_int = libc::SIGUSR1;

/// No thread holds the turn.
const NO_THREAD: u64 = 0;

#[derive(Default)]
struct Inner {
    requested: AtomicBool,
    /// The `pthread_t` of the thread in [`Stop::scope`], [`NO_THREAD`]
    /// outside it.
    thread: AtomicU64,
}

/// The stop of a run's turns; its clones are the same stop.
#[derive(Clone, Default)]
pub struct Stop(Arc<Inner>);

thread_local! {
    /// The stop of the turn this thread runs.
    static CURRENT: RefCell<Option<Stop>> = const { RefCell::new(None) };
}

impl Stop {
    pub fn new() -> Stop {
        install_wake();
        Stop::default()
    }

    /// Asks the turn to stop and wakes its thread; true when it was already
    /// asked. Only atomics and `pthread_kill`: a signal handler may call it.
    pub fn request(&self) -> bool {
        let was = self.0.requested.swap(true, Ordering::SeqCst);
        let thread = self.0.thread.load(Ordering::SeqCst);
        if thread != NO_THREAD {
            unsafe {
                libc::pthread_kill(thread as libc::pthread_t, WAKE);
            }
        }
        was
    }

    /// Whether the turn was asked to stop.
    pub fn requested(&self) -> bool {
        self.0.requested.load(Ordering::SeqCst)
    }

    /// Forgets an earlier request: the next turn starts clean (D131).
    pub fn reset(&self) {
        self.0.requested.store(false, Ordering::SeqCst);
    }

    /// Runs `work` as the turn of this stop on this thread.
    pub fn scope<R>(&self, work: impl FnOnce() -> R) -> R {
        struct Leave(Option<Stop>, Stop);
        impl Drop for Leave {
            fn drop(&mut self) {
                self.1.0.thread.store(NO_THREAD, Ordering::SeqCst);
                CURRENT.with(|current| *current.borrow_mut() = self.0.take());
            }
        }
        install_wake();
        let outer = CURRENT.with(|current| current.borrow_mut().replace(self.clone()));
        self.0
            .thread
            .store(unsafe { libc::pthread_self() } as u64, Ordering::SeqCst);
        let _leave = Leave(outer, self.clone());
        work()
    }
}

/// Whether the turn running on this thread was asked to stop; false on a
/// thread that runs no turn.
pub fn requested() -> bool {
    CURRENT.with(|current| current.borrow().as_ref().is_some_and(Stop::requested))
}

/// The wake's handler, with no `SA_RESTART`, so that a blocking read returns
/// early and the reader sees the request.
fn install_wake() {
    static ONCE: Once = Once::new();
    extern "C" fn on_wake(_: libc::c_int) {}
    ONCE.call_once(|| unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_wake as *const () as libc::sighandler_t;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(WAKE, &action, std::ptr::null_mut());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn a_thread_outside_a_turn_is_never_stopped() {
        assert!(!requested());
    }

    #[test]
    fn each_turn_reads_its_own_stop() {
        let (first, second) = (Stop::new(), Stop::new());
        let (seen, results) = mpsc::channel();
        let (go, wait) = mpsc::channel::<()>();
        let waiter = {
            let second = second.clone();
            let seen = seen.clone();
            thread::spawn(move || {
                second.scope(|| {
                    wait.recv().unwrap();
                    seen.send(("second", requested())).unwrap();
                })
            })
        };
        first.scope(|| {
            first.request();
            seen.send(("first", requested())).unwrap();
        });
        go.send(()).unwrap();
        waiter.join().unwrap();
        let mut results: Vec<_> = results.try_iter().collect();
        results.sort();
        assert_eq!(results, [("first", true), ("second", false)]);
        assert!(!requested(), "the scope is left");
    }

    #[test]
    fn a_request_wakes_a_blocking_read_of_the_turn() {
        let stop = Stop::new();
        let (reader, _writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let turn = {
            let stop = stop.clone();
            thread::spawn(move || {
                stop.scope(|| {
                    tx.send(()).unwrap();
                    let mut byte = [0u8];
                    // Without the wake the read would block for ever.
                    loop {
                        if requested() {
                            return true;
                        }
                        let read = unsafe {
                            libc::read(
                                std::os::fd::AsRawFd::as_raw_fd(&reader),
                                byte.as_mut_ptr().cast(),
                                1,
                            )
                        };
                        if read < 0 && requested() {
                            return true;
                        }
                    }
                })
            })
        };
        rx.recv().unwrap();
        thread::sleep(Duration::from_millis(50));
        assert!(!stop.request());
        assert!(turn.join().unwrap());
        assert!(stop.request(), "asked twice");
        stop.reset();
        assert!(!stop.requested());
    }
}
