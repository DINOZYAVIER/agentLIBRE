use std::collections::BTreeSet;

use agl_artifact::{
    ArtifactChange, ArtifactChangeKind, ArtifactCommitInput, ArtifactCommitMachine,
    ArtifactCommitPrepare, ArtifactCommitState, GitCommitMaterial,
};
use agl_kernel::{
    ArtifactId, DeclarationDigest, EffectId, ExtensionId, MemoryToolEffectJournal, ToolDelivery,
    ToolEffectCorrelation, ToolEffectJournal, ToolEffectLifecycleState, ToolEffectMachine, ToolId,
};
use serde_json::json;

fn oid(byte: char) -> String {
    std::iter::repeat_n(byte, 40).collect()
}

fn started_journal(
    terminal: Option<ToolEffectLifecycleState>,
) -> (MemoryToolEffectJournal, ToolEffectCorrelation) {
    let mut machine = ToolEffectMachine::new(
        "call-1",
        "core.repo:artifact.commit".parse::<ToolId>().unwrap(),
        "core.repo".parse::<ExtensionId>().unwrap(),
        DeclarationDigest::from_json(&json!({"fixture": "artifact-commit"})),
        ToolDelivery::AtMostOnce,
        BTreeSet::from([
            "agl:artifact.repository".parse::<EffectId>().unwrap(),
            "agl:repo.gitlink".parse::<EffectId>().unwrap(),
        ]),
    );
    let admitted = machine
        .apply(ToolEffectLifecycleState::Admitted, Vec::new(), None)
        .unwrap();
    let started = machine
        .apply(ToolEffectLifecycleState::Started, Vec::new(), None)
        .unwrap();
    let correlation = ToolEffectCorrelation::from_record(&started);
    let mut journal = MemoryToolEffectJournal::default();
    journal.append(&admitted).unwrap();
    journal.append(&started).unwrap();
    if let Some(terminal) = terminal {
        let terminal = machine.apply(terminal, Vec::new(), None).unwrap();
        journal.append(&terminal).unwrap();
    }
    (journal, correlation)
}

fn correlation() -> ToolEffectCorrelation {
    started_journal(None).1
}

fn preparation() -> ArtifactCommitPrepare {
    ArtifactCommitPrepare::builder(
        "operation-1",
        ArtifactId::new("core.repo:tasks").unwrap(),
        correlation(),
    )
    .parent_head(oid('1'))
    .parent_gitlink(oid('2'))
    .child_head(oid('3'))
    .changes([
        ArtifactChange::new("README.md", ArtifactChangeKind::Update).unwrap(),
        ArtifactChange::new("deleted.md", ArtifactChangeKind::Delete).unwrap(),
    ])
    .child_commit(GitCommitMaterial::fixture(
        oid('3'),
        oid('4'),
        oid('5'),
        "Artifact update",
    ))
    .parent_identity(
        "AGL Fixture <agl-fixture@example.invalid> 0 +0000",
        "AGL Fixture <agl-fixture@example.invalid> 0 +0000",
    )
    .parent_message("Artifact update")
    .build()
    .unwrap()
}

// AGL172-025, AGL172-037, AGL172-042 and AGL172-043.
#[test]
fn commit_machine_accepts_only_the_selected_durable_transition_table() {
    let mut machine = ArtifactCommitMachine::default();
    let prepared = machine
        .apply(ArtifactCommitInput::Prepare(Box::new(preparation())))
        .unwrap();
    assert!(matches!(prepared, ArtifactCommitState::Prepared { .. }));

    let child = machine
        .apply(ArtifactCommitInput::RecordChildCommit {
            observed_commit: oid('5'),
            parent_commit: GitCommitMaterial::fixture(
                oid('1'),
                oid('6'),
                oid('7'),
                "Advance core.repo:tasks",
            ),
        })
        .unwrap();
    assert!(matches!(child, ArtifactCommitState::ChildCommitted { .. }));

    let parent = machine
        .apply(ArtifactCommitInput::RecordParentCommit {
            observed_commit: oid('7'),
        })
        .unwrap();
    assert!(matches!(
        parent,
        ArtifactCommitState::ParentCommitted { .. }
    ));

    let committed = machine
        .apply(ArtifactCommitInput::ConfirmDurableEvidence)
        .unwrap();
    assert!(matches!(committed, ArtifactCommitState::Committed { .. }));
}

// AGL172-028, AGL172-042 and AGL172-043.
#[test]
fn exact_replay_is_idempotent_and_different_commit_material_conflicts() {
    let mut machine = ArtifactCommitMachine::default();
    let prepare = ArtifactCommitInput::Prepare(Box::new(preparation()));
    let first = machine.apply(prepare.clone()).unwrap();
    let revision = machine.revision();
    assert_eq!(machine.apply(prepare).unwrap(), first);
    assert_eq!(machine.revision(), revision);

    let wrong = ArtifactCommitInput::RecordChildCommit {
        observed_commit: oid('9'),
        parent_commit: GitCommitMaterial::fixture(
            oid('1'),
            oid('6'),
            oid('7'),
            "Advance core.repo:tasks",
        ),
    };
    let error = machine.apply(wrong).unwrap_err();
    assert!(error.is_identity_conflict());
    assert!(matches!(
        machine.state(),
        Some(ArtifactCommitState::Prepared { .. })
    ));
}

// AGL172-029 and AGL172-042.
#[test]
fn unsafe_parent_after_child_commit_is_terminal_conflict_and_retains_child() {
    let mut machine = ArtifactCommitMachine::default();
    machine
        .apply(ArtifactCommitInput::Prepare(Box::new(preparation())))
        .unwrap();
    machine
        .apply(ArtifactCommitInput::RecordChildCommit {
            observed_commit: oid('5'),
            parent_commit: GitCommitMaterial::fixture(
                oid('1'),
                oid('6'),
                oid('7'),
                "Advance core.repo:tasks",
            ),
        })
        .unwrap();
    let conflict = machine
        .apply(ArtifactCommitInput::ObserveUnsafeParent {
            observed_head: oid('8'),
            observed_gitlink: oid('9'),
        })
        .unwrap();
    assert!(matches!(
        conflict,
        ArtifactCommitState::Conflict { ref child_commit, .. } if child_commit == &oid('5')
    ));
    assert!(
        machine
            .apply(ArtifactCommitInput::ConfirmDurableEvidence)
            .unwrap_err()
            .is_terminal()
    );
}

// AGL172-030, AGL172-065 and AGL172-066.
#[test]
fn domain_recovery_never_rewrites_terminal_kernel_outcome_unknown() {
    let (mut journal, correlation) =
        started_journal(Some(ToolEffectLifecycleState::OutcomeUnknown));
    let before = journal.records().to_vec();

    let result = agl_artifact::reconcile_tool_effect(
        &mut journal,
        &correlation,
        ArtifactCommitState::Committed {
            child_commit: oid('5'),
            parent_commit: oid('7'),
        },
    )
    .unwrap();
    assert_eq!(journal.records(), before);
    assert_eq!(
        result.tool_effect_state(),
        ToolEffectLifecycleState::OutcomeUnknown
    );
    assert!(matches!(
        result.artifact_state(),
        ArtifactCommitState::Committed { .. }
    ));
}

// AGL172-027 and AGL172-065.
#[test]
fn committed_domain_evidence_precedes_one_kernel_terminal_record() {
    let (mut journal, correlation) = started_journal(None);
    let result = agl_artifact::reconcile_tool_effect(
        &mut journal,
        &correlation,
        ArtifactCommitState::Committed {
            child_commit: oid('5'),
            parent_commit: oid('7'),
        },
    )
    .unwrap();
    assert_eq!(
        result.evidence_order(),
        [
            "artifact.committed",
            "tool_effect.committed",
            "tool.success",
        ]
    );
    assert_eq!(
        journal
            .records()
            .iter()
            .filter(|record| {
                matches!(
                    record.state(),
                    ToolEffectLifecycleState::Committed
                        | ToolEffectLifecycleState::Failed
                        | ToolEffectLifecycleState::Cancelled
                        | ToolEffectLifecycleState::OutcomeUnknown
                )
            })
            .count(),
        1
    );
}
