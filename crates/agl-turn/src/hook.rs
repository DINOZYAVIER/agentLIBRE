use std::collections::BTreeSet;

pub use agl_capabilities::{
    HookBatchRequest, HookBatchResult, HookEvent, HookId, HookMessage, HookResult, HookStatus,
};
use serde::{Serialize, Serializer};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnHookBatch {
    pub event: HookEvent,
    pub required_hooks: Vec<HookId>,
    pub optional_hooks: Vec<HookId>,
}

impl TurnHookBatch {
    pub fn new(event: HookEvent) -> Self {
        Self {
            event,
            required_hooks: Vec::new(),
            optional_hooks: Vec::new(),
        }
    }

    pub fn with_required_hook(mut self, hook_id: HookId) -> Self {
        self.required_hooks.push(hook_id);
        self
    }

    pub fn with_optional_hook(mut self, hook_id: HookId) -> Self {
        self.optional_hooks.push(hook_id);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.required_hooks.is_empty() && self.optional_hooks.is_empty()
    }

    pub fn hook_ids(&self) -> Vec<HookId> {
        self.required_hooks
            .iter()
            .chain(self.optional_hooks.iter())
            .cloned()
            .collect()
    }

    pub fn summary(&self) -> HookBatchSummary {
        HookBatchSummary::new(
            self.event,
            self.required_hooks.clone(),
            self.optional_hooks.clone(),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookBatchOutcome {
    Pass,
    Warn,
    Fail,
    Repair,
}

impl HookBatchOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Repair => "repair",
        }
    }
}

impl From<HookStatus> for HookBatchOutcome {
    fn from(status: HookStatus) -> Self {
        match status {
            HookStatus::Pass => Self::Pass,
            HookStatus::Warn => Self::Warn,
            HookStatus::Fail => Self::Fail,
            HookStatus::Repair => Self::Repair,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HookResultSummary {
    pub hook_id: HookId,
    pub status: HookBatchOutcome,
    pub message_codes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HookBatchSummary {
    #[serde(serialize_with = "serialize_hook_event")]
    pub event: HookEvent,
    pub required_hooks: Vec<HookId>,
    pub optional_hooks: Vec<HookId>,
    pub results: Vec<HookResultSummary>,
    pub missing_required_hooks: Vec<HookId>,
    pub message_codes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<HookBatchOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl HookBatchSummary {
    pub fn new(event: HookEvent, required_hooks: Vec<HookId>, optional_hooks: Vec<HookId>) -> Self {
        Self {
            event,
            required_hooks,
            optional_hooks,
            results: Vec::new(),
            missing_required_hooks: Vec::new(),
            message_codes: Vec::new(),
            outcome: None,
            duration_ms: None,
        }
    }

    pub fn from_batch_result(
        batch: &TurnHookBatch,
        result: HookBatchResult,
        duration_ms: Option<u64>,
    ) -> Self {
        let seen = result
            .results
            .iter()
            .map(|result| result.hook_id.clone())
            .collect::<BTreeSet<_>>();
        let missing_required_hooks = batch
            .required_hooks
            .iter()
            .filter(|hook_id| !seen.contains(*hook_id))
            .cloned()
            .collect::<Vec<_>>();
        let results = result
            .results
            .into_iter()
            .map(HookResultSummary::from)
            .collect::<Vec<_>>();
        let mut summary = Self {
            event: batch.event,
            required_hooks: batch.required_hooks.clone(),
            optional_hooks: batch.optional_hooks.clone(),
            results,
            missing_required_hooks,
            message_codes: Vec::new(),
            outcome: None,
            duration_ms,
        };
        summary.message_codes = summary.collect_message_codes();
        summary.outcome = Some(summary.derived_outcome());
        summary
    }

    pub fn failed_without_results(
        batch: &TurnHookBatch,
        duration_ms: Option<u64>,
        message_code: impl Into<String>,
    ) -> Self {
        let mut summary = Self {
            event: batch.event,
            required_hooks: batch.required_hooks.clone(),
            optional_hooks: batch.optional_hooks.clone(),
            results: Vec::new(),
            missing_required_hooks: batch.required_hooks.clone(),
            message_codes: vec![message_code.into()],
            outcome: Some(HookBatchOutcome::Fail),
            duration_ms,
        };
        summary.message_codes = summary.collect_message_codes();
        summary
    }

    pub fn required_count(&self) -> usize {
        self.required_hooks.len()
    }

    pub fn optional_count(&self) -> usize {
        self.optional_hooks.len()
    }

    pub fn hook_ids(&self) -> Vec<HookId> {
        self.required_hooks
            .iter()
            .chain(self.optional_hooks.iter())
            .cloned()
            .collect()
    }

    pub fn failed_required_count(&self) -> usize {
        let required = self.required_set();
        self.results
            .iter()
            .filter(|result| {
                required.contains(&result.hook_id) && result.status == HookBatchOutcome::Fail
            })
            .count()
    }

    pub fn warning_count(&self) -> usize {
        let required = self.required_set();
        self.results
            .iter()
            .filter(|result| {
                result.status == HookBatchOutcome::Warn
                    || (result.status == HookBatchOutcome::Fail
                        && !required.contains(&result.hook_id))
            })
            .count()
    }

    pub fn repair_count(&self) -> usize {
        self.results
            .iter()
            .filter(|result| result.status == HookBatchOutcome::Repair)
            .count()
    }

    pub fn missing_required_count(&self) -> usize {
        self.missing_required_hooks.len()
    }

    pub fn outcome(&self) -> HookBatchOutcome {
        self.outcome.unwrap_or_else(|| self.derived_outcome())
    }

    fn derived_outcome(&self) -> HookBatchOutcome {
        if self.failed_required_count() > 0 || self.missing_required_count() > 0 {
            HookBatchOutcome::Fail
        } else if self.repair_count() > 0 {
            HookBatchOutcome::Repair
        } else if self.warning_count() > 0 {
            HookBatchOutcome::Warn
        } else {
            HookBatchOutcome::Pass
        }
    }

    fn collect_message_codes(&self) -> Vec<String> {
        let mut codes = self
            .message_codes
            .iter()
            .cloned()
            .chain(
                self.results
                    .iter()
                    .flat_map(|result| result.message_codes.iter().cloned()),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        codes.sort();
        codes
    }

    fn required_set(&self) -> BTreeSet<HookId> {
        self.required_hooks.iter().cloned().collect()
    }
}

impl From<HookResult> for HookResultSummary {
    fn from(result: HookResult) -> Self {
        Self {
            hook_id: result.hook_id,
            status: HookBatchOutcome::from(result.status),
            message_codes: result
                .messages
                .into_iter()
                .map(|message| message.code)
                .collect(),
        }
    }
}

fn serialize_hook_event<S>(event: &HookEvent, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(event.as_str())
}
