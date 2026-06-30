#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreMigration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

pub const CURRENT_SCHEMA_VERSION: u32 = 9;

pub const STORE_MIGRATIONS: &[StoreMigration] = &[
    StoreMigration {
        version: 1,
        name: "001_foundation",
        sql: r#"
            CREATE TABLE IF NOT EXISTS idempotency_keys (
                namespace TEXT NOT NULL,
                key TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('in_progress', 'completed', 'failed')),
                result_ref TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (namespace, key)
            );
        "#,
    },
    StoreMigration {
        version: 2,
        name: "002_idempotency_skipped_status",
        sql: r#"
            ALTER TABLE idempotency_keys RENAME TO idempotency_keys_v1;
            CREATE TABLE idempotency_keys (
                namespace TEXT NOT NULL,
                key TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('in_progress', 'completed', 'failed', 'skipped')),
                result_ref TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (namespace, key)
            );
            INSERT INTO idempotency_keys
                (namespace, key, fingerprint, status, result_ref, created_at, updated_at)
            SELECT namespace, key, fingerprint, status, result_ref, created_at, updated_at
            FROM idempotency_keys_v1;
            DROP TABLE idempotency_keys_v1;
        "#,
    },
    StoreMigration {
        version: 3,
        name: "003_memory_entries",
        sql: r#"
            CREATE TABLE memory_entries (
                id TEXT PRIMARY KEY,
                scope_kind TEXT NOT NULL,
                scope_key TEXT NOT NULL,
                kind TEXT NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                source_ref TEXT,
                confidence INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted_at TEXT
            );
            CREATE INDEX memory_entries_scope_idx
                ON memory_entries(scope_kind, scope_key, deleted_at);
            CREATE VIRTUAL TABLE memory_entries_fts
                USING fts5(id UNINDEXED, title, body);
        "#,
    },
    StoreMigration {
        version: 4,
        name: "004_notes",
        sql: r#"
            CREATE TABLE notes (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted_at TEXT
            );
            CREATE INDEX notes_deleted_idx
                ON notes(deleted_at, updated_at);
            CREATE TABLE note_links (
                id TEXT PRIMARY KEY,
                note_id TEXT NOT NULL,
                target_ref TEXT NOT NULL,
                label TEXT,
                created_at TEXT NOT NULL,
                FOREIGN KEY(note_id) REFERENCES notes(id)
            );
            CREATE INDEX note_links_note_idx
                ON note_links(note_id, created_at);
        "#,
    },
    StoreMigration {
        version: 5,
        name: "005_cron_jobs",
        sql: r#"
            CREATE TABLE cron_jobs (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                enabled INTEGER NOT NULL,
                target_kind TEXT NOT NULL,
                target_ref TEXT NOT NULL,
                schedule_expr TEXT NOT NULL,
                timezone TEXT NOT NULL,
                notify_ref TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted_at TEXT
            );
            CREATE INDEX cron_jobs_enabled_idx
                ON cron_jobs(enabled, deleted_at, updated_at);
            CREATE TABLE cron_runs (
                id TEXT PRIMARY KEY,
                job_id TEXT NOT NULL,
                scheduled_for TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT,
                status TEXT NOT NULL,
                result_ref TEXT,
                error TEXT,
                FOREIGN KEY(job_id) REFERENCES cron_jobs(id)
            );
            CREATE INDEX cron_runs_job_idx
                ON cron_runs(job_id, scheduled_for);
        "#,
    },
    StoreMigration {
        version: 6,
        name: "006_memory_suggestions",
        sql: r#"
            CREATE TABLE memory_suggestions (
                id TEXT PRIMARY KEY,
                scope_kind TEXT NOT NULL,
                scope_key TEXT NOT NULL,
                kind TEXT NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                source_ref TEXT NOT NULL,
                confidence INTEGER NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'rejected')),
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                resolved_at TEXT,
                resolution_ref TEXT,
                resolution_note TEXT
            );
            CREATE INDEX memory_suggestions_status_idx
                ON memory_suggestions(status, updated_at);
            CREATE INDEX memory_suggestions_scope_idx
                ON memory_suggestions(scope_kind, scope_key, status);
        "#,
    },
    StoreMigration {
        version: 7,
        name: "007_cron_prompt_input_and_matrix_outbox",
        sql: r#"
            ALTER TABLE cron_jobs ADD COLUMN prompt TEXT;
            ALTER TABLE cron_jobs ADD COLUMN input TEXT;
            UPDATE cron_jobs SET timezone = 'UTC' WHERE timezone = 'local';
            CREATE TABLE matrix_notification_outbox (
                id TEXT PRIMARY KEY,
                notify_ref TEXT NOT NULL,
                source_kind TEXT NOT NULL,
                source_id TEXT NOT NULL,
                dedupe_key TEXT NOT NULL UNIQUE,
                body TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('queued', 'sent', 'failed')),
                error TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                delivered_at TEXT
            );
            CREATE INDEX matrix_notification_outbox_status_idx
                ON matrix_notification_outbox(status, updated_at);
        "#,
    },
    StoreMigration {
        version: 8,
        name: "008_permission_requests_and_grants",
        sql: r#"
            CREATE TABLE permission_requests (
                id TEXT PRIMARY KEY,
                requested_tools_json TEXT NOT NULL,
                max_operation_kind TEXT NOT NULL,
                state_effects_json TEXT NOT NULL,
                scope_json TEXT NOT NULL,
                duration TEXT NOT NULL,
                reason TEXT NOT NULL,
                requester_ref TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('pending', 'granted', 'denied', 'revoked')),
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                resolved_at TEXT,
                resolution_ref TEXT,
                resolution_note TEXT
            );
            CREATE INDEX permission_requests_status_idx
                ON permission_requests(status, updated_at);

            CREATE TABLE permission_grants (
                id TEXT PRIMARY KEY,
                request_id TEXT,
                tool_id TEXT NOT NULL,
                max_operation_kind TEXT NOT NULL,
                state_effects_json TEXT NOT NULL,
                scope_json TEXT NOT NULL,
                duration TEXT NOT NULL,
                granted_by_ref TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('active', 'revoked', 'expired')),
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                revoked_at TEXT,
                revoke_ref TEXT,
                FOREIGN KEY(request_id) REFERENCES permission_requests(id)
            );
            CREATE INDEX permission_grants_status_idx
                ON permission_grants(status, updated_at);
            CREATE INDEX permission_grants_tool_idx
                ON permission_grants(tool_id, status);
        "#,
    },
    StoreMigration {
        version: 9,
        name: "009_permission_grant_admission_lifecycle",
        sql: r#"
            ALTER TABLE permission_grants ADD COLUMN admitted_at TEXT;
            ALTER TABLE permission_grants ADD COLUMN last_admitted_run_id TEXT;
            ALTER TABLE permission_grants ADD COLUMN consumed_at TEXT;
        "#,
    },
];
