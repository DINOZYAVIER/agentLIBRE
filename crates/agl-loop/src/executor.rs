use std::collections::BTreeSet;

use agl_actions::{ModelAction, RepairStrategy, ToolCall, ToolJsonRepair};
use agl_capabilities::{DispatchDenialCode, HookBatchRequest, HookBatchResult, HookEvent};
use agl_content::Content;
use agl_events::{EventDraft, EventScope, RuntimeEvent};
use agl_turn::policy::{ToolCallDecision, ToolCallStop, decide_tool_call};
use agl_turn::{
    HookBatchOutcome, HookBatchSummary, ModelRequest, StopReason, TurnFailureOperation,
    TurnHookBatch, TurnInput, TurnMessage, TurnOutput, TurnPhase, TurnState, TurnTerminalStatus,
    TurnTransition,
};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::json;

use crate::effect::{
    EffectFailure, EffectFailureCode, EffectKey, EffectOutcome, HookEffectOutput, TurnAdvance,
    TurnAdvanceState, TurnEffect, TurnEffectKind, TurnEffectResult, TurnExecutionFailure,
    TurnExecutorError, TurnTerminal,
};
use crate::event_map::{event_for_record, malformed_kind};

pub const TURN_CHECKPOINT_SCHEMA: &str = "agentlibre.turn-checkpoint.v1alpha";

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TurnCheckpoint {
    schema: String,
    state: TurnState,
    phase: ExecutorPhase,
    pending: Option<PendingEffect>,
    effect_sequence: u64,
    consumed_effects: Vec<EffectKey>,
    hook_repair_attempts: usize,
    pending_repair_message: Option<String>,
    cancellation_requested: bool,
    terminal: Option<TurnTerminal>,
}

impl TurnCheckpoint {
    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn state(&self) -> &TurnState {
        &self.state
    }

    pub fn pending_effect(&self) -> Option<&TurnEffect> {
        self.pending.as_ref().map(|pending| &pending.effect)
    }

    pub fn terminal(&self) -> Option<&TurnTerminal> {
        self.terminal.as_ref()
    }

    pub fn validate(&self) -> Result<(), TurnExecutorError> {
        if self.schema != TURN_CHECKPOINT_SCHEMA {
            return Err(TurnExecutorError::InvalidCheckpoint(format!(
                "unsupported schema {:?}",
                self.schema
            )));
        }
        if self.state.input.run_id != *self.state.machine.run_id()
            || self.state.input.turn_id != *self.state.machine.turn_id()
        {
            return Err(TurnExecutorError::InvalidCheckpoint(
                "turn input and machine identity differ".to_string(),
            ));
        }
        let turn_id = &self.state.input.turn_id;
        let mut previous = 0;
        let mut seen = BTreeSet::new();
        for key in &self.consumed_effects {
            if &key.turn_id != turn_id
                || key.sequence == 0
                || key.sequence > self.effect_sequence
                || key.sequence <= previous
                || !seen.insert(key.clone())
            {
                return Err(TurnExecutorError::InvalidCheckpoint(
                    "consumed effect keys are not strictly monotonic for this turn".to_string(),
                ));
            }
            previous = key.sequence;
        }
        if let Some(pending) = &self.pending {
            let key = pending.effect.key();
            if &key.turn_id != turn_id
                || key.sequence == 0
                || key.sequence != self.effect_sequence
                || seen.contains(key)
                || pending.effect.kind() != pending.continuation.kind()
            {
                return Err(TurnExecutorError::InvalidCheckpoint(
                    "pending effect identity or continuation is inconsistent".to_string(),
                ));
            }
        }
        if self.terminal.is_some() {
            if self.pending.is_some()
                || self.phase != ExecutorPhase::Terminal
                || self.state.machine.phase() != TurnPhase::Finished
            {
                return Err(TurnExecutorError::InvalidCheckpoint(
                    "terminal checkpoint retains nonterminal state".to_string(),
                ));
            }
        } else if self.phase == ExecutorPhase::Terminal
            || self.state.machine.phase() == TurnPhase::Finished
        {
            return Err(TurnExecutorError::InvalidCheckpoint(
                "nonterminal checkpoint has a terminal phase".to_string(),
            ));
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for TurnCheckpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            schema: String,
            state: TurnState,
            phase: ExecutorPhase,
            pending: Option<PendingEffect>,
            effect_sequence: u64,
            consumed_effects: Vec<EffectKey>,
            hook_repair_attempts: usize,
            pending_repair_message: Option<String>,
            cancellation_requested: bool,
            terminal: Option<TurnTerminal>,
        }

        let fields = Fields::deserialize(deserializer)?;
        let checkpoint = Self {
            schema: fields.schema,
            state: fields.state,
            phase: fields.phase,
            pending: fields.pending,
            effect_sequence: fields.effect_sequence,
            consumed_effects: fields.consumed_effects,
            hook_repair_attempts: fields.hook_repair_attempts,
            pending_repair_message: fields.pending_repair_message,
            cancellation_requested: fields.cancellation_requested,
            terminal: fields.terminal,
        };
        checkpoint.validate().map_err(D::Error::custom)?;
        Ok(checkpoint)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum ExecutorPhase {
    Initialize,
    ContextPrepare,
    PrepareModelRequest,
    StartModelRequest,
    CreateModelEffect {
        request_index: usize,
    },
    ModelResponseHook {
        request_index: usize,
        content: Content,
    },
    ParseModelResponse {
        content: Content,
    },
    ArtifactWriteHook {
        answer: String,
    },
    TurnFinishHook {
        answer: String,
    },
    ScheduleTranscript {
        output: TurnOutput,
        messages: Vec<TurnMessage>,
    },
    Terminal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingEffect {
    effect: TurnEffect,
    continuation: EffectContinuation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "continuation", rename_all = "snake_case", deny_unknown_fields)]
enum EffectContinuation {
    Hook {
        batch: TurnHookBatch,
        next: HookContinuation,
    },
    Model {
        request_index: usize,
    },
    Capability {
        tool_call: ToolCall,
        capability_name: String,
    },
    Transcript {
        output: TurnOutput,
    },
}

impl EffectContinuation {
    fn kind(&self) -> TurnEffectKind {
        match self {
            Self::Hook { .. } => TurnEffectKind::HookBatch,
            Self::Model { .. } => TurnEffectKind::ModelGeneration,
            Self::Capability { .. } => TurnEffectKind::CapabilityDispatch,
            Self::Transcript { .. } => TurnEffectKind::TranscriptAppend,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "next", rename_all = "snake_case", deny_unknown_fields)]
enum HookContinuation {
    ContextPrepare,
    ModelRequest {
        request_index: usize,
    },
    ModelResponse {
        request_index: usize,
        content: Content,
    },
    ArtifactWrite {
        answer: String,
    },
    TurnFinish {
        answer: String,
    },
}

#[derive(Clone, Debug)]
pub struct TurnExecutor {
    checkpoint: TurnCheckpoint,
}

impl TurnExecutor {
    pub fn new(input: TurnInput) -> Self {
        Self {
            checkpoint: TurnCheckpoint {
                schema: TURN_CHECKPOINT_SCHEMA.to_string(),
                state: TurnState::new(input),
                phase: ExecutorPhase::Initialize,
                pending: None,
                effect_sequence: 0,
                consumed_effects: Vec::new(),
                hook_repair_attempts: 0,
                pending_repair_message: None,
                cancellation_requested: false,
                terminal: None,
            },
        }
    }

    pub fn from_checkpoint(checkpoint: TurnCheckpoint) -> Result<Self, TurnExecutorError> {
        checkpoint.validate()?;
        Ok(Self { checkpoint })
    }

    pub fn checkpoint(&self) -> TurnCheckpoint {
        self.checkpoint.clone()
    }

    pub fn request_cancellation(&mut self) -> Result<(), TurnExecutorError> {
        if self.checkpoint.terminal.is_some() {
            return Err(TurnExecutorError::AlreadyTerminal);
        }
        self.checkpoint.cancellation_requested = true;
        Ok(())
    }

    pub fn next_effect(&mut self) -> Result<TurnAdvance, TurnExecutorError> {
        if let Some(pending) = &self.checkpoint.pending {
            return Ok(TurnAdvance {
                events: Vec::new(),
                state: TurnAdvanceState::Pending {
                    effect: pending.effect.clone(),
                },
            });
        }
        if let Some(terminal) = &self.checkpoint.terminal {
            return Ok(TurnAdvance {
                events: Vec::new(),
                state: TurnAdvanceState::Terminal {
                    terminal: terminal.clone(),
                },
            });
        }
        let mut events = Vec::new();
        self.drive(&mut events)?;
        self.advance(events)
    }

    pub fn resume(&mut self, result: TurnEffectResult) -> Result<TurnAdvance, TurnExecutorError> {
        if self
            .checkpoint
            .consumed_effects
            .iter()
            .any(|key| key == result.key())
        {
            return Err(TurnExecutorError::DuplicateEffectKey(result.key().clone()));
        }
        let pending = self
            .checkpoint
            .pending
            .as_ref()
            .ok_or(TurnExecutorError::NoPendingEffect)?;
        if pending.effect.key() != result.key() {
            return Err(TurnExecutorError::StaleEffectKey {
                expected: pending.effect.key().clone(),
                actual: result.key().clone(),
            });
        }
        if pending.effect.kind() != result.kind() {
            return Err(TurnExecutorError::MismatchedEffectResult {
                expected: pending.effect.kind(),
                actual: result.kind(),
            });
        }
        let pending = self
            .checkpoint
            .pending
            .take()
            .expect("pending effect was validated above");
        self.checkpoint.consumed_effects.push(result.key().clone());
        let mut events = Vec::new();
        self.consume_result(pending, result, &mut events)?;
        if self.checkpoint.terminal.is_none() {
            self.drive(&mut events)?;
        }
        self.advance(events)
    }

    fn advance(
        &self,
        events: Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<TurnAdvance, TurnExecutorError> {
        if let Some(pending) = &self.checkpoint.pending {
            Ok(TurnAdvance {
                events,
                state: TurnAdvanceState::Pending {
                    effect: pending.effect.clone(),
                },
            })
        } else if let Some(terminal) = &self.checkpoint.terminal {
            Ok(TurnAdvance {
                events,
                state: TurnAdvanceState::Terminal {
                    terminal: terminal.clone(),
                },
            })
        } else {
            Err(TurnExecutorError::Transition(
                "advancement produced neither an effect nor a terminal state".to_string(),
            ))
        }
    }

    fn drive(
        &mut self,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnExecutorError> {
        loop {
            if self.checkpoint.cancellation_requested {
                self.cancel(events)?;
                return Ok(());
            }
            match self.checkpoint.phase.clone() {
                ExecutorPhase::Initialize => {
                    let user_input = self.checkpoint.state.input.user_input.clone();
                    self.apply(TurnTransition::Start { user_input }, events)?;
                    self.checkpoint.phase = ExecutorPhase::ContextPrepare;
                }
                ExecutorPhase::ContextPrepare => {
                    let payload = context_prepare_payload(&self.checkpoint.state);
                    if self.schedule_hook(
                        HookEvent::ContextPrepare,
                        payload,
                        HookContinuation::ContextPrepare,
                        events,
                    )? {
                        return Ok(());
                    }
                    self.checkpoint.phase = ExecutorPhase::PrepareModelRequest;
                }
                ExecutorPhase::PrepareModelRequest => {
                    let message_count = self.checkpoint.state.messages.len();
                    self.apply(
                        TurnTransition::PrepareModelRequest { message_count },
                        events,
                    )?;
                    self.checkpoint.phase = ExecutorPhase::StartModelRequest;
                }
                ExecutorPhase::StartModelRequest => {
                    let request_index = self.checkpoint.state.request_index;
                    self.apply(TurnTransition::RequestModel { request_index }, events)?;
                    let payload = model_request_payload(&self.checkpoint.state, request_index);
                    if self.schedule_hook(
                        HookEvent::ModelRequest,
                        payload,
                        HookContinuation::ModelRequest { request_index },
                        events,
                    )? {
                        return Ok(());
                    }
                    self.checkpoint.phase = ExecutorPhase::CreateModelEffect { request_index };
                }
                ExecutorPhase::CreateModelEffect { request_index } => {
                    let mut messages = self.checkpoint.state.messages.clone();
                    if let Some(message) = self.checkpoint.pending_repair_message.take() {
                        messages.push(TurnMessage::System {
                            content: Content::text(message).map_err(|error| {
                                TurnExecutorError::Transition(error.to_string())
                            })?,
                        });
                    }
                    let request = ModelRequest {
                        run_id: self.checkpoint.state.input.run_id.clone(),
                        turn_id: self.checkpoint.state.input.turn_id.clone(),
                        request_index,
                        messages,
                        visible_tools: self.checkpoint.state.input.visible_tools.clone(),
                    };
                    self.set_pending(
                        |key| TurnEffect::ModelGeneration { key, request },
                        EffectContinuation::Model { request_index },
                    )?;
                    return Ok(());
                }
                ExecutorPhase::ModelResponseHook {
                    request_index,
                    content,
                } => {
                    let payload = model_response_payload(
                        &self.checkpoint.state,
                        request_index,
                        content.text_byte_len(),
                    );
                    if self.schedule_hook(
                        HookEvent::ModelResponse,
                        payload,
                        HookContinuation::ModelResponse {
                            request_index,
                            content: content.clone(),
                        },
                        events,
                    )? {
                        return Ok(());
                    }
                    self.checkpoint.phase = ExecutorPhase::ParseModelResponse { content };
                }
                ExecutorPhase::ParseModelResponse { content } => {
                    self.parse_model_response(content, events)?;
                    if self.checkpoint.pending.is_some() || self.checkpoint.terminal.is_some() {
                        return Ok(());
                    }
                }
                ExecutorPhase::ArtifactWriteHook { answer } => {
                    let payload = artifact_write_payload(&self.checkpoint.state, &answer);
                    if self.schedule_hook(
                        HookEvent::ArtifactWrite,
                        payload,
                        HookContinuation::ArtifactWrite {
                            answer: answer.clone(),
                        },
                        events,
                    )? {
                        return Ok(());
                    }
                    self.checkpoint.phase = ExecutorPhase::TurnFinishHook { answer };
                }
                ExecutorPhase::TurnFinishHook { answer } => {
                    let payload = turn_finish_payload(&self.checkpoint.state, answer.len());
                    if self.schedule_hook(
                        HookEvent::TurnFinish,
                        payload,
                        HookContinuation::TurnFinish {
                            answer: answer.clone(),
                        },
                        events,
                    )? {
                        return Ok(());
                    }
                    self.schedule_answer_transcript(answer)?;
                }
                ExecutorPhase::ScheduleTranscript { output, messages } => {
                    let continuation = EffectContinuation::Transcript {
                        output: output.clone(),
                    };
                    self.set_pending(
                        |key| TurnEffect::TranscriptAppend {
                            key,
                            messages,
                            output,
                        },
                        continuation,
                    )?;
                    return Ok(());
                }
                ExecutorPhase::Terminal => {
                    return Ok(());
                }
            }
        }
    }

    fn consume_result(
        &mut self,
        pending: PendingEffect,
        result: TurnEffectResult,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnExecutorError> {
        match (pending.continuation, result) {
            (
                EffectContinuation::Hook { batch, next },
                TurnEffectResult::HookBatch { outcome, .. },
            ) => self.consume_hook_result(batch, next, outcome, events),
            (
                EffectContinuation::Model { request_index },
                TurnEffectResult::ModelGeneration { outcome, .. },
            ) => match outcome {
                EffectOutcome::Succeeded(response) => {
                    self.checkpoint.state.request_index += 1;
                    self.apply(
                        TurnTransition::ReceiveModelResponse {
                            request_index,
                            content: response.content.clone(),
                        },
                        events,
                    )?;
                    self.checkpoint.phase = ExecutorPhase::ModelResponseHook {
                        request_index,
                        content: response.content,
                    };
                    Ok(())
                }
                EffectOutcome::Failed(failure) => self.fail_effect(
                    TurnFailureOperation::ModelRequest { request_index },
                    failure,
                    events,
                ),
                EffectOutcome::Cancelled => self.cancel(events),
            },
            (
                EffectContinuation::Capability {
                    tool_call,
                    capability_name,
                },
                TurnEffectResult::CapabilityDispatch { outcome, .. },
            ) => match outcome {
                EffectOutcome::Succeeded(response) => {
                    self.apply(
                        TurnTransition::FinishToolCall {
                            name: capability_name.clone(),
                            result: response.result.clone(),
                        },
                        events,
                    )?;
                    self.apply(
                        TurnTransition::AppendObservation {
                            name: capability_name,
                            result: response.result.clone(),
                        },
                        events,
                    )?;
                    self.checkpoint
                        .state
                        .append_tool_result(tool_call, response.result);
                    self.checkpoint.phase = ExecutorPhase::StartModelRequest;
                    Ok(())
                }
                EffectOutcome::Failed(failure) => self.fail_effect(
                    TurnFailureOperation::ToolDispatch {
                        name: capability_name,
                    },
                    failure,
                    events,
                ),
                EffectOutcome::Cancelled => self.cancel(events),
            },
            (
                EffectContinuation::Transcript { output },
                TurnEffectResult::TranscriptAppend { outcome, .. },
            ) => match outcome {
                EffectOutcome::Succeeded(()) => {
                    let status = match output {
                        TurnOutput::Answered { .. } => TurnTerminalStatus::Answered,
                        TurnOutput::Stopped { .. } => TurnTerminalStatus::Stopped,
                    };
                    self.apply(TurnTransition::Finish { status }, events)?;
                    self.finish(TurnTerminal::Completed { output });
                    Ok(())
                }
                EffectOutcome::Failed(failure) => {
                    self.fail_effect(TurnFailureOperation::TranscriptAppend, failure, events)
                }
                EffectOutcome::Cancelled => self.cancel(events),
            },
            _ => unreachable!("effect result kind was checked before consumption"),
        }
    }

    fn consume_hook_result(
        &mut self,
        batch: TurnHookBatch,
        next: HookContinuation,
        outcome: EffectOutcome<HookEffectOutput>,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnExecutorError> {
        match outcome {
            EffectOutcome::Cancelled => self.cancel(events),
            EffectOutcome::Failed(failure) => {
                let summary =
                    HookBatchSummary::failed_without_results(&batch, None, failure.code.as_str());
                self.apply(
                    TurnTransition::FinishHookBatch {
                        summary: summary.clone(),
                    },
                    events,
                )?;
                self.fail_hook(summary, failure, events)
            }
            EffectOutcome::Succeeded(output) => {
                if output.result.event != batch.event {
                    let summary = HookBatchSummary::failed_without_results(
                        &batch,
                        output.duration_ms,
                        "hook.event_mismatch",
                    );
                    self.apply(
                        TurnTransition::FinishHookBatch {
                            summary: summary.clone(),
                        },
                        events,
                    )?;
                    return self.fail_hook(
                        summary,
                        EffectFailure::new(
                            EffectFailureCode::Hook,
                            format!(
                                "hook batch `{}` returned mismatched event `{}`",
                                batch.event.as_str(),
                                output.result.event.as_str()
                            ),
                            false,
                        ),
                        events,
                    );
                }
                let result_for_repair = output.result.clone();
                let summary =
                    HookBatchSummary::from_batch_result(&batch, output.result, output.duration_ms);
                self.apply(
                    TurnTransition::FinishHookBatch {
                        summary: summary.clone(),
                    },
                    events,
                )?;
                match summary.outcome() {
                    HookBatchOutcome::Pass | HookBatchOutcome::Warn => {
                        self.continue_after_hook(next)
                    }
                    HookBatchOutcome::Repair => {
                        self.handle_hook_repair(next, summary, &result_for_repair, events)
                    }
                    HookBatchOutcome::Fail => self.fail_hook(
                        summary,
                        EffectFailure::new(
                            EffectFailureCode::Hook,
                            format!("required hook batch `{}` failed", batch.event.as_str()),
                            false,
                        ),
                        events,
                    ),
                }
            }
        }
    }

    fn continue_after_hook(&mut self, next: HookContinuation) -> Result<(), TurnExecutorError> {
        self.checkpoint.phase = match next {
            HookContinuation::ContextPrepare => ExecutorPhase::PrepareModelRequest,
            HookContinuation::ModelRequest { request_index } => {
                ExecutorPhase::CreateModelEffect { request_index }
            }
            HookContinuation::ModelResponse {
                request_index: _,
                content,
            } => ExecutorPhase::ParseModelResponse { content },
            HookContinuation::ArtifactWrite { answer } => ExecutorPhase::TurnFinishHook { answer },
            HookContinuation::TurnFinish { answer } => {
                self.schedule_answer_transcript(answer)?;
                return Ok(());
            }
        };
        Ok(())
    }

    fn handle_hook_repair(
        &mut self,
        next: HookContinuation,
        summary: HookBatchSummary,
        result: &HookBatchResult,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnExecutorError> {
        if matches!(next, HookContinuation::ArtifactWrite { .. })
            && self.checkpoint.hook_repair_attempts
                < self.checkpoint.state.input.max_hook_repair_attempts
        {
            self.checkpoint.hook_repair_attempts += 1;
            let attempt = self.checkpoint.hook_repair_attempts;
            self.apply(
                TurnTransition::PrepareRepair {
                    summary: summary.clone(),
                    attempt,
                },
                events,
            )?;
            self.checkpoint.pending_repair_message = Some(build_hook_repair_message(
                summary.event,
                &summary,
                Some(result),
            ));
            self.checkpoint.phase = ExecutorPhase::StartModelRequest;
            return Ok(());
        }

        let attempt = self
            .checkpoint
            .state
            .input
            .max_hook_repair_attempts
            .saturating_add(1)
            .max(1);
        self.apply(
            TurnTransition::PrepareRepair {
                summary: summary.clone(),
                attempt,
            },
            events,
        )?;
        self.fail_hook(
            summary,
            EffectFailure::new(
                EffectFailureCode::Hook,
                "hook repair was unsupported or exhausted",
                false,
            ),
            events,
        )
    }

    fn parse_model_response(
        &mut self,
        content: Content,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnExecutorError> {
        let text = content.text_only().ok_or_else(|| {
            TurnExecutorError::Transition(
                "unsupported_content: model responses must be text-only".to_string(),
            )
        })?;
        match agl_actions::parse_model_action(&text) {
            ModelAction::Answer(answer) => {
                self.apply(TurnTransition::ParseAnswer, events)?;
                self.apply(
                    TurnTransition::FinalAnswer {
                        answer: answer.clone(),
                    },
                    events,
                )?;
                self.checkpoint.phase = ExecutorPhase::ArtifactWriteHook { answer };
            }
            ModelAction::ToolCall(tool_call) => self.handle_tool_call(tool_call, events)?,
            ModelAction::MalformedToolCall(malformed) => {
                self.apply(
                    TurnTransition::DetectMalformedToolJson {
                        classification: malformed_kind(malformed.classification),
                        raw_json: malformed.raw_json,
                    },
                    events,
                )?;
                match malformed.repair {
                    Some(ToolJsonRepair::Succeeded {
                        strategy,
                        repaired_json,
                        tool_call,
                    }) => {
                        self.apply_repair_attempt(strategy, events)?;
                        self.apply(
                            TurnTransition::SucceedToolJsonRepair {
                                strategy: strategy.as_str().to_string(),
                                repaired_json,
                            },
                            events,
                        )?;
                        self.handle_tool_call(tool_call, events)?;
                    }
                    Some(ToolJsonRepair::Failed { strategy, message }) => {
                        self.apply_repair_attempt(strategy, events)?;
                        self.apply(
                            TurnTransition::FailToolJsonRepair {
                                strategy: strategy.as_str().to_string(),
                                message,
                            },
                            events,
                        )?;
                        self.stop(StopReason::ToolJsonUnrepairable, None, events)?;
                    }
                    None => {
                        self.apply_repair_attempt(RepairStrategy::None, events)?;
                        self.apply(
                            TurnTransition::FailToolJsonRepair {
                                strategy: RepairStrategy::None.as_str().to_string(),
                                message: "no repair returned".to_string(),
                            },
                            events,
                        )?;
                        self.stop(StopReason::ToolJsonUnrepairable, None, events)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn handle_tool_call(
        &mut self,
        tool_call: ToolCall,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnExecutorError> {
        self.apply(
            TurnTransition::ParseToolCall {
                name: tool_call.name.clone(),
            },
            events,
        )?;
        let dispatch = match decide_tool_call(&self.checkpoint.state, &tool_call) {
            ToolCallDecision::Dispatch(dispatch) => dispatch,
            ToolCallDecision::Stop(stop) => {
                self.emit_capability_denial(&stop, events)?;
                self.apply_tool_stop(&stop, events)?;
                return self.stop(stop.reason(), Some(stop.detail()), events);
            }
        };
        let capability_name = dispatch.capability_id.as_str().to_string();
        self.apply(
            TurnTransition::ValidateToolArgs {
                name: capability_name.clone(),
                arguments: dispatch.arguments.clone(),
            },
            events,
        )?;
        self.apply(
            TurnTransition::StartToolCall {
                name: capability_name.clone(),
                arguments: dispatch.arguments.clone(),
            },
            events,
        )?;
        self.set_pending(
            |key| TurnEffect::CapabilityDispatch {
                key,
                request: dispatch,
            },
            EffectContinuation::Capability {
                tool_call,
                capability_name,
            },
        )
    }

    fn schedule_hook(
        &mut self,
        event: HookEvent,
        payload: serde_json::Value,
        next: HookContinuation,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<bool, TurnExecutorError> {
        let batch = hook_batch_for_event(&self.checkpoint.state.input, event);
        if batch.is_empty() {
            return Ok(false);
        }
        let summary = batch.summary();
        self.apply(
            TurnTransition::PrepareHookBatch {
                summary: summary.clone(),
            },
            events,
        )?;
        self.apply(TurnTransition::RunHookBatch { summary }, events)?;
        let request = HookBatchRequest {
            event,
            hooks: batch.hook_ids(),
            payload,
        };
        self.set_pending(
            |key| TurnEffect::HookBatch { key, request },
            EffectContinuation::Hook { batch, next },
        )?;
        Ok(true)
    }

    fn schedule_answer_transcript(&mut self, answer: String) -> Result<(), TurnExecutorError> {
        let mut messages = self.checkpoint.state.messages.clone();
        messages.push(TurnMessage::Assistant {
            content: Content::text(answer.clone())
                .map_err(|error| TurnExecutorError::Transition(error.to_string()))?,
        });
        self.checkpoint.phase = ExecutorPhase::ScheduleTranscript {
            output: TurnOutput::Answered { answer },
            messages,
        };
        Ok(())
    }

    fn stop(
        &mut self,
        reason: StopReason,
        detail: Option<agl_turn::StopDetail>,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnExecutorError> {
        self.apply(
            TurnTransition::Stop {
                reason,
                visible: true,
            },
            events,
        )?;
        self.checkpoint.phase = ExecutorPhase::ScheduleTranscript {
            output: TurnOutput::Stopped { reason, detail },
            messages: self.checkpoint.state.messages.clone(),
        };
        Ok(())
    }

    fn fail_effect(
        &mut self,
        operation: TurnFailureOperation,
        failure: EffectFailure,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnExecutorError> {
        self.apply(
            TurnTransition::Fail {
                operation,
                message: failure.code.as_str().to_string(),
            },
            events,
        )?;
        self.apply(
            TurnTransition::Finish {
                status: TurnTerminalStatus::Failed,
            },
            events,
        )?;
        self.finish(TurnTerminal::Failed {
            failure: TurnExecutionFailure {
                code: failure.code,
                message: failure.message,
            },
        });
        Ok(())
    }

    fn fail_hook(
        &mut self,
        summary: HookBatchSummary,
        failure: EffectFailure,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnExecutorError> {
        self.apply(
            TurnTransition::RejectHookFailure {
                summary,
                message: failure.code.as_str().to_string(),
            },
            events,
        )?;
        self.apply(
            TurnTransition::Finish {
                status: TurnTerminalStatus::Failed,
            },
            events,
        )?;
        self.finish(TurnTerminal::Failed {
            failure: TurnExecutionFailure {
                code: failure.code,
                message: failure.message,
            },
        });
        Ok(())
    }

    fn cancel(
        &mut self,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnExecutorError> {
        self.apply(TurnTransition::Cancel, events)?;
        self.apply(
            TurnTransition::Finish {
                status: TurnTerminalStatus::Cancelled,
            },
            events,
        )?;
        self.finish(TurnTerminal::Cancelled);
        Ok(())
    }

    fn finish(&mut self, terminal: TurnTerminal) {
        self.checkpoint.phase = ExecutorPhase::Terminal;
        self.checkpoint.pending = None;
        self.checkpoint.terminal = Some(terminal);
    }

    fn set_pending(
        &mut self,
        effect: impl FnOnce(EffectKey) -> TurnEffect,
        continuation: EffectContinuation,
    ) -> Result<(), TurnExecutorError> {
        if self.checkpoint.pending.is_some() {
            return Err(TurnExecutorError::Transition(
                "attempted to expose two pending effects".to_string(),
            ));
        }
        self.checkpoint.effect_sequence = self
            .checkpoint
            .effect_sequence
            .checked_add(1)
            .ok_or_else(|| TurnExecutorError::Transition("effect sequence overflow".to_string()))?;
        let key = EffectKey {
            turn_id: self.checkpoint.state.input.turn_id.clone(),
            sequence: self.checkpoint.effect_sequence,
        };
        self.checkpoint.pending = Some(PendingEffect {
            effect: effect(key),
            continuation,
        });
        Ok(())
    }

    fn apply(
        &mut self,
        transition: TurnTransition,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnExecutorError> {
        let record = self
            .checkpoint
            .state
            .apply_transition(transition)
            .map_err(|error| TurnExecutorError::Transition(error.to_string()))?;
        let scope = EventScope::builder(record.run_id.clone())
            .turn_id(record.turn_id.clone())
            .build()
            .map_err(|error| TurnExecutorError::Transition(error.to_string()))?;
        events.push(EventDraft::new(scope, event_for_record(&record)));
        Ok(())
    }

    fn apply_repair_attempt(
        &mut self,
        strategy: RepairStrategy,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnExecutorError> {
        self.apply(
            TurnTransition::AttemptToolJsonRepair {
                strategy: strategy.as_str().to_string(),
            },
            events,
        )
    }

    fn apply_tool_stop(
        &mut self,
        stop: &ToolCallStop,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnExecutorError> {
        let transition = match stop {
            ToolCallStop::ToolLimitReached { limit } => {
                TurnTransition::RejectToolLimit { limit: *limit }
            }
            ToolCallStop::HiddenTool { name } => {
                TurnTransition::RejectHiddenTool { name: name.clone() }
            }
            ToolCallStop::InvalidArguments { name, message } => TurnTransition::RejectToolArgs {
                name: name.clone(),
                message: message.clone(),
            },
        };
        self.apply(transition, events)
    }

    fn emit_capability_denial(
        &self,
        stop: &ToolCallStop,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnExecutorError> {
        let Some(policy_hash) = &self.checkpoint.state.input.capability_policy_hash else {
            return Ok(());
        };
        let (capability_id, code) = match stop {
            ToolCallStop::HiddenTool { name } => (
                agl_capabilities::CapabilityId::new(name.clone())
                    .ok()
                    .map(|id| id.as_str().to_string()),
                DispatchDenialCode::CapabilityNotEffective,
            ),
            ToolCallStop::InvalidArguments { name, .. } => (
                agl_capabilities::CapabilityId::new(name.clone())
                    .ok()
                    .map(|id| id.as_str().to_string()),
                DispatchDenialCode::InvalidArguments,
            ),
            ToolCallStop::ToolLimitReached { .. } => return Ok(()),
        };
        let scope = EventScope::builder(self.checkpoint.state.input.run_id.clone())
            .turn_id(self.checkpoint.state.input.turn_id.clone())
            .build()
            .map_err(|error| TurnExecutorError::Transition(error.to_string()))?;
        events.push(EventDraft::new(
            scope,
            RuntimeEvent::CapabilityCallDenied {
                policy_hash: policy_hash.clone(),
                capability_id,
                reason_code: code.as_str().to_string(),
            },
        ));
        Ok(())
    }
}

fn hook_batch_for_event(input: &TurnInput, event: HookEvent) -> TurnHookBatch {
    let mut batch = TurnHookBatch::new(event);
    for configured in input
        .hook_batches
        .iter()
        .filter(|batch| batch.event == event)
    {
        batch
            .required_hooks
            .extend(configured.required_hooks.iter().cloned());
        batch
            .optional_hooks
            .extend(configured.optional_hooks.iter().cloned());
    }
    batch
}

fn context_prepare_payload(state: &TurnState) -> serde_json::Value {
    json!({
        "turn_id": state.input.turn_id,
        "message_count": state.messages.len(),
        "visible_tool_count": state.input.visible_tools.len(),
    })
}

fn model_request_payload(state: &TurnState, request_index: usize) -> serde_json::Value {
    json!({
        "turn_id": state.input.turn_id,
        "request_index": request_index,
        "message_count": state.messages.len(),
        "visible_tool_count": state.input.visible_tools.len(),
    })
}

fn model_response_payload(
    state: &TurnState,
    request_index: usize,
    content_bytes: usize,
) -> serde_json::Value {
    json!({
        "turn_id": state.input.turn_id,
        "request_index": request_index,
        "content_bytes": content_bytes,
    })
}

fn artifact_write_payload(state: &TurnState, answer: &str) -> serde_json::Value {
    let mut payload = json!({
        "turn_id": state.input.turn_id,
        "artifact_kind": "answer",
        "content": answer,
        "content_bytes": answer.len(),
    });
    merge_hook_payload(&mut payload, &state.input.hook_payload);
    payload
}

fn turn_finish_payload(state: &TurnState, answer_bytes: usize) -> serde_json::Value {
    json!({
        "turn_id": state.input.turn_id,
        "answer_bytes": answer_bytes,
    })
}

fn merge_hook_payload(payload: &mut serde_json::Value, extra: &serde_json::Value) {
    let (Some(payload), Some(extra)) = (payload.as_object_mut(), extra.as_object()) else {
        return;
    };
    for (key, value) in extra {
        payload.insert(key.clone(), value.clone());
    }
}

fn build_hook_repair_message(
    event: HookEvent,
    summary: &HookBatchSummary,
    result: Option<&HookBatchResult>,
) -> String {
    let fixes = result
        .into_iter()
        .flat_map(|result| result.results.iter())
        .flat_map(|result| result.messages.iter())
        .filter_map(|message| message.fix.as_deref())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let codes = summary.message_codes.join(", ");
    let mut message = format!(
        "The previous answer failed AgentLIBRE hook validation for `{}`.",
        event.as_str()
    );
    if !codes.is_empty() {
        message.push_str(" Message codes: ");
        message.push_str(&codes);
        message.push('.');
    }
    if !fixes.is_empty() {
        message.push_str(" Required fix: ");
        message.push_str(&fixes.join(" "));
    }
    message.push_str(" Return a corrected final answer.");
    message
}
