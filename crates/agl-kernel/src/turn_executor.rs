use std::collections::BTreeSet;

use crate::turn_policy::{InvalidToolArguments, ToolCallDecision, ToolCallStop, decide_tool_call};
use crate::{DispatchDenialCode, ToolOutcome};
use crate::{
    HookBatchOutcome, HookBatchSummary, IncompleteOutputReason, ModelRequest, ModelResponseOutcome,
    StopReason, ToolDispatchRequest, TurnFailureOperation, TurnHookBatch, TurnInput, TurnMessage,
    TurnOutput, TurnPhase, TurnState, TurnTerminalStatus, TurnTransition,
};
use crate::{HookBatchRequest, HookBatchResult, HookEvent};
use agl_actions::{ParsedModelOutput, RepairStrategy, ToolCall, ToolJsonRepair};
use agl_content::Content;
use agl_events::{EventDraft, EventScope, RuntimeEvent};
use agl_ids::MessageId;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::json;

use crate::turn_event::{event_for_record, malformed_kind};
use crate::turn_request::{
    HookRequestOutput, TurnAdvance, TurnAdvanceState, TurnExecutionFailure, TurnMachineError,
    TurnRequest, TurnRequestFailure, TurnRequestFailureCode, TurnRequestKey, TurnRequestKind,
    TurnRequestOutcome, TurnRequestResult, TurnTerminal,
};

pub const TURN_CHECKPOINT_SCHEMA: &str = "agentlibre.turn-checkpoint.v1alpha";

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TurnCheckpoint {
    schema: String,
    state: TurnState,
    pending: Option<PendingRequest>,
    request_sequence: u64,
    consumed_requests: Vec<TurnRequestKey>,
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

    pub fn pending_request(&self) -> Option<&TurnRequest> {
        self.pending.as_ref().map(|pending| &pending.request)
    }

    pub fn terminal(&self) -> Option<&TurnTerminal> {
        self.terminal.as_ref()
    }

    pub fn validate(&self) -> Result<(), TurnMachineError> {
        if self.schema != TURN_CHECKPOINT_SCHEMA {
            return Err(TurnMachineError::InvalidCheckpoint(format!(
                "unsupported schema {:?}",
                self.schema
            )));
        }
        if self.state.input.run_id != *self.state.transition_state.run_id()
            || self.state.input.turn_id != *self.state.transition_state.turn_id()
        {
            return Err(TurnMachineError::InvalidCheckpoint(
                "turn input and machine identity differ".to_string(),
            ));
        }
        let turn_id = &self.state.input.turn_id;
        let mut previous = 0;
        let mut seen = BTreeSet::new();
        for key in &self.consumed_requests {
            if &key.turn_id != turn_id
                || key.sequence == 0
                || key.sequence > self.request_sequence
                || key.sequence <= previous
                || !seen.insert(key.clone())
            {
                return Err(TurnMachineError::InvalidCheckpoint(
                    "consumed request keys are not strictly monotonic for this turn".to_string(),
                ));
            }
            previous = key.sequence;
        }
        if let Some(pending) = &self.pending {
            let key = pending.request.key();
            if &key.turn_id != turn_id
                || key.sequence == 0
                || key.sequence != self.request_sequence
                || seen.contains(key)
                || pending.request.kind() != pending.continuation.kind()
            {
                return Err(TurnMachineError::InvalidCheckpoint(
                    "pending request identity or continuation is inconsistent".to_string(),
                ));
            }
        }
        if self.terminal.is_some() {
            if self.pending.is_some()
                || self.state.continuation != TurnContinuation::Terminal
                || self.state.transition_state.phase() != TurnPhase::Finished
            {
                return Err(TurnMachineError::InvalidCheckpoint(
                    "terminal checkpoint retains nonterminal state".to_string(),
                ));
            }
        } else if self.state.continuation == TurnContinuation::Terminal
            || self.state.transition_state.phase() == TurnPhase::Finished
        {
            return Err(TurnMachineError::InvalidCheckpoint(
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
            pending: Option<PendingRequest>,
            request_sequence: u64,
            consumed_requests: Vec<TurnRequestKey>,
            cancellation_requested: bool,
            terminal: Option<TurnTerminal>,
        }

        let fields = Fields::deserialize(deserializer)?;
        let checkpoint = Self {
            schema: fields.schema,
            state: fields.state,
            pending: fields.pending,
            request_sequence: fields.request_sequence,
            consumed_requests: fields.consumed_requests,
            cancellation_requested: fields.cancellation_requested,
            terminal: fields.terminal,
        };
        checkpoint.validate().map_err(D::Error::custom)?;
        Ok(checkpoint)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TurnContinuation {
    Initialize,
    ContextPrepare,
    PrepareModelRequest,
    StartModelRequest,
    CreateModelRequest {
        request_index: usize,
    },
    ModelResponseHook {
        request_index: usize,
        content: Content,
        outcome: ModelResponseOutcome,
        provisional_message_id: MessageId,
    },
    ParseModelResponse {
        content: Content,
        provisional_message_id: MessageId,
    },
    ToolBeforeHook {
        tool_call: ToolCall,
        dispatch: ToolDispatchRequest,
        tool_name: String,
    },
    ToolAfterHook {
        tool_call: ToolCall,
        tool_name: String,
        outcome: Box<ToolOutcome>,
    },
    ArtifactWriteHook {
        answer: String,
        provisional_message_id: MessageId,
    },
    TurnFinishHook {
        answer: String,
        provisional_message_id: MessageId,
    },
    ScheduleTranscript {
        output: TurnOutput,
        messages: Vec<TurnMessage>,
        assistant_message_id: Option<MessageId>,
    },
    Terminal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingRequest {
    request: TurnRequest,
    continuation: RequestContinuation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "continuation", rename_all = "snake_case", deny_unknown_fields)]
enum RequestContinuation {
    Hook {
        batch: TurnHookBatch,
        next: Box<HookContinuation>,
    },
    Model {
        request_index: usize,
    },
    Tool {
        tool_call: ToolCall,
        tool_name: String,
    },
    Transcript {
        output: TurnOutput,
    },
}

impl RequestContinuation {
    fn kind(&self) -> TurnRequestKind {
        match self {
            Self::Hook { .. } => TurnRequestKind::HookBatch,
            Self::Model { .. } => TurnRequestKind::ModelGeneration,
            Self::Tool { .. } => TurnRequestKind::ToolDispatch,
            Self::Transcript { .. } => TurnRequestKind::TranscriptAppend,
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
        outcome: ModelResponseOutcome,
        provisional_message_id: MessageId,
    },
    ToolBefore {
        tool_call: ToolCall,
        dispatch: ToolDispatchRequest,
        tool_name: String,
    },
    ToolAfter {
        tool_call: ToolCall,
        tool_name: String,
        outcome: Box<ToolOutcome>,
    },
    ArtifactWrite {
        answer: String,
        provisional_message_id: MessageId,
    },
    TurnFinish {
        answer: String,
        provisional_message_id: MessageId,
    },
}

#[derive(Clone, Debug)]
pub struct TurnMachine {
    checkpoint: TurnCheckpoint,
}

impl TurnMachine {
    pub fn new(input: TurnInput) -> Self {
        Self {
            checkpoint: TurnCheckpoint {
                schema: TURN_CHECKPOINT_SCHEMA.to_string(),
                state: TurnState::new(input),
                pending: None,
                request_sequence: 0,
                consumed_requests: Vec::new(),
                cancellation_requested: false,
                terminal: None,
            },
        }
    }

    pub fn from_checkpoint(checkpoint: TurnCheckpoint) -> Result<Self, TurnMachineError> {
        checkpoint.validate()?;
        Ok(Self { checkpoint })
    }

    pub fn checkpoint(&self) -> TurnCheckpoint {
        self.checkpoint.clone()
    }

    pub fn request_cancellation(&mut self) -> Result<(), TurnMachineError> {
        if self.checkpoint.terminal.is_some() {
            return Err(TurnMachineError::AlreadyTerminal);
        }
        self.checkpoint.cancellation_requested = true;
        Ok(())
    }

    pub fn next_request(&mut self) -> Result<TurnAdvance, TurnMachineError> {
        validate_hook_requirements(&self.checkpoint.state.input)?;
        if let Some(pending) = &self.checkpoint.pending {
            return Ok(TurnAdvance {
                events: Vec::new(),
                state: TurnAdvanceState::Pending {
                    request: pending.request.clone(),
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

    pub fn resume(&mut self, result: TurnRequestResult) -> Result<TurnAdvance, TurnMachineError> {
        if self
            .checkpoint
            .consumed_requests
            .iter()
            .any(|key| key == result.key())
        {
            return Err(TurnMachineError::DuplicateRequestKey(result.key().clone()));
        }
        let pending = self
            .checkpoint
            .pending
            .as_ref()
            .ok_or(TurnMachineError::NoPendingRequest)?;
        if pending.request.key() != result.key() {
            return Err(TurnMachineError::StaleRequestKey {
                expected: pending.request.key().clone(),
                actual: result.key().clone(),
            });
        }
        if pending.request.kind() != result.kind() {
            return Err(TurnMachineError::MismatchedRequestResult {
                expected: pending.request.kind(),
                actual: result.kind(),
            });
        }
        if let (
            RequestContinuation::Hook { batch, .. },
            TurnRequestResult::HookBatch {
                outcome: TurnRequestOutcome::Succeeded(output),
                ..
            },
        ) = (&pending.continuation, &result)
        {
            validate_hook_result(batch, &output.result)?;
        }
        let pending = self
            .checkpoint
            .pending
            .take()
            .expect("pending request was validated above");
        self.checkpoint.consumed_requests.push(result.key().clone());
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
    ) -> Result<TurnAdvance, TurnMachineError> {
        if let Some(pending) = &self.checkpoint.pending {
            Ok(TurnAdvance {
                events,
                state: TurnAdvanceState::Pending {
                    request: pending.request.clone(),
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
            Err(TurnMachineError::Transition(
                "advancement produced neither an request nor a terminal state".to_string(),
            ))
        }
    }

    fn drive(
        &mut self,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
        loop {
            if self.checkpoint.pending.is_some() {
                return Ok(());
            }
            if self.checkpoint.cancellation_requested {
                self.cancel(events)?;
                return Ok(());
            }
            match self.checkpoint.state.continuation.clone() {
                TurnContinuation::Initialize => {
                    let user_input = self.checkpoint.state.input.user_input.clone();
                    self.apply(TurnTransition::Start { user_input }, events)?;
                    self.checkpoint.state.continuation = TurnContinuation::ContextPrepare;
                }
                TurnContinuation::ContextPrepare => {
                    let payload = context_prepare_payload(&self.checkpoint.state);
                    if self.schedule_hook(
                        HookEvent::ContextPrepare,
                        payload,
                        HookContinuation::ContextPrepare,
                        events,
                    )? {
                        return Ok(());
                    }
                    self.checkpoint.state.continuation = TurnContinuation::PrepareModelRequest;
                }
                TurnContinuation::PrepareModelRequest => {
                    let message_count = self.checkpoint.state.messages.len();
                    self.apply(
                        TurnTransition::PrepareModelRequest { message_count },
                        events,
                    )?;
                    self.checkpoint.state.continuation = TurnContinuation::StartModelRequest;
                }
                TurnContinuation::StartModelRequest => {
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
                    self.checkpoint.state.continuation =
                        TurnContinuation::CreateModelRequest { request_index };
                }
                TurnContinuation::CreateModelRequest { request_index } => {
                    let request = ModelRequest {
                        run_id: self.checkpoint.state.input.run_id.clone(),
                        turn_id: self.checkpoint.state.input.turn_id.clone(),
                        request_index,
                        messages: self.checkpoint.state.messages.clone(),
                        visible_tools: self.checkpoint.state.input.visible_tools.clone(),
                    };
                    self.set_pending(
                        |key| TurnRequest::ModelGeneration {
                            key,
                            provisional_message_id: MessageId::generate(),
                            request,
                        },
                        RequestContinuation::Model { request_index },
                    )?;
                    return Ok(());
                }
                TurnContinuation::ModelResponseHook {
                    request_index,
                    content,
                    outcome,
                    provisional_message_id,
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
                            outcome,
                            provisional_message_id: provisional_message_id.clone(),
                        },
                        events,
                    )? {
                        return Ok(());
                    }
                    match outcome {
                        ModelResponseOutcome::Complete => {
                            self.checkpoint.state.continuation =
                                TurnContinuation::ParseModelResponse {
                                    content,
                                    provisional_message_id,
                                };
                        }
                        ModelResponseOutcome::Incomplete { reason } => {
                            self.schedule_incomplete_transcript(
                                content,
                                reason,
                                provisional_message_id,
                                events,
                            )?;
                        }
                    }
                }
                TurnContinuation::ParseModelResponse {
                    content,
                    provisional_message_id,
                } => {
                    self.parse_model_response(content, provisional_message_id, events)?;
                    if self.checkpoint.pending.is_some() || self.checkpoint.terminal.is_some() {
                        return Ok(());
                    }
                }
                TurnContinuation::ToolBeforeHook {
                    tool_call,
                    dispatch,
                    tool_name,
                } => {
                    let payload = tool_before_payload(&self.checkpoint.state, &dispatch);
                    if self.schedule_hook(
                        HookEvent::ToolCallBefore,
                        payload,
                        HookContinuation::ToolBefore {
                            tool_call: tool_call.clone(),
                            dispatch: dispatch.clone(),
                            tool_name: tool_name.clone(),
                        },
                        events,
                    )? {
                        return Ok(());
                    }
                    self.schedule_tool(tool_call, dispatch, tool_name, events)?;
                    return Ok(());
                }
                TurnContinuation::ToolAfterHook {
                    tool_call,
                    tool_name,
                    outcome,
                } => {
                    let payload = tool_after_payload(&self.checkpoint.state, &outcome);
                    if self.schedule_hook(
                        HookEvent::ToolCallAfter,
                        payload,
                        HookContinuation::ToolAfter {
                            tool_call: tool_call.clone(),
                            tool_name: tool_name.clone(),
                            outcome: outcome.clone(),
                        },
                        events,
                    )? {
                        return Ok(());
                    }
                    self.finish_tool(tool_call, tool_name, *outcome, events)?;
                }
                TurnContinuation::ArtifactWriteHook {
                    answer,
                    provisional_message_id,
                } => {
                    let payload = artifact_write_payload(&self.checkpoint.state, &answer);
                    if self.schedule_hook(
                        HookEvent::ArtifactWrite,
                        payload,
                        HookContinuation::ArtifactWrite {
                            answer: answer.clone(),
                            provisional_message_id: provisional_message_id.clone(),
                        },
                        events,
                    )? {
                        return Ok(());
                    }
                    self.checkpoint.state.continuation = TurnContinuation::TurnFinishHook {
                        answer,
                        provisional_message_id,
                    };
                }
                TurnContinuation::TurnFinishHook {
                    answer,
                    provisional_message_id,
                } => {
                    let payload = turn_finish_payload(&self.checkpoint.state, answer.len());
                    if self.schedule_hook(
                        HookEvent::TurnFinish,
                        payload,
                        HookContinuation::TurnFinish {
                            answer: answer.clone(),
                            provisional_message_id: provisional_message_id.clone(),
                        },
                        events,
                    )? {
                        return Ok(());
                    }
                    self.schedule_answer_transcript(answer, provisional_message_id)?;
                }
                TurnContinuation::ScheduleTranscript {
                    output,
                    messages,
                    assistant_message_id,
                } => {
                    let continuation = RequestContinuation::Transcript {
                        output: output.clone(),
                    };
                    self.set_pending(
                        |key| TurnRequest::TranscriptAppend {
                            key,
                            assistant_message_id,
                            messages,
                            output,
                        },
                        continuation,
                    )?;
                    return Ok(());
                }
                TurnContinuation::Terminal => {
                    return Ok(());
                }
            }
        }
    }

    fn consume_result(
        &mut self,
        pending: PendingRequest,
        result: TurnRequestResult,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
        let provisional_message_id = match &pending.request {
            TurnRequest::ModelGeneration {
                provisional_message_id,
                ..
            } => Some(provisional_message_id.clone()),
            _ => None,
        };
        match (pending.continuation, result) {
            (
                RequestContinuation::Hook { batch, next },
                TurnRequestResult::HookBatch { outcome, .. },
            ) => self.consume_hook_result(batch, *next, outcome, events),
            (
                RequestContinuation::Model { request_index },
                TurnRequestResult::ModelGeneration { outcome, .. },
            ) => match outcome {
                TurnRequestOutcome::Succeeded(response) => {
                    let provisional_message_id = provisional_message_id.expect(
                        "model continuation was validated against a model generation request",
                    );
                    self.checkpoint.state.request_index += 1;
                    self.apply(
                        TurnTransition::ReceiveModelResponse {
                            request_index,
                            content: response.content.clone(),
                        },
                        events,
                    )?;
                    self.checkpoint.state.continuation = TurnContinuation::ModelResponseHook {
                        request_index,
                        content: response.content,
                        outcome: response.outcome,
                        provisional_message_id,
                    };
                    Ok(())
                }
                TurnRequestOutcome::Failed(failure) => self.fail_effect(
                    TurnFailureOperation::ModelRequest { request_index },
                    failure,
                    events,
                ),
                TurnRequestOutcome::Cancelled => self.cancel(events),
            },
            (
                RequestContinuation::Tool {
                    tool_call,
                    tool_name,
                },
                TurnRequestResult::ToolDispatch { outcome, .. },
            ) => match *outcome {
                TurnRequestOutcome::Succeeded(response) => {
                    self.checkpoint.state.continuation = TurnContinuation::ToolAfterHook {
                        tool_call,
                        tool_name,
                        outcome: Box::new(response.result),
                    };
                    Ok(())
                }
                TurnRequestOutcome::Failed(failure) => self.fail_effect(
                    TurnFailureOperation::ToolDispatch { name: tool_name },
                    failure,
                    events,
                ),
                TurnRequestOutcome::Cancelled => self.cancel(events),
            },
            (
                RequestContinuation::Transcript { output },
                TurnRequestResult::TranscriptAppend { outcome, .. },
            ) => match outcome {
                TurnRequestOutcome::Succeeded(()) => {
                    let status = match output {
                        TurnOutput::Answered { .. } => TurnTerminalStatus::Answered,
                        TurnOutput::Incomplete { .. } => TurnTerminalStatus::IncompleteOutput,
                        TurnOutput::Stopped { .. } => TurnTerminalStatus::Stopped,
                    };
                    self.apply(TurnTransition::Finish { status }, events)?;
                    self.finish(TurnTerminal::Completed { output });
                    Ok(())
                }
                TurnRequestOutcome::Failed(failure) => {
                    self.fail_effect(TurnFailureOperation::TranscriptAppend, failure, events)
                }
                TurnRequestOutcome::Cancelled => self.cancel(events),
            },
            _ => unreachable!("request result kind was checked before consumption"),
        }
    }

    fn consume_hook_result(
        &mut self,
        batch: TurnHookBatch,
        next: HookContinuation,
        outcome: TurnRequestOutcome<HookRequestOutput>,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
        match outcome {
            TurnRequestOutcome::Cancelled => self.cancel(events),
            TurnRequestOutcome::Failed(failure) => {
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
            TurnRequestOutcome::Succeeded(output) => {
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
                        TurnRequestFailure::new(
                            TurnRequestFailureCode::Hook,
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
                        self.continue_after_hook(next, events)
                    }
                    HookBatchOutcome::Repair => {
                        self.handle_hook_repair(next, summary, &result_for_repair, events)
                    }
                    HookBatchOutcome::Fail => self.fail_hook(
                        summary,
                        TurnRequestFailure::new(
                            TurnRequestFailureCode::Hook,
                            format!("required hook batch `{}` failed", batch.event.as_str()),
                            false,
                        ),
                        events,
                    ),
                }
            }
        }
    }

    fn continue_after_hook(
        &mut self,
        next: HookContinuation,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
        self.checkpoint.state.continuation = match next {
            HookContinuation::ContextPrepare => TurnContinuation::PrepareModelRequest,
            HookContinuation::ModelRequest { request_index } => {
                TurnContinuation::CreateModelRequest { request_index }
            }
            HookContinuation::ModelResponse {
                request_index: _,
                content,
                outcome,
                provisional_message_id,
            } => match outcome {
                ModelResponseOutcome::Complete => TurnContinuation::ParseModelResponse {
                    content,
                    provisional_message_id,
                },
                ModelResponseOutcome::Incomplete { reason } => {
                    self.schedule_incomplete_transcript(
                        content,
                        reason,
                        provisional_message_id,
                        events,
                    )?;
                    return Ok(());
                }
            },
            HookContinuation::ToolBefore {
                tool_call,
                dispatch,
                tool_name,
            } => {
                self.schedule_tool(tool_call, dispatch, tool_name, events)?;
                return Ok(());
            }
            HookContinuation::ToolAfter {
                tool_call,
                tool_name,
                outcome,
            } => {
                self.finish_tool(tool_call, tool_name, *outcome, events)?;
                return Ok(());
            }
            HookContinuation::ArtifactWrite {
                answer,
                provisional_message_id,
            } => TurnContinuation::TurnFinishHook {
                answer,
                provisional_message_id,
            },
            HookContinuation::TurnFinish {
                answer,
                provisional_message_id,
            } => {
                self.schedule_answer_transcript(answer, provisional_message_id)?;
                return Ok(());
            }
        };
        Ok(())
    }

    fn handle_hook_repair(
        &mut self,
        _next: HookContinuation,
        summary: HookBatchSummary,
        result: &HookBatchResult,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
        let messages = result
            .results
            .iter()
            .flat_map(|result| result.messages.iter().cloned())
            .collect();
        self.stop(
            StopReason::RepairRequired,
            Some(crate::StopDetail::RepairRequired {
                event: summary.event.as_str().to_string(),
                messages,
            }),
            events,
        )
    }

    fn parse_model_response(
        &mut self,
        content: Content,
        provisional_message_id: MessageId,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
        let text = content.text_only().ok_or_else(|| {
            TurnMachineError::Transition(
                "unsupported_content: model responses must be text-only".to_string(),
            )
        })?;
        match agl_actions::parse_model_output(&text) {
            ParsedModelOutput::Answer(answer) => {
                self.apply(TurnTransition::ParseAnswer, events)?;
                self.apply(
                    TurnTransition::FinalAnswer {
                        answer: answer.clone(),
                    },
                    events,
                )?;
                self.checkpoint.state.continuation = TurnContinuation::ArtifactWriteHook {
                    answer,
                    provisional_message_id,
                };
            }
            ParsedModelOutput::ToolCall(tool_call) => self.handle_tool_call(tool_call, events)?,
            ParsedModelOutput::MalformedToolCall(malformed) => {
                self.apply(
                    TurnTransition::DetectMalformedToolJson {
                        classification: malformed_kind(malformed.classification),
                        raw_json: malformed.raw_json,
                    },
                    events,
                )?;
                if !self.checkpoint.state.input.repair_malformed_tool_calls {
                    self.stop(StopReason::ToolJsonUnrepairable, None, events)?;
                    return Ok(());
                }
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
    ) -> Result<(), TurnMachineError> {
        self.apply(
            TurnTransition::ParseToolCall {
                name: tool_call.name.clone(),
            },
            events,
        )?;
        let dispatch = match decide_tool_call(&self.checkpoint.state, &tool_call) {
            ToolCallDecision::Dispatch(dispatch) => dispatch,
            ToolCallDecision::ObserveInvalidArguments(invalid) => {
                self.emit_invalid_argument_denial(&invalid, events)?;
                self.apply(
                    TurnTransition::RejectToolArgs {
                        name: invalid.name.clone(),
                        message: invalid.message.clone(),
                    },
                    events,
                )?;
                let result = invalid.observation_result();
                self.apply(
                    TurnTransition::AppendInvalidToolArgsObservation {
                        name: invalid.name,
                        result: result.clone(),
                    },
                    events,
                )?;
                self.checkpoint.state.append_tool_result(tool_call, result);
                self.checkpoint.state.continuation = TurnContinuation::StartModelRequest;
                return Ok(());
            }
            ToolCallDecision::Stop(stop) => {
                self.emit_tool_denial(&stop, events)?;
                self.apply_tool_stop(&stop, events)?;
                return self.stop(stop.reason(), Some(stop.detail()), events);
            }
        };
        let tool_name = dispatch.tool_id.as_str().to_string();
        self.apply(
            TurnTransition::ValidateToolArgs {
                name: tool_name.clone(),
                arguments: dispatch.arguments.clone(),
            },
            events,
        )?;
        self.checkpoint.state.continuation = TurnContinuation::ToolBeforeHook {
            tool_call,
            dispatch,
            tool_name,
        };
        Ok(())
    }

    fn schedule_tool(
        &mut self,
        tool_call: ToolCall,
        dispatch: ToolDispatchRequest,
        tool_name: String,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
        self.apply(
            TurnTransition::StartToolCall {
                name: tool_name.clone(),
                arguments: dispatch.arguments.clone(),
            },
            events,
        )?;
        self.set_pending(
            |key| TurnRequest::ToolDispatch {
                key,
                request: dispatch,
            },
            RequestContinuation::Tool {
                tool_call,
                tool_name,
            },
        )
    }

    fn finish_tool(
        &mut self,
        tool_call: ToolCall,
        tool_name: String,
        outcome: ToolOutcome,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
        if let Some(event) = &outcome.workflow_event {
            let event = crate::KernelWorkflowEvent::parse(event).ok_or_else(|| {
                TurnMachineError::Transition(format!(
                    "Tool outcome requested unknown workflow event `{event}`"
                ))
            })?;
            if event != crate::KernelWorkflowEvent::ToolObservationAppend {
                return Err(TurnMachineError::Transition(format!(
                    "workflow event `{}` is not legal after a Tool call",
                    event.id()
                )));
            }
        }
        let observation = outcome.observation_result();
        self.apply(
            TurnTransition::FinishToolCall {
                name: tool_name.clone(),
                result: observation.clone(),
            },
            events,
        )?;
        self.apply(
            TurnTransition::AppendObservation {
                name: tool_name,
                result: observation.clone(),
            },
            events,
        )?;
        self.checkpoint
            .state
            .append_tool_result(tool_call, observation);
        self.checkpoint.state.continuation = TurnContinuation::StartModelRequest;
        Ok(())
    }

    fn schedule_hook(
        &mut self,
        event: HookEvent,
        payload: serde_json::Value,
        next: HookContinuation,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<bool, TurnMachineError> {
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
            |key| TurnRequest::HookBatch { key, request },
            RequestContinuation::Hook {
                batch,
                next: Box::new(next),
            },
        )?;
        Ok(true)
    }

    fn schedule_answer_transcript(
        &mut self,
        answer: String,
        provisional_message_id: MessageId,
    ) -> Result<(), TurnMachineError> {
        let mut messages = self.checkpoint.state.messages.clone();
        messages.push(TurnMessage::Assistant {
            content: Content::text(answer.clone())
                .map_err(|error| TurnMachineError::Transition(error.to_string()))?,
        });
        self.checkpoint.state.continuation = TurnContinuation::ScheduleTranscript {
            output: TurnOutput::Answered { answer },
            messages,
            assistant_message_id: Some(provisional_message_id),
        };
        Ok(())
    }

    fn schedule_incomplete_transcript(
        &mut self,
        content: Content,
        reason: IncompleteOutputReason,
        provisional_message_id: MessageId,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
        let partial = content.text_only().ok_or_else(|| {
            TurnMachineError::Transition(
                "incomplete model output must contain text parts only".to_string(),
            )
        })?;
        self.apply(
            TurnTransition::IncompleteOutput {
                partial: content.clone(),
                reason,
            },
            events,
        )?;
        let mut messages = self.checkpoint.state.messages.clone();
        messages.push(TurnMessage::Assistant { content });
        self.checkpoint.state.continuation = TurnContinuation::ScheduleTranscript {
            output: TurnOutput::Incomplete { partial, reason },
            messages,
            assistant_message_id: Some(provisional_message_id),
        };
        Ok(())
    }

    fn stop(
        &mut self,
        reason: StopReason,
        detail: Option<crate::StopDetail>,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
        self.apply(
            TurnTransition::Stop {
                reason,
                visible: true,
            },
            events,
        )?;
        self.checkpoint.state.continuation = TurnContinuation::ScheduleTranscript {
            output: TurnOutput::Stopped { reason, detail },
            messages: self.checkpoint.state.messages.clone(),
            assistant_message_id: None,
        };
        Ok(())
    }

    fn fail_effect(
        &mut self,
        operation: TurnFailureOperation,
        failure: TurnRequestFailure,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
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
        failure: TurnRequestFailure,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
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
    ) -> Result<(), TurnMachineError> {
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
        self.checkpoint.state.continuation = TurnContinuation::Terminal;
        self.checkpoint.pending = None;
        self.checkpoint.terminal = Some(terminal);
    }

    fn set_pending(
        &mut self,
        request: impl FnOnce(TurnRequestKey) -> TurnRequest,
        continuation: RequestContinuation,
    ) -> Result<(), TurnMachineError> {
        if self.checkpoint.pending.is_some() {
            return Err(TurnMachineError::Transition(
                "attempted to expose two pending effects".to_string(),
            ));
        }
        self.checkpoint.request_sequence = self
            .checkpoint
            .request_sequence
            .checked_add(1)
            .ok_or_else(|| TurnMachineError::Transition("request sequence overflow".to_string()))?;
        let key = TurnRequestKey {
            turn_id: self.checkpoint.state.input.turn_id.clone(),
            sequence: self.checkpoint.request_sequence,
        };
        self.checkpoint.pending = Some(PendingRequest {
            request: request(key),
            continuation,
        });
        Ok(())
    }

    fn apply(
        &mut self,
        transition: TurnTransition,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
        let record = self
            .checkpoint
            .state
            .apply_transition(transition)
            .map_err(|error| TurnMachineError::Transition(error.to_string()))?;
        let scope = EventScope::builder(record.run_id.clone())
            .turn_id(record.turn_id.clone())
            .build()
            .map_err(|error| TurnMachineError::Transition(error.to_string()))?;
        events.push(EventDraft::new(scope, event_for_record(&record)));
        Ok(())
    }

    fn apply_repair_attempt(
        &mut self,
        strategy: RepairStrategy,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
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
    ) -> Result<(), TurnMachineError> {
        let transition = match stop {
            ToolCallStop::ToolLimitReached { limit } => {
                TurnTransition::RejectToolLimit { limit: *limit }
            }
            ToolCallStop::HiddenTool { name } => {
                TurnTransition::RejectHiddenTool { name: name.clone() }
            }
            ToolCallStop::InvalidSchema { name, message } => TurnTransition::RejectToolArgs {
                name: name.clone(),
                message: message.clone(),
            },
        };
        self.apply(transition, events)
    }

    fn emit_tool_denial(
        &self,
        stop: &ToolCallStop,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
        let Some(policy_hash) = &self.checkpoint.state.input.tool_policy_hash else {
            return Ok(());
        };
        let (tool_id, code) = match stop {
            ToolCallStop::HiddenTool { name } => (
                crate::ToolId::new(name.clone())
                    .ok()
                    .map(|id| id.as_str().to_string()),
                DispatchDenialCode::ToolNotEffective,
            ),
            ToolCallStop::InvalidSchema { name, .. } => (
                crate::ToolId::new(name.clone())
                    .ok()
                    .map(|id| id.as_str().to_string()),
                DispatchDenialCode::InvalidArguments,
            ),
            ToolCallStop::ToolLimitReached { .. } => return Ok(()),
        };
        let scope = EventScope::builder(self.checkpoint.state.input.run_id.clone())
            .turn_id(self.checkpoint.state.input.turn_id.clone())
            .build()
            .map_err(|error| TurnMachineError::Transition(error.to_string()))?;
        events.push(EventDraft::new(
            scope,
            RuntimeEvent::ToolCallDenied {
                policy_hash: policy_hash.clone(),
                tool_id,
                reason_code: code.as_str().to_string(),
            },
        ));
        Ok(())
    }

    fn emit_invalid_argument_denial(
        &self,
        invalid: &InvalidToolArguments,
        events: &mut Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<(), TurnMachineError> {
        let Some(policy_hash) = &self.checkpoint.state.input.tool_policy_hash else {
            return Ok(());
        };
        let tool_id = crate::ToolId::new(invalid.name.clone())
            .ok()
            .map(|id| id.as_str().to_string());
        let scope = EventScope::builder(self.checkpoint.state.input.run_id.clone())
            .turn_id(self.checkpoint.state.input.turn_id.clone())
            .build()
            .map_err(|error| TurnMachineError::Transition(error.to_string()))?;
        events.push(EventDraft::new(
            scope,
            RuntimeEvent::ToolCallDenied {
                policy_hash: policy_hash.clone(),
                tool_id,
                reason_code: DispatchDenialCode::InvalidArguments.as_str().to_string(),
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

fn validate_hook_requirements(input: &TurnInput) -> Result<(), TurnMachineError> {
    let mut requirements = BTreeSet::new();
    for batch in &input.hook_batches {
        for hook_id in batch.required_hooks.iter().chain(&batch.optional_hooks) {
            if !requirements.insert((batch.event, hook_id.clone())) {
                return Err(TurnMachineError::Transition(format!(
                    "duplicate hook requirement `{hook_id}` for `{}`",
                    batch.event.as_str()
                )));
            }
        }
    }
    Ok(())
}

fn validate_hook_result(
    batch: &TurnHookBatch,
    result: &HookBatchResult,
) -> Result<(), TurnMachineError> {
    if result.event != batch.event {
        return Err(TurnMachineError::Transition(format!(
            "hook batch `{}` returned mismatched event `{}`",
            batch.event.as_str(),
            result.event.as_str()
        )));
    }
    let expected = batch.hook_ids().into_iter().collect::<BTreeSet<_>>();
    let actual = result
        .results
        .iter()
        .map(|result| result.hook_id.clone())
        .collect::<BTreeSet<_>>();
    if result.results.len() != expected.len() || actual != expected {
        return Err(TurnMachineError::Transition(
            "hook batch result IDs or cardinality do not match the request".to_string(),
        ));
    }
    Ok(())
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

fn tool_before_payload(state: &TurnState, dispatch: &ToolDispatchRequest) -> serde_json::Value {
    json!({
        "turn_id": state.input.turn_id,
        "tool_id": dispatch.tool_id,
        "arguments": dispatch.arguments,
    })
}

fn tool_after_payload(state: &TurnState, outcome: &ToolOutcome) -> serde_json::Value {
    json!({
        "turn_id": state.input.turn_id,
        "tool_id": outcome.tool_id,
        "outcome": outcome,
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
