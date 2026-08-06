use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_chat::{
    ChildRunPresentation, IncompleteOutputReason as ChatIncompleteOutputReason,
    ModelAttemptOutcome, PolicyPresentationOutcome, PresentationDelivery, ToolActionOutcome,
    ToolPresentationCompleteness, ToolPresentationDetail, ToolPresentationExecutionProfile,
    TurnPresentationEvent, TurnPresentationOutcome, TurnPresentationSink,
};

use crate::{
    ActionItemState, ActivityCacheDisposition, ActivityCompleteness, ActivityDetailView,
    ActivityGraphDeltaBatch, ActivityNodeKind, ActivityNodeState, ActivityNodeView, ActivityPhase,
    ActivityPolicyOutcome, ApplicationError, ApplicationErrorCode, ApplicationService,
    AssistantItemState, ContinueActionView, IncompleteAssistantItemView, IncompleteOutputReason,
    InferenceActivityDetail, InferenceProductStageView, SanitizedDisplayPath,
    SessionPresentationEvent, SessionPresentationItem, ToolActivityDetail,
};

const MAX_ACTION_SUMMARY_BYTES: usize = 1024;

#[derive(Clone, Default)]
pub struct TurnPresentationProxy {
    target: Arc<OnceLock<ApplicationService>>,
}

impl TurnPresentationProxy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn connect(&self, service: ApplicationService) -> Result<(), ApplicationError> {
        self.target.set(service).map_err(|_| {
            ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "turn presentation proxy is already connected",
            )
        })
    }

    pub fn is_connected(&self) -> bool {
        self.target.get().is_some()
    }
}

impl TurnPresentationSink for TurnPresentationProxy {
    fn try_publish(&self, event: TurnPresentationEvent) -> PresentationDelivery {
        self.target
            .get()
            .map_or(PresentationDelivery::Closed, |service| {
                service.try_publish(event)
            })
    }
}

impl TurnPresentationSink for ApplicationService {
    fn try_publish(&self, event: TurnPresentationEvent) -> PresentationDelivery {
        let (session_id, events) = application_events(event);
        match self.publish_batch(&session_id, events) {
            Ok(()) => PresentationDelivery::Delivered,
            Err(error) if error.code == ApplicationErrorCode::ResyncRequired => {
                PresentationDelivery::Lagged
            }
            Err(_) => PresentationDelivery::Closed,
        }
    }
}

fn application_events(
    event: TurnPresentationEvent,
) -> (agl_ids::SessionId, Vec<SessionPresentationEvent>) {
    match event {
        TurnPresentationEvent::ModelAttemptStarted {
            session_id,
            run_id,
            turn_id,
            attempt_id,
            provisional_message_id,
            child_run,
        } => {
            let is_child = child_run.is_some();
            let run_node = child_run.as_ref().map_or_else(
                || run_activity_node(&run_id, ActivityNodeState::Running, None),
                |child| child_run_activity_node(&run_id, child, ActivityNodeState::Running, None),
            );
            let turn_node = turn_activity_node(&run_id, &turn_id, ActivityNodeState::Running, None);
            let attempt_node = attempt_activity_node(
                &run_id,
                &turn_id,
                &attempt_id,
                ActivityPhase::Model,
                ActivityNodeState::Running,
                "model attempt",
                None,
                ActivityDetailView::None,
            );
            let current_path = vec![
                run_node.node_id.clone(),
                turn_node.node_id.clone(),
                attempt_node.node_id.clone(),
            ];
            let mut events = Vec::new();
            if !is_child {
                events.extend([
                    SessionPresentationEvent::PromptActivated {
                        run_id: run_id.clone(),
                    },
                    SessionPresentationEvent::ItemRemoved {
                        item_key: provisional_message_id.to_string(),
                    },
                ]);
            }
            events.push(activity_delta(
                vec![run_node, turn_node, attempt_node],
                &current_path,
            ));
            (session_id, events)
        }
        TurnPresentationEvent::AssistantTextDelta {
            session_id,
            run_id,
            turn_id,
            provisional_message_id,
            sequence,
            text,
            ..
        } => (
            session_id,
            vec![SessionPresentationEvent::AssistantTextDelta {
                run_id,
                turn_id,
                provisional_message_id,
                sequence,
                text,
            }],
        ),
        TurnPresentationEvent::AssistantMessageFinal {
            session_id,
            message_id,
            content,
            ..
        } => (
            session_id,
            vec![SessionPresentationEvent::ItemUpsert {
                item: SessionPresentationItem::AssistantMessage {
                    message_id,
                    content,
                    state: AssistantItemState::Final,
                },
            }],
        ),
        TurnPresentationEvent::AssistantMessageIncomplete {
            session_id,
            run_id,
            turn_id,
            message_id,
            content,
            source_attempt_id,
            reason,
            continuation_index,
        } => (
            session_id,
            vec![SessionPresentationEvent::ItemUpsert {
                item: SessionPresentationItem::IncompleteAssistant {
                    item: IncompleteAssistantItemView {
                        message_id,
                        content,
                        source_run_id: run_id,
                        source_turn_id: turn_id,
                        source_attempt_id,
                        reason: match reason {
                            ChatIncompleteOutputReason::ModelLength => {
                                IncompleteOutputReason::ModelLength
                            }
                            ChatIncompleteOutputReason::ContentByteLimit => {
                                IncompleteOutputReason::ContentByteLimit
                            }
                        },
                        continuation_index,
                        continue_action: ContinueActionView::Available,
                    },
                },
            }],
        ),
        TurnPresentationEvent::ModelAttemptFinished {
            session_id,
            run_id,
            turn_id,
            attempt_id,
            outcome,
            ..
        } => {
            let state = match outcome {
                ModelAttemptOutcome::Completed => ActivityNodeState::Succeeded,
                ModelAttemptOutcome::Incomplete => ActivityNodeState::Incomplete,
                ModelAttemptOutcome::Failed => ActivityNodeState::Failed,
            };
            let node = attempt_activity_node(
                &run_id,
                &turn_id,
                &attempt_id,
                ActivityPhase::Model,
                state,
                "model attempt",
                Some(now_unix_ms()),
                ActivityDetailView::None,
            );
            let path = vec![run_node_id(&run_id), turn_node_id(&turn_id)];
            (session_id, vec![activity_delta(vec![node], &path)])
        }
        TurnPresentationEvent::InferenceStage {
            session_id,
            run_id,
            turn_id,
            event,
        } => {
            let phase = inference_activity_phase(event.stage);
            let state = inference_activity_state(event.stage);
            let node_id = format!("inference:{}", event.attempt_id);
            let attempt_id = event.attempt_id.clone();
            let node = ActivityNodeView {
                node_id: node_id.clone(),
                parent_node_id: Some(attempt_node_id(&attempt_id)),
                order_index: 0,
                run_id: run_id.clone(),
                turn_id: Some(turn_id.clone()),
                attempt_id: Some(attempt_id.clone()),
                step_id: None,
                kind: ActivityNodeKind::Inference,
                phase,
                state,
                retry: 0,
                started_at_unix_ms: if event.stage_sequence == 1 {
                    now_unix_ms()
                } else {
                    0
                },
                updated_at_unix_ms: now_unix_ms(),
                finished_at_unix_ms: event.stage.is_terminal().then(now_unix_ms),
                elapsed_ms: 0,
                summary: inference_stage_summary(event.stage),
                detail: ActivityDetailView::Inference(InferenceActivityDetail {
                    stage: inference_stage_view(event.stage),
                    completed: event.completed,
                    total: event.total,
                    unit: event.unit.map(|unit| match unit {
                        agl_chat::InferenceProgressUnit::Tokens => {
                            crate::InferenceProgressUnit::Tokens
                        }
                        agl_chat::InferenceProgressUnit::Chunks => {
                            crate::InferenceProgressUnit::Chunks
                        }
                    }),
                    cache: inference_cache_disposition(event.stage),
                }),
            };
            let mut path = vec![
                run_node_id(&run_id),
                turn_node_id(&turn_id),
                attempt_node_id(&attempt_id),
            ];
            if !event.stage.is_terminal() {
                path.push(node_id);
            }
            (session_id, vec![activity_delta(vec![node], &path)])
        }
        TurnPresentationEvent::ToolActionStarted {
            session_id,
            run_id,
            turn_id,
            attempt_id,
            step_id,
            tool_id,
            ..
        } => {
            let node = step_activity_node(
                &run_id,
                &turn_id,
                attempt_id.as_ref(),
                &step_id,
                tool_id.as_str(),
                ActivityNodeState::Running,
                None,
                None,
            );
            let mut path = vec![run_node_id(&run_id), turn_node_id(&turn_id)];
            path.push(node.node_id.clone());
            (
                session_id,
                vec![
                    SessionPresentationEvent::ItemUpsert {
                        item: SessionPresentationItem::AgentAction {
                            run_id: run_id.clone(),
                            step_id: step_id.clone(),
                            tool_id: Some(tool_id.to_string()),
                            summary: bounded_summary(tool_id.as_str()),
                            state: ActionItemState::Running,
                        },
                    },
                    activity_delta(vec![node], &path),
                ],
            )
        }
        TurnPresentationEvent::ToolActionFinished {
            session_id,
            run_id,
            turn_id,
            attempt_id,
            step_id,
            tool_id,
            outcome,
            detail,
            ..
        } => {
            let state = match outcome {
                ToolActionOutcome::Succeeded => ActivityNodeState::Succeeded,
                ToolActionOutcome::Waiting => ActivityNodeState::Waiting,
                ToolActionOutcome::Failed => ActivityNodeState::Failed,
            };
            let node = step_activity_node(
                &run_id,
                &turn_id,
                attempt_id.as_ref(),
                &step_id,
                tool_id.as_str(),
                state,
                state.is_terminal().then(now_unix_ms),
                detail.map(tool_activity_detail),
            );
            let mut path = vec![run_node_id(&run_id), turn_node_id(&turn_id)];
            if !state.is_terminal() {
                path.push(node.node_id.clone());
            }
            (
                session_id,
                vec![
                    SessionPresentationEvent::ItemUpsert {
                        item: SessionPresentationItem::AgentAction {
                            run_id: run_id.clone(),
                            step_id,
                            tool_id: Some(tool_id.to_string()),
                            summary: bounded_summary(tool_id.as_str()),
                            state: match outcome {
                                ToolActionOutcome::Succeeded => ActionItemState::Succeeded,
                                ToolActionOutcome::Waiting => ActionItemState::Running,
                                ToolActionOutcome::Failed => ActionItemState::Failed,
                            },
                        },
                    },
                    activity_delta(vec![node], &path),
                ],
            )
        }
        TurnPresentationEvent::PolicyCheck {
            session_id,
            run_id,
            turn_id,
            attempt_id,
            step_id,
            tool_id,
            outcome,
        } => {
            let state = match outcome {
                PolicyPresentationOutcome::Allowed => ActivityNodeState::Succeeded,
                PolicyPresentationOutcome::Denied => ActivityNodeState::Failed,
            };
            let node = policy_activity_node(
                &run_id,
                &turn_id,
                attempt_id.as_ref(),
                &step_id,
                tool_id.as_str(),
                state,
                outcome,
            );
            (
                session_id,
                vec![activity_delta(
                    vec![node],
                    &[run_node_id(&run_id), turn_node_id(&turn_id)],
                )],
            )
        }
        TurnPresentationEvent::TurnFinished {
            session_id,
            run_id,
            turn_id,
            outcome,
            child_run,
            ..
        } => {
            let (activity_state, state) = match outcome {
                TurnPresentationOutcome::Answered => (ActivityNodeState::Succeeded, "answered"),
                TurnPresentationOutcome::IncompleteOutput => {
                    (ActivityNodeState::Incomplete, "incomplete_output")
                }
                TurnPresentationOutcome::Stopped => (ActivityNodeState::Incomplete, "stopped"),
                TurnPresentationOutcome::Failed => (ActivityNodeState::Failed, "failed"),
                TurnPresentationOutcome::Cancelled => (ActivityNodeState::Cancelled, "cancelled"),
            };
            let finished = now_unix_ms();
            let turn_node = turn_activity_node(&run_id, &turn_id, activity_state, Some(finished));
            let run_node = child_run.as_ref().map_or_else(
                || run_activity_node(&run_id, activity_state, Some(finished)),
                |child| child_run_activity_node(&run_id, child, activity_state, Some(finished)),
            );
            let mut events = vec![activity_delta(vec![run_node, turn_node], &[])];
            if child_run.is_none() {
                events.push(SessionPresentationEvent::PromptFinished {
                    run_id,
                    state: state.to_owned(),
                });
            }
            (session_id, events)
        }
    }
}

fn bounded_summary(value: &str) -> String {
    if value.len() <= MAX_ACTION_SUMMARY_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_ACTION_SUMMARY_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn run_node_id(run_id: &agl_ids::RunId) -> String {
    format!("run:{run_id}")
}

fn turn_node_id(turn_id: &agl_ids::TurnId) -> String {
    format!("turn:{turn_id}")
}

fn attempt_node_id(attempt_id: &agl_ids::AttemptId) -> String {
    format!("attempt:{attempt_id}")
}

fn activity_delta(
    upserts: Vec<ActivityNodeView>,
    current_path: &[String],
) -> SessionPresentationEvent {
    SessionPresentationEvent::ActivityGraphDelta {
        batch: ActivityGraphDeltaBatch {
            graph_revision: 0,
            upserts,
            removals: Vec::new(),
            current_path: Some(current_path.to_vec()),
            truncated: false,
        },
    }
}

fn run_activity_node(
    run_id: &agl_ids::RunId,
    state: ActivityNodeState,
    finished_at_unix_ms: Option<i64>,
) -> ActivityNodeView {
    ActivityNodeView {
        node_id: run_node_id(run_id),
        parent_node_id: None,
        order_index: 0,
        run_id: run_id.clone(),
        turn_id: None,
        attempt_id: None,
        step_id: None,
        kind: ActivityNodeKind::Run,
        phase: ActivityPhase::Queued,
        state,
        retry: 0,
        started_at_unix_ms: finished_at_unix_ms.map_or_else(now_unix_ms, |_| 0),
        updated_at_unix_ms: now_unix_ms(),
        finished_at_unix_ms,
        elapsed_ms: 0,
        summary: "run".to_owned(),
        detail: ActivityDetailView::None,
    }
}

fn child_run_activity_node(
    run_id: &agl_ids::RunId,
    child: &ChildRunPresentation,
    state: ActivityNodeState,
    finished_at_unix_ms: Option<i64>,
) -> ActivityNodeView {
    ActivityNodeView {
        node_id: run_node_id(run_id),
        parent_node_id: Some(format!("step:{}", child.spawned_by_step_id)),
        order_index: 0,
        run_id: run_id.clone(),
        turn_id: None,
        attempt_id: None,
        step_id: None,
        kind: ActivityNodeKind::ChildRun,
        phase: ActivityPhase::ChildRun,
        state,
        retry: 0,
        started_at_unix_ms: finished_at_unix_ms.map_or_else(now_unix_ms, |_| 0),
        updated_at_unix_ms: now_unix_ms(),
        finished_at_unix_ms,
        elapsed_ms: 0,
        summary: bounded_summary(&child.subagent_id),
        detail: ActivityDetailView::None,
    }
}

fn turn_activity_node(
    run_id: &agl_ids::RunId,
    turn_id: &agl_ids::TurnId,
    state: ActivityNodeState,
    finished_at_unix_ms: Option<i64>,
) -> ActivityNodeView {
    ActivityNodeView {
        node_id: turn_node_id(turn_id),
        parent_node_id: Some(run_node_id(run_id)),
        order_index: 0,
        run_id: run_id.clone(),
        turn_id: Some(turn_id.clone()),
        attempt_id: None,
        step_id: None,
        kind: ActivityNodeKind::Turn,
        phase: ActivityPhase::Model,
        state,
        retry: 0,
        started_at_unix_ms: finished_at_unix_ms.map_or_else(now_unix_ms, |_| 0),
        updated_at_unix_ms: now_unix_ms(),
        finished_at_unix_ms,
        elapsed_ms: 0,
        summary: "turn".to_owned(),
        detail: ActivityDetailView::None,
    }
}

#[allow(clippy::too_many_arguments)]
fn attempt_activity_node(
    run_id: &agl_ids::RunId,
    turn_id: &agl_ids::TurnId,
    attempt_id: &agl_ids::AttemptId,
    phase: ActivityPhase,
    state: ActivityNodeState,
    summary: &str,
    finished_at_unix_ms: Option<i64>,
    detail: ActivityDetailView,
) -> ActivityNodeView {
    ActivityNodeView {
        node_id: attempt_node_id(attempt_id),
        parent_node_id: Some(turn_node_id(turn_id)),
        order_index: 0,
        run_id: run_id.clone(),
        turn_id: Some(turn_id.clone()),
        attempt_id: Some(attempt_id.clone()),
        step_id: None,
        kind: ActivityNodeKind::Attempt,
        phase,
        state,
        retry: 0,
        started_at_unix_ms: finished_at_unix_ms.map_or_else(now_unix_ms, |_| 0),
        updated_at_unix_ms: now_unix_ms(),
        finished_at_unix_ms,
        elapsed_ms: 0,
        summary: summary.to_owned(),
        detail,
    }
}

#[allow(clippy::too_many_arguments)]
fn step_activity_node(
    run_id: &agl_ids::RunId,
    turn_id: &agl_ids::TurnId,
    attempt_id: Option<&agl_ids::AttemptId>,
    step_id: &agl_ids::StepId,
    tool_id: &str,
    state: ActivityNodeState,
    finished_at_unix_ms: Option<i64>,
    detail: Option<ActivityDetailView>,
) -> ActivityNodeView {
    ActivityNodeView {
        node_id: format!("step:{step_id}"),
        parent_node_id: Some(turn_node_id(turn_id)),
        order_index: 0,
        run_id: run_id.clone(),
        turn_id: Some(turn_id.clone()),
        attempt_id: attempt_id.cloned(),
        step_id: Some(step_id.clone()),
        kind: ActivityNodeKind::Step,
        phase: ActivityPhase::Tool,
        state,
        retry: 0,
        started_at_unix_ms: finished_at_unix_ms.map_or_else(now_unix_ms, |_| 0),
        updated_at_unix_ms: now_unix_ms(),
        finished_at_unix_ms,
        elapsed_ms: 0,
        summary: bounded_summary(tool_id),
        detail: detail.unwrap_or_else(|| ActivityDetailView::UnknownTool {
            tool_id: bounded_summary(tool_id),
        }),
    }
}

fn policy_activity_node(
    run_id: &agl_ids::RunId,
    turn_id: &agl_ids::TurnId,
    attempt_id: Option<&agl_ids::AttemptId>,
    step_id: &agl_ids::StepId,
    tool_id: &str,
    state: ActivityNodeState,
    outcome: PolicyPresentationOutcome,
) -> ActivityNodeView {
    let now = now_unix_ms();
    ActivityNodeView {
        node_id: format!("policy:{step_id}"),
        parent_node_id: Some(format!("step:{step_id}")),
        order_index: 0,
        run_id: run_id.clone(),
        turn_id: Some(turn_id.clone()),
        attempt_id: attempt_id.cloned(),
        step_id: Some(step_id.clone()),
        kind: ActivityNodeKind::Step,
        phase: ActivityPhase::Policy,
        state,
        retry: 0,
        started_at_unix_ms: 0,
        updated_at_unix_ms: now,
        finished_at_unix_ms: Some(now),
        elapsed_ms: 0,
        summary: bounded_summary(tool_id),
        detail: ActivityDetailView::Tool(ToolActivityDetail::PolicyCheck {
            tool_id: bounded_summary(tool_id),
            outcome: match outcome {
                PolicyPresentationOutcome::Allowed => ActivityPolicyOutcome::Allowed,
                PolicyPresentationOutcome::Denied => ActivityPolicyOutcome::Denied,
            },
        }),
    }
}

fn tool_activity_detail(detail: ToolPresentationDetail) -> ActivityDetailView {
    ActivityDetailView::Tool(match detail {
        ToolPresentationDetail::FilesystemList {
            path,
            entries,
            completeness,
        } => ToolActivityDetail::FilesystemList {
            path: SanitizedDisplayPath::from_utf8(&path),
            entries,
            completeness: match completeness {
                ToolPresentationCompleteness::Complete => ActivityCompleteness::Complete,
                ToolPresentationCompleteness::Truncated => ActivityCompleteness::Truncated,
            },
        },
        ToolPresentationDetail::FilesystemRead { path, bytes } => {
            ToolActivityDetail::FilesystemRead {
                path: SanitizedDisplayPath::from_utf8(&path),
                bytes,
            }
        }
        ToolPresentationDetail::RepositorySearch {
            scope,
            matches,
            complete,
        } => ToolActivityDetail::RepositorySearch {
            scope: SanitizedDisplayPath::from_utf8(&scope),
            matches,
            complete,
        },
        ToolPresentationDetail::ProcessExecution {
            profile,
            exit_status,
        } => ToolActivityDetail::ProcessExecution {
            profile: match profile {
                ToolPresentationExecutionProfile::Workspace => {
                    agl_exec::ExecutionProfile::Workspace
                }
                ToolPresentationExecutionProfile::Host => agl_exec::ExecutionProfile::Host,
            },
            exit_status,
        },
    })
}

fn inference_activity_phase(stage: agl_chat::InferenceProductStage) -> ActivityPhase {
    use agl_chat::InferenceProductStage as Stage;
    match stage {
        Stage::Queued => ActivityPhase::InferenceQueue,
        Stage::Admission => ActivityPhase::InferenceAdmission,
        Stage::ModelLoad | Stage::ModelReuse => ActivityPhase::ModelLoad,
        Stage::ContextReuse | Stage::ContextRebuild => ActivityPhase::Context,
        Stage::Prefill => ActivityPhase::Prefill,
        Stage::Generation => ActivityPhase::Generation,
        Stage::OutputParse => ActivityPhase::OutputParsing,
        Stage::Completed
        | Stage::Incomplete
        | Stage::Cancelled
        | Stage::Failed
        | Stage::BackendLost => ActivityPhase::Terminal,
    }
}

fn inference_activity_state(stage: agl_chat::InferenceProductStage) -> ActivityNodeState {
    use agl_chat::InferenceProductStage as Stage;
    match stage {
        Stage::Queued | Stage::Admission => ActivityNodeState::Waiting,
        Stage::ModelLoad
        | Stage::ModelReuse
        | Stage::ContextReuse
        | Stage::ContextRebuild
        | Stage::Prefill
        | Stage::Generation
        | Stage::OutputParse => ActivityNodeState::Running,
        Stage::Completed => ActivityNodeState::Succeeded,
        Stage::Incomplete => ActivityNodeState::Incomplete,
        Stage::Cancelled => ActivityNodeState::Cancelled,
        Stage::Failed | Stage::BackendLost => ActivityNodeState::Failed,
    }
}

fn inference_stage_view(stage: agl_chat::InferenceProductStage) -> InferenceProductStageView {
    use agl_chat::InferenceProductStage as Stage;
    match stage {
        Stage::Queued => InferenceProductStageView::Queued,
        Stage::Admission => InferenceProductStageView::Admission,
        Stage::ModelLoad => InferenceProductStageView::ModelLoad,
        Stage::ModelReuse => InferenceProductStageView::ModelReuse,
        Stage::ContextReuse => InferenceProductStageView::ContextReuse,
        Stage::ContextRebuild => InferenceProductStageView::ContextRebuild,
        Stage::Prefill => InferenceProductStageView::Prefill,
        Stage::Generation => InferenceProductStageView::Generation,
        Stage::OutputParse => InferenceProductStageView::OutputParse,
        Stage::Completed => InferenceProductStageView::Completed,
        Stage::Incomplete => InferenceProductStageView::Incomplete,
        Stage::Cancelled => InferenceProductStageView::Cancelled,
        Stage::Failed => InferenceProductStageView::Failed,
        Stage::BackendLost => InferenceProductStageView::BackendLost,
    }
}

fn inference_cache_disposition(stage: agl_chat::InferenceProductStage) -> ActivityCacheDisposition {
    use agl_chat::InferenceProductStage as Stage;
    match stage {
        Stage::ModelLoad => ActivityCacheDisposition::Cold,
        Stage::ModelReuse | Stage::ContextReuse => ActivityCacheDisposition::Reused,
        Stage::ContextRebuild => ActivityCacheDisposition::Rebuilt,
        _ => ActivityCacheDisposition::NotApplicable,
    }
}

fn inference_stage_summary(stage: agl_chat::InferenceProductStage) -> String {
    use agl_chat::InferenceProductStage as Stage;
    match stage {
        Stage::Queued => "inference queued",
        Stage::Admission => "accelerator admission",
        Stage::ModelLoad => "loading model",
        Stage::ModelReuse => "reusing model",
        Stage::ContextReuse => "reusing context",
        Stage::ContextRebuild => "rebuilding context",
        Stage::Prefill => "prefill",
        Stage::Generation => "generating",
        Stage::OutputParse => "parsing output",
        Stage::Completed => "inference complete",
        Stage::Incomplete => "output incomplete",
        Stage::Cancelled => "inference cancelled",
        Stage::Failed => "inference failed",
        Stage::BackendLost => "inference backend lost",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use agl_content::Content;
    use agl_ids::{AttemptId, MessageId, RunId, SessionId, StepId, TurnId};

    use super::*;

    #[test]
    fn incomplete_turn_event_preserves_partial_output_as_a_distinct_actionable_item() {
        let session_id = SessionId::generate();
        let run_id = RunId::generate();
        let turn_id = TurnId::generate();
        let attempt_id = AttemptId::generate();
        let message_id = MessageId::generate();

        let (actual_session_id, events) =
            application_events(TurnPresentationEvent::AssistantMessageIncomplete {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                message_id: message_id.clone(),
                content: Content::text("bounded partial output").unwrap(),
                source_attempt_id: attempt_id.clone(),
                reason: ChatIncompleteOutputReason::ContentByteLimit,
                continuation_index: 3,
            });

        assert_eq!(actual_session_id, session_id);
        assert!(matches!(
            events.as_slice(),
            [SessionPresentationEvent::ItemUpsert {
                item: SessionPresentationItem::IncompleteAssistant {
                    item: IncompleteAssistantItemView {
                        message_id: actual_message_id,
                        content,
                        source_run_id,
                        source_turn_id,
                        source_attempt_id,
                        reason: IncompleteOutputReason::ContentByteLimit,
                        continuation_index: 3,
                        continue_action: ContinueActionView::Available,
                    },
                },
            }] if actual_message_id == &message_id
                && content.text_only().as_deref() == Some("bounded partial output")
                && source_run_id == &run_id
                && source_turn_id == &turn_id
                && source_attempt_id == &attempt_id
        ));
    }

    #[test]
    fn child_start_is_an_activity_branch_without_a_human_prompt_transition() {
        let session_id = SessionId::generate();
        let parent_run_id = RunId::generate();
        let run_id = RunId::generate();
        let turn_id = TurnId::generate();
        let attempt_id = AttemptId::generate();
        let step_id = StepId::generate();
        let (_, events) = application_events(TurnPresentationEvent::ModelAttemptStarted {
            session_id,
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
            attempt_id: attempt_id.clone(),
            provisional_message_id: MessageId::generate(),
            child_run: Some(ChildRunPresentation {
                parent_run_id,
                spawned_by_step_id: step_id.clone(),
                subagent_id: "reviewer".to_owned(),
            }),
        });

        assert!(!events.iter().any(|event| matches!(
            event,
            SessionPresentationEvent::PromptActivated { .. }
                | SessionPresentationEvent::ItemRemoved { .. }
        )));
        assert!(matches!(
            events.as_slice(),
            [SessionPresentationEvent::ActivityGraphDelta { batch }]
                if matches!(batch.upserts.as_slice(), [child, turn, attempt]
                    if child.node_id == format!("run:{run_id}")
                        && child.parent_node_id == Some(format!("step:{step_id}"))
                        && child.kind == ActivityNodeKind::ChildRun
                        && turn.parent_node_id == Some(child.node_id.clone())
                        && attempt.parent_node_id == Some(turn.node_id.clone())
                        && attempt.attempt_id.as_ref() == Some(&attempt_id))
        ));
    }

    #[test]
    fn tool_and_policy_events_map_only_closed_typed_details() {
        let session_id = SessionId::generate();
        let run_id = RunId::generate();
        let turn_id = TurnId::generate();
        let attempt_id = AttemptId::generate();
        let step_id = StepId::generate();
        let tool_id = agl_kernel::ToolId::new("core.workspace:fs.list").unwrap();
        let (_, tool_events) = application_events(TurnPresentationEvent::ToolActionFinished {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
            attempt_id: Some(attempt_id.clone()),
            provisional_message_id: None,
            step_id: step_id.clone(),
            tool_id: tool_id.clone(),
            outcome: ToolActionOutcome::Succeeded,
            detail: Some(ToolPresentationDetail::FilesystemList {
                path: "crates".to_owned(),
                entries: 42,
                completeness: ToolPresentationCompleteness::Truncated,
            }),
        });
        let tool_node = tool_events
            .iter()
            .find_map(|event| match event {
                SessionPresentationEvent::ActivityGraphDelta { batch } => batch.upserts.first(),
                _ => None,
            })
            .unwrap();
        assert_eq!(tool_node.parent_node_id, Some(format!("turn:{turn_id}")));
        assert_eq!(
            tool_node.detail,
            ActivityDetailView::Tool(ToolActivityDetail::FilesystemList {
                path: SanitizedDisplayPath::from_utf8("crates"),
                entries: 42,
                completeness: ActivityCompleteness::Truncated,
            })
        );

        let (_, policy_events) = application_events(TurnPresentationEvent::PolicyCheck {
            session_id,
            run_id,
            turn_id,
            attempt_id: Some(attempt_id),
            step_id: step_id.clone(),
            tool_id,
            outcome: PolicyPresentationOutcome::Denied,
        });
        let policy_node = policy_events
            .iter()
            .find_map(|event| match event {
                SessionPresentationEvent::ActivityGraphDelta { batch } => batch.upserts.first(),
                _ => None,
            })
            .unwrap();
        assert_eq!(policy_node.node_id, format!("policy:{step_id}"));
        assert_eq!(policy_node.parent_node_id, Some(format!("step:{step_id}")));
        assert_eq!(policy_node.phase, ActivityPhase::Policy);
        assert_eq!(
            policy_node.detail,
            ActivityDetailView::Tool(ToolActivityDetail::PolicyCheck {
                tool_id: "core.workspace:fs.list".to_owned(),
                outcome: ActivityPolicyOutcome::Denied,
            })
        );
    }
}
