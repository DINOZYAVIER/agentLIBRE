use std::path::PathBuf;

use agl_ids::{AttemptId, RunId, TurnId};
use agl_inference::{
    AttemptJournal, InferenceAttemptFailure, InferenceAttemptMachine, InferenceAttemptOutcome,
    InferenceAttemptPhase, InferenceAttemptTransition, InferenceRejectionStage,
};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

// MIW-FSM-002, MIW-JRN-001 and MIW-JRN-002.
#[test]
fn durable_failure_path_replays_to_the_identical_terminal_machine() {
    let mut machine =
        InferenceAttemptMachine::new(RunId::generate(), TurnId::generate(), AttemptId::generate());
    let mut journal = AttemptJournal::in_memory();
    journal
        .append(
            &mut machine,
            InferenceAttemptTransition::StartAttempt {
                backend: "llama_cpp".to_owned(),
                request_path: PathBuf::from("request.json"),
                projection_root: None,
            },
        )
        .unwrap();
    journal
        .append(
            &mut machine,
            InferenceAttemptTransition::RecordFailure {
                failure: InferenceAttemptFailure {
                    code: "queue_full".to_owned(),
                    stage: InferenceRejectionStage::Queue,
                    message: "capacity exhausted".to_owned(),
                    plan_rejection: None,
                },
            },
        )
        .unwrap();
    journal
        .append(
            &mut machine,
            InferenceAttemptTransition::FinishAttempt {
                outcome: InferenceAttemptOutcome::Failed,
            },
        )
        .unwrap();

    let replay = AttemptJournal::replay(journal.bytes()).unwrap();
    assert_eq!(replay.machine(), &machine);
    assert_eq!(replay.machine().phase(), InferenceAttemptPhase::Failed);
    assert_eq!(replay.journal_bytes(), journal.bytes());
}

// MIW-FSM-003 and MIW-JRN-002.
#[test]
fn illegal_and_post_terminal_inputs_do_not_mutate_authority() {
    let mut machine =
        InferenceAttemptMachine::new(RunId::generate(), TurnId::generate(), AttemptId::generate());
    let mut journal = AttemptJournal::in_memory();
    let before = machine.clone();
    assert!(
        journal
            .append(
                &mut machine,
                InferenceAttemptTransition::FinishAttempt {
                    outcome: InferenceAttemptOutcome::Succeeded,
                },
            )
            .is_err()
    );
    assert_eq!(machine, before);
    assert!(journal.records().is_empty());
}

// MIW-JRN-002 and MIW-JRN-003.
#[test]
fn corrupt_or_truncated_journal_fails_closed() {
    assert!(AttemptJournal::replay(b"not-json\n").is_err());
    assert!(AttemptJournal::replay(b"").is_err());
}

// MIW-FSM-001 and MIW-OUT-001. Success and bounded incomplete output remain
// different immutable terminal phases.
#[test]
fn successful_terminals_are_exact_and_immutable() {
    for (outcome, expected) in [
        (
            InferenceAttemptOutcome::Succeeded,
            InferenceAttemptPhase::Succeeded,
        ),
        (
            InferenceAttemptOutcome::IncompleteOutput,
            InferenceAttemptPhase::IncompleteOutput,
        ),
    ] {
        let mut machine = InferenceAttemptMachine::new(
            RunId::generate(),
            TurnId::generate(),
            AttemptId::generate(),
        );
        let mut journal = AttemptJournal::in_memory();
        append_success_prefix(&mut journal, &mut machine);
        journal
            .append(
                &mut machine,
                InferenceAttemptTransition::FinishAttempt { outcome },
            )
            .unwrap();
        assert_eq!(machine.phase(), expected);
        let before = machine.clone();
        assert!(
            journal
                .append(
                    &mut machine,
                    InferenceAttemptTransition::RecordFailure {
                        failure: InferenceAttemptFailure {
                            code: "late".to_owned(),
                            stage: InferenceRejectionStage::Evidence,
                            message: "late".to_owned(),
                            plan_rejection: None,
                        },
                    },
                )
                .is_err()
        );
        assert_eq!(machine, before);
    }
}

// MIW-JRN-003 and MIW-JRN-005. Both observable files are disposable,
// versioned projections of the complete journal evidence.
#[test]
fn missing_projections_rebuild_from_the_journal() {
    let root = temp_root("projections");
    let journal_path = root.join("attempt").join("transitions.jsonl");
    let mut journal = AttemptJournal::create(&journal_path).unwrap();
    let mut machine =
        InferenceAttemptMachine::new(RunId::generate(), TurnId::generate(), AttemptId::generate());
    journal
        .append(
            &mut machine,
            InferenceAttemptTransition::StartAttempt {
                backend: "llama_cpp".to_owned(),
                request_path: PathBuf::from("request.json"),
                projection_root: None,
            },
        )
        .unwrap();
    let resolution = root.join("attempt/runtime-resolution.json");
    let events = root.join("attempt/inference-events.jsonl");
    assert!(resolution.is_file());
    assert!(events.is_file());
    std::fs::remove_file(&resolution).unwrap();
    std::fs::remove_file(&events).unwrap();
    drop(journal);

    let (_journal, restored) = AttemptJournal::open(&journal_path).unwrap();
    assert_eq!(restored.phase(), InferenceAttemptPhase::Started);
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&resolution).unwrap()).unwrap();
    assert_eq!(
        value["schema"],
        "agentlibre.inference-runtime-resolution/v1"
    );
    assert_eq!(value["phase"], "started");
    assert!(
        std::fs::read_to_string(events)
            .unwrap()
            .contains("agentlibre.inference-transition-event/v1")
    );
}

fn append_success_prefix(journal: &mut AttemptJournal, machine: &mut InferenceAttemptMachine) {
    let transitions = [
        InferenceAttemptTransition::StartAttempt {
            backend: "llama_cpp".to_owned(),
            request_path: PathBuf::from("request.json"),
            projection_root: None,
        },
        InferenceAttemptTransition::RecordRequest {
            path: PathBuf::from("request.json"),
        },
        InferenceAttemptTransition::RecordPlan {
            plan: agl_inference::InferencePlanEvidence {
                plan_digest: format!("sha256:{}", "a".repeat(64)),
                package_refs: vec!["function:test@=1.0.0".to_owned()],
                profile_id: "test".to_owned(),
                product_resolution: None,
            },
        },
        InferenceAttemptTransition::RecordContentReady {
            content: agl_inference::InferenceContentEvidence {
                content_digest: format!("sha256:{}", "b".repeat(64)),
                resolved_bytes: 0,
            },
        },
        InferenceAttemptTransition::RecordAdmissionGrant {
            admission: agl_inference::InferenceAdmissionEvidence {
                reservation_id: "reservation:1".to_owned(),
                engine_reservation_id: "reservation:1".to_owned(),
                reused_resident_allocation: false,
                resource_components: vec![("host_bytes".to_owned(), 1)],
            },
        },
        InferenceAttemptTransition::RecordDispatch {
            dispatch: agl_inference::InferenceDispatchEvidence {
                descriptor_set_id: "descriptors".to_owned(),
                engine_generation: "engine:1".to_owned(),
            },
        },
        InferenceAttemptTransition::RecordRuntimeStarted {
            runtime: agl_inference::InferenceRuntimeEvidence {
                allocation_receipt_id: "receipt:1".to_owned(),
                plan_digest: format!("sha256:{}", "a".repeat(64)),
                reservation_id: "reservation:1".to_owned(),
                engine_generation: "engine:1".to_owned(),
                selected_device: None,
                host_bytes: 1,
                device_bytes: 0,
                shared_bytes: 0,
            },
        },
        InferenceAttemptTransition::RecordGenerationMetrics {
            generation: agl_inference::InferenceGenerationEvidence {
                input_tokens: 10,
                output_tokens: 2,
                configured_batch_size: 8,
                prefill_chunks: 2,
            },
        },
        InferenceAttemptTransition::RecordRuntimeLog {
            path: PathBuf::from("engine.log"),
        },
        InferenceAttemptTransition::RecordResponse {
            path: PathBuf::from("response.json"),
        },
    ];
    for transition in transitions {
        journal.append(machine, transition).unwrap();
    }
}

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agl173-journal-{label}-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}
