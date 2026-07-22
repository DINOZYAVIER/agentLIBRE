use std::error::Error;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

const MAX_HEALTH_IDENTITY_BYTES: usize = 256;

/// Durable identity for one accelerator/driver/worker-build health domain.
///
/// Process identifiers deliberately do not participate in this key: a worker
/// replacement on the same exact device and software generation must observe
/// the same circuit-breaker state.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct WorkerHealthKey {
    physical_device_id: String,
    driver_build_id: String,
    worker_build_id: String,
}

impl WorkerHealthKey {
    pub fn new(
        physical_device_id: impl Into<String>,
        driver_build_id: impl Into<String>,
        worker_build_id: impl Into<String>,
    ) -> Result<Self, WorkerIdentityError> {
        let key = Self {
            physical_device_id: physical_device_id.into(),
            driver_build_id: driver_build_id.into(),
            worker_build_id: worker_build_id.into(),
        };
        validate_identity_component(WorkerIdentityField::PhysicalDevice, &key.physical_device_id)?;
        validate_identity_component(WorkerIdentityField::DriverBuild, &key.driver_build_id)?;
        validate_identity_component(WorkerIdentityField::WorkerBuild, &key.worker_build_id)?;
        Ok(key)
    }

    pub fn physical_device_id(&self) -> &str {
        &self.physical_device_id
    }

    pub fn driver_build_id(&self) -> &str {
        &self.driver_build_id
    }

    pub fn worker_build_id(&self) -> &str {
        &self.worker_build_id
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerHealthKeyRepr {
    physical_device_id: String,
    driver_build_id: String,
    worker_build_id: String,
}

impl<'de> Deserialize<'de> for WorkerHealthKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = WorkerHealthKeyRepr::deserialize(deserializer)?;
        Self::new(
            value.physical_device_id,
            value.driver_build_id,
            value.worker_build_id,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Exact identity for one child process epoch.
///
/// `launch_generation` is allocated by the host and must never be reused by
/// that host. Pairing it with the PID prevents PID reuse from making a late
/// event look current; pairing it with the immutable worker build prevents a
/// mixed runtime generation from being admitted.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct WorkerGenerationIdentity {
    pid: u32,
    launch_generation: u64,
    worker_build_id: String,
}

impl WorkerGenerationIdentity {
    pub fn new(
        pid: u32,
        launch_generation: u64,
        worker_build_id: impl Into<String>,
    ) -> Result<Self, WorkerIdentityError> {
        if pid == 0 {
            return Err(WorkerIdentityError::InvalidProcessId);
        }
        if launch_generation == 0 {
            return Err(WorkerIdentityError::InvalidLaunchGeneration);
        }
        let worker_build_id = worker_build_id.into();
        validate_identity_component(WorkerIdentityField::WorkerBuild, &worker_build_id)?;
        Ok(Self {
            pid,
            launch_generation,
            worker_build_id,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn launch_generation(&self) -> u64 {
        self.launch_generation
    }

    pub fn worker_build_id(&self) -> &str {
        &self.worker_build_id
    }
}

/// Host-assigned identity for one dispatched native attempt.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ActiveAttemptIdentity(u64);

impl ActiveAttemptIdentity {
    pub fn new(value: u64) -> Result<Self, WorkerIdentityError> {
        if value == 0 {
            return Err(WorkerIdentityError::InvalidAttemptGeneration);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerCircuitBreakerPolicy {
    initial_cooldown_ms: u64,
    maximum_cooldown_ms: u64,
    maximum_crash_streak: u8,
}

impl WorkerCircuitBreakerPolicy {
    pub fn new(
        initial_cooldown_ms: u64,
        maximum_cooldown_ms: u64,
        maximum_crash_streak: u8,
    ) -> Result<Self, WorkerCircuitBreakerPolicyError> {
        if initial_cooldown_ms == 0 {
            return Err(WorkerCircuitBreakerPolicyError::ZeroInitialCooldown);
        }
        if maximum_cooldown_ms < initial_cooldown_ms {
            return Err(WorkerCircuitBreakerPolicyError::MaximumBelowInitial);
        }
        if maximum_crash_streak == 0 {
            return Err(WorkerCircuitBreakerPolicyError::ZeroMaximumCrashStreak);
        }
        Ok(Self {
            initial_cooldown_ms,
            maximum_cooldown_ms,
            maximum_crash_streak,
        })
    }

    pub fn initial_cooldown_ms(self) -> u64 {
        self.initial_cooldown_ms
    }

    pub fn maximum_cooldown_ms(self) -> u64 {
        self.maximum_cooldown_ms
    }

    pub fn maximum_crash_streak(self) -> u8 {
        self.maximum_crash_streak
    }

    fn cooldown_ms(self, crash_streak: u8) -> u64 {
        debug_assert!(crash_streak > 0);
        let shift = u32::from(crash_streak.saturating_sub(1)).min(63);
        self.initial_cooldown_ms
            .checked_mul(1_u64 << shift)
            .unwrap_or(self.maximum_cooldown_ms)
            .min(self.maximum_cooldown_ms)
    }
}

/// The only supervisor state which is persisted across host processes.
///
/// The failure timestamp makes the serialized cooldown independently
/// verifiable: a corrupt record cannot extend the circuit beyond the selected
/// maximum merely by supplying an arbitrary `not_before` value.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerHealthState {
    key: WorkerHealthKey,
    crash_streak: u8,
    last_failure_at_unix_ms: Option<u64>,
    cooldown_not_before_unix_ms: Option<u64>,
}

impl WorkerHealthState {
    pub fn new(key: WorkerHealthKey) -> Self {
        Self {
            key,
            crash_streak: 0,
            last_failure_at_unix_ms: None,
            cooldown_not_before_unix_ms: None,
        }
    }

    pub fn key(&self) -> &WorkerHealthKey {
        &self.key
    }

    pub fn crash_streak(&self) -> u8 {
        self.crash_streak
    }

    pub fn last_failure_at_unix_ms(&self) -> Option<u64> {
        self.last_failure_at_unix_ms
    }

    pub fn cooldown_not_before_unix_ms(&self) -> Option<u64> {
        self.cooldown_not_before_unix_ms
    }

    pub fn validate(
        &self,
        policy: WorkerCircuitBreakerPolicy,
    ) -> Result<(), WorkerHealthStateError> {
        if self.crash_streak > policy.maximum_crash_streak {
            return Err(WorkerHealthStateError::CrashStreakExceedsPolicy);
        }
        match (
            self.crash_streak,
            self.last_failure_at_unix_ms,
            self.cooldown_not_before_unix_ms,
        ) {
            (0, None, None) => Ok(()),
            (0, _, _) | (_, None, _) | (_, _, None) => {
                Err(WorkerHealthStateError::IncompleteCooldownRecord)
            }
            (_, Some(failed_at), Some(not_before)) => {
                let duration = not_before
                    .checked_sub(failed_at)
                    .ok_or(WorkerHealthStateError::InvalidCooldownDeadline)?;
                if duration != policy.cooldown_ms(self.crash_streak) {
                    return Err(WorkerHealthStateError::CooldownOutsidePolicy);
                }
                Ok(())
            }
        }
    }

    fn record_failure(
        &mut self,
        policy: WorkerCircuitBreakerPolicy,
        now_unix_ms: u64,
    ) -> Result<u64, WorkerSupervisorError> {
        self.validate(policy)
            .map_err(WorkerSupervisorError::InvalidPersistedHealth)?;
        let crash_streak = self
            .crash_streak
            .saturating_add(1)
            .min(policy.maximum_crash_streak);
        let cooldown_ms = policy.cooldown_ms(crash_streak);
        let not_before = now_unix_ms
            .checked_add(cooldown_ms)
            .ok_or(WorkerSupervisorError::ClockOverflow)?;
        self.crash_streak = crash_streak;
        self.last_failure_at_unix_ms = Some(now_unix_ms);
        self.cooldown_not_before_unix_ms = Some(not_before);
        Ok(not_before)
    }

    fn record_success(&mut self) {
        self.crash_streak = 0;
        self.last_failure_at_unix_ms = None;
        self.cooldown_not_before_unix_ms = None;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLifecyclePhase {
    Cold,
    Starting,
    Ready,
    Busy,
    CoolingDown,
}

impl WorkerLifecyclePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Busy => "busy",
            Self::CoolingDown => "cooling_down",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFailureKind {
    SpawnFailed,
    HandshakeFailed,
    StartupTimedOut,
    ProtocolViolation,
    Exited,
    Signaled,
    DeviceLost,
    ForcedAfterCancellation,
    ForcedAfterDeadline,
    ReapTimedOut,
}

impl WorkerFailureKind {
    pub fn code(self) -> &'static str {
        match self {
            Self::SpawnFailed => "inference_worker_spawn_failed",
            Self::HandshakeFailed => "inference_worker_handshake_failed",
            Self::StartupTimedOut => "inference_worker_startup_timed_out",
            Self::ProtocolViolation => "inference_worker_protocol_violation",
            Self::Exited => "inference_worker_exited",
            Self::Signaled => "inference_worker_signaled",
            Self::DeviceLost => "inference_device_lost",
            Self::ForcedAfterCancellation => "inference_worker_forced_after_cancel",
            Self::ForcedAfterDeadline => "inference_worker_forced_after_deadline",
            Self::ReapTimedOut => "inference_worker_reap_timed_out",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ActiveTerminalOutcome {
    Succeeded,
    BackendLost { failure: WorkerFailureKind },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ActiveTerminalRecord {
    pub worker: WorkerGenerationIdentity,
    pub attempt: ActiveAttemptIdentity,
    pub outcome: ActiveTerminalOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingQueueDisposition {
    Preserve,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkerFailureEffect {
    pub active_terminal: Option<ActiveTerminalRecord>,
    pub pending_queue: PendingQueueDisposition,
    pub crash_streak: u8,
    pub cooldown_not_before_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkerLifecycleState {
    Cold,
    Starting {
        worker: WorkerGenerationIdentity,
    },
    Ready {
        worker: WorkerGenerationIdentity,
    },
    Busy {
        worker: WorkerGenerationIdentity,
        attempt: ActiveAttemptIdentity,
    },
    CoolingDown {
        not_before_unix_ms: u64,
    },
}

impl WorkerLifecycleState {
    fn phase(&self) -> WorkerLifecyclePhase {
        match self {
            Self::Cold => WorkerLifecyclePhase::Cold,
            Self::Starting { .. } => WorkerLifecyclePhase::Starting,
            Self::Ready { .. } => WorkerLifecyclePhase::Ready,
            Self::Busy { .. } => WorkerLifecyclePhase::Busy,
            Self::CoolingDown { .. } => WorkerLifecyclePhase::CoolingDown,
        }
    }

    fn worker(&self) -> Option<&WorkerGenerationIdentity> {
        match self {
            Self::Starting { worker } | Self::Ready { worker } | Self::Busy { worker, .. } => {
                Some(worker)
            }
            Self::Cold | Self::CoolingDown { .. } => None,
        }
    }
}

/// Pure host-side lifecycle and circuit-breaker state.
///
/// The pending inference queue is intentionally absent. A worker failure
/// returns an explicit [`PendingQueueDisposition::Preserve`] effect and can
/// close only the active attempt held in `Busy`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerSupervisorState {
    policy: WorkerCircuitBreakerPolicy,
    health: WorkerHealthState,
    lifecycle: WorkerLifecycleState,
    last_reaped_worker: Option<WorkerGenerationIdentity>,
    last_terminal: Option<ActiveTerminalRecord>,
}

impl WorkerSupervisorState {
    pub fn restore(
        health: WorkerHealthState,
        policy: WorkerCircuitBreakerPolicy,
        now_unix_ms: u64,
    ) -> Result<Self, WorkerSupervisorError> {
        health
            .validate(policy)
            .map_err(WorkerSupervisorError::InvalidPersistedHealth)?;
        let lifecycle = match health.cooldown_not_before_unix_ms {
            Some(not_before) if now_unix_ms < not_before => WorkerLifecycleState::CoolingDown {
                not_before_unix_ms: not_before,
            },
            _ => WorkerLifecycleState::Cold,
        };
        Ok(Self {
            policy,
            health,
            lifecycle,
            last_reaped_worker: None,
            last_terminal: None,
        })
    }

    pub fn phase(&self) -> WorkerLifecyclePhase {
        self.lifecycle.phase()
    }

    pub fn health(&self) -> &WorkerHealthState {
        &self.health
    }

    pub fn current_worker(&self) -> Option<&WorkerGenerationIdentity> {
        self.lifecycle.worker()
    }

    pub fn active_attempt(&self) -> Option<ActiveAttemptIdentity> {
        match self.lifecycle {
            WorkerLifecycleState::Busy { attempt, .. } => Some(attempt),
            _ => None,
        }
    }

    pub fn cooldown_not_before_unix_ms(&self) -> Option<u64> {
        match self.lifecycle {
            WorkerLifecycleState::CoolingDown { not_before_unix_ms } => Some(not_before_unix_ms),
            _ => None,
        }
    }

    pub fn last_terminal(&self) -> Option<&ActiveTerminalRecord> {
        self.last_terminal.as_ref()
    }

    pub fn begin_start(
        &mut self,
        worker: WorkerGenerationIdentity,
    ) -> Result<(), WorkerSupervisorError> {
        if self.lifecycle.phase() != WorkerLifecyclePhase::Cold {
            return Err(WorkerSupervisorError::UnexpectedTransition {
                phase: self.lifecycle.phase(),
                action: WorkerSupervisorAction::BeginStart,
            });
        }
        if worker.worker_build_id != self.health.key.worker_build_id {
            return Err(WorkerSupervisorError::WorkerBuildMismatch);
        }
        if self.last_reaped_worker.as_ref() == Some(&worker) {
            return Err(WorkerSupervisorError::WorkerGenerationReused);
        }
        self.lifecycle = WorkerLifecycleState::Starting { worker };
        Ok(())
    }

    pub fn mark_ready(
        &mut self,
        worker: &WorkerGenerationIdentity,
    ) -> Result<(), WorkerSupervisorError> {
        match &self.lifecycle {
            WorkerLifecycleState::Starting { worker: current } if current == worker => {
                self.lifecycle = WorkerLifecycleState::Ready {
                    worker: worker.clone(),
                };
                Ok(())
            }
            WorkerLifecycleState::Starting { .. }
            | WorkerLifecycleState::Ready { .. }
            | WorkerLifecycleState::Busy { .. }
                if self
                    .lifecycle
                    .worker()
                    .is_some_and(|current| current != worker) =>
            {
                Err(WorkerSupervisorError::StaleWorkerGeneration)
            }
            _ => Err(WorkerSupervisorError::UnexpectedTransition {
                phase: self.lifecycle.phase(),
                action: WorkerSupervisorAction::MarkReady,
            }),
        }
    }

    pub fn begin_attempt(
        &mut self,
        worker: &WorkerGenerationIdentity,
        attempt: ActiveAttemptIdentity,
    ) -> Result<(), WorkerSupervisorError> {
        match &self.lifecycle {
            WorkerLifecycleState::Ready { worker: current } if current == worker => {
                if self
                    .last_terminal
                    .as_ref()
                    .is_some_and(|terminal| terminal.attempt == attempt)
                {
                    return Err(WorkerSupervisorError::AttemptGenerationReused);
                }
                self.lifecycle = WorkerLifecycleState::Busy {
                    worker: worker.clone(),
                    attempt,
                };
                Ok(())
            }
            WorkerLifecycleState::Starting { .. }
            | WorkerLifecycleState::Ready { .. }
            | WorkerLifecycleState::Busy { .. }
                if self
                    .lifecycle
                    .worker()
                    .is_some_and(|current| current != worker) =>
            {
                Err(WorkerSupervisorError::StaleWorkerGeneration)
            }
            _ => Err(WorkerSupervisorError::UnexpectedTransition {
                phase: self.lifecycle.phase(),
                action: WorkerSupervisorAction::BeginAttempt,
            }),
        }
    }

    pub fn complete_active_success(
        &mut self,
        worker: &WorkerGenerationIdentity,
        attempt: ActiveAttemptIdentity,
    ) -> Result<ActiveTerminalRecord, WorkerSupervisorError> {
        self.validate_active_event(worker, attempt)?;
        let terminal = ActiveTerminalRecord {
            worker: worker.clone(),
            attempt,
            outcome: ActiveTerminalOutcome::Succeeded,
        };
        let mut health = self.health.clone();
        health.record_success();
        self.health = health;
        self.lifecycle = WorkerLifecycleState::Ready {
            worker: worker.clone(),
        };
        self.last_terminal = Some(terminal.clone());
        Ok(terminal)
    }

    /// Ends an application attempt which failed without losing the worker.
    ///
    /// The application/evidence owner records that terminal result. The
    /// supervisor only returns the exact worker to `ready`; a non-backend
    /// failure is not a clean inference success and therefore does not reset a
    /// prior crash streak.
    pub fn end_active_without_worker_failure(
        &mut self,
        worker: &WorkerGenerationIdentity,
        attempt: ActiveAttemptIdentity,
    ) -> Result<(), WorkerSupervisorError> {
        self.validate_active_event(worker, attempt)?;
        self.lifecycle = WorkerLifecycleState::Ready {
            worker: worker.clone(),
        };
        Ok(())
    }

    /// Retires an exact healthy process generation without opening cooldown.
    pub fn retire_worker(
        &mut self,
        worker: &WorkerGenerationIdentity,
    ) -> Result<(), WorkerSupervisorError> {
        match &self.lifecycle {
            WorkerLifecycleState::Starting { worker: current }
            | WorkerLifecycleState::Ready { worker: current }
                if current == worker =>
            {
                self.lifecycle = WorkerLifecycleState::Cold;
                self.last_reaped_worker = Some(worker.clone());
                Ok(())
            }
            WorkerLifecycleState::Starting { .. }
            | WorkerLifecycleState::Ready { .. }
            | WorkerLifecycleState::Busy { .. } => {
                Err(WorkerSupervisorError::StaleWorkerGeneration)
            }
            WorkerLifecycleState::Cold | WorkerLifecycleState::CoolingDown { .. } => {
                Err(WorkerSupervisorError::UnexpectedTransition {
                    phase: self.lifecycle.phase(),
                    action: WorkerSupervisorAction::RetireWorker,
                })
            }
        }
    }

    /// Records a failure before a trustworthy child identity was available.
    /// Spawn and handshake failures still participate in the durable circuit
    /// breaker, but cannot fabricate a PID/generation receipt.
    pub fn record_start_failure(
        &mut self,
        now_unix_ms: u64,
    ) -> Result<WorkerFailureEffect, WorkerSupervisorError> {
        if self.lifecycle.phase() != WorkerLifecyclePhase::Cold {
            return Err(WorkerSupervisorError::UnexpectedTransition {
                phase: self.lifecycle.phase(),
                action: WorkerSupervisorAction::RecordStartFailure,
            });
        }
        let mut health = self.health.clone();
        let not_before = health.record_failure(self.policy, now_unix_ms)?;
        self.health = health;
        self.lifecycle = WorkerLifecycleState::CoolingDown {
            not_before_unix_ms: not_before,
        };
        Ok(WorkerFailureEffect {
            active_terminal: None,
            pending_queue: PendingQueueDisposition::Preserve,
            crash_streak: self.health.crash_streak,
            cooldown_not_before_unix_ms: not_before,
        })
    }

    pub fn record_worker_failure(
        &mut self,
        worker: &WorkerGenerationIdentity,
        failure: WorkerFailureKind,
        now_unix_ms: u64,
    ) -> Result<WorkerFailureEffect, WorkerSupervisorError> {
        let active_attempt = match &self.lifecycle {
            WorkerLifecycleState::Starting { worker: current }
            | WorkerLifecycleState::Ready { worker: current }
                if current == worker =>
            {
                None
            }
            WorkerLifecycleState::Busy {
                worker: current,
                attempt,
            } if current == worker => Some(*attempt),
            WorkerLifecycleState::Starting { .. }
            | WorkerLifecycleState::Ready { .. }
            | WorkerLifecycleState::Busy { .. } => {
                return Err(WorkerSupervisorError::StaleWorkerGeneration);
            }
            WorkerLifecycleState::Cold | WorkerLifecycleState::CoolingDown { .. }
                if self.last_reaped_worker.as_ref() == Some(worker) =>
            {
                return Err(WorkerSupervisorError::DuplicateWorkerFailure);
            }
            WorkerLifecycleState::Cold | WorkerLifecycleState::CoolingDown { .. } => {
                return Err(WorkerSupervisorError::StaleWorkerGeneration);
            }
        };

        let mut health = self.health.clone();
        let not_before = health.record_failure(self.policy, now_unix_ms)?;
        let active_terminal = active_attempt.map(|attempt| ActiveTerminalRecord {
            worker: worker.clone(),
            attempt,
            outcome: ActiveTerminalOutcome::BackendLost { failure },
        });

        self.health = health;
        self.lifecycle = WorkerLifecycleState::CoolingDown {
            not_before_unix_ms: not_before,
        };
        self.last_reaped_worker = Some(worker.clone());
        if let Some(terminal) = &active_terminal {
            self.last_terminal = Some(terminal.clone());
        }

        Ok(WorkerFailureEffect {
            active_terminal,
            pending_queue: PendingQueueDisposition::Preserve,
            crash_streak: self.health.crash_streak,
            cooldown_not_before_unix_ms: not_before,
        })
    }

    pub fn release_cooldown(&mut self, now_unix_ms: u64) -> Result<(), WorkerSupervisorError> {
        match self.lifecycle {
            WorkerLifecycleState::CoolingDown { not_before_unix_ms }
                if now_unix_ms >= not_before_unix_ms =>
            {
                self.lifecycle = WorkerLifecycleState::Cold;
                Ok(())
            }
            WorkerLifecycleState::CoolingDown { not_before_unix_ms } => {
                Err(WorkerSupervisorError::CooldownActive { not_before_unix_ms })
            }
            _ => Err(WorkerSupervisorError::UnexpectedTransition {
                phase: self.lifecycle.phase(),
                action: WorkerSupervisorAction::ReleaseCooldown,
            }),
        }
    }

    fn validate_active_event(
        &self,
        worker: &WorkerGenerationIdentity,
        attempt: ActiveAttemptIdentity,
    ) -> Result<(), WorkerSupervisorError> {
        match &self.lifecycle {
            WorkerLifecycleState::Busy {
                worker: current,
                attempt: current_attempt,
            } if current != worker => Err(WorkerSupervisorError::StaleWorkerGeneration),
            WorkerLifecycleState::Busy {
                attempt: current_attempt,
                ..
            } if *current_attempt != attempt => {
                if self.is_duplicate_terminal(worker, attempt) {
                    Err(WorkerSupervisorError::DuplicateTerminalEvent)
                } else {
                    Err(WorkerSupervisorError::StaleAttemptEvent)
                }
            }
            WorkerLifecycleState::Busy { .. } => Ok(()),
            WorkerLifecycleState::Starting { worker: current }
            | WorkerLifecycleState::Ready { worker: current }
                if current != worker =>
            {
                Err(WorkerSupervisorError::StaleWorkerGeneration)
            }
            WorkerLifecycleState::Starting { .. } | WorkerLifecycleState::Ready { .. }
                if self.is_duplicate_terminal(worker, attempt) =>
            {
                Err(WorkerSupervisorError::DuplicateTerminalEvent)
            }
            WorkerLifecycleState::Starting { .. } | WorkerLifecycleState::Ready { .. } => {
                Err(WorkerSupervisorError::StaleAttemptEvent)
            }
            WorkerLifecycleState::Cold | WorkerLifecycleState::CoolingDown { .. }
                if self.is_duplicate_terminal(worker, attempt) =>
            {
                Err(WorkerSupervisorError::DuplicateTerminalEvent)
            }
            WorkerLifecycleState::Cold | WorkerLifecycleState::CoolingDown { .. } => {
                Err(WorkerSupervisorError::StaleWorkerGeneration)
            }
        }
    }

    fn is_duplicate_terminal(
        &self,
        worker: &WorkerGenerationIdentity,
        attempt: ActiveAttemptIdentity,
    ) -> bool {
        self.last_terminal
            .as_ref()
            .is_some_and(|terminal| terminal.worker == *worker && terminal.attempt == attempt)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerSupervisorAction {
    BeginStart,
    MarkReady,
    BeginAttempt,
    RecordStartFailure,
    ReleaseCooldown,
    RetireWorker,
}

impl WorkerSupervisorAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::BeginStart => "begin_start",
            Self::MarkReady => "mark_ready",
            Self::BeginAttempt => "begin_attempt",
            Self::RecordStartFailure => "record_start_failure",
            Self::ReleaseCooldown => "release_cooldown",
            Self::RetireWorker => "retire_worker",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerIdentityField {
    PhysicalDevice,
    DriverBuild,
    WorkerBuild,
}

impl WorkerIdentityField {
    fn as_str(self) -> &'static str {
        match self {
            Self::PhysicalDevice => "physical device identity",
            Self::DriverBuild => "driver build identity",
            Self::WorkerBuild => "worker build identity",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerIdentityError {
    InvalidComponent { field: WorkerIdentityField },
    InvalidProcessId,
    InvalidLaunchGeneration,
    InvalidAttemptGeneration,
}

impl WorkerIdentityError {
    pub fn code(&self) -> &'static str {
        "inference_worker_identity_invalid"
    }
}

impl fmt::Display for WorkerIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidComponent { field } => {
                write!(formatter, "invalid {}", field.as_str())
            }
            Self::InvalidProcessId => formatter.write_str("invalid worker process id"),
            Self::InvalidLaunchGeneration => {
                formatter.write_str("invalid worker launch generation")
            }
            Self::InvalidAttemptGeneration => {
                formatter.write_str("invalid worker attempt generation")
            }
        }
    }
}

impl Error for WorkerIdentityError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerCircuitBreakerPolicyError {
    ZeroInitialCooldown,
    MaximumBelowInitial,
    ZeroMaximumCrashStreak,
}

impl WorkerCircuitBreakerPolicyError {
    pub fn code(self) -> &'static str {
        "inference_worker_circuit_policy_invalid"
    }
}

impl fmt::Display for WorkerCircuitBreakerPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroInitialCooldown => {
                formatter.write_str("initial worker cooldown must be non-zero")
            }
            Self::MaximumBelowInitial => {
                formatter.write_str("maximum worker cooldown is below its initial value")
            }
            Self::ZeroMaximumCrashStreak => {
                formatter.write_str("maximum worker crash streak must be non-zero")
            }
        }
    }
}

impl Error for WorkerCircuitBreakerPolicyError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerHealthStateError {
    CrashStreakExceedsPolicy,
    IncompleteCooldownRecord,
    InvalidCooldownDeadline,
    CooldownOutsidePolicy,
}

impl WorkerHealthStateError {
    pub fn code(self) -> &'static str {
        "inference_worker_health_invalid"
    }
}

impl fmt::Display for WorkerHealthStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CrashStreakExceedsPolicy => {
                formatter.write_str("worker crash streak exceeds its configured bound")
            }
            Self::IncompleteCooldownRecord => {
                formatter.write_str("worker cooldown record is incomplete")
            }
            Self::InvalidCooldownDeadline => {
                formatter.write_str("worker cooldown deadline precedes its failure")
            }
            Self::CooldownOutsidePolicy => {
                formatter.write_str("worker cooldown is outside its configured bounds")
            }
        }
    }
}

impl Error for WorkerHealthStateError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerSupervisorError {
    InvalidPersistedHealth(WorkerHealthStateError),
    WorkerBuildMismatch,
    WorkerGenerationReused,
    AttemptGenerationReused,
    StaleWorkerGeneration,
    StaleAttemptEvent,
    DuplicateTerminalEvent,
    DuplicateWorkerFailure,
    CooldownActive {
        not_before_unix_ms: u64,
    },
    ClockOverflow,
    UnexpectedTransition {
        phase: WorkerLifecyclePhase,
        action: WorkerSupervisorAction,
    },
}

impl WorkerSupervisorError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidPersistedHealth(_) => "inference_worker_health_invalid",
            Self::WorkerBuildMismatch => "inference_worker_build_mismatch",
            Self::WorkerGenerationReused => "inference_worker_generation_reused",
            Self::AttemptGenerationReused => "inference_attempt_generation_reused",
            Self::StaleWorkerGeneration => "inference_worker_event_stale",
            Self::StaleAttemptEvent => "inference_attempt_event_stale",
            Self::DuplicateTerminalEvent => "inference_attempt_terminal_duplicate",
            Self::DuplicateWorkerFailure => "inference_worker_failure_duplicate",
            Self::CooldownActive { .. } => "inference_worker_cooling_down",
            Self::ClockOverflow => "inference_worker_clock_overflow",
            Self::UnexpectedTransition { .. } => "inference_worker_transition_invalid",
        }
    }
}

impl fmt::Display for WorkerSupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPersistedHealth(error) => write!(formatter, "{error}"),
            Self::WorkerBuildMismatch => {
                formatter.write_str("worker build does not match the health generation")
            }
            Self::WorkerGenerationReused => {
                formatter.write_str("reaped worker generation cannot be reused")
            }
            Self::AttemptGenerationReused => {
                formatter.write_str("finished attempt generation cannot be reused")
            }
            Self::StaleWorkerGeneration => {
                formatter.write_str("event belongs to a stale worker generation")
            }
            Self::StaleAttemptEvent => formatter.write_str("event belongs to a stale attempt"),
            Self::DuplicateTerminalEvent => {
                formatter.write_str("attempt already has a terminal outcome")
            }
            Self::DuplicateWorkerFailure => {
                formatter.write_str("worker failure was already recorded")
            }
            Self::CooldownActive { .. } => {
                formatter.write_str("worker health generation is cooling down")
            }
            Self::ClockOverflow => formatter.write_str("worker cooldown deadline overflowed"),
            Self::UnexpectedTransition { phase, action } => write!(
                formatter,
                "worker action `{}` is invalid while lifecycle is `{}`",
                action.as_str(),
                phase.as_str()
            ),
        }
    }
}

impl Error for WorkerSupervisorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidPersistedHealth(error) => Some(error),
            _ => None,
        }
    }
}

fn validate_identity_component(
    field: WorkerIdentityField,
    value: &str,
) -> Result<(), WorkerIdentityError> {
    if value.trim().is_empty()
        || value.len() > MAX_HEALTH_IDENTITY_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(WorkerIdentityError::InvalidComponent { field });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    const BUILD: &str = "worker-build-150";

    fn policy() -> WorkerCircuitBreakerPolicy {
        WorkerCircuitBreakerPolicy::new(100, 250, 3).unwrap()
    }

    fn key() -> WorkerHealthKey {
        WorkerHealthKey::new("pci-0000:03:00.0", "radv-26.1", BUILD).unwrap()
    }

    fn worker(pid: u32, generation: u64) -> WorkerGenerationIdentity {
        WorkerGenerationIdentity::new(pid, generation, BUILD).unwrap()
    }

    fn attempt(value: u64) -> ActiveAttemptIdentity {
        ActiveAttemptIdentity::new(value).unwrap()
    }

    fn cold_supervisor() -> WorkerSupervisorState {
        WorkerSupervisorState::restore(WorkerHealthState::new(key()), policy(), 1_000).unwrap()
    }

    fn make_ready(supervisor: &mut WorkerSupervisorState, worker: &WorkerGenerationIdentity) {
        supervisor.begin_start(worker.clone()).unwrap();
        assert_eq!(supervisor.phase(), WorkerLifecyclePhase::Starting);
        supervisor.mark_ready(worker).unwrap();
        assert_eq!(supervisor.phase(), WorkerLifecyclePhase::Ready);
    }

    #[test]
    fn lifecycle_covers_success_failure_cooldown_and_cold_restart() {
        let mut supervisor = cold_supervisor();
        let first = worker(41, 1);
        assert_eq!(supervisor.phase(), WorkerLifecyclePhase::Cold);
        make_ready(&mut supervisor, &first);

        supervisor.begin_attempt(&first, attempt(1)).unwrap();
        assert_eq!(supervisor.phase(), WorkerLifecyclePhase::Busy);
        let success = supervisor
            .complete_active_success(&first, attempt(1))
            .unwrap();
        assert_eq!(success.outcome, ActiveTerminalOutcome::Succeeded);
        assert_eq!(supervisor.phase(), WorkerLifecyclePhase::Ready);

        supervisor.begin_attempt(&first, attempt(2)).unwrap();
        let failure = supervisor
            .record_worker_failure(&first, WorkerFailureKind::DeviceLost, 2_000)
            .unwrap();
        assert_eq!(supervisor.phase(), WorkerLifecyclePhase::CoolingDown);
        assert_eq!(failure.pending_queue, PendingQueueDisposition::Preserve);
        assert_eq!(failure.cooldown_not_before_unix_ms, 2_100);
        assert_eq!(
            failure.active_terminal.unwrap().outcome,
            ActiveTerminalOutcome::BackendLost {
                failure: WorkerFailureKind::DeviceLost,
            }
        );
        assert!(matches!(
            supervisor.release_cooldown(2_099),
            Err(WorkerSupervisorError::CooldownActive { .. })
        ));
        supervisor.release_cooldown(2_100).unwrap();
        assert_eq!(supervisor.phase(), WorkerLifecyclePhase::Cold);

        let second = worker(42, 2);
        make_ready(&mut supervisor, &second);
    }

    #[test]
    fn pid_reuse_does_not_accept_a_late_worker_generation() {
        let mut supervisor = cold_supervisor();
        let old = worker(77, 10);
        make_ready(&mut supervisor, &old);
        supervisor.begin_attempt(&old, attempt(10)).unwrap();
        supervisor
            .record_worker_failure(&old, WorkerFailureKind::Exited, 1_000)
            .unwrap();
        supervisor.release_cooldown(1_100).unwrap();

        let replacement = worker(77, 11);
        make_ready(&mut supervisor, &replacement);
        supervisor.begin_attempt(&replacement, attempt(11)).unwrap();

        assert_eq!(
            supervisor.complete_active_success(&old, attempt(10)),
            Err(WorkerSupervisorError::StaleWorkerGeneration)
        );
        assert_eq!(supervisor.active_attempt(), Some(attempt(11)));
        assert_eq!(supervisor.current_worker(), Some(&replacement));
    }

    #[test]
    fn active_attempt_has_exactly_one_terminal_outcome() {
        let mut supervisor = cold_supervisor();
        let worker = worker(81, 1);
        make_ready(&mut supervisor, &worker);
        supervisor.begin_attempt(&worker, attempt(1)).unwrap();

        let terminal = supervisor
            .complete_active_success(&worker, attempt(1))
            .unwrap();
        assert_eq!(
            supervisor.complete_active_success(&worker, attempt(1)),
            Err(WorkerSupervisorError::DuplicateTerminalEvent)
        );
        assert_eq!(supervisor.last_terminal(), Some(&terminal));

        supervisor.begin_attempt(&worker, attempt(2)).unwrap();
        let failure = supervisor
            .record_worker_failure(&worker, WorkerFailureKind::Signaled, 5_000)
            .unwrap();
        assert!(failure.active_terminal.is_some());
        assert_eq!(
            supervisor.record_worker_failure(&worker, WorkerFailureKind::Signaled, 5_001),
            Err(WorkerSupervisorError::DuplicateWorkerFailure)
        );
        assert_eq!(
            supervisor.complete_active_success(&worker, attempt(2)),
            Err(WorkerSupervisorError::DuplicateTerminalEvent)
        );
        assert_eq!(supervisor.health().crash_streak(), 1);
    }

    #[test]
    fn ordinary_attempt_failure_can_retire_worker_without_opening_cooldown() {
        let mut supervisor = cold_supervisor();
        let worker = worker(82, 1);
        make_ready(&mut supervisor, &worker);
        supervisor.begin_attempt(&worker, attempt(1)).unwrap();

        supervisor
            .end_active_without_worker_failure(&worker, attempt(1))
            .unwrap();
        assert_eq!(supervisor.phase(), WorkerLifecyclePhase::Ready);
        supervisor.retire_worker(&worker).unwrap();
        assert_eq!(supervisor.phase(), WorkerLifecyclePhase::Cold);
        assert_eq!(supervisor.health().crash_streak(), 0);
        assert_eq!(supervisor.cooldown_not_before_unix_ms(), None);

        assert_eq!(
            supervisor.complete_active_success(&worker, attempt(1)),
            Err(WorkerSupervisorError::StaleWorkerGeneration)
        );
    }

    #[test]
    fn spawn_failure_has_no_fabricated_worker_identity_and_is_durable() {
        let mut supervisor = cold_supervisor();
        let effect = supervisor.record_start_failure(4_000).unwrap();

        assert_eq!(effect.active_terminal, None);
        assert_eq!(effect.pending_queue, PendingQueueDisposition::Preserve);
        assert_eq!(effect.cooldown_not_before_unix_ms, 4_100);
        assert_eq!(supervisor.current_worker(), None);
        assert_eq!(supervisor.phase(), WorkerLifecyclePhase::CoolingDown);
        assert_eq!(
            supervisor.record_start_failure(4_001),
            Err(WorkerSupervisorError::UnexpectedTransition {
                phase: WorkerLifecyclePhase::CoolingDown,
                action: WorkerSupervisorAction::RecordStartFailure,
            })
        );

        let restored =
            WorkerSupervisorState::restore(supervisor.health().clone(), policy(), 4_050).unwrap();
        assert_eq!(restored.phase(), WorkerLifecyclePhase::CoolingDown);
        assert_eq!(restored.cooldown_not_before_unix_ms(), Some(4_100));
    }

    #[test]
    fn crash_streak_and_exponential_cooldown_are_bounded() {
        let mut supervisor = cold_supervisor();
        let expected = [(1, 100), (2, 200), (3, 250), (3, 250), (3, 250)];
        let mut now = 10_000;

        for (index, (expected_streak, expected_cooldown)) in expected.into_iter().enumerate() {
            let identity = worker(100 + index as u32, 100 + index as u64);
            make_ready(&mut supervisor, &identity);
            let effect = supervisor
                .record_worker_failure(&identity, WorkerFailureKind::Exited, now)
                .unwrap();
            assert_eq!(effect.crash_streak, expected_streak);
            assert_eq!(effect.cooldown_not_before_unix_ms - now, expected_cooldown);
            now = effect.cooldown_not_before_unix_ms;
            supervisor.release_cooldown(now).unwrap();
        }
    }

    #[test]
    fn clean_success_resets_persisted_crash_streak() {
        let mut supervisor = cold_supervisor();
        let first = worker(201, 1);
        make_ready(&mut supervisor, &first);
        let first_failure = supervisor
            .record_worker_failure(&first, WorkerFailureKind::Exited, 1_000)
            .unwrap();
        supervisor
            .release_cooldown(first_failure.cooldown_not_before_unix_ms)
            .unwrap();

        let clean = worker(202, 2);
        make_ready(&mut supervisor, &clean);
        supervisor.begin_attempt(&clean, attempt(1)).unwrap();
        supervisor
            .complete_active_success(&clean, attempt(1))
            .unwrap();
        assert_eq!(supervisor.health().crash_streak(), 0);
        assert_eq!(supervisor.health().last_failure_at_unix_ms(), None);
        assert_eq!(supervisor.health().cooldown_not_before_unix_ms(), None);

        let next = supervisor
            .record_worker_failure(&clean, WorkerFailureKind::Exited, 2_000)
            .unwrap();
        assert_eq!(next.crash_streak, 1);
        assert_eq!(next.cooldown_not_before_unix_ms, 2_100);
    }

    #[test]
    fn durable_health_round_trip_preserves_cooldown_across_supervisors() {
        let mut supervisor = cold_supervisor();
        let identity = worker(301, 1);
        make_ready(&mut supervisor, &identity);
        let effect = supervisor
            .record_worker_failure(&identity, WorkerFailureKind::ProtocolViolation, 8_000)
            .unwrap();
        let json = serde_json::to_string(supervisor.health()).unwrap();
        let restored_health: WorkerHealthState = serde_json::from_str(&json).unwrap();
        let restored = WorkerSupervisorState::restore(restored_health, policy(), 8_050).unwrap();

        assert_eq!(restored.phase(), WorkerLifecyclePhase::CoolingDown);
        assert_eq!(
            restored.health().cooldown_not_before_unix_ms(),
            Some(effect.cooldown_not_before_unix_ms)
        );
        assert!(!json.contains("prompt"));
    }

    #[test]
    fn changed_driver_or_worker_build_starts_a_distinct_health_generation() {
        let first = WorkerHealthKey::new("device", "driver-a", "worker-a").unwrap();
        let changed_driver = WorkerHealthKey::new("device", "driver-b", "worker-a").unwrap();
        let changed_worker = WorkerHealthKey::new("device", "driver-a", "worker-b").unwrap();
        assert_ne!(first, changed_driver);
        assert_ne!(first, changed_worker);

        let mut supervisor =
            WorkerSupervisorState::restore(WorkerHealthState::new(changed_worker), policy(), 1_000)
                .unwrap();
        let old_build = WorkerGenerationIdentity::new(501, 1, "worker-a").unwrap();
        assert_eq!(
            supervisor.begin_start(old_build),
            Err(WorkerSupervisorError::WorkerBuildMismatch)
        );
        assert_eq!(supervisor.phase(), WorkerLifecyclePhase::Cold);
        assert_eq!(supervisor.health().crash_streak(), 0);
    }

    #[test]
    fn worker_failure_cannot_consume_or_fail_pending_queue_entries() {
        let mut pending = VecDeque::from([attempt(2), attempt(3), attempt(4)]);
        let before = pending.clone();
        let mut supervisor = cold_supervisor();
        let worker = worker(401, 1);
        make_ready(&mut supervisor, &worker);
        supervisor.begin_attempt(&worker, attempt(1)).unwrap();

        let effect = supervisor
            .record_worker_failure(&worker, WorkerFailureKind::DeviceLost, 9_000)
            .unwrap();

        assert_eq!(effect.pending_queue, PendingQueueDisposition::Preserve);
        assert_eq!(pending, before);
        assert_eq!(pending.pop_front(), Some(attempt(2)));
        assert_eq!(effect.active_terminal.unwrap().attempt, attempt(1));
    }

    #[test]
    fn invalid_identity_errors_are_typed_and_do_not_echo_input() {
        let sensitive = "secret\nrequest body";
        let error = WorkerHealthKey::new(sensitive, "driver", BUILD).unwrap_err();
        assert_eq!(error.code(), "inference_worker_identity_invalid");
        assert!(!error.to_string().contains(sensitive));

        let invalid_json = serde_json::json!({
            "key": {
                "physical_device_id": "device",
                "driver_build_id": "driver",
                "worker_build_id": BUILD,
            },
            "crash_streak": 1,
            "last_failure_at_unix_ms": 1_000,
            "cooldown_not_before_unix_ms": 9_000,
        });
        let health: WorkerHealthState = serde_json::from_value(invalid_json).unwrap();
        assert_eq!(
            WorkerSupervisorState::restore(health, policy(), 1_001),
            Err(WorkerSupervisorError::InvalidPersistedHealth(
                WorkerHealthStateError::CooldownOutsidePolicy,
            ))
        );
    }
}
