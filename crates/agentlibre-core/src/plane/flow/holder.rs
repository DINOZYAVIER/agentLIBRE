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

use super::drive::{self, Review, Wake};
use crate::machine::Event;
use super::journal::{self, Entry};
use super::{FlowFile, Verb};

/// What a due timer waits before it is looked at again (D9): a second, so a
/// tick that moved nothing retries without spinning.
const DUE_AGAIN: Duration = Duration::from_secs(1);

/// What the holder asks of the daemon (D18): a wake to carry to a session,
/// a review turn to run (D19).
#[derive(Debug)]
pub enum Need {
    Wake { plane_dir: PathBuf, wake: Wake },
    Review { plane_dir: PathBuf, review: Review },
}

/// Which waiting turn a letter speaks of: a flow may stand on a `wake` and
/// a `review` in one state (D9), and each asks its own once (D20).
pub type Kind = &'static str;
pub const WAKE: Kind = "wake";
pub const REVIEW: Kind = "review";

/// What reaches the holder's thread: a verb, and the daemon's word back.
pub enum Letter {
    /// A verb of the CLI or the tool, answering on its channel (D17).
    Ask {
        verb: Verb,
        lang: Lang,
        reply: Sender<Said>,
    },
    /// A wake went to its session; the holder writes the journal's `woke`
    /// line — no other hand writes it (D14, D20).
    Delivered {
        flow: String,
        session: String,
        text: String,
    },
    /// A verdict: the answer named one of the state's events (D5, D19),
    /// `note` naming the session that turned. It moves the flow.
    Verdict {
        flow: String,
        event: Event,
        note: Option<String>,
    },
    /// No open session answered to a wake's name, or no verdict came (D20,
    /// D19): the flow stands, and a poke — an opening, a turn's end — asks
    /// again.
    Miss { flow: String, kind: Kind },
    /// The turn of the session this flow's wake woke has ended: the
    /// `turn.end` event, pair kept by the daemon (D22).
    TurnEnd { flow: String },
    /// Look again (D18): a session of the plane opened, or one's turn ended.
    Poke,
}

/// The holder, held by the daemon (D17, AGL-121 D39): letters go in, the
/// flow's word comes back.
#[derive(Clone)]
pub struct Holder {
    asks: Sender<Letter>,
}

/// The daemon's holders, one per plane store (AGL-118, D12, D17): the one
/// driver a plane's flows have, so a CLI verb, an agent's tool and a timer
/// tick all ask the same thread and none of them writes the journal beside
/// another (D13).
#[derive(Default)]
pub struct Holders {
    holders: Mutex<HashMap<PathBuf, Holder>>,
    /// The daemon's ear for its flows' needs (D18); `None` outside a daemon,
    /// where a waiting state simply stands and `status` says so.
    needs: Option<Sender<Need>>,
}

impl Holders {
    /// A registry whose holders ask this daemon for the turns they wait for
    /// (D18).
    pub fn with_delivery(needs: Sender<Need>) -> Self {
        Holders {
            holders: Mutex::new(HashMap::new()),
            needs: Some(needs),
        }
    }

    /// The holder of the plane `plane_dir`, made when first asked (D17).
    pub fn holder(&self, plane_dir: &Path, workdir: &Path) -> Holder {
        let mut holders = self.holders.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(one) = holders.get(plane_dir) {
            return one.clone();
        }
        let one = spawn(plane_dir.to_path_buf(), workdir.to_path_buf(), self.needs.clone());
        holders.insert(plane_dir.to_path_buf(), one.clone());
        one
    }

    /// The holder of `plane_dir` when one runs; never starts one (D18): the
    /// poke of a turn's end follows only flows that already have a driver.
    pub fn peek(&self, plane_dir: &Path) -> Option<Holder> {
        self.holders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(plane_dir)
            .cloned()
    }
}

/// The holders, shared by the daemon and the planes it opens (D17).
pub type Shared = Arc<Holders>;

impl Holder {
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
pub fn spawn(plane_dir: PathBuf, workdir: PathBuf, needs: Option<Sender<Need>>) -> Holder {
    let (asks, inbox) = mpsc::channel();
    thread::spawn(move || {
        // Asks in flight (D20): sent, and the daemon has not answered yet;
        // every ask gets its word, so none sticks.
        let mut pending: HashSet<(String, Kind)> = HashSet::new();
        // What stood waiting when the daemon died is asked anew (D20).
        issue(&plane_dir, &needs, &mut pending);
        loop {
            match next_timer(&plane_dir) {
                // A flow stands where a timer runs: wake when it runs out.
                Some(wait) => match inbox.recv_timeout(wait) {
                    Ok(letter) => answer(&plane_dir, &workdir, letter, &needs, &mut pending),
                    Err(RecvTimeoutError::Timeout) => {
                        drive::tick(&plane_dir, &workdir);
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
    Holder { asks }
}

fn answer(
    plane_dir: &Path,
    workdir: &Path,
    letter: Letter,
    needs: &Option<Sender<Need>>,
    pending: &mut HashSet<(String, Kind)>,
) {
    match letter {
        Letter::Ask { verb, lang, reply } => {
            let said = drive::run(plane_dir, workdir, &verb, lang);
            let _ = reply.send(said);
            issue(plane_dir, needs, pending);
        }
        Letter::Delivered {
            flow,
            session,
            text,
        } => {
            pending.remove(&(flow.clone(), WAKE));
            // Only a delivery writes the `woke` line (D14); a late word
            // about a reset flow finds no journal and goes unsaid.
            let file = journal::path(plane_dir, &flow);
            if file.is_file() {
                let _ = journal::append(
                    &file,
                    &Entry::Woke {
                        time: journal::now(),
                        session,
                        text,
                    },
                );
            }
            issue(plane_dir, needs, pending);
        }
        Letter::Verdict { flow, event, note } => {
            pending.remove(&(flow.clone(), REVIEW));
            drive::settle(plane_dir, workdir, &flow, event, note);
            issue(plane_dir, needs, pending);
        }
        Letter::Miss { flow, kind } => {
            pending.remove(&(flow, kind));
        }
        Letter::TurnEnd { flow } => {
            drive::settle(plane_dir, workdir, &flow, Event::TurnEnd, None);
            issue(plane_dir, needs, pending);
        }
        Letter::Poke => issue(plane_dir, needs, pending),
    }
}

/// Sends every wake and review the flows stand waiting for (D18, D19, D20);
/// an ask in flight is not sent twice. Outside a daemon nothing is asked:
/// the state stands, and `status` says what it waits for (D6).
fn issue(plane_dir: &Path, needs: &Option<Sender<Need>>, pending: &mut HashSet<(String, Kind)>) {
    let Some(needs) = needs else {
        return;
    };
    for wake in drive::wakeups(plane_dir) {
        if pending.insert((wake.flow.clone(), WAKE)) {
            let _ = needs.send(Need::Wake {
                plane_dir: plane_dir.to_path_buf(),
                wake,
            });
        }
    }
    for review in drive::reviews(plane_dir) {
        if pending.insert((review.flow.clone(), REVIEW)) {
            let _ = needs.send(Need::Review {
                plane_dir: plane_dir.to_path_buf(),
                review,
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
