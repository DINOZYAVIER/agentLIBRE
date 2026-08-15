use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};

use agl_ids::SessionId;
use agl_kernel::{OperationKind, RunRepository, SensitiveInput, ToolId};
use agl_matrix::{
    MatrixDeliveryResult, MatrixOperationId, MatrixOutboxDraft, MatrixOutboxId,
    MatrixOutboxRepository, MatrixOutboxState, stable_matrix_transaction_id,
};
use agl_permission::{
    PermissionDuration, PermissionGrantDraft, PermissionGrantState, PermissionOperationId,
    PermissionRepository, PermissionRequestDraft, PermissionRequestState,
};
use agl_store::{CURRENT_SCHEMA_VERSION, DEFAULT_DATABASE_FILE, STORE_MIGRATIONS, StoreHandle};
use rusqlite::{Connection, params};

#[test]
fn matrix_adapter_fences_lease_and_preserves_transaction_identity() {
    let root = temp_root("matrix");
    let store = StoreHandle::open_at(&root).unwrap();
    let draft = MatrixOutboxDraft::new(
        "matrix-room:!room:example.org",
        "cron",
        "run_1",
        "cron:run_1:room",
        "done",
    )
    .unwrap();
    let queued = store.enqueue(draft.clone()).unwrap();
    assert_eq!(store.enqueue(draft).unwrap(), queued);

    let claimed = store
        .claim(
            &queued.id,
            MatrixOperationId::new("claim-1").unwrap(),
            "worker-1",
            1,
            100,
        )
        .unwrap();
    assert!(matches!(
        claimed.state,
        MatrixOutboxState::Delivering { .. }
    ));
    assert_eq!(claimed.transaction_id, queued.transaction_id);

    let retried = store
        .complete(
            &queued.id,
            MatrixOperationId::new("complete-1").unwrap(),
            "worker-1",
            MatrixDeliveryResult::Retryable {
                not_before_ms: 200,
                error: "rate limited".to_owned(),
            },
        )
        .unwrap();
    assert_eq!(retried.transaction_id, queued.transaction_id);
    assert!(matches!(
        retried.state,
        MatrixOutboxState::Queued { not_before_ms: 200 }
    ));
}

#[test]
fn matrix_adapter_concurrent_claim_has_one_lease_owner_and_fences_the_loser() {
    let root = temp_root("matrix-concurrent-claim");
    let store = Arc::new(StoreHandle::open_at(&root).unwrap());
    let queued = store
        .enqueue(
            MatrixOutboxDraft::new(
                "matrix-room:!room:example.org",
                "cron",
                "run-concurrent",
                "cron:run-concurrent:room",
                "done",
            )
            .unwrap(),
        )
        .unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for owner in ["worker-a", "worker-b"] {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        let id = queued.id.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            let result = store.claim(
                &id,
                MatrixOperationId::new(format!("claim-{owner}")).unwrap(),
                owner,
                1,
                100,
            );
            (owner, result)
        }));
    }
    barrier.wait();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_err()).count(),
        1
    );

    let winner = results
        .iter()
        .find_map(|(owner, result)| result.as_ref().ok().map(|_| *owner))
        .unwrap();
    let stored = store.get(&queued.id).unwrap().unwrap();
    assert!(matches!(
        &stored.state,
        MatrixOutboxState::Delivering {
            lease_owner,
            lease_expires_at_ms: 100,
            attempt: 1,
        } if lease_owner == winner
    ));
    assert_eq!(stored.revision.get(), 2);

    let loser = if winner == "worker-a" {
        "worker-b"
    } else {
        "worker-a"
    };
    assert!(
        store
            .complete(
                &queued.id,
                MatrixOperationId::new("complete-loser").unwrap(),
                loser,
                MatrixDeliveryResult::Delivered,
            )
            .is_err()
    );
    assert_eq!(store.get(&queued.id).unwrap().unwrap(), stored);
}

#[test]
fn permission_adapter_persists_strict_terminal_transition() {
    let root = temp_root("permission");
    let store = StoreHandle::open_at(&root).unwrap();
    let grant = store
        .create_grant(PermissionGrantDraft {
            request_id: None,
            tool_id: ToolId::new("core.process:start").unwrap(),
            max_operation_kind: OperationKind::Execute,
            state_effects: BTreeSet::new(),
            sensitive_inputs: BTreeSet::<SensitiveInput>::new(),
            scope: serde_json::json!({"session_id": SessionId::generate().as_str()}),
            duration: PermissionDuration::OneTurn,
            granted_by_ref: "cli:operator".to_owned(),
        })
        .unwrap();
    let run = agl_kernel::DurableRunDraft {
        run_id: agl_ids::RunId::generate(),
        session_id: None,
        turn_id: None,
        kind: agl_kernel::RunKind::Cron,
        priority: 0,
        concurrency_key: None,
        input: serde_json::json!({"builtin": "test"}),
        checkpoint: None,
        effective_policy_hash: None,
        budget: agl_kernel::RunBudget::default(),
        execution_context: execution_context(),
        not_before_ms: None,
    };
    store.admit_run_at(&run, 1).unwrap();

    let consumed = store
        .admit_grant(
            &grant.id,
            &run.run_id,
            PermissionOperationId::new("admit-1").unwrap(),
        )
        .unwrap();
    assert_eq!(
        consumed.state,
        PermissionGrantState::Consumed {
            run_id: run.run_id.clone()
        }
    );
    assert!(
        store
            .revoke_grant(
                &grant.id,
                PermissionOperationId::new("revoke-after-consume").unwrap(),
                None,
            )
            .is_err()
    );
}

#[test]
fn permission_request_and_all_grants_commit_atomically() {
    let root = temp_root("permission-atomic-grants");
    let request = {
        let store = StoreHandle::open_at(&root).unwrap();
        store
            .create_request(PermissionRequestDraft {
                requested_tools: vec![
                    ToolId::new("core.process:start").unwrap(),
                    ToolId::new("core.fs:read").unwrap(),
                ],
                max_operation_kind: OperationKind::Execute,
                state_effects: BTreeSet::new(),
                sensitive_inputs: BTreeSet::new(),
                scope: serde_json::json!({"session_id": SessionId::generate()}),
                duration: PermissionDuration::Session,
                reason: "atomic grant fixture".to_owned(),
                requester_ref: "test:operator".to_owned(),
            })
            .unwrap()
    };
    let database_path = root.join(DEFAULT_DATABASE_FILE);
    let connection = Connection::open(&database_path).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER agl175_fail_second_grant
             BEFORE INSERT ON permission_grants
             WHEN NEW.tool_id = 'core.fs:read'
             BEGIN
                 SELECT RAISE(ABORT, 'injected grant failure');
             END;",
        )
        .unwrap();
    drop(connection);

    let operation_id = PermissionOperationId::new("grant-request-atomic").unwrap();
    let store = StoreHandle::open_current_at(&root).unwrap();
    assert!(
        store
            .grant_request(
                &request.id,
                "test:operator",
                operation_id.clone(),
                Some("test-resolution"),
            )
            .is_err()
    );
    let unchanged = store.request(&request.id).unwrap().unwrap();
    assert_eq!(unchanged.state, PermissionRequestState::Pending);
    assert_eq!(unchanged.revision.get(), 1);
    assert!(store.active_grants().unwrap().is_empty());
    drop(store);

    let connection = Connection::open(&database_path).unwrap();
    connection
        .execute_batch("DROP TRIGGER agl175_fail_second_grant;")
        .unwrap();
    drop(connection);
    let store = StoreHandle::open_current_at(&root).unwrap();
    let grants = store
        .grant_request(
            &request.id,
            "test:operator",
            operation_id.clone(),
            Some("test-resolution"),
        )
        .unwrap();
    assert_eq!(grants.len(), 2);
    assert_eq!(
        store.request(&request.id).unwrap().unwrap().state,
        PermissionRequestState::Granted
    );
    assert_eq!(
        store
            .grant_request(
                &request.id,
                "test:operator",
                operation_id,
                Some("test-resolution"),
            )
            .unwrap(),
        grants
    );
}

#[test]
fn migration_v19_to_v20_converts_permission_and_matrix_records() {
    let root = temp_root("migration-v19-compatible");
    let database_path = create_v19_store(&root);
    let connection = Connection::open(&database_path).unwrap();
    connection
        .execute(
            "INSERT INTO permission_requests (
                 id, requested_tools_json, max_operation_kind, state_effects_json,
                 sensitive_inputs_json, scope_json, duration, reason, requester_ref,
                 status, created_at, updated_at
             ) VALUES (?1, ?2, 'execute', '[]', '[]', '{}', 'session', ?3, ?4,
                       'pending', 'unix:1', 'unix:1')",
            params![
                "permission_request_legacy",
                "[\"core.process:start\"]",
                "legacy request",
                "test:operator"
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO permission_grants (
                 id, request_id, tool_id, max_operation_kind, state_effects_json,
                 sensitive_inputs_json, scope_json, duration, granted_by_ref, status,
                 created_at, updated_at
             ) VALUES (?1, NULL, 'core.process:start', 'execute', '[]', '[]', '{}',
                       'session', ?2, 'active', 'unix:1', 'unix:1')",
            params!["permission_grant_legacy", "test:operator"],
        )
        .unwrap();
    for (id, status, error, delivered_at) in [
        ("matrix_outbox_legacy_queued", "queued", None, None),
        ("matrix_outbox_legacy_sent", "sent", None, Some("unix:2")),
        (
            "matrix_outbox_legacy_failed",
            "failed",
            Some("permanent legacy failure"),
            None,
        ),
    ] {
        connection
            .execute(
                "INSERT INTO matrix_notification_outbox (
                     id, notify_ref, source_kind, source_id, dedupe_key, body, status,
                     error, created_at, updated_at, delivered_at
                 ) VALUES (?1, 'matrix-room:!room:example.org', 'cron', ?2, ?3,
                           'legacy body', ?4, ?5, 'unix:1', 'unix:1', ?6)",
                params![id, id, format!("dedupe:{id}"), status, error, delivered_at],
            )
            .unwrap();
    }
    drop(connection);

    let store = StoreHandle::open_at(&root).unwrap();
    assert_eq!(
        store.health().unwrap().migration_version,
        CURRENT_SCHEMA_VERSION
    );
    let request = store.request("permission_request_legacy").unwrap().unwrap();
    assert_eq!(request.state, PermissionRequestState::Pending);
    assert_eq!(request.revision.get(), 1);
    let grant = store.grant("permission_grant_legacy").unwrap().unwrap();
    assert_eq!(grant.state, PermissionGrantState::Active);
    assert_eq!(grant.revision.get(), 1);

    for (id, expected_state) in [
        (
            "matrix_outbox_legacy_queued",
            MatrixOutboxState::Queued { not_before_ms: 0 },
        ),
        ("matrix_outbox_legacy_sent", MatrixOutboxState::Sent),
        (
            "matrix_outbox_legacy_failed",
            MatrixOutboxState::Failed {
                error: "permanent legacy failure".to_owned(),
            },
        ),
    ] {
        let id = MatrixOutboxId::new(id).unwrap();
        let record = store.get(&id).unwrap().unwrap();
        assert_eq!(record.state, expected_state);
        assert_eq!(record.revision.get(), 1);
        assert_eq!(record.transaction_id, stable_matrix_transaction_id(&id));
        assert_eq!(
            record.payload_fingerprint,
            record.draft.payload_fingerprint()
        );
    }
}

#[test]
fn migration_v20_rejects_invalid_permission_lifetime_without_partial_schema() {
    let root = temp_root("migration-v19-invalid-permission");
    let database_path = create_v19_store(&root);
    let connection = Connection::open(&database_path).unwrap();
    connection
        .execute(
            "INSERT INTO permission_grants (
                 id, request_id, tool_id, max_operation_kind, state_effects_json,
                 sensitive_inputs_json, scope_json, duration, granted_by_ref, status,
                 created_at, updated_at
             ) VALUES ('permission_grant_invalid', NULL, 'core.process:start', 'execute',
                       '[]', '[]', '{}', 'one_turn', 'test:operator', 'expired',
                       'unix:1', 'unix:1')",
            [],
        )
        .unwrap();
    drop(connection);

    assert!(StoreHandle::open_at(&root).is_err());
    assert_v19_schema_unchanged(&database_path);
    let connection = Connection::open(&database_path).unwrap();
    let status = connection
        .query_row(
            "SELECT status FROM permission_grants WHERE id = 'permission_grant_invalid'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    assert_eq!(status, "expired");
}

#[test]
fn migration_v20_sql_failure_rolls_back_preparation_and_all_ddl() {
    let root = temp_root("migration-v20-injected-ddl-failure");
    let database_path = create_v19_store(&root);
    let connection = Connection::open(&database_path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE permission_operations (
                 operation_id TEXT PRIMARY KEY,
                 injected_fixture INTEGER NOT NULL
             );",
        )
        .unwrap();
    drop(connection);

    assert!(StoreHandle::open_at(&root).is_err());
    assert_v19_schema_unchanged(&database_path);
    let connection = Connection::open(&database_path).unwrap();
    assert!(table_exists(&connection, "permission_operations"));
    let columns = connection
        .prepare("PRAGMA table_info(permission_operations)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(columns, ["operation_id", "injected_fixture"]);
}

fn execution_context() -> agl_exec::ExecutionContextSnapshot {
    let workspace = std::env::temp_dir().canonicalize().unwrap();
    agl_exec::ExecutionContextSnapshot {
        workspace_root: workspace.clone(),
        working_directory: workspace,
        private_execution_roots: Vec::new(),
        shell: agl_exec::ShellProfileSnapshot {
            program: PathBuf::from("/bin/sh"),
            command_args: vec!["-c".to_owned()],
            login_command_args: Some(vec!["-l".to_owned(), "-c".to_owned()]),
            environment_names: vec!["PATH".to_owned()],
            executable_digest: "sha256:test-shell".to_owned(),
            config_digest: "sha256:test-config".to_owned(),
        },
        revision: 1,
        profile_metadata: "workspace".to_owned(),
    }
}

fn temp_root(label: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agl175-store-{label}-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_dir_all(&root);
    root
}

fn create_v19_store(root: &std::path::Path) -> PathBuf {
    std::fs::create_dir_all(root).unwrap();
    let database_path = root.join(DEFAULT_DATABASE_FILE);
    let connection = Connection::open(&database_path).unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE schema_migrations (
                 version INTEGER PRIMARY KEY,
                 applied_at TEXT NOT NULL
             );",
        )
        .unwrap();
    for migration in STORE_MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= 19)
    {
        if migration.version == 17 {
            connection
                .execute_batch("PRAGMA foreign_keys = OFF;")
                .unwrap();
        }
        connection.execute_batch(migration.sql).unwrap();
        connection
            .execute(
                "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, 'unix:1')",
                params![migration.version],
            )
            .unwrap();
        connection
            .pragma_update(None, "user_version", migration.version)
            .unwrap();
        if migration.version == 17 {
            connection
                .execute_batch("PRAGMA foreign_keys = ON;")
                .unwrap();
        }
    }
    drop(connection);
    database_path
}

fn assert_v19_schema_unchanged(database_path: &std::path::Path) {
    let connection = Connection::open(database_path).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        19
    );
    assert!(table_exists(&connection, "permission_requests"));
    assert!(table_exists(&connection, "permission_grants"));
    assert!(!table_exists(&connection, "permission_requests_v19"));
    assert!(!table_exists(&connection, "permission_grants_v19"));
    assert!(!table_exists(&connection, "agl175_matrix_migration"));
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 20",
                [],
                |row| row.get::<_, u32>(0),
            )
            .unwrap(),
        0
    );
}

fn table_exists(connection: &Connection, name: &str) -> bool {
    connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
             )",
            [name],
            |row| row.get::<_, bool>(0),
        )
        .unwrap()
}
