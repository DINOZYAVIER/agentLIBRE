use super::*;
use crate::artifacts::ArtifactWriteFailpoint;
use agl_content::{
    ArtifactRetention, ArtifactSensitivity, ArtifactSource, ArtifactSourceKind, ImageDimensions,
    MediaType,
};
use agl_events::{EventScope, SafeRuntimeEvent, SafeRuntimeEventEnvelope};
use agl_ids::{EventId, RunId, SessionId, StepId, TurnId};
use serde_json::json;

#[test]
fn current_schema_version_matches_last_migration() {
    assert_eq!(
        STORE_MIGRATIONS.last().map(|migration| migration.version),
        Some(CURRENT_SCHEMA_VERSION)
    );
    for window in STORE_MIGRATIONS.windows(2) {
        assert!(
            window[0].version < window[1].version,
            "store migrations must be ordered"
        );
    }
}

#[test]
fn opens_store_at_explicit_root_and_reports_health() {
    let root = temp_root("health");
    let store_root = root.join("data/store");

    let store = AglStore::open_at(&store_root).unwrap();
    let health = store.health().unwrap();
    let status = store.status().unwrap();

    assert_eq!(health.migration_version, CURRENT_SCHEMA_VERSION);
    assert_eq!(
        health.database_path,
        root.join("data/store/agentlibre.sqlite3")
    );
    assert_eq!(status.schema_version, CURRENT_SCHEMA_VERSION);
    assert_eq!(status.domains.len(), StoreDomain::all().len());
    assert!(status.domains.iter().all(|domain| domain.total_rows == 0));
}

#[test]
fn explicit_migration_reports_applied_steps_and_schema_status() {
    let root = temp_root("explicit-migrate");
    let before = AglStore::schema_status_at(&root).unwrap();
    assert!(!before.database_exists);
    assert!(before.migration_required);

    let report = AglStore::migrate_at(&root).unwrap();
    let after = AglStore::schema_status_at(&root).unwrap();

    assert_eq!(report.before_schema_version, 0);
    assert_eq!(report.after_schema_version, CURRENT_SCHEMA_VERSION);
    assert_eq!(report.applied_migrations.len(), STORE_MIGRATIONS.len());
    assert!(after.database_exists);
    assert!(!after.migration_required);
}

#[test]
fn transaction_commits_and_rolls_back() {
    let (_root, store) = open_temp_store("transaction");
    store
        .transaction(|tx| {
            tx.execute("CREATE TABLE tx_probe(value TEXT NOT NULL)", [])?;
            Ok(())
        })
        .unwrap();

    let err = store
        .transaction(|tx| {
            tx.execute("INSERT INTO tx_probe(value) VALUES ('rolled_back')", [])?;
            Err::<(), StoreError>(StoreError::InvalidValue {
                field: "tx",
                value: "rollback".to_string(),
                reason: "test rollback",
            })
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidValue { field: "tx", .. }));
    let count: u64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM tx_probe", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);

    store
        .transaction(|tx| {
            tx.execute("INSERT INTO tx_probe(value) VALUES ('committed')", [])?;
            Ok(())
        })
        .unwrap();
    let count: u64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM tx_probe", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn connections_enforce_durable_pragmas() {
    let (root, store) = open_temp_store("connection-pragmas");
    let (journal, foreign_keys, synchronous, busy_timeout) =
        crate::connection::connection_pragmas(store.connection()).unwrap();
    assert_eq!(journal, "wal");
    assert!(foreign_keys);
    assert_eq!(synchronous, 2);
    assert_eq!(busy_timeout, 5_000);

    drop(store);
    let read_only = AglStore::open_current_read_only_at(&root).unwrap();
    let (journal, foreign_keys, _synchronous, busy_timeout) =
        crate::connection::connection_pragmas(read_only.connection()).unwrap();
    assert_eq!(journal, "wal");
    assert!(foreign_keys);
    assert_eq!(busy_timeout, 5_000);
}

#[cfg(unix)]
#[test]
fn database_and_wal_sidecars_remain_private() {
    use std::os::unix::fs::PermissionsExt;

    let (root, store) = open_temp_store("private-sidecars");
    store
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO runs
                 (id, kind, state, priority, input_json, budget_json, usage_json,
                  lease_generation, attempts, created_at_ms, updated_at_ms)
                 VALUES (?1, 'cron', 'queued', 0, '{}', '{}', '{}', 0, 0, 1, 1)",
                [RunId::generate().as_str()],
            )?;
            Ok(())
        })
        .unwrap();

    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let database = store.database_path().to_path_buf();
    for path in [
        database.clone(),
        PathBuf::from(format!("{}-wal", database.display())),
        PathBuf::from(format!("{}-shm", database.display())),
    ] {
        assert!(path.exists(), "{} should exist", path.display());
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn durable_run_repository_enforces_fifo_fencing_and_event_uniqueness() {
    let (_root, store) = open_temp_store("durable-run");
    let session_id = SessionId::generate();
    let first = run_draft(Some(session_id.clone()));
    let second = run_draft(Some(session_id));
    store.admit_run_at(&first, 10).unwrap();
    store.admit_run_at(&second, 11).unwrap();

    let lease = store.claim_next_run("owner-a", 20, 100).unwrap().unwrap();
    assert_eq!(lease.run_id, first.run_id);
    assert!(store.claim_next_run("owner-b", 20, 100).unwrap().is_none());

    let event = safe_event(
        &first,
        1,
        SafeRuntimeEvent::TurnStarted {
            user_input_bytes: 5,
        },
    );
    let step = RunStepDraft {
        step_id: StepId::generate(),
        turn_id: first.turn_id.clone(),
        effect_sequence: 1,
        effect_kind: "model_generation".to_string(),
        delivery_class: EffectDeliveryClass::ReplaySafe,
        request: json!({"effect": "model_generation"}),
    };
    store
        .publish_run_step(&lease, &json!({"phase": "pending"}), &step, &[event], 21)
        .unwrap();
    let step_lease = store
        .claim_run_step(&lease, &step.step_id, 120, 22)
        .unwrap();
    store
        .complete_run_step(
            &lease,
            &step_lease,
            RunStepState::Succeeded,
            Some(&json!({"ok": true})),
            &json!({"phase": "complete"}),
            &RunUsage::default(),
            &[],
            None,
            23,
        )
        .unwrap();
    assert!(matches!(
        store.complete_run_step(
            &lease,
            &step_lease,
            RunStepState::Succeeded,
            None,
            &json!({}),
            &RunUsage::default(),
            &[],
            None,
            24,
        ),
        Err(StoreError::LeaseLost { .. })
    ));
    store
        .finish_run(
            &lease,
            RunState::Succeeded,
            Some(&json!({"terminal": true})),
            &RunUsage::default(),
            Some(&json!({"status": "answered"})),
            None,
            None,
            &[],
            25,
        )
        .unwrap();

    let second_lease = store.claim_next_run("owner-b", 26, 100).unwrap().unwrap();
    assert_eq!(second_lease.run_id, second.run_id);
    let events = store.run_events_after(&first.run_id, 0, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence, 1);
}

#[test]
fn child_admission_is_atomic_clamped_and_replay_safe() {
    let (_root, store) = open_temp_store("child-admission");
    let (parent, _parent_lease, step, _step_lease) = running_delegation_step(&store, 1);
    let mut draft = child_run_draft(&parent.run_id, &step.step_id);
    draft.budget.model_output_tokens = 100;
    draft.tree_budget.max_total_output_tokens = 40;

    let admitted = store.admit_child_run_at(&draft, 5).unwrap();
    assert!(!admitted.replayed);
    assert_eq!(admitted.run.kind, RunKind::Subagent);
    assert_eq!(admitted.run.session_id, None);
    assert_eq!(admitted.run.turn_id, None);
    assert_eq!(admitted.run.parent_run_id.as_ref(), Some(&parent.run_id));
    assert_eq!(admitted.run.root_run_id, parent.run_id);
    assert_eq!(admitted.run.depth, 1);
    assert_eq!(admitted.run.budget.model_output_tokens, 40);

    let root = store.run(&parent.run_id).unwrap().unwrap();
    assert_eq!(root.delegation_budget, Some(draft.tree_budget.clone()));
    assert_eq!(root.delegation_reserved_descendants, 1);
    assert_eq!(root.delegation_reserved_output_tokens, 40);

    let mut replay = draft.clone();
    replay.run_id = RunId::generate();
    let replayed = store.admit_child_run_at(&replay, 6).unwrap();
    assert!(replayed.replayed);
    assert_eq!(replayed.run.run_id, admitted.run.run_id);

    replay.subagent_id = "forged".to_string();
    assert!(matches!(
        store.admit_child_run_at(&replay, 7),
        Err(StoreError::DelegationDenied {
            code: "spawn_replay_mismatch"
        })
    ));
    assert_eq!(store.run_children(&parent.run_id).unwrap().len(), 1);
    let tree = store.run_tree(&parent.run_id).unwrap();
    assert_eq!(tree.len(), 2);
    assert_eq!(tree[1].spawned_by_step_id.as_ref(), Some(&step.step_id));
    assert_eq!(
        tree[1].child_spec_digest.as_deref(),
        Some(digest('b').as_str())
    );
}

#[test]
fn terminal_child_rolls_up_usage_and_is_delivered_once() {
    let (_root, store) = open_temp_store("child-delivery");
    let (parent, parent_lease, step, step_lease) = running_delegation_step(&store, 1);
    let draft = child_run_draft(&parent.run_id, &step.step_id);
    let child = store.admit_child_run_at(&draft, 5).unwrap().run;
    store
        .retry_run_step(
            &parent_lease,
            &step_lease,
            100,
            "delegation.child_waiting",
            &json!({"phase": "waiting"}),
            &RunUsage::default(),
            &[],
            6,
        )
        .unwrap();

    let child_lease = store
        .claim_next_run("child-owner", 7, 100)
        .unwrap()
        .unwrap();
    assert_eq!(child_lease.run_id, child.run_id);
    let usage = RunUsage {
        model_output_tokens: 17,
        ..RunUsage::default()
    };
    store
        .finish_run(
            &child_lease,
            RunState::Succeeded,
            None,
            &usage,
            Some(&json!({"status": "answered", "answer": "done"})),
            None,
            None,
            &[],
            8,
        )
        .unwrap();

    let root = store.run(&parent.run_id).unwrap().unwrap();
    assert_eq!(root.state, RunState::Waiting);
    assert_eq!(root.not_before_ms, Some(8));
    assert_eq!(root.delegation_reserved_output_tokens, 0);
    assert_eq!(root.delegation_used_output_tokens, 17);
    let completed_child = store.run(&child.run_id).unwrap().unwrap();
    assert!(completed_child.tree_usage_recorded_at_ms.is_some());
    assert!(completed_child.result_delivered_at_ms.is_none());

    let resumed = store
        .claim_next_run("parent-owner-2", 9, 100)
        .unwrap()
        .unwrap();
    assert_eq!(resumed.run_id, parent.run_id);
    let resumed_step = store
        .claim_run_step(&resumed, &step.step_id, 109, 9)
        .unwrap();
    store
        .complete_run_step(
            &resumed,
            &resumed_step,
            RunStepState::Succeeded,
            Some(&json!({"child_run_id": child.run_id, "status": "succeeded"})),
            &json!({"phase": "resumed"}),
            &RunUsage::default(),
            &[],
            None,
            10,
        )
        .unwrap();
    assert!(
        store
            .run(&child.run_id)
            .unwrap()
            .unwrap()
            .result_delivered_at_ms
            .is_some()
    );
    assert!(matches!(
        store.complete_run_step(
            &resumed,
            &resumed_step,
            RunStepState::Succeeded,
            None,
            &json!({}),
            &RunUsage::default(),
            &[],
            None,
            11,
        ),
        Err(StoreError::LeaseLost { .. })
    ));
}

#[test]
fn duplicate_child_spawn_race_creates_one_run() {
    let (root, store) = open_temp_store("child-race");
    let (parent, _parent_lease, step, _step_lease) = running_delegation_step(&store, 1);
    let draft = child_run_draft(&parent.run_id, &step.step_id);
    drop(store);

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles = (0..2)
        .map(|_| {
            let root = root.path.clone();
            let barrier = barrier.clone();
            let mut draft = draft.clone();
            draft.run_id = RunId::generate();
            std::thread::spawn(move || {
                let store = AglStore::open_current_at(root).unwrap();
                barrier.wait();
                store.admit_child_run_at(&draft, 5).unwrap()
            })
        })
        .collect::<Vec<_>>();
    let admissions = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(admissions[0].run.run_id, admissions[1].run.run_id);
    assert_eq!(admissions.iter().filter(|entry| !entry.replayed).count(), 1);
    let store = AglStore::open_current_at(&root).unwrap();
    assert_eq!(store.run_children(&parent.run_id).unwrap().len(), 1);
}

#[test]
fn cancellation_cascades_through_the_child_tree() {
    let (_root, store) = open_temp_store("child-cancel-tree");
    let (root, root_lease, root_step, root_step_lease) = running_delegation_step(&store, 1);
    let child_draft = child_run_draft(&root.run_id, &root_step.step_id);
    let child = store.admit_child_run_at(&child_draft, 5).unwrap().run;
    store
        .retry_run_step(
            &root_lease,
            &root_step_lease,
            100,
            "delegation.child_waiting",
            &json!({}),
            &RunUsage::default(),
            &[],
            6,
        )
        .unwrap();
    let child_lease = store
        .claim_next_run("child-owner", 7, 100)
        .unwrap()
        .unwrap();
    let nested_step = RunStepDraft {
        step_id: StepId::generate(),
        turn_id: None,
        effect_sequence: 1,
        effect_kind: "capability_dispatch".to_string(),
        delivery_class: EffectDeliveryClass::Idempotent,
        request: json!({"subagent_id": "nested", "task": "work"}),
    };
    store
        .publish_run_step(&child_lease, &json!({}), &nested_step, &[], 8)
        .unwrap();
    store
        .claim_run_step(&child_lease, &nested_step.step_id, 108, 8)
        .unwrap();
    let mut grandchild_draft = child_run_draft(&child.run_id, &nested_step.step_id);
    grandchild_draft.subagent_id = "nested".to_string();
    grandchild_draft.tree_budget = child_draft.tree_budget.clone();
    let grandchild = store.admit_child_run_at(&grandchild_draft, 9).unwrap().run;

    let statuses = store
        .request_run_tree_cancellation(&root.run_id, 10)
        .unwrap();
    assert_eq!(statuses.len(), 3);
    assert_eq!(
        store.run(&root.run_id).unwrap().unwrap().state,
        RunState::Cancelled
    );
    let running_child = store.run(&child.run_id).unwrap().unwrap();
    assert_eq!(running_child.state, RunState::Running);
    assert!(running_child.cancellation_requested_at_ms.is_some());
    assert_eq!(
        store.run(&grandchild.run_id).unwrap().unwrap().state,
        RunState::Cancelled
    );

    store
        .finish_run(
            &child_lease,
            RunState::Cancelled,
            None,
            &RunUsage::default(),
            None,
            Some("run_cancelled"),
            None,
            &[],
            11,
        )
        .unwrap();
    let tree = store.run_tree(&root.run_id).unwrap();
    assert!(tree.iter().all(|run| run.state == RunState::Cancelled));
    let root = store.run(&root.run_id).unwrap().unwrap();
    assert_eq!(root.delegation_reserved_output_tokens, 0);
}

#[test]
fn tree_budget_denials_do_not_create_or_reserve_children() {
    for (case, expected_code) in [
        ("fanout", "fanout_exhausted"),
        ("descendants", "descendants_exhausted"),
        ("output", "output_tokens_exhausted"),
    ] {
        let (_root, store) = open_temp_store(&format!("child-budget-{case}"));
        let (parent, parent_lease, first_step, _first_step_lease) =
            running_delegation_step(&store, 1);
        let mut first = child_run_draft(&parent.run_id, &first_step.step_id);
        match case {
            "fanout" => first.tree_budget.max_children_per_run = 1,
            "descendants" => first.tree_budget.max_descendants = 1,
            "output" => first.tree_budget.max_total_output_tokens = 40,
            _ => unreachable!(),
        }
        store.admit_child_run_at(&first, 5).unwrap();

        let second_step = RunStepDraft {
            step_id: StepId::generate(),
            turn_id: parent.turn_id.clone(),
            effect_sequence: 2,
            effect_kind: "capability_dispatch".to_string(),
            delivery_class: EffectDeliveryClass::Idempotent,
            request: json!({"subagent_id": "reviewer", "task": "second"}),
        };
        store
            .publish_run_step(&parent_lease, &json!({}), &second_step, &[], 6)
            .unwrap();
        store
            .claim_run_step(&parent_lease, &second_step.step_id, 107, 7)
            .unwrap();
        let mut second = child_run_draft(&parent.run_id, &second_step.step_id);
        second.input = json!({"run_kind": "subagent", "task": "second"});
        second.tree_budget = first.tree_budget.clone();

        assert!(matches!(
            store.admit_child_run_at(&second, 8),
            Err(StoreError::DelegationDenied { code }) if code == expected_code
        ));
        assert_eq!(store.run_children(&parent.run_id).unwrap().len(), 1);
        let root = store.run(&parent.run_id).unwrap().unwrap();
        assert_eq!(root.delegation_reserved_descendants, 1);
        assert_eq!(
            root.delegation_reserved_output_tokens,
            first.tree_budget.max_total_output_tokens.min(100)
        );
    }

    let (_root, store) = open_temp_store("child-budget-timeout");
    let (parent, _lease, step, _step_lease) = running_delegation_step(&store, 1);
    let mut expired = child_run_draft(&parent.run_id, &step.step_id);
    expired.tree_budget.timeout_ms = 3;
    assert!(matches!(
        store.admit_child_run_at(&expired, 5),
        Err(StoreError::DelegationDenied {
            code: "tree_timeout_exhausted"
        })
    ));
    let root = store.run(&parent.run_id).unwrap().unwrap();
    assert!(root.delegation_budget.is_none());
    assert_eq!(root.delegation_reserved_descendants, 0);
    assert_eq!(root.delegation_reserved_output_tokens, 0);
}

#[test]
fn depth_budget_is_enforced_against_persisted_relationships() {
    let (_root, store) = open_temp_store("child-budget-depth");
    let (root, root_lease, root_step, root_step_lease) = running_delegation_step(&store, 1);
    let mut child_draft = child_run_draft(&root.run_id, &root_step.step_id);
    child_draft.tree_budget.max_depth = 1;
    let child = store.admit_child_run_at(&child_draft, 5).unwrap().run;
    store
        .retry_run_step(
            &root_lease,
            &root_step_lease,
            100,
            "delegation.child_waiting",
            &json!({}),
            &RunUsage::default(),
            &[],
            6,
        )
        .unwrap();
    let child_lease = store
        .claim_next_run("child-owner", 7, 100)
        .unwrap()
        .unwrap();
    let step = RunStepDraft {
        step_id: StepId::generate(),
        turn_id: None,
        effect_sequence: 1,
        effect_kind: "capability_dispatch".to_string(),
        delivery_class: EffectDeliveryClass::Idempotent,
        request: json!({"subagent_id": "nested", "task": "work"}),
    };
    store
        .publish_run_step(&child_lease, &json!({}), &step, &[], 8)
        .unwrap();
    store
        .claim_run_step(&child_lease, &step.step_id, 108, 8)
        .unwrap();
    let mut nested = child_run_draft(&child.run_id, &step.step_id);
    nested.subagent_id = "nested".to_string();
    nested.tree_budget = child_draft.tree_budget;
    assert!(matches!(
        store.admit_child_run_at(&nested, 9),
        Err(StoreError::DelegationDenied {
            code: "depth_exhausted"
        })
    ));
    assert!(store.run_children(&child.run_id).unwrap().is_empty());
    assert_eq!(
        store
            .run(&root.run_id)
            .unwrap()
            .unwrap()
            .delegation_reserved_descendants,
        1
    );
}

#[test]
fn absolute_tree_timeout_cancels_waiting_parent_and_queued_child() {
    let (_root, store) = open_temp_store("child-tree-timeout");
    let (parent, parent_lease, step, step_lease) = running_delegation_step(&store, 1);
    let mut child = child_run_draft(&parent.run_id, &step.step_id);
    child.tree_budget.timeout_ms = 5;
    let child_run = store.admit_child_run_at(&child, 4).unwrap().run;
    store
        .retry_run_step(
            &parent_lease,
            &step_lease,
            100,
            "delegation.child_waiting",
            &json!({}),
            &RunUsage::default(),
            &[],
            5,
        )
        .unwrap();

    let statuses = store.expire_delegation_trees(6).unwrap();
    assert_eq!(statuses.len(), 2);
    assert_eq!(
        store.run(&parent.run_id).unwrap().unwrap().state,
        RunState::Cancelled
    );
    assert_eq!(
        store.run(&child_run.run_id).unwrap().unwrap().state,
        RunState::Cancelled
    );
    let parent = store.run(&parent.run_id).unwrap().unwrap();
    assert_eq!(parent.delegation_reserved_output_tokens, 0);
    assert!(store.expire_delegation_trees(7).unwrap().is_empty());
}

#[test]
fn startup_recovery_reconciles_an_undelivered_terminal_child() {
    let (_root, store) = open_temp_store("child-terminal-recovery");
    let (parent, parent_lease, step, step_lease) = running_delegation_step(&store, 1);
    let child_draft = child_run_draft(&parent.run_id, &step.step_id);
    let child = store.admit_child_run_at(&child_draft, 5).unwrap().run;
    store
        .retry_run_step(
            &parent_lease,
            &step_lease,
            100,
            "delegation.child_waiting",
            &json!({}),
            &RunUsage::default(),
            &[],
            6,
        )
        .unwrap();
    let usage = RunUsage {
        model_output_tokens: 7,
        ..RunUsage::default()
    };
    store
        .connection()
        .execute(
            "UPDATE runs
             SET state = 'succeeded', usage_json = ?1, terminal_result_json = ?2,
                 finished_at_ms = 7, updated_at_ms = 7
             WHERE id = ?3",
            rusqlite::params![
                serde_json::to_string(&usage).unwrap(),
                serde_json::to_string(&json!({"status": "answered", "answer": "done"})).unwrap(),
                child.run_id.as_str()
            ],
        )
        .unwrap();

    store.recover_expired_work(8).unwrap();

    let root = store.run(&parent.run_id).unwrap().unwrap();
    assert_eq!(root.delegation_reserved_output_tokens, 0);
    assert_eq!(root.delegation_used_output_tokens, 7);
    assert_eq!(root.not_before_ms, Some(8));
    let step = store.run_steps(&parent.run_id).unwrap().remove(0);
    assert_eq!(step.not_before_ms, Some(8));
    let child = store.run(&child.run_id).unwrap().unwrap();
    assert_eq!(child.tree_usage_recorded_at_ms, Some(8));
    assert!(child.result_delivered_at_ms.is_none());
}

#[test]
fn recovery_requeues_safe_work_and_fails_uncertain_at_most_once_work() {
    let (_root, store) = open_temp_store("recovery");
    let safe = run_draft(None);
    store.admit_run_at(&safe, 1).unwrap();
    let safe_lease = store.claim_next_run("owner", 2, 10).unwrap().unwrap();
    let safe_step = RunStepDraft {
        step_id: StepId::generate(),
        turn_id: None,
        effect_sequence: 1,
        effect_kind: "hook_batch".to_string(),
        delivery_class: EffectDeliveryClass::ReplaySafe,
        request: json!({}),
    };
    store
        .publish_run_step(&safe_lease, &json!({}), &safe_step, &[], 3)
        .unwrap();
    store
        .claim_run_step(&safe_lease, &safe_step.step_id, 12, 4)
        .unwrap();

    let uncertain = run_draft(None);
    store.admit_run_at(&uncertain, 5).unwrap();
    let report = store.recover_expired_work(13).unwrap();
    assert_eq!(report.requeued_steps, 1);
    assert_eq!(report.requeued_runs, 1);
    assert_eq!(
        store.run_steps(&safe.run_id).unwrap()[0].state,
        RunStepState::Pending
    );

    let recovered_safe_lease = store.claim_next_run("owner", 14, 10).unwrap().unwrap();
    let recovered_step_lease = store
        .claim_run_step(&recovered_safe_lease, &safe_step.step_id, 24, 15)
        .unwrap();
    store
        .complete_run_step(
            &recovered_safe_lease,
            &recovered_step_lease,
            RunStepState::Succeeded,
            Some(&json!({"ok": true})),
            &json!({}),
            &RunUsage::default(),
            &[],
            None,
            16,
        )
        .unwrap();
    store
        .finish_run(
            &recovered_safe_lease,
            RunState::Succeeded,
            None,
            &RunUsage::default(),
            None,
            None,
            None,
            &[],
            17,
        )
        .unwrap();

    let uncertain_lease = store.claim_next_run("owner", 18, 10).unwrap().unwrap();
    assert_eq!(uncertain_lease.run_id, uncertain.run_id);
    let uncertain_step = RunStepDraft {
        step_id: StepId::generate(),
        turn_id: None,
        effect_sequence: 1,
        effect_kind: "capability_dispatch".to_string(),
        delivery_class: EffectDeliveryClass::AtMostOnce,
        request: json!({}),
    };
    store
        .publish_run_step(&uncertain_lease, &json!({}), &uncertain_step, &[], 19)
        .unwrap();
    store
        .claim_run_step(&uncertain_lease, &uncertain_step.step_id, 28, 20)
        .unwrap();
    let report = store.recover_expired_work(29).unwrap();
    assert_eq!(report.outcome_unknown_steps, 1);
    assert_eq!(report.failed_runs, 1);
    assert_eq!(
        store
            .safe_run_status(&uncertain.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Failed
    );
}

#[test]
fn artifacts_are_private_deduplicated_and_run_scoped() {
    let (root, store) = open_temp_store("artifacts");
    let first_run = run_draft(None);
    let second_run = run_draft(None);
    store.admit_run(&first_run).unwrap();
    store.admit_run(&second_run).unwrap();
    let bytes = b"validated-png-bytes";
    let source = ArtifactSource {
        kind: ArtifactSourceKind::ScreenCapture,
        provider: Some("fake-portal".to_string()),
    };
    let first = store
        .write_artifact(
            &first_run.run_id,
            MediaType::ImagePng,
            bytes,
            Some(ImageDimensions::new(2, 2).unwrap()),
            ArtifactSensitivity::Sensitive,
            source.clone(),
            ArtifactRetention::RunScoped,
        )
        .unwrap();
    let second = store
        .write_artifact(
            &second_run.run_id,
            MediaType::ImagePng,
            bytes,
            Some(ImageDimensions::new(2, 2).unwrap()),
            ArtifactSensitivity::Sensitive,
            source,
            ArtifactRetention::RunScoped,
        )
        .unwrap();

    assert_ne!(first.reference.artifact_id, second.reference.artifact_id);
    assert_eq!(first.reference.digest, second.reference.digest);
    let blob_count: u64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM content_blobs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(blob_count, 1);
    assert_eq!(
        store
            .resolve_artifact(&first_run.run_id, &first.reference)
            .unwrap()
            .bytes,
        bytes
    );
    assert!(matches!(
        store.resolve_artifact(&second_run.run_id, &first.reference),
        Err(StoreError::ArtifactAccessDenied)
    ));
    let encoded = serde_json::to_string(&first.reference).unwrap();
    assert!(!encoded.contains(root.to_string_lossy().as_ref()));
    assert!(!encoded.contains("validated-png-bytes"));

    store.tombstone_run_artifacts(&first_run.run_id).unwrap();
    let first_gc = store.garbage_collect_artifacts().unwrap();
    assert_eq!(first_gc.artifact_records_deleted, 1);
    assert_eq!(first_gc.blob_records_deleted, 0);
    assert!(
        store
            .resolve_artifact(&second_run.run_id, &second.reference)
            .is_ok()
    );
    store.tombstone_run_artifacts(&second_run.run_id).unwrap();
    let second_gc = store.garbage_collect_artifacts().unwrap();
    assert_eq!(second_gc.blob_records_deleted, 1);
    assert_eq!(second_gc.blob_files_deleted, 1);
}

#[test]
fn concurrent_identical_artifact_writes_share_one_complete_blob() {
    let (root, store) = open_temp_store("artifact-concurrency");
    let first = run_draft(None);
    let second = run_draft(None);
    store.admit_run(&first).unwrap();
    store.admit_run(&second).unwrap();
    drop(store);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles = [first.run_id, second.run_id].map(|run_id| {
        let root = root.path.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            let store = AglStore::open_current_at(root).unwrap();
            barrier.wait();
            store
                .write_artifact(
                    &run_id,
                    MediaType::ImagePng,
                    b"same-private-image",
                    Some(ImageDimensions::new(1, 1).unwrap()),
                    ArtifactSensitivity::Sensitive,
                    ArtifactSource {
                        kind: ArtifactSourceKind::ScreenCapture,
                        provider: Some("fake".to_string()),
                    },
                    ArtifactRetention::RunScoped,
                )
                .unwrap()
        })
    });
    let artifacts = handles.map(|handle| handle.join().unwrap());
    assert_eq!(artifacts[0].reference.digest, artifacts[1].reference.digest);
    let store = AglStore::open_current_at(&root).unwrap();
    let blob_count: u64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM content_blobs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(blob_count, 1);
}

#[test]
fn artifact_write_failpoints_leave_valid_metadata_or_collectable_orphans() {
    let (root, store) = open_temp_store("artifact-failpoints");
    let run = run_draft(None);
    store.admit_run(&run).unwrap();
    let write_at = |bytes: &[u8], failpoint| {
        store.write_artifact_injected(
            &run.run_id,
            MediaType::ImagePng,
            bytes,
            Some(ImageDimensions::new(1, 1).unwrap()),
            ArtifactSensitivity::Sensitive,
            ArtifactSource {
                kind: ArtifactSourceKind::ScreenCapture,
                provider: Some("fake".to_string()),
            },
            ArtifactRetention::RunScoped,
            failpoint,
        )
    };

    assert!(write_at(b"before-blob", ArtifactWriteFailpoint::BeforeBlobWrite).is_err());
    assert_eq!(
        store
            .garbage_collect_artifacts()
            .unwrap()
            .orphan_files_deleted,
        0
    );

    for (bytes, failpoint) in [
        (
            b"after-blob".as_slice(),
            ArtifactWriteFailpoint::AfterBlobWrite,
        ),
        (
            b"before-metadata".as_slice(),
            ArtifactWriteFailpoint::BeforeMetadataCommit,
        ),
    ] {
        assert!(write_at(bytes, failpoint).is_err());
        let report = store.garbage_collect_artifacts().unwrap();
        assert_eq!(report.orphan_files_deleted, 1);
    }

    assert!(
        write_at(
            b"after-metadata",
            ArtifactWriteFailpoint::AfterMetadataCommit,
        )
        .is_err()
    );
    let artifact_id: String = store
        .connection()
        .query_row("SELECT id FROM artifacts", [], |row| row.get(0))
        .unwrap();
    let stored = store
        .artifact(&agl_content::ArtifactId::parse(artifact_id).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .resolve_artifact(&run.run_id, &stored.reference)
            .unwrap()
            .bytes,
        b"after-metadata"
    );
    assert_eq!(
        store
            .garbage_collect_artifacts()
            .unwrap()
            .orphan_files_deleted,
        0
    );

    let temp_root = root.join("blobs/.tmp");
    std::fs::create_dir_all(&temp_root).unwrap();
    std::fs::write(temp_root.join("stale-private-temp"), b"partial").unwrap();
    assert_eq!(
        store
            .garbage_collect_artifacts()
            .unwrap()
            .orphan_files_deleted,
        1
    );
}

#[test]
fn idempotent_admission_has_one_durable_run() {
    let (root, _store) = open_temp_store("idempotent-run");
    let root = root.path.clone();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles = (0..2)
        .map(|_| {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = AglStore::open_current_at(root).unwrap();
                let draft = run_draft(None);
                barrier.wait();
                store
                    .admit_idempotent_run(
                        &draft,
                        "run.submit",
                        "same-key",
                        "same-fingerprint",
                        "owner",
                        100,
                        1,
                    )
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let admissions = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(admissions[0].run.run_id, admissions[1].run.run_id);
    assert_eq!(admissions.iter().filter(|entry| !entry.replayed).count(), 1);
}

#[test]
fn migration_history_gaps_fail_clearly() {
    let root = temp_root("migration-gap");
    write_raw_database(
        &root,
        r#"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations(version, applied_at)
            VALUES (1, 'unix:1'), (3, 'unix:3');
            PRAGMA user_version = 3;
            "#,
    );

    let err = AglStore::open_at(&root).unwrap_err();
    assert!(matches!(err, StoreError::MigrationGap { missing: 2 }));
}

#[test]
fn store_status_counts_domain_rows() {
    let (_root, store) = open_temp_store("domain-status");
    execute_fixture(
        &store,
        "INSERT INTO memory_entries
                 (id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at)
                 VALUES ('mem_active', 'user', 'default', 'fact', 'Active', 'Body', NULL, 100, 'unix:1', 'unix:1', NULL),
                        ('mem_deleted', 'user', 'default', 'fact', 'Deleted', 'Body', NULL, 100, 'unix:1', 'unix:2', 'unix:3')",
    );
    execute_fixture(
        &store,
        "INSERT INTO notes
                 (id, title, body, created_at, updated_at, deleted_at)
                 VALUES ('note_active', 'Active', 'Body', 'unix:1', 'unix:1', NULL),
                        ('note_deleted', 'Deleted', 'Body', 'unix:1', 'unix:2', 'unix:3')",
    );
    execute_fixture(
        &store,
        "INSERT INTO cron_jobs
                 (id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, created_at, updated_at, deleted_at)
                 VALUES ('cron_active', 'Active', 1, 'builtin', 'store-status', 'hourly', 'UTC', NULL, 'unix:1', 'unix:1', NULL),
                        ('cron_deleted', 'Deleted', 0, 'builtin', 'store-status', 'hourly', 'UTC', NULL, 'unix:1', 'unix:2', 'unix:3')",
    );

    let status = store.status().unwrap();

    for domain in status.domains {
        assert_eq!(domain.status, StoreDomainStatus::Ok);
        if domain.domain == StoreDomain::Permissions {
            assert_eq!(domain.total_rows, 0, "domain={}", domain.domain.as_str());
            assert_eq!(domain.active_rows, 0, "domain={}", domain.domain.as_str());
        } else {
            assert_eq!(domain.total_rows, 2, "domain={}", domain.domain.as_str());
            assert_eq!(domain.active_rows, 1, "domain={}", domain.domain.as_str());
        }
    }
}

#[test]
fn status_reports_in_progress_idempotency_without_recovering_it() {
    let (_root, store) = open_temp_store("stale-idempotency");
    execute_fixture(
        &store,
        "INSERT INTO idempotency_keys
                 (namespace, key, fingerprint, status, result_ref, created_at, updated_at)
                 VALUES ('cron.run', 'job-1:unix:60', 'sha256:abc', 'in_progress', NULL, 'unix:1', 'unix:2')",
    );

    let status = store.status().unwrap();
    let record = store
        .idempotency_record("cron.run", "job-1:unix:60")
        .unwrap()
        .expect("idempotency record should remain present");

    assert_eq!(status.idempotency.in_progress, 1);
    assert_eq!(status.idempotency.stale_in_progress.len(), 1);
    assert_eq!(status.idempotency.stale_in_progress[0].key, "job-1:unix:60");
    assert_eq!(record.status, IdempotencyStatus::InProgress);
    assert!(record.result_ref.is_none());
}

#[test]
fn export_memory_jsonl_respects_tombstones() {
    let (_root, store) = open_temp_store("export-memory");
    execute_fixture(
        &store,
        "INSERT INTO memory_entries
                 (id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at)
                 VALUES ('mem_active', 'user', 'default', 'fact', 'Active', 'Body', NULL, 100, 'unix:1', 'unix:1', NULL),
                        ('mem_deleted', 'user', 'default', 'fact', 'Deleted', 'Body', NULL, 100, 'unix:1', 'unix:2', 'unix:3')",
    );
    let (active_count, active) = export_domain(&store, StoreDomain::Memory, false);
    let (all_count, all) = export_domain(&store, StoreDomain::Memory, true);
    assert_eq!(active_count, 1);
    assert!(active.contains("\"id\":\"mem_active\""));
    assert!(!active.contains("mem_deleted"));
    assert_eq!(all_count, 2);
    assert!(all.contains("\"id\":\"mem_deleted\""));
}

#[test]
fn export_memory_jsonl_includes_pending_suggestions() {
    let (_root, store) = open_temp_store("export-memory-suggestions");
    execute_fixture(
        &store,
        "INSERT INTO memory_suggestions
                 (id, scope_kind, scope_key, kind, title, body, source_ref, confidence, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note)
                 VALUES ('suggest_pending', 'user', 'default', 'decision', 'Pending', 'Body', 'chat:1', 95, 'pending', 'unix:1', 'unix:1', NULL, NULL, NULL),
                        ('suggest_rejected', 'user', 'default', 'fact', 'Rejected', 'Body', 'chat:2', 90, 'rejected', 'unix:1', 'unix:2', 'unix:2', NULL, 'not durable')",
    );
    let (active_count, active) = export_domain(&store, StoreDomain::Memory, false);
    let (all_count, all) = export_domain(&store, StoreDomain::Memory, true);
    assert_eq!(active_count, 1);
    assert!(active.contains("\"record_type\":\"memory_suggestion\""));
    assert!(active.contains("\"id\":\"suggest_pending\""));
    assert!(!active.contains("suggest_rejected"));
    assert_eq!(all_count, 2);
    assert!(all.contains("\"id\":\"suggest_rejected\""));
}

#[test]
fn export_notes_and_cron_include_related_rows() {
    let (_root, store) = open_temp_store("export-related");
    execute_fixture(
        &store,
        "INSERT INTO notes
                 (id, title, body, created_at, updated_at, deleted_at)
                 VALUES ('note_active', 'Active', 'Body', 'unix:1', 'unix:1', NULL)",
    );
    execute_fixture(
        &store,
        "INSERT INTO note_links
                 (id, note_id, target_ref, label, created_at)
                 VALUES ('link_1', 'note_active', 'memory:mem_1', 'remembered', 'unix:2')",
    );
    execute_fixture(
        &store,
        "INSERT INTO cron_jobs
                 (id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, created_at, updated_at, deleted_at)
                 VALUES ('cron_active', 'Active', 1, 'builtin', 'store-status', 'hourly', 'UTC', NULL, 'unix:1', 'unix:1', NULL)",
    );
    execute_fixture(
        &store,
        "INSERT INTO cron_runs
                 (id, job_id, scheduled_for, started_at, finished_at, status, result_ref, error)
                 VALUES ('run_1', 'cron_active', 'unix:2', 'unix:2', 'unix:2', 'succeeded', 'builtin:store-status', NULL)",
    );
    let (notes_count, notes) = export_domain(&store, StoreDomain::Notes, false);
    let (cron_count, cron) = export_domain(&store, StoreDomain::Cron, false);
    assert_eq!(notes_count, 2);
    assert!(notes.contains("\"record_type\":\"note\""));
    assert!(notes.contains("\"record_type\":\"note_link\""));
    assert_eq!(cron_count, 2);
    assert!(cron.contains("\"record_type\":\"cron_job\""));
    assert!(cron.contains("\"record_type\":\"cron_run\""));
}

#[test]
fn matrix_notification_outbox_enqueues_once_and_exports_with_cron() {
    let (_root, store) = open_temp_store("matrix-outbox");

    let first = store
        .enqueue_matrix_notification(MatrixNotificationOutboxDraft::new(
            "matrix-room:!room",
            "cron",
            "run_1",
            "cron:run_1:matrix-room:!room",
            "Cron job completed.",
        ))
        .unwrap();
    let second = store
        .enqueue_matrix_notification(MatrixNotificationOutboxDraft::new(
            "matrix-room:!room",
            "cron",
            "run_1",
            "cron:run_1:matrix-room:!room",
            "Cron job completed.",
        ))
        .unwrap();
    let queued = store.queued_matrix_notifications(10).unwrap();

    assert_eq!(first.id, second.id);
    assert_eq!(queued, vec![first.clone()]);
    assert_eq!(first.status, MatrixNotificationOutboxStatus::Queued);

    let (page, truncated) = store.queued_matrix_notifications_page(1).unwrap();
    assert_eq!(page, vec![first.clone()]);
    assert!(!truncated);

    let sent = store.mark_matrix_notification_sent(&first.id).unwrap();
    assert_eq!(sent.status, MatrixNotificationOutboxStatus::Sent);
    assert!(sent.delivered_at.is_some());
    assert!(store.queued_matrix_notifications(10).unwrap().is_empty());

    let (count, cron) = export_domain(&store, StoreDomain::Cron, false);
    assert_eq!(count, 1);
    assert!(cron.contains("\"record_type\":\"matrix_notification_outbox\""));
    assert!(cron.contains("\"status\":\"sent\""));
}

#[test]
fn matrix_notification_outbox_page_reports_truncation_only_for_extra_rows() {
    let (_root, store) = open_temp_store("matrix-outbox-page");

    let first = store
        .enqueue_matrix_notification(MatrixNotificationOutboxDraft::new(
            "matrix-room:!room",
            "cron",
            "run_1",
            "cron:run_1:matrix-room:!room",
            "First.",
        ))
        .unwrap();
    let second = store
        .enqueue_matrix_notification(MatrixNotificationOutboxDraft::new(
            "matrix-room:!room",
            "cron",
            "run_2",
            "cron:run_2:matrix-room:!room",
            "Second.",
        ))
        .unwrap();

    let (exact_page, exact_truncated) = store.queued_matrix_notifications_page(2).unwrap();
    let (limited_page, limited_truncated) = store.queued_matrix_notifications_page(1).unwrap();

    assert_eq!(exact_page, vec![first.clone(), second]);
    assert!(!exact_truncated);
    assert_eq!(limited_page, vec![first]);
    assert!(limited_truncated);
}

#[test]
fn permission_requests_grants_and_revokes_are_persisted() {
    let (_root, store) = open_temp_store("permission-requests");

    let request = store
        .create_permission_request(PermissionRequestDraft {
            requested_tools: vec!["cron.add".to_string(), "matrix.outbox.enqueue".to_string()],
            max_operation_kind: "write".to_string(),
            state_effects: vec!["store_cron".to_string(), "matrix_outbox".to_string()],
            sensitive_inputs: Vec::new(),
            scope: json!({"repo": "/tmp/repo", "matrix_room": "!room:server"}),
            duration: "one_turn".to_string(),
            reason: "Schedule a daily Matrix greeting.".to_string(),
            requester_ref: "chat:session-1:turn-1".to_string(),
        })
        .unwrap();

    assert_eq!(request.status, PermissionRequestStatus::Pending);
    assert_eq!(request.requested_tools.len(), 2);
    assert_eq!(
        store.pending_permission_requests().unwrap(),
        vec![request.clone()]
    );

    let grants = store
        .grant_permission_request(&request.id, "cli:operator", Some("chat:session-1:turn-2"))
        .unwrap();
    let resolved = store.permission_request(&request.id).unwrap().unwrap();
    let active = store.active_permission_grants().unwrap();

    assert_eq!(resolved.status, PermissionRequestStatus::Granted);
    assert_eq!(
        resolved.resolution_ref.as_deref(),
        Some("chat:session-1:turn-2")
    );
    assert_eq!(grants.len(), 2);
    assert_eq!(active.len(), 2);
    assert!(
        active
            .iter()
            .all(|grant| grant.status == PermissionGrantStatus::Active)
    );
    assert!(active.iter().all(|grant| grant.duration == "one_turn"));

    let revoked = store
        .revoke_permission_grant(&active[0].id, Some("chat:session-1:turn-3"))
        .unwrap();
    assert_eq!(revoked.status, PermissionGrantStatus::Revoked);
    assert_eq!(revoked.revoke_ref.as_deref(), Some("chat:session-1:turn-3"));
    assert_eq!(store.active_permission_grants().unwrap().len(), 1);

    let status = store.status().unwrap();
    let permissions = status
        .domains
        .iter()
        .find(|domain| domain.domain == StoreDomain::Permissions)
        .unwrap();
    assert_eq!(permissions.total_rows, 3);
    assert_eq!(permissions.active_rows, 1);
}

#[test]
fn grant_permission_request_rolls_back_grants_when_resolution_fails() {
    let (_root, store) = open_temp_store("permission-grant-transaction");

    let request = store
        .create_permission_request(PermissionRequestDraft {
            requested_tools: vec!["cron.add".to_string(), "matrix.outbox.enqueue".to_string()],
            max_operation_kind: "write".to_string(),
            state_effects: vec!["store_cron".to_string(), "matrix_outbox".to_string()],
            sensitive_inputs: Vec::new(),
            scope: json!({"repo": "/tmp/repo", "matrix_room": "!room:server"}),
            duration: "one_turn".to_string(),
            reason: "Schedule a daily Matrix greeting.".to_string(),
            requester_ref: "chat:session-1:turn-1".to_string(),
        })
        .unwrap();
    store
        .connection()
        .execute_batch(
            "CREATE TRIGGER permission_request_resolution_blocker
             BEFORE UPDATE ON permission_requests
             BEGIN
                 SELECT RAISE(FAIL, 'blocked permission resolution');
             END;",
        )
        .unwrap();

    let err = store
        .grant_permission_request(&request.id, "cli:operator", Some("chat:session-1:turn-2"))
        .unwrap_err();
    assert!(
        err.to_string().contains("blocked permission resolution"),
        "{err}"
    );

    let unresolved = store.permission_request(&request.id).unwrap().unwrap();
    assert_eq!(unresolved.status, PermissionRequestStatus::Pending);
    assert_eq!(store.active_permission_grants().unwrap(), Vec::new());
}

#[test]
fn permission_export_reports_pending_and_historical_records() {
    let (_root, store) = open_temp_store("permission-export");
    let request = store
        .create_permission_request(PermissionRequestDraft {
            requested_tools: vec!["notes.add".to_string()],
            max_operation_kind: "write".to_string(),
            state_effects: vec!["store_notes".to_string()],
            sensitive_inputs: Vec::new(),
            scope: json!({"repo": "/tmp/repo"}),
            duration: "one_turn".to_string(),
            reason: "Create one explicit note.".to_string(),
            requester_ref: "chat:turn-1".to_string(),
        })
        .unwrap();
    let grants = store
        .grant_permission_request(&request.id, "cli:operator", Some("chat:turn-2"))
        .unwrap();
    store
        .revoke_permission_grant(&grants[0].id, Some("chat:turn-3"))
        .unwrap();

    let (active_count, active) = export_domain(&store, StoreDomain::Permissions, false);
    let (all_count, all) = export_domain(&store, StoreDomain::Permissions, true);
    assert_eq!(active_count, 0);
    assert_eq!(all_count, 2);
    assert!(active.is_empty());
    assert!(all.contains("\"record_type\":\"permission_request\""));
    assert!(all.contains("\"record_type\":\"permission_grant\""));
    assert!(all.contains("\"status\":\"revoked\""));
}

#[test]
fn migrations_are_repeatable() {
    let root = temp_root("repeatable");
    let first = AglStore::open_at(&root).unwrap();
    assert_eq!(
        first.health().unwrap().migration_version,
        CURRENT_SCHEMA_VERSION
    );
    drop(first);

    let second = AglStore::open_at(&root).unwrap();
    assert_eq!(
        second.health().unwrap().migration_version,
        CURRENT_SCHEMA_VERSION
    );
}

#[test]
fn schema_v1_database_migrates_to_current() {
    let root = temp_root("migrate-v1");
    write_raw_database(
        &root,
        r#"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
            CREATE TABLE idempotency_keys (
                namespace TEXT NOT NULL,
                key TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('in_progress', 'completed', 'failed')),
                result_ref TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (namespace, key)
            );
            INSERT INTO schema_migrations(version, applied_at)
            VALUES (1, 'unix:1');
            INSERT INTO idempotency_keys
                (namespace, key, fingerprint, status, result_ref, created_at, updated_at)
            VALUES ('cron.run', 'job-001:unix:1', 'sha256:abc', 'completed', 'run-001', 'unix:1', 'unix:1');
            PRAGMA user_version = 1;
            "#,
    );

    let store = AglStore::open_at(&root).unwrap();
    assert_eq!(
        store.health().unwrap().migration_version,
        CURRENT_SCHEMA_VERSION
    );
    let record = store
        .idempotency_record("cron.run", "job-001:unix:1")
        .unwrap()
        .expect("v1 idempotency record should migrate");
    assert_eq!(record.status, IdempotencyStatus::Completed);
    assert_eq!(record.result_ref.as_deref(), Some("run-001"));

    let skipped = store
        .begin_idempotency("cron.run", "job-002:unix:1", "sha256:def")
        .unwrap();
    assert!(matches!(skipped, IdempotencyOutcome::Inserted(_)));
    let skipped = store
        .skip_idempotency("cron.run", "job-002:unix:1", Some("no-op"))
        .unwrap();
    assert_eq!(skipped.status, IdempotencyStatus::Skipped);
}

#[test]
fn future_schema_version_is_rejected() {
    let root = temp_root("future-version");
    write_raw_database(
        &root,
        r#"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations(version, applied_at)
            VALUES (999, 'unix:1');
            PRAGMA user_version = 999;
            "#,
    );

    let err = AglStore::open_at(&root).unwrap_err();

    assert!(matches!(
        err,
        StoreError::UnsupportedSchemaVersion {
            found: 999,
            supported: CURRENT_SCHEMA_VERSION
        }
    ));
}

#[test]
fn idempotency_replays_same_fingerprint() {
    let (_root, store) = open_temp_store("idempotency-replay");

    let first = store
        .begin_idempotency("matrix", "event-001", "sha256:abc")
        .unwrap();
    let second = store
        .begin_idempotency("matrix", "event-001", "sha256:abc")
        .unwrap();

    assert!(matches!(first, IdempotencyOutcome::Inserted(_)));
    assert!(matches!(second, IdempotencyOutcome::Replayed(_)));
}

#[test]
fn idempotency_rejects_different_fingerprint() {
    let (_root, store) = open_temp_store("idempotency-conflict");
    store
        .begin_idempotency("matrix", "event-001", "sha256:abc")
        .unwrap();

    let err = store
        .begin_idempotency("matrix", "event-001", "sha256:def")
        .unwrap_err();

    assert!(matches!(err, StoreError::IdempotencyConflict { .. }));
}

#[test]
fn complete_idempotency_records_result_ref() {
    let (_root, store) = open_temp_store("idempotency-complete");
    store
        .begin_idempotency("matrix", "event-001", "sha256:abc")
        .unwrap();

    let record = store
        .complete_idempotency("matrix", "event-001", Some("session/turn-001"))
        .unwrap();

    assert_eq!(record.status, IdempotencyStatus::Completed);
    assert_eq!(record.result_ref.as_deref(), Some("session/turn-001"));
}

#[test]
fn fail_idempotency_records_failed_status() {
    let (_root, store) = open_temp_store("idempotency-failed");
    store
        .begin_idempotency("cron.run", "job-001:unix:1", "sha256:abc")
        .unwrap();

    let record = store
        .fail_idempotency("cron.run", "job-001:unix:1", Some("error-001"))
        .unwrap();
    let replay = store
        .begin_idempotency("cron.run", "job-001:unix:1", "sha256:abc")
        .unwrap();

    assert_eq!(record.status, IdempotencyStatus::Failed);
    assert_eq!(record.result_ref.as_deref(), Some("error-001"));
    assert!(matches!(replay, IdempotencyOutcome::Replayed(_)));
}

#[test]
fn skip_idempotency_records_skipped_status() {
    let (_root, store) = open_temp_store("idempotency-skipped");
    store
        .begin_idempotency("cron.run", "job-001:unix:1", "sha256:abc")
        .unwrap();

    let record = store
        .skip_idempotency("cron.run", "job-001:unix:1", Some("not-due"))
        .unwrap();

    assert_eq!(record.status, IdempotencyStatus::Skipped);
    assert_eq!(record.result_ref.as_deref(), Some("not-due"));
}

#[test]
fn database_file_rejects_path_traversal() {
    let root = temp_root("bad-path");

    let err = database_path(&root, "../agentlibre.sqlite3").unwrap_err();

    assert!(matches!(err, StoreError::InvalidPath { .. }));
}

fn export_domain(store: &AglStore, domain: StoreDomain, include_deleted: bool) -> (usize, String) {
    let mut output = Vec::new();
    let count = store
        .export_domain_jsonl(
            &StoreExportOptions {
                domain,
                include_deleted,
            },
            &mut output,
        )
        .unwrap();
    (count, String::from_utf8(output).unwrap())
}

fn execute_fixture(store: &AglStore, sql: &str) {
    store.connection().execute(sql, []).unwrap();
}

fn write_raw_database(root: &std::path::Path, sql: &str) {
    std::fs::create_dir_all(root).unwrap();
    let db_path = database_path(root, DEFAULT_DATABASE_FILE).unwrap();
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(sql).unwrap();
}

fn open_temp_store(label: &str) -> (TempRoot, AglStore) {
    let root = temp_root(label);
    let store = AglStore::open_at(&root).unwrap();
    (root, store)
}

fn temp_root(label: &str) -> TempRoot {
    let root = std::env::temp_dir().join(format!("agl-store-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    TempRoot { path: root }
}

fn run_draft(session_id: Option<SessionId>) -> DurableRunDraft {
    let turn_id = session_id.as_ref().map(|_| TurnId::generate());
    let kind = if turn_id.is_some() {
        RunKind::Turn
    } else {
        RunKind::Cron
    };
    DurableRunDraft {
        run_id: RunId::generate(),
        session_id,
        turn_id,
        kind,
        priority: 0,
        input: json!({"prompt": "test"}),
        checkpoint: None,
        effective_policy_hash: None,
        budget: RunBudget::default(),
        not_before_ms: None,
    }
}

fn running_delegation_step(
    store: &AglStore,
    admitted_at_ms: i64,
) -> (DurableRunDraft, RunLease, RunStepDraft, StepLease) {
    let parent = run_draft(Some(SessionId::generate()));
    store.admit_run_at(&parent, admitted_at_ms).unwrap();
    let lease = store
        .claim_next_run("parent-owner", admitted_at_ms + 1, 1_000)
        .unwrap()
        .unwrap();
    let step = RunStepDraft {
        step_id: StepId::generate(),
        turn_id: parent.turn_id.clone(),
        effect_sequence: 1,
        effect_kind: "capability_dispatch".to_string(),
        delivery_class: EffectDeliveryClass::Idempotent,
        request: json!({"subagent_id": "reviewer", "task": "review"}),
    };
    store
        .publish_run_step(
            &lease,
            &json!({"phase": "delegate"}),
            &step,
            &[],
            admitted_at_ms + 2,
        )
        .unwrap();
    let step_lease = store
        .claim_run_step(
            &lease,
            &step.step_id,
            admitted_at_ms + 1_000,
            admitted_at_ms + 3,
        )
        .unwrap();
    (parent, lease, step, step_lease)
}

fn child_run_draft(parent_run_id: &RunId, step_id: &StepId) -> ChildRunDraft {
    ChildRunDraft {
        run_id: RunId::generate(),
        parent_run_id: parent_run_id.clone(),
        spawned_by_step_id: step_id.clone(),
        subagent_id: "reviewer".to_string(),
        input: json!({"run_kind": "subagent", "task": "review"}),
        priority: 0,
        effective_policy_hash: digest('a'),
        budget: RunBudget {
            wall_time_ms: 500,
            model_input_tokens: 1_000,
            model_output_tokens: 100,
            model_attempts: 4,
            capability_calls: 8,
        },
        child_spec_digest: digest('b'),
        model_profile_digest: digest('c'),
        tree_budget: DelegationTreeBudget {
            max_depth: 3,
            max_children_per_run: 4,
            max_descendants: 8,
            max_total_output_tokens: 1_000,
            timeout_ms: 10_000,
        },
    }
}

fn digest(character: char) -> String {
    format!("sha256:{}", character.to_string().repeat(64))
}

fn safe_event(
    draft: &DurableRunDraft,
    sequence: u64,
    payload: SafeRuntimeEvent,
) -> SafeRuntimeEventEnvelope {
    let mut scope = EventScope::builder(draft.run_id.clone());
    if let Some(session_id) = &draft.session_id {
        scope = scope.session_id(session_id.clone());
    }
    if let Some(turn_id) = &draft.turn_id {
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

struct TempRoot {
    path: PathBuf,
}

impl AsRef<std::path::Path> for TempRoot {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

impl std::ops::Deref for TempRoot {
    type Target = std::path::Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
