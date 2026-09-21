use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agl_core::agent::{
    AgentRunOrigin, AgentRunStatus, InstructionBlock, InstructionSet, InstructionSource,
    MessageRole,
};
use agl_core::implementation_plan::{
    AdditionalRead, GitCommit, ImplementationPlan, ImplementationSlice, PlanDigest, PlanId,
    PlanState, PlanText, SliceResult, SliceResultSchema, SliceState, VerificationExpected,
    VerificationResult, WorkspacePath, WorkspaceSnapshot,
};
use agl_core::{Content, ConversationId, MessageId};
use agl_daemon_api::PlanArtifactView;
use agl_execution_api::{
    ExecutionClient, ExecutionIo, ExecutionOutcome, ExecutionOutputStream, ExecutionOwner,
    ExecutionStartRequest, ExecutionState,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::agent::AgentHandle;
use crate::plans::PlanArtifactStore;
use crate::store::StoreHandle;
use crate::workspace::{self, WorkspaceState};

static IMPLEMENTATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

const IMPLEMENTATION_RUN_WAIT_CAP_MS: i64 = 3_600_000;
const IMPLEMENTATION_RUN_CANCEL_GRACE_MS: i64 = 5_000;
const VERIFICATION_TIMEOUT_MS: u64 = 3_600_000;
const VERIFICATION_MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct CoderInput<'a> {
    schema: &'static str,
    plan_digest: PlanDigest,
    objective: &'a agl_core::implementation_plan::Objective,
    slice: &'a ImplementationSlice,
    evidence: Vec<&'a agl_core::implementation_plan::Evidence>,
    decisions: Vec<&'a agl_core::implementation_plan::Decision>,
    invariants: Vec<&'a agl_core::implementation_plan::Invariant>,
    required_reads: Vec<&'a agl_core::implementation_plan::ReadRequirement>,
    conditional_reads: Vec<&'a agl_core::implementation_plan::ConditionalRead>,
    predecessor_results: Vec<SliceResult>,
    current_workspace: &'a WorkspaceSnapshot,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CoderOutput {
    schema: SliceResultSchema,
    state: SliceState,
    #[serde(rename = "verification")]
    _verification: Vec<VerificationResult>,
    additional_reads: Vec<AdditionalRead>,
    failure: Option<PlanText>,
}

pub fn response_format() -> serde_json::Value {
    json!({
        "type": "json_schema",
        "json_schema": {
            "name": "agentlibre_slice_result_v1",
            "strict": true,
            "schema": {
                "type": "object", "additionalProperties": false,
                "required": ["schema", "state", "verification", "additional_reads", "failure"],
                "properties": {
                    "schema": {"const": "agentlibre.slice-result/v1"},
                    "state": {"enum": ["completed", "failed"]},
                    "verification": {"type":"array", "maxItems":256, "items": {"type":"object", "additionalProperties":false, "required":["command","passed","evidence"], "properties":{"command":{"type":"string"},"passed":{"type":"boolean"},"evidence":{"type":"string"}}}},
                    "additional_reads": {"type":"array", "maxItems":256, "items": {"type":"object", "additionalProperties":false, "required":["path","justification"], "properties":{"path":{"type":"string"},"justification":{"type":"string"}}}},
                    "failure": {"anyOf":[{"type":"string"},{"type":"null"}]}
                }
            }
        }
    })
}

impl PlanArtifactStore {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn implement(
        &self,
        store: &StoreHandle,
        agent: &AgentHandle,
        runtime: &agl_runtime::RuntimeHandle,
        execution: &ExecutionClient,
        id: &PlanId,
        expected: &PlanDigest,
        function_path: &Path,
    ) -> Result<PlanArtifactView> {
        let _lock = IMPLEMENTATION_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| anyhow::anyhow!("implementation scheduler lock poisoned"))?;
        let approved = self.read_approved(expected)?;
        ensure!(
            &approved.plan.id == id,
            "approved artifact does not match plan ID"
        );
        let plan = approved.plan;
        ensure!(
            plan.open_decisions.is_empty(),
            "approved plan contains open decisions"
        );
        let record = store.plan_record(id.clone())?;
        if matches!(
            record.state,
            PlanState::Completed | PlanState::Failed | PlanState::Stale
        ) {
            return self.view(store, id);
        }
        let root = PathBuf::from(record.workspace_root);
        ensure!(
            root.is_absolute(),
            "approved plan workspace is not absolute"
        );
        ensure!(
            plan.workspace_matches(&root)?,
            "workspace root differs from approved plan"
        );
        let slice_ids = plan
            .slices
            .iter()
            .map(|slice| slice.id.as_str().to_owned())
            .collect::<Vec<_>>();
        let initial = match record.state {
            PlanState::Approved => {
                let initial = workspace::capture(&root)?;
                let git = match git_evidence(execution, &root, plan.workspace.git_commit.is_some())
                {
                    Ok(git) => git,
                    Err(error) => {
                        store.mark_initial_plan_stale(id, expected, &slice_ids, &[])?;
                        return Err(error).context("cannot establish exact Git workspace evidence");
                    }
                };
                if let Err(error) = ensure_initial_evidence_with_git(&plan, &root, &git) {
                    let paths = initial_evidence_mismatch_paths(&plan, &root, &git);
                    store.mark_initial_plan_stale(id, expected, &slice_ids, &paths)?;
                    return Err(error);
                }
                store.begin_plan_implementation(
                    id.clone(),
                    expected,
                    &slice_ids,
                    &initial.snapshot,
                )?;
                initial
            }
            PlanState::Implementing => snapshot_state(
                &record
                    .initial_workspace
                    .context("implementing plan has no durable initial workspace")?,
            ),
            _ => bail!("plan is not approved or implementing"),
        };
        let slice_records = store.slice_records(id)?;
        let order = execution_order(&plan)?;
        let mut completed = Vec::new();
        let mut expected_snapshot = initial.snapshot.clone();
        for slice in &order {
            let record = slice_records
                .iter()
                .find(|record| record.slice_id == slice.id.as_str())
                .context("approved slice has no durable state")?;
            if record.state == SliceState::Completed {
                let result = record
                    .result
                    .as_ref()
                    .context("completed slice has no durable result")?;
                ensure_result_identity(result, id, expected)?;
                ensure_result_chain(result, &expected_snapshot)?;
                expected_snapshot = result.workspace.clone();
                completed.push(result.clone());
            }
        }
        let running = store
            .slice_records(id)?
            .into_iter()
            .filter(|record| record.state == SliceState::Running)
            .collect::<Vec<_>>();
        if !running.is_empty() {
            let current = workspace::capture(&root)?;
            let expected_state = snapshot_state(&expected_snapshot);
            if current.snapshot != expected_snapshot {
                let paths = current
                    .changed_paths(&expected_state)
                    .into_iter()
                    .map(|path| path.path.as_str().to_owned())
                    .collect::<Vec<_>>();
                store.mark_plan_stale(id, &paths)?;
                bail!(
                    "workspace drift found while recovering a running slice; changed paths: {}",
                    paths.join(", ")
                );
            }
            for record in running {
                let slice = plan
                    .slices
                    .iter()
                    .find(|slice| slice.id.as_str() == record.slice_id)
                    .context("durable running slice is absent from the approved plan")?;
                let result = interrupted_result(&plan, slice, expected, &current.snapshot)?;
                finish_claimed_slice(store, id, slice, result)?;
            }
            return self.view(store, id);
        }
        for slice in execution_order(&plan)? {
            if completed
                .iter()
                .any(|result| result.slice_id == slice.id && result.state == SliceState::Completed)
            {
                continue;
            }
            let before = workspace::capture(&root)?;
            if before.snapshot != expected_snapshot {
                let expected_state = snapshot_state(&expected_snapshot);
                let paths = before
                    .changed_paths(&expected_state)
                    .into_iter()
                    .map(|path| path.path.as_str().to_owned())
                    .collect::<Vec<_>>();
                store.mark_plan_stale(id, &paths)?;
                bail!(
                    "workspace drift invalidated implementation; changed paths: {}",
                    paths.join(", ")
                );
            }
            if !slice.depends_on.iter().all(|dependency| {
                completed.iter().any(|result| {
                    result.slice_id == *dependency && result.state == SliceState::Completed
                })
            }) {
                bail!("slice {} has an unfinished dependency", slice.id);
            }
            ensure!(
                store.claim_slice(id, slice.id.as_str())?,
                "slice {} is already running or committed",
                slice.id
            );
            let rechecked = match workspace::capture(&root) {
                Ok(rechecked) => rechecked,
                Err(error) => {
                    let result =
                        failed_result(id, slice, expected, &before, &before, error.into())?;
                    finish_claimed_slice(store, id, slice, result)?;
                    return self.view(store, id);
                }
            };
            if rechecked.snapshot != before.snapshot {
                let paths = rechecked
                    .changed_paths(&before)
                    .into_iter()
                    .map(|path| path.path.as_str().to_owned())
                    .collect::<Vec<_>>();
                mark_claimed_slice_stale(store, id, slice, expected, &before, &rechecked, &paths)?;
                bail!("workspace drift detected before slice operation");
            }
            let result = match execute_slice(
                store,
                agent,
                runtime,
                execution,
                &plan,
                slice,
                &completed,
                &before,
                function_path,
                root.clone(),
            ) {
                Ok(result) => result,
                Err(error) => {
                    let after = workspace::capture(&root).unwrap_or_else(|_| before.clone());
                    let result = failed_result(id, slice, expected, &before, &after, error)?;
                    finish_claimed_slice(store, id, slice, result)?;
                    return self.view(store, id);
                }
            };
            let result = finish_claimed_slice(store, id, slice, result)?;
            if result.state == SliceState::Failed {
                return self.view(store, id);
            }
            expected_snapshot = result.workspace.clone();
            completed.push(result);
        }
        self.view(store, id)
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_slice(
    store: &StoreHandle,
    agent: &AgentHandle,
    runtime: &agl_runtime::RuntimeHandle,
    execution: &ExecutionClient,
    plan: &ImplementationPlan,
    slice: &ImplementationSlice,
    completed: &[SliceResult],
    before: &WorkspaceState,
    function_path: &Path,
    root: PathBuf,
) -> Result<SliceResult> {
    let activated = tokio::runtime::Handle::current()
        .block_on(runtime.activate(function_path, &root))
        .context("failed to activate verified coder Function")?;
    let mut snapshot = activated.snapshot.clone();
    let mut instructions = snapshot.instructions.blocks.clone();
    instructions.push(InstructionBlock {
        source: InstructionSource::Agent,
        content: Content::text(
            "You are executing exactly one implementation slice. Modify only the files listed in input.slice.files; do not implement later slices and do not modify any unlisted file, including generated test caches or build artifacts. Run only the verification for the current slice. When verifying Python code, use python3 -B to prevent __pycache__; for pytest, also pass -p no:cacheprovider to prevent .pytest_cache. If verification creates an undeclared artifact anyway, remove it before returning. Your final response MUST be exactly one JSON object with exactly these keys: schema, state, verification, additional_reads, failure. Set schema to agentlibre.slice-result/v1; state to completed or failed; verification to an array of {command, passed, evidence}; additional_reads to an array of {path, justification}; failure to null on success or a string on failure. Do not return slice_id, status, outcome, steps_completed, files_modified, done_when, acceptance_covered, notes, Markdown, or commentary.",
        )?,
    });
    snapshot.instructions = InstructionSet::new(instructions).map_err(anyhow::Error::msg)?;
    snapshot.response_format = Some(response_format());
    let function = agl_core::agent::ExactPackageRef {
        id: activated.function_id.clone(),
        version: activated.function_version.clone(),
        digest: activated.function_digest,
    };
    let conversation_id = ConversationId::generate();
    store.create_conversation(conversation_id, &function, &snapshot)?;
    let input = coder_input(plan, slice, completed, &before.snapshot)?;
    let message_id = MessageId::generate();
    let run_id = agent.start_run(agl_core::agent::AgentRunSpec {
        origin: AgentRunOrigin::User {
            conversation_id,
            message_id,
        },
        input: Content::text(serde_json::to_string(&input)?)?,
        reasoning: None,
    })?;
    let run = wait_for_run(store, agent, run_id)?;
    let after = workspace::capture(&root)?;
    let changed_paths = after.changed_paths(before);
    let output =
        latest_assistant(store, conversation_id).and_then(|value| decode_coder_output(&value));
    let (coder_state, additional_reads, coder_failure, outside_write_set) =
        match (run.status, output) {
            (AgentRunStatus::Completed, Some(output)) if output.schema == SliceResultSchema::V1 => {
                let outside = changed_paths
                    .iter()
                    .any(|path| !slice.files.iter().any(|file| file.path == path.path));
                if outside {
                    (
                        SliceState::Failed,
                        output.additional_reads,
                        Some(
                            PlanText::new(
                                "coder changed a path outside the declared slice write set",
                            )
                            .unwrap(),
                        ),
                        true,
                    )
                } else {
                    (output.state, output.additional_reads, output.failure, false)
                }
            }
            (_, Some(output)) => (
                SliceState::Failed,
                output.additional_reads,
                output
                    .failure
                    .or_else(|| PlanText::new("coder returned an invalid slice result").ok()),
                false,
            ),
            (_, None) => (
                SliceState::Failed,
                Vec::new(),
                PlanText::new("coder produced no valid structured slice result").ok(),
                false,
            ),
        };
    let verification = run_approved_verifications(execution, slice, &root);
    let all_verifications_passed = verification.iter().all(|result| result.passed)
        && verification.len() == slice.verification.len();
    let state =
        if coder_state == SliceState::Completed && !outside_write_set && all_verifications_passed {
            SliceState::Completed
        } else {
            SliceState::Failed
        };
    let failure = (state == SliceState::Failed)
        .then(|| {
            coder_failure.or_else(|| {
                PlanText::new("runtime verification failed; slice was not completed").ok()
            })
        })
        .flatten();
    Ok(SliceResult {
        schema: SliceResultSchema::V1,
        plan_id: plan.id.clone(),
        plan_digest: plan.digest()?,
        slice_id: slice.id.clone(),
        state,
        changed_paths,
        verification,
        additional_reads,
        conversation_id: Some(conversation_id),
        run_id: Some(run_id),
        workspace: after.snapshot,
        failure,
    })
}

fn coder_input<'a>(
    plan: &'a ImplementationPlan,
    slice: &'a ImplementationSlice,
    completed: &[SliceResult],
    current_workspace: &'a WorkspaceSnapshot,
) -> Result<CoderInput<'a>> {
    let conditions = slice
        .start_condition
        .iter()
        .map(|id| id.as_str())
        .collect::<HashSet<_>>();
    let evidence = plan
        .evidence
        .iter()
        .filter(|item| conditions.contains(item.id.as_str()))
        .collect();
    // Decisions and invariants are plan-wide constraints. The wire model does
    // not have a per-slice association for them, so omitting one here would
    // force the fresh coder Conversation to rediscover a recorded constraint.
    let decisions = plan.decisions.iter().collect();
    let invariants = plan.invariants.iter().collect();
    let required_ids = slice
        .required_reads
        .iter()
        .map(|read| read.id.as_str())
        .collect::<HashSet<_>>();
    let required_reads = slice
        .required_reads
        .iter()
        .filter(|read| required_ids.contains(read.id.as_str()))
        .collect();
    let conditional_reads = slice.conditional_reads.iter().collect();
    let predecessor_results = completed
        .iter()
        .filter(|result| slice.depends_on.iter().any(|id| id == &result.slice_id))
        .cloned()
        .collect();
    Ok(CoderInput {
        schema: "agentlibre.coder-input/v1",
        plan_digest: plan.digest()?,
        objective: &plan.objective,
        slice,
        evidence,
        decisions,
        invariants,
        required_reads,
        conditional_reads,
        predecessor_results,
        current_workspace,
    })
}

fn finish_claimed_slice(
    store: &StoreHandle,
    plan_id: &PlanId,
    slice: &ImplementationSlice,
    result: SliceResult,
) -> Result<SliceResult> {
    match store.finish_slice(plan_id, slice.id.as_str(), &result) {
        Ok(()) => Ok(result),
        Err(first_error) => {
            let mut fallback = result.clone();
            fallback.state = SliceState::Failed;
            fallback.failure = Some(bounded_failure_text(anyhow::anyhow!(
                "could not persist slice result: {first_error}"
            ))?);
            match store.finish_slice(plan_id, slice.id.as_str(), &fallback) {
                Ok(()) => Ok(fallback),
                Err(recovery_error) => match store.slice_records(plan_id) {
                    Ok(records) => records
                        .into_iter()
                        .find(|record| record.slice_id == slice.id.as_str())
                        .and_then(|record| {
                            matches!(record.state, SliceState::Completed | SliceState::Failed)
                                .then_some(record.result)
                                .flatten()
                        })
                        .ok_or_else(|| {
                            anyhow::Error::new(first_error).context(format!(
                                "slice result persistence failed and failed-state recovery also failed: {recovery_error}"
                            ))
                        }),
                    Err(state_error) => Err(first_error).context(format!(
                        "slice result persistence failed, failed-state recovery failed: {recovery_error}; durable state unreadable: {state_error}"
                    )),
                },
            }
        }
    }
}

fn mark_claimed_slice_stale(
    store: &StoreHandle,
    plan_id: &PlanId,
    slice: &ImplementationSlice,
    expected: &PlanDigest,
    before: &WorkspaceState,
    after: &WorkspaceState,
    paths: &[String],
) -> Result<()> {
    match store.mark_plan_stale(plan_id, paths) {
        Ok(()) => Ok(()),
        Err(stale_error) => {
            let failure = failed_result(
                plan_id,
                slice,
                expected,
                before,
                after,
                anyhow::anyhow!(
                    "workspace drift detected but stale transition failed: {stale_error}"
                ),
            )?;
            finish_claimed_slice(store, plan_id, slice, failure).map(|_| ())
        }
    }
}

fn wait_for_run(
    store: &StoreHandle,
    agent: &AgentHandle,
    run_id: agl_core::AgentRunId,
) -> Result<agl_core::agent::AgentRunView> {
    let deadline_at_ms = store.agent_run_deadline_at_ms(run_id)?;
    wait_for_run_with(
        deadline_at_ms,
        || store.agent_run_view(run_id).map_err(anyhow::Error::from),
        || agent.cancel(run_id).map_err(anyhow::Error::from),
        now_ms,
        std::thread::sleep,
    )
}

fn wait_for_run_with<I, C, N, S>(
    deadline_at_ms: i64,
    mut inspect: I,
    mut cancel: C,
    mut now: N,
    mut sleep: S,
) -> Result<agl_core::agent::AgentRunView>
where
    I: FnMut() -> Result<agl_core::agent::AgentRunView>,
    C: FnMut() -> Result<()>,
    N: FnMut() -> i64,
    S: FnMut(Duration),
{
    let started_at_ms = now();
    let hard_deadline_at_ms = deadline_at_ms.min(
        started_at_ms
            .saturating_add(IMPLEMENTATION_RUN_WAIT_CAP_MS)
            .max(started_at_ms),
    );
    let mut cancellation_deadline_at_ms = None;
    loop {
        let view = inspect()?;
        if view.status.is_terminal() {
            return Ok(view);
        }
        let current_ms = now();
        if let Some(cancel_deadline) = cancellation_deadline_at_ms {
            if current_ms >= cancel_deadline {
                bail!(
                    "agent run {} remained non-terminal after cancellation",
                    view.id
                );
            }
        } else if current_ms >= hard_deadline_at_ms {
            cancel().context("failed to cancel non-terminal implementation agent run")?;
            cancellation_deadline_at_ms =
                Some(current_ms.saturating_add(IMPLEMENTATION_RUN_CANCEL_GRACE_MS));
        }
        sleep(Duration::from_millis(50));
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn run_approved_verifications(
    execution: &ExecutionClient,
    slice: &ImplementationSlice,
    root: &Path,
) -> Vec<VerificationResult> {
    slice
        .verification
        .iter()
        .map(|verification| run_verification(execution, verification, root))
        .collect()
}

fn run_verification(
    execution: &ExecutionClient,
    verification: &agl_core::implementation_plan::Verification,
    root: &Path,
) -> VerificationResult {
    let command = verification.command.clone();
    let failure = |message: String| VerificationResult {
        command: command.clone(),
        passed: false,
        evidence: bounded_failure_text(anyhow::anyhow!(message))
            .unwrap_or_else(|_| PlanText::new("verification failed").unwrap()),
    };
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(error) => return failure(format!("workspace root is not accessible: {error}")),
    };
    let working_directory = root.join(verification.working_directory.as_str());
    let working_directory = match working_directory.canonicalize() {
        Ok(path) if path.starts_with(&root) && path.is_dir() => path,
        Ok(path) => {
            return failure(format!(
                "verification directory escapes workspace: {}",
                path.display()
            ));
        }
        Err(error) => return failure(format!("verification directory is not accessible: {error}")),
    };
    let request = ExecutionStartRequest {
        owner: ExecutionOwner::Runtime {
            component: "agl-daemon.implementation-verification".into(),
        },
        argv: vec!["/bin/sh".into(), "-c".into(), command.as_str().into()],
        cwd: working_directory.to_string_lossy().into_owned(),
        environment: Default::default(),
        clear_environment: false,
        io: ExecutionIo::Pipes,
        timeout_ms: VERIFICATION_TIMEOUT_MS,
        max_output_bytes: VERIFICATION_MAX_OUTPUT_BYTES,
        terminal_size: None,
        isolation: Default::default(),
    };
    let handle = tokio::runtime::Handle::current();
    let started = match handle.block_on(execution.start(request)) {
        Ok(status) => status,
        Err(error) => return failure(format!("verification could not start: {error}")),
    };
    let mut cursor = 0;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        let status = match handle.block_on(execution.inspect(started.execution_id)) {
            Ok(status) => status,
            Err(error) => return failure(format!("verification status failed: {error}")),
        };
        let output = match handle.block_on(execution.read(
            started.execution_id,
            cursor,
            agl_execution_api::MAX_EXECUTION_OUTPUT_READ_BYTES,
        )) {
            Ok(output) => output,
            Err(error) => return failure(format!("verification output failed: {error}")),
        };
        for chunk in output.chunks {
            match chunk.stream {
                ExecutionOutputStream::Stdout => stdout.extend(chunk.data),
                ExecutionOutputStream::Stderr => stderr.extend(chunk.data),
                ExecutionOutputStream::Pty => {
                    return failure("verification returned PTY output".into());
                }
            }
        }
        cursor = output.next;
        if output.eof && status.state != ExecutionState::Running {
            let truncated = output.truncated || status.output_truncated;
            let (passed, evidence) = evaluate_verification(
                &verification.expected,
                status.state,
                status.outcome.as_ref(),
                &stdout,
                &stderr,
                truncated,
            );
            return VerificationResult {
                command,
                passed,
                evidence,
            };
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn evaluate_verification(
    expected: &VerificationExpected,
    state: ExecutionState,
    outcome: Option<&ExecutionOutcome>,
    stdout: &[u8],
    stderr: &[u8],
    truncated: bool,
) -> (bool, PlanText) {
    let stdout_text = String::from_utf8_lossy(stdout);
    let stderr_text = String::from_utf8_lossy(stderr);
    let exit_matches = matches!(
        (state, outcome),
        (ExecutionState::Exited, Some(ExecutionOutcome::Exit { code }))
            if *code == expected.exit_code
    );
    let stdout_matches = expected
        .stdout_contains
        .iter()
        .all(|needle| stdout_text.contains(needle.as_str()));
    let stderr_matches = expected
        .stderr_contains
        .iter()
        .all(|needle| stderr_text.contains(needle.as_str()));
    let passed = !truncated && exit_matches && stdout_matches && stderr_matches;
    let evidence = bounded_failure_text(anyhow::anyhow!(
        "state={state:?}; outcome={outcome:?}; truncated={truncated}; exit_code_expected={}; stdout_predicates_match={stdout_matches}; stderr_predicates_match={stderr_matches}; stdout={:?}; stderr={:?}",
        expected.exit_code,
        bounded_output(&stdout_text),
        bounded_output(&stderr_text),
    ))
    .unwrap_or_else(|_| PlanText::new("verification result could not be recorded").unwrap());
    (passed, evidence)
}

fn bounded_output(value: &str) -> String {
    const MAX_EVIDENCE_OUTPUT: usize = 4 * 1024;
    value.chars().take(MAX_EVIDENCE_OUTPUT).collect()
}

fn latest_assistant(store: &StoreHandle, conversation: ConversationId) -> Option<String> {
    store
        .conversation_messages(conversation, None, 1_000)
        .ok()?
        .messages
        .into_iter()
        .rev()
        .find(|message| message.role == MessageRole::Assistant)
        .map(|message| message.content.as_text().to_owned())
}

fn decode_coder_output(value: &str) -> Option<CoderOutput> {
    let value = value.trim();
    let object = &value[value.find('{')?..];
    serde_json::from_str(object).ok()
}

fn execution_order(plan: &ImplementationPlan) -> Result<Vec<&ImplementationSlice>> {
    let mut remaining = plan.slices.iter().collect::<Vec<_>>();
    let mut done = HashSet::new();
    let mut order = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let Some(index) = remaining
            .iter()
            .position(|slice| slice.depends_on.iter().all(|id| done.contains(id.as_str())))
        else {
            bail!("slice dependency graph cannot be scheduled")
        };
        let slice = remaining.remove(index);
        done.insert(slice.id.as_str().to_owned());
        order.push(slice);
    }
    Ok(order)
}

fn ensure_result_identity(
    result: &SliceResult,
    plan_id: &PlanId,
    expected: &PlanDigest,
) -> Result<()> {
    ensure!(
        result.plan_id == *plan_id && result.plan_digest == *expected,
        "durable slice result does not match the approved plan"
    );
    ensure!(
        result.state == SliceState::Completed,
        "durable completed slice result has a non-completed state"
    );
    Ok(())
}

fn ensure_result_chain(result: &SliceResult, before: &WorkspaceSnapshot) -> Result<()> {
    let expected = snapshot_state(&result.workspace).changed_paths(&snapshot_state(before));
    ensure!(
        canonical_changed_paths(&result.changed_paths) == canonical_changed_paths(&expected),
        "durable slice result changed paths do not match its workspace snapshots"
    );
    Ok(())
}

fn canonical_changed_paths(
    paths: &[agl_core::implementation_plan::SliceChangedPath],
) -> Vec<agl_core::implementation_plan::SliceChangedPath> {
    let mut paths = paths.to_vec();
    paths.sort_by(|left, right| left.path.as_str().cmp(right.path.as_str()));
    paths
}

fn snapshot_state(snapshot: &WorkspaceSnapshot) -> WorkspaceState {
    WorkspaceState {
        snapshot: snapshot.clone(),
        files: snapshot
            .files
            .iter()
            .map(|file| (file.path.as_str().to_owned(), file.digest.clone()))
            .collect(),
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct GitEvidence {
    commit: Option<GitCommit>,
    dirty_paths: Vec<WorkspacePath>,
}

fn ensure_initial_evidence_with_git(
    plan: &ImplementationPlan,
    root: &Path,
    git: &GitEvidence,
) -> Result<()> {
    ensure!(
        plan.workspace_matches(root)?,
        "workspace root differs from approved plan evidence"
    );
    let expected = plan
        .workspace
        .dirty_paths
        .iter()
        .map(|path| path.as_str())
        .collect::<HashSet<_>>();
    let actual = git
        .dirty_paths
        .iter()
        .map(|path| path.as_str())
        .collect::<HashSet<_>>();
    ensure!(
        expected == actual,
        "workspace dirty path evidence differs from approved plan"
    );
    if let Some(expected_commit) = &plan.workspace.git_commit {
        ensure!(
            git.commit.as_ref() == Some(expected_commit),
            "workspace Git commit evidence differs from approved plan"
        );
    }
    Ok(())
}

fn initial_evidence_mismatch_paths(
    plan: &ImplementationPlan,
    root: &Path,
    git: &GitEvidence,
) -> Vec<String> {
    let expected = plan
        .workspace
        .dirty_paths
        .iter()
        .map(|path| path.as_str().to_owned())
        .collect::<HashSet<_>>();
    let actual = git
        .dirty_paths
        .iter()
        .map(|path| path.as_str().to_owned())
        .collect::<HashSet<_>>();
    let mut paths = expected
        .symmetric_difference(&actual)
        .cloned()
        .collect::<Vec<_>>();
    if plan.workspace.git_commit.is_some()
        && plan.workspace.git_commit.as_ref() != git.commit.as_ref()
    {
        paths.push(".".to_owned());
    }
    if !plan.workspace_matches(root).unwrap_or(false) {
        paths.push(".".to_owned());
    }
    paths.sort();
    paths.dedup();
    paths
}

fn git_evidence(
    client: &ExecutionClient,
    root: &Path,
    require_commit: bool,
) -> Result<GitEvidence> {
    let git = git_executable()?;
    let status = run_read_only_command(
        client,
        root,
        vec![
            git.to_string_lossy().into_owned(),
            "status".into(),
            "--porcelain=v1".into(),
            "-z".into(),
            "--untracked-files=all".into(),
        ],
    )?;
    let commit = if require_commit {
        let commit = run_read_only_command(
            client,
            root,
            vec![
                git.to_string_lossy().into_owned(),
                "rev-parse".into(),
                "--verify".into(),
                "HEAD".into(),
            ],
        )?;
        Some(
            GitCommit::new(
                String::from_utf8(commit)
                    .context("Git commit evidence is not UTF-8")?
                    .trim()
                    .to_owned(),
            )
            .map_err(anyhow::Error::msg)?,
        )
    } else {
        None
    };
    Ok(GitEvidence {
        commit,
        dirty_paths: parse_git_dirty_paths(&status)?,
    })
}

fn git_executable() -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is unavailable for Git evidence")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join("git");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("Git executable is unavailable for exact workspace evidence")
}

fn run_read_only_command(
    client: &ExecutionClient,
    root: &Path,
    argv: Vec<String>,
) -> Result<Vec<u8>> {
    let request = ExecutionStartRequest {
        owner: ExecutionOwner::Runtime {
            component: "agl-daemon.workspace-evidence".into(),
        },
        argv,
        cwd: root
            .to_str()
            .context("workspace root is not UTF-8")?
            .to_owned(),
        environment: Default::default(),
        clear_environment: false,
        io: ExecutionIo::Pipes,
        timeout_ms: 30_000,
        max_output_bytes: 4 * 1024 * 1024,
        terminal_size: None,
        isolation: Default::default(),
    };
    let handle = tokio::runtime::Handle::current();
    let started = handle
        .block_on(client.start(request))
        .context("failed to start workspace evidence command")?;
    let mut after = 0;
    let mut stdout = Vec::new();
    loop {
        let status = handle
            .block_on(client.inspect(started.execution_id))
            .context("failed to inspect workspace evidence command")?;
        let output = handle
            .block_on(client.read(started.execution_id, after, 1024 * 1024))
            .context("failed to read workspace evidence command")?;
        for chunk in output.chunks {
            if chunk.stream == ExecutionOutputStream::Stdout {
                stdout.extend_from_slice(&chunk.data);
            }
        }
        after = output.next;
        if output.eof && status.state != ExecutionState::Running {
            match status.outcome {
                Some(ExecutionOutcome::Exit { code: 0 }) => return Ok(stdout),
                Some(outcome) => bail!("workspace evidence command failed: {outcome:?}"),
                None => bail!("workspace evidence command exited without an outcome"),
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn parse_git_dirty_paths(output: &[u8]) -> Result<Vec<WorkspacePath>> {
    let mut paths = Vec::new();
    let mut records = output.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        ensure!(record.len() >= 4, "Git status record is malformed");
        let status = &record[..2];
        let path = std::str::from_utf8(&record[3..])
            .context("Git status path is not UTF-8")?
            .replace('\\', "/");
        paths.push(WorkspacePath::new(path).map_err(anyhow::Error::msg)?);
        if status[0] == b'R' || status[0] == b'C' {
            let original = records
                .next()
                .context("Git rename status record has no original path")?;
            let original = std::str::from_utf8(original)
                .context("Git original path is not UTF-8")?
                .replace('\\', "/");
            paths.push(WorkspacePath::new(original).map_err(anyhow::Error::msg)?);
        }
    }
    paths.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    paths.dedup_by(|left, right| left == right);
    Ok(paths)
}

fn interrupted_result(
    plan: &ImplementationPlan,
    slice: &ImplementationSlice,
    expected: &PlanDigest,
    workspace: &WorkspaceSnapshot,
) -> Result<SliceResult> {
    Ok(SliceResult {
        schema: SliceResultSchema::V1,
        plan_id: plan.id.clone(),
        plan_digest: expected.clone(),
        slice_id: slice.id.clone(),
        state: SliceState::Failed,
        changed_paths: Vec::new(),
        verification: Vec::new(),
        additional_reads: Vec::new(),
        conversation_id: None,
        run_id: None,
        workspace: workspace.clone(),
        failure: Some(
            PlanText::new("slice was running during daemon restart").map_err(anyhow::Error::msg)?,
        ),
    })
}

fn failed_result(
    plan_id: &PlanId,
    slice: &ImplementationSlice,
    expected: &PlanDigest,
    before: &WorkspaceState,
    after: &WorkspaceState,
    error: anyhow::Error,
) -> Result<SliceResult> {
    Ok(SliceResult {
        schema: SliceResultSchema::V1,
        plan_id: plan_id.clone(),
        plan_digest: expected.clone(),
        slice_id: slice.id.clone(),
        state: SliceState::Failed,
        changed_paths: after.changed_paths(before),
        verification: Vec::new(),
        additional_reads: Vec::new(),
        conversation_id: None,
        run_id: None,
        workspace: after.snapshot.clone(),
        failure: Some(bounded_failure_text(error)?),
    })
}

fn bounded_failure_text(error: anyhow::Error) -> Result<PlanText> {
    let mut value = error
        .to_string()
        .chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\t') {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    if value.is_empty() {
        value.push_str("slice execution failed");
    }
    if value.len() > agl_core::MAX_TEXT_BYTES {
        let end = value
            .char_indices()
            .take_while(|(index, _)| *index <= agl_core::MAX_TEXT_BYTES)
            .last()
            .map(|(index, character)| index + character.len_utf8())
            .unwrap_or(agl_core::MAX_TEXT_BYTES);
        value.truncate(end.min(agl_core::MAX_TEXT_BYTES));
    }
    PlanText::new(value).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agl_core::implementation_plan::*;
    use std::fs;

    fn text(value: &str) -> PlanText {
        PlanText::new(value).unwrap()
    }

    #[test]
    fn approved_verification_checks_exit_streams_and_complete_output() {
        let expected = VerificationExpected {
            exit_code: 0,
            stdout_contains: vec![text("ready")],
            stderr_contains: vec![text("warning")],
        };
        let good = Some(ExecutionOutcome::Exit { code: 0 });
        let args = (b"ready".as_slice(), b"warning".as_slice());
        assert!(
            evaluate_verification(
                &expected,
                ExecutionState::Exited,
                good.as_ref(),
                args.0,
                args.1,
                false
            )
            .0
        );
        assert!(
            !evaluate_verification(
                &expected,
                ExecutionState::Exited,
                good.as_ref(),
                args.1,
                args.0,
                false
            )
            .0
        );
        assert!(
            !evaluate_verification(
                &expected,
                ExecutionState::Exited,
                good.as_ref(),
                args.0,
                args.1,
                true
            )
            .0
        );
        assert!(
            !evaluate_verification(
                &expected,
                ExecutionState::Exited,
                Some(&ExecutionOutcome::Exit { code: 1 }),
                args.0,
                args.1,
                false
            )
            .0
        );
        assert!(
            !evaluate_verification(
                &expected,
                ExecutionState::OutcomeUnknown,
                Some(&ExecutionOutcome::UnknownAfterServiceRestart),
                args.0,
                args.1,
                false
            )
            .0
        );
    }

    #[test]
    fn nonterminal_run_is_cancelled_and_wait_exits_after_grace_period() {
        use std::cell::Cell;

        let run_id = agl_core::AgentRunId::generate();
        let view = agl_core::agent::AgentRunView {
            id: run_id,
            origin: AgentRunOrigin::User {
                conversation_id: ConversationId::generate(),
                message_id: MessageId::generate(),
            },
            status: AgentRunStatus::Running,
            usage: Default::default(),
            current_operation: None,
            failure: None,
            last_event_id: agl_core::agent::AgentEventId(0),
        };
        let clock = Cell::new(0_i64);
        let cancellations = Cell::new(0_usize);
        let result = wait_for_run_with(
            100,
            || Ok(view.clone()),
            || {
                cancellations.set(cancellations.get() + 1);
                Ok(())
            },
            || clock.get(),
            |_| clock.set(clock.get() + 1000),
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("non-terminal after cancellation")
        );
        assert_eq!(cancellations.get(), 1);
    }

    fn id(value: &str) -> PlanItemId {
        PlanItemId::new(value).unwrap()
    }

    fn source() -> SourceRef {
        SourceRef {
            kind: SourceKind::Message,
            locator: text("test"),
            digest: None,
            line_start: None,
            line_end: None,
        }
    }

    #[test]
    fn coder_output_accepts_one_trailing_structured_object() {
        let valid = r#"{"schema":"agentlibre.slice-result/v1","state":"completed","verification":[],"additional_reads":[],"failure":null}"#;
        assert!(decode_coder_output(valid).is_some());
        assert!(decode_coder_output(&format!("Verification passed.\n\n{valid}")).is_some());
        assert!(decode_coder_output(&format!("{valid}\nExtra commentary")).is_none());
        assert!(decode_coder_output(&format!("{valid}\n{valid}")).is_none());
        assert!(decode_coder_output(r#"{"status":"complete"}"#).is_none());
    }

    fn slice(name: &str, depends_on: Vec<SliceId>) -> ImplementationSlice {
        let file = WorkspacePath::new(format!("{name}.rs")).unwrap();
        ImplementationSlice {
            id: id(name),
            outcome: text("implemented"),
            depends_on,
            files: vec![FileChange {
                path: file.clone(),
                disposition: FileDisposition::Modify,
                purpose: text("change"),
                symbols: vec![],
            }],
            required_reads: vec![],
            conditional_reads: vec![],
            start_condition: vec![id("evidence")],
            steps: vec![ImplementationStep {
                id: id(&format!("{name}-step")),
                action: text("change"),
                files: vec![file],
                symbols: vec![],
                depends_on: vec![],
                satisfies: vec![id("accept")],
            }],
            verification: vec![Verification {
                command: text("test"),
                working_directory: WorkspacePath::new(".").unwrap(),
                expected: VerificationExpected {
                    exit_code: 0,
                    stdout_contains: vec![text("pass")],
                    stderr_contains: Vec::new(),
                },
                covers: vec![id("accept")],
            }],
            done_when: vec![Check {
                id: id(&format!("{name}-done")),
                statement: text("done"),
                sources: vec![source()],
            }],
        }
    }

    fn plan(
        slices: Vec<ImplementationSlice>,
        dirty_paths: Vec<WorkspacePath>,
    ) -> ImplementationPlan {
        ImplementationPlan {
            schema: PlanSchema::V1,
            id: PlanId::generate(),
            workspace: WorkspaceEvidence {
                canonical_root_sha256: PlanDigest::from_bytes([0; 32]),
                git_commit: None,
                dirty_paths,
            },
            objective: Objective {
                outcome: text("outcome"),
                acceptance: vec![Check {
                    id: id("accept"),
                    statement: text("accepted"),
                    sources: vec![source()],
                }],
                sources: vec![source()],
            },
            evidence: vec![Evidence {
                id: id("evidence"),
                statement: text("fact"),
                sources: vec![source()],
            }],
            decisions: vec![],
            invariants: vec![],
            slices,
            open_decisions: vec![],
        }
    }

    fn result(
        plan_id: PlanId,
        digest: PlanDigest,
        slice_id: SliceId,
        workspace: WorkspaceSnapshot,
        state: SliceState,
    ) -> SliceResult {
        SliceResult {
            schema: SliceResultSchema::V1,
            plan_id,
            plan_digest: digest,
            slice_id,
            state,
            changed_paths: vec![],
            verification: vec![],
            additional_reads: vec![],
            conversation_id: Some(ConversationId::generate()),
            run_id: Some(agl_core::AgentRunId::generate()),
            workspace,
            failure: (state == SliceState::Failed).then(|| text("fake coder failed")),
        }
    }

    #[test]
    fn dependency_order_is_topological_and_scheduler_is_serialized() {
        let first = slice("first", vec![]);
        let second = slice("second", vec![first.id.clone()]);
        let third = slice("third", vec![second.id.clone()]);
        let plan = plan(vec![third.clone(), first.clone(), second.clone()], vec![]);
        let order = execution_order(&plan).unwrap();
        assert_eq!(
            order
                .iter()
                .map(|slice| slice.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );

        let lock = IMPLEMENTATION_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        assert!(IMPLEMENTATION_LOCK.get().unwrap().try_lock().is_err());
        drop(lock);
    }

    #[test]
    fn coder_input_has_fresh_scope_and_no_planner_history() {
        let first = slice("first", vec![]);
        let second = slice("second", vec![first.id.clone()]);
        let plan = plan(vec![first.clone(), second.clone()], vec![]);
        let predecessor = result(
            plan.id.clone(),
            PlanDigest::from_bytes([1; 32]),
            first.id,
            WorkspaceSnapshot {
                digest: PlanDigest::from_bytes([2; 32]),
                files: vec![],
            },
            SliceState::Completed,
        );
        let current_workspace = WorkspaceSnapshot {
            digest: PlanDigest::from_bytes([8; 32]),
            files: vec![],
        };
        let encoded = serde_json::to_string(
            &coder_input(&plan, &second, &[predecessor], &current_workspace).unwrap(),
        )
        .unwrap();
        assert!(encoded.contains("second"));
        assert!(encoded.contains("objective"));
        assert!(encoded.contains("current_workspace"));
        assert!(encoded.contains("predecessor_results"));
        assert!(!encoded.contains("planner_conversation"));
        assert!(!encoded.contains("private_reasoning"));
        assert_ne!(ConversationId::generate(), ConversationId::generate());
    }

    #[test]
    fn initial_evidence_mismatch_transitions_to_stale() {
        let root = std::env::temp_dir().join(format!(
            "agl-implementation-initial-{}",
            uuid::Uuid::now_v7()
        ));
        fs::create_dir_all(&root).unwrap();
        let plan = plan(
            vec![slice("first", vec![])],
            vec![WorkspacePath::new("missing.rs").unwrap()],
        );
        let git = GitEvidence {
            commit: None,
            dirty_paths: vec![WorkspacePath::new("other.rs").unwrap()],
        };
        assert!(ensure_initial_evidence_with_git(&plan, &root, &git).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn initial_evidence_requires_the_complete_dirty_set_and_exact_commit() {
        let root = std::env::temp_dir().join(format!(
            "agl-implementation-evidence-{}",
            uuid::Uuid::now_v7()
        ));
        fs::create_dir_all(&root).unwrap();
        let mut plan = plan(
            vec![slice("first", vec![])],
            vec![
                WorkspacePath::new("tracked.rs").unwrap(),
                WorkspacePath::new("untracked.txt").unwrap(),
            ],
        );
        plan.workspace.canonical_root_sha256 =
            PlanDigest::from_bytes(agl_core::implementation_plan::workspace_root_digest(&root));
        plan.workspace.git_commit = Some(GitCommit::new("a".repeat(40)).unwrap());
        let exact = GitEvidence {
            commit: Some(GitCommit::new("a".repeat(40)).unwrap()),
            dirty_paths: vec![
                WorkspacePath::new("untracked.txt").unwrap(),
                WorkspacePath::new("tracked.rs").unwrap(),
            ],
        };
        ensure_initial_evidence_with_git(&plan, &root, &exact).unwrap();

        let mut extra = exact.clone();
        extra.dirty_paths.push(WorkspacePath::new("extra").unwrap());
        assert!(ensure_initial_evidence_with_git(&plan, &root, &extra).is_err());

        let mismatched_commit = GitEvidence {
            commit: Some(GitCommit::new("b".repeat(40)).unwrap()),
            dirty_paths: exact.dirty_paths,
        };
        assert!(ensure_initial_evidence_with_git(&plan, &root, &mismatched_commit).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn durable_result_chain_rejects_a_mismatched_changed_path_record() {
        let before = WorkspaceSnapshot {
            digest: PlanDigest::from_bytes([1; 32]),
            files: vec![],
        };
        let after = WorkspaceSnapshot {
            digest: PlanDigest::from_bytes([2; 32]),
            files: vec![WorkspaceFileDigest {
                path: WorkspacePath::new("changed").unwrap(),
                digest: PlanDigest::from_bytes([3; 32]),
            }],
        };
        let mut result = result(
            PlanId::generate(),
            PlanDigest::from_bytes([4; 32]),
            id("first"),
            after,
            SliceState::Completed,
        );
        result.changed_paths = vec![];
        assert!(ensure_result_chain(&result, &before).is_err());
    }

    #[test]
    fn git_status_parser_preserves_the_complete_deterministic_path_set() {
        let paths =
            parse_git_dirty_paths(b" M tracked.rs\0?? untracked.txt\0R  moved.rs\0old.rs\0")
                .unwrap();
        assert_eq!(
            paths.iter().map(|path| path.as_str()).collect::<Vec<_>>(),
            ["moved.rs", "old.rs", "tracked.rs", "untracked.txt"]
        );
    }

    #[test]
    fn external_drift_is_exact_and_predecessor_snapshot_is_strict() {
        let root =
            std::env::temp_dir().join(format!("agl-implementation-drift-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a"), "a").unwrap();
        let before = workspace::capture(&root).unwrap();
        fs::write(root.join("internal"), "known").unwrap();
        let predecessor = workspace::capture(&root).unwrap();
        fs::write(root.join("external"), "unexpected").unwrap();
        let after = workspace::capture(&root).unwrap();
        let changed = after.changed_paths(&predecessor);
        assert_eq!(
            changed
                .iter()
                .map(|path| path.path.as_str())
                .collect::<Vec<_>>(),
            ["external"]
        );
        assert_ne!(after.snapshot, predecessor.snapshot);
        assert_ne!(before.snapshot, predecessor.snapshot);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exact_predecessor_change_is_accepted_but_mismatch_is_rejected() {
        let root = std::env::temp_dir().join(format!(
            "agl-implementation-predecessor-{}",
            uuid::Uuid::now_v7()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a"), "a").unwrap();
        let expected = workspace::capture(&root).unwrap();
        assert_eq!(
            workspace::capture(&root).unwrap().snapshot,
            expected.snapshot
        );
        fs::write(root.join("a"), "different").unwrap();
        assert_ne!(
            workspace::capture(&root).unwrap().snapshot,
            expected.snapshot
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn structured_result_persists_failure_and_restart_does_not_reclaim_committed_slice() {
        let root =
            std::env::temp_dir().join(format!("agl-implementation-store-{}", uuid::Uuid::now_v7()));
        let store = StoreHandle::open_at(root.join("store")).unwrap();
        let conversation = ConversationId::generate();
        {
            let database = store.lock().unwrap();
            database.transaction(|tx| {
                tx.execute("INSERT INTO agent_conversations (conversation_id, display_name, function_json, workspace_root, snapshot_json, created_at_ms, last_active_at_ms) VALUES (?1, NULL, '{}', '/workspace', '{}', 1, 1)", rusqlite::params![conversation.as_bytes().as_slice()])?;
                Ok(())
            }).unwrap();
        }
        let plan_id = PlanId::generate();
        let digest = PlanDigest::from_bytes([9; 32]);
        let initial = WorkspaceSnapshot {
            digest: PlanDigest::from_bytes([3; 32]),
            files: vec![],
        };
        store
            .register_plan_draft(
                plan_id.clone(),
                digest.clone(),
                Path::new("/workspace"),
                conversation,
                PlanState::ReadyForApproval,
            )
            .unwrap();
        store.approve_plan(plan_id.clone(), &digest).unwrap();
        store
            .begin_plan_implementation(
                plan_id.clone(),
                &digest,
                &["first".into(), "second".into()],
                &initial,
            )
            .unwrap();
        assert!(store.claim_slice(&plan_id, "first").unwrap());
        let completed = result(
            plan_id.clone(),
            digest.clone(),
            id("first"),
            initial.clone(),
            SliceState::Completed,
        );
        store.finish_slice(&plan_id, "first", &completed).unwrap();
        let records = store.slice_records(&plan_id).unwrap();
        assert_eq!(records[0].state, SliceState::Completed);
        assert_eq!(records[0].result.as_ref(), Some(&completed));
        assert!(!store.claim_slice(&plan_id, "first").unwrap());
        drop(store);
        let reopened = StoreHandle::open_at(root.join("store")).unwrap();
        assert!(!reopened.claim_slice(&plan_id, "first").unwrap());
        assert!(reopened.claim_slice(&plan_id, "second").unwrap());
        let second_slice = slice("second", vec![]);
        let malformed = result(
            plan_id.clone(),
            digest.clone(),
            second_slice.id.clone(),
            initial.clone(),
            SliceState::Pending,
        );
        let failed = finish_claimed_slice(&reopened, &plan_id, &second_slice, malformed).unwrap();
        assert_eq!(failed.state, SliceState::Failed);
        assert_eq!(
            reopened.plan_record(plan_id.clone()).unwrap().state,
            PlanState::Failed
        );
        assert_eq!(
            reopened.slice_records(&plan_id).unwrap()[1].result.as_ref(),
            Some(&failed)
        );
        fs::remove_dir_all(root).unwrap();
    }
}
