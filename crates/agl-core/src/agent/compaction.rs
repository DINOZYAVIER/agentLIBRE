use serde::{Deserialize, Serialize};

use crate::{AgentRunId, Content, MessageId, ToolId};

use super::MemoryTopic;
use super::{
    AgentOperationDeliveryState, AgentOperationFailureKind, AgentOperationKey, AgentOperationKind,
    CompactionBudgets, ContextCapacity, EffectReceipt, InferenceRealizationRef,
    ModelRuntimeSelection, ModelSelection, ModelUsage, PackageDigest, ReasoningEffort,
    ReasoningSelection, WorkspaceScope,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionRequest {
    pub context: Vec<MessageId>,
    pub before: ContextCapacity,
    pub correction_of: Option<AgentOperationKey>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryClaim {
    pub text: String,
    pub sources: Vec<MessageId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticSummary {
    pub objective: SummaryClaim,
    pub rationale: Vec<SummaryClaim>,
    pub decisions: Vec<SummaryClaim>,
    pub completed: Vec<SummaryClaim>,
    pub discoveries: Vec<SummaryClaim>,
    pub unresolved: Vec<SummaryClaim>,
    pub next_position: SummaryClaim,
    /// Durable, sourced memory candidates extracted in the same summary pass.
    /// Older stored summaries omit this field and decode as an empty list.
    #[serde(default)]
    pub memory: Vec<MemoryTopic>,
}

impl SemanticSummary {
    pub fn validate(&self, source: &[MessageId]) -> Result<(), &'static str> {
        let claims = std::iter::once(&self.objective)
            .chain(&self.rationale)
            .chain(&self.decisions)
            .chain(&self.completed)
            .chain(&self.discoveries)
            .chain(&self.unresolved)
            .chain(std::iter::once(&self.next_position));
        for claim in claims {
            if claim.text.trim().is_empty() {
                return Err("summary claim text is empty");
            }
            if claim.sources.is_empty() || claim.sources.iter().any(|id| !source.contains(id)) {
                return Err("summary claim must reference messages in the summarized source");
            }
        }
        Ok(())
    }
}

/// Exact structured facts are built by the daemon, never decoded from model output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionExactState {
    pub snapshot_run: AgentRunId,
    pub snapshot_digest: PackageDigest,
    pub workspace: WorkspaceScope,
    pub operations: Vec<FoldedOperationFact>,
    pub inspected_files: Vec<InspectedFileFact>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectedFileRange {
    pub start_line: u64,
    pub end_line: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectedFileFact {
    pub path: String,
    pub digest: String,
    pub ranges: Vec<InspectedFileRange>,
    pub sources: Vec<MessageId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationFact {
    pub kind: AgentOperationKind,
    pub tool_id: Option<ToolId>,
    pub status: AgentOperationDeliveryState,
    pub failure: Option<AgentOperationFailureKind>,
    pub effect_receipts: Vec<EffectReceipt>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FoldedOperationFact {
    pub fact: OperationFact,
    pub count: u64,
    pub first: AgentOperationKey,
    pub last: AgentOperationKey,
    pub sources: Vec<AgentOperationKey>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionContent {
    pub exact: CompactionExactState,
    pub semantic: SemanticSummary,
}

impl CompactionContent {
    pub fn render(&self) -> Result<Content, &'static str> {
        let json = serde_json::to_string(self).map_err(|_| "invalid compaction content")?;
        Content::text(json).map_err(|_| "compaction content exceeds the content limit")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionMetadata {
    pub source_start: MessageId,
    pub source_end: MessageId,
    pub source: Vec<MessageId>,
    pub retained_checkpoints: Vec<MessageId>,
    pub tail_first: Option<MessageId>,
    pub tail: Vec<MessageId>,
    pub tail_tokens: u64,
    pub summary_tokens: u64,
    pub before: ContextCapacity,
    pub summary_request: ContextCapacity,
    pub after: ContextCapacity,
    pub tokenizer_artifact: PackageDigest,
    pub realization: InferenceRealizationRef,
    pub summary_operation: AgentOperationKey,
    pub correction_of: Option<AgentOperationKey>,
    pub reasoning: ReasoningSelection,
    pub usage: ModelUsage,
}

impl CompactionMetadata {
    /// Check durable measurements against the admitted model, not model-authored text.
    pub fn validate_model(&self, model: &ModelSelection) -> Result<(), &'static str> {
        let runtime = summary_runtime(model).ok_or("invalid summary budget")?;
        let budgets = CompactionBudgets::new(
            model.runtime.load.context_tokens,
            self.before.reserved_output_tokens,
            false,
        )
        .ok_or("invalid rebuilt context budget")?;
        let capacity = model.runtime.load.context_tokens;
        if self.tokenizer_artifact != runtime.artifact.digest
            || self.reasoning != runtime.reasoning
            || self.before.context_capacity_tokens != capacity
            || self.before.reserved_output_tokens > model.runtime.generation.max_output_tokens
            || self.summary_request
                != ContextCapacity::new(
                    self.summary_request.prompt_tokens,
                    runtime.generation.max_output_tokens,
                    capacity,
                )
            || self.after
                != ContextCapacity::new(
                    self.after.prompt_tokens,
                    self.before.reserved_output_tokens,
                    capacity,
                )
            || self.before
                != ContextCapacity::new(
                    self.before.prompt_tokens,
                    self.before.reserved_output_tokens,
                    capacity,
                )
            || !self.summary_request.fits()
            || self.after.prompt_tokens > budgets.rebuilt_input_target
            || self.usage.output_tokens > runtime.generation.max_output_tokens
        {
            return Err("compaction metadata does not match the admitted model");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionResult {
    pub content: CompactionContent,
    pub metadata: CompactionMetadata,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionFailureStage {
    SummarySource,
    SemanticOutput,
    RebuiltInput,
    Inference,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionFailure {
    pub stage: CompactionFailureStage,
    pub model_called: bool,
    pub usage: Option<ModelUsage>,
    pub summary_source_start: Option<MessageId>,
    pub summary_source_end: Option<MessageId>,
    pub source_contributions: Vec<SourceTokenContribution>,
}

/// Exact measurements of isolated source ranges including the request's
/// instructions and template. These counts are not additive token estimates.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTokenContribution {
    pub source_start: MessageId,
    pub source_end: MessageId,
    pub isolated_prompt_tokens: u64,
}

pub fn summary_runtime(model: &ModelSelection) -> Option<ModelRuntimeSelection> {
    let supports_low = model.reasoning_efforts.contains(&ReasoningEffort::Low);
    let budgets = CompactionBudgets::new(
        model.runtime.load.context_tokens,
        model.runtime.generation.max_output_tokens,
        supports_low,
    )?;
    let mut runtime = model.runtime.clone();
    runtime.generation.max_output_tokens = budgets.summary_output_tokens;
    runtime.reasoning = if supports_low {
        ReasoningSelection::Enabled {
            effort: Some(ReasoningEffort::Low),
            max_tokens: budgets.summary_reasoning_tokens as u32,
            preserve: false,
        }
    } else {
        ReasoningSelection::Disabled
    };
    Some(runtime)
}
