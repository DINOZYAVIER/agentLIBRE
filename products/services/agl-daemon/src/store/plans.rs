use std::path::Path;

use agl_core::implementation_plan::{
    PlanDigest, PlanId, PlanState, SliceResult, SliceState, WorkspaceSnapshot,
};
use rusqlite::{OptionalExtension, params};

use super::{StoreError, StoreHandle};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlanRecord {
    pub plan_id: PlanId,
    pub state: PlanState,
    pub draft_digest: PlanDigest,
    pub approved_digest: Option<PlanDigest>,
    pub workspace_root: String,
    pub planner_conversation_id: agl_core::ConversationId,
    pub initial_workspace: Option<WorkspaceSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SliceRecord {
    pub slice_id: String,
    pub state: SliceState,
    pub result: Option<SliceResult>,
    pub stale_paths: Vec<String>,
}

impl StoreHandle {
    pub(crate) fn register_plan_draft(
        &self,
        plan_id: PlanId,
        draft_digest: PlanDigest,
        workspace_root: &Path,
        planner_conversation_id: agl_core::ConversationId,
        state: PlanState,
    ) -> crate::store::Result<()> {
        let workspace_root = workspace_root
            .to_str()
            .ok_or_else(|| StoreError::InvalidValue {
                field: "plan workspace root",
                value: workspace_root.display().to_string(),
                reason: "workspace root must be valid UTF-8",
            })?;
        let store = self.lock()?;
        store.transaction(|tx| {
            super::agent::create_agent_schema_connection(tx)?;
            let now = unix_ms();
            tx.execute(
                "INSERT INTO implementation_plans (
                    plan_id, state, draft_digest, approved_digest, workspace_root,
                    planner_conversation_id, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?6)
                 ON CONFLICT(plan_id) DO UPDATE SET
                    state=excluded.state,
                    draft_digest=excluded.draft_digest,
                    approved_digest=NULL,
                    workspace_root=excluded.workspace_root,
                    planner_conversation_id=excluded.planner_conversation_id,
                    updated_at_ms=excluded.updated_at_ms",
                params![
                    plan_id.as_bytes().as_slice(),
                    state_name(state),
                    draft_digest.as_bytes().as_slice(),
                    workspace_root,
                    planner_conversation_id.as_bytes().as_slice(),
                    now,
                ],
            )?;
            Ok(())
        })
    }

    pub(crate) fn plan_record(&self, plan_id: PlanId) -> crate::store::Result<PlanRecord> {
        self.read(|connection| {
            connection
                .query_row(
                    "SELECT state, draft_digest, approved_digest, workspace_root,
                            planner_conversation_id, initial_workspace_json
                     FROM implementation_plans WHERE plan_id=?1",
                    [plan_id.clone().as_bytes().as_slice()],
                    |row| {
                        let state: String = row.get(0)?;
                        let draft_digest = digest_from_blob(row.get(1)?, "draft_digest")?;
                        let approved_digest = row
                            .get::<_, Option<Vec<u8>>>(2)?
                            .map(|bytes| digest_from_blob(bytes, "approved_digest"))
                            .transpose()?;
                        let workspace_root = row.get(3)?;
                        let conversation_id =
                            conversation_from_blob(row.get(4)?, "planner_conversation_id")?;
                        let initial_workspace = row
                            .get::<_, Option<String>>(5)?
                            .map(|value| decode_json(&value, 5, "initial_workspace_json"))
                            .transpose()?;
                        Ok(PlanRecord {
                            plan_id: plan_id.clone(),
                            state: parse_state(&state)?,
                            draft_digest,
                            approved_digest,
                            workspace_root,
                            planner_conversation_id: conversation_id,
                            initial_workspace,
                        })
                    },
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound {
                    resource: format!("implementation plan {plan_id}"),
                })
        })
    }

    /// Commit the database half of approval after the immutable artifact exists.
    /// A failed transaction leaves an unreferenced artifact, never a dangling row.
    pub(crate) fn approve_plan(
        &self,
        plan_id: PlanId,
        expected: &PlanDigest,
    ) -> crate::store::Result<PlanRecord> {
        let store = self.lock()?;
        store.transaction(|tx| {
            super::agent::create_agent_schema_connection(tx)?;
            let current = tx
                .query_row(
                    "SELECT state, draft_digest, approved_digest, workspace_root,
                            planner_conversation_id, initial_workspace_json
                     FROM implementation_plans WHERE plan_id=?1",
                    [plan_id.as_bytes().as_slice()],
                    |row| {
                        let state: String = row.get(0)?;
                        let draft_digest = digest_from_blob(row.get(1)?, "draft_digest")?;
                        let approved_digest = row
                            .get::<_, Option<Vec<u8>>>(2)?
                            .map(|bytes| digest_from_blob(bytes, "approved_digest"))
                            .transpose()?;
                        Ok((
                            parse_state(&state)?,
                            draft_digest,
                            approved_digest,
                            row.get::<_, String>(3)?,
                            conversation_from_blob(row.get(4)?, "planner_conversation_id")?,
                            row.get::<_, Option<String>>(5)?
                                .map(|value| decode_json(&value, 5, "initial_workspace_json"))
                                .transpose()?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound {
                    resource: format!("implementation plan {plan_id}"),
                })?;
            let (
                state,
                draft_digest,
                approved_digest,
                workspace_root,
                conversation_id,
                initial_workspace,
            ) = current;
            if state == PlanState::Approved {
                if approved_digest.as_ref() != Some(expected) {
                    return Err(StoreError::TransitionRejected {
                        resource: format!("implementation plan {plan_id}"),
                        from: "approved with another digest".into(),
                        to: "approved".into(),
                    });
                }
                return Ok(PlanRecord {
                    plan_id,
                    state,
                    draft_digest,
                    approved_digest,
                    workspace_root,
                    planner_conversation_id: conversation_id,
                    initial_workspace,
                });
            }
            if state != PlanState::ReadyForApproval || &draft_digest != expected {
                return Err(StoreError::TransitionRejected {
                    resource: format!("implementation plan {plan_id}"),
                    from: format!("{} with digest {draft_digest}", state_name(state)),
                    to: format!("approved with digest {expected}"),
                });
            }
            tx.execute(
                "UPDATE implementation_plans
                 SET state='approved', approved_digest=?2, updated_at_ms=?3
                 WHERE plan_id=?1 AND state='ready_for_approval'
                   AND draft_digest=?2 AND approved_digest IS NULL",
                params![
                    plan_id.as_bytes().as_slice(),
                    expected.as_bytes().as_slice(),
                    unix_ms()
                ],
            )?;
            Ok(PlanRecord {
                plan_id,
                state: PlanState::Approved,
                draft_digest,
                approved_digest: Some(expected.clone()),
                workspace_root,
                planner_conversation_id: conversation_id,
                initial_workspace: None,
            })
        })
    }

    pub(crate) fn begin_plan_implementation(
        &self,
        plan_id: PlanId,
        expected: &PlanDigest,
        slice_ids: &[String],
        initial_workspace: &WorkspaceSnapshot,
    ) -> crate::store::Result<PlanRecord> {
        let store = self.lock()?;
        store.transaction(|tx| {
            super::agent::create_agent_schema_connection(tx)?;
            let record = load_plan_record(tx, &plan_id)?;
            if record.state == PlanState::Implementing
                || record.state == PlanState::Completed
                || record.state == PlanState::Failed
                || record.state == PlanState::Stale
            {
                if record.approved_digest.as_ref() != Some(expected) {
                    return Err(StoreError::TransitionRejected {
                        resource: format!("implementation plan {plan_id}"),
                        from: "approved with another digest".into(),
                        to: "implement".into(),
                    });
                }
                return Ok(record);
            }
            if record.state != PlanState::Approved || record.approved_digest.as_ref() != Some(expected) {
                return Err(StoreError::TransitionRejected {
                    resource: format!("implementation plan {plan_id}"),
                    from: state_name(record.state).to_string(),
                    to: "implementing".into(),
                });
            }
            let initial_workspace = serde_json::to_string(initial_workspace)?;
            tx.execute(
                "UPDATE implementation_plans SET state='implementing', initial_workspace_json=?2, updated_at_ms=?3 WHERE plan_id=?1 AND state='approved' AND approved_digest=?4",
                rusqlite::params![plan_id.as_bytes().as_slice(), initial_workspace, unix_ms(), expected.as_bytes().as_slice()],
            )?;
            for slice_id in slice_ids {
                tx.execute(
                    "INSERT OR IGNORE INTO implementation_plan_slices (plan_id, slice_id, state, result_json, stale_paths_json, updated_at_ms) VALUES (?1, ?2, 'pending', NULL, '[]', ?3)",
                    rusqlite::params![plan_id.as_bytes().as_slice(), slice_id, unix_ms()],
                )?;
            }
            load_plan_record(tx, &plan_id)
        })
    }

    pub(crate) fn claim_slice(
        &self,
        plan_id: &PlanId,
        slice_id: &str,
    ) -> crate::store::Result<bool> {
        let store = self.lock()?;
        store.transaction(|tx| {
            let changed = tx.execute(
                "UPDATE implementation_plan_slices SET state='running', updated_at_ms=?3 WHERE plan_id=?1 AND slice_id=?2 AND state='pending' AND EXISTS (SELECT 1 FROM implementation_plans WHERE plan_id=?1 AND state='implementing')",
                rusqlite::params![plan_id.as_bytes().as_slice(), slice_id, unix_ms()],
            )?;
            Ok(changed == 1)
        })
    }

    pub(crate) fn finish_slice(
        &self,
        plan_id: &PlanId,
        slice_id: &str,
        result: &SliceResult,
    ) -> crate::store::Result<()> {
        let state = match result.state {
            SliceState::Completed => "completed",
            SliceState::Failed => "failed",
            _ => {
                return Err(StoreError::InvalidValue {
                    field: "slice result",
                    value: slice_id.into(),
                    reason: "terminal result required",
                });
            }
        };
        let json = serde_json::to_string(result)?;
        let store = self.lock()?;
        store.transaction(|tx| {
            let expected_digest: Vec<u8> = tx.query_row(
                "SELECT approved_digest FROM implementation_plans WHERE plan_id=?1",
                [plan_id.as_bytes().as_slice()],
                |row| row.get(0),
            )?;
            if result.plan_id != *plan_id
                || result.slice_id.as_str() != slice_id
                || result.plan_digest.as_bytes() != expected_digest.as_slice()
            {
                return Err(StoreError::TransitionRejected {
                    resource: format!("slice {slice_id}"),
                    from: "result identity mismatch".into(),
                    to: state.into(),
                });
            }
            let changed = tx.execute(
                "UPDATE implementation_plan_slices SET state=?3, result_json=?4, updated_at_ms=?5 WHERE plan_id=?1 AND slice_id=?2 AND state='running'",
                rusqlite::params![plan_id.as_bytes().as_slice(), slice_id, state, json, unix_ms()],
            )?;
            if changed != 1 {
                return Err(StoreError::TransitionRejected { resource: format!("slice {slice_id}"), from: "not running".into(), to: state.into() });
            }
            if state == "failed" {
                tx.execute("UPDATE implementation_plans SET state='failed', updated_at_ms=?2 WHERE plan_id=?1 AND state='implementing'", rusqlite::params![plan_id.as_bytes().as_slice(), unix_ms()])?;
            } else {
                let pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM implementation_plan_slices WHERE plan_id=?1 AND state != 'completed')", [plan_id.as_bytes().as_slice()], |row| row.get(0))?;
                if !pending {
                    tx.execute("UPDATE implementation_plans SET state='completed', updated_at_ms=?2 WHERE plan_id=?1 AND state='implementing'", rusqlite::params![plan_id.as_bytes().as_slice(), unix_ms()])?;
                }
            }
            Ok(())
        })
    }

    pub(crate) fn mark_plan_stale(
        &self,
        plan_id: &PlanId,
        paths: &[String],
    ) -> crate::store::Result<()> {
        let paths = canonical_paths(paths);
        let paths = serde_json::to_string(&paths)?;
        let store = self.lock()?;
        store.transaction(|tx| {
            tx.execute("UPDATE implementation_plans SET state='stale', updated_at_ms=?2 WHERE plan_id=?1 AND state IN ('approved','implementing')", rusqlite::params![plan_id.as_bytes().as_slice(), unix_ms()])?;
            tx.execute("UPDATE implementation_plan_slices SET state='stale', stale_paths_json=?3, updated_at_ms=?4 WHERE plan_id=?1 AND state IN ('pending','running')", rusqlite::params![plan_id.as_bytes().as_slice(), "stale", paths, unix_ms()])?;
            Ok(())
        })
    }

    pub(crate) fn mark_initial_plan_stale(
        &self,
        plan_id: &PlanId,
        expected: &PlanDigest,
        slice_ids: &[String],
        paths: &[String],
    ) -> crate::store::Result<()> {
        let paths = serde_json::to_string(&canonical_paths(paths))?;
        let store = self.lock()?;
        store.transaction(|tx| {
            let changed = tx.execute(
                "UPDATE implementation_plans SET state='stale', updated_at_ms=?3 WHERE plan_id=?1 AND state='approved' AND approved_digest=?2",
                rusqlite::params![plan_id.as_bytes().as_slice(), expected.as_bytes().as_slice(), unix_ms()],
            )?;
            if changed != 1 {
                return Err(StoreError::TransitionRejected {
                    resource: format!("implementation plan {plan_id}"),
                    from: "not approved with expected digest".into(),
                    to: "stale".into(),
                });
            }
            for slice_id in slice_ids {
                tx.execute(
                    "INSERT INTO implementation_plan_slices (plan_id, slice_id, state, result_json, stale_paths_json, updated_at_ms) VALUES (?1, ?2, 'stale', NULL, ?3, ?4)",
                    rusqlite::params![plan_id.as_bytes().as_slice(), slice_id, paths, unix_ms()],
                )?;
            }
            Ok(())
        })
    }

    pub(crate) fn slice_records(&self, plan_id: &PlanId) -> crate::store::Result<Vec<SliceRecord>> {
        self.read(|connection| {
            let mut stmt = connection.prepare("SELECT slice_id, state, result_json, stale_paths_json FROM implementation_plan_slices WHERE plan_id=?1 ORDER BY rowid")?;
            let rows = stmt.query_map([plan_id.as_bytes().as_slice()], |row| {
                let state: String = row.get(1)?;
                let result: Option<String> = row.get(2)?;
                let stale: String = row.get(3)?;
                Ok(SliceRecord {
                    slice_id: row.get(0)?,
                    state: parse_slice_state(&state)?,
                    result: result
                        .map(|value| decode_json(&value, 2, "result_json"))
                        .transpose()?,
                    stale_paths: decode_json(&stale, 3, "stale_paths_json")?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        })
    }
}

fn canonical_paths(paths: &[String]) -> Vec<String> {
    let mut paths = paths.to_vec();
    paths.sort();
    paths.dedup();
    paths
}

fn load_plan_record(
    tx: &rusqlite::Transaction<'_>,
    plan_id: &PlanId,
) -> crate::store::Result<PlanRecord> {
    tx.query_row("SELECT state, draft_digest, approved_digest, workspace_root, planner_conversation_id, initial_workspace_json FROM implementation_plans WHERE plan_id=?1", [plan_id.as_bytes().as_slice()], |row| {
        let state: String = row.get(0)?;
        Ok(PlanRecord { plan_id: plan_id.clone(), state: parse_state(&state)?, draft_digest: digest_from_blob(row.get(1)?, "draft_digest")?, approved_digest: row.get::<_, Option<Vec<u8>>>(2)?.map(|v| digest_from_blob(v, "approved_digest")).transpose()?, workspace_root: row.get(3)?, planner_conversation_id: conversation_from_blob(row.get(4)?, "planner_conversation_id")?, initial_workspace: row.get::<_, Option<String>>(5)?.map(|v| decode_json(&v, 5, "initial_workspace_json")).transpose()? })
    }).map_err(Into::into)
}

fn state_name(state: PlanState) -> &'static str {
    match state {
        PlanState::Draft => "draft",
        PlanState::AwaitingDecisions => "awaiting_decisions",
        PlanState::ReadyForApproval => "ready_for_approval",
        PlanState::Approved => "approved",
        PlanState::Implementing => "implementing",
        PlanState::Completed => "completed",
        PlanState::Failed => "failed",
        PlanState::Stale => "stale",
    }
}

fn parse_state(value: &str) -> rusqlite::Result<PlanState> {
    match value {
        "draft" => Ok(PlanState::Draft),
        "awaiting_decisions" => Ok(PlanState::AwaitingDecisions),
        "ready_for_approval" => Ok(PlanState::ReadyForApproval),
        "approved" => Ok(PlanState::Approved),
        "implementing" => Ok(PlanState::Implementing),
        "completed" => Ok(PlanState::Completed),
        "failed" => Ok(PlanState::Failed),
        "stale" => Ok(PlanState::Stale),
        _ => Err(rusqlite::Error::InvalidColumnType(
            0,
            "state".into(),
            rusqlite::types::Type::Text,
        )),
    }
}

fn parse_slice_state(value: &str) -> rusqlite::Result<SliceState> {
    match value {
        "pending" => Ok(SliceState::Pending),
        "running" => Ok(SliceState::Running),
        "completed" => Ok(SliceState::Completed),
        "failed" => Ok(SliceState::Failed),
        "stale" => Ok(SliceState::Stale),
        _ => Err(rusqlite::Error::InvalidColumnType(
            1,
            "state".into(),
            rusqlite::types::Type::Text,
        )),
    }
}

fn decode_json<T: serde::de::DeserializeOwned>(
    value: &str,
    column: usize,
    field: &'static str,
) -> rusqlite::Result<T> {
    serde_json::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(JsonCorruption { field, error }),
        )
    })
}

#[derive(Debug)]
struct JsonCorruption {
    field: &'static str,
    error: serde_json::Error,
}

impl std::fmt::Display for JsonCorruption {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "corrupt {field}: {error}",
            field = self.field,
            error = self.error
        )
    }
}

impl std::error::Error for JsonCorruption {}

fn digest_from_blob(bytes: Vec<u8>, field: &'static str) -> rusqlite::Result<PlanDigest> {
    bytes.try_into().map(PlanDigest::from_bytes).map_err(|_| {
        rusqlite::Error::InvalidColumnType(0, field.into(), rusqlite::types::Type::Blob)
    })
}

fn conversation_from_blob(
    bytes: Vec<u8>,
    field: &'static str,
) -> rusqlite::Result<agl_core::ConversationId> {
    let bytes = bytes.try_into().map_err(|_| {
        rusqlite::Error::InvalidColumnType(0, field.into(), rusqlite::types::Type::Blob)
    })?;
    agl_core::ConversationId::from_bytes(bytes).map_err(|_| {
        rusqlite::Error::InvalidColumnType(0, field.into(), rusqlite::types::Type::Blob)
    })
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!("agl-plan-state-{}", uuid::Uuid::now_v7()))
    }

    #[test]
    fn plan_state_survives_store_restart() {
        let root = root();
        let store = StoreHandle::open_at(root.join("store")).unwrap();
        let conversation = agl_core::ConversationId::generate();
        {
            let database = store.lock().unwrap();
            database
                .transaction(|tx| {
                    tx.execute(
                        "INSERT INTO agent_conversations (
                            conversation_id, display_name, function_json, workspace_root,
                            snapshot_json, created_at_ms, last_active_at_ms
                         ) VALUES (?1, NULL, '{}', '/workspace', '{}', 1, 1)",
                        params![conversation.as_bytes().as_slice()],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        let plan_id = PlanId::generate();
        let digest = PlanDigest::from_bytes([7; 32]);
        store
            .register_plan_draft(
                plan_id.clone(),
                digest.clone(),
                Path::new("/workspace"),
                conversation,
                PlanState::ReadyForApproval,
            )
            .unwrap();
        drop(store);

        let reopened = StoreHandle::open_at(root.join("store")).unwrap();
        let record = reopened.plan_record(plan_id).unwrap();
        assert_eq!(record.state, PlanState::ReadyForApproval);
        assert_eq!(record.draft_digest, digest);
        drop(reopened);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn result_and_workspace_json_corruption_is_reported_as_sqlite_conversion_failure() {
        let root = root();
        let store = StoreHandle::open_at(root.join("store")).unwrap();
        let conversation = agl_core::ConversationId::generate();
        {
            let database = store.lock().unwrap();
            database
                .transaction(|tx| {
                    tx.execute(
                        "INSERT INTO agent_conversations (conversation_id, display_name, function_json, workspace_root, snapshot_json, created_at_ms, last_active_at_ms) VALUES (?1, NULL, '{}', '/workspace', '{}', 1, 1)",
                        params![conversation.as_bytes().as_slice()],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        let plan_id = PlanId::generate();
        let digest = PlanDigest::from_bytes([8; 32]);
        let initial = WorkspaceSnapshot {
            digest: PlanDigest::from_bytes([1; 32]),
            files: vec![],
        };
        store
            .register_plan_draft(
                plan_id.clone(),
                digest.clone(),
                Path::new("/workspace"),
                conversation,
                PlanState::ReadyForApproval,
            )
            .unwrap();
        store.approve_plan(plan_id.clone(), &digest).unwrap();
        store
            .begin_plan_implementation(plan_id.clone(), &digest, &["slice".into()], &initial)
            .unwrap();
        {
            let database = store.lock().unwrap();
            database
                .transaction(|tx| {
                    tx.execute(
                        "UPDATE implementation_plans SET initial_workspace_json='[]' WHERE plan_id=?1",
                        params![plan_id.as_bytes().as_slice()],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        let error = store.plan_record(plan_id.clone()).unwrap_err();
        assert!(error.to_string().contains("corrupt initial_workspace_json"));

        {
            let database = store.lock().unwrap();
            database
                .transaction(|tx| {
                    tx.execute(
                        "UPDATE implementation_plans SET initial_workspace_json=NULL WHERE plan_id=?1",
                        params![plan_id.as_bytes().as_slice()],
                    )?;
                    tx.execute(
                        "UPDATE implementation_plan_slices SET state='completed', result_json='[]' WHERE plan_id=?1 AND slice_id='slice'",
                        params![plan_id.as_bytes().as_slice()],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        let error = store.slice_records(&plan_id).unwrap_err();
        assert!(error.to_string().contains("corrupt result_json"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stale_transition_persists_exact_external_paths_after_completed_predecessor() {
        let root = root();
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let store = StoreHandle::open_at(root.join("store")).unwrap();
        let conversation = agl_core::ConversationId::generate();
        {
            let database = store.lock().unwrap();
            database
                .transaction(|tx| {
                    tx.execute(
                        "INSERT INTO agent_conversations (conversation_id, display_name, function_json, workspace_root, snapshot_json, created_at_ms, last_active_at_ms) VALUES (?1, NULL, '{}', ?2, '{}', 1, 1)",
                        params![conversation.as_bytes().as_slice(), workspace.to_str().unwrap()],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        let plan_id = PlanId::generate();
        let digest = PlanDigest::from_bytes([6; 32]);
        let initial = WorkspaceSnapshot {
            digest: PlanDigest::from_bytes([2; 32]),
            files: vec![],
        };
        store
            .register_plan_draft(
                plan_id.clone(),
                digest.clone(),
                &workspace,
                conversation,
                PlanState::ReadyForApproval,
            )
            .unwrap();
        store.approve_plan(plan_id.clone(), &digest).unwrap();
        store
            .begin_plan_implementation(
                plan_id.clone(),
                &digest,
                &["first".into(), "second".into()],
                &initial,
            )
            .unwrap();
        assert!(store.claim_slice(&plan_id, "first").unwrap());
        let completed = SliceResult {
            schema: agl_core::implementation_plan::SliceResultSchema::V1,
            plan_id: plan_id.clone(),
            plan_digest: digest,
            slice_id: agl_core::implementation_plan::PlanItemId::new("first").unwrap(),
            state: SliceState::Completed,
            changed_paths: vec![],
            verification: vec![],
            additional_reads: vec![],
            conversation_id: Some(agl_core::ConversationId::generate()),
            run_id: Some(agl_core::AgentRunId::generate()),
            workspace: initial,
            failure: None,
        };
        store.finish_slice(&plan_id, "first", &completed).unwrap();
        std::fs::write(workspace.join("external"), "drift").unwrap();
        let current = crate::workspace::capture(&workspace).unwrap();
        let expected = crate::workspace::WorkspaceState {
            snapshot: completed.workspace.clone(),
            files: completed
                .workspace
                .files
                .iter()
                .map(|file| (file.path.as_str().to_owned(), file.digest.clone()))
                .collect(),
        };
        let paths = current
            .changed_paths(&expected)
            .into_iter()
            .map(|path| path.path.as_str().to_owned())
            .collect::<Vec<_>>();
        store.mark_plan_stale(&plan_id, &paths).unwrap();
        let record = store.plan_record(plan_id.clone()).unwrap();
        assert_eq!(record.state, PlanState::Stale);
        let slices = store.slice_records(&plan_id).unwrap();
        assert_eq!(slices[1].state, SliceState::Stale);
        assert_eq!(slices[1].stale_paths, vec!["external"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn claimed_slice_failure_is_terminal_and_cannot_be_reclaimed() {
        let root = root();
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let store = StoreHandle::open_at(root.join("store")).unwrap();
        let conversation = agl_core::ConversationId::generate();
        {
            let database = store.lock().unwrap();
            database
                .transaction(|tx| {
                    tx.execute(
                        "INSERT INTO agent_conversations (conversation_id, display_name, function_json, workspace_root, snapshot_json, created_at_ms, last_active_at_ms) VALUES (?1, NULL, '{}', ?2, '{}', 1, 1)",
                        params![conversation.as_bytes().as_slice(), workspace.to_str().unwrap()],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        let plan_id = PlanId::generate();
        let digest = PlanDigest::from_bytes([9; 32]);
        let initial = WorkspaceSnapshot {
            digest: PlanDigest::from_bytes([1; 32]),
            files: vec![],
        };
        store
            .register_plan_draft(
                plan_id.clone(),
                digest.clone(),
                &workspace,
                conversation,
                PlanState::ReadyForApproval,
            )
            .unwrap();
        store.approve_plan(plan_id.clone(), &digest).unwrap();
        store
            .begin_plan_implementation(plan_id.clone(), &digest, &["slice".into()], &initial)
            .unwrap();
        assert!(store.claim_slice(&plan_id, "slice").unwrap());
        let result = SliceResult {
            schema: agl_core::implementation_plan::SliceResultSchema::V1,
            plan_id: plan_id.clone(),
            plan_digest: digest.clone(),
            slice_id: agl_core::implementation_plan::PlanItemId::new("slice").unwrap(),
            state: SliceState::Failed,
            changed_paths: vec![],
            verification: vec![],
            additional_reads: vec![],
            conversation_id: None,
            run_id: None,
            workspace: initial,
            failure: Some(
                agl_core::implementation_plan::PlanText::new("activation failed").unwrap(),
            ),
        };
        store.finish_slice(&plan_id, "slice", &result).unwrap();
        assert_eq!(
            store.plan_record(plan_id.clone()).unwrap().state,
            PlanState::Failed
        );
        assert!(!store.claim_slice(&plan_id, "slice").unwrap());
        let persisted = store.slice_records(&plan_id).unwrap();
        assert_eq!(persisted[0].state, SliceState::Failed);
        assert_eq!(persisted[0].result, Some(result));
        let _ = std::fs::remove_dir_all(root);
    }
}
