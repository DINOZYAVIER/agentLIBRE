use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::install::atomic_write;
use crate::{ModelArtifactRole, ModelPackageId};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupPhase {
    Confirmed,
    ArtifactsReady,
    WorkspaceReady,
    BindingsStaged,
    SmokePassed,
    Committed,
}

impl SetupPhase {
    const ORDERED: [Self; 6] = [
        Self::Confirmed,
        Self::ArtifactsReady,
        Self::WorkspaceReady,
        Self::BindingsStaged,
        Self::SmokePassed,
        Self::Committed,
    ];
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedArtifactRole {
    pub role: ModelArtifactRole,
    pub model_id: agl_config::ModelId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupCheckpoint {
    pub version: u32,
    pub workspace_identity: PathBuf,
    pub workspace_digest: String,
    pub package_id: ModelPackageId,
    pub planned_artifacts: Vec<PlannedArtifactRole>,
    pub low_memory_consent: bool,
    pub plan_hash: String,
    pub cpu_fallback_consent_plan_hash: Option<String>,
    pub completed_phases: Vec<SetupPhase>,
}

impl SetupCheckpoint {
    pub fn new(
        workspace: impl AsRef<Path>,
        package_id: ModelPackageId,
        planned_artifacts: Vec<PlannedArtifactRole>,
        low_memory_consent: bool,
        plan_hash: String,
    ) -> Result<Self> {
        let workspace_identity = std::fs::canonicalize(workspace.as_ref()).with_context(|| {
            format!(
                "failed to canonicalize setup workspace {}",
                workspace.as_ref().display()
            )
        })?;
        let workspace_digest = canonical_workspace_digest(&workspace_identity)?;
        let checkpoint = Self {
            version: 1,
            workspace_identity,
            workspace_digest,
            package_id,
            planned_artifacts,
            low_memory_consent,
            plan_hash,
            cpu_fallback_consent_plan_hash: None,
            completed_phases: Vec::new(),
        };
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported setup checkpoint version");
        ensure!(
            self.workspace_identity.is_absolute(),
            "setup workspace identity must be absolute"
        );
        validate_digest("workspace_digest", &self.workspace_digest)?;
        ensure!(
            self.workspace_digest == workspace_identity_digest(&self.workspace_identity),
            "setup workspace digest does not match its recorded identity"
        );
        validate_digest("plan_hash", &self.plan_hash)?;
        if let Some(consent) = &self.cpu_fallback_consent_plan_hash {
            validate_digest("cpu_fallback_consent_plan_hash", consent)?;
            ensure!(
                consent == &self.plan_hash,
                "CPU fallback consent must match the current setup plan"
            );
        }
        ensure!(
            !self.planned_artifacts.is_empty(),
            "setup checkpoint has no planned artifacts"
        );
        let mut model_ids = std::collections::BTreeSet::new();
        ensure!(
            self.planned_artifacts
                .iter()
                .any(|artifact| artifact.role == ModelArtifactRole::Main),
            "setup checkpoint has no main artifact"
        );
        for artifact in &self.planned_artifacts {
            ensure!(
                model_ids.insert(&artifact.model_id),
                "setup checkpoint contains duplicate model ids"
            );
        }
        ensure!(
            self.completed_phases.len() <= SetupPhase::ORDERED.len(),
            "setup checkpoint contains too many completed phases"
        );
        ensure!(
            self.completed_phases == SetupPhase::ORDERED[..self.completed_phases.len()],
            "setup checkpoint phases must be a completed prefix"
        );
        Ok(())
    }

    pub fn completed(&self, phase: SetupPhase) -> bool {
        self.completed_phases.contains(&phase)
    }

    pub fn advance(&mut self, phase: SetupPhase) -> Result<()> {
        if self.completed(phase) {
            return Ok(());
        }
        let next = SetupPhase::ORDERED
            .get(self.completed_phases.len())
            .copied()
            .context("setup checkpoint is already complete")?;
        ensure!(
            next == phase,
            "cannot complete setup phase {phase:?}; next phase is {next:?}"
        );
        self.completed_phases.push(phase);
        Ok(())
    }

    pub fn consent_to_cpu_fallback(&mut self) {
        self.cpu_fallback_consent_plan_hash = Some(self.plan_hash.clone());
    }
}

#[derive(Clone, Debug)]
pub struct SetupCheckpointStore {
    root: PathBuf,
}

impl SetupCheckpointStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path_for_workspace(&self, workspace: impl AsRef<Path>) -> Result<PathBuf> {
        let digest = canonical_workspace_digest(workspace)?;
        Ok(self.root.join(format!("{digest}.json")))
    }

    pub fn workspace_state_dir(&self, workspace: impl AsRef<Path>) -> Result<PathBuf> {
        let digest = canonical_workspace_digest(workspace)?;
        Ok(self.root.join(digest))
    }

    pub fn staged_bindings_path(&self, workspace: impl AsRef<Path>) -> Result<PathBuf> {
        Ok(self
            .workspace_state_dir(workspace)?
            .join(agl_config::MODEL_BINDINGS_FILE_NAME))
    }

    pub fn load(&self, workspace: impl AsRef<Path>) -> Result<Option<SetupCheckpoint>> {
        let workspace = std::fs::canonicalize(workspace.as_ref()).with_context(|| {
            format!(
                "failed to canonicalize setup workspace {}",
                workspace.as_ref().display()
            )
        })?;
        let digest = workspace_identity_digest(&workspace);
        let path = self.root.join(format!("{digest}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let checkpoint: SetupCheckpoint = serde_json::from_slice(&std::fs::read(&path)?)
            .with_context(|| format!("failed to parse setup checkpoint {}", path.display()))?;
        checkpoint.validate()?;
        ensure!(
            checkpoint.workspace_identity == workspace && checkpoint.workspace_digest == digest,
            "setup checkpoint {} belongs to a different workspace",
            path.display()
        );
        Ok(Some(checkpoint))
    }

    pub fn list(&self) -> Result<Vec<SetupCheckpoint>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut paths = entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.sort();
        let mut checkpoints = Vec::new();
        for path in paths {
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let checkpoint: SetupCheckpoint = serde_json::from_slice(&std::fs::read(&path)?)
                .with_context(|| format!("failed to parse setup checkpoint {}", path.display()))?;
            checkpoint.validate()?;
            let expected_name = format!("{}.json", checkpoint.workspace_digest);
            ensure!(
                path.file_name().and_then(|value| value.to_str()) == Some(expected_name.as_str()),
                "setup checkpoint filename does not match its workspace digest: {}",
                path.display()
            );
            checkpoints.push(checkpoint);
        }
        Ok(checkpoints)
    }

    pub fn save(&self, checkpoint: &SetupCheckpoint) -> Result<()> {
        checkpoint.validate()?;
        let path = self
            .root
            .join(format!("{}.json", checkpoint.workspace_digest));
        atomic_write(&path, &serde_json::to_vec_pretty(checkpoint)?)
    }

    pub fn remove(&self, workspace: impl AsRef<Path>) -> Result<()> {
        let path = self.path_for_workspace(workspace)?;
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to remove setup checkpoint {}", path.display()))?;
        }
        if let Ok(directory) = std::fs::File::open(&self.root) {
            let _ = directory.sync_all();
        }
        Ok(())
    }
}

pub fn setup_plan_hash(value: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value).context("failed to serialize setup plan fingerprint")?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(hex(&hasher.finalize()))
}

pub fn canonical_workspace_digest(workspace: impl AsRef<Path>) -> Result<String> {
    let canonical = std::fs::canonicalize(workspace.as_ref()).with_context(|| {
        format!(
            "failed to canonicalize workspace {}",
            workspace.as_ref().display()
        )
    })?;
    Ok(workspace_identity_digest(&canonical))
}

fn workspace_identity_digest(workspace: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(workspace.as_os_str().as_encoded_bytes());
    hex(&hasher.finalize())
}

fn validate_digest(name: &str, value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "setup checkpoint {name} must be a lowercase SHA-256"
    );
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn workspace() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agl-models-checkpoint-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn phases_are_durable_and_strictly_ordered() {
        let workspace = workspace();
        let store = SetupCheckpointStore::new(workspace.join("state"));
        let mut checkpoint = SetupCheckpoint::new(
            &workspace,
            ModelPackageId::new("gemma4-e4b").unwrap(),
            vec![PlannedArtifactRole {
                role: ModelArtifactRole::Main,
                model_id: agl_config::ModelId::new("gemma4-e4b").unwrap(),
            }],
            false,
            "a".repeat(64),
        )
        .unwrap();
        assert!(checkpoint.advance(SetupPhase::ArtifactsReady).is_err());
        checkpoint.advance(SetupPhase::Confirmed).unwrap();
        store.save(&checkpoint).unwrap();
        assert_eq!(
            store.load(&workspace).unwrap().unwrap().completed_phases,
            vec![SetupPhase::Confirmed]
        );
    }

    #[test]
    fn every_persisted_phase_resumes_the_same_intent_to_completion() {
        let workspace = workspace();
        let store = SetupCheckpointStore::new(workspace.join("state"));
        let planned = vec![
            PlannedArtifactRole {
                role: ModelArtifactRole::Main,
                model_id: agl_config::ModelId::new("gemma4-e4b").unwrap(),
            },
            PlannedArtifactRole {
                role: ModelArtifactRole::Projector,
                model_id: agl_config::ModelId::new("gemma4-e4b-mmproj").unwrap(),
            },
        ];
        let mut checkpoint = SetupCheckpoint::new(
            &workspace,
            ModelPackageId::new("gemma4-e4b").unwrap(),
            planned.clone(),
            false,
            "c".repeat(64),
        )
        .unwrap();

        for phase in SetupPhase::ORDERED {
            checkpoint.advance(phase).unwrap();
            store.save(&checkpoint).unwrap();
            checkpoint = store.load(&workspace).unwrap().unwrap();
            assert_eq!(checkpoint.package_id.as_str(), "gemma4-e4b");
            assert_eq!(checkpoint.planned_artifacts, planned);
            assert_eq!(checkpoint.completed_phases.last(), Some(&phase));
        }

        store.remove(&workspace).unwrap();
        assert!(store.load(&workspace).unwrap().is_none());
    }

    #[test]
    fn cpu_fallback_consent_is_scoped_to_one_exact_plan() {
        let workspace = workspace();
        let planned = vec![PlannedArtifactRole {
            role: ModelArtifactRole::Main,
            model_id: agl_config::ModelId::new("gemma4-e4b").unwrap(),
        }];
        let mut original = SetupCheckpoint::new(
            &workspace,
            ModelPackageId::new("gemma4-e4b").unwrap(),
            planned.clone(),
            true,
            "d".repeat(64),
        )
        .unwrap();
        original.consent_to_cpu_fallback();
        original.validate().unwrap();
        assert_eq!(
            original.cpu_fallback_consent_plan_hash,
            Some("d".repeat(64))
        );

        let replacement = SetupCheckpoint::new(
            &workspace,
            ModelPackageId::new("gemma4-12b").unwrap(),
            planned,
            true,
            "e".repeat(64),
        )
        .unwrap();
        assert!(replacement.cpu_fallback_consent_plan_hash.is_none());

        original.plan_hash = "e".repeat(64);
        assert!(original.validate().is_err());
    }

    #[test]
    fn store_lists_every_workspace_and_rejects_identity_tampering() {
        let root = workspace();
        let first_workspace = root.join("first");
        let second_workspace = root.join("second");
        std::fs::create_dir_all(&first_workspace).unwrap();
        std::fs::create_dir_all(&second_workspace).unwrap();
        let store = SetupCheckpointStore::new(root.join("state"));
        for (workspace, package) in [
            (&first_workspace, "gemma4-e4b"),
            (&second_workspace, "gemma4-12b"),
        ] {
            let mut checkpoint = SetupCheckpoint::new(
                workspace,
                ModelPackageId::new(package).unwrap(),
                vec![PlannedArtifactRole {
                    role: ModelArtifactRole::Main,
                    model_id: agl_config::ModelId::new(package).unwrap(),
                }],
                false,
                "f".repeat(64),
            )
            .unwrap();
            checkpoint.advance(SetupPhase::Confirmed).unwrap();
            store.save(&checkpoint).unwrap();
        }

        let checkpoints = store.list().unwrap();
        assert_eq!(checkpoints.len(), 2);
        assert_eq!(checkpoints[0].completed_phases, vec![SetupPhase::Confirmed]);
        assert_eq!(checkpoints[1].completed_phases, vec![SetupPhase::Confirmed]);

        let mut tampered = store.load(&first_workspace).unwrap().unwrap();
        tampered.workspace_identity = second_workspace.canonicalize().unwrap();
        assert!(tampered.validate().is_err());
    }
}
