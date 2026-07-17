use std::collections::{BTreeMap, VecDeque};

use agl_ids::{RunId, SessionId};

use crate::{ApplicationError, ApplicationErrorCode, PromptAdmission, PromptSubmission};

pub const MAX_QUEUED_PROMPTS_PER_SESSION: usize = 32;

#[derive(Default)]
pub struct PromptQueue {
    sessions: BTreeMap<SessionId, SessionQueue>,
}

#[derive(Default)]
struct SessionQueue {
    active: Option<RunId>,
    queued: VecDeque<PromptAdmission>,
    submissions: BTreeMap<String, PromptAdmission>,
}

impl PromptQueue {
    pub fn admit(
        &mut self,
        submission: &PromptSubmission,
        run_id: RunId,
    ) -> Result<PromptAdmission, ApplicationError> {
        let queue = self
            .sessions
            .entry(submission.session_id.clone())
            .or_default();
        if let Some(admission) = queue.submissions.get(&submission.client_submission_id) {
            return Ok(admission.clone());
        }
        if queue.queued.len() >= MAX_QUEUED_PROMPTS_PER_SESSION {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InputBackpressure,
                "session prompt queue is full",
            ));
        }
        let ordinal = u32::try_from(queue.queued.len() + usize::from(queue.active.is_some()) + 1)
            .unwrap_or(u32::MAX);
        let admission = PromptAdmission {
            session_id: submission.session_id.clone(),
            run_id,
            ordinal,
            queued: queue.active.is_some(),
            replayed: false,
        };
        queue
            .submissions
            .insert(submission.client_submission_id.clone(), admission.clone());
        if queue.active.is_none() {
            queue.active = Some(admission.run_id.clone());
        } else {
            queue.queued.push_back(admission.clone());
        }
        Ok(admission)
    }

    pub fn finish(&mut self, session_id: &SessionId, run_id: &RunId) -> Option<PromptAdmission> {
        let queue = self.sessions.get_mut(session_id)?;
        if queue.active.as_ref() != Some(run_id) {
            return None;
        }
        queue.active = None;
        let next = queue.queued.pop_front();
        if let Some(next) = &next {
            queue.active = Some(next.run_id.clone());
        }
        next
    }
}
