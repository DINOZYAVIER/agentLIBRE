use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{InferenceAttemptTransition, InferenceAttemptTransitionRecord};

use super::{InferenceAttemptId, InferenceRunId};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceFinishStatus {
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum InferenceObservationEvent {
    #[serde(rename = "inference.attempt_started")]
    AttemptStarted {
        run_id: InferenceRunId,
        attempt_id: InferenceAttemptId,
        backend: String,
        request_path: PathBuf,
    },
    #[serde(rename = "inference.request_recorded")]
    RequestRecorded {
        run_id: InferenceRunId,
        attempt_id: InferenceAttemptId,
        path: PathBuf,
    },
    #[serde(rename = "inference.response_recorded")]
    ResponseRecorded {
        run_id: InferenceRunId,
        attempt_id: InferenceAttemptId,
        path: PathBuf,
    },
    #[serde(rename = "inference.attempt_finished")]
    AttemptFinished {
        run_id: InferenceRunId,
        attempt_id: InferenceAttemptId,
        finish_status: InferenceFinishStatus,
    },
    #[serde(rename = "inference.attempt_failed")]
    AttemptFailed {
        run_id: InferenceRunId,
        attempt_id: InferenceAttemptId,
        message: String,
    },
}

impl InferenceObservationEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            InferenceObservationEvent::AttemptStarted { .. } => "inference.attempt_started",
            InferenceObservationEvent::RequestRecorded { .. } => "inference.request_recorded",
            InferenceObservationEvent::ResponseRecorded { .. } => "inference.response_recorded",
            InferenceObservationEvent::AttemptFinished { .. } => "inference.attempt_finished",
            InferenceObservationEvent::AttemptFailed { .. } => "inference.attempt_failed",
        }
    }

    pub fn to_jsonl_line(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferenceEventWriter {
    events_jsonl: PathBuf,
}

impl InferenceEventWriter {
    pub fn new(events_jsonl: impl Into<PathBuf>) -> Self {
        Self {
            events_jsonl: events_jsonl.into(),
        }
    }

    pub fn append_transition(&self, record: &InferenceAttemptTransitionRecord) -> Result<()> {
        let Some(event) = event_for_record(record) else {
            return Ok(());
        };
        self.append_event(&event)
    }

    fn append_event(&self, event: &InferenceObservationEvent) -> Result<()> {
        if let Some(parent) = self.events_jsonl.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create inference event directory {}",
                    parent.display()
                )
            })?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.events_jsonl)
            .with_context(|| {
                format!(
                    "failed to open inference event stream {}",
                    self.events_jsonl.display()
                )
            })?;
        let line = event
            .to_jsonl_line()
            .context("failed to serialize inference observation event")?;
        file.write_all(line.as_bytes()).with_context(|| {
            format!(
                "failed to write inference event {}",
                self.events_jsonl.display()
            )
        })?;
        file.write_all(b"\n").with_context(|| {
            format!(
                "failed to write inference event {}",
                self.events_jsonl.display()
            )
        })?;
        file.flush().with_context(|| {
            format!(
                "failed to flush inference event {}",
                self.events_jsonl.display()
            )
        })
    }
}

fn event_for_record(
    record: &InferenceAttemptTransitionRecord,
) -> Option<InferenceObservationEvent> {
    match &record.transition {
        InferenceAttemptTransition::StartAttempt {
            backend,
            request_path,
        } => Some(InferenceObservationEvent::AttemptStarted {
            run_id: record.run_id.clone(),
            attempt_id: record.attempt_id.clone(),
            backend: backend.clone(),
            request_path: request_path.clone(),
        }),
        InferenceAttemptTransition::RecordRequest { path } => {
            Some(InferenceObservationEvent::RequestRecorded {
                run_id: record.run_id.clone(),
                attempt_id: record.attempt_id.clone(),
                path: path.clone(),
            })
        }
        InferenceAttemptTransition::RecordResponse { path } => {
            Some(InferenceObservationEvent::ResponseRecorded {
                run_id: record.run_id.clone(),
                attempt_id: record.attempt_id.clone(),
                path: path.clone(),
            })
        }
        InferenceAttemptTransition::FailAttempt { message } => {
            Some(InferenceObservationEvent::AttemptFailed {
                run_id: record.run_id.clone(),
                attempt_id: record.attempt_id.clone(),
                message: message.clone(),
            })
        }
        InferenceAttemptTransition::FinishAttempt { status } => {
            Some(InferenceObservationEvent::AttemptFinished {
                run_id: record.run_id.clone(),
                attempt_id: record.attempt_id.clone(),
                finish_status: *status,
            })
        }
        InferenceAttemptTransition::StartRuntime
        | InferenceAttemptTransition::RecordRuntimeLog { .. } => None,
    }
}
