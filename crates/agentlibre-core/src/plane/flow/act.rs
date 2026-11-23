//! Running the mechanical actions of a flow (AGL-118, D5, D9): a command of
//! the declaration, whose exit code is the verdict where one exists. Where no
//! exit code exists by nature, the verdict is a check-turn (D5), and that
//! belongs to the plane's driver, not here.
//!
//! A command runs in a process group of its own and is watched off the
//! thread that started it (AGL-121, D35): the driver answers a verb while a
//! build runs, and `stop` ends the whole group (D57).

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::action::Outcome;
use crate::error::Error;
use agentlibre_common::programs::{clean_env, program};

/// How long a command gets between SIGTERM and SIGKILL (D102, D103).
const GRACE: Duration = Duration::from_secs(2);

/// A command started and not waited for (AGL-121, D35).
pub struct Child {
    /// Its process group, which `stop` ends whole (D57).
    group: u32,
}

/// Starts `cmd` in `dir` in a group of its own (D102, D57) and watches it
/// on a thread of its own: the outcome — a zero exit `Ok`, a nonzero one
/// `Fail`, no exit in `timeout` `Timeout` — goes to `on_exit` (D9). Output
/// is read and dropped: a flow's verdict is the code, and the words a
/// command left are the human's to look at in the terminal it started from
/// (D5).
pub fn spawn(
    dir: &Path,
    cmd: &str,
    timeout: Duration,
    on_exit: impl FnOnce(Outcome) + Send + 'static,
) -> Result<Child, Error> {
    let child = start(dir, cmd)?;
    let group = child.id();
    thread::spawn(move || on_exit(watch(child, timeout)));
    Ok(Child { group })
}

/// Ends everything `child` left (D57): the whole group, SIGKILL after the
/// grace. It returns at once; the sweep of what would not end runs on its
/// own thread, off the driver (D35).
pub fn kill(child: Child) {
    let group = child.group;
    signal(group, libc::SIGTERM);
    thread::spawn(move || {
        let until = Instant::now() + GRACE;
        while Instant::now() < until && alive(group) {
            thread::sleep(Duration::from_millis(50));
        }
        if alive(group) {
            signal(group, libc::SIGKILL);
        }
    });
}

/// Runs `cmd` in `dir` to its end, for a caller that waits (the tests):
/// what the state's `exit.*` keys are matched against (D9).
pub fn run(dir: &Path, cmd: &str, timeout: Duration) -> Result<Outcome, Error> {
    Ok(watch(start(dir, cmd)?, timeout))
}

/// The command of the state, in a group of its own (D102), its output
/// dropped with its stdin shut.
fn start(dir: &Path, cmd: &str) -> Result<std::process::Child, Error> {
    let bash = program("bash")?;
    let child = clean_env(&mut Command::new(bash))
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|source| Error::from(agentlibre_common::error::Error::Write {
            path: dir.into(),
            source,
        }))?;
    Ok(child)
}

/// The command's outcome by `timeout` (D9): its exit when it comes,
/// `Timeout` when it does not and the group ends for it.
fn watch(mut child: std::process::Child, timeout: Duration) -> Outcome {
    match wait(&mut child, Instant::now() + timeout) {
        Some(status) if status.success() => Outcome::Ok,
        Some(_) => Outcome::Fail,
        None => {
            let group = child.id();
            signal(group, libc::SIGTERM);
            if wait(&mut child, Instant::now() + GRACE).is_none() {
                signal(group, libc::SIGKILL);
            }
            Outcome::Timeout
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

/// Whether the group still has a process of its own.
fn alive(group: u32) -> bool {
    // SAFETY: `pid` is the group of a child this process spawned; a signal
    // of 0 only asks whether it lives, and its answer is what we read.
    unsafe { libc::killpg(group as i32, 0) == 0 }
}

/// `signal` to the whole group; a group already gone is not an error here.
fn signal(group: u32, signal: i32) {
    // SAFETY: `pid` is the group of the child this process spawned, and a
    // signal to a group that has ended answers an error we ignore.
    unsafe {
        libc::killpg(group as i32, signal);
    }
}
