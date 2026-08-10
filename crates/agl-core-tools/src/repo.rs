use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agl_artifact::{
    ArtifactBinding, ArtifactCommitEntry, ArtifactCommitEntryKind, ArtifactCommitRequest,
    ArtifactHandle,
};
use agl_kernel::{
    ArtifactAccess, ArtifactDeclaration, ArtifactEffectLink, ArtifactId, ArtifactKindId,
    ArtifactTargetSelector, AuthorityClass, EffectDeclaration, EffectId, ExtensionDescriptor,
    ExtensionId, ObservedEffect, OperationKind, ToolDeclaration, ToolDispatchContext, ToolHandler,
    ToolId, ToolResult,
};
use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::parse_tool_args as parse_args;

pub const EXTENSION_ID: &str = "core.repo";
pub const ARTIFACT_COMMIT_TOOL_ID: &str = "core.repo:artifact.commit";
pub const TASKS_VERIFY_TOOL_ID: &str = "core.repo:tasks.verify";

const ARTIFACT_REPOSITORY_EFFECT_ID: &str = "agl:artifact.repository";
const REPO_GITLINK_EFFECT_ID: &str = "agl:repo.gitlink";

#[derive(Clone, Debug)]
pub struct RepoTools {
    workspace_root: PathBuf,
    store_root: Option<PathBuf>,
    artifacts: BTreeMap<ArtifactId, (ArtifactBinding, ArtifactHandle)>,
}

impl RepoTools {
    pub fn new(workspace_root: impl AsRef<Path>) -> Self {
        Self {
            workspace_root: workspace_root.as_ref().to_path_buf(),
            store_root: None,
            artifacts: BTreeMap::new(),
        }
    }

    pub fn with_store_root(mut self, store_root: impl AsRef<Path>) -> Self {
        self.store_root = Some(store_root.as_ref().to_path_buf());
        self
    }

    pub fn with_artifact(
        mut self,
        binding: ArtifactBinding,
        handle: ArtifactHandle,
    ) -> Result<Self> {
        anyhow::ensure!(
            binding.artifact_id() == handle.id(),
            "Artifact binding and handle IDs differ"
        );
        anyhow::ensure!(
            self.artifacts
                .insert(handle.id().clone(), (binding, handle))
                .is_none(),
            "duplicate Artifact handler binding"
        );
        Ok(self)
    }

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<Value> {
        match name {
            ARTIFACT_COMMIT_TOOL_ID => {
                parse_args::<ArtifactCommitArgs>(name, arguments)?;
                anyhow::bail!("Artifact commit requires an admitted kernel Tool Effect call")
            }
            TASKS_VERIFY_TOOL_ID => {
                parse_args::<TasksVerifyArgs>(name, arguments)?;
                self.verify_tasks()
            }
            _ => anyhow::bail!("unknown repo tool `{name}`"),
        }
    }

    fn verify_tasks(&self) -> Result<Value> {
        let id = ArtifactId::new("core.repo:tasks").expect("fixed Artifact ID is valid");
        let (_, handle) = self
            .artifacts
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("core.repo:tasks ArtifactHandle is not admitted"))?;
        let report = agl_repo::verify_task_specs(
            handle,
            &agl_repo::TaskSpecVerifyOptions { strict: false },
        )?;
        Ok(json!({
            "tool": TASKS_VERIFY_TOOL_ID,
            "status": if report.errors.is_empty() { "ok" } else { "invalid" },
            "artifact_id": id,
            "files": report.files,
            "errors": report.errors,
        }))
    }
}

impl ToolHandler for RepoTools {
    fn dispatch(&self, context: ToolDispatchContext) -> agl_kernel::ToolHandlerFuture<'_> {
        Box::pin(async move {
            let correlation = context.effect_correlation().cloned();
            let invocation = context.into_invocation();
            if invocation.tool_id.as_str() == TASKS_VERIFY_TOOL_ID {
                parse_args::<TasksVerifyArgs>(TASKS_VERIFY_TOOL_ID, invocation.arguments)
                    .map_err(agl_kernel::ToolHandlerError::from)?;
                return self.verify_tasks().map(ToolResult::new).map_err(Into::into);
            }
            let operation_id = invocation
                .run_step_idempotency_key()
                .or_else(|| invocation.request_id.as_ref().map(ToString::to_string))
                .ok_or_else(|| {
                    agl_kernel::ToolHandlerError::from(anyhow::anyhow!(
                        "Artifact commit requires a request or run-step idempotency identity"
                    ))
                })?;
            let args =
                parse_args::<ArtifactCommitArgs>(ARTIFACT_COMMIT_TOOL_ID, invocation.arguments)
                    .map_err(agl_kernel::ToolHandlerError::from)?;
            let correlation = correlation.ok_or_else(|| {
                agl_kernel::ToolHandlerError::from(anyhow::anyhow!(
                    "Artifact commit was dispatched without Tool Effect correlation"
                ))
            })?;
            let id = ArtifactId::new(args.artifact_id)
                .map_err(|error| agl_kernel::ToolHandlerError::from(anyhow::anyhow!(error)))?;
            let (binding, handle) = self.artifacts.get(&id).ok_or_else(|| {
                agl_kernel::ToolHandlerError::from(anyhow::anyhow!(
                    "ArtifactHandle is not admitted for {id}"
                ))
            })?;
            handle
                .require_access(ArtifactAccess::MutateTree)
                .map_err(|error| agl_kernel::ToolHandlerError::from(anyhow::anyhow!(error)))?;
            let entries = args
                .entries
                .into_iter()
                .map(|entry| {
                    let kind = match entry.operation {
                        ArtifactCommitEntryOperation::Create => ArtifactCommitEntryKind::Create,
                        ArtifactCommitEntryOperation::Update => ArtifactCommitEntryKind::Update,
                        ArtifactCommitEntryOperation::Delete => ArtifactCommitEntryKind::Delete,
                    };
                    ArtifactCommitEntry::new(entry.path, kind)
                })
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| agl_kernel::ToolHandlerError::from(anyhow::anyhow!(error)))?;
            let request = ArtifactCommitRequest::new(
                operation_id,
                correlation,
                id.clone(),
                entries,
                args.message,
            )
            .map_err(|error| agl_kernel::ToolHandlerError::from(anyhow::anyhow!(error)))?;
            let store_root = self.store_root.as_ref().ok_or_else(|| {
                agl_kernel::ToolHandlerError::from(anyhow::anyhow!(
                    "Artifact commit store is not configured"
                ))
            })?;
            let mut store = agl_store::AglStore::open_at(store_root)
                .map_err(|error| agl_kernel::ToolHandlerError::from(anyhow::anyhow!(error)))?;
            let repository = agl_repo::ArtifactGitRepository::open(&self.workspace_root)
                .map_err(|error| agl_kernel::ToolHandlerError::from(anyhow::anyhow!(error)))?;
            let result = repository
                .commit_artifact(binding, request, &mut store)
                .map_err(|error| agl_kernel::ToolHandlerError::from(anyhow::anyhow!(error)))?;
            let data = json!({
                "tool": ARTIFACT_COMMIT_TOOL_ID,
                "status": if result.is_conflict() { "conflict" } else { "committed" },
                "artifact_id": id,
                "operation_id": result.operation_id(),
                "child_commit": result.child_commit(),
                "parent_commit": result.parent_commit(),
            });
            Ok(ToolResult::new(data).with_observed_effects([
                ObservedEffect::new(
                    EffectId::new(ARTIFACT_REPOSITORY_EFFECT_ID).expect("fixed Effect ID"),
                    [
                        ("artifact_id".to_owned(), id.to_string()),
                        ("child_commit".to_owned(), result.child_commit().to_owned()),
                    ],
                ),
                ObservedEffect::new(
                    EffectId::new(REPO_GITLINK_EFFECT_ID).expect("fixed Effect ID"),
                    [
                        ("artifact_id".to_owned(), id.to_string()),
                        (
                            "parent_commit".to_owned(),
                            result.parent_commit().to_owned(),
                        ),
                    ],
                ),
            ]))
        })
    }
}

pub fn declaration() -> ExtensionDescriptor {
    let tasks_id = ArtifactId::new("core.repo:tasks").unwrap();
    let repository_effect = EffectId::new(ARTIFACT_REPOSITORY_EFFECT_ID).unwrap();
    let gitlink_effect = EffectId::new(REPO_GITLINK_EFFECT_ID).unwrap();
    let commit = ToolDeclaration::from_schema::<ArtifactCommitArgs>(
        ToolId::new(ARTIFACT_COMMIT_TOOL_ID).unwrap(),
        "Commit exact changed entries in one verified Artifact and advance its parent gitlink.",
        OperationKind::Write,
    )
    .unwrap()
    .with_state_effects([repository_effect.clone(), gitlink_effect.clone()])
    .with_artifact_link(ArtifactEffectLink::new(
        repository_effect.clone(),
        ArtifactTargetSelector::FromArgument {
            pointer: "/artifact_id".to_owned(),
            access: ArtifactAccess::MutateTree,
        },
        ArtifactAccess::MutateTree,
    ));
    let verify = ToolDeclaration::from_schema::<TasksVerifyArgs>(
        ToolId::new(TASKS_VERIFY_TOOL_ID).unwrap(),
        "Validate planned task specifications from core.repo:tasks.",
        OperationKind::Read,
    )
    .unwrap()
    .with_conditional_state_effects([EffectId::repo_files()])
    .with_artifact_link(ArtifactEffectLink::new(
        EffectId::repo_files(),
        ArtifactTargetSelector::Fixed(tasks_id.clone()),
        ArtifactAccess::ReadTree,
    ));

    ExtensionDescriptor::builtin(
        ExtensionId::new(EXTENSION_ID).unwrap(),
        "Repository Artifact Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .unwrap()
    .with_artifact(
        ArtifactDeclaration::new(
            tasks_id,
            ArtifactKindId::new("agentlibre.task-specs").unwrap(),
            [ArtifactAccess::ReadTree],
        )
        .unwrap(),
    )
    .with_tool(commit)
    .with_tool(verify)
    .with_effects([
        EffectDeclaration::new(
            repository_effect,
            AuthorityClass::RepositoryMutation.as_str(),
        ),
        EffectDeclaration::new(gitlink_effect, AuthorityClass::RepositoryMutation.as_str()),
        EffectDeclaration::for_standard(EffectId::repo_files()).unwrap(),
    ])
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ArtifactCommitArgs {
    artifact_id: String,
    entries: Vec<ArtifactCommitEntryArgs>,
    message: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct ArtifactCommitEntryArgs {
    path: String,
    operation: ArtifactCommitEntryOperation,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ArtifactCommitEntryOperation {
    Create,
    Update,
    Delete,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TasksVerifyArgs {}
