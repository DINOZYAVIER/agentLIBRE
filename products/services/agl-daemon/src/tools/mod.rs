//! Host composition for native tools. Admission and mutation ownership stay here.
pub(crate) mod memory;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use agl_core::agent::{ToolFailure, ToolFailureKind};
use agl_runtime::extension::parse_package_view;
use agl_tool_api::ExecutionHost;
use agl_tool_api::{ExtensionBindings, ServiceGit};
use agl_tools::searxng::SearxngConfig;
use anyhow::Result;

pub(crate) const COMMITTED_RESULT_LIMIT: &str = r#"{"kind":"tool_result_limit","effect":"committed","output":"omitted","next_actions":["Inspect the committed effect; do not repeat this operation."]}"#;

static WORKSPACE_MUTATIONS: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

/// Return the process-local mutation lock for one canonical workspace root.
/// Memory commits and `agentlibre.files:file.patch` deliberately share this registry so a
/// digest check and its following rename sequence cannot interleave.
pub(crate) fn workspace_mutation(root: &Path) -> Arc<Mutex<()>> {
    let locks = WORKSPACE_MUTATIONS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut locks = locks.lock().expect("workspace mutation registry poisoned");
    Arc::clone(
        locks
            .entry(root.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

pub(crate) fn files_bindings(git: ServiceGit) -> Result<ExtensionBindings> {
    let view = agl_tools::files_declaration();
    let package = parse_package_view(&view)?;
    agl_tools::files_bindings(
        package.manifest.version,
        agl_core::package::compute_package_digest(&view)?,
        package.definition,
        Arc::new(workspace_mutation),
        git,
    )
}

pub(crate) fn forge_bindings() -> Result<ExtensionBindings> {
    let view = agl_tools::forge_declaration();
    let package = parse_package_view(&view)?;
    agl_tools::forge_bindings(
        package.manifest.version,
        agl_core::package::compute_package_digest(&view)?,
        package.definition,
        Arc::new(resolve_artifact),
    )
}

pub(crate) fn execution_bindings(client: ExecutionHost) -> Result<ExtensionBindings> {
    let view = agl_tools::execution::declaration();
    let package = parse_package_view(&view)?;
    agl_tools::execution::bindings(
        package.manifest.version,
        agl_core::package::compute_package_digest(&view)?,
        package.definition,
        client,
    )
}

pub(crate) fn search_bindings(config: SearxngConfig) -> Result<ExtensionBindings> {
    let view = agl_tools::searxng::declaration();
    let package = parse_package_view(&view)?;
    agl_tools::searxng::bindings(
        package.manifest.version,
        agl_core::package::compute_package_digest(&view)?,
        package.definition,
        config,
    )
}

fn resolve_artifact(
    workspace: &Path,
    id: &str,
    relative: &Path,
) -> Result<agl_tool_api::ForgeFile, ToolFailure> {
    let artifacts = agl_runtime::resolve_workspace_forge(workspace).map_err(|_| {
        ToolFailure::no_effect(
            ToolFailureKind::Execution,
            Some("forge"),
            &["Verify the workspace agentLIBRE.toml and Forge lock."],
        )
    })?;
    artifacts
        .declarations
        .iter()
        .find(|artifact| artifact.id == id)
        .map(|artifact| {
            artifact.open_file(relative).map_err(|_| {
                ToolFailure::no_effect(
                    ToolFailureKind::Execution,
                    Some("forge"),
                    &["Run agl forge sync and use a contained file."],
                )
            })
        })
        .ok_or_else(|| {
            ToolFailure::no_effect(
                ToolFailureKind::InvalidInput,
                Some("path"),
                &["Use an artifact ID declared by the workspace and an existing relative file."],
            )
        })?
}

#[cfg(test)]
fn unavailable_execution() -> ExecutionHost {
    ExecutionHost::new(|_, _| {
        Box::pin(async { Err(ToolFailure::unknown(ToolFailureKind::Unavailable)) })
    })
}

#[cfg(test)]
fn unavailable_git() -> ServiceGit {
    Arc::new(|_| Err(agl_tool_api::ServiceGitNotStarted))
}
#[cfg(test)]
fn test_git() -> ServiceGit {
    use std::io::Write;
    Arc::new(|request| {
        let mut child = std::process::Command::new("git")
            .args(request.args)
            .current_dir(request.cwd)
            .env_clear()
            .envs(request.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|_| agl_tool_api::ServiceGitNotStarted)?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&request.stdin)
            .unwrap();
        let output = child.wait_with_output().unwrap();
        Ok(agl_tool_api::ServiceGitOutput {
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
            ..Default::default()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agl_core::package::{InMemoryPackageView, compute_package_digest};
    use agl_runtime::package::DirectoryPackageView;
    use sha2::{Digest as _, Sha256};

    fn assert_source_matches(view: &InMemoryPackageView, directory: &str) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../extensions")
            .join(directory);
        let source = DirectoryPackageView::new(root).unwrap();
        assert_eq!(
            compute_package_digest(view).unwrap(),
            compute_package_digest(&source).unwrap()
        );
        assert_eq!(
            parse_package_view(view).unwrap(),
            parse_package_view(&source).unwrap()
        );
    }

    fn assert_bindings_exact(bindings: &ExtensionBindings, view: &InMemoryPackageView) {
        let package = parse_package_view(view).unwrap();
        assert_eq!(bindings.version, package.manifest.version);
        assert_eq!(bindings.definition, package.definition);
        assert_eq!(
            bindings.content_digest,
            compute_package_digest(view).unwrap()
        );
        assert_eq!(bindings.tools.len(), bindings.definition.tools.len());
        for definition in &bindings.definition.tools {
            let digest = agl_core::agent::ToolDefinitionDigest::from_bytes(
                Sha256::digest(serde_json::to_vec(definition).unwrap()).into(),
            );
            assert!(
                bindings
                    .tools
                    .iter()
                    .any(|binding| binding.tool_id == definition.id
                        && binding.definition_digest == digest)
            );
        }
    }

    #[test]
    fn files_declarations_and_bindings_are_exact() {
        let view = agl_tools::files_declaration();
        assert_source_matches(&view, "agentlibre-files");
        let bindings = files_bindings(unavailable_git()).unwrap();
        assert_bindings_exact(&bindings, &view);
        assert_eq!(bindings.tools.len(), 2);
        let grant = |root| agl_core::AuthorityGrant {
            effect: agl_core::EffectId::new("agentlibre.files:file.write").unwrap(),
            scope: agl_core::CanonicalJson::new(serde_json::json!({"root":root})).unwrap(),
        };
        assert!((bindings.allows_authority)(&grant("/workspace")));
        assert!(!(bindings.allows_authority)(&grant("workspace")));
    }

    #[test]
    fn forge_declarations_and_bindings_are_exact() {
        let view = agl_tools::forge_declaration();
        assert_source_matches(&view, "agentlibre-forge");
        let bindings = forge_bindings().unwrap();
        assert_bindings_exact(&bindings, &view);
        assert_eq!(bindings.tools.len(), 1);
    }

    #[test]
    fn stock_execution_bindings_match_the_declarative_package() {
        let view = agl_tools::execution::declaration();
        assert_source_matches(&view, "agentlibre-execution");
        let bindings = execution_bindings(unavailable_execution()).unwrap();
        assert_bindings_exact(&bindings, &view);
        assert_eq!(bindings.tools.len(), 2);
    }

    #[test]
    fn search_declaration_matches_the_source() {
        assert_source_matches(&agl_tools::searxng::declaration(), "agentlibre-searxng");
    }

    #[tokio::test]
    async fn providers_use_host_artifacts_and_shared_workspace_mutation_lock() {
        use agl_core::agent::{
            AbsolutePath, AgentOperationKey, AuthorityGrantSet, RelativePath, WorkspaceScope,
        };
        use agl_tool_api::{ToolCancellation, ToolContext};
        use std::sync::atomic::{AtomicBool, Ordering};

        let root = std::env::temp_dir().join(format!("agl-tools-host-{}", uuid::Uuid::now_v7()));
        let artifact = root.join("artifact");
        std::fs::create_dir_all(&artifact).unwrap();
        std::fs::write(artifact.join("README.md"), "Host artifact\n").unwrap();
        let root = root.canonicalize().unwrap();
        let shared = workspace_mutation(&root);
        let requested_lock = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&requested_lock);
        let expected_root = root.clone();
        let expected_shared = Arc::clone(&shared);
        let resolver_root = root.clone();
        let files_view = agl_tools::files_declaration();
        let files_package = parse_package_view(&files_view).unwrap();
        let files = agl_tools::files_bindings(
            files_package.manifest.version,
            compute_package_digest(&files_view).unwrap(),
            files_package.definition,
            Arc::new(move |workspace| {
                assert_eq!(workspace, expected_root);
                let lock = workspace_mutation(workspace);
                assert!(Arc::ptr_eq(&lock, &expected_shared));
                observed.store(true, Ordering::SeqCst);
                lock
            }),
            test_git(),
        )
        .unwrap();
        let forge_view = agl_tools::forge_declaration();
        let forge_package = parse_package_view(&forge_view).unwrap();
        let forge = agl_tools::forge_bindings(
            forge_package.manifest.version,
            compute_package_digest(&forge_view).unwrap(),
            forge_package.definition,
            Arc::new(move |workspace, id, relative| {
                assert_eq!(workspace, resolver_root);
                assert_eq!(id, "test.document");
                assert_eq!(relative, Path::new("README.md"));
                Ok(agl_tool_api::ForgeFile::new(
                    std::fs::File::open(artifact.join(relative)).unwrap(),
                    Box::new(()),
                    agl_tool_api::VerificationStatus::Verified,
                ))
            }),
        )
        .unwrap();
        let context = ToolContext::new(
            AgentOperationKey {
                run_id: agl_core::AgentRunId::generate(),
                ordinal: std::num::NonZeroU32::new(1).unwrap(),
            },
            None,
            WorkspaceScope {
                root: AbsolutePath::try_from(root.to_string_lossy().into_owned()).unwrap(),
                working_directory: RelativePath::try_from(".".to_owned()).unwrap(),
            },
            AuthorityGrantSet(vec![agl_core::AuthorityGrant {
                effect: agl_core::EffectId::new("agentlibre.files:file.write").unwrap(),
                scope: agl_core::CanonicalJson::new(serde_json::json!({"root":root})).unwrap(),
            }]),
            i64::MAX,
            65_536,
            ToolCancellation::new(),
        );
        let read = forge
            .tools
            .iter()
            .find(|tool| tool.tool_id.as_str() == "agentlibre.forge:artifact.read")
            .unwrap();
        let result = read.handler.call(context.clone(), serde_json::json!({"path":"forge:test.document/README.md","cursor":1,"limit_lines":10})).await.unwrap();
        assert!(
            serde_json::to_string(&result)
                .unwrap()
                .contains("Host artifact")
        );
        std::fs::write(root.join("artifact/README.md"), [0xff, 0, 0xa5]).unwrap();
        let binary = read.handler.call(context.clone(), serde_json::json!({"path":"forge:test.document/README.md","cursor":1,"limit_lines":10})).await.unwrap();
        let encoded = serde_json::to_value(binary).unwrap();
        let encoded = serde_json::to_string(&encoded).unwrap();
        assert!(encoded.contains("binary"));
        assert!(encoded.contains("sha256:"));
        assert!(encoded.contains("verified"));
        assert!(encoded.contains("artifact/README.md"));
        let patch = files
            .tools
            .iter()
            .find(|tool| tool.tool_id.as_str() == "agentlibre.files:file.patch")
            .unwrap();
        patch.handler.call(context, serde_json::json!({"patch":"diff --git a/created.txt b/created.txt\nnew file mode 100644\n--- /dev/null\n+++ b/created.txt\n@@ -0,0 +1 @@\n+Created\n"})).await.unwrap();
        assert!(requested_lock.load(Ordering::SeqCst));
        assert_eq!(
            std::fs::read_to_string(root.join("created.txt")).unwrap(),
            "Created\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn providers_reject_mismatched_package_and_tool_identity() {
        let view = agl_tools::execution::declaration();
        let package = parse_package_view(&view).unwrap();
        let wrong_digest = compute_package_digest(&agl_tools::files_declaration()).unwrap();
        assert!(
            agl_tools::execution::bindings(
                package.manifest.version.clone(),
                wrong_digest,
                package.definition.clone(),
                unavailable_execution()
            )
            .is_err()
        );
        let mut wrong_definition = package.definition;
        wrong_definition.tools[0].id =
            agl_core::ToolId::new("agentlibre.execution:unsupported").unwrap();
        assert!(
            agl_tools::execution::bindings(
                package.manifest.version,
                compute_package_digest(&view).unwrap(),
                wrong_definition,
                unavailable_execution()
            )
            .is_err()
        );
    }
}
