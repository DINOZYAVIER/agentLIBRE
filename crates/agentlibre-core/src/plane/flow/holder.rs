//! The holder of a plane's flows (AGL-118, D12, D17): the daemon's thread
//! per plane, the one driver of its machines — it turns the verbs of the
//! human's CLI and the agent's tool, fires the states' timers, and keeps
//! nothing of its own: the journal is the truth, so a restarted daemon
//! re-folds and goes on. No session and no model are needed for a flow to
//! turn; a state that waits for a turn simply stands (D6).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::error::Error;
use crate::i18n::Lang;
use crate::plane::Said;

use super::host;
use super::journal::{self, Entry};
use super::{FlowFile, Verb};

/// What a due timer waits before it is looked at again (D9): a second, so a
/// tick that moved nothing retries without spinning.
const DUE_AGAIN: Duration = Duration::from_secs(1);

/// What the holder answers: a verb of the flows and the channel its word
/// goes back on (D17).
struct Ask {
    verb: Verb,
    lang: Lang,
    reply: Sender<Said>,
}

/// The holder's handle, held by the daemon (D17); verbs go in, the flow's
/// word comes back.
#[derive(Clone)]
pub struct Handle {
    asks: Sender<Ask>,
}

/// The daemon's holders, one per plane store (AGL-118, D12, D17): the one
/// driver a plane's flows have, so a CLI verb, an agent's tool and a timer
/// tick all ask the same thread and none of them writes the journal beside
/// another (D13).
#[derive(Default)]
pub struct Registry {
    holders: Mutex<HashMap<PathBuf, Handle>>,
}

impl Registry {
    /// The holder of the plane `plane_dir`, made when first asked (D17).
    pub fn holder(&self, plane_dir: &Path, workdir: &Path) -> Handle {
        let mut holders = self.holders.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = holders.get(plane_dir) {
            return handle.clone();
        }
        let handle = spawn(plane_dir.to_path_buf(), workdir.to_path_buf());
        holders.insert(plane_dir.to_path_buf(), handle.clone());
        handle
    }
}

/// The holders, shared by the daemon and the planes it opens (D17).
pub type Shared = Arc<Registry>;

impl Handle {
    /// Asks the holder to do `verb` and waits for its word (D17): the one
    /// driver, so a verb waits its turn behind a command, not beside it (D13).
    pub fn ask(&self, verb: Verb, lang: Lang) -> Said {
        let (to, from) = mpsc::channel();
        let gone = || Said::fault(&Error::PlaneGone, lang);
        match self.asks.send(Ask { verb, lang, reply: to }) {
            Ok(()) => from.recv().unwrap_or_else(|_| gone()),
            Err(_) => gone(),
        }
    }
}

/// The holder of the flows of `plane_dir`, whose states' commands run in
/// `workdir` (D17): idle until a verb comes or a timer runs out.
pub fn spawn(plane_dir: PathBuf, workdir: PathBuf) -> Handle {
    let (asks, inbox) = mpsc::channel();
    thread::spawn(move || loop {
        match next_timer(&plane_dir) {
            // A flow stands where a timer runs: wake when it runs out.
            Some(wait) => match inbox.recv_timeout(wait) {
                Ok(ask) => answer(&plane_dir, &workdir, ask),
                Err(RecvTimeoutError::Timeout) => {
                    host::tick(&plane_dir, &workdir);
                }
                Err(RecvTimeoutError::Disconnected) => return,
            },
            // Nothing armed: sleep until a verb comes.
            None => match inbox.recv() {
                Ok(ask) => answer(&plane_dir, &workdir, ask),
                Err(_) => return,
            },
        }
    });
    Handle { asks }
}

fn answer(plane_dir: &Path, workdir: &Path, ask: Ask) {
    let said = host::run(plane_dir, workdir, &ask.verb, ask.lang);
    let _ = ask.reply.send(said);
}

/// The time until the next `timer` of a flow runs out (D9, D16): a state's
/// timer counts from the second of the flow's last journal line — the second
/// it came to rest where it stands. A due one answers `DUE_AGAIN`, and a
/// stopped or unfinished fold waits: the human holds those (D7, D11).
fn next_timer(plane_dir: &Path) -> Option<Duration> {
    let decls = FlowFile::load(plane_dir).ok()?;
    let now = journal::now();
    let mut next: Option<u64> = None;
    for decl in decls.flows {
        let file = journal::path(plane_dir, decl.name());
        let Ok(entries) = journal::read(&file) else { continue };
        let Some(last) = entries.last() else { continue };
        if matches!(last, Entry::Stopped { .. }) {
            continue;
        }
        let Ok(turn) = journal::fold(&decl, &file, &entries) else { continue };
        let Some(state) = turn.state else { continue };
        let at = decl.states().iter().position(|s| s.name == state)?;
        let Some(due) = decl.timer(at) else { continue };
        let left = due.as_secs().saturating_sub(now.saturating_sub(last.time()));
        next = Some(next.map_or(left, |min| min.min(left)));
    }
    let left = next?;
    Some(if left == 0 { DUE_AGAIN } else { Duration::from_secs(left) })
}
