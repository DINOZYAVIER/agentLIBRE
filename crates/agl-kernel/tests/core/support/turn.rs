#![allow(dead_code)]

use std::collections::VecDeque;

use agl_content::Content;
use agl_events::RuntimeEvent;
use agl_ids::{RunId, TurnId};
use agl_kernel::{
    DeclarationDigest, HookBatchResult, HookEvent, HookId, HookMessage, HookResult, HookStatus,
    OperationKind, ToolDeclaration, ToolResult,
};
use agl_kernel::{
    HookRequestOutput, TurnAdvance, TurnAdvanceState, TurnMachine, TurnMachineError, TurnRequest,
    TurnRequestFailure, TurnRequestFailureCode, TurnRequestKey, TurnRequestKind,
    TurnRequestOutcome, TurnRequestResult, TurnTerminal,
};
use agl_kernel::{
    ModelResponse, ModelResponseOutcome, TurnHookBatch, TurnInput, TurnMessage, VisibleTool,
};
use serde_json::{Value, json};

use crate::support::{extension_id, hook_id, tool_id};

pub const RUN_ID: &str = "run_01890f17-4a00-7000-8000-000000000001";
pub const TURN_ID: &str = "turn_01890f17-4a00-7000-8000-000000000002";

pub fn run_id() -> RunId {
    RunId::parse(RUN_ID).expect("test RunId is valid")
}

pub fn turn_id() -> TurnId {
    TurnId::parse(TURN_ID).expect("test TurnId is valid")
}

pub fn text(value: impl Into<String>) -> Content {
    Content::text(value).expect("test text content is valid")
}

pub fn turn_input() -> TurnInput {
    TurnInput::user(run_id(), turn_id(), text("hello"))
}

pub fn visible_read_tool() -> VisibleTool {
    let declaration = ToolDeclaration::new(
        tool_id("core.workspace:fs.read"),
        "Read a file",
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        }),
        OperationKind::Read,
    )
    .expect("test Tool declaration is valid");
    VisibleTool::from_declaration(&declaration)
}

pub fn tool_call(name: &str, arguments: Value) -> String {
    format!("<tool_call>{{\"name\":{name:?},\"arguments\":{arguments}}}</tool_call>")
}

#[derive(Default)]
pub struct Script {
    model: VecDeque<TurnRequestOutcome<ModelResponse>>,
    tool: VecDeque<TurnRequestOutcome<agl_kernel::ToolDispatchResponse>>,
    hooks: VecDeque<TurnRequestOutcome<HookRequestOutput>>,
    transcript: VecDeque<TurnRequestOutcome<()>>,
}

impl Script {
    pub fn model(mut self, content: impl Into<String>) -> Self {
        self.model
            .push_back(TurnRequestOutcome::Succeeded(ModelResponse {
                content: text(content),
                outcome: ModelResponseOutcome::Complete,
            }));
        self
    }

    pub fn model_failure(mut self, code: TurnRequestFailureCode, message: &str) -> Self {
        self.model
            .push_back(TurnRequestOutcome::Failed(TurnRequestFailure::new(
                code, message, false,
            )));
        self
    }

    pub fn model_incomplete(mut self, content: impl Into<String>) -> Self {
        self.model
            .push_back(TurnRequestOutcome::Succeeded(ModelResponse {
                content: text(content),
                outcome: ModelResponseOutcome::Incomplete {
                    reason: agl_kernel::IncompleteOutputReason::ModelLength,
                },
            }));
        self
    }

    pub fn tool_result(mut self, value: Value) -> Self {
        self.tool.push_back(TurnRequestOutcome::Succeeded(
            agl_kernel::ToolDispatchResponse {
                result: agl_kernel::ToolOutcome::succeeded(
                    "core-test-call".to_string(),
                    tool_id("core.workspace:fs.read"),
                    extension_id("core.workspace"),
                    DeclarationDigest::from_json(&json!({"core_test": true})),
                    ToolResult::new(value),
                ),
            },
        ));
        self
    }

    pub fn tool_cancelled(mut self) -> Self {
        self.tool.push_back(TurnRequestOutcome::Cancelled);
        self
    }

    pub fn tool_failure(mut self, message: &str) -> Self {
        self.tool
            .push_back(TurnRequestOutcome::Failed(TurnRequestFailure::new(
                TurnRequestFailureCode::Tool,
                message,
                false,
            )));
        self
    }

    pub fn hook_result(
        mut self,
        event: HookEvent,
        results: impl IntoIterator<Item = HookResult>,
    ) -> Self {
        self.hooks
            .push_back(TurnRequestOutcome::Succeeded(HookRequestOutput {
                result: HookBatchResult {
                    event,
                    results: results.into_iter().collect(),
                },
                duration_ms: Some(1),
            }));
        self
    }

    pub fn hook_status(mut self, event: HookEvent, id: &str, status: HookStatus) -> Self {
        let messages = if status == HookStatus::Repair {
            vec![HookMessage {
                code: "core.repair".to_string(),
                message: "repair diagnostic".to_string(),
                fix: Some("repair guidance".to_string()),
            }]
        } else {
            Vec::new()
        };
        self.hooks
            .push_back(TurnRequestOutcome::Succeeded(HookRequestOutput {
                result: HookBatchResult {
                    event,
                    results: vec![HookResult {
                        hook_id: hook_id(id),
                        status,
                        messages,
                    }],
                },
                duration_ms: Some(1),
            }));
        self
    }

    pub fn hook_failure(mut self, message: &str) -> Self {
        self.hooks
            .push_back(TurnRequestOutcome::Failed(TurnRequestFailure::new(
                TurnRequestFailureCode::Hook,
                message,
                false,
            )));
        self
    }

    pub fn transcript_failure(mut self, message: &str) -> Self {
        self.transcript
            .push_back(TurnRequestOutcome::Failed(TurnRequestFailure::new(
                TurnRequestFailureCode::Transcript,
                message,
                false,
            )));
        self
    }

    fn result_for(&mut self, request: &TurnRequest) -> TurnRequestResult {
        match request {
            TurnRequest::HookBatch { key, request } => TurnRequestResult::HookBatch {
                key: key.clone(),
                outcome: self.hooks.pop_front().unwrap_or_else(|| {
                    TurnRequestOutcome::Succeeded(HookRequestOutput {
                        result: HookBatchResult {
                            event: request.event,
                            results: request
                                .hooks
                                .iter()
                                .cloned()
                                .map(|hook_id| HookResult {
                                    hook_id,
                                    status: HookStatus::Pass,
                                    messages: Vec::new(),
                                })
                                .collect(),
                        },
                        duration_ms: Some(1),
                    })
                }),
            },
            TurnRequest::ModelGeneration { key, .. } => TurnRequestResult::ModelGeneration {
                key: key.clone(),
                outcome: self
                    .model
                    .pop_front()
                    .expect("missing scripted model result"),
            },
            TurnRequest::ToolDispatch { key, .. } => TurnRequestResult::ToolDispatch {
                key: key.clone(),
                outcome: Box::new(self.tool.pop_front().expect("missing scripted Tool result")),
            },
            TurnRequest::TranscriptAppend { key, .. } => TurnRequestResult::TranscriptAppend {
                key: key.clone(),
                outcome: self
                    .transcript
                    .pop_front()
                    .unwrap_or(TurnRequestOutcome::Succeeded(())),
            },
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct RunResult {
    pub terminal: TurnTerminal,
    pub events: Vec<RuntimeEvent>,
    pub request_kinds: Vec<RequestKind>,
    pub request_keys: Vec<TurnRequestKey>,
    pub messages: Vec<TurnMessage>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestKind {
    HookBatch,
    ModelGeneration,
    ToolDispatch,
    TranscriptAppend,
}

impl From<TurnRequestKind> for RequestKind {
    fn from(value: TurnRequestKind) -> Self {
        match value {
            TurnRequestKind::HookBatch => Self::HookBatch,
            TurnRequestKind::ModelGeneration => Self::ModelGeneration,
            TurnRequestKind::ToolDispatch => Self::ToolDispatch,
            TurnRequestKind::TranscriptAppend => Self::TranscriptAppend,
        }
    }
}

pub fn run_script(input: TurnInput, mut script: Script, checkpoint_each: bool) -> RunResult {
    let mut machine = TurnMachine::new(input);
    let mut advance = machine.next_request().expect("Turn advance succeeds");
    let mut events = Vec::new();
    let mut request_kinds = Vec::new();
    let mut request_keys = Vec::new();
    loop {
        events.extend(advance.events.into_iter().map(|draft| draft.payload));
        match advance.state {
            TurnAdvanceState::Terminal { terminal } => {
                return RunResult {
                    terminal,
                    events,
                    request_kinds,
                    request_keys,
                    messages: machine.checkpoint().state().messages.clone(),
                };
            }
            TurnAdvanceState::Pending { request } => {
                request_kinds.push(request.kind().into());
                request_keys.push(request.key().clone());
                if checkpoint_each {
                    machine = restore(machine);
                }
                let result = script.result_for(&request);
                advance = machine.resume(result).expect("Turn resume succeeds");
                if checkpoint_each {
                    machine = restore(machine);
                }
            }
        }
    }
}

fn restore(machine: TurnMachine) -> TurnMachine {
    let bytes = serde_json::to_vec(&machine.checkpoint()).expect("checkpoint serializes");
    let checkpoint = serde_json::from_slice(&bytes).expect("checkpoint deserializes");
    TurnMachine::from_checkpoint(checkpoint).expect("checkpoint restores")
}

pub fn first_advance(input: TurnInput) -> Result<TurnAdvance, TurnMachineError> {
    TurnMachine::new(input).next_request()
}

pub fn first_hook_ids(input: TurnInput) -> Result<Vec<HookId>, String> {
    let advance = first_advance(input).map_err(|error| error.to_string())?;
    match advance.state {
        TurnAdvanceState::Pending {
            request: TurnRequest::HookBatch { request, .. },
        } => Ok(request.hooks),
        other => Err(format!("expected first HookBatch request, got {other:?}")),
    }
}

pub fn validate_turn_start(input: TurnInput) -> Result<(), String> {
    first_advance(input)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub fn resume_first_hook(
    input: TurnInput,
    result: HookBatchResult,
) -> Result<TurnAdvance, TurnMachineError> {
    let mut machine = TurnMachine::new(input);
    let advance = machine.next_request()?;
    let TurnAdvanceState::Pending {
        request: TurnRequest::HookBatch { key, .. },
    } = advance.state
    else {
        panic!("expected first HookBatch request")
    };
    machine.resume(TurnRequestResult::HookBatch {
        key,
        outcome: TurnRequestOutcome::Succeeded(HookRequestOutput {
            result,
            duration_ms: Some(1),
        }),
    })
}

pub fn context_hook_batch(
    required: impl IntoIterator<Item = &'static str>,
    optional: impl IntoIterator<Item = &'static str>,
) -> TurnHookBatch {
    let batch = required.into_iter().fold(
        TurnHookBatch::new(HookEvent::ContextPrepare),
        |batch, id| batch.with_required_hook(hook_id(id)),
    );
    optional
        .into_iter()
        .fold(batch, |batch, id| batch.with_optional_hook(hook_id(id)))
}
