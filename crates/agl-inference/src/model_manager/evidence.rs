use crate::evidence::InferenceEventWriter;
use crate::{
    InferenceAdmissionEvidence, InferenceAttemptFailure, InferenceAttemptMachine,
    InferenceAttemptOutcome, InferenceAttemptTransition, InferenceContentEvidence,
    InferenceDispatchEvidence, InferenceFinishReason, InferencePlanEvidence,
    InferenceRejectionStage, InferenceResponse, InferenceRuntimeEvidence,
};

use super::{InferenceJob, ModelManagerError};

pub(super) struct AttemptEvidence {
    paths: crate::evidence::InferenceArtifactPaths,
    writer: InferenceEventWriter,
    machine: InferenceAttemptMachine,
}

impl AttemptEvidence {
    pub(super) fn start(job: &InferenceJob) -> Result<Self, ModelManagerError> {
        let request = job.request();
        let paths = job
            .artifact_root()
            .paths(&request.run_id, &request.attempt_id);
        let writer = InferenceEventWriter::open(
            paths.events_jsonl(),
            request.session_id.clone(),
            request.request_id.clone(),
        )
        .map_err(evidence_error)?;
        let mut machine = InferenceAttemptMachine::new(
            request.run_id.clone(),
            request.turn_id.clone(),
            request.attempt_id.clone(),
        );
        apply(
            &writer,
            &mut machine,
            InferenceAttemptTransition::StartAttempt {
                backend: job.config().backend.kind.as_str().to_string(),
                request_path: paths.request_json().to_path_buf(),
            },
        )?;
        paths.write_request_json(request).map_err(evidence_error)?;
        apply(
            &writer,
            &mut machine,
            InferenceAttemptTransition::RecordRequest {
                path: paths.request_json().to_path_buf(),
            },
        )?;
        let model_key = job.model_key().digest().to_owned();
        apply(
            &writer,
            &mut machine,
            InferenceAttemptTransition::RecordPlan {
                plan: InferencePlanEvidence {
                    plan_digest: model_key.clone(),
                    package_refs: Vec::new(),
                    profile_id: "pre-agl173-manager".to_owned(),
                },
            },
        )?;
        apply(
            &writer,
            &mut machine,
            InferenceAttemptTransition::RecordContentReady {
                content: InferenceContentEvidence {
                    content_digest: job.context_key().digest().to_owned(),
                    resolved_bytes: 0,
                },
            },
        )?;
        apply(
            &writer,
            &mut machine,
            InferenceAttemptTransition::RecordAdmissionGrant {
                admission: InferenceAdmissionEvidence {
                    reservation_id: model_key.clone(),
                    resource_components: Vec::new(),
                },
            },
        )?;
        apply(
            &writer,
            &mut machine,
            InferenceAttemptTransition::RecordDispatch {
                dispatch: InferenceDispatchEvidence {
                    descriptor_set_id: model_key.clone(),
                    engine_generation: model_key.clone(),
                },
            },
        )?;
        apply(
            &writer,
            &mut machine,
            InferenceAttemptTransition::RecordRuntimeStarted {
                runtime: InferenceRuntimeEvidence {
                    allocation_receipt_id: model_key,
                },
            },
        )?;
        Ok(Self {
            paths,
            writer,
            machine,
        })
    }

    pub(super) fn succeed(
        mut self,
        response: &InferenceResponse,
        log: String,
    ) -> Result<(), ModelManagerError> {
        self.write_runtime_log(log)?;
        self.paths
            .write_response_json(response)
            .map_err(evidence_error)?;
        apply(
            &self.writer,
            &mut self.machine,
            InferenceAttemptTransition::RecordResponse {
                path: self.paths.response_json().to_path_buf(),
            },
        )?;
        apply(
            &self.writer,
            &mut self.machine,
            InferenceAttemptTransition::FinishAttempt {
                outcome: match response.finish_reason {
                    InferenceFinishReason::Stop => InferenceAttemptOutcome::Succeeded,
                    InferenceFinishReason::Length | InferenceFinishReason::ContentByteLimit => {
                        InferenceAttemptOutcome::IncompleteOutput
                    }
                },
            },
        )?;
        Ok(())
    }

    pub(super) fn fail(
        mut self,
        error: &ModelManagerError,
        mut log: String,
    ) -> Result<(), ModelManagerError> {
        if !log.is_empty() && !log.ends_with('\n') {
            log.push('\n');
        }
        log.push_str("error = ");
        log.push_str(&error.to_string());
        log.push('\n');
        self.write_runtime_log(log)?;
        apply(
            &self.writer,
            &mut self.machine,
            InferenceAttemptTransition::RecordFailure {
                failure: InferenceAttemptFailure {
                    code: error.code().to_string(),
                    stage: InferenceRejectionStage::Engine,
                    message: error.to_string(),
                },
            },
        )?;
        apply(
            &self.writer,
            &mut self.machine,
            InferenceAttemptTransition::FinishAttempt {
                outcome: InferenceAttemptOutcome::Failed,
            },
        )?;
        Ok(())
    }

    fn write_runtime_log(&mut self, log: String) -> Result<(), ModelManagerError> {
        self.paths.write_runtime_log(log).map_err(evidence_error)?;
        apply(
            &self.writer,
            &mut self.machine,
            InferenceAttemptTransition::RecordRuntimeLog {
                path: self.paths.runtime_log().to_path_buf(),
            },
        )?;
        Ok(())
    }
}

fn apply(
    writer: &InferenceEventWriter,
    machine: &mut InferenceAttemptMachine,
    transition: InferenceAttemptTransition,
) -> Result<(), ModelManagerError> {
    let record = machine.apply(transition).map_err(evidence_error)?;
    writer.append_transition(&record).map_err(evidence_error)
}

fn evidence_error(error: impl std::fmt::Display) -> ModelManagerError {
    ModelManagerError::GenerationFailed {
        message: format!("failed to record inference evidence: {error}"),
    }
}
