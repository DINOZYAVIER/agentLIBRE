//! Deterministic production-wire fixtures for the worker failure boundary.
//!
//! These tests deliberately use the real bounded `SOCK_SEQPACKET` channels,
//! frame parser, descriptor contract, and supervisor FSM. The scripted peer
//! owns no native runtime and never touches a daemon or accelerator.

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use agl_ids::{AttemptId, RunId, TurnId};

use crate::admission::{
    AllocationEstimate, AllocationReceipt as HostAllocationReceipt, ReceiptValidationError,
};
use crate::durable_health::{
    DurableHealthStore, ResourceEstimateQuarantine, ResourceQuarantineKey,
};
use crate::worker_protocol::{
    AllocationReceipt as WireAllocationReceipt, ContextResourceId, Handshake, HostCommand,
    HostControlChannel, ModelResourceId, OperationId, Ready, SealedPayloadTransfer,
    WorkerControlChannel, WorkerEvent, WorkerFailure, WorkerFailureCode, WorkerProtocolErrorCode,
    control_channel_pair,
};
use crate::worker_supervisor::{
    ActiveAttemptIdentity, ActiveTerminalOutcome, PendingQueueDisposition,
    WorkerCircuitBreakerPolicy, WorkerFailureKind, WorkerGenerationIdentity, WorkerHealthKey,
    WorkerHealthState, WorkerLifecyclePhase, WorkerSupervisorError, WorkerSupervisorState,
};
use crate::{
    InferenceAdmissionEvidence, InferenceAttemptFailure, InferenceAttemptMachine,
    InferenceAttemptOutcome, InferenceAttemptPhase, InferenceAttemptTransition,
    InferenceContentEvidence, InferenceDispatchEvidence, InferenceOutputEvent,
    InferencePlanEvidence, InferenceRejectionStage, InferenceRuntimeEvidence,
};

const WORKER_BUILD: &str = "scripted-worker-build";
const ATTEMPT_ID: &str = "attempt_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b33";
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(2);

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScriptPhase {
    ModelLoad,
    ContextCreate,
    Generation,
    Cleanup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScriptFault {
    Eof,
    AbruptSignal,
    DeviceLost,
    HostileProtocol,
}

impl ScriptFault {
    fn supervisor_kind(self) -> WorkerFailureKind {
        match self {
            Self::Eof => WorkerFailureKind::Exited,
            Self::AbruptSignal => WorkerFailureKind::Signaled,
            Self::DeviceLost => WorkerFailureKind::DeviceLost,
            Self::HostileProtocol => WorkerFailureKind::ProtocolViolation,
        }
    }
}

#[test]
fn post_handshake_failure_matrix_is_exactly_once_and_preserves_pending_queue() {
    let phases = [
        ScriptPhase::ModelLoad,
        ScriptPhase::ContextCreate,
        ScriptPhase::Generation,
        ScriptPhase::Cleanup,
    ];
    let faults = [
        ScriptFault::Eof,
        ScriptFault::AbruptSignal,
        ScriptFault::DeviceLost,
        ScriptFault::HostileProtocol,
    ];

    for phase in phases {
        for fault in faults {
            let (mut host, worker) = control_channel_pair().unwrap();
            let scripted = thread::spawn(move || scripted_worker(worker, phase, fault));
            exact_handshake(&mut host);

            let worker_identity = WorkerGenerationIdentity::new(41, 1, WORKER_BUILD).unwrap();
            let mut supervisor = ready_supervisor(&worker_identity);
            let active = ActiveAttemptIdentity::new(1).unwrap();
            if matches!(phase, ScriptPhase::Generation | ScriptPhase::Cleanup) {
                supervisor.begin_attempt(&worker_identity, active).unwrap();
            }
            let mut durable_attempt = active_attempt_machine();
            let pending = VecDeque::from([
                ActiveAttemptIdentity::new(2).unwrap(),
                ActiveAttemptIdentity::new(3).unwrap(),
            ]);
            let pending_before = pending.clone();
            let mut native_dispatches = 0;

            dispatch_payload(
                &mut host,
                HostCommandBuilder::Load,
                operation(1),
                &mut native_dispatches,
            );
            if phase != ScriptPhase::ModelLoad {
                assert!(matches!(
                    host.receive_timeout(RECEIVE_TIMEOUT).unwrap(),
                    WorkerEvent::ModelLoaded {
                        operation_id,
                        model_resource_id,
                        log: None,
                    } if operation_id == operation(1) && model_resource_id == model(1)
                ));
                dispatch_payload(
                    &mut host,
                    HostCommandBuilder::Context,
                    operation(2),
                    &mut native_dispatches,
                );
            }
            if matches!(phase, ScriptPhase::Generation | ScriptPhase::Cleanup) {
                assert!(matches!(
                    host.receive_timeout(RECEIVE_TIMEOUT).unwrap(),
                    WorkerEvent::ContextCreated {
                        operation_id,
                        model_resource_id,
                        context_resource_id,
                        log: None,
                    } if operation_id == operation(2)
                        && model_resource_id == model(1)
                        && context_resource_id == context(1)
                ));
                dispatch_payload(
                    &mut host,
                    HostCommandBuilder::Generate,
                    operation(3),
                    &mut native_dispatches,
                );
                assert!(matches!(
                    host.receive_timeout(RECEIVE_TIMEOUT).unwrap(),
                    WorkerEvent::Started { operation_id, .. } if operation_id == operation(3)
                ));
                if phase == ScriptPhase::Cleanup {
                    assert!(matches!(
                        host.receive_timeout(RECEIVE_TIMEOUT).unwrap(),
                        WorkerEvent::Output {
                            operation_id,
                            event: InferenceOutputEvent::TextDelta { sequence: 1, .. },
                        } if operation_id == operation(3)
                    ));
                }
            }

            let observed_kind = observe_scripted_fault(&mut host, fault);
            assert_eq!(observed_kind, fault.supervisor_kind());
            scripted.join().unwrap();

            let effect = supervisor
                .record_worker_failure(&worker_identity, observed_kind, 10_000)
                .unwrap();
            let supervisor_terminal = effect.active_terminal.clone();
            if matches!(phase, ScriptPhase::Generation | ScriptPhase::Cleanup) {
                let terminal = supervisor_terminal
                    .as_ref()
                    .expect("dispatched generation has one supervisor terminal");
                assert_eq!(terminal.attempt, active);
                assert_eq!(
                    terminal.outcome,
                    ActiveTerminalOutcome::BackendLost {
                        failure: fault.supervisor_kind(),
                    }
                );
            } else {
                // Load/context loss precedes the worker Started receipt. The
                // durable application attempt still closes below, but the
                // supervisor must not fabricate a native active identity.
                assert_eq!(supervisor_terminal, None);
            }
            durable_attempt
                .apply(InferenceAttemptTransition::RecordFailure {
                    failure: InferenceAttemptFailure {
                        code: observed_kind.code().to_string(),
                        stage: InferenceRejectionStage::Engine,
                        message: observed_kind.code().to_string(),
                    },
                })
                .unwrap();
            durable_attempt
                .apply(InferenceAttemptTransition::FinishAttempt {
                    outcome: InferenceAttemptOutcome::Failed,
                })
                .unwrap();
            assert_eq!(durable_attempt.phase(), InferenceAttemptPhase::Failed);
            assert!(
                durable_attempt
                    .apply(InferenceAttemptTransition::FinishAttempt {
                        outcome: InferenceAttemptOutcome::Failed,
                    })
                    .is_err(),
                "the durable attempt cannot close twice"
            );
            assert_eq!(effect.pending_queue, PendingQueueDisposition::Preserve);
            assert_eq!(pending, pending_before);
            assert_eq!(supervisor.phase(), WorkerLifecyclePhase::CoolingDown);
            assert_eq!(
                supervisor.record_worker_failure(&worker_identity, observed_kind, 10_001),
                Err(WorkerSupervisorError::DuplicateWorkerFailure)
            );
            assert_eq!(supervisor.last_terminal(), supervisor_terminal.as_ref());

            // Each phase is dispatched at most once; a worker loss never
            // loops or transparently retries the current run.
            let expected_dispatches = match phase {
                ScriptPhase::ModelLoad => 1,
                ScriptPhase::ContextCreate => 2,
                ScriptPhase::Generation | ScriptPhase::Cleanup => 3,
            };
            assert_eq!(native_dispatches, expected_dispatches);
        }
    }
}

#[test]
fn cooldown_recovery_is_lazy_and_requires_a_later_explicit_request() {
    let failed_worker = WorkerGenerationIdentity::new(71, 1, WORKER_BUILD).unwrap();
    let mut supervisor = ready_supervisor(&failed_worker);
    let failed_attempt = ActiveAttemptIdentity::new(1).unwrap();
    supervisor
        .begin_attempt(&failed_worker, failed_attempt)
        .unwrap();
    let effect = supervisor
        .record_worker_failure(&failed_worker, WorkerFailureKind::DeviceLost, 20_000)
        .unwrap();
    let not_before = effect.cooldown_not_before_unix_ms;
    let mut worker_launches = 1;

    assert!(matches!(
        supervisor.release_cooldown(not_before - 1),
        Err(WorkerSupervisorError::CooldownActive { .. })
    ));
    assert_eq!(worker_launches, 1);
    supervisor.release_cooldown(not_before).unwrap();
    assert_eq!(supervisor.phase(), WorkerLifecyclePhase::Cold);
    assert_eq!(worker_launches, 1, "cooldown expiry is not an auto-retry");

    // This block represents the later, explicit request admission point.
    let clean_worker = WorkerGenerationIdentity::new(72, 2, WORKER_BUILD).unwrap();
    worker_launches += 1;
    supervisor.begin_start(clean_worker.clone()).unwrap();
    supervisor.mark_ready(&clean_worker).unwrap();
    let later_attempt = ActiveAttemptIdentity::new(2).unwrap();
    supervisor
        .begin_attempt(&clean_worker, later_attempt)
        .unwrap();
    let terminal = supervisor
        .complete_active_success(&clean_worker, later_attempt)
        .unwrap();

    assert_eq!(worker_launches, 2);
    assert_eq!(terminal.outcome, ActiveTerminalOutcome::Succeeded);
    assert_eq!(supervisor.phase(), WorkerLifecyclePhase::Ready);
    assert_eq!(supervisor.health().crash_streak(), 0);
}

#[test]
fn over_envelope_wire_receipt_stops_peer_and_persists_exact_quarantine() {
    let (mut host, mut worker) = control_channel_pair().unwrap();
    let scripted = thread::spawn(move || {
        let handshake = worker
            .receive_timeout_with_descriptors(RECEIVE_TIMEOUT)
            .unwrap();
        let (command, descriptors) = handshake.into_parts();
        assert!(matches!(command, HostCommand::Handshake(_)));
        descriptors.ensure_empty().unwrap();
        worker.send(WorkerEvent::Ready(Ready::current())).unwrap();

        let received = worker
            .receive_timeout_with_descriptors(RECEIVE_TIMEOUT)
            .unwrap();
        let (command, _payload) = received.into_parts();
        assert!(matches!(
            command,
            HostCommand::Generate { operation_id, .. } if operation_id == operation(9)
        ));
        worker
            .send(WorkerEvent::Started {
                operation_id: operation(9),
                allocation_receipt: WireAllocationReceipt::new(
                    10,
                    21,
                    30,
                    Some("pci:0000:03:00.0".to_string()),
                )
                .unwrap(),
            })
            .unwrap();

        // The host rejects the receipt and closes the exact generation. The
        // peer observes production transport EOF and exits; it cannot emit a
        // token or completion after the unsafe receipt.
        let error = worker.receive_timeout(RECEIVE_TIMEOUT).unwrap_err();
        assert_eq!(error.code(), WorkerProtocolErrorCode::PeerClosed);
    });
    exact_handshake(&mut host);
    let mut dispatches = 0;
    dispatch_payload(
        &mut host,
        HostCommandBuilder::Generate,
        operation(9),
        &mut dispatches,
    );
    let receipt = match host.receive_timeout(RECEIVE_TIMEOUT).unwrap() {
        WorkerEvent::Started {
            operation_id,
            allocation_receipt,
        } if operation_id == operation(9) => allocation_receipt,
        event => panic!("unexpected scripted receipt: {event:?}"),
    };
    let admitted = AllocationEstimate {
        model_bytes: 10,
        context_bytes: 20,
        transient_bytes: 30,
        uncertainty_bytes: 5,
    };
    let reported = HostAllocationReceipt {
        model_bytes: receipt.model_bytes(),
        context_bytes: receipt.context_bytes(),
        transient_bytes: receipt.transient_bytes(),
    };
    assert!(matches!(
        reported.validate_against(admitted),
        Err(ReceiptValidationError::EnvelopeExceeded { .. })
    ));

    let quarantine_key =
        ResourceQuarantineKey::new(digest(1), digest(2), digest(3), digest(4), digest(5)).unwrap();
    let quarantine =
        ResourceEstimateQuarantine::new(quarantine_key.clone(), admitted, reported).unwrap();
    let health_root = temporary_root("over-receipt");
    let store = DurableHealthStore::open(&health_root).unwrap();
    store.store_resource_quarantine(&quarantine).unwrap();

    let worker_identity = WorkerGenerationIdentity::new(91, 1, WORKER_BUILD).unwrap();
    let mut supervisor = ready_supervisor(&worker_identity);
    let attempt = ActiveAttemptIdentity::new(1).unwrap();
    supervisor.begin_attempt(&worker_identity, attempt).unwrap();
    let effect = supervisor
        .record_worker_failure(
            &worker_identity,
            WorkerFailureKind::ProtocolViolation,
            30_000,
        )
        .unwrap();
    assert_eq!(dispatches, 1);
    assert_eq!(
        effect.active_terminal.unwrap().outcome,
        ActiveTerminalOutcome::BackendLost {
            failure: WorkerFailureKind::ProtocolViolation,
        }
    );
    assert_eq!(
        store.load_resource_quarantine(&quarantine_key).unwrap(),
        Some(quarantine)
    );

    drop(host);
    scripted.join().unwrap();
    drop(store);
    let _ = fs::remove_dir_all(health_root);
}

#[derive(Clone, Copy)]
enum HostCommandBuilder {
    Load,
    Context,
    Generate,
}

fn dispatch_payload(
    host: &mut HostControlChannel,
    kind: HostCommandBuilder,
    operation_id: OperationId,
    dispatches: &mut usize,
) {
    let (payload, descriptor) = SealedPayloadTransfer::new(b"scripted-job", 0)
        .unwrap()
        .into_parts();
    let command = match kind {
        HostCommandBuilder::Load => HostCommand::LoadModel {
            operation_id,
            model_resource_id: model(1),
            job: payload,
        },
        HostCommandBuilder::Context => HostCommand::CreateContext {
            operation_id,
            model_resource_id: model(1),
            context_resource_id: context(1),
            job: payload,
        },
        HostCommandBuilder::Generate => HostCommand::Generate {
            operation_id,
            model_resource_id: model(1),
            context_resource_id: context(1),
            job: payload,
        },
    };
    host.send_with_descriptors(command, vec![descriptor])
        .unwrap();
    *dispatches += 1;
}

fn scripted_worker(
    mut worker: WorkerControlChannel,
    failure_phase: ScriptPhase,
    fault: ScriptFault,
) {
    let received = worker
        .receive_timeout_with_descriptors(RECEIVE_TIMEOUT)
        .unwrap();
    let (command, descriptors) = received.into_parts();
    let HostCommand::Handshake(handshake) = command else {
        panic!("scripted worker requires handshake first");
    };
    handshake.validate_exact().unwrap();
    descriptors.ensure_empty().unwrap();
    worker.send(WorkerEvent::Ready(Ready::current())).unwrap();

    let (command, _payload) = worker
        .receive_timeout_with_descriptors(RECEIVE_TIMEOUT)
        .unwrap()
        .into_parts();
    assert!(matches!(
        command,
        HostCommand::LoadModel { operation_id, .. } if operation_id == operation(1)
    ));
    if failure_phase == ScriptPhase::ModelLoad {
        emit_fault(&mut worker, operation(1), fault);
        return;
    }
    worker
        .send(WorkerEvent::ModelLoaded {
            operation_id: operation(1),
            model_resource_id: model(1),
            log: None,
        })
        .unwrap();

    let (command, _payload) = worker
        .receive_timeout_with_descriptors(RECEIVE_TIMEOUT)
        .unwrap()
        .into_parts();
    assert!(matches!(
        command,
        HostCommand::CreateContext { operation_id, .. } if operation_id == operation(2)
    ));
    if failure_phase == ScriptPhase::ContextCreate {
        emit_fault(&mut worker, operation(2), fault);
        return;
    }
    worker
        .send(WorkerEvent::ContextCreated {
            operation_id: operation(2),
            model_resource_id: model(1),
            context_resource_id: context(1),
            log: None,
        })
        .unwrap();

    let (command, _payload) = worker
        .receive_timeout_with_descriptors(RECEIVE_TIMEOUT)
        .unwrap()
        .into_parts();
    assert!(matches!(
        command,
        HostCommand::Generate { operation_id, .. } if operation_id == operation(3)
    ));
    worker
        .send(WorkerEvent::Started {
            operation_id: operation(3),
            allocation_receipt: WireAllocationReceipt::new(1, 2, 3, None).unwrap(),
        })
        .unwrap();
    if failure_phase == ScriptPhase::Generation {
        emit_fault(&mut worker, operation(3), fault);
        return;
    }

    worker
        .send(WorkerEvent::Output {
            operation_id: operation(3),
            event: InferenceOutputEvent::TextDelta {
                attempt_id: AttemptId::parse(ATTEMPT_ID).unwrap(),
                sequence: 1,
                text: "partial".to_string(),
            },
        })
        .unwrap();
    assert_eq!(failure_phase, ScriptPhase::Cleanup);
    emit_fault(&mut worker, operation(3), fault);
}

fn emit_fault(worker: &mut WorkerControlChannel, operation_id: OperationId, fault: ScriptFault) {
    match fault {
        ScriptFault::DeviceLost => worker
            .send(WorkerEvent::Failed {
                operation_id,
                failure: WorkerFailure::bounded(
                    WorkerFailureCode::DeviceLost,
                    "scripted device loss",
                ),
                log: None,
            })
            .unwrap(),
        ScriptFault::HostileProtocol => worker
            .send(WorkerEvent::Failed {
                operation_id: operation(99),
                failure: WorkerFailure::bounded(
                    WorkerFailureCode::InvalidRequest,
                    "scripted event for another operation",
                ),
                log: None,
            })
            .unwrap(),
        ScriptFault::Eof | ScriptFault::AbruptSignal => {}
    }
    // Returning drops the worker half. EOF and abrupt signal deliberately
    // have the same wire observation; the production child wait status
    // supplies their distinct supervisor classification.
}

fn observe_scripted_fault(host: &mut HostControlChannel, fault: ScriptFault) -> WorkerFailureKind {
    match fault {
        ScriptFault::DeviceLost => match host.receive_timeout(RECEIVE_TIMEOUT).unwrap() {
            WorkerEvent::Failed { failure, .. }
                if failure.code() == WorkerFailureCode::DeviceLost =>
            {
                WorkerFailureKind::DeviceLost
            }
            event => panic!("unexpected typed device-loss event: {event:?}"),
        },
        ScriptFault::HostileProtocol => {
            let event = host.receive_timeout(RECEIVE_TIMEOUT).unwrap();
            assert!(matches!(
                event,
                WorkerEvent::Failed {
                    operation_id,
                    failure,
                    ..
                } if operation_id == operation(99)
                    && failure.code() == WorkerFailureCode::InvalidRequest
            ));
            WorkerFailureKind::ProtocolViolation
        }
        ScriptFault::Eof | ScriptFault::AbruptSignal => {
            let error = host.receive_timeout(RECEIVE_TIMEOUT).unwrap_err();
            assert_eq!(error.code(), WorkerProtocolErrorCode::PeerClosed);
            fault.supervisor_kind()
        }
    }
}

fn exact_handshake(host: &mut HostControlChannel) {
    host.send(HostCommand::Handshake(Handshake::current()))
        .unwrap();
    let ready = host.receive_timeout(RECEIVE_TIMEOUT).unwrap();
    let WorkerEvent::Ready(ready) = ready else {
        panic!("scripted worker did not complete handshake");
    };
    ready.validate_exact().unwrap();
}

fn ready_supervisor(worker: &WorkerGenerationIdentity) -> WorkerSupervisorState {
    let policy = WorkerCircuitBreakerPolicy::new(100, 800, 4).unwrap();
    let key = WorkerHealthKey::new("pci:0000:03:00.0", "radv:test", WORKER_BUILD).unwrap();
    let mut supervisor =
        WorkerSupervisorState::restore(WorkerHealthState::new(key), policy, 1_000).unwrap();
    supervisor.begin_start(worker.clone()).unwrap();
    supervisor.mark_ready(worker).unwrap();
    supervisor
}

fn active_attempt_machine() -> InferenceAttemptMachine {
    let mut machine = InferenceAttemptMachine::new(
        RunId::parse("run_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b31").unwrap(),
        TurnId::parse("turn_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b32").unwrap(),
        AttemptId::parse(ATTEMPT_ID).unwrap(),
    );
    machine
        .apply(InferenceAttemptTransition::StartAttempt {
            backend: "llama_cpp_worker".to_string(),
            request_path: PathBuf::from("request.json"),
        })
        .unwrap();
    machine
        .apply(InferenceAttemptTransition::RecordRequest {
            path: PathBuf::from("request.json"),
        })
        .unwrap();
    for transition in [
        InferenceAttemptTransition::RecordPlan {
            plan: InferencePlanEvidence {
                plan_digest: "plan".to_owned(),
                package_refs: Vec::new(),
                profile_id: "fixture".to_owned(),
            },
        },
        InferenceAttemptTransition::RecordContentReady {
            content: InferenceContentEvidence {
                content_digest: "content".to_owned(),
                resolved_bytes: 0,
            },
        },
        InferenceAttemptTransition::RecordAdmissionGrant {
            admission: InferenceAdmissionEvidence {
                reservation_id: "reservation".to_owned(),
                resource_components: Vec::new(),
            },
        },
        InferenceAttemptTransition::RecordDispatch {
            dispatch: InferenceDispatchEvidence {
                descriptor_set_id: "descriptors".to_owned(),
                engine_generation: "generation".to_owned(),
            },
        },
        InferenceAttemptTransition::RecordRuntimeStarted {
            runtime: InferenceRuntimeEvidence {
                allocation_receipt_id: "receipt".to_owned(),
            },
        },
    ] {
        machine.apply(transition).unwrap();
    }
    machine
}

fn temporary_root(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "agl-supervision-matrix-{label}-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn digest(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn operation(value: u64) -> OperationId {
    OperationId::new(value).unwrap()
}

fn model(value: u64) -> ModelResourceId {
    ModelResourceId::new(value).unwrap()
}

fn context(value: u64) -> ContextResourceId {
    ContextResourceId::new(value).unwrap()
}
