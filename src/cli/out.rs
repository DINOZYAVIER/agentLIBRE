//! The lines agl prints on stdout (AGL-038, D2): a closed pipe, as in
//! `agl forge list | head -1`, is a normal end — the printing stops
//! quietly and agl ends as it would, its engine guard and all.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// The stdout of the process is closed: nothing more is printed.
static CLOSED: AtomicBool = AtomicBool::new(false);

/// `println!` that stops on a closed stdout instead of panicking.
#[macro_export]
macro_rules! outln {
    ($($arg:tt)*) => {
        $crate::cli::out::line(&format!($($arg)*))
    };
}

/// Prints `text` and a newline on stdout (AGL-038, D2).
pub fn line(text: &str) {
    line_to(&mut io::stdout().lock(), text, &CLOSED);
}

/// Writes `text` and a newline to `out` unless `closed`; a `BrokenPipe`
/// sets `closed`, any other error is the panic of `println!`.
fn line_to(out: &mut dyn Write, text: &str, closed: &AtomicBool) {
    if closed.load(Ordering::Relaxed) {
        return;
    }
    match writeln!(out, "{text}").and_then(|()| out.flush()) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
            closed.store(true, Ordering::Relaxed);
        }
        Err(error) => panic!("failed printing to stdout: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer whose reader is gone.
    struct Closed(usize);

    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            self.0 += 1;
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_pipe_stops_the_printing_quietly() {
        let closed = AtomicBool::new(false);
        let mut out = Vec::new();
        line_to(&mut out, "one", &closed);
        assert_eq!(out, b"one\n");
        let mut gone = Closed(0);
        line_to(&mut gone, "two", &closed);
        assert!(closed.load(Ordering::Relaxed));
        // Nothing more is tried once the pipe is closed.
        line_to(&mut gone, "three", &closed);
        assert_eq!(gone.0, 1);
    }
}
