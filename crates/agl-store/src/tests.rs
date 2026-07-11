use super::*;
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
