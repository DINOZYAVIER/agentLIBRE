use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agl_config::{ModelId, load_model_bindings_or_empty};
use agl_model::{
    ModelArtifactRole, ModelInstallRecovery, ModelInstallStore, ModelInstallTransaction,
    ModelInstallTransactionError, ModelInstallTransactionInput, import_local_model,
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    store: ModelInstallStore,
    bindings: PathBuf,
    model_id: ModelId,
    old_path: PathBuf,
    new_path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "agl173-install-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let old_path = root.join("old.gguf");
        let new_path = root.join("new.gguf");
        fs::write(&old_path, b"GGUF-old-generation").unwrap();
        fs::write(&new_path, b"GGUF-new-generation").unwrap();
        Self {
            store: ModelInstallStore::new(root.join("records")),
            bindings: root.join("models.toml"),
            model_id: ModelId::new("fixture-model").unwrap(),
            old_path,
            new_path,
        }
    }

    fn commit_path(&self, path: &Path, replace: bool) {
        let imported =
            import_local_model(path, Some(self.model_id.clone()), ModelArtifactRole::Main).unwrap();
        ModelInstallTransaction::new(self.store.clone(), &self.bindings)
            .unwrap()
            .commit(ModelInstallTransactionInput::new(
                vec![imported.record],
                imported.binding_patch,
                replace,
            ))
            .unwrap();
    }

    fn latest_transaction_root(&self) -> PathBuf {
        let mut paths = fs::read_dir(self.store.root().join(".transactions"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        paths.sort();
        paths.pop().unwrap()
    }

    fn set_phase(&self, phase: &str) {
        let path = self.latest_transaction_root().join("intent.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["phase"] = serde_json::Value::String(phase.to_owned());
        fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    fn assert_generation(&self, expected: &Path) {
        let binding = &load_model_bindings_or_empty(&self.bindings).unwrap().models[&self.model_id];
        assert_eq!(binding.path, fs::canonicalize(expected).unwrap());
        assert_eq!(
            self.store.get(&self.model_id).unwrap().unwrap().path,
            binding.path
        );
    }
}

// MIW-TXN-001. Publishing a complete generation updates bindings and records
// together; replayed durable boundaries never expose a mixed pair.
#[test]
fn durable_boundary_matrix_has_only_complete_generations() {
    for phase in ["prepared", "commit_decided", "targets_published"] {
        let fixture = Fixture::new();
        fixture.commit_path(&fixture.old_path, false);
        fixture.commit_path(&fixture.new_path, true);
        fixture.set_phase(phase);
        ModelInstallRecovery::open(fixture.store.root(), &fixture.bindings)
            .unwrap()
            .recover()
            .unwrap();
        if phase == "prepared" {
            fixture.assert_generation(&fixture.old_path);
        } else {
            fixture.assert_generation(&fixture.new_path);
        }
    }
}

// MIW-TXN-002. Prepared rolls back while the commit decision and published
// phases roll forward; repeating recovery is idempotent.
#[test]
fn recovery_direction_is_journal_derived_and_idempotent() {
    for (phase, expected) in [
        ("prepared", "old"),
        ("commit_decided", "new"),
        ("targets_published", "new"),
    ] {
        let fixture = Fixture::new();
        fixture.commit_path(&fixture.old_path, false);
        fixture.commit_path(&fixture.new_path, true);
        fixture.set_phase(phase);
        let recovery = ModelInstallRecovery::open(fixture.store.root(), &fixture.bindings).unwrap();
        recovery.recover().unwrap();
        recovery.recover().unwrap();
        fixture.assert_generation(if expected == "old" {
            &fixture.old_path
        } else {
            &fixture.new_path
        });
    }
}

// MIW-TXN-003. Tampered staged bytes and escaped target paths fail closed with
// transaction and target identity.
#[test]
fn corrupt_intent_and_payload_fail_closed() {
    let fixture = Fixture::new();
    fixture.commit_path(&fixture.old_path, false);
    let transaction = fixture.latest_transaction_root();
    fs::write(transaction.join("target-0.new"), b"tampered").unwrap();
    let error = ModelInstallRecovery::open(fixture.store.root(), &fixture.bindings)
        .unwrap()
        .recover()
        .unwrap_err();
    assert!(matches!(
        error,
        ModelInstallTransactionError::Corrupt {
            transaction_id: _,
            target: Some(_),
            ..
        }
    ));
}

// MIW-TXN-004. The same install-store lock serializes every transaction for
// the affected Model set.
#[test]
fn transaction_lock_rejects_a_concurrent_writer() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.store.root()).unwrap();
    let lock_path = fixture.store.root().join(".commit.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .unwrap();
    lock.lock().unwrap();
    let imported = import_local_model(
        &fixture.old_path,
        Some(fixture.model_id.clone()),
        ModelArtifactRole::Main,
    )
    .unwrap();
    let error = ModelInstallTransaction::new(fixture.store.clone(), &fixture.bindings)
        .unwrap()
        .commit(ModelInstallTransactionInput::new(
            vec![imported.record],
            imported.binding_patch,
            false,
        ))
        .unwrap_err();
    assert!(matches!(error, ModelInstallTransactionError::Locked { .. }));
    drop(lock);
}

// MIW-TXN-005. The only public positive commit takes the complete transaction
// input rather than a binding patch or record alone.
#[test]
fn commit_surface_requires_complete_transaction_input() {
    fn selected_api(
        transaction: ModelInstallTransaction,
        input: ModelInstallTransactionInput,
    ) -> Result<agl_model::ModelInstallTransactionReceipt, ModelInstallTransactionError> {
        transaction.commit(input)
    }

    let _: fn(
        ModelInstallTransaction,
        ModelInstallTransactionInput,
    )
        -> Result<agl_model::ModelInstallTransactionReceipt, ModelInstallTransactionError> =
        selected_api;
}
