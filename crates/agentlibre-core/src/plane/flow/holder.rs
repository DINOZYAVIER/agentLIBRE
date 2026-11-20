//! The holder of a plane's flows (AGL-118, D12, D17): the daemon's thread
//! per plane, the one driver of its machines — it turns the verbs of the
//! human's CLI and the agent's tool, fires the states' timers, and asks the
//! daemon for the turns its states wait for (D18), keeping nothing of its
//! own: the journal is the truth, so a restarted daemon re-folds and goes
//! on. No session and no model are needed for a flow to turn; a state that
//! waits for a turn simply stands (D6).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::error::Error;
use crate::i18n::Lang;
use crate::plane::Said;

use super::host::{self, Wake};
use super::journal::{self, Entry};
use super::{FlowFile, Verb};

/// What a due timer waits before it is looked at again (D9): a second, so a
/// tick that moved nothing retries without spinning.
const DUE_AGAIN: Duration = Duration::from_secs(1);

/// What the holder asks of the daemon (D18): a wake to carry to a session.
/// Review turns join this list when the daemon runs one (D19).
#[derive(Debug)]
pub enum Need {
    Wake { plane_dir: PathBuf, wake: Wake },
}

/// What reaches the holder's thread: a verb, and the daemon's word back.
pub enum Letter {
    /// A verb of the CLI or the tool, answering on its channel (D17).
    Ask {
        verb: Verb,
        lang: Lang,
        reply: Sender<Said>,
    },
    /// A wake went to its session; the holder writes the journal's `said`
    /// line — no other hand writes it (D14, D20).
    Delivered {
        flow: String,
        session: String,
        text: String,
    },
    /// No open session of the plane answered to that name (D20): the flow
    /// stands, and a poke — an opening, a turn's end — will ask again.
    Miss { flow: String },
    /// Look again (D18): a session of the plane opened, or one's turn ended.
    Poke,
}

/// The holder's handle, held by the daemon (D17); letters go in, the flow's
/// word comes back.
#[derive(Clone)]
pub struct Handle {
    asks: Sender<Letter>,
}

/// The daemon's holders, one per plane store (AGL-118, D12, D17): the one
/// driver a plane's flows have, so a CLI verb, an agent's tool and a timer
/// tick all ask the same thread and none of them writes the journal beside
/// another (D13).
#[derive(Default)]
pub struct Registry {
    holders: Mutex<HashMap<PathBuf, Handle>>,
    /// The daemon's ear for its flows' needs (D18); `None` outside a daemon,
    /// where a waiting state simply stands and `status` says so.
    needs: Option<Sender<Need>>,
}

impl Registry {
    /// A registry whose holders ask this daemon for the turns they wait for
    /// (D18).
    pub fn with_delivery(needs: Sender<Need>) -> Self {
        Registry {
            holders: Mutex::new(HashMap::new()),
            needs: Some(needs),
        }
    }

    /// The holder of the plane `plane_dir`, made when first asked (D17).
    pub fn holder(&self, plane_dir: &Path, workdir: &Path) -> Handle {
        let mut holders = self.holders.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = holders.get(plane_dir) {
            return handle.clone();
        }
        let handle = spawn(plane_dir.to_path_buf(), workdir.to_path_buf(), self.needs.clone());
        holders.insert(plane_dir.to_path_buf(), handle.clone());
        handle
    }

    /// The holder of `plane_dir` when one runs; never starts one (D18): the
    /// poke of a turn's end follows only flows that already have a driver.
    pub fn peek(&self, plane_dir: &Path) -> Option<Handle> {
        self.holders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(plane_dir)
            .cloned()
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
        match self.asks.send(Letter::Ask {
            verb,
            lang,
            reply: to,
        }) {
            Ok(()) => from.recv().unwrap_or_else(|_| gone()),
            Err(_) => gone(),
        }
    }

    /// Posts a letter to the holder (D18): the daemon's word about a wake,
    /// or a poke to look again.
    pub fn post(&self, letter: Letter) {
        let _ = self.asks.send(letter);
    }
}

/// The holder of the flows of `plane_dir`, whose states' commands run in
/// `workdir` (D17): idle until a verb comes, a timer runs out, or the
/// daemon answers a wake (D18).
pub fn spawn(plane_dir: PathBuf, workdir: PathBuf, needs: Option<Sender<Need>>) -> Handle {
    let (asks, inbox) = mpsc::channel();
    thread::spawn(move || {
        // Flows with a wake in flight (D20): asked, and the daemon has not
        // answered yet; every ask gets its Delivered or Miss, so none sticks.
        let mut pending: HashSet<String> = HashSet::new();
        loop {
            match next_timer(&plane_dir) {
                // A flow stands where a timer runs: wake when it runs out.
                Some(wait) => match inbox.recv_timeout(wait) {
                    Ok(letter) => answer(&plane_dir, &workdir, letter, &needs, &mut pending),
                    Err(RecvTimeoutError::Timeout) => {
                        host::tick(&plane_dir, &workdir);
                        issue(&plane_dir, &needs, &mut pending);
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                // Nothing armed: sleep until a letter comes.
                None => match inbox.recv() {
                    Ok(letter) => answer(&plane_dir, &workdir, letter, &needs, &mut pending),
                    Err(_) => return,
                },
            }
        }
    });
    Handle { asks }
}

fn answer(
    plane_dir: &Path,
    workdir: &Path,
    letter: Letter,
    needs: &Option<Sender<Need>>,
    pending: &mut HashSet<String>,
) {
    match letter {
        Letter::Ask { verb, lang, reply } => {
            let said = host::run(plane_dir, workdir, &verb, lang);
            let _ = reply.send(said);
            issue(plane_dir, needs, pending);
        }
        Letter::Delivered {
            flow,
            session,
            text,
        } => {
            pending.remove(&flow);
            // Only a delivery writes the `said` line (D14); a late word
            // about a reset flow finds no journal and goes unsaid.
            let file = journal::path(plane_dir, &flow);
            if file.is_file() {
                let _ = journal::append(
                    &file,
                    &Entry::Said {
                        time: journal::now(),
                        session,
                        text,
                    },
                );
            }
            issue(plane_dir, needs, pending);
        }
        Letter::Miss { flow } => {
            pending.remove(&flow);
        }
        Letter::Poke => issue(plane_dir, needs, pending),
    }
}

/// Sends every wake the flows stand waiting for (D18, D20); a flow with a
/// wake in flight is not asked twice. Outside a daemon nothing is asked:
/// the state stands, and `status` says what it waits for (D6).
fn issue(plane_dir: &Path, needs: &Option<Sender<Need>>, pending: &mut HashSet<String>) {
    let Some(needs) = needs else {
        return;
    };
    for wake in host::wakeups(plane_dir) {
        if pending.insert(wake.flow.clone()) {
            let _ = needs.send(Need::Wake {
                plane_dir: plane_dir.to_path_buf(),
                wake,
            });
        }
    }
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
