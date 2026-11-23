//! The holder of a plane's flows (AGL-118, D12, D17): the daemon's thread
//! per plane, the one driver of its machines — it turns the verbs of the
//! human's CLI and the agent's tool, fires the states' timers, and asks the
//! daemon for the turns its states wait for (D18), keeping nothing of its
//! own: the journal is the truth, so a restarted daemon re-folds and goes
//! on. No session and no model are needed for a flow to turn; a state that
//! waits for a turn simply stands (D6).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use crate::error::Error;
use agentlibre_common::i18n::{m, Lang};
use crate::plane::Text;
use agentlibre_common::tr;

use super::drive::{self, Review, Wake, Word};
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
    /// A driver error dropped (AGL-121, D37): the daemon writes it to its
    /// log; `status` shows it as a warning line from the driver's memory.
    Warn { plane_dir: PathBuf, text: String },
}

impl Need {
    /// The plane that asked (AGL-121, D36): the daemon routes every need
    /// to the one thread of its plane.
    pub fn plane_dir(&self) -> &Path {
        match self {
            Need::Wake { plane_dir, .. }
            | Need::Review { plane_dir, .. }
            | Need::Warn { plane_dir, .. } => plane_dir,
        }
    }
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
        reply: Sender<Text>,
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
    /// A state's command ended (D35): its word from the watcher thread. A
    /// word the machine has left behind finds nothing and writes nothing
    /// (D57), so a `stop` leaves `stopped` alone in the journal.
    Exited(Word),
}

/// The holder of a plane's flows (AGL-118, D12, D17): the one driver its
/// flows have, so a CLI verb, an agent's tool and a timer tick all ask the
/// same thread and none of them writes the journal beside another (D13).
/// Letters go in, the flow's word comes back (AGL-121, D39).
#[derive(Clone)]
pub struct Holder {
    asks: Sender<Letter>,
}

impl Holder {
    /// The holder of the flows of `plane_dir`, whose states' commands run in
    /// `workdir` (D17): idle until a verb comes, a timer runs out, or the
    /// daemon answers a wake (D18). `needs` is the daemon's ear for the
    /// turns its states wait on; `None` outside a daemon, where a waiting
    /// state simply stands and `status` says so.
    pub fn spawn(plane_dir: PathBuf, workdir: PathBuf, needs: Option<Sender<Need>>) -> Holder {
        let (asks, inbox) = mpsc::channel();
        let posted = asks.clone();
        thread::spawn(move || {
            // Commands run off this thread; their word comes back as a
            // letter, so the holder answers verbs while a build runs (D35).
            let mut flight = drive::Flight::new(plane_dir.clone(), needs.clone(), move |word| {
                let _ = posted.send(Letter::Exited(word));
            });
            // Asks in flight (D20): sent, and the daemon has not answered yet;
            // every ask gets its word, so none sticks.
            let mut pending: HashSet<(String, Kind)> = HashSet::new();
            // What stood waiting when the daemon died is asked anew (D20).
            issue(&plane_dir, &needs, &mut pending);
            loop {
                match next_timer(&plane_dir) {
                    // A flow stands where a timer runs: wake when it runs out.
                    Some(wait) => match inbox.recv_timeout(wait) {
                        Ok(letter) => {
                            answer(&plane_dir, &workdir, letter, &needs, &mut pending, &mut flight)
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            drive::tick(&plane_dir, &workdir, Some(&mut flight));
                            flight.clean_pass();
                            issue(&plane_dir, &needs, &mut pending);
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    },
                    // Nothing armed: sleep until a letter comes.
                    None => match inbox.recv() {
                        Ok(letter) => {
                            answer(&plane_dir, &workdir, letter, &needs, &mut pending, &mut flight)
                        }
                        Err(_) => return,
                    },
                }
            }
        });
        Holder { asks }
    }

    /// Asks the holder to do `verb` and waits for its word (D17): the one
    /// driver of the journal, while a state's command runs off the thread
    /// and `status` answers beside it (D35).
    pub fn ask(&self, verb: Verb, lang: Lang) -> Text {
        let (to, from) = mpsc::channel();
        let gone = || Text::fault(&Error::PlaneGone, lang);
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

fn answer(
    plane_dir: &Path,
    workdir: &Path,
    letter: Letter,
    needs: &Option<Sender<Need>>,
    pending: &mut HashSet<(String, Kind)>,
    flight: &mut drive::Flight,
) {
    match letter {
        Letter::Ask { verb, lang, reply } => {
            // A verb of a flow whose command runs answers now: the command
            // is off this thread, and `stop` ends it (D35).
            let read = matches!(verb, Verb::Status { .. } | Verb::List);
            let mut said = drive::run(plane_dir, workdir, &verb, lang, Some(flight));
            // A dropped driver error shows as a warning line beside the
            // place, until a clean pass clears it (D37).
            if let Some(warning) = flight.warning().map(str::to_owned) {
                let line = tr!(lang, m::FLOW_WARN, text = warning);
                said.lines.push(crate::plane::Told::Notice(line.clone()));
                said.content =
                    Some(said.content.map_or(line.clone(), |c| format!("{c}
{line}")));
            }
            let _ = reply.send(said);
            issue(plane_dir, needs, pending);
            // A status read shows the warning, it does not spend it (D37).
            if read {
                return;
            }
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
            drive::settle(plane_dir, workdir, &flow, event, note, Some(flight));
            issue(plane_dir, needs, pending);
        }
        Letter::Miss { flow, kind } => {
            pending.remove(&(flow, kind));
        }
        Letter::TurnEnd { flow } => {
            drive::settle(plane_dir, workdir, &flow, Event::TurnEnd, None, Some(flight));
            issue(plane_dir, needs, pending);
        }
        Letter::Poke => issue(plane_dir, needs, pending),
        Letter::Exited(word) => {
            // The word moves the machine, and what it waits for is asked (D35).
            drive::ended(plane_dir, workdir, word, flight);
            issue(plane_dir, needs, pending);
        }
    }
    // A pass that dropped nothing clears the warning (D37).
    flight.clean_pass();
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
    for decl in decls.flows.iter() {
        let file = journal::path(plane_dir, decl.name());
        let Ok(entries) = journal::read(&file) else { continue };
        let Some(last) = entries.last() else { continue };
        if matches!(last, Entry::Stopped { .. }) {
            continue;
        }
        let Ok(turn) = journal::fold(decl, &file, &entries) else { continue };
        let Some(state) = turn.state else { continue };
        let at = decl.states().iter().position(|s| s.name == state)?;
        let Some(due) = decl.timer(at) else { continue };
        let left = due.as_secs().saturating_sub(now.saturating_sub(last.time()));
        next = Some(next.map_or(left, |min| min.min(left)));
    }
    let left = next?;
    Some(if left == 0 { DUE_AGAIN } else { Duration::from_secs(left) })
}
