//! The servers' half of the daemon: the idle clocks, the sweep, the whisper's start (AGL-121, D28): split off `daemon/mod.rs`.

use super::*;

/// How often the idle sweep looks at the clocks (AGL-120, D2).
pub(super) const SWEEP_POLL: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(5)
};

/// The idle clock of a server (AGL-120, D2): the seconds of no turn that
/// stop it, the last turn's end, how to stop it (its unit, or the child
/// agl owns), and the store of its record.
#[derive(Clone)]
pub(super) struct Clock {
    pub(super) idle: Duration,
    pub(super) last: Instant,
    pub(super) unit: Option<String>,
    pub(super) engine: Arc<Mutex<Option<Engine>>>,
    pub(super) dir: PathBuf,
}

/// Whether a server's clock is due (AGL-120, D2): `elapsed` past `idle`
/// with no turn running on it; an `idle` of 0 means never.
fn due(idle: Duration, elapsed: Duration, busy: bool) -> bool {
    !idle.is_zero() && !busy && elapsed >= idle
}

impl Daemon {
    /// An audio's text (D15), by the transcription of the plane of a session
    /// open on `dir` in `plane`.
    ///
    /// With `draft` it is a chunk of a live dictation: the draft server of
    /// the session's whisper hears it, with no warm-up and no whisper
    /// (AGL-127, D4); a session with none refuses it.
    pub(super) fn transcribe(
        &self,
        dir: &str,
        plane: Option<&str>,
        audio: proto::Attached,
        draft: bool,
    ) -> Result<String, Error> {
        let (voice, drafter) = {
            let state = self.lock();
            let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| PathBuf::from(dir));
            state
                .sessions
                .iter()
                .find(|(id, entry)| {
                    state.plane_of(id).map(|held| held.file.name.as_str()) == plane
                        && std::fs::canonicalize(&entry.opened.workspace)
                            .is_ok_and(|workspace| workspace == dir)
                })
                .map(|(_, entry)| (Arc::clone(&entry.voice), entry.draft.clone()))
                .ok_or_else(|| Error::NoTranscriber { dir: dir.clone() })?
        };
        let drafter = if draft {
            Some(drafter.ok_or(Error::NoDraft)?)
        } else {
            None
        };
        let source = wire::attached_of(audio).ok_or(Error::BadAudio)?.source;
        let wav = if source.ext.eq_ignore_ascii_case("wav") {
            source.bytes
        } else {
            agentlibre_core::media::audio_to_wav(&source.bytes)?
        };
        if let Some(drafter) = drafter {
            return drafter.transcribe(&wav).map_err(Error::from);
        }
        voice.warm_up(&mut |_| {})?;
        voice.transcribe(&wav, None, &mut |_| {}).map_err(Error::from)
    }
    /// One look at the idle clocks (AGL-120, D2): a server whose clock
    /// passed `idle_stop` with no turn running on it stops — its unit, or
    /// the child agl owns. The clock stays: its `last` rewinds, so the
    /// server a later turn brings up is watched again on the same clock.
    pub(super) fn sweep(&self) {
        let runtime = self.paths.runtime.clone();
        let due: Vec<(String, Clock)> = {
            let state = self.lock();
            state
                .clocks
                .iter()
                .filter(|(key, clock)| {
                    let busy = state
                        .sessions
                        .values()
                        .any(|entry| &entry.clock == *key && entry.busy);
                    due(clock.idle, clock.last.elapsed(), busy)
                })
                .map(|(key, clock)| (key.clone(), clock.clone()))
                .collect()
        };
        for (key, clock) in due {
            // A child agl owns stops first; else the unit of the project.
            if let Some(engine) = clock.engine.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let pid = engine.pid();
                engine.shutdown();
                remove_record(&clock.dir, runtime.as_deref(), Some(pid));
            } else if let Some(unit) = &clock.unit
                && unit_active(unit).unwrap_or(false)
            {
                let pid = unit_main_pid(unit);
                let _ = systemctl(&["stop", unit]);
                remove_record(&clock.dir, runtime.as_deref(), pid);
            }
            // Rewind: the clock is spent, and only a running server earns
            // another stop. A turn rewinds it sooner (in `tell`).
            if let Some(clock) = self.lock().clocks.get_mut(&key) {
                clock.last = Instant::now();
            }
        }
    }

    /// `client` no longer holds `session`, or anything when it is gone.
    pub(super) fn release(&self, client: u64, session: Option<&str>) {
        let mut state = self.lock();
        let ids: Vec<String> = match session {
            Some(id) => vec![id.to_owned()],
            None => state.sessions.keys().cloned().collect(),
        };
        for id in ids {
            let Some(entry) = state.sessions.get_mut(&id) else { continue };
            let held = entry.holders.remove(&client);
            entry.subscribers.remove(&client);
            // The session lives on, its turn too (AGL-125, D1, D2): its idle
            // clock begins as its last holder leaves.
            if held && entry.holders.is_empty() {
                entry.last = Instant::now();
            }
        }
    }

    /// The sessions that nobody holds and that have had nothing to do for
    /// their `session_idle` are unloaded (AGL-125, D6); their files are their
    /// record.
    pub(super) fn unload(&self) {
        let mut state = self.lock();
        let idle: Vec<String> = state
            .sessions
            .iter()
            .filter(|(_, entry)| {
                entry.holders.is_empty() && due(entry.asking.idle, entry.last.elapsed(), entry.busy)
            })
            .map(|(id, _)| id.clone())
            .collect();
        if idle.is_empty() {
            return;
        }
        for id in idle {
            if let Some(dir) = state.plane_of(&id).map(|plane| plane.file.dir.clone()) {
                state.follow(&dir, &id, false);
            }
            // Its commands dropped, the run's thread ends.
            let entry = state.sessions.remove(&id).expect("the entry");
            // A wake whose session died before its turn ended gives
            // its gate back with it (D58), or no plane's flows would
            // ever turn again.
            let stale: Vec<(PathBuf, String)> = state
                .flow_wakes
                .iter()
                .filter(|(_, session)| **session == id)
                .map(|(pair, _)| pair.clone())
                .collect();
            for pair in stale {
                state.flow_wakes.remove(&pair);
                self.flow_gate.release();
            }
            let url = entry.opened.server_url.clone();
            if let Some(engine) = entry.engine.lock().unwrap_or_else(|e| e.into_inner()).take() {
                state.engines.push((url, engine, entry.project_dir.clone()));
            }
        }
        // An engine no plane uses any more is stopped (D16).
        let used: HashSet<String> = state
            .sessions
            .values()
            .map(|entry| entry.opened.server_url.clone())
            .collect();
        // Its clock goes with it (AGL-120, D2): with no session on the
        // engine, nothing rewinds the clock, and D16 has already stopped it.
        let clocked: HashSet<String> = state.sessions.values().map(|entry| entry.clock.clone()).collect();
        state.clocks.retain(|key, _| clocked.contains(key));
        let runtime = self.paths.runtime.clone();
        let (keep, stop): (Vec<_>, Vec<_>) =
            std::mem::take(&mut state.engines).into_iter().partition(|(url, _, _)| used.contains(url));
        state.engines = keep;
        drop(state);
        for (_, engine, dir) in stop {
            let pid = engine.pid();
            engine.shutdown();
            agentlibre_core::engine::remove_record(&dir, runtime.as_deref(), Some(pid));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (AGL-120, D2); a running turn holds it, an idle of 0 never fires.
    #[test]
    pub(super) fn an_idle_clock_is_due_when_its_time_passes() {
        assert!(due(Duration::from_secs(30), Duration::from_secs(30), false));
        assert!(due(Duration::from_secs(30), Duration::from_secs(31), false));
        assert!(!due(Duration::from_secs(30), Duration::from_secs(29), false));
        // A turn running on the server holds the clock.
        assert!(!due(Duration::from_secs(30), Duration::from_secs(600), true));
        // idle_stop = 0 means never.
        assert!(!due(Duration::ZERO, Duration::from_secs(600), false));
    }
}
