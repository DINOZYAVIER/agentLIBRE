use std::path::Path;

use agl_core::Content;
use agl_core::agent::{InstructionBlock, InstructionSet, InstructionSource};

use super::*;

fn effective_generation_max_output(configured: u64, context_tokens: u32) -> u64 {
    configured.min(u64::from(context_tokens) / 2)
}

const MEMORY_POINTER_INSTRUCTION: &str = "Durable workspace memory is opt-in. If the task touches memory, use forge_read with forge:agentlibre.memory/INDEX.md and then the relevant forge:agentlibre.memory/entries/<slug>.md before re-investigating. Do not treat memory as authoritative without checking its cited sources.";

fn memory_opted_in(root: &Path) -> bool {
    super::memory_io::MemoryWorkspace::new(root)
        .is_ok_and(|workspace| workspace.load_corpus().is_ok_and(|corpus| corpus.is_some()))
}

fn append_memory_pointer(
    root: &Path,
    instructions: &InstructionSet,
) -> Result<InstructionSet, AgentServiceError> {
    if !memory_opted_in(root) {
        return Ok(instructions.clone());
    }
    let mut blocks = instructions.blocks.clone();
    blocks.push(InstructionBlock {
        source: InstructionSource::Agent,
        content: Content::text(MEMORY_POINTER_INSTRUCTION.to_owned())
            .map_err(|_| AgentServiceError::InvalidSnapshot)?,
    });
    InstructionSet::new(blocks).map_err(|_| AgentServiceError::InvalidSnapshot)
}

pub(crate) fn admit(
    dependencies: &AgentDependencies,
    bindings: &BTreeMap<String, PreparedTool>,
    spec: AgentRunSpec,
    allow_creation: bool,
) -> Result<AgentRun, AgentServiceError> {
    if let Some(run) = dependencies.store.replay_agent_run(&spec)? {
        return Ok(run);
    }
    if !allow_creation {
        return Err(AgentServiceError::Unavailable);
    }
    let mut snapshot = dependencies
        .store
        .resolve_agent_run_snapshot(&spec.origin)?;
    snapshot.instructions =
        append_memory_pointer(snapshot.workspace.root.as_path(), &snapshot.instructions)?;
    validate_snapshot(&snapshot, bindings)?;
    let admitted = dependencies.store.admit_agent_run(&spec, snapshot)?;
    Ok(match admitted {
        AgentRunAdmission::Created(run) | AgentRunAdmission::Replayed(run) => run,
    })
}

pub(crate) fn validate_snapshot(
    snapshot: &agl_core::agent::AgentRunSnapshot,
    bindings: &BTreeMap<String, PreparedTool>,
) -> Result<(), AgentServiceError> {
    if snapshot.limits.deadline_ms <= 0
        || snapshot
            .limits
            .model_input_tokens
            .is_some_and(|limit| limit == 0 || limit > i64::MAX as u64)
        || snapshot.limits.model_output_tokens == 0
        || snapshot.limits.model_calls == 0
        || snapshot.limits.tool_result_bytes == 0
        || snapshot.limits.tool_result_bytes > agl_core::agent::MAX_TOOL_RESULT_BYTES
    {
        return Err(AgentServiceError::InvalidSnapshot);
    }
    snapshot
        .authority
        .validate()
        .map_err(|_| AgentServiceError::InvalidSnapshot)?;
    snapshot
        .instructions
        .validate()
        .map_err(|_| AgentServiceError::InvalidSnapshot)?;
    let root = snapshot
        .workspace
        .root
        .as_path()
        .canonicalize()
        .map_err(|_| AgentServiceError::InvalidSnapshot)?;
    if root != snapshot.workspace.root.as_path() || !root.is_dir() {
        return Err(AgentServiceError::InvalidSnapshot);
    }
    let working = root
        .join(snapshot.workspace.working_directory.as_path())
        .canonicalize()
        .map_err(|_| AgentServiceError::InvalidSnapshot)?;
    if !working.starts_with(&root) || !working.is_dir() {
        return Err(AgentServiceError::InvalidSnapshot);
    }
    let mut previous = None;
    for admitted in &snapshot.tools {
        admitted
            .definition
            .validate()
            .map_err(|_| AgentServiceError::InvalidSnapshot)?;
        if previous
            .as_ref()
            .is_some_and(|id: &ToolId| id >= &admitted.definition.id)
        {
            return Err(AgentServiceError::InvalidSnapshot);
        }
        previous = Some(admitted.definition.id.clone());
        let prepared = bindings
            .get(admitted.definition.id.as_str())
            .ok_or(AgentServiceError::InvalidBindings)?;
        if prepared.definition != admitted.definition
            || prepared.binding.definition_digest != admitted.definition_digest
            || prepared.extension_id != admitted.extension.id
            || prepared.extension_id.as_str() != admitted.extension.package.id.as_str()
            || prepared.extension_version != admitted.extension.package.version
            || prepared.extension_digest != admitted.extension.definition_digest
        {
            return Err(AgentServiceError::InvalidBindings);
        }
        for effect in &admitted.definition.required_effects {
            let validator = prepared
                .effect_validators
                .get(effect)
                .ok_or(AgentServiceError::InvalidBindings)?;
            let matching = snapshot
                .authority
                .0
                .iter()
                .filter(|grant| &grant.effect == effect)
                .collect::<Vec<_>>();
            if matching.is_empty()
                || matching
                    .iter()
                    .any(|grant| !valid_realized_scope(validator, &grant.scope, &root))
            {
                return Err(AgentServiceError::InvalidSnapshot);
            }
        }
    }
    Ok(())
}

pub(super) fn valid_realized_scope(
    validator: &jsonschema::Validator,
    scope: &CanonicalJson,
    workspace_root: &Path,
) -> bool {
    let mut portable = scope.as_value().clone();
    if let Some(root) = portable.get_mut("root") {
        if root.as_str() != Some(workspace_root.to_string_lossy().as_ref()) {
            return false;
        }
        *root = serde_json::Value::String("workspace".to_owned());
    }
    CanonicalJson::new(portable)
        .is_ok_and(|portable| validator.validate(portable.as_value()).is_ok())
}

pub(crate) fn drive(
    dependencies: &AgentDependencies,
    tools: &BTreeMap<String, PreparedTool>,
    runtime: Option<&tokio::runtime::Runtime>,
    progress: &ProgressSubscribers,
    cancellation: &RunCancellation,
    run: AgentRun,
) -> Result<(), AgentServiceError> {
    let deadline_at_ms = dependencies.store.agent_run_deadline_at_ms(run.id)?;
    let agl_core::agent::AgentRunOrigin::User {
        conversation_id, ..
    } = &run.origin;
    let conversation_id = Some(*conversation_id);
    // A generation reservation larger than half the realized window can make
    // even a minimal exact-state compaction impossible to resume. Apply this
    // at execution time so durable Conversations created with an older,
    // oversized Function reservation recover as well.
    let generation_max_output_tokens = effective_generation_max_output(
        run.snapshot.model.runtime.generation.max_output_tokens,
        run.snapshot.model.runtime.load.context_tokens,
    );
    let fsm = AgentFsm::new(
        run.id,
        conversation_id,
        run.snapshot.limits,
        generation_max_output_tokens,
        &run.snapshot.tools,
    );
    let mut state = AgentFsmState {
        status: run.status,
        checkpoint: run.checkpoint,
        usage: run.usage,
    };
    let mut revision = run.revision;
    loop {
        if state.status.is_terminal() {
            return Ok(());
        }
        if now_ms() >= deadline_at_ms
            && matches!(
                state.checkpoint,
                agl_core::agent::AgentCheckpoint::Ready { .. }
            )
        {
            let expired = fsm.transition(&state, AgentFsmInput::DeadlineReached)?;
            dependencies.store.commit_agent_transition(
                run.id,
                revision,
                &expired.state,
                &expired.output,
            )?;
            emit_durable(progress, run.id);
            return Ok(());
        }
        if cancellation.is_cancelled()
            && matches!(
                state.checkpoint,
                agl_core::agent::AgentCheckpoint::Ready { .. }
            )
        {
            let cancelled = fsm.transition(&state, AgentFsmInput::Cancel)?;
            dependencies.store.commit_agent_transition(
                run.id,
                revision,
                &cancelled.state,
                &cancelled.output,
            )?;
            emit_durable(progress, run.id);
            return Ok(());
        }
        let operation = match &state.checkpoint {
            agl_core::agent::AgentCheckpoint::Ready { .. } => {
                let driven = fsm.transition(&state, AgentFsmInput::Drive)?;
                dependencies.store.commit_agent_transition(
                    run.id,
                    revision,
                    &driven.state,
                    &driven.output,
                )?;
                emit_durable(progress, run.id);
                revision += 1;
                state = driven.state;
                if state.status.is_terminal() {
                    return Ok(());
                }
                driven
                    .output
                    .operation
                    .ok_or(AgentServiceError::InvalidTransition)?
            }
            agl_core::agent::AgentCheckpoint::Waiting { operation, .. } => {
                dependencies.store.agent_operation(operation)?
            }
        };
        let execution = OperationExecution {
            dependencies,
            tools,
            runtime,
            progress,
            cancellation,
            snapshot: &run.snapshot,
            conversation_id,
            deadline_at_ms,
        };
        let (previous_operation, operation, operation_output) = execution.complete(operation)?;
        let message_id = operation_message_id(&operation);
        let resumed = fsm.transition(
            &state,
            AgentFsmInput::OperationFinished {
                operation: operation.clone(),
                message_id,
            },
        )?;
        dependencies.store.commit_agent_cycle(
            &previous_operation,
            &operation,
            &operation_output,
            revision,
            &resumed.state,
            &resumed.output,
        )?;
        emit_durable(progress, run.id);
        revision += 1;
        state = resumed.state;
        if matches!(
            state.status,
            AgentRunStatus::Completed | AgentRunStatus::Failed | AgentRunStatus::Cancelled
        ) {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod realized_scope_tests {
    use super::*;

    #[test]
    fn generation_reservation_leaves_half_the_context_for_input() {
        assert_eq!(effective_generation_max_output(49_152, 62_208), 31_104);
        assert_eq!(effective_generation_max_output(8_192, 62_208), 8_192);
    }

    #[test]
    fn realized_workspace_is_checked_against_portable_effect_schema() {
        let schema = JsonSchema::new(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["root", "executables"],
            "properties": {
                "root": {"const": "workspace"},
                "executables": {"type": "array", "items": {"type": "string"}}
            }
        }))
        .unwrap();
        let validator = jsonschema::validator_for(schema.as_value()).unwrap();
        let scope = |root: &str| {
            CanonicalJson::new(serde_json::json!({
                "root": root,
                "executables": ["cargo"]
            }))
            .unwrap()
        };

        assert!(valid_realized_scope(
            &validator,
            &scope("/workspace/project"),
            Path::new("/workspace/project")
        ));
        assert!(!valid_realized_scope(
            &validator,
            &scope("/another/project"),
            Path::new("/workspace/project")
        ));
        assert!(!valid_realized_scope(
            &validator,
            &scope("workspace"),
            Path::new("/workspace/project")
        ));
    }

    #[test]
    fn memory_pointer_is_added_only_for_an_opted_in_workspace() {
        let root =
            std::env::temp_dir().join(format!("agl-memory-pointer-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        let instructions = InstructionSet::new(vec![]).unwrap();
        let absent = append_memory_pointer(&root, &instructions).unwrap();
        assert_eq!(absent, instructions);
        std::fs::create_dir(root.join("memory")).unwrap();
        let present = append_memory_pointer(&root, &instructions).unwrap();
        assert_eq!(present.blocks.len(), 1);
        assert!(
            present.blocks[0]
                .content
                .as_text()
                .contains("memory/INDEX.md")
        );
        assert_ne!(present.digest, instructions.digest);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn memory_pointer_does_not_follow_a_memory_symlink() {
        let root =
            std::env::temp_dir().join(format!("agl-memory-pointer-{}", uuid::Uuid::now_v7()));
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("memory")).unwrap();
        let instructions = InstructionSet::new(vec![]).unwrap();
        assert_eq!(
            append_memory_pointer(&root, &instructions).unwrap(),
            instructions
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
