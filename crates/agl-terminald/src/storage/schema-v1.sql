BEGIN;
CREATE TABLE executions (
    id TEXT PRIMARY KEY, owner_namespace TEXT NOT NULL, owner_namespace_version INTEGER NOT NULL,
    owner_id TEXT NOT NULL, owner_kind TEXT NOT NULL, owner_role TEXT NOT NULL,
    lifecycle_scope_id TEXT NOT NULL, correlation_namespace TEXT NOT NULL,
    correlation_namespace_version INTEGER NOT NULL, correlation_group_id TEXT NOT NULL,
    correlation_operation_id TEXT NOT NULL, execution_kind TEXT NOT NULL, state TEXT NOT NULL,
    profile TEXT NOT NULL, io TEXT NOT NULL, cwd TEXT NOT NULL, terminal_columns INTEGER,
    terminal_rows INTEGER, supervisor_id TEXT NOT NULL, exit_kind TEXT, exit_code INTEGER,
    exit_signal INTEGER, exit_error_code TEXT, error_code TEXT, started_at_ms INTEGER,
    finished_at_ms INTEGER, created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL,
    first_retained_sequence INTEGER, last_sequence INTEGER NOT NULL DEFAULT 0,
    retained_bytes INTEGER NOT NULL DEFAULT 0, discarded_output_bytes INTEGER NOT NULL DEFAULT 0,
    accepted_input_bytes INTEGER NOT NULL DEFAULT 0, output_truncated INTEGER NOT NULL DEFAULT 0,
    output_expired INTEGER NOT NULL DEFAULT 0, input_lease_id TEXT, input_lease_renewed_at_ms INTEGER,
    grant_lease_json TEXT, invocation_json TEXT NOT NULL, spool_ref TEXT NOT NULL,
    retention_deadline_ms INTEGER, cleanup_state TEXT NOT NULL DEFAULT 'live'
);
CREATE INDEX executions_owner_idx ON executions(owner_namespace, owner_namespace_version, owner_id, state, created_at_ms);
CREATE INDEX executions_correlation_group_idx ON executions(correlation_namespace, correlation_namespace_version, correlation_group_id, state, created_at_ms);
CREATE INDEX executions_supervisor_idx ON executions(supervisor_id, state);
CREATE INDEX executions_retention_idx ON executions(cleanup_state, retention_deadline_ms);
CREATE TABLE execution_events (
    execution_id TEXT NOT NULL, sequence INTEGER NOT NULL, kind TEXT NOT NULL, channel TEXT,
    spool_offset INTEGER, byte_length INTEGER NOT NULL DEFAULT 0, bounded_preview_json TEXT,
    occurred_at_ms INTEGER NOT NULL, safe_digest TEXT NOT NULL,
    PRIMARY KEY (execution_id, sequence), FOREIGN KEY(execution_id) REFERENCES executions(id) ON DELETE CASCADE
);
CREATE INDEX execution_events_replay_idx ON execution_events(execution_id, sequence);
CREATE TABLE terminal_sessions (
    terminal_id TEXT PRIMARY KEY, execution_id TEXT NOT NULL UNIQUE, topology_id TEXT NOT NULL,
    owner_json TEXT NOT NULL, lifecycle_scope_id TEXT NOT NULL, profile TEXT NOT NULL,
    workspace_root BLOB NOT NULL, shell_kind TEXT NOT NULL, shell_program BLOB NOT NULL,
    shell_argv_json TEXT NOT NULL, shell_login_argv_json TEXT, shell_environment_names_json TEXT NOT NULL,
    shell_executable_digest TEXT NOT NULL, shell_config_digest TEXT NOT NULL,
    environment_digest TEXT NOT NULL, command_sequence INTEGER NOT NULL, prompt_kind TEXT NOT NULL,
    prompt_sequence INTEGER, prompt_last_exit INTEGER, prompt_process_group INTEGER,
    integration_health TEXT NOT NULL, cwd BLOB NOT NULL, state TEXT NOT NULL, slot_key TEXT NOT NULL,
    fingerprint TEXT NOT NULL, active_slot INTEGER NOT NULL
);
CREATE UNIQUE INDEX terminal_sessions_active_slot_unique_idx ON terminal_sessions(slot_key) WHERE active_slot = 1;
CREATE TABLE service_admissions (
    resource_kind TEXT NOT NULL, resource_id TEXT NOT NULL,
    request_fingerprint TEXT NOT NULL UNIQUE, authority_fingerprint TEXT NOT NULL,
    operations_json TEXT NOT NULL,
    PRIMARY KEY (resource_kind, resource_id)
);
CREATE INDEX service_admissions_authority_idx ON service_admissions(authority_fingerprint, resource_kind);
PRAGMA user_version=1;
COMMIT;
