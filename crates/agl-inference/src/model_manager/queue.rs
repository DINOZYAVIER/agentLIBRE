use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use super::{InferenceCancellation, InferenceJobScope, ModelManagerError, ModelManagerStatus};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct QueueEntryId(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueueEntryState {
    Queued,
    Active,
    Completed,
    Cancelled,
    Expired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WaitAbandonReason {
    Cancelled,
    DeadlineExceeded,
    CallerDropped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TerminalReason {
    Cancelled,
    DeadlineExceeded,
    ManagerUnavailable,
}

impl TerminalReason {
    fn error(self) -> ModelManagerError {
        match self {
            Self::Cancelled => ModelManagerError::Cancelled,
            Self::DeadlineExceeded => ModelManagerError::DeadlineExceeded,
            Self::ManagerUnavailable => ModelManagerError::ManagerUnavailable,
        }
    }

    fn state(self) -> QueueEntryState {
        match self {
            Self::Cancelled | Self::ManagerUnavailable => QueueEntryState::Cancelled,
            Self::DeadlineExceeded => QueueEntryState::Expired,
        }
    }
}

pub(super) trait QueueCommand: Send + 'static {
    fn is_generation(&self) -> bool;
    fn cancellation(&self) -> Option<&InferenceCancellation>;
    fn deadline(&self) -> Option<Instant>;
    fn active_scope(&self) -> Option<InferenceJobScope>;
    fn complete(self, error: ModelManagerError);

    fn on_queued(&self) {}

    fn on_active(&self) {}

    fn terminal_gate(&self, now: Instant) -> Option<TerminalReason> {
        if self
            .cancellation()
            .is_some_and(InferenceCancellation::is_cancelled)
        {
            Some(TerminalReason::Cancelled)
        } else if self.deadline().is_some_and(|deadline| now >= deadline) {
            Some(TerminalReason::DeadlineExceeded)
        } else {
            None
        }
    }
}

trait QueueClock: Send + Sync + 'static {
    fn now(&self) -> Instant;
}

struct SystemQueueClock;

impl QueueClock for SystemQueueClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

struct PendingEntry<C> {
    id: QueueEntryId,
    command: C,
    state: QueueEntryState,
}

struct ActiveEntry {
    id: QueueEntryId,
    state: QueueEntryState,
    generation: bool,
    cancellation: Option<InferenceCancellation>,
    scope: Option<InferenceJobScope>,
}

struct PendingQueueState<C> {
    entries: VecDeque<PendingEntry<C>>,
    active: Option<ActiveEntry>,
    capacity: usize,
    next_id: u64,
    closed: bool,
    worker_running: bool,
    worker_exit_clean: bool,
}

pub(super) struct PendingQueue<C: QueueCommand> {
    state: Mutex<PendingQueueState<C>>,
    changed: Condvar,
    status: Arc<Mutex<ModelManagerStatus>>,
    clock: Arc<dyn QueueClock>,
}

pub(super) struct ActiveCommand<C> {
    pub(super) id: QueueEntryId,
    pub(super) command: C,
}

pub(super) struct QueueSnapshot {
    pub(super) depth: usize,
    pub(super) active_scope: Option<InferenceJobScope>,
}

impl<C: QueueCommand> PendingQueue<C> {
    pub(super) fn new(capacity: usize, status: Arc<Mutex<ModelManagerStatus>>) -> Self {
        Self::with_clock(capacity, status, Arc::new(SystemQueueClock))
    }

    fn with_clock(
        capacity: usize,
        status: Arc<Mutex<ModelManagerStatus>>,
        clock: Arc<dyn QueueClock>,
    ) -> Self {
        Self {
            state: Mutex::new(PendingQueueState {
                entries: VecDeque::new(),
                active: None,
                capacity,
                next_id: 1,
                closed: false,
                worker_running: true,
                worker_exit_clean: false,
            }),
            changed: Condvar::new(),
            status,
            clock,
        }
    }

    pub(super) fn enqueue(&self, command: C) -> Result<QueueEntryId, ModelManagerError> {
        let now = self.clock.now();
        let mut terminal = Vec::new();
        let result = {
            let mut state = self.lock_state();
            if state.closed || !state.worker_running {
                Err(ModelManagerError::ManagerUnavailable)
            } else {
                terminal = prune_terminal_entries(&mut state.entries, now);
                if state.entries.len() >= state.capacity {
                    Err(ModelManagerError::QueueFull {
                        capacity: state.capacity,
                    })
                } else {
                    let id = QueueEntryId(state.next_id);
                    match state.next_id.checked_add(1) {
                        Some(next_id) => {
                            // Publish Queued while the queue lock still makes
                            // this entry invisible to the worker. This keeps
                            // the public stage stream ordered even under an
                            // immediate consumer wakeup.
                            command.on_queued();
                            state.next_id = next_id;
                            state.entries.push_back(PendingEntry {
                                id,
                                command,
                                state: QueueEntryState::Queued,
                            });
                            Ok(id)
                        }
                        None => Err(ModelManagerError::ManagerUnavailable),
                    }
                }
            }
        };
        self.complete_terminal(terminal);
        if result.is_ok() {
            self.changed.notify_one();
        }
        result
    }

    pub(super) fn pop(&self) -> Option<ActiveCommand<C>> {
        loop {
            let mut state = self.lock_state();
            let terminal = prune_terminal_entries(&mut state.entries, self.clock.now());
            if !terminal.is_empty() {
                drop(state);
                self.complete_terminal(terminal);
                continue;
            }
            if let Some(mut entry) = state.entries.pop_front() {
                entry.state = QueueEntryState::Active;
                let active = ActiveEntry {
                    id: entry.id,
                    state: entry.state,
                    generation: entry.command.is_generation(),
                    cancellation: entry.command.cancellation().cloned(),
                    scope: entry.command.active_scope(),
                };
                debug_assert!(state.active.is_none());
                state.active = Some(active);
                return Some(ActiveCommand {
                    id: entry.id,
                    command: entry.command,
                });
            }
            if state.closed {
                return None;
            }
            drop(
                self.changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        }
    }

    pub(super) fn complete_active(&self, id: QueueEntryId) {
        let mut state = self.lock_state();
        let Some(mut active) = state.active.take() else {
            return;
        };
        debug_assert_eq!(active.id, id);
        if active.id != id {
            state.active = Some(active);
            return;
        }
        debug_assert_eq!(active.state, QueueEntryState::Active);
        active.state = QueueEntryState::Completed;
        self.changed.notify_all();
    }

    pub(super) fn abandon(&self, id: QueueEntryId, reason: WaitAbandonReason) {
        let mut terminal = None;
        {
            let mut state = self.lock_state();
            if let Some(index) = state.entries.iter().position(|entry| entry.id == id) {
                let mut entry = state
                    .entries
                    .remove(index)
                    .expect("located pending queue entry remains present");
                let reason = terminal_reason(&entry.command, reason);
                entry.state = reason.state();
                terminal = Some((entry.command, reason));
            } else if let Some(active) = state.active.as_ref().filter(|active| active.id == id)
                && active.generation
                && matches!(
                    reason,
                    WaitAbandonReason::Cancelled | WaitAbandonReason::CallerDropped
                )
                && let Some(cancellation) = &active.cancellation
            {
                cancellation.cancel();
            }
        }
        if let Some((command, reason)) = terminal {
            self.record_terminal(reason);
            command.complete(reason.error());
        }
    }

    pub(super) fn close_for_shutdown(&self) {
        let (terminal, active_cancellation) = {
            let mut state = self.lock_state();
            if state.closed {
                return;
            }
            state.closed = true;
            let terminal = state
                .entries
                .drain(..)
                .map(|mut entry| {
                    let reason = if entry.command.is_generation() {
                        TerminalReason::Cancelled
                    } else {
                        TerminalReason::ManagerUnavailable
                    };
                    entry.state = reason.state();
                    (entry.command, reason)
                })
                .collect::<Vec<_>>();
            let active_cancellation = state.active.as_ref().and_then(|active| {
                active
                    .generation
                    .then(|| active.cancellation.clone())
                    .flatten()
            });
            (terminal, active_cancellation)
        };
        if let Some(cancellation) = active_cancellation {
            cancellation.cancel();
        }
        self.complete_terminal(terminal);
        self.changed.notify_all();
    }

    pub(super) fn worker_stopped(&self, clean: bool) {
        let terminal = {
            let mut state = self.lock_state();
            state.closed = true;
            state.worker_running = false;
            state.worker_exit_clean = clean;
            state.active.take();
            state
                .entries
                .drain(..)
                .map(|mut entry| {
                    entry.state = QueueEntryState::Cancelled;
                    (entry.command, TerminalReason::ManagerUnavailable)
                })
                .collect::<Vec<_>>()
        };
        self.complete_terminal(terminal);
        self.changed.notify_all();
    }

    pub(super) fn wait_for_worker(&self) -> Result<(), ModelManagerError> {
        let mut state = self.lock_state();
        while state.worker_running {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if state.worker_exit_clean {
            Ok(())
        } else {
            Err(ModelManagerError::ManagerUnavailable)
        }
    }

    pub(super) fn snapshot(&self) -> Result<QueueSnapshot, ModelManagerError> {
        let state = self.lock_state();
        if state.closed || !state.worker_running {
            return Err(ModelManagerError::ManagerUnavailable);
        }
        Ok(QueueSnapshot {
            depth: state.entries.len(),
            active_scope: state
                .active
                .as_ref()
                .and_then(|active| active.scope.clone()),
        })
    }

    #[cfg(test)]
    fn try_pop(&self) -> Option<ActiveCommand<C>> {
        let mut state = self.lock_state();
        if let Some(mut entry) = state.entries.pop_front() {
            entry.state = QueueEntryState::Active;
            state.active = Some(ActiveEntry {
                id: entry.id,
                state: entry.state,
                generation: entry.command.is_generation(),
                cancellation: entry.command.cancellation().cloned(),
                scope: entry.command.active_scope(),
            });
            Some(ActiveCommand {
                id: entry.id,
                command: entry.command,
            })
        } else {
            None
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, PendingQueueState<C>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn complete_terminal(&self, terminal: Vec<(C, TerminalReason)>) {
        for (command, reason) in terminal {
            self.record_terminal(reason);
            command.complete(reason.error());
        }
    }

    fn record_terminal(&self, reason: TerminalReason) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match reason {
            TerminalReason::Cancelled => {
                status.cancellations = status.cancellations.saturating_add(1);
            }
            TerminalReason::DeadlineExceeded => {
                status.deadline_exceeded = status.deadline_exceeded.saturating_add(1);
            }
            TerminalReason::ManagerUnavailable => {}
        }
    }
}

pub(super) struct PendingWaitGuard<C: QueueCommand> {
    queue: Arc<PendingQueue<C>>,
    id: QueueEntryId,
    armed: bool,
}

impl<C: QueueCommand> PendingWaitGuard<C> {
    pub(super) fn new(queue: Arc<PendingQueue<C>>, id: QueueEntryId) -> Self {
        Self {
            queue,
            id,
            armed: true,
        }
    }

    pub(super) fn abandon(&mut self, reason: WaitAbandonReason) {
        self.queue.abandon(self.id, reason);
        self.armed = false;
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<C: QueueCommand> Drop for PendingWaitGuard<C> {
    fn drop(&mut self) {
        if self.armed {
            self.queue
                .abandon(self.id, WaitAbandonReason::CallerDropped);
        }
    }
}

fn prune_terminal_entries<C: QueueCommand>(
    entries: &mut VecDeque<PendingEntry<C>>,
    now: Instant,
) -> Vec<(C, TerminalReason)> {
    let mut terminal = Vec::new();
    let mut index = 0;
    while index < entries.len() {
        let reason = entries[index].command.terminal_gate(now);
        if let Some(reason) = reason {
            let mut entry = entries
                .remove(index)
                .expect("indexed pending queue entry remains present");
            entry.state = reason.state();
            terminal.push((entry.command, reason));
        } else {
            index += 1;
        }
    }
    terminal
}

fn terminal_reason<C: QueueCommand>(command: &C, reason: WaitAbandonReason) -> TerminalReason {
    if !command.is_generation() {
        return TerminalReason::ManagerUnavailable;
    }
    match reason {
        WaitAbandonReason::Cancelled | WaitAbandonReason::CallerDropped => {
            TerminalReason::Cancelled
        }
        WaitAbandonReason::DeadlineExceeded => TerminalReason::DeadlineExceeded,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Barrier, mpsc};
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[derive(Clone)]
    struct ManualClock(Arc<Mutex<Instant>>);

    impl ManualClock {
        fn new(now: Instant) -> Self {
            Self(Arc::new(Mutex::new(now)))
        }

        fn advance(&self, duration: Duration) {
            let mut now = self.0.lock().unwrap();
            *now += duration;
        }
    }

    impl QueueClock for ManualClock {
        fn now(&self) -> Instant {
            *self.0.lock().unwrap()
        }
    }

    struct FakeCommand {
        label: &'static str,
        generation: bool,
        cancellation: Option<InferenceCancellation>,
        deadline: Option<Instant>,
        reply: mpsc::Sender<ModelManagerError>,
    }

    impl QueueCommand for FakeCommand {
        fn is_generation(&self) -> bool {
            self.generation
        }

        fn cancellation(&self) -> Option<&InferenceCancellation> {
            self.cancellation.as_ref()
        }

        fn deadline(&self) -> Option<Instant> {
            self.deadline
        }

        fn active_scope(&self) -> Option<InferenceJobScope> {
            None
        }

        fn complete(self, error: ModelManagerError) {
            let _ = self.reply.send(error);
        }
    }

    fn command(
        label: &'static str,
        generation: bool,
        cancellation: Option<InferenceCancellation>,
        deadline: Option<Instant>,
    ) -> (FakeCommand, mpsc::Receiver<ModelManagerError>) {
        let (reply, receiver) = mpsc::channel();
        (
            FakeCommand {
                label,
                generation,
                cancellation,
                deadline,
                reply,
            },
            receiver,
        )
    }

    fn queue(capacity: usize) -> Arc<PendingQueue<FakeCommand>> {
        Arc::new(PendingQueue::new(
            capacity,
            Arc::new(Mutex::new(ModelManagerStatus::default())),
        ))
    }

    #[test]
    fn queued_cancellation_reclaims_capacity_and_preserves_fifo_survivors() {
        let queue = queue(3);
        let (first, _) = command("first", true, None, None);
        let (second, second_reply) = command("second", true, None, None);
        let (third, _) = command("third", true, None, None);
        let first_id = queue.enqueue(first).unwrap();
        let second_id = queue.enqueue(second).unwrap();
        queue.enqueue(third).unwrap();

        queue.abandon(second_id, WaitAbandonReason::Cancelled);
        assert_eq!(second_reply.recv().unwrap(), ModelManagerError::Cancelled);
        assert_eq!(queue.snapshot().unwrap().depth, 2);
        assert_eq!(queue.try_pop().unwrap().command.label, "first");
        queue.complete_active(first_id);
        let third = queue.try_pop().unwrap();
        assert_eq!(third.command.label, "third");
        queue.complete_active(third.id);
    }

    #[test]
    fn admission_prunes_deadlines_using_the_injected_clock() {
        let start = Instant::now();
        let clock = Arc::new(ManualClock::new(start));
        let queue = PendingQueue::with_clock(
            1,
            Arc::new(Mutex::new(ModelManagerStatus::default())),
            clock.clone(),
        );
        let (expired, expired_reply) =
            command("expired", true, None, Some(start + Duration::from_secs(1)));
        queue.enqueue(expired).unwrap();
        clock.advance(Duration::from_secs(2));

        let (replacement, _) = command("replacement", true, None, None);
        queue.enqueue(replacement).unwrap();

        assert_eq!(
            expired_reply.recv().unwrap(),
            ModelManagerError::DeadlineExceeded
        );
        assert_eq!(queue.snapshot().unwrap().depth, 1);
    }

    #[test]
    fn entry_id_exhaustion_still_completes_entries_pruned_during_admission() {
        let start = Instant::now();
        let clock = Arc::new(ManualClock::new(start));
        let queue = PendingQueue::with_clock(
            1,
            Arc::new(Mutex::new(ModelManagerStatus::default())),
            clock.clone(),
        );
        let (expired, expired_reply) =
            command("expired", true, None, Some(start + Duration::from_secs(1)));
        queue.enqueue(expired).unwrap();
        clock.advance(Duration::from_secs(2));
        queue.lock_state().next_id = u64::MAX;

        let (replacement, _) = command("replacement", true, None, None);
        assert_eq!(
            queue.enqueue(replacement).unwrap_err(),
            ModelManagerError::ManagerUnavailable
        );

        assert_eq!(
            expired_reply.recv().unwrap(),
            ModelManagerError::DeadlineExceeded
        );
        assert_eq!(queue.snapshot().unwrap().depth, 0);
    }

    #[test]
    fn dropped_wait_guard_cannot_leave_a_pending_slot() {
        let queue = queue(1);
        let (candidate, reply) = command("dropped", true, None, None);
        let id = queue.enqueue(candidate).unwrap();

        drop(PendingWaitGuard::new(Arc::clone(&queue), id));

        assert_eq!(reply.recv().unwrap(), ModelManagerError::Cancelled);
        assert_eq!(queue.snapshot().unwrap().depth, 0);
        let (replacement, _) = command("replacement", true, None, None);
        queue.enqueue(replacement).unwrap();
    }

    #[test]
    fn cancel_and_pop_race_has_one_owner_for_each_entry() {
        const ITERATIONS: usize = 1_000;
        let queue = queue(1);
        let active_wins = AtomicUsize::new(0);
        for _ in 0..ITERATIONS {
            let cancellation = InferenceCancellation::new();
            let (candidate, reply) = command("race", true, Some(cancellation.clone()), None);
            let id = queue.enqueue(candidate).unwrap();
            let barrier = Arc::new(Barrier::new(3));
            let pop_queue = Arc::clone(&queue);
            let pop_barrier = Arc::clone(&barrier);
            let pop = thread::spawn(move || {
                pop_barrier.wait();
                pop_queue.try_pop()
            });
            let cancel_queue = Arc::clone(&queue);
            let cancel_barrier = Arc::clone(&barrier);
            let cancel = thread::spawn(move || {
                cancel_barrier.wait();
                cancel_queue.abandon(id, WaitAbandonReason::Cancelled);
            });
            barrier.wait();
            let active = pop.join().unwrap();
            cancel.join().unwrap();
            if let Some(active) = active {
                active_wins.fetch_add(1, Ordering::Relaxed);
                assert!(cancellation.is_cancelled());
                active.command.complete(ModelManagerError::Cancelled);
                queue.complete_active(active.id);
            }
            assert_eq!(reply.recv().unwrap(), ModelManagerError::Cancelled);
            assert_eq!(queue.snapshot().unwrap().depth, 0);
        }
        assert!(active_wins.load(Ordering::Relaxed) <= ITERATIONS);
    }

    #[test]
    fn deadline_and_pop_race_has_one_owner_for_each_entry() {
        const ITERATIONS: usize = 1_000;
        let queue = queue(1);
        for _ in 0..ITERATIONS {
            let (candidate, reply) = command("race", true, None, None);
            let id = queue.enqueue(candidate).unwrap();
            let barrier = Arc::new(Barrier::new(3));
            let pop_queue = Arc::clone(&queue);
            let pop_barrier = Arc::clone(&barrier);
            let pop = thread::spawn(move || {
                pop_barrier.wait();
                pop_queue.try_pop()
            });
            let expire_queue = Arc::clone(&queue);
            let expire_barrier = Arc::clone(&barrier);
            let expire = thread::spawn(move || {
                expire_barrier.wait();
                expire_queue.abandon(id, WaitAbandonReason::DeadlineExceeded);
            });
            barrier.wait();
            let active = pop.join().unwrap();
            expire.join().unwrap();
            if let Some(active) = active {
                active.command.complete(ModelManagerError::DeadlineExceeded);
                queue.complete_active(active.id);
            }
            assert_eq!(reply.recv().unwrap(), ModelManagerError::DeadlineExceeded);
            assert_eq!(queue.snapshot().unwrap().depth, 0);
        }
    }

    #[test]
    fn worker_pop_prunes_an_entry_at_its_deadline_before_fifo_survivors() {
        let start = Instant::now();
        let clock = Arc::new(ManualClock::new(start));
        let queue = PendingQueue::with_clock(
            2,
            Arc::new(Mutex::new(ModelManagerStatus::default())),
            clock.clone(),
        );
        let (expired, expired_reply) =
            command("expired", true, None, Some(start + Duration::from_secs(1)));
        let (survivor, _) = command("survivor", true, None, None);
        queue.enqueue(expired).unwrap();
        queue.enqueue(survivor).unwrap();
        clock.advance(Duration::from_secs(1));

        let active = queue.pop().unwrap();

        assert_eq!(active.command.label, "survivor");
        assert_eq!(
            expired_reply.recv().unwrap(),
            ModelManagerError::DeadlineExceeded
        );
        queue.complete_active(active.id);
    }

    #[test]
    fn shutdown_closes_full_admission_and_completes_each_pending_kind() {
        let queue = queue(2);
        let active_cancellation = InferenceCancellation::new();
        let (active, _) = command("active", true, Some(active_cancellation.clone()), None);
        let active_id = queue.enqueue(active).unwrap();
        let active = queue.try_pop().unwrap();
        let (generation, generation_reply) = command("generation", true, None, None);
        let (management, management_reply) = command("management", false, None, None);
        queue.enqueue(generation).unwrap();
        queue.enqueue(management).unwrap();

        queue.close_for_shutdown();

        assert!(active_cancellation.is_cancelled());
        assert_eq!(
            generation_reply.recv().unwrap(),
            ModelManagerError::Cancelled
        );
        assert_eq!(
            management_reply.recv().unwrap(),
            ModelManagerError::ManagerUnavailable
        );
        let (rejected, _) = command("rejected", true, None, None);
        assert_eq!(
            queue.enqueue(rejected).unwrap_err(),
            ModelManagerError::ManagerUnavailable
        );
        active.command.complete(ModelManagerError::Cancelled);
        queue.complete_active(active_id);
    }
}
