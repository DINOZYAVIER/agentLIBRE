use std::path::PathBuf;

use agl_model::ModelExecutionPlan;

use super::media::MediaAccounting;
use super::resource_ledger::ResourceReservation;
use super::{InferenceFailure, InferenceHostStartError, validate_recovered_projection_root};

pub(super) struct AttemptRecorder {
    journal: crate::AttemptJournal,
    machine: crate::InferenceAttemptMachine,
}

impl AttemptRecorder {
    pub(super) fn begin(
        root: Option<&PathBuf>,
        projection_root: Option<&std::path::Path>,
        product_resolution: Option<serde_json::Value>,
        plan: &ModelExecutionPlan,
        request: &crate::InferenceRequest,
    ) -> Result<Self, InferenceFailure> {
        use crate::InferenceAttemptTransition as Transition;

        let mut recorder = Self::begin_request(root, projection_root, request)?;
        recorder.append(Transition::RecordPlan {
            plan: crate::InferencePlanEvidence {
                plan_digest: plan.digest().as_str().to_owned(),
                package_refs: vec![
                    plan.function_package().reference.to_string(),
                    plan.model_package().reference.to_string(),
                ],
                profile_id: plan.profile_id().to_owned(),
                product_resolution,
            },
        })?;
        Ok(recorder)
    }

    pub(super) fn reject_plan(
        root: Option<&PathBuf>,
        projection_root: Option<&std::path::Path>,
        rejection: crate::InferencePlanRejectionEvidence,
        request: &crate::InferenceRequest,
    ) -> Result<(), InferenceFailure> {
        use crate::{
            InferenceAttemptFailure, InferenceAttemptOutcome,
            InferenceAttemptTransition as Transition, InferenceRejectionStage,
        };

        let mut recorder = Self::begin_request(root, projection_root, request)?;
        recorder.append(Transition::RecordFailure {
            failure: InferenceAttemptFailure {
                code: rejection.rejection.code().to_owned(),
                stage: InferenceRejectionStage::Plan,
                message: rejection.rejection.to_string(),
                plan_rejection: Some(rejection),
            },
        })?;
        recorder.append(Transition::FinishAttempt {
            outcome: InferenceAttemptOutcome::Failed,
        })?;
        Ok(())
    }

    fn begin_request(
        root: Option<&PathBuf>,
        projection_root: Option<&std::path::Path>,
        request: &crate::InferenceRequest,
    ) -> Result<Self, InferenceFailure> {
        use crate::InferenceAttemptTransition as Transition;

        let journal = if let Some(root) = root {
            crate::AttemptJournal::create(
                root.join(request.attempt_id.as_str())
                    .join("transitions.jsonl"),
            )
            .map_err(journal_failure)?
        } else {
            crate::AttemptJournal::in_memory()
        };
        let mut recorder = Self {
            journal,
            machine: crate::InferenceAttemptMachine::new(
                request.run_id.clone(),
                request.turn_id.clone(),
                request.attempt_id.clone(),
            ),
        };
        recorder.append(Transition::StartAttempt {
            backend: "llama_cpp".to_owned(),
            request_path: PathBuf::from("request.json"),
            projection_root: projection_root.map(std::path::Path::to_path_buf),
        })?;
        recorder.append(Transition::RecordRequest {
            path: PathBuf::from("request.json"),
        })?;
        Ok(recorder)
    }

    pub(super) fn content_ready(
        &mut self,
        request: &crate::InferenceRequest,
        media: MediaAccounting,
    ) -> Result<(), InferenceFailure> {
        use sha2::{Digest as _, Sha256};

        let canonical = serde_json::to_vec(&serde_json::json!({
            "domain": "agentlibre.inference-content/v1",
            "request": request,
            "resolved_media_bytes": media.resolved_bytes,
            "transport_bytes": media.transport_bytes,
            "decoder_allowance_bytes": media.decoder_allowance_bytes,
        }))
        .map_err(|error| InferenceFailure::EngineProtocol {
            reason: format!("failed to encode inference content identity: {error}"),
        })?;
        let digest = Sha256::digest(canonical);
        let mut content_digest = String::with_capacity(71);
        content_digest.push_str("sha256:");
        for byte in digest {
            use std::fmt::Write as _;
            write!(&mut content_digest, "{byte:02x}")
                .expect("writing a SHA-256 digest to String cannot fail");
        }
        self.append(crate::InferenceAttemptTransition::RecordContentReady {
            content: crate::InferenceContentEvidence {
                content_digest,
                resolved_bytes: media.resolved_bytes,
            },
        })
    }

    pub(super) fn admitted(
        &mut self,
        engine: &ResourceReservation,
        transient: &ResourceReservation,
        reused_resident_allocation: bool,
    ) -> Result<(), InferenceFailure> {
        self.append(crate::InferenceAttemptTransition::RecordAdmissionGrant {
            admission: crate::InferenceAdmissionEvidence {
                reservation_id: format!("engine:{};transient:{}", engine.id(), transient.id()),
                engine_reservation_id: format!("reservation:{}", engine.id()),
                reused_resident_allocation,
                resource_components: vec![
                    ("model_host_bytes".to_owned(), engine.host_bytes()),
                    ("model_device_bytes".to_owned(), engine.device_bytes()),
                    ("model_shared_bytes".to_owned(), engine.shared_bytes()),
                    ("transient_host_bytes".to_owned(), transient.host_bytes()),
                ],
            },
        })
    }

    pub(super) fn dispatched(
        &mut self,
        generation: u64,
        model_key: &str,
    ) -> Result<(), InferenceFailure> {
        self.append(crate::InferenceAttemptTransition::RecordDispatch {
            dispatch: crate::InferenceDispatchEvidence {
                descriptor_set_id: model_key.to_owned(),
                engine_generation: format!("engine:{generation}"),
            },
        })
    }

    pub(super) fn runtime_started(
        &mut self,
        receipt: &crate::engine::process::EngineAllocationReceipt,
    ) -> Result<(), InferenceFailure> {
        self.append(crate::InferenceAttemptTransition::RecordRuntimeStarted {
            runtime: crate::InferenceRuntimeEvidence {
                allocation_receipt_id: receipt.receipt_id.clone(),
                plan_digest: receipt.plan_digest.clone(),
                reservation_id: format!("reservation:{}", receipt.reservation_id),
                engine_generation: format!("engine:{}", receipt.engine_generation),
                selected_device: receipt.selected_device.clone(),
                host_bytes: receipt.host_bytes,
                device_bytes: receipt.device_bytes,
                shared_bytes: receipt.shared_bytes,
            },
        })
    }

    pub(super) fn response_recorded(
        &mut self,
        response: &crate::InferenceResponse,
    ) -> Result<(), InferenceFailure> {
        use crate::InferenceAttemptTransition as Transition;

        self.append(Transition::RecordGenerationMetrics {
            generation: crate::InferenceGenerationEvidence {
                input_tokens: response.metadata.input_tokens,
                output_tokens: response.metadata.output_tokens,
                configured_batch_size: response.metadata.configured_batch_size,
                prefill_chunks: response.metadata.prefill_chunks,
            },
        })?;
        self.append(crate::InferenceAttemptTransition::RecordRuntimeLog {
            path: PathBuf::from("engine.log"),
        })?;
        self.append(crate::InferenceAttemptTransition::RecordResponse {
            path: PathBuf::from("response.json"),
        })
    }

    pub(super) fn finish(
        &mut self,
        result: &Result<crate::InferenceResponse, InferenceFailure>,
    ) -> Result<(), InferenceFailure> {
        use crate::{
            InferenceAttemptCancellation, InferenceAttemptFailure, InferenceAttemptOutcome,
            InferenceAttemptTransition as Transition,
        };

        match result {
            Ok(response) => self.append(Transition::FinishAttempt {
                outcome: match response.finish_reason {
                    crate::InferenceFinishReason::Stop => InferenceAttemptOutcome::Succeeded,
                    crate::InferenceFinishReason::Length
                    | crate::InferenceFinishReason::ContentByteLimit => {
                        InferenceAttemptOutcome::IncompleteOutput
                    }
                },
            }),
            Err(InferenceFailure::Cancelled | InferenceFailure::DeadlineExceeded) => {
                self.append(Transition::RecordCancellation {
                    cancellation: InferenceAttemptCancellation {
                        reason: result.as_ref().unwrap_err().to_string(),
                    },
                })?;
                self.append(Transition::FinishAttempt {
                    outcome: InferenceAttemptOutcome::Cancelled,
                })
            }
            Err(error) => {
                self.append(Transition::RecordFailure {
                    failure: InferenceAttemptFailure {
                        code: failure_code(error).to_owned(),
                        stage: failure_stage(error),
                        message: error.to_string(),
                        plan_rejection: None,
                    },
                })?;
                self.append(Transition::FinishAttempt {
                    outcome: InferenceAttemptOutcome::Failed,
                })
            }
        }
    }

    fn append(
        &mut self,
        transition: crate::InferenceAttemptTransition,
    ) -> Result<(), InferenceFailure> {
        self.journal
            .append(&mut self.machine, transition)
            .map(|_| ())
            .map_err(journal_failure)
    }
}

fn failure_code(error: &InferenceFailure) -> &'static str {
    match error {
        InferenceFailure::Admission(_) => "live_admission_rejected",
        InferenceFailure::Queue(_) => "queue_rejected",
        InferenceFailure::DescriptorSet(_) | InferenceFailure::DescriptorChanged { .. } => {
            "descriptor_rejected"
        }
        InferenceFailure::Cancelled => "cancelled",
        InferenceFailure::DeadlineExceeded => "deadline_exceeded",
        InferenceFailure::ContextOverflow { .. } => "context_overflow",
        InferenceFailure::InvalidMedia { .. } => "content_rejected",
        InferenceFailure::Busy { .. } => "model_busy",
        InferenceFailure::CoolingDown { .. } => "engine_cooling_down",
        InferenceFailure::Quarantined { .. } => "resource_quarantined",
        InferenceFailure::HealthAuthority { .. } => "health_authority_failed",
        InferenceFailure::EngineProtocol { .. }
        | InferenceFailure::InvalidAllocationReceipt { .. } => "engine_failed",
    }
}

fn failure_stage(error: &InferenceFailure) -> crate::InferenceRejectionStage {
    match error {
        InferenceFailure::Admission(_) => crate::InferenceRejectionStage::Admission,
        InferenceFailure::Queue(_) => crate::InferenceRejectionStage::Queue,
        InferenceFailure::DescriptorSet(_) | InferenceFailure::DescriptorChanged { .. } => {
            crate::InferenceRejectionStage::Descriptor
        }
        InferenceFailure::ContextOverflow { .. } | InferenceFailure::InvalidMedia { .. } => {
            crate::InferenceRejectionStage::Content
        }
        InferenceFailure::Busy { .. }
        | InferenceFailure::CoolingDown { .. }
        | InferenceFailure::Quarantined { .. }
        | InferenceFailure::HealthAuthority { .. } => crate::InferenceRejectionStage::Dispatch,
        _ => crate::InferenceRejectionStage::Engine,
    }
}

fn journal_failure(error: crate::AttemptJournalError) -> InferenceFailure {
    InferenceFailure::EngineProtocol {
        reason: format!("attempt journal failed: {error}"),
    }
}

pub(super) fn recover_attempt_journals(
    root: &std::path::Path,
    evidence_root: &std::path::Path,
) -> Result<(), InferenceHostStartError> {
    use crate::{
        InferenceAttemptFailure, InferenceAttemptOutcome, InferenceAttemptPhase,
        InferenceAttemptTransition, InferenceRejectionStage,
    };

    std::fs::create_dir_all(root).map_err(|error| InferenceHostStartError::EngineStart {
        reason: format!("failed to create attempt journal root: {error}"),
    })?;
    let entries =
        std::fs::read_dir(root).map_err(|error| InferenceHostStartError::EngineStart {
            reason: format!("failed to inspect attempt journal root: {error}"),
        })?;
    for (index, entry) in entries.enumerate() {
        if index >= 4096 {
            return Err(InferenceHostStartError::EngineStart {
                reason: "attempt journal root exceeds 4096 entries".to_owned(),
            });
        }
        let entry = entry.map_err(|error| InferenceHostStartError::EngineStart {
            reason: format!("failed to inspect attempt journal entry: {error}"),
        })?;
        let file_type =
            entry
                .file_type()
                .map_err(|error| InferenceHostStartError::EngineStart {
                    reason: format!("failed to inspect attempt journal entry type: {error}"),
                })?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let path = entry.path().join("transitions.jsonl");
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|error| InferenceHostStartError::EngineStart {
            reason: format!("attempt journal recovery failed: {error}"),
        })?;
        let replay = crate::AttemptJournal::replay(&bytes).map_err(recovery_error)?;
        for record in replay.records() {
            if let InferenceAttemptTransition::StartAttempt {
                projection_root: Some(projection_root),
                ..
            } = record.transition()
                && validate_recovered_projection_root(projection_root, evidence_root).is_err()
            {
                return Err(InferenceHostStartError::EngineStart {
                    reason: "attempt projection root is outside the configured evidence authority"
                        .to_owned(),
                });
            }
        }
        let (mut journal, mut machine) = crate::AttemptJournal::open(&path).map_err(|error| {
            InferenceHostStartError::EngineStart {
                reason: format!("attempt journal recovery failed: {error}"),
            }
        })?;
        match machine.phase() {
            phase if phase.is_terminal() => {}
            InferenceAttemptPhase::FailureRecorded => {
                journal
                    .append(
                        &mut machine,
                        InferenceAttemptTransition::FinishAttempt {
                            outcome: InferenceAttemptOutcome::Failed,
                        },
                    )
                    .map_err(recovery_error)?;
            }
            InferenceAttemptPhase::CancellationRecorded => {
                journal
                    .append(
                        &mut machine,
                        InferenceAttemptTransition::FinishAttempt {
                            outcome: InferenceAttemptOutcome::Cancelled,
                        },
                    )
                    .map_err(recovery_error)?;
            }
            _ => {
                journal
                    .append(
                        &mut machine,
                        InferenceAttemptTransition::RecordFailure {
                            failure: InferenceAttemptFailure {
                                code: "host_restarted".to_owned(),
                                stage: InferenceRejectionStage::Evidence,
                                message:
                                    "host restarted before the inference attempt became terminal"
                                        .to_owned(),
                                plan_rejection: None,
                            },
                        },
                    )
                    .map_err(recovery_error)?;
                journal
                    .append(
                        &mut machine,
                        InferenceAttemptTransition::FinishAttempt {
                            outcome: InferenceAttemptOutcome::Failed,
                        },
                    )
                    .map_err(recovery_error)?;
            }
        }
    }
    Ok(())
}

fn recovery_error(error: crate::AttemptJournalError) -> InferenceHostStartError {
    InferenceHostStartError::EngineStart {
        reason: format!("attempt journal recovery failed: {error}"),
    }
}
