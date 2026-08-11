use std::fs::{self, File, OpenOptions};
use std::io::Write;
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
}

#[derive(Debug)]
pub struct AttemptJournal {
    path: Option<PathBuf>,
    file: Option<File>,
    records: Vec<InferenceAttemptTransitionRecord>,
    bytes: Vec<u8>,
}

impl AttemptJournal {
    pub fn in_memory() -> Self {
        Self {
            path: None,
            file: None,
            records: Vec::new(),
            bytes: Vec::new(),
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
        })
    }

    pub fn append(
        &mut self,
        machine: &mut InferenceAttemptMachine,
        transition: InferenceAttemptTransition,
    ) -> Result<&InferenceAttemptTransitionRecord, AttemptJournalError> {
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
