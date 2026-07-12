#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreMigration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

pub const CURRENT_SCHEMA_VERSION: u32 = 13;

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
    StoreMigration {
        version: 10,
        name: "010_durable_runs",
        sql: r#"
            CREATE TABLE runs (
                id TEXT PRIMARY KEY,
                session_id TEXT,
                turn_id TEXT,
                kind TEXT NOT NULL CHECK (kind IN ('turn', 'cron')),
                state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'waiting', 'succeeded', 'failed', 'cancelled')),
                priority INTEGER NOT NULL,
                input_json TEXT NOT NULL,
                checkpoint_json TEXT,
                effective_policy_hash TEXT,
                budget_json TEXT NOT NULL,
                usage_json TEXT NOT NULL,
                lease_owner TEXT,
                lease_generation INTEGER NOT NULL DEFAULT 0 CHECK (lease_generation >= 0),
                lease_expires_at_ms INTEGER,
                cancellation_requested_at_ms INTEGER,
                attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
                not_before_ms INTEGER,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                started_at_ms INTEGER,
                finished_at_ms INTEGER,
                terminal_result_json TEXT,
                error_code TEXT,
                error_message TEXT,
                CHECK ((session_id IS NULL) = (turn_id IS NULL)),
                CHECK ((lease_owner IS NULL) = (lease_expires_at_ms IS NULL))
            );
            CREATE INDEX runs_runnable_idx
                ON runs(state, not_before_ms, priority DESC, created_at_ms);
            CREATE INDEX runs_lease_idx
                ON runs(state, lease_expires_at_ms);
            CREATE INDEX runs_session_fifo_idx
                ON runs(session_id, created_at_ms, state);

            CREATE TABLE run_steps (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                turn_id TEXT,
                effect_sequence INTEGER NOT NULL CHECK (effect_sequence > 0),
                effect_kind TEXT NOT NULL,
                delivery_class TEXT NOT NULL CHECK (delivery_class IN ('replay_safe', 'idempotent', 'at_most_once')),
                request_json TEXT NOT NULL,
                result_json TEXT,
                state TEXT NOT NULL CHECK (state IN ('pending', 'running', 'succeeded', 'failed', 'cancelled', 'outcome_unknown')),
                attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
                lease_owner TEXT,
                lease_generation INTEGER NOT NULL DEFAULT 0 CHECK (lease_generation >= 0),
                lease_expires_at_ms INTEGER,
                not_before_ms INTEGER,
                error_code TEXT,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                finished_at_ms INTEGER,
                FOREIGN KEY(run_id) REFERENCES runs(id) ON DELETE CASCADE,
                UNIQUE(run_id, effect_sequence),
                CHECK ((lease_owner IS NULL) = (lease_expires_at_ms IS NULL))
            );
            CREATE INDEX run_steps_runnable_idx
                ON run_steps(state, not_before_ms, created_at_ms);
            CREATE INDEX run_steps_lease_idx
                ON run_steps(state, lease_expires_at_ms);
            CREATE INDEX run_steps_run_idx
                ON run_steps(run_id, effect_sequence);

            CREATE TABLE run_events (
                event_id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                sequence INTEGER NOT NULL CHECK (sequence > 0),
                kind TEXT NOT NULL,
                occurred_at_unix_ms INTEGER NOT NULL,
                envelope_json TEXT NOT NULL,
                FOREIGN KEY(run_id) REFERENCES runs(id) ON DELETE CASCADE,
                UNIQUE(run_id, sequence)
            );
            CREATE INDEX run_events_replay_idx
                ON run_events(run_id, sequence);

            ALTER TABLE idempotency_keys ADD COLUMN lease_owner TEXT;
            ALTER TABLE idempotency_keys ADD COLUMN lease_expires_at_ms INTEGER;
            ALTER TABLE idempotency_keys ADD COLUMN admitted_run_id TEXT REFERENCES runs(id);
            ALTER TABLE idempotency_keys ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE idempotency_keys ADD COLUMN last_error_code TEXT;
            CREATE INDEX idempotency_lease_idx
                ON idempotency_keys(status, lease_expires_at_ms);
            CREATE INDEX idempotency_run_idx
                ON idempotency_keys(admitted_run_id);

            ALTER TABLE cron_runs ADD COLUMN supervisor_run_id TEXT REFERENCES runs(id);
            CREATE INDEX cron_runs_supervisor_run_idx
                ON cron_runs(supervisor_run_id);
            CREATE UNIQUE INDEX cron_runs_schedule_unique_idx
                ON cron_runs(job_id, scheduled_for);
        "#,
    },
    StoreMigration {
        version: 11,
        name: "011_content_artifacts",
        sql: r#"
            CREATE TABLE content_blobs (
                digest TEXT PRIMARY KEY,
                media_type TEXT NOT NULL,
                byte_length INTEGER NOT NULL CHECK (byte_length > 0),
                created_at_ms INTEGER NOT NULL
            );
            CREATE TABLE artifacts (
                id TEXT PRIMARY KEY,
                blob_digest TEXT NOT NULL REFERENCES content_blobs(digest),
                run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                media_type TEXT NOT NULL,
                byte_length INTEGER NOT NULL CHECK (byte_length > 0),
                width INTEGER,
                height INTEGER,
                sensitivity TEXT NOT NULL CHECK (sensitivity IN ('private', 'sensitive')),
                source_json TEXT NOT NULL,
                retention TEXT NOT NULL CHECK (retention IN ('run_scoped', 'persistent')),
                state TEXT NOT NULL CHECK (state IN ('live', 'tombstoned')),
                created_at_ms INTEGER NOT NULL,
                tombstoned_at_ms INTEGER,
                CHECK ((width IS NULL) = (height IS NULL))
            );
            CREATE INDEX artifacts_run_idx ON artifacts(run_id, state, created_at_ms);
            CREATE INDEX artifacts_blob_idx ON artifacts(blob_digest, state);
            CREATE INDEX artifacts_gc_idx ON artifacts(state, tombstoned_at_ms);
        "#,
    },
    StoreMigration {
        version: 12,
        name: "012_permission_sensitive_inputs",
        sql: r#"
            ALTER TABLE permission_requests
                ADD COLUMN sensitive_inputs_json TEXT NOT NULL DEFAULT '[]';
            ALTER TABLE permission_grants
                ADD COLUMN sensitive_inputs_json TEXT NOT NULL DEFAULT '[]';
        "#,
    },
    StoreMigration {
        version: 13,
        name: "013_supervised_subagent_runs",
        sql: r#"
            ALTER TABLE runs RENAME COLUMN kind TO obsolete_kind;
            ALTER TABLE runs ADD COLUMN kind TEXT NOT NULL DEFAULT 'cron'
                CHECK (kind IN ('turn', 'cron', 'subagent'));
            UPDATE runs SET kind = obsolete_kind;
            ALTER TABLE runs DROP COLUMN obsolete_kind;

            ALTER TABLE runs ADD COLUMN parent_run_id TEXT REFERENCES runs(id);
            ALTER TABLE runs ADD COLUMN root_run_id TEXT REFERENCES runs(id);
            ALTER TABLE runs ADD COLUMN depth INTEGER NOT NULL DEFAULT 0 CHECK (depth >= 0);
            ALTER TABLE runs ADD COLUMN subagent_id TEXT;
            ALTER TABLE runs ADD COLUMN spawned_by_step_id TEXT REFERENCES run_steps(id);
            ALTER TABLE runs ADD COLUMN child_spec_digest TEXT;
            ALTER TABLE runs ADD COLUMN model_profile_digest TEXT;
            ALTER TABLE runs ADD COLUMN result_delivered_at_ms INTEGER;
            ALTER TABLE runs ADD COLUMN tree_usage_recorded_at_ms INTEGER;
            ALTER TABLE runs ADD COLUMN delegation_budget_json TEXT;
            ALTER TABLE runs ADD COLUMN delegation_reserved_descendants INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE runs ADD COLUMN delegation_reserved_output_tokens INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE runs ADD COLUMN delegation_used_output_tokens INTEGER NOT NULL DEFAULT 0;
            UPDATE runs SET root_run_id = id;

            CREATE UNIQUE INDEX runs_spawn_step_unique_idx
                ON runs(spawned_by_step_id) WHERE spawned_by_step_id IS NOT NULL;
            CREATE INDEX runs_parent_idx ON runs(parent_run_id, created_at_ms);
            CREATE INDEX runs_root_depth_idx ON runs(root_run_id, depth, created_at_ms);

            CREATE TRIGGER runs_subagent_insert_guard
            BEFORE INSERT ON runs
            WHEN (
                NEW.kind = 'subagent' AND (
                    NEW.session_id IS NOT NULL OR NEW.turn_id IS NOT NULL OR
                    NEW.parent_run_id IS NULL OR NEW.root_run_id IS NULL OR
                    NEW.depth <= 0 OR NEW.subagent_id IS NULL OR
                    NEW.spawned_by_step_id IS NULL OR NEW.child_spec_digest IS NULL OR
                    NEW.model_profile_digest IS NULL
                )
            ) OR (
                NEW.kind != 'subagent' AND (
                    NEW.parent_run_id IS NOT NULL OR NEW.depth != 0 OR
                    NEW.subagent_id IS NOT NULL OR NEW.spawned_by_step_id IS NOT NULL OR
                    NEW.child_spec_digest IS NOT NULL OR NEW.model_profile_digest IS NOT NULL
                )
            )
            BEGIN
                SELECT RAISE(ABORT, 'invalid supervised subagent run shape');
            END;
        "#,
    },
];
