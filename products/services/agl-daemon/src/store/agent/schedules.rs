use super::*;
use agl_core::agent::AgentRunOrigin;
use agl_runtime::function::FunctionSchedule;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ScheduleRecord {
    pub function: ExactPackageRef,
    pub conversation_id: ConversationId,
    pub enabled: bool,
    pub enabled_at_ms: i64,
    pub generation: agl_core::RequestId,
    pub config: FunctionSchedule,
    pub permissions: serde_json::Value,
    pub snapshot: AgentRunSnapshot,
    pub last_run: Option<AgentRunId>,
}

pub(crate) fn next_tick_after(anchor: i64, interval_seconds: u64, now: i64) -> Option<i64> {
    let interval = i64::try_from(interval_seconds).ok()?.checked_mul(1000)?;
    if anchor < 0 || interval <= 0 || now < 0 {
        return None;
    }
    let elapsed = now.saturating_sub(anchor).max(0);
    anchor.checked_add(
        elapsed
            .checked_div(interval)?
            .checked_add(1)?
            .checked_mul(interval)?,
    )
}

impl ScheduleRecord {
    pub fn view(&self, now: i64) -> agl_daemon_api::ScheduleView {
        agl_daemon_api::ScheduleView {
            function: self.function.clone(),
            workspace_path: Some(
                self.snapshot
                    .workspace
                    .root
                    .as_path()
                    .to_string_lossy()
                    .into_owned(),
            ),
            conversation_id: Some(self.conversation_id),
            enabled: self.enabled,
            interval_seconds: self.config.interval_seconds,
            prompt: self.config.prompt.clone(),
            max_run_seconds: self.config.max_run_seconds,
            events: self.config.events.clone(),
            permissions: self.permissions.clone(),
            last_run: self.last_run,
            next_tick_at_ms: self
                .enabled
                .then(|| next_tick_after(self.enabled_at_ms, self.config.interval_seconds, now))
                .flatten(),
        }
    }
}

fn records(
    connection: &rusqlite::Connection,
    workspace: Option<&Path>,
) -> crate::store::Result<Vec<ScheduleRecord>> {
    let mut statement = connection.prepare(
        "SELECT c.function_json, s.conversation_id, s.enabled, s.enabled_at_ms,
                s.config_json, s.permissions_json, c.snapshot_json, s.generation,
                (SELECT r.agent_run_id FROM agent_runs r WHERE r.origin_conversation_id=s.conversation_id
                 ORDER BY r.rowid DESC LIMIT 1)
         FROM agent_schedules s JOIN agent_conversations c ON c.conversation_id=s.conversation_id
         WHERE (?1 IS NULL OR s.workspace_root=?1) ORDER BY s.workspace_root, s.function_id")?;
    let rows = statement.query_map(
        [workspace.map(|p| p.to_string_lossy().into_owned())],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<Vec<u8>>>(8)?,
            ))
        },
    )?;
    rows.map(|row| {
        let (
            function,
            conversation,
            enabled,
            enabled_at_ms,
            config,
            permissions,
            snapshot,
            generation,
            last_run,
        ) = row?;
        let conversation_id = decode_optional_id(
            Some(conversation),
            "ConversationId",
            ConversationId::from_bytes,
        )?
        .ok_or_else(|| StoreError::NotFound {
            resource: "schedule Conversation".into(),
        })?;
        let config: FunctionSchedule = serde_json::from_str(&config)?;
        config.validate().map_err(|_| StoreError::InvalidValue {
            field: "schedule",
            value: conversation_id.to_string(),
            reason: "invalid stored schedule",
        })?;
        Ok(ScheduleRecord {
            function: serde_json::from_str(&function)?,
            conversation_id,
            enabled,
            enabled_at_ms,
            generation: agl_core::RequestId::parse(&generation).map_err(|_| {
                StoreError::InvalidValue {
                    field: "schedule generation",
                    value: generation,
                    reason: "invalid generation UUID",
                }
            })?,
            config,
            permissions: serde_json::from_str(&permissions)?,
            snapshot: serde_json::from_str(&snapshot)?,
            last_run: decode_optional_id(last_run, "AgentRunId", AgentRunId::from_bytes)?,
        })
    })
    .collect()
}

impl StoreHandle {
    pub(crate) fn enabled_schedule_keys(
        &self,
    ) -> crate::store::Result<Vec<(ConversationId, agl_core::RequestId)>> {
        self.read(|connection| {
            let mut stmt = connection.prepare("SELECT conversation_id,generation FROM agent_schedules WHERE enabled=1 ORDER BY workspace_root,function_id")?;
            let rows = stmt.query_map([], |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)))?;
            rows.map(|row| { let (id, generation) = row?;
                let generation = agl_core::RequestId::parse(&generation).map_err(|_| StoreError::InvalidValue { field: "schedule generation", value: generation, reason: "invalid generation UUID" })?;
                Ok((decode_optional_id(Some(id), "ConversationId", ConversationId::from_bytes)?
                    .ok_or_else(|| StoreError::NotFound { resource: "schedule Conversation".into() })?, generation))
            }).collect()
        })
    }

    pub(crate) fn schedules(
        &self,
        workspace: Option<&Path>,
    ) -> crate::store::Result<Vec<ScheduleRecord>> {
        self.read(|connection| records(connection, workspace))
    }

    pub(crate) fn enable_schedule(
        &self,
        function: &ExactPackageRef,
        snapshot: &AgentRunSnapshot,
        config: &FunctionSchedule,
        permissions: &serde_json::Value,
    ) -> crate::store::Result<ScheduleRecord> {
        config.validate().map_err(|_| StoreError::InvalidValue {
            field: "schedule",
            value: function.id.to_string(),
            reason: "invalid schedule",
        })?;
        let workspace = snapshot.workspace.root.as_path();
        if !workspace.is_absolute() || !workspace.is_dir() || workspace.canonicalize()? != workspace
        {
            return Err(StoreError::InvalidValue {
                field: "schedule workspace",
                value: workspace.display().to_string(),
                reason: "workspace must be a canonical directory",
            });
        }
        let now = unix_ms();
        if next_tick_after(now, config.interval_seconds, now).is_none() {
            return Err(StoreError::InvalidValue {
                field: "schedule.interval_seconds",
                value: config.interval_seconds.to_string(),
                reason: "first tick overflows UTC milliseconds",
            });
        }
        let store = self.lock()?;
        store.transaction(|tx| {
            let existing = records(tx, Some(workspace))?.into_iter().find(|r| r.function.id == function.id);
            if let Some(mut record) = existing
                && record.function == *function && record.snapshot == *snapshot
                && record.config == *config && record.permissions == *permissions {
                if !record.enabled {
                    record.generation = agl_core::RequestId::generate();
                    tx.execute("UPDATE agent_schedules SET enabled=1, enabled_at_ms=?3, generation=?4 WHERE workspace_root=?1 AND function_id=?2",
                        params![workspace.to_string_lossy(), function.id.as_str(), now, record.generation.as_str()])?;
                    record.enabled = true;
                    record.enabled_at_ms = now;
                }
                return Ok(record);
            }
            let conversation_id = ConversationId::generate();
            let generation = agl_core::RequestId::generate();
            tx.execute("INSERT INTO agent_conversations (conversation_id, display_name, function_json, workspace_root, snapshot_json, created_at_ms, last_active_at_ms)
                VALUES (?1,NULL,?2,?3,?4,?5,?5)", params![conversation_id.as_bytes().as_slice(), strict_json(function)?, workspace.to_string_lossy(), strict_json(snapshot)?, now])?;
            tx.execute("INSERT INTO agent_schedule_conversations(conversation_id) VALUES (?1)", [conversation_id.as_bytes().as_slice()])?;
            tx.execute("INSERT INTO agent_schedules (workspace_root,function_id,conversation_id,enabled,enabled_at_ms,config_json,permissions_json,generation)
                VALUES (?1,?2,?3,1,?4,?5,?6,?7) ON CONFLICT(workspace_root,function_id) DO UPDATE SET
                conversation_id=excluded.conversation_id,enabled=1,enabled_at_ms=excluded.enabled_at_ms,config_json=excluded.config_json,permissions_json=excluded.permissions_json,generation=excluded.generation",
                params![workspace.to_string_lossy(), function.id.as_str(), conversation_id.as_bytes().as_slice(), now, strict_json(config)?, strict_json(permissions)?, generation.as_str()])?;
            Ok(ScheduleRecord { function: function.clone(), conversation_id, enabled: true, enabled_at_ms: now, generation,
                config: config.clone(), permissions: permissions.clone(), snapshot: snapshot.clone(), last_run: None })
        })
    }

    pub(crate) fn disable_schedule(
        &self,
        workspace: &Path,
        function_id: &agl_core::package::PackageId,
    ) -> crate::store::Result<(ScheduleRecord, Vec<AgentRunId>)> {
        let store = self.lock()?;
        store.transaction(|tx| {
            let mut record = records(tx, Some(workspace))?
                .into_iter()
                .find(|r| r.function.id == *function_id)
                .ok_or_else(|| StoreError::NotFound {
                    resource: format!("schedule {function_id} in {}", workspace.display()),
                })?;
            tx.execute(
                "UPDATE agent_schedules SET enabled=0 WHERE workspace_root=?1 AND function_id=?2",
                params![workspace.to_string_lossy(), function_id.as_str()],
            )?;
            record.enabled = false;
            Ok((record, active_autonomous(tx, workspace, function_id)?))
        })
    }

    pub(crate) fn function_activity(
        &self,
        workspace: &Path,
        function_id: &agl_core::package::PackageId,
    ) -> crate::store::Result<(bool, Option<AgentRunId>)> {
        self.read(|connection| {
            let (busy, latest): (bool, Option<Vec<u8>>) = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_runs r JOIN agent_conversations c ON c.conversation_id=r.origin_conversation_id
                    WHERE c.workspace_root=?1 AND json_extract(c.function_json,'$.id')=?2 AND r.status IN ('pending','running')),
                    (SELECT r.agent_run_id FROM agent_runs r JOIN agent_conversations c ON c.conversation_id=r.origin_conversation_id
                    WHERE c.workspace_root=?1 AND json_extract(c.function_json,'$.id')=?2 ORDER BY r.rowid DESC LIMIT 1)",
                params![workspace.to_string_lossy(), function_id.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok((busy, decode_optional_id(latest, "AgentRunId", AgentRunId::from_bytes)?))
        })
    }
}

fn active_autonomous(
    connection: &rusqlite::Connection,
    workspace: &Path,
    function_id: &agl_core::package::PackageId,
) -> crate::store::Result<Vec<AgentRunId>> {
    let mut stmt = connection.prepare("SELECT r.agent_run_id FROM agent_runs r JOIN agent_conversations c ON c.conversation_id=r.origin_conversation_id
        WHERE c.workspace_root=?1 AND json_extract(c.function_json,'$.id')=?2 AND r.status IN ('pending','running') AND json_extract(r.origin_json,'$.type') IN ('timer','file_event')")?;
    let rows = stmt.query_map(
        params![workspace.to_string_lossy(), function_id.as_str()],
        |row| row.get::<_, Vec<u8>>(0),
    )?;
    rows.map(|row| {
        decode_optional_id(Some(row?), "AgentRunId", AgentRunId::from_bytes)?.ok_or_else(|| {
            StoreError::NotFound {
                resource: "schedule Run".into(),
            }
        })
    })
    .collect()
}

pub(super) fn validate_scheduled_origin(
    tx: &rusqlite::Transaction<'_>,
    spec: &AgentRunSpec,
    snapshot: &mut AgentRunSnapshot,
) -> crate::store::Result<()> {
    let conversation = spec.origin.conversation_id();
    if matches!(spec.origin, AgentRunOrigin::User { .. }) {
        let scheduled: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_schedule_conversations WHERE conversation_id=?1)",
            [conversation.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        if scheduled {
            return Err(StoreError::InvalidValue {
                field: "Run origin",
                value: conversation.to_string(),
                reason: "schedule Conversation cannot accept User messages",
            });
        }
        return Ok(());
    }
    let config: Option<String> = tx
        .query_row(
            "SELECT config_json FROM agent_schedules WHERE conversation_id=?1 AND enabled=1",
            [conversation.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    let config: FunctionSchedule =
        serde_json::from_str(&config.ok_or_else(|| StoreError::InvalidValue {
            field: "Run origin",
            value: conversation.to_string(),
            reason: "schedule must be explicitly enabled for this Conversation",
        })?)?;
    if spec.input.as_text() != config.prompt || spec.reasoning.is_some() {
        return Err(StoreError::InvalidValue {
            field: "scheduled task",
            value: conversation.to_string(),
            reason: "task and reasoning must match enabled schedule",
        });
    }
    if let AgentRunOrigin::FileEvent { paths, .. } = &spec.origin
        && (paths.is_empty()
            || paths.iter().any(|p| {
                !config
                    .events
                    .contains(&format!("file:{}", p.as_path().display()))
            }))
    {
        return Err(StoreError::InvalidValue {
            field: "scheduled event",
            value: conversation.to_string(),
            reason: "event paths must be declared by the enabled schedule",
        });
    }
    snapshot.limits.deadline_ms = (config.max_run_seconds * 1000) as i64;
    Ok(())
}
