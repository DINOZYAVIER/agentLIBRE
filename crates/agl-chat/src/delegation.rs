use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agl_content::Content;
use agl_function::{RuntimeDelegationPlan, RuntimeSubagentSpec};
use agl_ids::{RunId, SessionId, TurnId};
use agl_kernel::{
    ToolDispatchContext, ToolHandler, ToolHandlerError, ToolId, ToolInvocation, ToolResult,
};
use agl_store::{
    AglStore, ChildRunAdmission, ChildRunDraft, DelegationTreeBudget, DurableRunRecord, RunBudget,
    StoreError,
};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::delegation_contract::DelegateActionArgs;
use crate::supervisor_driver::ChatRunInput;

#[derive(Clone)]
pub(crate) struct DelegationHandler {
    context: Option<DelegationContext>,
}

#[derive(Clone)]
struct DelegationContext {
    store_root: PathBuf,
    runtime_paths: agl_runtime::AgentLibrePaths,
    workspace_root: PathBuf,
    artifact_root: PathBuf,
    trust_store_path: PathBuf,
    parent_function_plan_input: agl_model::ResolvedFunctionPlanInput,
    parent_model_plan_input: agl_model::ResolvedModelPlanInput,
    plan: RuntimeDelegationPlan,
    children: BTreeSet<String>,
    authority_ceiling: BTreeSet<ToolId>,
    execution_context_state: Arc<Mutex<agl_exec::ExecutionContextSnapshot>>,
    runtime_bundle: Option<agl_runtime::ResolvedRuntimeBundle>,
}

impl DelegationHandler {
    pub(crate) fn disabled() -> Self {
        Self { context: None }
    }

    pub(crate) fn from_session(
        session: &crate::InferenceSession,
        execution_context_state: Arc<Mutex<agl_exec::ExecutionContextSnapshot>>,
    ) -> Option<Self> {
        let plan = session.delegation_plan()?.clone();
        let children = session
            .delegation_children()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if children.is_empty() {
            return None;
        }
        let authority_ceiling = session.delegation_authority_ceiling().clone();
        let (parent_function_plan_input, parent_model_plan_input) =
            session.model_plan_inputs().ok()?;
        Some(Self {
            context: Some(DelegationContext {
                store_root: session.store_root().to_path_buf(),
                runtime_paths: session.runtime_paths().clone(),
                workspace_root: session.workspace_root().to_path_buf(),
                artifact_root: session.artifact_root().to_path_buf(),
                trust_store_path: session.trust_store_path().to_path_buf(),
                parent_function_plan_input,
                parent_model_plan_input,
                plan,
                children,
                authority_ceiling,
                execution_context_state,
                runtime_bundle: session.runtime_bundle().cloned(),
            }),
        })
    }

    fn dispatch_inner(&self, invocation: ToolInvocation) -> Result<ToolResult> {
        let context = self
            .context
            .as_ref()
            .context("delegation is not enabled for this runtime node")?;
        let args: DelegateActionArgs = serde_json::from_value(invocation.arguments.clone())
            .context("invalid agent.supervisor:delegate arguments")?;
        args.validate().map_err(anyhow::Error::msg)?;
        ensure!(
            context.children.contains(&args.subagent_id),
            "subagent `{}` is not declared for this runtime node",
            args.subagent_id
        );
        let step_id = invocation
            .scope
            .step_id()
            .cloned()
            .context("agent.supervisor:delegate requires a durable run step")?;
        let parent_run_id = invocation.scope.run_id().clone();
        let store = AglStore::open_current_at(&context.store_root)
            .context("failed to open delegation store")?;
        let spec = context
            .plan
            .subagent_specs
            .get(&args.subagent_id)
            .with_context(|| {
                format!(
                    "persisted delegation plan has no subagent `{}`",
                    args.subagent_id
                )
            })?;

        if let Some(existing) = store.child_run_by_spawn_step(&step_id)? {
            validate_existing_child(&existing, &parent_run_id, &args, context, spec)?;
            return delegation_result(&existing);
        }

        let parent = store
            .run(&parent_run_id)?
            .with_context(|| format!("parent run {parent_run_id} disappeared"))?;
        let effective = crate::session::resolve_subagent_effective_capabilities(
            spec,
            &context.authority_ceiling,
            &context.runtime_paths,
            &context.workspace_root,
            &context.trust_store_path,
            context.runtime_bundle.as_ref(),
        )?;
        let (function_plan_input, model_plan_input) =
            resolve_child_model_inputs(context, spec, &effective)?;
        let execution_session_id = SessionId::generate();
        let execution_turn_id = TurnId::generate();
        let task = Content::text(args.task.clone())?;
        let input = ChatRunInput::Subagent {
            task,
            execution_session_id,
            execution_turn_id,
            workspace_root: context.workspace_root.clone(),
            artifact_root: context.artifact_root.clone(),
            function_plan_input: function_plan_input.clone(),
            model_plan_input: Box::new(model_plan_input.clone()),
            delegation_plan: context.plan.clone(),
            authority_ceiling: context.authority_ceiling.clone(),
        };
        let tree_budget = DelegationTreeBudget {
            max_depth: context.plan.budget.max_depth,
            max_children_per_run: context.plan.budget.max_children_per_run,
            max_descendants: context.plan.budget.max_descendants,
            max_total_output_tokens: context.plan.budget.max_total_output_tokens,
            timeout_ms: context
                .plan
                .budget
                .timeout_seconds
                .checked_mul(1_000)
                .context("delegation timeout overflows milliseconds")?,
        };
        let draft = ChildRunDraft {
            run_id: RunId::generate(),
            parent_run_id: parent_run_id.clone(),
            spawned_by_step_id: step_id.clone(),
            subagent_id: args.subagent_id.clone(),
            input: serde_json::to_value(input)?,
            priority: parent.priority,
            effective_policy_hash: effective.policy_hash().as_str().to_string(),
            budget: RunBudget {
                wall_time_ms: spec
                    .limits
                    .timeout_seconds
                    .checked_mul(1_000)
                    .context("subagent timeout overflows milliseconds")?,
                model_input_tokens: parent.budget.model_input_tokens,
                model_output_tokens: spec.limits.max_output_tokens,
                model_attempts: spec.limits.max_model_attempts,
                tool_calls: spec.limits.max_tool_calls,
            },
            child_spec_digest: spec.spec_digest.clone(),
            model_profile_digest: model_plan_inputs_digest(
                &function_plan_input,
                &model_plan_input,
            )?,
            tree_budget,
            execution_context: context
                .execution_context_state
                .lock()
                .map_err(|error| {
                    anyhow::anyhow!("delegation execution context lock is poisoned: {error}")
                })?
                .clone(),
        };
        let admission = match store.admit_child_run(&draft) {
            Ok(admission) => admission,
            Err(StoreError::DelegationDenied {
                code: "spawn_replay_mismatch",
            }) => {
                let existing = store
                    .child_run_by_spawn_step(&step_id)?
                    .context("spawn replay conflict has no durable child")?;
                validate_existing_child(&existing, &parent_run_id, &args, context, spec)?;
                ChildRunAdmission {
                    run: existing,
                    replayed: true,
                }
            }
            Err(error) => return Err(error).context("child run admission was denied"),
        };
        delegation_result(&admission.run)
    }
}

impl ToolHandler for DelegationHandler {
    fn dispatch(&self, context: ToolDispatchContext) -> agl_kernel::ToolHandlerFuture<'_> {
        Box::pin(async move {
            let invocation = context.into_invocation();
            self.dispatch_inner(invocation)
                .map_err(|error| ToolHandlerError::execution_failed(format!("{error:#}")))
        })
    }
}

fn resolve_child_model_inputs(
    context: &DelegationContext,
    spec: &RuntimeSubagentSpec,
    effective: &agl_kernel::EffectiveToolSet,
) -> Result<(
    agl_model::ResolvedFunctionPlanInput,
    agl_model::ResolvedModelPlanInput,
)> {
    let mut function = context.parent_function_plan_input.clone();
    function.selected_profile_id = if spec.model.inherit {
        context
            .parent_function_plan_input
            .selected_profile_id
            .clone()
    } else {
        spec.model
            .profile
            .clone()
            .context("subagent Model selection has neither inherit nor profile")?
    };
    let parent_policy = &context.parent_function_plan_input.generation_policy;
    function.generation_policy = agl_model::GenerationPolicy::greedy(
        u32::try_from(spec.limits.max_output_tokens)
            .context("subagent max_output_tokens exceeds the inference limit")?,
        parent_policy.stop_rules().to_vec(),
        parent_policy.structured_mode(),
        parent_policy.repair_malformed_tool_calls(),
    )?;
    function.prompt_template_digest = sha256_bytes(spec.system_body.as_bytes());
    let visible_tools = crate::session::visible_tools_from_effective(effective);
    let visible_tools_value = serde_json::to_value(visible_tools)?;
    function.visible_tools_digest =
        sha256_bytes(agl_kernel::render_canonical_json(&visible_tools_value).as_bytes());
    Ok((function, context.parent_model_plan_input.clone()))
}

fn validate_existing_child(
    child: &DurableRunRecord,
    parent_run_id: &RunId,
    args: &DelegateActionArgs,
    context: &DelegationContext,
    spec: &RuntimeSubagentSpec,
) -> Result<()> {
    ensure!(
        child.kind == agl_store::RunKind::Subagent
            && child.session_id.is_none()
            && child.turn_id.is_none()
            && child.parent_run_id.as_ref() == Some(parent_run_id)
            && child.subagent_id.as_deref() == Some(args.subagent_id.as_str()),
        "durable child does not match the delegation invocation"
    );
    let input: ChatRunInput = serde_json::from_value(child.input.clone())?;
    let ChatRunInput::Subagent {
        task,
        workspace_root,
        artifact_root,
        function_plan_input,
        model_plan_input,
        delegation_plan,
        authority_ceiling,
        ..
    } = input
    else {
        bail!("durable child has a non-subagent input");
    };
    ensure!(
        !task.has_attachments() && task.text_only().as_deref() == Some(args.task.as_str()),
        "durable child task does not match the delegation invocation"
    );
    ensure!(
        workspace_root == context.workspace_root
            && artifact_root == context.artifact_root
            && delegation_plan == context.plan
            && authority_ceiling == context.authority_ceiling
            && child.child_spec_digest.as_deref() == Some(spec.spec_digest.as_str())
            && child.model_profile_digest.as_deref()
                == Some(
                    model_plan_inputs_digest(&function_plan_input, &model_plan_input,)?.as_str()
                ),
        "durable child snapshot differs from the delegation invocation"
    );
    Ok(())
}

fn delegation_result(child: &DurableRunRecord) -> Result<ToolResult> {
    if !child.state.is_terminal() {
        return Ok(ToolResult::new(serde_json::json!({
            "status": "waiting",
            "child_run_id": child.run_id,
            "subagent_id": child.subagent_id,
        })));
    }
    let final_text = child
        .terminal_result
        .as_ref()
        .and_then(|result| {
            (result.get("status").and_then(serde_json::Value::as_str) == Some("answered"))
                .then(|| result.get("answer").and_then(serde_json::Value::as_str))
                .flatten()
        })
        .map(str::to_string);
    let mut result = ToolResult::new(serde_json::json!({
        "status": child.state.as_str(),
        "child_run_id": child.run_id,
        "subagent_id": child.subagent_id,
        "final_text": final_text,
        "usage": child.usage,
        "error_code": child.error_code,
    }));
    if let Some(final_text) = final_text {
        result = result.with_content(Content::text(final_text)?);
    }
    Ok(result)
}

pub(crate) fn result_is_waiting(result: &agl_kernel::TurnRequestResult) -> bool {
    let agl_kernel::TurnRequestResult::ToolDispatch { outcome, .. } = result else {
        return false;
    };
    matches!(
        outcome.as_ref(),
        agl_kernel::TurnRequestOutcome::Succeeded(response)
            if response.result.data.as_ref()
                .and_then(|data| data.get("status"))
                .and_then(serde_json::Value::as_str)
                == Some("waiting")
    )
}

pub(crate) fn model_plan_inputs_digest(
    function: &agl_model::ResolvedFunctionPlanInput,
    model: &agl_model::ResolvedModelPlanInput,
) -> Result<String> {
    Ok(sha256_bytes(&serde_json::to_vec(&(function, model))?))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut value = String::with_capacity("sha256:".len() + digest.len() * 2);
    value.push_str("sha256:");
    for byte in digest {
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value
}

#[cfg(test)]
mod tests {
    use agl_ids::StepId;
    use agl_store::{RunKind, RunState, RunUsage};

    use super::*;

    #[test]
    fn child_failure_is_a_safe_structured_tool_result() {
        let child = failed_child_record();

        let result = delegation_result(&child).unwrap();
        let encoded = serde_json::to_string(&result).unwrap();

        assert_eq!(result.data["status"], "failed");
        assert_eq!(result.data["error_code"], "chat_turn_failed");
        assert!(result.content.is_none());
        assert!(!encoded.contains("private backend detail"));
        assert!(!encoded.contains("transcript"));
    }

    fn failed_child_record() -> DurableRunRecord {
        let run_id = RunId::generate();
        let parent_run_id = RunId::generate();
        DurableRunRecord {
            run_id,
            session_id: None,
            turn_id: None,
            kind: RunKind::Subagent,
            state: RunState::Failed,
            revision: agl_kernel::RunRevision::new(1),
            priority: 0,
            concurrency_key: None,
            input: serde_json::json!({}),
            checkpoint: None,
            effective_policy_hash: Some(format!("sha256:{}", "a".repeat(64))),
            execution_context: test_execution_context(),
            budget: RunBudget::default(),
            usage: RunUsage::default(),
            lease_owner: None,
            lease_generation: 1,
            lease_expires_at_ms: None,
            cancellation_requested_at_ms: None,
            attempts: 1,
            not_before_ms: None,
            created_at_ms: 1,
            updated_at_ms: 2,
            started_at_ms: Some(1),
            finished_at_ms: Some(2),
            terminal_result: None,
            error_code: Some("chat_turn_failed".to_string()),
            error_message: Some("private backend detail".to_string()),
            parent_run_id: Some(parent_run_id.clone()),
            root_run_id: parent_run_id,
            depth: 1,
            subagent_id: Some("reviewer".to_string()),
            spawned_by_step_id: Some(StepId::generate()),
            child_spec_digest: Some(format!("sha256:{}", "b".repeat(64))),
            model_profile_digest: Some(format!("sha256:{}", "c".repeat(64))),
            result_delivered_at_ms: None,
            tree_usage_recorded_at_ms: Some(2),
            delegation_budget: None,
            delegation_reserved_descendants: 0,
            delegation_reserved_output_tokens: 0,
            delegation_used_output_tokens: 0,
        }
    }

    fn test_execution_context() -> agl_exec::ExecutionContextSnapshot {
        let workspace = std::env::temp_dir().canonicalize().unwrap();
        agl_exec::ExecutionContextSnapshot {
            workspace_root: workspace.clone(),
            working_directory: workspace,
            private_execution_roots: Vec::new(),
            shell: agl_exec::ShellProfileSnapshot {
                program: std::path::PathBuf::from("/bin/sh"),
                command_args: vec!["-c".to_owned()],
                login_command_args: Some(vec!["-l".to_owned(), "-c".to_owned()]),
                environment_names: vec!["PATH".to_owned()],
                executable_digest: "sha256:test-shell".to_owned(),
                config_digest: "sha256:test-config".to_owned(),
            },
            revision: 1,
            profile_metadata: "workspace".to_owned(),
        }
    }
}
