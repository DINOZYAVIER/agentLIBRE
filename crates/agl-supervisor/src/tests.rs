use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agl_events::{EventScope, SafeRuntimeEvent, SafeRuntimeEventEnvelope, TurnFinishStatus};
use agl_ids::{EventId, RunId, SessionId, StepId, TurnId};
use agl_store::{
    AglStore, DurableRunDraft, DurableRunRecord, EffectDeliveryClass, RunBudget, RunKind, RunState,
    RunUsage,
};
use serde_json::json;

use crate::*;

#[derive(Default)]
struct TestClock {
    now_ms: AtomicI64,
}

impl TestClock {
    fn new(now_ms: i64) -> Self {
        Self {
            now_ms: AtomicI64::new(now_ms),
        }
    }

    fn set(&self, now_ms: i64) {
        self.now_ms.store(now_ms, Ordering::Release);
    }
}

impl SupervisorClock for TestClock {
    fn now_ms(&self) -> i64 {
        self.now_ms.load(Ordering::Acquire)
    }
}

struct FakeBehavior {
    effects: u64,
    delivery: EffectDeliveryClass,
    blocked: AtomicBool,
    fail_until_attempt: u32,
    attempts: AtomicU32,
    invocation_keys: Mutex<Vec<StepId>>,
    committed_keys: Mutex<BTreeSet<StepId>>,
    opened_runs: Mutex<Vec<RunId>>,
}

impl FakeBehavior {
    fn new(effects: u64, delivery: EffectDeliveryClass) -> Self {
        Self {
            effects,
            delivery,
            blocked: AtomicBool::new(false),
            fail_until_attempt: 0,
            attempts: AtomicU32::new(0),
            invocation_keys: Mutex::new(Vec::new()),
            committed_keys: Mutex::new(BTreeSet::new()),
            opened_runs: Mutex::new(Vec::new()),
        }
    }
}

struct FakeFactory {
    behavior: Arc<FakeBehavior>,
}

impl DurableRunDriverFactory for FakeFactory {
    fn open(
        &self,
        run: &DurableRunRecord,
        cancellation: RunCancellation,
    ) -> Result<Box<dyn DurableRunDriver>> {
        self.behavior
            .opened_runs
            .lock()
            .unwrap()
            .push(run.run_id.clone());
        let (phase, event_sequence, initial) = match run.checkpoint.as_ref() {
            Some(checkpoint) => (
                checkpoint["phase"].as_u64().unwrap(),
                checkpoint["event_sequence"].as_u64().unwrap(),
                false,
            ),
            None => (0, 1, true),
        };
        Ok(Box::new(FakeDriver {
            run_id: run.run_id.clone(),
            session_id: run.session_id.clone(),
            turn_id: run.turn_id.clone(),
            behavior: self.behavior.clone(),
            cancellation,
            phase,
            event_sequence,
            events: if initial {
                vec![safe_event(
                    run,
                    1,
                    SafeRuntimeEvent::TurnStarted {
                        user_input_bytes: 4,
                    },
                )]
            } else {
                Vec::new()
            },
            terminal: None,
            usage: run.usage.clone(),
        }))
    }
}

struct FakeDriver {
    run_id: RunId,
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
    behavior: Arc<FakeBehavior>,
    cancellation: RunCancellation,
    phase: u64,
    event_sequence: u64,
    events: Vec<SafeRuntimeEventEnvelope>,
    terminal: Option<SupervisorTerminal>,
    usage: RunUsage,
}

impl DurableRunDriver for FakeDriver {
    fn snapshot(&mut self) -> Result<DriverSnapshot> {
        Ok(DriverSnapshot {
            checkpoint: json!({
                "phase": self.phase,
                "event_sequence": self.event_sequence,
            }),
            pending_effect: self.terminal.is_none().then(|| SupervisorEffect {
                sequence: self.phase + 1,
                kind: "fake_effect".to_string(),
                delivery_class: self.behavior.delivery,
                request: json!({"phase": self.phase}),
            }),
            events: std::mem::take(&mut self.events),
            terminal: self.terminal.clone(),
            usage: self.usage.clone(),
        })
    }

    fn execute_pending_effect(
        &mut self,
        context: &EffectExecutionContext,
    ) -> std::result::Result<serde_json::Value, DriverEffectError> {
        self.behavior.attempts.fetch_add(1, Ordering::SeqCst);
        self.behavior
            .invocation_keys
            .lock()
            .unwrap()
            .push(context.step_id.clone());
        while self.behavior.blocked.load(Ordering::Acquire) && !context.cancellation.is_cancelled()
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        if self.cancellation.is_cancelled() || context.cancellation.is_cancelled() {
            self.event_sequence += 1;
            self.events.push(scoped_event(
                &self.run_id,
                self.session_id.as_ref(),
                self.turn_id.as_ref(),
                self.event_sequence,
                SafeRuntimeEvent::TurnCancelled {
                    reason_code: "turn.cancelled".to_string(),
                },
            ));
            self.event_sequence += 1;
            self.events.push(scoped_event(
                &self.run_id,
                self.session_id.as_ref(),
                self.turn_id.as_ref(),
                self.event_sequence,
                SafeRuntimeEvent::TurnFinished {
                    status: TurnFinishStatus::Cancelled,
                },
            ));
            self.terminal = Some(SupervisorTerminal {
                state: RunState::Cancelled,
                result: None,
                error_code: None,
                error_message: None,
            });
            return Ok(json!({"cancelled": true}));
        }

        self.behavior
            .committed_keys
            .lock()
            .unwrap()
            .insert(context.step_id.clone());
        if context.attempt <= self.behavior.fail_until_attempt {
            return Err(DriverEffectError::new(
                "fake.infrastructure",
                "injected retryable failure",
                true,
            ));
        }
        self.phase += 1;
        self.usage.model_attempts += 1;
        if self.phase == self.behavior.effects {
            self.event_sequence += 1;
            self.events.push(scoped_event(
                &self.run_id,
                self.session_id.as_ref(),
                self.turn_id.as_ref(),
                self.event_sequence,
                SafeRuntimeEvent::AnswerFinal { answer_bytes: 4 },
            ));
            self.event_sequence += 1;
            self.events.push(scoped_event(
                &self.run_id,
                self.session_id.as_ref(),
                self.turn_id.as_ref(),
                self.event_sequence,
                SafeRuntimeEvent::TurnFinished {
                    status: TurnFinishStatus::Answered,
                },
            ));
            self.terminal = Some(SupervisorTerminal {
                state: RunState::Succeeded,
                result: Some(json!({"answer": "done"})),
                error_code: None,
                error_message: None,
            });
        }
        Ok(json!({"phase": self.phase}))
    }
}

#[test]
fn options_require_heartbeat_shorter_than_lease() {
    let options = SupervisorOptions {
        heartbeat_interval: Duration::from_secs(30),
        lease_duration: Duration::from_secs(30),
        ..SupervisorOptions::default()
    };
    assert!(matches!(
        options.validate(),
        Err(SupervisorError::InvalidOptions(_))
    ));
}

#[test]
fn admission_is_immediate_and_active_cancel_is_durable() {
    let root = TempRoot::new("cancel");
    let behavior = Arc::new(FakeBehavior::new(1, EffectDeliveryClass::ReplaySafe));
    behavior.blocked.store(true, Ordering::Release);
    let supervisor = Supervisor::spawn(
        &root.path,
        Arc::new(FakeFactory {
            behavior: behavior.clone(),
        }),
        fast_options(),
    )
    .unwrap();
    let handle = supervisor.handle();
    let draft = turn_draft();

    let accepted = handle
        .submit(RunSpec {
            run: draft.clone(),
            idempotency: None,
        })
        .unwrap();
    assert_eq!(accepted.status.run_id, draft.run_id);
    assert!(!accepted.status.state.is_terminal());
    wait_for_state(&handle, &draft.run_id, RunState::Running);
    wait_for_attempts(&behavior, 1);

    let status = handle.cancel(draft.run_id.clone()).unwrap();
    assert!(status.cancellation_requested);
    behavior.blocked.store(false, Ordering::Release);
    wait_for_state(&handle, &draft.run_id, RunState::Cancelled);

    drop(handle);
    supervisor.shutdown().unwrap();
    let store = AglStore::open_current_at(&root.path).unwrap();
    let persisted = store.safe_run_status(&draft.run_id).unwrap().unwrap();
    assert_eq!(persisted.state, RunState::Cancelled);
    let events = store.run_events_after(&draft.run_id, 0, 10).unwrap();
    assert!(matches!(
        events.last().unwrap().payload,
        SafeRuntimeEvent::TurnFinished {
            status: TurnFinishStatus::Cancelled
        }
    ));
}

#[test]
fn heartbeat_advances_while_effect_is_blocked() {
    let root = TempRoot::new("heartbeat");
    let clock = Arc::new(TestClock::new(1_000));
    let behavior = Arc::new(FakeBehavior::new(1, EffectDeliveryClass::ReplaySafe));
    behavior.blocked.store(true, Ordering::Release);
    let options = SupervisorOptions {
        clock: clock.clone(),
        heartbeat_interval: Duration::from_millis(20),
        lease_duration: Duration::from_millis(100),
        ..fast_options()
    };
    let supervisor = Supervisor::spawn(
        &root.path,
        Arc::new(FakeFactory {
            behavior: behavior.clone(),
        }),
        options,
    )
    .unwrap();
    let handle = supervisor.handle();
    let draft = turn_draft();
    handle
        .submit(RunSpec {
            run: draft.clone(),
            idempotency: None,
        })
        .unwrap();
    wait_for_state(&handle, &draft.run_id, RunState::Running);
    wait_for_attempts(&behavior, 1);
    let before = AglStore::open_current_at(&root.path)
        .unwrap()
        .run(&draft.run_id)
        .unwrap()
        .unwrap()
        .lease_expires_at_ms
        .unwrap();
    clock.set(1_025);
    std::thread::sleep(Duration::from_millis(50));
    let after = AglStore::open_current_at(&root.path)
        .unwrap()
        .run(&draft.run_id)
        .unwrap()
        .unwrap()
        .lease_expires_at_ms
        .unwrap();
    assert!(after > before);

    handle.cancel(draft.run_id.clone()).unwrap();
    behavior.blocked.store(false, Ordering::Release);
    wait_for_state(&handle, &draft.run_id, RunState::Cancelled);
    drop(handle);
    supervisor.shutdown().unwrap();
}

#[test]
fn idempotent_retry_reuses_the_stable_step_key() {
    let root = TempRoot::new("retry");
    let mut configured = FakeBehavior::new(1, EffectDeliveryClass::Idempotent);
    configured.fail_until_attempt = 2;
    let behavior = Arc::new(configured);
    let supervisor = Supervisor::spawn(
        &root.path,
        Arc::new(FakeFactory {
            behavior: behavior.clone(),
        }),
        fast_options(),
    )
    .unwrap();
    let handle = supervisor.handle();
    let draft = turn_draft();
    handle
        .submit(RunSpec {
            run: draft.clone(),
            idempotency: Some(IdempotentRunSpec {
                namespace: "test.run".to_string(),
                key: "stable".to_string(),
                fingerprint: "sha256:stable".to_string(),
            }),
        })
        .unwrap();
    wait_for_state(&handle, &draft.run_id, RunState::Succeeded);
    assert_eq!(behavior.attempts.load(Ordering::SeqCst), 3);
    let keys = behavior.invocation_keys.lock().unwrap();
    assert_eq!(keys.len(), 3);
    assert!(keys.iter().all(|key| key == &keys[0]));
    assert_eq!(behavior.committed_keys.lock().unwrap().len(), 1);

    drop(handle);
    supervisor.shutdown().unwrap();
}

#[test]
fn subscriber_replays_without_gap_and_overflow_does_not_block_run() {
    let root = TempRoot::new("subscription");
    let behavior = Arc::new(FakeBehavior::new(1, EffectDeliveryClass::ReplaySafe));
    behavior.blocked.store(true, Ordering::Release);
    let options = SupervisorOptions {
        subscriber_capacity: 1,
        ..fast_options()
    };
    let supervisor = Supervisor::spawn(
        &root.path,
        Arc::new(FakeFactory {
            behavior: behavior.clone(),
        }),
        options,
    )
    .unwrap();
    let handle = supervisor.handle();
    let draft = turn_draft();
    handle
        .submit(RunSpec {
            run: draft.clone(),
            idempotency: None,
        })
        .unwrap();
    wait_for_event_count(&handle, &draft.run_id, 1);
    let subscription = handle.subscribe(draft.run_id.clone(), 0).unwrap();
    assert_eq!(subscription.backlog.len(), 1);
    behavior.blocked.store(false, Ordering::Release);
    wait_for_state(&handle, &draft.run_id, RunState::Succeeded);

    assert!(subscription.recv().unwrap().is_some());
    assert!(matches!(
        subscription.recv(),
        Err(SupervisorError::SubscriberOverflow { .. })
    ));
    let replay = handle.events_after(draft.run_id.clone(), 1, 10).unwrap();
    assert_eq!(replay.len(), 2);
    assert_eq!(replay[0].sequence, 2);
    assert_eq!(replay[1].sequence, 3);

    drop(handle);
    supervisor.shutdown().unwrap();
}

fn fast_options() -> SupervisorOptions {
    SupervisorOptions {
        worker_limit: 2,
        heartbeat_interval: Duration::from_millis(50),
        lease_duration: Duration::from_millis(500),
        retry_base_delay: Duration::from_millis(2),
        retry_max_delay: Duration::from_millis(5),
        ..SupervisorOptions::default()
    }
}

fn turn_draft() -> DurableRunDraft {
    DurableRunDraft {
        run_id: RunId::generate(),
        session_id: Some(SessionId::generate()),
        turn_id: Some(TurnId::generate()),
        kind: RunKind::Turn,
        priority: 0,
        input: json!({"text": "test"}),
        checkpoint: None,
        effective_policy_hash: None,
        budget: RunBudget::default(),
        not_before_ms: None,
    }
}

fn safe_event(
    run: &DurableRunRecord,
    sequence: u64,
    payload: SafeRuntimeEvent,
) -> SafeRuntimeEventEnvelope {
    scoped_event(
        &run.run_id,
        run.session_id.as_ref(),
        run.turn_id.as_ref(),
        sequence,
        payload,
    )
}

fn scoped_event(
    run_id: &RunId,
    session_id: Option<&SessionId>,
    turn_id: Option<&TurnId>,
    sequence: u64,
    payload: SafeRuntimeEvent,
) -> SafeRuntimeEventEnvelope {
    let mut scope = EventScope::builder(run_id.clone());
    if let Some(session_id) = session_id {
        scope = scope.session_id(session_id.clone());
    }
    if let Some(turn_id) = turn_id {
        scope = scope.turn_id(turn_id.clone());
    }
    SafeRuntimeEventEnvelope {
        schema: agl_events::EVENT_SCHEMA.to_string(),
        event_id: EventId::generate(),
        sequence,
        occurred_at_unix_ms: sequence,
        scope: scope.build().unwrap(),
        request_id: None,
        caused_by: None,
        payload,
    }
}

fn wait_for_state(handle: &SupervisorHandle, run_id: &RunId, state: RunState) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let current = handle.status(run_id.clone()).unwrap().unwrap();
        if current.state == state {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "run stayed in {:?}",
            current.state
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_event_count(handle: &SupervisorHandle, run_id: &RunId, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let events = handle.events_after(run_id.clone(), 0, 100).unwrap();
        if events.len() >= count {
            return;
        }
        assert!(Instant::now() < deadline, "run events were not committed");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_attempts(behavior: &FakeBehavior, count: u32) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while behavior.attempts.load(Ordering::SeqCst) < count {
        assert!(Instant::now() < deadline, "effect did not begin");
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct TempRoot {
    path: std::path::PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "agl-supervisor-{label}-{}-{}",
            std::process::id(),
            RunId::generate()
        ));
        let _ = std::fs::remove_dir_all(&path);
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
