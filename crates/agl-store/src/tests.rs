use super::*;
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

    let _ = std::fs::remove_dir_all(root);
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

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn transaction_commits_and_rolls_back() {
    let root = temp_root("transaction");
    let store = AglStore::open_at(&root).unwrap();
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

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn migration_history_gaps_fail_clearly() {
    let root = temp_root("migration-gap");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = database_path(&root, DEFAULT_DATABASE_FILE).unwrap();
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        r#"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations(version, applied_at)
            VALUES (1, 'unix:1'), (3, 'unix:3');
            PRAGMA user_version = 3;
            "#,
    )
    .unwrap();
    drop(conn);

    let err = AglStore::open_at(&root).unwrap_err();
    assert!(matches!(err, StoreError::MigrationGap { missing: 2 }));

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn store_status_counts_domain_rows() {
    let root = temp_root("domain-status");
    let store = AglStore::open_at(&root).unwrap();
    store
            .connection()
            .execute(
                "INSERT INTO memory_entries
                 (id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at)
                 VALUES ('mem_active', 'user', 'default', 'fact', 'Active', 'Body', NULL, 100, 'unix:1', 'unix:1', NULL),
                        ('mem_deleted', 'user', 'default', 'fact', 'Deleted', 'Body', NULL, 100, 'unix:1', 'unix:2', 'unix:3')",
                [],
            )
            .unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO notes
                 (id, title, body, created_at, updated_at, deleted_at)
                 VALUES ('note_active', 'Active', 'Body', 'unix:1', 'unix:1', NULL),
                        ('note_deleted', 'Deleted', 'Body', 'unix:1', 'unix:2', 'unix:3')",
            [],
        )
        .unwrap();
    store
            .connection()
            .execute(
                "INSERT INTO cron_jobs
                 (id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, created_at, updated_at, deleted_at)
                 VALUES ('cron_active', 'Active', 1, 'builtin', 'store-status', 'hourly', 'UTC', NULL, 'unix:1', 'unix:1', NULL),
                        ('cron_deleted', 'Deleted', 0, 'builtin', 'store-status', 'hourly', 'UTC', NULL, 'unix:1', 'unix:2', 'unix:3')",
                [],
            )
            .unwrap();

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

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn status_reports_in_progress_idempotency_without_recovering_it() {
    let root = temp_root("stale-idempotency");
    let store = AglStore::open_at(&root).unwrap();
    store
            .connection()
            .execute(
                "INSERT INTO idempotency_keys
                 (namespace, key, fingerprint, status, result_ref, created_at, updated_at)
                 VALUES ('cron.run', 'job-1:unix:60', 'sha256:abc', 'in_progress', NULL, 'unix:1', 'unix:2')",
                [],
            )
            .unwrap();

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

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn export_memory_jsonl_respects_tombstones() {
    let root = temp_root("export-memory");
    let store = AglStore::open_at(&root).unwrap();
    store
            .connection()
            .execute(
                "INSERT INTO memory_entries
                 (id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at)
                 VALUES ('mem_active', 'user', 'default', 'fact', 'Active', 'Body', NULL, 100, 'unix:1', 'unix:1', NULL),
                        ('mem_deleted', 'user', 'default', 'fact', 'Deleted', 'Body', NULL, 100, 'unix:1', 'unix:2', 'unix:3')",
                [],
            )
            .unwrap();
    let mut active = Vec::new();
    let mut all = Vec::new();

    let active_count = store
        .export_domain_jsonl(
            &StoreExportOptions {
                domain: StoreDomain::Memory,
                include_deleted: false,
            },
            &mut active,
        )
        .unwrap();
    let all_count = store
        .export_domain_jsonl(
            &StoreExportOptions {
                domain: StoreDomain::Memory,
                include_deleted: true,
            },
            &mut all,
        )
        .unwrap();

    let active = String::from_utf8(active).unwrap();
    let all = String::from_utf8(all).unwrap();
    assert_eq!(active_count, 1);
    assert!(active.contains("\"id\":\"mem_active\""));
    assert!(!active.contains("mem_deleted"));
    assert_eq!(all_count, 2);
    assert!(all.contains("\"id\":\"mem_deleted\""));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn export_memory_jsonl_includes_pending_suggestions() {
    let root = temp_root("export-memory-suggestions");
    let store = AglStore::open_at(&root).unwrap();
    store
            .connection()
            .execute(
                "INSERT INTO memory_suggestions
                 (id, scope_kind, scope_key, kind, title, body, source_ref, confidence, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note)
                 VALUES ('suggest_pending', 'user', 'default', 'decision', 'Pending', 'Body', 'chat:1', 95, 'pending', 'unix:1', 'unix:1', NULL, NULL, NULL),
                        ('suggest_rejected', 'user', 'default', 'fact', 'Rejected', 'Body', 'chat:2', 90, 'rejected', 'unix:1', 'unix:2', 'unix:2', NULL, 'not durable')",
                [],
            )
            .unwrap();
    let mut active = Vec::new();
    let mut all = Vec::new();

    let active_count = store
        .export_domain_jsonl(
            &StoreExportOptions {
                domain: StoreDomain::Memory,
                include_deleted: false,
            },
            &mut active,
        )
        .unwrap();
    let all_count = store
        .export_domain_jsonl(
            &StoreExportOptions {
                domain: StoreDomain::Memory,
                include_deleted: true,
            },
            &mut all,
        )
        .unwrap();

    let active = String::from_utf8(active).unwrap();
    let all = String::from_utf8(all).unwrap();
    assert_eq!(active_count, 1);
    assert!(active.contains("\"record_type\":\"memory_suggestion\""));
    assert!(active.contains("\"id\":\"suggest_pending\""));
    assert!(!active.contains("suggest_rejected"));
    assert_eq!(all_count, 2);
    assert!(all.contains("\"id\":\"suggest_rejected\""));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn export_notes_and_cron_include_related_rows() {
    let root = temp_root("export-related");
    let store = AglStore::open_at(&root).unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO notes
                 (id, title, body, created_at, updated_at, deleted_at)
                 VALUES ('note_active', 'Active', 'Body', 'unix:1', 'unix:1', NULL)",
            [],
        )
        .unwrap();
    store
        .connection()
        .execute(
            "INSERT INTO note_links
                 (id, note_id, target_ref, label, created_at)
                 VALUES ('link_1', 'note_active', 'memory:mem_1', 'remembered', 'unix:2')",
            [],
        )
        .unwrap();
    store
            .connection()
            .execute(
                "INSERT INTO cron_jobs
                 (id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, created_at, updated_at, deleted_at)
                 VALUES ('cron_active', 'Active', 1, 'builtin', 'store-status', 'hourly', 'UTC', NULL, 'unix:1', 'unix:1', NULL)",
                [],
            )
            .unwrap();
    store
            .connection()
            .execute(
                "INSERT INTO cron_runs
                 (id, job_id, scheduled_for, started_at, finished_at, status, result_ref, error)
                 VALUES ('run_1', 'cron_active', 'unix:2', 'unix:2', 'unix:2', 'succeeded', 'builtin:store-status', NULL)",
                [],
            )
            .unwrap();
    let mut notes = Vec::new();
    let mut cron = Vec::new();

    let notes_count = store
        .export_domain_jsonl(
            &StoreExportOptions {
                domain: StoreDomain::Notes,
                include_deleted: false,
            },
            &mut notes,
        )
        .unwrap();
    let cron_count = store
        .export_domain_jsonl(
            &StoreExportOptions {
                domain: StoreDomain::Cron,
                include_deleted: false,
            },
            &mut cron,
        )
        .unwrap();

    let notes = String::from_utf8(notes).unwrap();
    let cron = String::from_utf8(cron).unwrap();
    assert_eq!(notes_count, 2);
    assert!(notes.contains("\"record_type\":\"note\""));
    assert!(notes.contains("\"record_type\":\"note_link\""));
    assert_eq!(cron_count, 2);
    assert!(cron.contains("\"record_type\":\"cron_job\""));
    assert!(cron.contains("\"record_type\":\"cron_run\""));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn matrix_notification_outbox_enqueues_once_and_exports_with_cron() {
    let root = temp_root("matrix-outbox");
    let store = AglStore::open_at(&root).unwrap();

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

    let mut cron = Vec::new();
    let count = store
        .export_domain_jsonl(
            &StoreExportOptions {
                domain: StoreDomain::Cron,
                include_deleted: false,
            },
            &mut cron,
        )
        .unwrap();
    let cron = String::from_utf8(cron).unwrap();
    assert_eq!(count, 1);
    assert!(cron.contains("\"record_type\":\"matrix_notification_outbox\""));
    assert!(cron.contains("\"status\":\"sent\""));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn matrix_notification_outbox_page_reports_truncation_only_for_extra_rows() {
    let root = temp_root("matrix-outbox-page");
    let store = AglStore::open_at(&root).unwrap();

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

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn permission_requests_grants_and_revokes_are_persisted() {
    let root = temp_root("permission-requests");
    let store = AglStore::open_at(&root).unwrap();

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

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn permission_export_reports_pending_and_historical_records() {
    let root = temp_root("permission-export");
    let store = AglStore::open_at(&root).unwrap();
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

    let mut active = Vec::new();
    let active_count = store
        .export_domain_jsonl(
            &StoreExportOptions {
                domain: StoreDomain::Permissions,
                include_deleted: false,
            },
            &mut active,
        )
        .unwrap();
    let mut all = Vec::new();
    let all_count = store
        .export_domain_jsonl(
            &StoreExportOptions {
                domain: StoreDomain::Permissions,
                include_deleted: true,
            },
            &mut all,
        )
        .unwrap();

    let active = String::from_utf8(active).unwrap();
    let all = String::from_utf8(all).unwrap();
    assert_eq!(active_count, 0);
    assert_eq!(all_count, 2);
    assert!(active.is_empty());
    assert!(all.contains("\"record_type\":\"permission_request\""));
    assert!(all.contains("\"record_type\":\"permission_grant\""));
    assert!(all.contains("\"status\":\"revoked\""));

    std::fs::remove_dir_all(root).unwrap();
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

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn schema_v1_database_migrates_to_current() {
    let root = temp_root("migrate-v1");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = database_path(&root, DEFAULT_DATABASE_FILE).unwrap();
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
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
        )
        .unwrap();
    drop(conn);

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

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn future_schema_version_is_rejected() {
    let root = temp_root("future-version");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = database_path(&root, DEFAULT_DATABASE_FILE).unwrap();
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        r#"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations(version, applied_at)
            VALUES (999, 'unix:1');
            PRAGMA user_version = 999;
            "#,
    )
    .unwrap();
    drop(conn);

    let err = AglStore::open_at(&root).unwrap_err();

    assert!(matches!(
        err,
        StoreError::UnsupportedSchemaVersion {
            found: 999,
            supported: CURRENT_SCHEMA_VERSION
        }
    ));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn idempotency_replays_same_fingerprint() {
    let root = temp_root("idempotency-replay");
    let store = AglStore::open_at(&root).unwrap();

    let first = store
        .begin_idempotency("matrix", "event-001", "sha256:abc")
        .unwrap();
    let second = store
        .begin_idempotency("matrix", "event-001", "sha256:abc")
        .unwrap();

    assert!(matches!(first, IdempotencyOutcome::Inserted(_)));
    assert!(matches!(second, IdempotencyOutcome::Replayed(_)));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn idempotency_rejects_different_fingerprint() {
    let root = temp_root("idempotency-conflict");
    let store = AglStore::open_at(&root).unwrap();
    store
        .begin_idempotency("matrix", "event-001", "sha256:abc")
        .unwrap();

    let err = store
        .begin_idempotency("matrix", "event-001", "sha256:def")
        .unwrap_err();

    assert!(matches!(err, StoreError::IdempotencyConflict { .. }));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn complete_idempotency_records_result_ref() {
    let root = temp_root("idempotency-complete");
    let store = AglStore::open_at(&root).unwrap();
    store
        .begin_idempotency("matrix", "event-001", "sha256:abc")
        .unwrap();

    let record = store
        .complete_idempotency("matrix", "event-001", Some("session/turn-001"))
        .unwrap();

    assert_eq!(record.status, IdempotencyStatus::Completed);
    assert_eq!(record.result_ref.as_deref(), Some("session/turn-001"));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn fail_idempotency_records_failed_status() {
    let root = temp_root("idempotency-failed");
    let store = AglStore::open_at(&root).unwrap();
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

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn skip_idempotency_records_skipped_status() {
    let root = temp_root("idempotency-skipped");
    let store = AglStore::open_at(&root).unwrap();
    store
        .begin_idempotency("cron.run", "job-001:unix:1", "sha256:abc")
        .unwrap();

    let record = store
        .skip_idempotency("cron.run", "job-001:unix:1", Some("not-due"))
        .unwrap();

    assert_eq!(record.status, IdempotencyStatus::Skipped);
    assert_eq!(record.result_ref.as_deref(), Some("not-due"));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn database_file_rejects_path_traversal() {
    let root = temp_root("bad-path");

    let err = database_path(&root, "../agentlibre.sqlite3").unwrap_err();

    assert!(matches!(err, StoreError::InvalidPath { .. }));

    let _ = std::fs::remove_dir_all(root);
}

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("agl-store-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}
