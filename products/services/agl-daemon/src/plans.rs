//! Daemon-owned plan artifacts. Database state is intentionally out of scope here;
//! this module owns only the canonical JSON handoff files.

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use agl_core::agent::{AgentRunOrigin, AgentRunSpec, AgentRunStatus, MessageRole};
use agl_core::implementation_plan::{
    ImplementationPlan, PlanDigest, PlanId, PlanState, SliceResult,
};
use agl_core::{Content, ConversationId, MessageId};
use agl_daemon_api::{PlanArtifactView, SliceStatusView};
use agl_runtime::PlannerWorkspaceBoundary;
use anyhow::{Context, Result, bail, ensure};
use uuid::Uuid;

const PLANS_DIRECTORY: &str = "plans";
const MAX_CORRECTION_INPUT_BYTES: usize = agl_core::MAX_TEXT_BYTES;

#[derive(Clone, Debug)]
pub struct PlanArtifactStore {
    root: PathBuf,
}

impl PlanArtifactStore {
    /// Create a store below the daemon's application data root.
    pub fn new(data_root: impl AsRef<Path>) -> Result<Self> {
        let root = data_root.as_ref().join(PLANS_DIRECTORY);
        let drafts = root.join("drafts");
        let approved = root.join("approved");
        crate::store::path::ensure_private_dir(&drafts)?;
        crate::store::path::ensure_private_dir(&approved)?;
        Ok(Self { root })
    }

    pub fn draft_path(&self, id: &PlanId) -> PathBuf {
        self.root.join("drafts").join(format!("{id}.json"))
    }

    pub fn approved_path(&self, digest: &PlanDigest) -> PathBuf {
        self.root
            .join("approved")
            .join(format!("{}.json", digest.hex()))
    }

    /// Replace the complete mutable draft after validating and canonicalizing it.
    pub fn publish_draft(&self, plan: &ImplementationPlan) -> Result<PlanDigest> {
        let bytes = plan
            .canonical_bytes()
            .context("failed to canonicalize implementation plan")?;
        let digest = plan
            .digest()
            .context("failed to digest implementation plan")?;
        let path = self.draft_path(&plan.id);
        atomic_replace(&path, &bytes)?;
        Ok(digest)
    }

    /// Create a new durable Conversation, run the planner through the normal
    /// Agent scheduler (including its admitted Tool handlers), and publish the
    /// final plan only after the complete document passes validation.
    pub fn create_from_conversation(
        &self,
        agent: &crate::agent::AgentHandle,
        store: &crate::store::StoreHandle,
        function: &agl_core::agent::ExactPackageRef,
        activated: &agl_runtime::ActivatedFunction,
        workspace: &Path,
        prompt: Content,
    ) -> Result<(StoredPlan, agl_core::agent::ConversationView)> {
        let conversation_id = ConversationId::generate();
        let mut snapshot = activated.snapshot.clone();
        snapshot.instructions = agl_runtime::planner::instructions(&snapshot.instructions)
            .context("failed to build planner instructions")?;
        snapshot.response_format = Some(agl_runtime::planner::response_format());
        snapshot.planner_read_only = true;
        snapshot.invalid_model_output_recovery = None;
        let boundary = PlannerWorkspaceBoundary {
            workspace_root: workspace.to_owned(),
            scratch_root: self
                .root
                .parent()
                .context("plan artifact root has no daemon data parent")?
                .join("planner-scratch"),
        };
        boundary
            .validate_capabilities(&snapshot.tools)
            .context("planner Function is not read-only")?;
        ensure!(
            snapshot.authority.0.iter().all(|grant| !matches!(
                grant.effect.as_str(),
                "agentlibre.builtins:filesystem_write" | "agentlibre.execution:terminal.control"
            )),
            "planner Function carries write or terminal authority"
        );
        ensure!(
            snapshot.workspace.root.as_path() == workspace,
            "planner Conversation workspace differs from the requested workspace"
        );
        let mut conversation = store.create_conversation(conversation_id, function, &snapshot)?;
        let first = AgentRunSpec {
            origin: AgentRunOrigin::User {
                conversation_id,
                message_id: MessageId::generate(),
            },
            input: prompt.clone(),
            reasoning: None,
        };
        let first_run = agent.start_run(first)?;
        wait_for_run(store, first_run)?;
        let first_output = latest_assistant(store, conversation_id)?;
        let first_error = match decode_plan(&first_output) {
            Ok(mut plan) => {
                bind_workspace_identity(&mut plan, workspace)?;
                ensure!(
                    plan.workspace_matches(workspace)?,
                    "planner output workspace identity does not match the Conversation workspace"
                );
                return Ok((
                    self.publish_draft_durable(store, &plan, workspace, conversation_id)?,
                    conversation,
                ));
            }
            Err(error) => error,
        };
        let correction = correction_input(prompt.as_text(), &first_error)?;
        let correction_conversation_id = ConversationId::generate();
        let mut correction_snapshot = snapshot.clone();
        correction_snapshot.tools.clear();
        correction_snapshot.authority = Default::default();
        conversation = store.create_conversation(
            correction_conversation_id,
            function,
            &correction_snapshot,
        )?;
        let second = AgentRunSpec {
            origin: AgentRunOrigin::User {
                conversation_id: correction_conversation_id,
                message_id: MessageId::generate(),
            },
            input: correction,
            reasoning: None,
        };
        let second_run = agent.start_run(second)?;
        wait_for_run(store, second_run)?;
        let second_output = latest_assistant(store, correction_conversation_id)?;
        let mut plan = decode_plan(&second_output).map_err(|error| {
            anyhow::anyhow!(
                "planner output remained invalid after exactly one correction: first={first_error}; second={error}"
            )
        })?;
        bind_workspace_identity(&mut plan, workspace)?;
        ensure!(
            plan.workspace_matches(workspace)?,
            "planner output workspace identity does not match the Conversation workspace"
        );
        Ok((
            self.publish_draft_durable(store, &plan, workspace, correction_conversation_id)?,
            conversation,
        ))
    }

    fn publish_draft_durable(
        &self,
        store: &crate::store::StoreHandle,
        plan: &ImplementationPlan,
        workspace: &Path,
        conversation_id: ConversationId,
    ) -> Result<StoredPlan> {
        let digest = self.publish_draft(plan)?;
        let state = PlanState::for_draft(plan);
        store
            .register_plan_draft(plan.id.clone(), digest, workspace, conversation_id, state)
            .context("failed to persist implementation plan draft state")?;
        self.read_draft(&plan.id)
    }

    pub fn view(&self, store: &crate::store::StoreHandle, id: &PlanId) -> Result<PlanArtifactView> {
        let stored = self.read_draft(id)?;
        let record = store
            .plan_record(id.clone())
            .context("failed to read implementation plan state")?;
        ensure!(
            record.draft_digest == stored.digest,
            "durable plan state does not match current draft digest"
        );
        let slices = self.slice_statuses(store, id, &stored.plan)?;
        Ok(PlanArtifactView {
            plan: stored.plan,
            digest: stored.digest,
            state: record.state,
            results: results_from_statuses(&slices),
            slices,
        })
    }

    /// Perform the human approval boundary. The immutable artifact is created
    /// and verified before the durable state transition is committed.
    pub fn approve_draft_durable(
        &self,
        store: &crate::store::StoreHandle,
        id: &PlanId,
        expected: &PlanDigest,
    ) -> Result<PlanArtifactView> {
        self.approve_draft_durable_inner(store, id, expected, || Ok(()))
    }

    fn approve_draft_durable_inner<F>(
        &self,
        store: &crate::store::StoreHandle,
        id: &PlanId,
        expected: &PlanDigest,
        after_artifact: F,
    ) -> Result<PlanArtifactView>
    where
        F: FnOnce() -> Result<()>,
    {
        let record = store
            .plan_record(id.clone())
            .context("failed to read implementation plan state")?;
        ensure!(
            record.state == PlanState::ReadyForApproval
                || (record.state == PlanState::Approved
                    && record.approved_digest.as_ref() == Some(expected)),
            "plan is not ready for explicit approval"
        );
        let draft = self.read_draft(id)?;
        ensure!(
            &draft.digest == expected && record.draft_digest == draft.digest,
            "draft digest is stale; inspect the current plan before approving"
        );
        let approved = self.approve_draft(id, expected)?;
        after_artifact().context("approval failed after immutable artifact publication")?;
        let record = store
            .approve_plan(id.clone(), expected)
            .context("failed to persist implementation plan approval")?;
        ensure!(
            record.state == PlanState::Approved
                && record.approved_digest.as_ref() == Some(expected),
            "approval state transition did not commit"
        );
        let slices = self.slice_statuses(store, id, &approved.plan)?;
        Ok(PlanArtifactView {
            plan: approved.plan,
            digest: approved.digest,
            state: record.state,
            results: results_from_statuses(&slices),
            slices,
        })
    }

    pub fn results(
        &self,
        store: &crate::store::StoreHandle,
        id: &PlanId,
    ) -> Result<Vec<SliceResult>> {
        let mut results = store
            .slice_records(id)?
            .into_iter()
            .filter_map(|record| record.result)
            .collect::<Vec<_>>();
        results.sort_by(|left, right| left.slice_id.as_str().cmp(right.slice_id.as_str()));
        Ok(results)
    }

    pub fn slice_statuses(
        &self,
        store: &crate::store::StoreHandle,
        id: &PlanId,
        plan: &ImplementationPlan,
    ) -> Result<Vec<SliceStatusView>> {
        let records = store
            .slice_records(id)?
            .into_iter()
            .map(|record| (record.slice_id.clone(), record))
            .collect::<std::collections::BTreeMap<_, _>>();
        plan.slices
            .iter()
            .map(|slice| {
                let record = records.get(slice.id.as_str());
                let (state, result, mut stale_paths) = record
                    .map(|record| {
                        (
                            record.state,
                            record.result.clone(),
                            record.stale_paths.clone(),
                        )
                    })
                    .unwrap_or((
                        agl_core::implementation_plan::SliceState::Pending,
                        None,
                        Vec::new(),
                    ));
                stale_paths.sort();
                stale_paths.dedup();
                Ok(SliceStatusView {
                    slice_id: slice.id.as_str().to_owned(),
                    state,
                    result,
                    stale_paths,
                })
            })
            .collect()
    }

    #[cfg(test)]
    fn approve_draft_with_injected_commit_failure(
        &self,
        store: &crate::store::StoreHandle,
        id: &PlanId,
        expected: &PlanDigest,
    ) -> Result<PlanArtifactView> {
        self.approve_draft_durable_inner(store, id, expected, || {
            bail!("injected SQLite commit failure")
        })
    }

    pub fn read_draft(&self, id: &PlanId) -> Result<StoredPlan> {
        let stored = self.read_at(&self.draft_path(id), None)?;
        ensure!(
            &stored.plan.id == id,
            "draft plan ID does not match artifact name"
        );
        Ok(stored)
    }

    /// Create the immutable approved object from the current draft bytes.
    /// Existing bytes are accepted only when they are exactly the same digest.
    pub fn approve_draft(&self, id: &PlanId, expected: &PlanDigest) -> Result<StoredPlan> {
        let draft = self.read_draft(id)?;
        draft
            .plan
            .validate_for_implementation()
            .context("draft is not implementation-ready")?;
        ensure!(
            &draft.digest == expected,
            "draft digest does not match approval target"
        );
        let path = self.approved_path(expected);
        if path.try_exists()? {
            let existing = self.read_at(&path, Some(expected))?;
            ensure!(
                existing.bytes == draft.bytes,
                "approved artifact is immutable and differs from draft"
            );
            return Ok(existing);
        }
        atomic_create(&path, &draft.bytes)?;
        self.read_at(&path, Some(expected))
    }

    pub fn read_approved(&self, digest: &PlanDigest) -> Result<StoredPlan> {
        let stored = self.read_at(&self.approved_path(digest), Some(digest))?;
        stored
            .plan
            .validate_for_implementation()
            .context("approved artifact is not implementation-ready")?;
        Ok(stored)
    }

    fn read_at(&self, path: &Path, expected: Option<&PlanDigest>) -> Result<StoredPlan> {
        crate::store::path::validate_private_regular_file(path)
            .with_context(|| format!("invalid plan artifact {}", path.display()))?;
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read plan artifact {}", path.display()))?;
        let plan = ImplementationPlan::from_canonical_bytes(&bytes).map_err(|error| {
            anyhow::anyhow!("invalid plan artifact {}: {error}", path.display())
        })?;
        let digest = plan
            .digest()
            .map_err(|error| anyhow::anyhow!("failed to digest plan artifact: {error}"))?;
        if let Some(expected) = expected {
            ensure!(&digest == expected, "plan artifact digest mismatch");
        }
        Ok(StoredPlan {
            plan,
            digest,
            bytes,
        })
    }
}

fn results_from_statuses(statuses: &[SliceStatusView]) -> Vec<SliceResult> {
    statuses
        .iter()
        .filter_map(|status| status.result.clone())
        .collect()
}

fn wait_for_run(store: &crate::store::StoreHandle, run_id: agl_core::AgentRunId) -> Result<()> {
    loop {
        let view = store.agent_run_view(run_id)?;
        if view.status.is_terminal() {
            ensure!(
                view.status == AgentRunStatus::Completed,
                "planner AgentRun failed: {:?}",
                view.failure
            );
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn latest_assistant(
    store: &crate::store::StoreHandle,
    conversation_id: ConversationId,
) -> Result<String> {
    let page = store.conversation_messages(conversation_id, None, 1_000)?;
    page.messages
        .into_iter()
        .rev()
        .find(|message| message.role == MessageRole::Assistant)
        .map(|message| message.content.into_text())
        .context("planner produced no assistant output")
}

fn decode_plan(output: &str) -> Result<ImplementationPlan, String> {
    let plan: ImplementationPlan = serde_json::from_str(output)
        .map_err(|error| format!("implementation plan JSON is not decodable: {error}"))?;
    plan.validate().map_err(|error| error.to_string())?;
    Ok(plan)
}

fn bind_workspace_identity(plan: &mut ImplementationPlan, workspace: &Path) -> Result<()> {
    let canonical = workspace
        .canonicalize()
        .context("failed to canonicalize planner workspace")?;
    plan.workspace.canonical_root_sha256 = PlanDigest::from_bytes(
        agl_core::implementation_plan::workspace_root_digest(&canonical),
    );
    Ok(())
}

fn correction_input(original_prompt: &str, diagnostic: &str) -> Result<Content> {
    let prefix = format!(
        "{} Do not reuse the previous object's shape. The top-level schema field must be exactly \"agentlibre.implementation-plan/v1\". The top-level id must be a lowercase UUIDv7 with form xxxxxxxx-xxxx-7xxx-[89ab]xxx-xxxxxxxxxxxx. {}\nRequired JSON Schema:\n",
        agl_runtime::planner::PLANNER_CORRECTION_PREFIX,
        agl_runtime::planner::PLANNER_DOMAIN_RULES
    );
    let schema = serde_json::to_string(&agl_runtime::planner::implementation_plan_schema())?;
    let task_prefix = "\nOriginal planning task:\n";
    let diagnostic_prefix = "\nValidation diagnostics:\n";
    let fixed_bytes = prefix.len() + schema.len() + task_prefix.len() + diagnostic_prefix.len();
    let mut original_prompt = original_prompt.to_owned();
    while fixed_bytes + original_prompt.len() > MAX_CORRECTION_INPUT_BYTES {
        original_prompt.pop();
    }
    let available = MAX_CORRECTION_INPUT_BYTES.saturating_sub(fixed_bytes + original_prompt.len());
    let mut diagnostic = diagnostic.to_owned();
    while diagnostic.len() > available {
        diagnostic.pop();
    }
    Content::text(format!(
        "{prefix}{schema}{task_prefix}{original_prompt}{diagnostic_prefix}{diagnostic}"
    ))
    .context("planner correction prompt is invalid")
}

#[derive(Clone, Debug)]
pub struct StoredPlan {
    pub plan: ImplementationPlan,
    pub digest: PlanDigest,
    pub bytes: Vec<u8>,
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let staged = stage_path(path);
    write_staged(&staged, bytes)?;
    if let Err(error) = fs::rename(&staged, path) {
        let _ = fs::remove_file(&staged);
        return Err(error)
            .with_context(|| format!("failed to replace plan artifact {}", path.display()));
    }
    sync_parent(path)
}

fn atomic_create(path: &Path, bytes: &[u8]) -> Result<()> {
    let staged = stage_path(path);
    write_staged(&staged, bytes)?;
    match rename_noreplace(&staged, path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&staged);
            bail!("approved artifact already exists")
        }
        Err(error) => {
            let _ = fs::remove_file(&staged);
            Err(error)
                .with_context(|| format!("failed to create approved artifact {}", path.display()))
        }
    }
}

fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: both C strings remain live and RENAME_NOREPLACE atomically publishes
    // the complete staged file only when the destination name is absent.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn stage_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        Uuid::now_v7()
    ))
}

fn write_staged(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options
            .open(path)
            .with_context(|| format!("failed to stage plan artifact {}", path.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        crate::store::path::set_private_file_permissions(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path.parent().context("plan artifact has no parent")?;
    fs::File::open(parent)?
        .sync_all()
        .context("failed to sync artifact directory")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use agl_core::ExtensionDefinition;
    use agl_core::agent::{
        AbsolutePath, AdmittedTool, AgentDefinitionRef, AgentRunLimits, AgentRunSnapshot,
        AuthorityGrantSet, ExactPackageRef, ExtensionDefinitionRef, InferenceEngineBuildDigest,
        InferenceRealizationRef, InferenceRuntimeProfileDigest, InstructionSet, ModelDefinitionRef,
        ModelFinishReason, ModelGenerationOutput, ModelGenerationResult, ModelRuntimeSelection,
        ModelSelection, ModelUsage, PackageDigest, PhysicalResourceDigest, RelativePath,
        WorkspaceScope,
    };
    use agl_core::implementation_plan::*;
    use agl_core::{Content, ToolId};
    use agl_runtime::extension::{
        ExtensionBindings, ToolBinding, ToolContext, ToolFuture, ToolHandler,
    };
    use agl_runtime::inference::{
        InferenceConfig, InferenceGenerateRequest, InferenceGenerator, InferenceService,
        InferenceServiceError, RestoredInferenceHealth,
    };
    use agl_runtime::package::{PackageTreeDigest, PackageVersion};
    use agl_runtime::{RuntimeConfig, RuntimeService};
    use serde_json::{Value, json};

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!("agl-plan-store-{}", Uuid::now_v7()))
    }

    struct InspectTool {
        calls: Arc<Mutex<Vec<(Value, bool)>>>,
    }

    impl ToolHandler for InspectTool {
        fn call(&self, context: ToolContext, input: Value) -> ToolFuture {
            self.calls
                .lock()
                .unwrap()
                .push((input, context.read_only_workspace));
            Box::pin(async {
                Ok(agl_core::agent::ToolResult {
                    content: Content::text("repository evidence").unwrap(),
                    effect_receipts: vec![],
                })
            })
        }
    }

    #[derive(Clone, Copy)]
    enum GenerationMode {
        FirstPass,
        Correction,
        AlwaysInvalid,
        FreshConversation,
        ApprovalTool,
    }

    struct PlannerGenerator {
        calls: Arc<AtomicUsize>,
        mode: GenerationMode,
        valid: String,
    }

    impl InferenceGenerator for PlannerGenerator {
        fn measure(
            &self,
            request: InferenceGenerateRequest,
        ) -> Result<agl_core::agent::ContextCapacity, InferenceServiceError> {
            Ok(agl_core::agent::ContextCapacity::new(
                0,
                request.generation.max_output_tokens,
                request.runtime.load.context_tokens,
            ))
        }

        fn generate(
            &self,
            request: InferenceGenerateRequest,
        ) -> Result<agl_runtime::inference::InferenceGenerateResult, InferenceServiceError>
        {
            if request.response_format.is_some() {
                assert!(request.tools.is_empty());
                assert!(request.context.iter().any(|entry| {
                    entry
                        .message
                        .content
                        .as_text()
                        .starts_with(agl_runtime::planner::PLANNER_CORRECTION_PREFIX)
                }));
            } else {
                assert_eq!(request.tools.len(), 1);
            }
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let tool = match self.mode {
                GenerationMode::FirstPass
                | GenerationMode::Correction
                | GenerationMode::AlwaysInvalid => call == 0,
                GenerationMode::FreshConversation | GenerationMode::ApprovalTool => {
                    call.is_multiple_of(2)
                }
            };
            let invalid = match self.mode {
                GenerationMode::FirstPass => false,
                GenerationMode::Correction => call == 1,
                GenerationMode::AlwaysInvalid => call >= 1,
                GenerationMode::FreshConversation | GenerationMode::ApprovalTool => false,
            };
            let output = if tool {
                ModelGenerationOutput::ToolCall(agl_core::agent::ToolCall {
                    tool_id: ToolId::new(if matches!(self.mode, GenerationMode::ApprovalTool) {
                        "agentlibre.plan:approve"
                    } else {
                        "test.planner:inspect"
                    })
                    .unwrap(),
                    input: json!({"path":"src/lib.rs"}),
                })
            } else if invalid {
                ModelGenerationOutput::Assistant(Content::text("not canonical plan JSON").unwrap())
            } else {
                ModelGenerationOutput::Assistant(Content::text(self.valid.clone()).unwrap())
            };
            Ok(agl_runtime::inference::InferenceGenerateResult {
                operation: request.operation,
                delivery_attempt: request.delivery_attempt,
                result: ModelGenerationResult {
                    private_reasoning: None,
                    output,
                    finish_reason: if tool {
                        ModelFinishReason::ToolCall
                    } else {
                        ModelFinishReason::Stop
                    },
                    usage: ModelUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    realization: InferenceRealizationRef {
                        runtime_profile_digest: InferenceRuntimeProfileDigest::from_bytes([1; 32]),
                        engine_build_digest: InferenceEngineBuildDigest::from_bytes([2; 32]),
                        physical_resource_digest: PhysicalResourceDigest::from_bytes([3; 32]),
                    },
                    correction: None,
                },
            })
        }
    }

    fn inspect_extension(
        calls: Arc<Mutex<Vec<(Value, bool)>>>,
    ) -> (ExtensionBindings, AdmittedTool) {
        let tool = agl_core::ToolDefinition {
            id: ToolId::new("test.planner:inspect").unwrap(),
            description: "inspect a repository file".into(),
            input_schema: agl_core::JsonSchema::new(json!({
                "type":"object", "additionalProperties":false,
                "required":["path"], "properties":{"path":{"type":"string"}}
            }))
            .unwrap(),
            required_effects: vec![],
            delivery: agl_core::agent::DeliveryClass::AtMostOnce,
        };
        let extension = ExtensionDefinition {
            id: agl_core::ExtensionId::new("test.planner").unwrap(),
            effects: vec![],
            tools: vec![tool.clone()],
        };
        let tool_digest = agl_core::agent::ToolDefinitionDigest::from_bytes(crate::agent::sha256(
            &serde_json::to_vec(&tool).unwrap(),
        ));
        let extension_digest = agl_core::agent::ExtensionDefinitionDigest::from_bytes(
            crate::agent::sha256(&serde_json::to_vec(&extension).unwrap()),
        );
        let bindings = ExtensionBindings {
            version: PackageVersion::new("1.0.0").unwrap(),
            content_digest: PackageTreeDigest::new(format!("sha256:{}", "01".repeat(32))).unwrap(),
            definition: extension.clone(),
            tools: vec![ToolBinding {
                tool_id: tool.id.clone(),
                definition_digest: tool_digest,
                handler: Arc::new(InspectTool { calls }),
            }],
            allows_authority: Arc::new(|_| true),
        };
        let admitted = AdmittedTool {
            definition: tool,
            extension: ExtensionDefinitionRef {
                id: extension.id,
                package: ExactPackageRef {
                    id: agl_runtime::package::PackageId::new("test.planner").unwrap(),
                    version: PackageVersion::new("1.0.0").unwrap(),
                    digest: PackageDigest::from_bytes([4; 32]),
                },
                definition_digest: extension_digest,
            },
            definition_digest: tool_digest,
        };
        (bindings, admitted)
    }

    fn model_runtime() -> ModelRuntimeSelection {
        use agl_core::agent::{
            GenerationSettings, GpuLayerSelection, KvCacheType, ModelArtifactKind,
            ModelArtifactRef, ModelLoadSelection, ModelServiceSelection, SplitMode,
        };
        ModelRuntimeSelection {
            artifact: ModelArtifactRef {
                kind: ModelArtifactKind::Gguf,
                url: "https://example.invalid/model.gguf".into(),
                digest: PackageDigest::from_bytes([5; 32]),
                bytes: 4,
            },
            dialect: agl_core::agent::ModelDialect::Generic,
            tool_call_format: agl_core::agent::ToolCallFormat::HermesJson,
            generation: GenerationSettings {
                max_output_tokens: 512,
                seed: 1,
                temperature: 0.0,
                top_k: 1,
                top_p: 1.0,
                min_p: 0.0,
                typical_p: 1.0,
                repeat_last_n: 64,
                repeat_penalty: 1.0,
                presence_penalty: 0.0,
                frequency_penalty: 0.0,
                stop: vec![],
            },
            reasoning: agl_core::agent::ReasoningSelection::Disabled,
            speculative: None,
            load: ModelLoadSelection {
                context_tokens: 8_192,
                batch_size: 8,
                ubatch_size: 8,
                threads: 1,
                threads_batch: 1,
                gpu_layers: GpuLayerSelection::Count(0),
                devices: vec![],
                split_mode: SplitMode::None,
                main_gpu: None,
                tensor_split: vec![],
                mmap: true,
                mlock: false,
                flash_attention: false,
                kv_cache_type_k: KvCacheType::F16,
                kv_cache_type_v: KvCacheType::F16,
                engine_build_digest: PackageDigest::from_bytes([6; 32]),
            },
            adapters: vec![],
            service: ModelServiceSelection {
                key: PackageDigest::from_bytes([7; 32]),
                slots: 1,
                queue_capacity: 16,
                continuous_batching: true,
                idle_timeout_ms: 900_000,
            },
        }
    }

    fn valid_output(workspace: &Path) -> String {
        let source = json!({
            "kind":"file", "locator":"reports/spec.md", "digest":null,
            "line_start":null, "line_end":null
        });
        let value = json!({
            "schema":"agentlibre.implementation-plan/v1",
            "id":PlanId::generate().to_string(),
            "workspace":{
                "canonical_root_sha256":format!("sha256:{}", agl_core::implementation_plan::workspace_root_digest(workspace).iter().map(|byte| format!("{byte:02x}")).collect::<String>()),
                "git_commit":null, "dirty_paths":[]
            },
            "objective":{
                "outcome":"implement the requested change",
                "acceptance":[{"id":"acceptance","statement":"the change works","sources":[source]}],
                "sources":[source]
            },
            "evidence":[{"id":"evidence","statement":"the repository establishes the current behavior","sources":[source]}],
            "decisions":[], "invariants":[],
            "slices":[{
                "id":"slice", "outcome":"ship the change", "depends_on":[],
                "files":[{"path":"src/lib.rs","disposition":"modify","purpose":"implement the change","symbols":[]}],
                "required_reads":[], "conditional_reads":[], "start_condition":["evidence"],
                "steps":[{"id":"step","action":"make the change","files":["src/lib.rs"],"symbols":[],"depends_on":[],"satisfies":["acceptance"]}],
                "verification":[{"command":"cargo test","working_directory":".","expected":{"exit_code":0,"stdout_contains":[],"stderr_contains":[]},"covers":["acceptance"]}],
                "done_when":[{"id":"done","statement":"the change is complete","sources":[source]}]
            }],
            "open_decisions":[]
        });
        let plan: ImplementationPlan = serde_json::from_value(value).unwrap();
        String::from_utf8(plan.canonical_bytes().unwrap()).unwrap()
    }

    struct Harness {
        root: PathBuf,
        workspace: PathBuf,
        plans: PlanArtifactStore,
        store: Option<crate::store::StoreHandle>,
        service: Option<crate::agent::AgentService>,
        inference: Option<InferenceService>,
        runtime_service: Option<RuntimeService>,
        runtime: agl_runtime::RuntimeHandle,
        agent: crate::agent::AgentHandle,
        function: ExactPackageRef,
        activated: agl_runtime::ActivatedFunction,
        inspect_calls: Arc<Mutex<Vec<(Value, bool)>>>,
        model_calls: Arc<AtomicUsize>,
    }

    impl Harness {
        fn new(mode: GenerationMode) -> Self {
            let root = root();
            let workspace = root.join("workspace");
            std::fs::create_dir_all(workspace.join("src")).unwrap();
            std::fs::write(workspace.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
            let workspace = workspace.canonicalize().unwrap();
            let plans = PlanArtifactStore::new(root.join("data")).unwrap();
            let store = crate::store::StoreHandle::open_at(root.join("store")).unwrap();
            let inspect_calls = Arc::new(Mutex::new(Vec::new()));
            let (bindings, admitted) = inspect_extension(inspect_calls.clone());
            let valid = valid_output(&workspace);
            let model_calls = Arc::new(AtomicUsize::new(0));
            let generator = Arc::new(PlannerGenerator {
                calls: model_calls.clone(),
                mode,
                valid,
            });
            let (inference, inference_handle) = InferenceService::start(
                InferenceConfig::custom(generator),
                RestoredInferenceHealth::default(),
            )
            .unwrap();
            let (service, agent) = crate::agent::AgentService::start(
                crate::agent::AgentDependencies {
                    store: store.clone(),
                    inference: Arc::new(inference_handle),
                },
                vec![bindings],
            )
            .unwrap();
            let snapshot = AgentRunSnapshot {
                agent: AgentDefinitionRef {
                    id: agl_runtime::package::PackageId::new("test.planner-agent").unwrap(),
                    version: PackageVersion::new("1.0.0").unwrap(),
                    digest: PackageDigest::from_bytes([8; 32]),
                },
                model: ModelSelection {
                    model: ModelDefinitionRef {
                        id: agl_runtime::package::PackageId::new("test.planner-model").unwrap(),
                        version: PackageVersion::new("1.0.0").unwrap(),
                        digest: PackageDigest::from_bytes([9; 32]),
                    },
                    runtime: model_runtime(),
                    reasoning_efforts: vec![],
                },
                invalid_model_output_recovery: None,
                instructions: InstructionSet::new(vec![]).unwrap(),
                workspace: WorkspaceScope {
                    root: AbsolutePath::try_from(workspace.to_string_lossy().into_owned()).unwrap(),
                    working_directory: RelativePath::try_from(".".to_owned()).unwrap(),
                },
                tools: vec![admitted],
                authority: AuthorityGrantSet::default(),
                limits: AgentRunLimits {
                    deadline_ms: 60_000,
                    model_input_tokens: None,
                    model_output_tokens: 512,
                    model_calls: 8,
                    correction_input_tokens: 4_096,
                    correction_output_tokens: 512,
                    correction_calls: 1,
                    tool_calls: 8,
                    tool_result_bytes: 4_096,
                },
                presentation: Default::default(),
                response_format: None,
                planner_read_only: false,
            };
            let function = ExactPackageRef {
                id: agl_runtime::package::PackageId::new("test.planner").unwrap(),
                version: PackageVersion::new("1.0.0").unwrap(),
                digest: PackageDigest::from_bytes([10; 32]),
            };
            let activated = agl_runtime::ActivatedFunction {
                function_id: function.id.clone(),
                function_version: function.version.clone(),
                function_digest: function.digest,
                snapshot,
                service: agl_runtime::ModelServiceDisposition::New,
                progress: agl_runtime::ActivationProgress {
                    function: function.id.to_string(),
                    dependencies: "test".into(),
                    model_artifact: "test".into(),
                    model_service: "test".into(),
                    runtime_profile: PackageDigest::from_bytes([11; 32]),
                    active_slots: 1,
                    continuous_batching: true,
                },
            };
            let (runtime_service, runtime) = RuntimeService::start(
                RuntimeConfig {
                    data_root: root.join("runtime"),
                    inference: InferenceConfig::custom(Arc::new(PlannerGenerator {
                        calls: Arc::new(AtomicUsize::new(0)),
                        mode: GenerationMode::AlwaysInvalid,
                        valid: valid_output(&workspace),
                    })),
                    extensions: vec![],
                    max_resident_bytes: Some(1),
                    health: agl_runtime::inference::InferenceHealthSink::new(|_| Ok(())),
                },
                RestoredInferenceHealth::default(),
            )
            .unwrap();
            Self {
                root,
                workspace,
                plans,
                store: Some(store),
                service: Some(service),
                inference: Some(inference),
                runtime_service: Some(runtime_service),
                runtime,
                agent,
                function,
                activated,
                inspect_calls,
                model_calls,
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            if let Some(service) = self.service.take() {
                service.shutdown();
            }
            if let Some(inference) = self.inference.take() {
                inference.shutdown();
            }
            if let Some(runtime_service) = self.runtime_service.take() {
                runtime_service.shutdown();
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn create(
        harness: &Harness,
    ) -> anyhow::Result<(StoredPlan, agl_core::agent::ConversationView)> {
        harness.plans.create_from_conversation(
            &harness.agent,
            harness.store.as_ref().unwrap(),
            &harness.function,
            &harness.activated,
            &harness.workspace,
            Content::text("plan this repository change")?,
        )
    }

    #[test]
    fn planner_uses_real_agent_tool_path_and_read_only_context() {
        let harness = Harness::new(GenerationMode::FirstPass);
        let (stored, _) = create(&harness).unwrap();
        assert_eq!(stored.plan.schema, PlanSchema::V1);
        let calls = harness.inspect_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0["path"], "src/lib.rs");
        assert!(calls[0].1);
    }

    #[test]
    fn planner_has_no_approval_capability_and_unadmitted_approval_call_cannot_reach_a_handler() {
        let harness = Harness::new(GenerationMode::ApprovalTool);
        assert!(
            harness.activated.snapshot.tools.iter().all(|tool| !tool
                .definition
                .id
                .as_str()
                .contains("approve"))
        );
        assert!(create(&harness).is_ok());
        assert!(harness.inspect_calls.lock().unwrap().is_empty());
        assert!(
            !harness
                .plans
                .root
                .join("approved")
                .read_dir()
                .unwrap()
                .next()
                .is_some()
        );
    }

    #[test]
    fn planner_correction_succeeds_after_one_invalid_output() {
        let harness = Harness::new(GenerationMode::Correction);
        let (stored, conversation) = create(&harness).unwrap();
        assert_eq!(stored.plan.schema, PlanSchema::V1);
        let messages = harness
            .store
            .as_ref()
            .unwrap()
            .conversation_messages(conversation.id, None, 1_000)
            .unwrap();
        assert!(messages.messages.iter().any(|message| {
            message.role == MessageRole::User
                && message.content.as_text().contains("Validation diagnostics")
                && message.content.as_text().contains("Required JSON Schema")
                && message.content.as_text().contains("Original planning task")
                && message.content.as_text().contains("plan this")
                && !message
                    .content
                    .as_text()
                    .contains("not canonical plan JSON")
        }));
    }

    #[test]
    fn planner_double_invalid_output_preserves_existing_draft() {
        let harness = Harness::new(GenerationMode::AlwaysInvalid);
        let existing = plan();
        let digest = harness.plans.publish_draft(&existing).unwrap();
        let bytes = std::fs::read(harness.plans.draft_path(&existing.id)).unwrap();
        let error = create(&harness).unwrap_err();
        assert!(error.to_string().contains("exactly one correction"));
        assert_eq!(
            harness.plans.read_draft(&existing.id).unwrap().digest,
            digest
        );
        assert_eq!(
            std::fs::read(harness.plans.draft_path(&existing.id)).unwrap(),
            bytes
        );
    }

    #[test]
    fn every_planning_request_gets_a_unique_durable_conversation() {
        let harness = Harness::new(GenerationMode::FreshConversation);
        let (_, first) = create(&harness).unwrap();
        let (_, second) = create(&harness).unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(
            harness
                .store
                .as_ref()
                .unwrap()
                .conversations(None, 10)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn reopened_store_recovers_running_slice_without_repeating_coder() {
        let mut harness = Harness::new(GenerationMode::FirstPass);
        let (draft, _) = create(&harness).unwrap();
        let approved = harness
            .plans
            .approve_draft_durable(
                harness.store.as_ref().unwrap(),
                &draft.plan.id,
                &draft.digest,
            )
            .unwrap();
        let initial = crate::workspace::capture(&harness.workspace)
            .unwrap()
            .snapshot;
        harness
            .store
            .as_ref()
            .unwrap()
            .begin_plan_implementation(
                draft.plan.id.clone(),
                &draft.digest,
                &["slice".into()],
                &initial,
            )
            .unwrap();
        assert!(
            harness
                .store
                .as_ref()
                .unwrap()
                .claim_slice(&draft.plan.id, "slice")
                .unwrap()
        );
        let calls_before_restart = harness.model_calls.load(Ordering::SeqCst);
        if let Some(service) = harness.service.take() {
            service.shutdown();
        }
        drop(harness.store.take());
        let reopened = crate::store::StoreHandle::open_at(harness.root.join("store")).unwrap();

        let view = harness
            .plans
            .implement(
                &reopened,
                &harness.agent,
                &harness.runtime,
                &agl_execution_api::ExecutionClient::new(harness.root.join("unused-execd")),
                &draft.plan.id,
                &approved.digest,
                Path::new("/unused-coder-function"),
            )
            .unwrap();

        assert_eq!(view.state, PlanState::Failed);
        assert_eq!(view.slices.len(), 1);
        assert_eq!(view.slices[0].slice_id, "slice");
        assert_eq!(view.slices[0].state, SliceState::Failed);
        assert!(view.slices[0].stale_paths.is_empty());
        assert!(
            view.slices[0]
                .result
                .as_ref()
                .and_then(|result| result.failure.as_ref())
                .is_some_and(|failure| failure.as_str().contains("daemon restart"))
        );
        assert_eq!(
            reopened.slice_records(&draft.plan.id).unwrap()[0].state,
            SliceState::Failed
        );
        assert_eq!(
            reopened.conversations(None, 10).unwrap().len(),
            1,
            "recovery must not create a coder Conversation"
        );
        assert_eq!(
            harness.model_calls.load(Ordering::SeqCst),
            calls_before_restart,
            "recovery must not repeat the coder side effect"
        );
        assert_eq!(approved.plan.id, draft.plan.id);
    }
    fn text(value: &str) -> PlanText {
        PlanText::new(value).unwrap()
    }
    fn id(value: &str) -> PlanItemId {
        PlanItemId::new(value).unwrap()
    }
    fn source() -> SourceRef {
        SourceRef {
            kind: SourceKind::File,
            locator: text("spec.md"),
            digest: None,
            line_start: None,
            line_end: None,
        }
    }
    fn check(name: &str) -> Check {
        Check {
            id: id(name),
            statement: text("check"),
            sources: vec![source()],
        }
    }
    fn plan() -> ImplementationPlan {
        let file = WorkspacePath::new("src/lib.rs").unwrap();
        ImplementationPlan {
            schema: PlanSchema::V1,
            id: PlanId::generate(),
            workspace: WorkspaceEvidence {
                canonical_root_sha256: PlanDigest::from_bytes([0; 32]),
                git_commit: None,
                dirty_paths: vec![],
            },
            objective: Objective {
                outcome: text("outcome"),
                acceptance: vec![check("accept")],
                sources: vec![source()],
            },
            evidence: vec![Evidence {
                id: id("evidence"),
                statement: text("fact"),
                sources: vec![source()],
            }],
            decisions: vec![],
            invariants: vec![],
            slices: vec![ImplementationSlice {
                id: id("slice"),
                outcome: text("done"),
                depends_on: vec![],
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
                    id: id("step"),
                    action: text("change"),
                    files: vec![file],
                    symbols: vec![],
                    depends_on: vec![],
                    satisfies: vec![id("accept")],
                }],
                verification: vec![Verification {
                    command: text("cargo test"),
                    working_directory: WorkspacePath::new(".").unwrap(),
                    expected: VerificationExpected {
                        exit_code: 0,
                        stdout_contains: vec![],
                        stderr_contains: vec![],
                    },
                    covers: vec![],
                }],
                done_when: vec![check("done")],
            }],
            open_decisions: vec![],
        }
    }

    #[test]
    fn draft_replacement_and_approved_immutability() {
        let root = root();
        let store = PlanArtifactStore::new(&root).unwrap();
        let plan = plan();
        let digest = store.publish_draft(&plan).unwrap();
        assert_eq!(store.read_draft(&plan.id).unwrap().digest, digest);
        let approved = store.approve_draft(&plan.id, &digest).unwrap();
        assert_eq!(
            store.approve_draft(&plan.id, &digest).unwrap().bytes,
            approved.bytes
        );
        assert_eq!(
            approved.bytes,
            fs::read(store.approved_path(&digest)).unwrap()
        );
        let mut changed = plan.clone();
        changed.objective.outcome = text("changed");
        store.publish_draft(&changed).unwrap();
        assert_eq!(store.read_approved(&digest).unwrap().bytes, approved.bytes);
        assert!(store.approve_draft(&plan.id, &digest).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_approval_persists_state_and_is_idempotent_for_identical_bytes() {
        let harness = Harness::new(GenerationMode::FreshConversation);
        let (draft, conversation) = create(&harness).unwrap();
        let record = harness
            .store
            .as_ref()
            .unwrap()
            .plan_record(draft.plan.id.clone())
            .unwrap();
        assert_eq!(record.state, PlanState::ReadyForApproval);
        assert_eq!(record.draft_digest, draft.digest);
        assert_eq!(record.planner_conversation_id, conversation.id);

        let approved = harness
            .plans
            .approve_draft_durable(
                harness.store.as_ref().unwrap(),
                &draft.plan.id,
                &draft.digest,
            )
            .unwrap();
        assert_eq!(approved.state, PlanState::Approved);
        assert_eq!(
            harness
                .store
                .as_ref()
                .unwrap()
                .plan_record(draft.plan.id.clone())
                .unwrap()
                .approved_digest,
            Some(draft.digest.clone())
        );
        let repeated = harness
            .plans
            .approve_draft_durable(
                harness.store.as_ref().unwrap(),
                &draft.plan.id,
                &draft.digest,
            )
            .unwrap();
        assert_eq!(repeated, approved);
    }

    #[test]
    fn artifact_boundary_failure_leaves_no_approved_database_reference() {
        let harness = Harness::new(GenerationMode::FreshConversation);
        let (draft, _) = create(&harness).unwrap();
        assert!(
            harness
                .plans
                .approve_draft_with_injected_commit_failure(
                    harness.store.as_ref().unwrap(),
                    &draft.plan.id,
                    &draft.digest,
                )
                .is_err()
        );
        let record = harness
            .store
            .as_ref()
            .unwrap()
            .plan_record(draft.plan.id.clone())
            .unwrap();
        assert_eq!(record.state, PlanState::ReadyForApproval);
        assert!(record.approved_digest.is_none());
        assert_eq!(
            harness.plans.read_approved(&draft.digest).unwrap().bytes,
            draft.bytes
        );
    }

    #[test]
    fn approval_requires_current_digest_and_open_decisions_never_transition() {
        let harness = Harness::new(GenerationMode::FreshConversation);
        let (draft, conversation) = create(&harness).unwrap();
        let stale = PlanDigest::from_bytes([42; 32]);
        assert!(
            harness
                .plans
                .approve_draft_durable(harness.store.as_ref().unwrap(), &draft.plan.id, &stale)
                .is_err()
        );
        assert_eq!(
            harness
                .store
                .as_ref()
                .unwrap()
                .plan_record(draft.plan.id.clone())
                .unwrap()
                .state,
            PlanState::ReadyForApproval
        );

        let mut open = draft.plan.clone();
        open.open_decisions.push(OpenDecision {
            id: id("question"),
            question: text("choose"),
            consequences: text("changes the implementation"),
            affected_slices: vec![id("slice")],
            sources: vec![source()],
        });
        let open_digest = harness.plans.publish_draft(&open).unwrap();
        harness
            .store
            .as_ref()
            .unwrap()
            .register_plan_draft(
                open.id.clone(),
                open_digest.clone(),
                &harness.workspace,
                conversation.id,
                PlanState::for_draft(&open),
            )
            .unwrap();
        assert_eq!(
            harness
                .store
                .as_ref()
                .unwrap()
                .plan_record(open.id.clone())
                .unwrap()
                .state,
            PlanState::AwaitingDecisions
        );
        assert!(
            harness
                .plans
                .approve_draft_durable(harness.store.as_ref().unwrap(), &open.id, &open_digest)
                .is_err()
        );
    }

    #[test]
    fn rejects_partial_and_digest_mismatch_artifacts() {
        let root = root();
        let store = PlanArtifactStore::new(&root).unwrap();
        let plan = plan();
        let draft = store.draft_path(&plan.id);
        fs::write(&draft, b"{\"schema\":").unwrap();
        assert!(store.read_draft(&plan.id).is_err());
        let digest = PlanDigest::from_bytes([7; 32]);
        let path = store.approved_path(&digest);
        fs::write(&path, b"not the digest").unwrap();
        assert!(store.read_approved(&digest).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn draft_filename_must_match_canonical_plan_id() {
        let root = root();
        let store = PlanArtifactStore::new(&root).unwrap();
        let plan = plan();
        let requested = PlanId::generate();
        fs::write(
            store.draft_path(&requested),
            plan.canonical_bytes().unwrap(),
        )
        .unwrap();
        assert!(store.read_draft(&requested).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn open_decisions_cannot_be_published_as_approved() {
        let root = root();
        let store = PlanArtifactStore::new(&root).unwrap();
        let mut plan = plan();
        plan.open_decisions.push(OpenDecision {
            id: id("choice"),
            question: text("which"),
            consequences: text("changes"),
            affected_slices: vec![id("slice")],
            sources: vec![source()],
        });
        let digest = store.publish_draft(&plan).unwrap();
        assert!(store.approve_draft(&plan.id, &digest).is_err());
        assert!(!store.approved_path(&digest).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn immutable_creation_never_replaces_an_existing_target() {
        let root = root();
        let store = PlanArtifactStore::new(&root).unwrap();
        let target = store.root.join("approved").join("collision.json");
        atomic_create(&target, b"first").unwrap();
        assert!(atomic_create(&target, b"second").is_err());
        assert_eq!(fs::read(&target).unwrap(), b"first");
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 1);
        let _ = fs::remove_dir_all(root);
    }
}
