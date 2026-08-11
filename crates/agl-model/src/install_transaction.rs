use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_config::{ModelId, load_model_bindings_or_empty};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{ModelBindingPatch, ModelInstallRecord, ModelInstallStore};

const INSTALL_TRANSACTION_SCHEMA: &str = "agentlibre.model-install-transaction/v1";
static NEXT_TRANSACTION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelInstallTransactionPhase {
    Validated,
    Prepared,
    CommitDecided,
    TargetsPublished,
    Committed,
}

#[derive(Clone, Debug)]
pub struct ModelInstallTransactionInput {
    records: Vec<ModelInstallRecord>,
    binding_patch: ModelBindingPatch,
    remove_bindings: Vec<ModelId>,
    delete_records: Vec<ModelId>,
    replace: bool,
}

impl ModelInstallTransactionInput {
    pub fn new(
        records: Vec<ModelInstallRecord>,
        binding_patch: ModelBindingPatch,
        replace: bool,
    ) -> Self {
        Self {
            records,
            binding_patch,
            remove_bindings: Vec::new(),
            delete_records: Vec::new(),
            replace,
        }
    }

    pub fn unbind(model_ids: Vec<ModelId>) -> Self {
        Self {
            records: Vec::new(),
            binding_patch: ModelBindingPatch::default(),
            remove_bindings: model_ids,
            delete_records: Vec::new(),
            replace: false,
        }
    }

    pub fn update_records(records: Vec<ModelInstallRecord>) -> Self {
        Self {
            records,
            binding_patch: ModelBindingPatch::default(),
            remove_bindings: Vec::new(),
            delete_records: Vec::new(),
            replace: true,
        }
    }

    pub fn delete_records(model_ids: Vec<ModelId>) -> Self {
        Self {
            records: Vec::new(),
            binding_patch: ModelBindingPatch::default(),
            remove_bindings: Vec::new(),
            delete_records: model_ids,
            replace: false,
        }
    }

    pub fn records(&self) -> &[ModelInstallRecord] {
        &self.records
    }

    pub fn binding_patch(&self) -> &ModelBindingPatch {
        &self.binding_patch
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelInstallTransactionReceipt {
    transaction_id: String,
    affected_models: Vec<ModelId>,
}

impl ModelInstallTransactionReceipt {
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    pub fn affected_models(&self) -> &[ModelId] {
        &self.affected_models
    }
}

#[derive(Debug, Error)]
pub enum ModelInstallTransactionError {
    #[error("model install transaction input is invalid: {reason}")]
    InvalidInput { reason: String },
    #[error("model install transaction `{transaction_id}` is locked for {affected_models:?}")]
    Locked {
        transaction_id: String,
        affected_models: Vec<ModelId>,
    },
    #[error(
        "model install transaction `{transaction_id}` is corrupt{target_text}: {reason}",
        target_text = target.as_ref().map(|value| format!(" at {}", value.display())).unwrap_or_default()
    )]
    Corrupt {
        transaction_id: String,
        target: Option<PathBuf>,
        reason: String,
    },
    #[error(
        "model install recovery is required for `{transaction_id}` in phase {phase:?}: {reason}"
    )]
    RecoveryRequired {
        transaction_id: String,
        phase: ModelInstallTransactionPhase,
        reason: String,
    },
    #[error("model install transaction I/O failed at {}: {reason}", path.display())]
    Io { path: PathBuf, reason: String },
}

#[derive(Clone, Debug)]
pub struct ModelInstallTransaction {
    store: ModelInstallStore,
    bindings_path: PathBuf,
}

impl ModelInstallTransaction {
    pub fn new(
        store: ModelInstallStore,
        bindings_path: impl Into<PathBuf>,
    ) -> Result<Self, ModelInstallTransactionError> {
        let bindings_path = bindings_path.into();
        validate_target_path(&bindings_path)?;
        Ok(Self {
            store,
            bindings_path,
        })
    }

    pub fn commit(
        self,
        input: ModelInstallTransactionInput,
    ) -> Result<ModelInstallTransactionReceipt, ModelInstallTransactionError> {
        fs::create_dir_all(self.store.root()).map_err(|error| io(self.store.root(), error))?;
        let lock_path = self.store.root().join(".commit.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| io(&lock_path, error))?;
        lock.try_lock()
            .map_err(|_| ModelInstallTransactionError::Locked {
                transaction_id: "active".to_owned(),
                affected_models: affected_models(&input),
            })?;
        ModelInstallRecovery::new(self.store.root(), Some(self.bindings_path.clone()))?
            .recover()?;
        let prepared = PreparedTransaction::prepare(&self, input)?;
        prepared.commit()
    }
}

#[derive(Clone, Debug)]
pub struct ModelInstallRecovery {
    store_root: PathBuf,
    expected_bindings_path: Option<PathBuf>,
}

impl ModelInstallRecovery {
    pub fn open(
        store_root: impl Into<PathBuf>,
        bindings_path: impl Into<PathBuf>,
    ) -> Result<Self, ModelInstallTransactionError> {
        Self::new(store_root, Some(bindings_path.into()))
    }

    fn new(
        store_root: impl Into<PathBuf>,
        expected_bindings_path: Option<PathBuf>,
    ) -> Result<Self, ModelInstallTransactionError> {
        let store_root = store_root.into();
        validate_target_path(&store_root)?;
        Ok(Self {
            store_root,
            expected_bindings_path,
        })
    }

    pub fn recover(
        &self,
    ) -> Result<Vec<ModelInstallTransactionReceipt>, ModelInstallTransactionError> {
        let transactions_root = self.store_root.join(".transactions");
        if !transactions_root.exists() {
            return Ok(Vec::new());
        }
        let mut paths = fs::read_dir(&transactions_root)
            .map_err(|error| io(&transactions_root, error))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| io(&transactions_root, error))?;
        paths.sort();
        let mut recovered = Vec::new();
        for path in paths {
            if !path.is_dir() {
                continue;
            }
            let intent = read_intent(&path)?;
            validate_intent(
                &intent,
                &path,
                &self.store_root,
                self.expected_bindings_path.as_deref(),
            )?;
            match intent.phase {
                ModelInstallTransactionPhase::Validated
                | ModelInstallTransactionPhase::Prepared => {
                    restore_old_generation(&intent, &path)?;
                    write_phase(&path, &intent, ModelInstallTransactionPhase::Committed)?;
                }
                ModelInstallTransactionPhase::CommitDecided
                | ModelInstallTransactionPhase::TargetsPublished => {
                    publish_new_generation(&intent, &path)?;
                    write_phase(&path, &intent, ModelInstallTransactionPhase::Committed)?;
                }
                ModelInstallTransactionPhase::Committed => {}
            }
            recovered.push(ModelInstallTransactionReceipt {
                transaction_id: intent.transaction_id,
                affected_models: intent.affected_models,
            });
        }
        Ok(recovered)
    }
}

struct PreparedTransaction {
    root: PathBuf,
    intent: TransactionIntent,
}

impl PreparedTransaction {
    fn prepare(
        transaction: &ModelInstallTransaction,
        input: ModelInstallTransactionInput,
    ) -> Result<Self, ModelInstallTransactionError> {
        validate_input(&input)?;
        let transaction_id = new_transaction_id()?;
        let root = transaction
            .store
            .root()
            .join(".transactions")
            .join(&transaction_id);
        fs::create_dir_all(&root).map_err(|error| io(&root, error))?;

        let mut targets = Vec::with_capacity(input.records.len() + input.delete_records.len() + 1);
        if !input.binding_patch.models.is_empty() || !input.remove_bindings.is_empty() {
            let mut bindings = load_model_bindings_or_empty(&transaction.bindings_path)
                .map_err(|error| invalid(error.to_string()))?;
            for model_id in &input.remove_bindings {
                bindings.models.remove(model_id);
            }
            input
                .binding_patch
                .merge_into(&mut bindings, input.replace)
                .map_err(|error| invalid(error.to_string()))?;
            targets.push(stage_target(
                &root,
                targets.len(),
                transaction.bindings_path.clone(),
                Some(
                    toml::to_string_pretty(&bindings)
                        .map_err(|error| invalid(error.to_string()))?
                        .into_bytes(),
                ),
            )?);
        }
        for record in &input.records {
            targets.push(stage_target(
                &root,
                targets.len(),
                transaction.store.record_path(&record.model_id),
                Some(
                    serde_json::to_vec_pretty(record)
                        .map_err(|error| invalid(error.to_string()))?,
                ),
            )?);
        }
        for model_id in &input.delete_records {
            targets.push(stage_target(
                &root,
                targets.len(),
                transaction.store.record_path(model_id),
                None,
            )?);
        }
        let mut intent = TransactionIntent {
            schema: INSTALL_TRANSACTION_SCHEMA.to_owned(),
            transaction_id: transaction_id.clone(),
            phase: ModelInstallTransactionPhase::Validated,
            store_root: transaction.store.root().to_path_buf(),
            bindings_path: transaction.bindings_path.clone(),
            affected_models: affected_models(&input),
            targets,
        };
        write_intent(&root, &intent)?;
        intent.phase = ModelInstallTransactionPhase::Prepared;
        write_intent(&root, &intent)?;
        Ok(Self { root, intent })
    }

    fn commit(mut self) -> Result<ModelInstallTransactionReceipt, ModelInstallTransactionError> {
        self.intent.phase = ModelInstallTransactionPhase::CommitDecided;
        write_intent(&self.root, &self.intent)?;
        if let Err(error) = publish_new_generation(&self.intent, &self.root) {
            return Err(ModelInstallTransactionError::RecoveryRequired {
                transaction_id: self.intent.transaction_id,
                phase: ModelInstallTransactionPhase::CommitDecided,
                reason: error.to_string(),
            });
        }
        self.intent.phase = ModelInstallTransactionPhase::TargetsPublished;
        write_intent(&self.root, &self.intent)?;
        self.intent.phase = ModelInstallTransactionPhase::Committed;
        write_intent(&self.root, &self.intent)?;
        Ok(ModelInstallTransactionReceipt {
            transaction_id: self.intent.transaction_id,
            affected_models: self.intent.affected_models,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransactionIntent {
    schema: String,
    transaction_id: String,
    phase: ModelInstallTransactionPhase,
    store_root: PathBuf,
    bindings_path: PathBuf,
    affected_models: Vec<ModelId>,
    targets: Vec<TransactionTarget>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransactionTarget {
    target: PathBuf,
    staged_name: Option<String>,
    staged_digest: Option<String>,
    backup_name: Option<String>,
    backup_digest: Option<String>,
}

fn validate_input(
    input: &ModelInstallTransactionInput,
) -> Result<(), ModelInstallTransactionError> {
    if input.records.is_empty()
        && input.binding_patch.models.is_empty()
        && input.remove_bindings.is_empty()
        && input.delete_records.is_empty()
    {
        return Err(invalid("transaction has no mutations"));
    }
    let records = input
        .records
        .iter()
        .map(|record| (record.model_id.clone(), record))
        .collect::<BTreeMap<_, _>>();
    if records.len() != input.records.len() {
        return Err(invalid("transaction contains duplicate Model records"));
    }
    for (model_id, record) in &records {
        record
            .validate()
            .map_err(|error| invalid(error.to_string()))?;
        if let Some(binding) = input.binding_patch.models.get(model_id)
            && binding.path != record.path
        {
            return Err(invalid(format!(
                "record `{model_id}` and binding path differ"
            )));
        }
    }
    for model_id in input.binding_patch.models.keys() {
        if !records.contains_key(model_id) {
            return Err(invalid(format!(
                "binding `{model_id}` has no install record"
            )));
        }
    }
    let removed = input
        .remove_bindings
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    if removed.len() != input.remove_bindings.len() {
        return Err(invalid("transaction contains duplicate binding removals"));
    }
    let deleted = input
        .delete_records
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    if deleted.len() != input.delete_records.len() {
        return Err(invalid("transaction contains duplicate record deletions"));
    }
    if let Some(model_id) = records.keys().find(|model_id| deleted.contains(model_id)) {
        return Err(invalid(format!(
            "record `{model_id}` cannot be written and deleted in one transaction"
        )));
    }
    Ok(())
}

fn affected_models(input: &ModelInstallTransactionInput) -> Vec<ModelId> {
    input
        .records
        .iter()
        .map(|record| record.model_id.clone())
        .chain(input.binding_patch.models.keys().cloned())
        .chain(input.remove_bindings.iter().cloned())
        .chain(input.delete_records.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn stage_target(
    root: &Path,
    index: usize,
    target: PathBuf,
    bytes: Option<Vec<u8>>,
) -> Result<TransactionTarget, ModelInstallTransactionError> {
    validate_target_path(&target)?;
    let (staged_name, staged_digest) = if let Some(bytes) = bytes {
        let name = format!("target-{index}.new");
        write_synced(&root.join(&name), &bytes)?;
        (Some(name), Some(digest(&bytes)))
    } else {
        (None, None)
    };
    let (backup_name, backup_digest) = if target.exists() {
        let old = fs::read(&target).map_err(|error| io(&target, error))?;
        let name = format!("target-{index}.old");
        write_synced(&root.join(&name), &old)?;
        (Some(name), Some(digest(&old)))
    } else {
        (None, None)
    };
    Ok(TransactionTarget {
        target,
        staged_name,
        staged_digest,
        backup_name,
        backup_digest,
    })
}

fn publish_new_generation(
    intent: &TransactionIntent,
    root: &Path,
) -> Result<(), ModelInstallTransactionError> {
    for target in &intent.targets {
        match (&target.staged_name, &target.staged_digest) {
            (Some(name), Some(expected)) => {
                let staged = checked_payload(root, name, expected, intent)?;
                atomic_publish(&target.target, &staged)?;
            }
            (None, None) if target.target.exists() => {
                fs::remove_file(&target.target).map_err(|error| io(&target.target, error))?;
                sync_parent(&target.target)?;
            }
            (None, None) => {}
            _ => {
                return Err(corrupt(
                    intent,
                    Some(target.target.clone()),
                    "staged name/digest cardinality differs",
                ));
            }
        }
    }
    Ok(())
}

fn restore_old_generation(
    intent: &TransactionIntent,
    root: &Path,
) -> Result<(), ModelInstallTransactionError> {
    for target in &intent.targets {
        match (&target.backup_name, &target.backup_digest) {
            (Some(name), Some(expected)) => {
                let backup = checked_payload(root, name, expected, intent)?;
                atomic_publish(&target.target, &backup)?;
            }
            (None, None) if target.target.exists() => {
                fs::remove_file(&target.target).map_err(|error| io(&target.target, error))?;
                sync_parent(&target.target)?;
            }
            (None, None) => {}
            _ => {
                return Err(corrupt(
                    intent,
                    Some(target.target.clone()),
                    "backup name/digest cardinality differs",
                ));
            }
        }
    }
    Ok(())
}

fn validate_intent(
    intent: &TransactionIntent,
    root: &Path,
    store_root: &Path,
    expected_bindings_path: Option<&Path>,
) -> Result<(), ModelInstallTransactionError> {
    if intent.schema != INSTALL_TRANSACTION_SCHEMA
        || root.file_name().and_then(|value| value.to_str()) != Some(&intent.transaction_id)
        || intent.store_root != store_root
    {
        return Err(corrupt(intent, None, "identity or schema mismatch"));
    }
    if let Some(expected) = expected_bindings_path
        && intent.bindings_path != expected
    {
        return Err(corrupt(
            intent,
            Some(intent.bindings_path.clone()),
            "bindings target differs from configured target",
        ));
    }
    let record_root = store_root;
    for target in &intent.targets {
        validate_target_path(&target.target)?;
        if target.target != intent.bindings_path && target.target.parent() != Some(record_root) {
            return Err(corrupt(
                intent,
                Some(target.target.clone()),
                "record target escapes install store",
            ));
        }
        match (&target.staged_name, &target.staged_digest) {
            (Some(name), Some(expected)) => {
                checked_payload(root, name, expected, intent)?;
            }
            (None, None) => {}
            _ => {
                return Err(corrupt(
                    intent,
                    Some(target.target.clone()),
                    "staged name/digest cardinality differs",
                ));
            }
        }
        match (&target.backup_name, &target.backup_digest) {
            (Some(name), Some(expected)) => {
                checked_payload(root, name, expected, intent)?;
            }
            (None, None) => {}
            _ => {
                return Err(corrupt(
                    intent,
                    Some(target.target.clone()),
                    "backup name/digest cardinality differs",
                ));
            }
        }
    }
    Ok(())
}

fn checked_payload(
    root: &Path,
    name: &str,
    expected: &str,
    intent: &TransactionIntent,
) -> Result<Vec<u8>, ModelInstallTransactionError> {
    if Path::new(name).components().count() != 1 {
        return Err(corrupt(
            intent,
            Some(root.join(name)),
            "payload path escapes journal",
        ));
    }
    let path = root.join(name);
    let bytes = fs::read(&path).map_err(|error| io(&path, error))?;
    if digest(&bytes) != expected {
        return Err(corrupt(intent, Some(path), "payload digest mismatch"));
    }
    Ok(bytes)
}

fn read_intent(root: &Path) -> Result<TransactionIntent, ModelInstallTransactionError> {
    let path = root.join("intent.json");
    let bytes = fs::read(&path).map_err(|error| io(&path, error))?;
    serde_json::from_slice(&bytes).map_err(|error| ModelInstallTransactionError::Corrupt {
        transaction_id: root
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        target: Some(path),
        reason: error.to_string(),
    })
}

fn write_phase(
    root: &Path,
    intent: &TransactionIntent,
    phase: ModelInstallTransactionPhase,
) -> Result<(), ModelInstallTransactionError> {
    let mut next = intent.clone();
    next.phase = phase;
    write_intent(root, &next)
}

fn write_intent(
    root: &Path,
    intent: &TransactionIntent,
) -> Result<(), ModelInstallTransactionError> {
    let bytes = serde_json::to_vec_pretty(intent).map_err(|error| invalid(error.to_string()))?;
    atomic_publish(&root.join("intent.json"), &bytes)
}

fn atomic_publish(path: &Path, bytes: &[u8]) -> Result<(), ModelInstallTransactionError> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("target has no parent"))?;
    fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
    let temporary = parent.join(format!(
        ".{}.agl-install-tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    write_synced(&temporary, bytes)?;
    fs::rename(&temporary, path).map_err(|error| io(path, error))?;
    sync_parent(path)
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), ModelInstallTransactionError> {
    let mut file = File::create(path).map_err(|error| io(path, error))?;
    file.write_all(bytes).map_err(|error| io(path, error))?;
    file.sync_all().map_err(|error| io(path, error))
}

fn sync_parent(path: &Path) -> Result<(), ModelInstallTransactionError> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("target has no parent"))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io(parent, error))
}

fn validate_target_path(path: &Path) -> Result<(), ModelInstallTransactionError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(invalid(format!(
            "unsafe transaction path {}",
            path.display()
        )));
    }
    Ok(())
}

fn new_transaction_id() -> Result<String, ModelInstallTransactionError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| invalid(error.to_string()))?
        .as_nanos();
    Ok(format!(
        "model-install-{now:032x}-{:016x}",
        NEXT_TRANSACTION_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

fn digest(bytes: &[u8]) -> String {
    let value = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(71);
    encoded.push_str("sha256:");
    for byte in value {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn invalid(reason: impl Into<String>) -> ModelInstallTransactionError {
    ModelInstallTransactionError::InvalidInput {
        reason: reason.into(),
    }
}

fn corrupt(
    intent: &TransactionIntent,
    target: Option<PathBuf>,
    reason: impl Into<String>,
) -> ModelInstallTransactionError {
    ModelInstallTransactionError::Corrupt {
        transaction_id: intent.transaction_id.clone(),
        target,
        reason: reason.into(),
    }
}

fn io(path: impl AsRef<Path>, error: std::io::Error) -> ModelInstallTransactionError {
    ModelInstallTransactionError::Io {
        path: path.as_ref().to_path_buf(),
        reason: error.to_string(),
    }
}
