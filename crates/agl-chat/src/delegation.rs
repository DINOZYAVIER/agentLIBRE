use std::collections::BTreeSet;
use std::path::PathBuf;

use agl_capabilities::{
    ActionHandler, ActionHandlerError, ActionInvocation, ActionResult, CapabilityId,
    DelegateActionArgs,
};
use agl_config::{LocalInferenceConfig, load_local_inference_config};
use agl_content::Content;
use agl_functions::{RuntimeDelegationPlan, RuntimeSubagentSpec};
use agl_ids::{RunId, SessionId, TurnId};
use agl_store::{
    AglStore, ChildRunAdmission, ChildRunDraft, DelegationTreeBudget, DurableRunRecord, RunBudget,
    StoreError,
};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::supervisor_driver::ChatRunInput;

#[derive(Clone)]
pub(crate) struct DelegationHandler {
    context: Option<DelegationContext>,
}

#[derive(Clone)]
struct DelegationContext {
    store_root: PathBuf,
    workspace_root: PathBuf,
    artifact_root: PathBuf,
    trust_store_path: PathBuf,
    parent_inference_config: LocalInferenceConfig,
    plan: RuntimeDelegationPlan,
    children: BTreeSet<String>,
    authority_ceiling: BTreeSet<CapabilityId>,
}

impl DelegationHandler {
    pub(crate) fn disabled() -> Self {
        Self { context: None }
    }

    pub(crate) fn from_session(session: &crate::InferenceSession) -> Option<Self> {
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
        Some(Self {
            context: Some(DelegationContext {
                store_root: session.store_root().to_path_buf(),
                workspace_root: session.workspace_root().to_path_buf(),
                artifact_root: session.artifact_root().to_path_buf(),
                trust_store_path: session.trust_store_path().to_path_buf(),
                parent_inference_config: session.inference_config().clone(),
                plan,
                children,
                authority_ceiling,
            }),
        })
    }

    fn dispatch_inner(&self, invocation: ActionInvocation) -> Result<ActionResult> {
        let context = self
            .context
            .as_ref()
            .context("delegation is not enabled for this runtime node")?;
        let args: DelegateActionArgs = serde_json::from_value(invocation.arguments.clone())
            .context("invalid agent.delegate arguments")?;
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
            .context("agent.delegate requires a durable run step")?;
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
        let inference_config = resolve_child_inference_config(context, spec)?;
        let effective = crate::session::resolve_subagent_effective_capabilities(
            spec,
            &context.authority_ceiling,
            &context.workspace_root,
            &context.trust_store_path,
        )?;
        let execution_session_id = SessionId::generate();
        let execution_turn_id = TurnId::generate();
        let task = Content::text(args.task.clone())?;
        let input = ChatRunInput::Subagent {
            task,
            execution_session_id,
            execution_turn_id,
            workspace_root: context.workspace_root.clone(),
            artifact_root: context.artifact_root.clone(),
            inference_config: inference_config.clone(),
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
                capability_calls: spec.limits.max_capability_calls,
            },
            child_spec_digest: spec.spec_digest.clone(),
            model_profile_digest: inference_config_digest(&inference_config)?,
            tree_budget,
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

impl ActionHandler for DelegationHandler {
    fn dispatch(
        &self,
        invocation: ActionInvocation,
    ) -> std::result::Result<ActionResult, ActionHandlerError> {
        self.dispatch_inner(invocation)
            .map_err(|error| std::io::Error::other(format!("{error:#}")).into())
    }
}

fn resolve_child_inference_config(
    context: &DelegationContext,
    spec: &RuntimeSubagentSpec,
) -> Result<LocalInferenceConfig> {
    if spec.model.inherit {
        return Ok(context.parent_inference_config.clone());
    }
    let path = spec
        .model
        .profile_path
        .as_ref()
        .context("explicit subagent model profile has no resolved path")?;
    let expected_digest = spec
        .model
        .profile_digest
        .as_deref()
        .context("explicit subagent model profile has no source digest")?;
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read subagent profile {}", path.display()))?;
    let actual_digest = sha256_bytes(&bytes);
    ensure!(
        actual_digest == expected_digest,
        "subagent model profile changed after function resolution"
    );
    load_local_inference_config(path)
        .with_context(|| format!("failed to load subagent profile {}", path.display()))
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
        inference_config,
        delegation_plan,
        authority_ceiling,
        ..
    } = input
    else {
        bail!("durable child has a non-subagent input");
    };
    ensure!(
        !task.has_artifacts() && task.text_only().as_deref() == Some(args.task.as_str()),
        "durable child task does not match the delegation invocation"
    );
    ensure!(
        workspace_root == context.workspace_root
            && artifact_root == context.artifact_root
            && delegation_plan == context.plan
            && authority_ceiling == context.authority_ceiling
            && child.child_spec_digest.as_deref() == Some(spec.spec_digest.as_str())
            && child.model_profile_digest.as_deref()
                == Some(inference_config_digest(&inference_config)?.as_str()),
        "durable child snapshot differs from the delegation invocation"
    );
    Ok(())
}

fn delegation_result(child: &DurableRunRecord) -> Result<ActionResult> {
    if !child.state.is_terminal() {
        return Ok(ActionResult::new(serde_json::json!({
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
    let mut result = ActionResult::new(serde_json::json!({
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

pub(crate) fn result_is_waiting(result: &agl_loop::TurnEffectResult) -> bool {
    matches!(
        result,
        agl_loop::TurnEffectResult::CapabilityDispatch {
            outcome: agl_loop::EffectOutcome::Succeeded(response),
            ..
        } if response.result.data.get("status").and_then(serde_json::Value::as_str)
            == Some("waiting")
    )
}

pub(crate) fn inference_config_digest(config: &LocalInferenceConfig) -> Result<String> {
    Ok(sha256_bytes(&serde_json::to_vec(config)?))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

#[cfg(test)]
mod tests {
    use agl_ids::StepId;
    use agl_store::{RunKind, RunState, RunUsage};

    use super::*;

    #[test]
    fn child_failure_is_a_safe_structured_capability_result() {
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
            priority: 0,
            input: serde_json::json!({}),
            checkpoint: None,
            effective_policy_hash: Some(format!("sha256:{}", "a".repeat(64))),
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
}
