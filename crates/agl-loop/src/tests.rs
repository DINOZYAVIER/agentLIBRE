use std::collections::VecDeque;

use agl_capabilities::{
    ActionDeclaration, ActionResult, CapabilityId, HookBatchResult, HookId, HookMessage,
    HookResult, HookStatus, OperationKind,
};
use agl_content::Content;
use agl_ids::{RunId, TurnId};
use agl_turn::{ModelResponse, StopReason, TurnHookBatch, TurnInput, TurnOutput, VisibleTool};
use serde_json::json;

use crate::*;

const RUN_ID: &str = "run_01890f17-4a00-7000-8000-000000000001";
const TURN_ID: &str = "turn_01890f17-4a00-7000-8000-000000000002";

#[derive(Default)]
struct Script {
    model: VecDeque<EffectOutcome<ModelResponse>>,
    capability: VecDeque<EffectOutcome<agl_turn::ToolDispatchResponse>>,
    hooks: VecDeque<EffectOutcome<HookEffectOutput>>,
    transcript: VecDeque<EffectOutcome<()>>,
}

impl Script {
    fn model(mut self, content: impl Into<String>) -> Self {
        self.model
            .push_back(EffectOutcome::Succeeded(ModelResponse {
                content: Content::text(content).unwrap(),
            }));
        self
    }

    fn model_failure(mut self, code: EffectFailureCode, message: &str) -> Self {
        self.model
            .push_back(EffectOutcome::Failed(EffectFailure::new(
                code, message, false,
            )));
        self
    }

    fn observation(mut self, value: serde_json::Value) -> Self {
        self.capability
            .push_back(EffectOutcome::Succeeded(agl_turn::ToolDispatchResponse {
                result: ActionResult::new(value),
            }));
        self
    }

    fn hook(mut self, event: agl_capabilities::HookEvent, status: HookStatus) -> Self {
        self.hooks
            .push_back(EffectOutcome::Succeeded(HookEffectOutput {
                result: HookBatchResult {
                    event,
                    results: vec![HookResult {
                        hook_id: hook_id("guard.test"),
                        status,
                        messages: if status == HookStatus::Repair {
                            vec![HookMessage {
                                code: "answer.repair".to_string(),
                                message: "private diagnostic".to_string(),
                                fix: Some("remove the invalid claim".to_string()),
                            }]
                        } else {
                            Vec::new()
                        },
                    }],
                },
                duration_ms: Some(7),
            }));
        self
    }

    fn result_for(&mut self, effect: &TurnEffect) -> TurnEffectResult {
        match effect {
            TurnEffect::HookBatch { key, request } => TurnEffectResult::HookBatch {
                key: key.clone(),
                outcome: self.hooks.pop_front().unwrap_or_else(|| {
                    EffectOutcome::Succeeded(HookEffectOutput {
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
            TurnEffect::ModelGeneration { key, .. } => TurnEffectResult::ModelGeneration {
                key: key.clone(),
                outcome: self
                    .model
                    .pop_front()
                    .expect("missing scripted model result"),
            },
            TurnEffect::CapabilityDispatch { key, .. } => TurnEffectResult::CapabilityDispatch {
                key: key.clone(),
                outcome: self
                    .capability
                    .pop_front()
                    .expect("missing scripted capability result"),
            },
            TurnEffect::TranscriptAppend { key, .. } => TurnEffectResult::TranscriptAppend {
                key: key.clone(),
                outcome: self
                    .transcript
                    .pop_front()
                    .unwrap_or(EffectOutcome::Succeeded(())),
            },
        }
    }
}

#[derive(Debug, PartialEq)]
struct RunResult {
    terminal: TurnTerminal,
    events: Vec<agl_events::RuntimeEvent>,
    effects: Vec<TurnEffectKind>,
}

fn run_script(input: TurnInput, mut script: Script, checkpoint_each: bool) -> RunResult {
    let mut executor = TurnExecutor::new(input);
    let mut advance = executor.next_effect().unwrap();
    let mut events = Vec::new();
    let mut effects = Vec::new();
    loop {
        events.extend(advance.events.into_iter().map(|draft| draft.payload));
        match advance.state {
            TurnAdvanceState::Terminal { terminal } => {
                return RunResult {
                    terminal,
                    events,
                    effects,
                };
            }
            TurnAdvanceState::Pending { effect } => {
                effects.push(effect.kind());
                if checkpoint_each {
                    let bytes = serde_json::to_vec(&executor.checkpoint()).unwrap();
                    let checkpoint: TurnCheckpoint = serde_json::from_slice(&bytes).unwrap();
                    executor = TurnExecutor::from_checkpoint(checkpoint).unwrap();
                }
                let result = script.result_for(&effect);
                advance = executor.resume(result).unwrap();
                if checkpoint_each {
                    let bytes = serde_json::to_vec(&executor.checkpoint()).unwrap();
                    let checkpoint: TurnCheckpoint = serde_json::from_slice(&bytes).unwrap();
                    executor = TurnExecutor::from_checkpoint(checkpoint).unwrap();
                }
            }
        }
    }
}

fn run_id() -> RunId {
    RunId::parse(RUN_ID).unwrap()
}

fn turn_id() -> TurnId {
    TurnId::parse(TURN_ID).unwrap()
}

fn input() -> TurnInput {
    TurnInput::user(run_id(), turn_id(), Content::text("hello").unwrap())
}

fn read_tool() -> VisibleTool {
    let declaration = ActionDeclaration::new(
        CapabilityId::new("read_file").unwrap(),
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
    .unwrap();
    VisibleTool::from_declaration(&declaration)
}

fn tool_call(name: &str, arguments: serde_json::Value) -> String {
    format!("<tool_call>{{\"name\":{name:?},\"arguments\":{arguments}}}</tool_call>")
}

fn hook_id(value: &str) -> HookId {
    HookId::new(value).unwrap()
}

fn event_kinds(events: &[agl_events::RuntimeEvent]) -> Vec<&'static str> {
    events.iter().map(agl_events::RuntimeEvent::kind).collect()
}

#[test]
fn answer_path_is_effect_driven_and_checkpoint_equivalent() {
    let uninterrupted = run_script(input(), Script::default().model("done"), false);
    let resumed = run_script(input(), Script::default().model("done"), true);

    assert_eq!(uninterrupted, resumed);
    assert_eq!(
        uninterrupted.terminal,
        TurnTerminal::Completed {
            output: TurnOutput::Answered {
                answer: "done".to_string()
            }
        }
    );
    assert_eq!(
        uninterrupted.effects,
        [
            TurnEffectKind::ModelGeneration,
            TurnEffectKind::TranscriptAppend,
        ]
    );
    assert_eq!(
        event_kinds(&uninterrupted.events),
        [
            "turn.started",
            "model.request_prepared",
            "model.requested",
            "model.response_received",
            "model.action_parsed",
            "answer.final",
            "turn.finished",
        ]
    );
}

#[test]
fn tool_observation_loops_back_to_model_without_driver_policy() {
    let result = run_script(
        input()
            .with_visible_tool(read_tool())
            .with_max_tool_calls(2),
        Script::default()
            .model(tool_call("read_file", json!({"path": "README.md"})))
            .observation(json!({"text": "contents"}))
            .model("final"),
        true,
    );

    assert_eq!(
        result.effects,
        [
            TurnEffectKind::ModelGeneration,
            TurnEffectKind::CapabilityDispatch,
            TurnEffectKind::ModelGeneration,
            TurnEffectKind::TranscriptAppend,
        ]
    );
    assert!(event_kinds(&result.events).contains(&"observation.appended"));
    assert_eq!(
        result.terminal,
        TurnTerminal::Completed {
            output: TurnOutput::Answered {
                answer: "final".to_string()
            }
        }
    );
}

#[test]
fn hidden_invalid_and_limited_tools_stop_before_dispatch() {
    let hidden = run_script(
        input()
            .with_visible_tool(read_tool())
            .with_max_tool_calls(1)
            .with_capability_policy_hash("policy-test"),
        Script::default().model(tool_call("write_file", json!({"path": "README.md"}))),
        true,
    );
    assert!(!hidden.effects.contains(&TurnEffectKind::CapabilityDispatch));
    assert!(event_kinds(&hidden.events).contains(&"capability.call_denied"));
    assert!(matches!(
        hidden.terminal,
        TurnTerminal::Completed {
            output: TurnOutput::Stopped {
                reason: StopReason::HiddenTool,
                ..
            }
        }
    ));

    let invalid = run_script(
        input()
            .with_visible_tool(read_tool())
            .with_max_tool_calls(1),
        Script::default().model(tool_call("read_file", json!({"other": true}))),
        false,
    );
    assert!(matches!(
        invalid.terminal,
        TurnTerminal::Completed {
            output: TurnOutput::Stopped {
                reason: StopReason::InvalidToolArguments,
                ..
            }
        }
    ));

    let limited = run_script(
        input().with_visible_tool(read_tool()),
        Script::default().model(tool_call("read_file", json!({"path": "README.md"}))),
        false,
    );
    assert!(matches!(
        limited.terminal,
        TurnTerminal::Completed {
            output: TurnOutput::Stopped {
                reason: StopReason::ToolLimitReached,
                ..
            }
        }
    ));
}

#[test]
fn malformed_tool_json_repair_and_stop_paths_are_pure() {
    let repaired = run_script(
        input()
            .with_visible_tool(read_tool())
            .with_max_tool_calls(1),
        Script::default()
            .model(r#"<tool_call>{"name":"read_file","arguments":{"path":"README.md"}}"#)
            .observation(json!({"ok": true}))
            .model("done"),
        true,
    );
    assert!(event_kinds(&repaired.events).contains(&"tool.json_repair_succeeded"));

    let stopped = run_script(input(), Script::default().model("<tool_call>{bad"), true);
    assert!(matches!(
        stopped.terminal,
        TurnTerminal::Completed {
            output: TurnOutput::Stopped {
                reason: StopReason::ToolJsonUnrepairable,
                ..
            }
        }
    ));
}

#[test]
fn hook_repair_reissues_model_and_hook_failure_is_typed() {
    let artifact_hook = TurnHookBatch::new(agl_capabilities::HookEvent::ArtifactWrite)
        .with_required_hook(hook_id("guard.test"));
    let repaired = run_script(
        input()
            .with_hook_batch(artifact_hook.clone())
            .with_max_hook_repair_attempts(1),
        Script::default()
            .model("bad")
            .model("good")
            .hook(
                agl_capabilities::HookEvent::ArtifactWrite,
                HookStatus::Repair,
            )
            .hook(agl_capabilities::HookEvent::ArtifactWrite, HookStatus::Pass),
        true,
    );
    assert_eq!(
        repaired
            .effects
            .iter()
            .filter(|kind| **kind == TurnEffectKind::ModelGeneration)
            .count(),
        2
    );
    assert!(event_kinds(&repaired.events).contains(&"hook.repair_prepared"));

    let failed = run_script(
        input().with_hook_batch(artifact_hook),
        Script::default()
            .model("bad")
            .hook(agl_capabilities::HookEvent::ArtifactWrite, HookStatus::Fail),
        false,
    );
    assert!(matches!(
        failed.terminal,
        TurnTerminal::Failed {
            failure: TurnExecutionFailure {
                code: EffectFailureCode::Hook,
                ..
            }
        }
    ));
}

#[test]
fn model_capability_and_transcript_failures_are_typed() {
    let model = run_script(
        input(),
        Script::default().model_failure(
            EffectFailureCode::Inference,
            "raw inference backend diagnostic",
        ),
        false,
    );
    assert!(matches!(
        model.terminal,
        TurnTerminal::Failed {
            failure: TurnExecutionFailure {
                code: EffectFailureCode::Inference,
                ..
            }
        }
    ));
    assert!(
        !serde_json::to_string(&model.events)
            .unwrap()
            .contains("raw inference backend diagnostic")
    );

    let mut capability_script =
        Script::default().model(tool_call("read_file", json!({"path": "README.md"})));
    capability_script
        .capability
        .push_back(EffectOutcome::Failed(EffectFailure::new(
            EffectFailureCode::Capability,
            "private capability failure",
            false,
        )));
    let capability = run_script(
        input()
            .with_visible_tool(read_tool())
            .with_max_tool_calls(1),
        capability_script,
        false,
    );
    assert!(matches!(
        capability.terminal,
        TurnTerminal::Failed {
            failure: TurnExecutionFailure {
                code: EffectFailureCode::Capability,
                ..
            }
        }
    ));

    let mut transcript_script = Script::default().model("done");
    transcript_script
        .transcript
        .push_back(EffectOutcome::Failed(EffectFailure::new(
            EffectFailureCode::Transcript,
            "private transcript failure",
            false,
        )));
    let transcript = run_script(input(), transcript_script, false);
    assert!(matches!(
        transcript.terminal,
        TurnTerminal::Failed {
            failure: TurnExecutionFailure {
                code: EffectFailureCode::Transcript,
                ..
            }
        }
    ));
}

#[test]
fn pending_effect_is_stable_and_results_are_exactly_once() {
    let mut executor = TurnExecutor::new(input());
    let first = executor.next_effect().unwrap();
    let TurnAdvanceState::Pending { effect } = first.state else {
        panic!("expected model effect");
    };
    let repeated = executor.next_effect().unwrap();
    assert!(repeated.events.is_empty());
    let TurnAdvanceState::Pending {
        effect: repeated_effect,
    } = repeated.state
    else {
        panic!("expected repeated model effect");
    };
    assert_eq!(
        serde_json::to_vec(&effect).unwrap(),
        serde_json::to_vec(&repeated_effect).unwrap()
    );

    let stale = EffectKey {
        turn_id: TurnId::generate(),
        sequence: effect.key().sequence,
    };
    assert!(matches!(
        executor.resume(TurnEffectResult::ModelGeneration {
            key: stale,
            outcome: EffectOutcome::Succeeded(ModelResponse {
                content: Content::text("done").unwrap()
            }),
        }),
        Err(TurnExecutorError::StaleEffectKey { .. })
    ));
    assert!(matches!(
        executor.resume(TurnEffectResult::TranscriptAppend {
            key: effect.key().clone(),
            outcome: EffectOutcome::Succeeded(()),
        }),
        Err(TurnExecutorError::MismatchedEffectResult { .. })
    ));

    let result = TurnEffectResult::ModelGeneration {
        key: effect.key().clone(),
        outcome: EffectOutcome::Succeeded(ModelResponse {
            content: Content::text("done").unwrap(),
        }),
    };
    executor.resume(result.clone()).unwrap();
    assert!(matches!(
        executor.resume(result),
        Err(TurnExecutorError::DuplicateEffectKey(_))
    ));
}

#[test]
fn cancellation_is_terminal_before_during_and_between_effects() {
    let mut before = TurnExecutor::new(input());
    before.request_cancellation().unwrap();
    let advance = before.next_effect().unwrap();
    assert!(matches!(
        advance.state,
        TurnAdvanceState::Terminal {
            terminal: TurnTerminal::Cancelled
        }
    ));
    assert_eq!(
        event_kinds(
            &advance
                .events
                .into_iter()
                .map(|draft| draft.payload)
                .collect::<Vec<_>>()
        ),
        ["turn.cancelled", "turn.finished"]
    );
    assert_eq!(
        before.request_cancellation().unwrap_err(),
        TurnExecutorError::AlreadyTerminal
    );

    let mut active = TurnExecutor::new(input());
    let advance = active.next_effect().unwrap();
    let TurnAdvanceState::Pending { effect } = advance.state else {
        panic!("expected pending model");
    };
    let cancelled = active
        .resume(TurnEffectResult::ModelGeneration {
            key: effect.key().clone(),
            outcome: EffectOutcome::Cancelled,
        })
        .unwrap();
    assert!(matches!(
        cancelled.state,
        TurnAdvanceState::Terminal {
            terminal: TurnTerminal::Cancelled
        }
    ));

    let mut between = TurnExecutor::new(input());
    let advance = between.next_effect().unwrap();
    let TurnAdvanceState::Pending { effect } = advance.state else {
        panic!("expected pending model");
    };
    between.request_cancellation().unwrap();
    let cancelled = between
        .resume(TurnEffectResult::ModelGeneration {
            key: effect.key().clone(),
            outcome: EffectOutcome::Succeeded(ModelResponse {
                content: Content::text("completed concurrently").unwrap(),
            }),
        })
        .unwrap();
    assert!(matches!(
        cancelled.state,
        TurnAdvanceState::Terminal {
            terminal: TurnTerminal::Cancelled
        }
    ));
}

#[test]
fn checkpoint_decode_is_strict_and_validates_invariants() {
    let mut executor = TurnExecutor::new(input());
    executor.next_effect().unwrap();
    let checkpoint = executor.checkpoint();
    let encoded = serde_json::to_vec(&checkpoint).unwrap();
    let decoded: TurnCheckpoint = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded.pending_effect(), checkpoint.pending_effect());
    assert_eq!(decoded.schema(), TURN_CHECKPOINT_SCHEMA);

    let mut wrong_schema = serde_json::to_value(&checkpoint).unwrap();
    wrong_schema["schema"] = json!("agentlibre.turn-checkpoint.v0");
    assert!(serde_json::from_value::<TurnCheckpoint>(wrong_schema).is_err());

    let mut unknown = serde_json::to_value(&checkpoint).unwrap();
    unknown["legacy_state"] = json!(true);
    assert!(serde_json::from_value::<TurnCheckpoint>(unknown).is_err());

    let mut bad_sequence = serde_json::to_value(&checkpoint).unwrap();
    bad_sequence["effect_sequence"] = json!(0);
    assert!(serde_json::from_value::<TurnCheckpoint>(bad_sequence).is_err());
}
