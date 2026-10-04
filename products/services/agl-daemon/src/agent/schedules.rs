use std::collections::BTreeMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use agl_core::agent::{AgentRunOrigin, AgentRunSpec};
use agl_core::{AgentRunId, Content, ConversationId, RequestId};
use anyhow::{Context, Result};

use super::schedule_events::FileEvents;
use super::{AgentHandle, now_ms};
use crate::store::{ScheduleRecord, StoreHandle, next_tick_after};

pub(crate) struct ScheduleService {
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ScheduleService {
    pub fn start(store: StoreHandle, agent: AgentHandle) -> Result<Self> {
        let (stop, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("agl-schedule-triggers".into())
            .spawn(move || {
                let mut keys = Vec::new();
                let mut states = BTreeMap::<ConversationId, TriggerState>::new();
                loop {
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let result = poll(&store, &agent, &mut keys, &mut states, now_ms());
                    if let Err(error) = result {
                        tracing::warn!(%error, "cannot poll autonomous schedules");
                    }
                }
            })
            .context("cannot start schedule trigger service")?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for ScheduleService {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(super) struct TriggerState {
    record: ScheduleRecord,
    next_tick: Option<i64>,
    events: Option<FileEvents>,
    event_fault: bool,
    observed_run: Option<AgentRunId>,
}

impl TriggerState {
    pub(super) fn new(record: ScheduleRecord, now: i64) -> Self {
        let next_tick = next_tick_after(record.enabled_at_ms, record.config.interval_seconds, now);
        Self {
            record,
            next_tick,
            events: None,
            event_fault: false,
            observed_run: None,
        }
    }

    pub(super) fn poll(
        &mut self,
        store: &StoreHandle,
        agent: &AgentHandle,
        now: i64,
    ) -> Result<()> {
        let workspace = self.record.snapshot.workspace.root.as_path();
        let (busy, latest_run) = store.function_activity(workspace, &self.record.function.id)?;
        if latest_run != self.observed_run {
            self.events = None;
            self.observed_run = latest_run;
        }
        if busy {
            // Drop the event source while any Run of this Function is active.
            // Reopening after completion takes a fresh baseline and cannot replay
            // either this Run's writes or external writes during that interval.
            self.events = None;
        } else if self.events.is_none() && !self.record.config.events.is_empty() {
            match FileEvents::new(workspace, &self.record.config.events) {
                Ok(events) => {
                    self.events = Some(events);
                    self.event_fault = false;
                }
                Err(error) => {
                    if !self.event_fault {
                        tracing::warn!(function_id=%self.record.function.id, workspace=%workspace.display(), %error, "schedule file events are unavailable");
                    }
                    self.event_fault = true;
                }
            }
        }
        if self.next_tick.is_some_and(|tick| tick <= now) {
            let first_due = self.next_tick.unwrap();
            self.next_tick = next_tick_after(
                self.record.enabled_at_ms,
                self.record.config.interval_seconds,
                now,
            );
            let interval = (self.record.config.interval_seconds * 1000) as i64;
            let tick = self
                .next_tick
                .and_then(|next| next.checked_sub(interval))
                .unwrap_or(first_due);
            if tick > first_due {
                tracing::info!(function_id=%self.record.function.id, skipped=(tick-first_due)/interval, "skipped elapsed schedule ticks");
            }
            if busy {
                tracing::info!(function_id=%self.record.function.id, workspace=%workspace.display(), tick_at_ms=tick, "skipped overlapping schedule timer");
            } else {
                self.start(
                    agent,
                    AgentRunOrigin::Timer {
                        conversation_id: self.record.conversation_id,
                        function_id: self.record.function.id.clone(),
                        tick_at_ms: tick,
                    },
                );
                self.events = None;
                return Ok(());
            }
        }
        if let Some(events) = self.events.as_mut() {
            match events.changed() {
                Ok(paths) if !paths.is_empty() => {
                    let (active, latest) =
                        store.function_activity(workspace, &self.record.function.id)?;
                    if active || latest != self.observed_run {
                        self.events = None;
                        self.observed_run = latest;
                        return Ok(());
                    }
                    self.start(
                        agent,
                        AgentRunOrigin::FileEvent {
                            conversation_id: self.record.conversation_id,
                            function_id: self.record.function.id.clone(),
                            event_id: RequestId::generate(),
                            paths,
                        },
                    );
                    self.events = None;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(function_id=%self.record.function.id, %error, "schedule file-event baseline will be refreshed");
                    self.events = None;
                }
            }
        }
        Ok(())
    }

    fn start(&self, agent: &AgentHandle, origin: AgentRunOrigin) {
        let spec = AgentRunSpec {
            origin,
            input: Content::text(self.record.config.prompt.clone())
                .expect("validated schedule prompt"),
            reasoning: None,
        };
        match agent.start_scheduled_run(spec, self.record.generation.clone()) {
            Ok(run_id) => {
                tracing::info!(%run_id, function_id=%self.record.function.id, "autonomous scheduled Run admitted")
            }
            Err(error) => {
                tracing::info!(function_id=%self.record.function.id, %error, "skipped schedule trigger; admission refused")
            }
        }
    }
}

fn poll(
    store: &StoreHandle,
    agent: &AgentHandle,
    keys: &mut Vec<(ConversationId, RequestId)>,
    states: &mut BTreeMap<ConversationId, TriggerState>,
    now: i64,
) -> Result<()> {
    let current = store.enabled_schedule_keys()?;
    if current != *keys {
        let records = store.schedules(None)?;
        states.retain(|id, state| current.contains(&(*id, state.record.generation.clone())));
        for record in records.into_iter().filter(|r| r.enabled) {
            states
                .entry(record.conversation_id)
                .or_insert_with(|| TriggerState::new(record, now));
        }
        *keys = current;
    }
    for state in states.values_mut() {
        state.poll(store, agent, now)?;
    }
    Ok(())
}
