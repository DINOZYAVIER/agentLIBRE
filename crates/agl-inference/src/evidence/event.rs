use std::path::PathBuf;

use agl_events::{EventDraft, EventScope, RuntimeEvent, RuntimeEventWriter};
use agl_ids::{RequestId, SessionId};
use anyhow::Result;

use crate::{
    InferenceAttemptOutcome, InferenceAttemptTransition, InferenceAttemptTransitionRecord,
};

#[derive(Clone, Debug)]
pub struct InferenceEventWriter {
    writer: RuntimeEventWriter,
    session_id: Option<SessionId>,
    request_id: Option<RequestId>,
}

impl InferenceEventWriter {
    pub fn open(
        events_jsonl: impl Into<PathBuf>,
        session_id: Option<SessionId>,
        request_id: Option<RequestId>,
    ) -> Result<Self> {
        Ok(Self {
            writer: RuntimeEventWriter::open(events_jsonl)?,
            session_id,
            request_id,
        })
    }

    pub fn append_transition(&self, record: &InferenceAttemptTransitionRecord) -> Result<()> {
        let Some(payload) = payload_for_transition(&record.transition) else {
            return Ok(());
        };
        let mut scope = EventScope::builder(record.run_id.clone())
            .turn_id(record.turn_id.clone())
            .attempt_id(record.attempt_id.clone());
        if let Some(session_id) = &self.session_id {
            scope = scope.session_id(session_id.clone());
        }
        let scope = scope.build()?;
        let mut draft = EventDraft::new(scope, payload);
        if let Some(request_id) = &self.request_id {
            draft = draft.with_request_id(request_id.clone());
        }
        self.writer.append(draft)?;
        Ok(())
    }
}

fn payload_for_transition(transition: &InferenceAttemptTransition) -> Option<RuntimeEvent> {
    match transition {
        InferenceAttemptTransition::StartAttempt {
            backend,
            request_path,
            ..
        } => Some(RuntimeEvent::InferenceAttemptStarted {
            backend: backend.clone(),
            request_path: request_path.clone(),
        }),
        InferenceAttemptTransition::RecordRequest { path } => {
            Some(RuntimeEvent::InferenceRequestRecorded { path: path.clone() })
        }
        InferenceAttemptTransition::RecordResponse { path } => {
            Some(RuntimeEvent::InferenceResponseRecorded { path: path.clone() })
        }
        InferenceAttemptTransition::RecordFailure { failure } => {
            Some(RuntimeEvent::InferenceAttemptFailed {
                message: failure.code.clone(),
            })
        }
        InferenceAttemptTransition::FinishAttempt { outcome } => {
            Some(RuntimeEvent::InferenceAttemptFinished {
                finish_status: match outcome {
                    InferenceAttemptOutcome::Succeeded => {
                        agl_events::InferenceFinishStatus::Succeeded
                    }
                    InferenceAttemptOutcome::IncompleteOutput => {
                        agl_events::InferenceFinishStatus::IncompleteOutput
                    }
                    InferenceAttemptOutcome::Failed => agl_events::InferenceFinishStatus::Failed,
                    InferenceAttemptOutcome::Cancelled => {
                        agl_events::InferenceFinishStatus::Cancelled
                    }
                },
            })
        }
        InferenceAttemptTransition::RecordPlan { .. }
        | InferenceAttemptTransition::RecordContentReady { .. }
        | InferenceAttemptTransition::RecordAdmissionGrant { .. }
        | InferenceAttemptTransition::RecordDispatch { .. }
        | InferenceAttemptTransition::RecordRuntimeStarted { .. }
        | InferenceAttemptTransition::RecordGenerationMetrics { .. }
        | InferenceAttemptTransition::RecordRuntimeLog { .. }
        | InferenceAttemptTransition::RecordCancellation { .. } => None,
    }
}
