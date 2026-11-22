//! Running the mechanical actions of a flow (AGL-118, D5, D9): a command of
//! the declaration, whose exit code is the verdict where one exists. Where no
//! exit code exists by nature, the verdict is a check-turn (D5), and that
//! belongs to the plane's driver, not here.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::action::Outcome;
use crate::error::Error;
use crate::programs::{clean_env, program};

/// How long a timed-out command gets between SIGTERM and SIGKILL (D102, D103).
const GRACE: Duration = Duration::from_secs(2);

/// Runs the flow's `cmd` in `dir` with `timeout`, and gives what the state's
/// `exit.*` keys are matched against (D9): a zero exit is `Ok`, a nonzero one
/// `Fail`, no exit in `timeout` is `Timeout`. Output is read and dropped: a
/// flow's verdict is the code, and the words a command left are the human's
/// to look at in the terminal the flow was started from (D5).
pub fn run(dir: &Path, cmd: &str, timeout: Duration) -> Result<Outcome, Error> {
    let bash = program("bash")?;
    let mut command = Command::new(bash);
    let mut child = clean_env(&mut command)
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // A group of its own, so a timeout ends what the command left (D102).
        .process_group(0)
        .spawn()
        .map_err(|source| Error::Write {
            path: dir.into(),
            source,
        })?;
    let group = child.id();
    match wait(&mut child, Instant::now() + timeout) {
        Some(status) if status.success() => Ok(Outcome::Ok),
        Some(_) => Ok(Outcome::Fail),
        None => {
            signal(group, libc::SIGTERM);
            if wait(&mut child, Instant::now() + GRACE).is_none() {
                signal(group, libc::SIGKILL);
            }
            Ok(Outcome::Timeout)
        }
    }
}

/// The child's status by `deadline`, `None` while it runs.
fn wait(child: &mut std::process::Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
    None
}

/// `signal` to the whole group; a group already gone is not an error here.
fn signal(group: u32, signal: i32) {
    // SAFETY: `pid` is the group of the child this process spawned, and a
    // signal to a group that has ended answers an error we ignore.
    unsafe {
        libc::killpg(group as i32, signal);
    }
}

