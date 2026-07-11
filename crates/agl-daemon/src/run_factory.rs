use std::path::{Path, PathBuf};

use agl_chat::ChatSupervisorFactory;
use agl_store::{AglStore, DurableRunRecord, EffectDeliveryClass, RunKind, RunState, RunUsage};
use agl_supervisor::{
    DriverEffectError, DriverSnapshot, DurableRunDriver, DurableRunDriverFactory,
    EffectExecutionContext, Result, RunCancellation, SupervisorEffect, SupervisorError,
    SupervisorTerminal,
};
use serde::{Deserialize, Serialize};

use agl_cron::STORE_STATUS_BUILTIN_CRON_TARGET;

#[derive(Clone)]
pub(crate) struct DaemonRunFactory {
    chat: ChatSupervisorFactory,
    store_root: PathBuf,
}

impl DaemonRunFactory {
    pub(crate) fn new(chat: ChatSupervisorFactory, store_root: impl AsRef<Path>) -> Self {
        Self {
            chat,
            store_root: store_root.as_ref().to_path_buf(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuiltinCronRunInput {
    pub(crate) builtin: String,
}

impl DurableRunDriverFactory for DaemonRunFactory {
    fn open(
        &self,
        run: &DurableRunRecord,
        cancellation: RunCancellation,
    ) -> Result<Box<dyn DurableRunDriver>> {
        if run.kind == RunKind::Cron
            && let Ok(input) = serde_json::from_value::<BuiltinCronRunInput>(run.input.clone())
        {
            if input.builtin != STORE_STATUS_BUILTIN_CRON_TARGET {
                return Err(SupervisorError::Driver(format!(
                    "unsupported builtin cron target {}",
                    input.builtin
                )));
            }
            let completed = run
                .checkpoint
                .as_ref()
                .and_then(|checkpoint| checkpoint.get("completed"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            return Ok(Box::new(StoreStatusDriver {
                store_root: self.store_root.clone(),
                completed,
                result: None,
                cancellation,
            }));
        }
        self.chat.open(run, cancellation)
    }
}

struct StoreStatusDriver {
    store_root: PathBuf,
    completed: bool,
    result: Option<serde_json::Value>,
    cancellation: RunCancellation,
}

impl DurableRunDriver for StoreStatusDriver {
    fn snapshot(&mut self) -> Result<DriverSnapshot> {
        let terminal = self.completed.then(|| SupervisorTerminal {
            state: RunState::Succeeded,
            result: self.result.clone(),
            error_code: None,
            error_message: None,
        });
        Ok(DriverSnapshot {
            checkpoint: serde_json::json!({ "completed": self.completed }),
            pending_effect: (!self.completed).then(|| SupervisorEffect {
                sequence: 1,
                kind: "store_status".to_string(),
                delivery_class: EffectDeliveryClass::ReplaySafe,
                request: serde_json::json!({ "store_root": "private" }),
            }),
            events: Vec::new(),
            terminal,
            usage: RunUsage::default(),
        })
    }

    fn execute_pending_effect(
        &mut self,
        _context: &EffectExecutionContext,
    ) -> std::result::Result<serde_json::Value, DriverEffectError> {
        if self.cancellation.is_cancelled() {
            return Err(DriverEffectError::new(
                "cron.cancelled",
                "cron run was cancelled",
                false,
            ));
        }
        let store = AglStore::open_current_read_only_at(&self.store_root)
            .map_err(|error| DriverEffectError::new("store.open", error.to_string(), true))?;
        let health = store
            .health()
            .map_err(|error| DriverEffectError::new("store.status", error.to_string(), true))?;
        let result = serde_json::json!({
            "status": "succeeded",
            "target": STORE_STATUS_BUILTIN_CRON_TARGET,
            "schema_version": health.migration_version,
        });
        self.completed = true;
        self.result = Some(result.clone());
        Ok(result)
    }
}
