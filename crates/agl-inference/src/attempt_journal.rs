use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::{
    InferenceAttemptMachine, InferenceAttemptTransition, InferenceAttemptTransitionError,
    InferenceAttemptTransitionRecord,
};

#[derive(Debug, Error)]
pub enum AttemptJournalError {
    #[error(transparent)]
    Transition(#[from] InferenceAttemptTransitionError),
    #[error("attempt journal I/O failed at {}: {reason}", path.display())]
    Io { path: PathBuf, reason: String },
    #[error("attempt journal is corrupt at record {record}: {reason}")]
    Corrupt { record: usize, reason: String },
    #[error("attempt projection failed at {}: {reason}", path.display())]
    Projection { path: PathBuf, reason: String },
}

#[derive(Debug)]
pub struct AttemptJournal {
    path: Option<PathBuf>,
    file: Option<File>,
    records: Vec<InferenceAttemptTransitionRecord>,
    bytes: Vec<u8>,
    projection_dirty: bool,
}

impl AttemptJournal {
    pub fn in_memory() -> Self {
        Self {
            path: None,
            file: None,
            records: Vec::new(),
            bytes: Vec::new(),
            projection_dirty: false,
        }
    }

    pub fn create(path: impl Into<PathBuf>) -> Result<Self, AttemptJournalError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
        }
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .map_err(|error| io(&path, error))?;
        sync_parent(&path)?;
        Ok(Self {
            path: Some(path),
            file: Some(file),
            records: Vec::new(),
            bytes: Vec::new(),
            projection_dirty: false,
        })
    }

    pub fn open(
        path: impl Into<PathBuf>,
    ) -> Result<(Self, InferenceAttemptMachine), AttemptJournalError> {
        let path = path.into();
        let mut bytes = Vec::new();
        File::open(&path)
            .map_err(|error| io(&path, error))?
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| io(&path, error))?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(AttemptJournalError::Corrupt {
                record: 0,
                reason: "journal exceeds 16 MiB".to_owned(),
            });
        }
        let replay = Self::replay(&bytes)?;
        let machine = replay.machine.clone();
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|error| io(&path, error))?;
        let mut journal = Self {
            path: Some(path),
            file: Some(file),
            records: replay.records,
            bytes,
            projection_dirty: true,
        };
        journal.repair_projections()?;
        Ok((journal, machine))
    }

    pub fn append(
        &mut self,
        machine: &mut InferenceAttemptMachine,
        transition: InferenceAttemptTransition,
    ) -> Result<&InferenceAttemptTransitionRecord, AttemptJournalError> {
        self.repair_projections()?;
        let record = machine.preview(transition)?;
        let mut encoded =
            serde_json::to_vec(&record).map_err(|error| AttemptJournalError::Corrupt {
                record: self.records.len() + 1,
                reason: error.to_string(),
            })?;
        encoded.push(b'\n');
        if let Some(file) = &mut self.file {
            file.write_all(&encoded)
                .map_err(|error| io(self.path.as_deref().unwrap_or(Path::new("journal")), error))?;
            file.sync_data()
                .map_err(|error| io(self.path.as_deref().unwrap_or(Path::new("journal")), error))?;
        }
        machine.commit(&record)?;
        self.bytes.extend_from_slice(&encoded);
        self.records.push(record);
        self.projection_dirty = true;
        self.repair_projections()?;
        Ok(self.records.last().expect("record was just pushed"))
    }

    pub fn records(&self) -> &[InferenceAttemptTransitionRecord] {
        &self.records
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn replay(bytes: &[u8]) -> Result<AttemptJournalReplay, AttemptJournalError> {
        let mut records = Vec::new();
        for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            records.push(
                serde_json::from_slice::<InferenceAttemptTransitionRecord>(line).map_err(
                    |error| AttemptJournalError::Corrupt {
                        record: index + 1,
                        reason: error.to_string(),
                    },
                )?,
            );
        }
        let first = records
            .first()
            .ok_or_else(|| AttemptJournalError::Corrupt {
                record: 0,
                reason: "journal has no records".to_owned(),
            })?;
        let mut machine = InferenceAttemptMachine::new(
            first.run_id.clone(),
            first.turn_id.clone(),
            first.attempt_id.clone(),
        );
        for (index, record) in records.iter().enumerate() {
            machine
                .commit(record)
                .map_err(|error| AttemptJournalError::Corrupt {
                    record: index + 1,
                    reason: error.to_string(),
                })?;
        }
        Ok(AttemptJournalReplay {
            machine,
            records,
            bytes: bytes.to_vec(),
        })
    }

    pub fn repair_projections(&mut self) -> Result<(), AttemptJournalError> {
        if !self.projection_dirty || self.path.is_none() {
            return Ok(());
        }
        let journal_path = self.path.as_deref().expect("checked as persistent");
        let attempt_root = projection_root(&self.records)
            .or_else(|| journal_path.parent())
            .ok_or_else(|| AttemptJournalError::Projection {
                path: journal_path.to_path_buf(),
                reason: "journal has no projection directory".to_owned(),
            })?;
        let resolution = runtime_resolution_projection(&self.records)?;
        let events = event_projection(&self.records)?;
        write_projection_atomic(&attempt_root.join("runtime-resolution.json"), &resolution)?;
        write_projection_atomic(&attempt_root.join("inference-events.jsonl"), &events)?;
        self.projection_dirty = false;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct AttemptJournalReplay {
    machine: InferenceAttemptMachine,
    records: Vec<InferenceAttemptTransitionRecord>,
    bytes: Vec<u8>,
}

impl AttemptJournalReplay {
    pub fn machine(&self) -> &InferenceAttemptMachine {
        &self.machine
    }

    pub fn records(&self) -> &[InferenceAttemptTransitionRecord] {
        &self.records
    }

    pub fn journal_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

fn sync_parent(path: &Path) -> Result<(), AttemptJournalError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io(parent, error))
}

fn io(path: impl AsRef<Path>, error: std::io::Error) -> AttemptJournalError {
    AttemptJournalError::Io {
        path: path.as_ref().to_path_buf(),
        reason: error.to_string(),
    }
}

fn runtime_resolution_projection(
    records: &[InferenceAttemptTransitionRecord],
) -> Result<Vec<u8>, AttemptJournalError> {
    let first = records
        .first()
        .ok_or_else(|| AttemptJournalError::Corrupt {
            record: 0,
            reason: "journal has no records".to_owned(),
        })?;
    let mut plan = None;
    let mut content = None;
    let mut admission = None;
    let mut dispatch = None;
    let mut runtime = None;
    let mut generation = None;
    let mut failure = None;
    let mut cancellation = None;
    let mut outcome = None;
    let mut product_resolution = None;
    for record in records {
        match &record.transition {
            InferenceAttemptTransition::RecordPlan { plan: value } => {
                product_resolution = value.product_resolution.as_ref();
                plan = Some(value);
            }
            InferenceAttemptTransition::RecordContentReady { content: value } => {
                content = Some(value)
            }
            InferenceAttemptTransition::RecordAdmissionGrant { admission: value } => {
                admission = Some(value)
            }
            InferenceAttemptTransition::RecordDispatch { dispatch: value } => {
                dispatch = Some(value)
            }
            InferenceAttemptTransition::RecordRuntimeStarted { runtime: value } => {
                runtime = Some(value)
            }
            InferenceAttemptTransition::RecordGenerationMetrics { generation: value } => {
                generation = Some(value)
            }
            InferenceAttemptTransition::RecordFailure { failure: value } => {
                if let Some(rejection) = value.plan_rejection.as_ref() {
                    product_resolution = rejection.product_resolution.as_ref();
                }
                failure = Some(value);
            }
            InferenceAttemptTransition::RecordCancellation {
                cancellation: value,
            } => cancellation = Some(value),
            InferenceAttemptTransition::FinishAttempt { outcome: value } => outcome = Some(value),
            _ => {}
        }
    }
    let admission_projection = if let Some(value) = admission {
        serde_json::json!({"status": "granted", "grant": value, "error": null})
    } else if let Some(value) = failure {
        serde_json::json!({"status": "rejected", "grant": null, "error": value})
    } else {
        serde_json::json!({"status": "pending", "grant": null, "error": null})
    };
    serde_json::to_vec_pretty(&serde_json::json!({
        "schema": "agentlibre.inference-runtime-resolution/v1",
        "run_id": first.run_id,
        "turn_id": first.turn_id,
        "attempt_id": first.attempt_id,
        "sequence": records.len(),
        "phase": records.last().map(|record| record.to.as_str()),
        "plan": plan,
        "content": content,
        "product_resolution": product_resolution,
        "admission": admission_projection,
        "admission_evidence": admission,
        "dispatch": dispatch,
        "runtime": runtime,
        "generation": generation,
        "failure": failure,
        "cancellation": cancellation,
        "outcome": outcome,
    }))
    .map_err(|error| AttemptJournalError::Corrupt {
        record: records.len(),
        reason: error.to_string(),
    })
}

fn projection_root(records: &[InferenceAttemptTransitionRecord]) -> Option<&Path> {
    records.iter().find_map(|record| match &record.transition {
        InferenceAttemptTransition::StartAttempt {
            projection_root: Some(root),
            ..
        } => Some(root.as_path()),
        _ => None,
    })
}

fn event_projection(
    records: &[InferenceAttemptTransitionRecord],
) -> Result<Vec<u8>, AttemptJournalError> {
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(
            &mut bytes,
            &serde_json::json!({
                "schema": "agentlibre.inference-transition-event/v1",
                "record": record,
            }),
        )
        .map_err(|error| AttemptJournalError::Corrupt {
            record: record.sequence,
            reason: error.to_string(),
        })?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn write_projection_atomic(path: &Path, bytes: &[u8]) -> Result<(), AttemptJournalError> {
    let temporary = path.with_extension("tmp");
    let write = || -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()
    };
    write().map_err(|error| AttemptJournalError::Projection {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })
}
