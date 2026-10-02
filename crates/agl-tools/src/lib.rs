//! First-party file and Forge Tool bindings.
//!
//! Registering these handlers does not admit them to an AgentRun. The admitted
//! snapshot must carry the exact Tool and Extension definition digests.

pub mod execution;
mod filesystem;
mod forge;
pub mod searxng;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agl_core::Content;
use agl_core::ToolId;
use agl_core::agent::{
    EffectReceipt, ToolDefinition, ToolDefinitionDigest, ToolFailure, ToolFailureKind, ToolResult,
};
use agl_core::package::{
    InMemoryPackageView, PackageRelativePath, PackageView, compute_package_digest,
};
use agl_tool_api::{ExtensionBindings, ToolBinding, ToolContext, ToolFuture, ToolHandler};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

pub(crate) const COMMITTED_RESULT_LIMIT: &str = r#"{"kind":"tool_result_limit","effect":"committed","output":"omitted","next_actions":["Inspect the committed effect; do not repeat this operation."]}"#;

/// Resolve a host-declared artifact for a workspace.
pub type ArtifactResolver = Arc<dyn Fn(&Path, &str) -> Result<PathBuf, ToolFailure> + Send + Sync>;
/// Obtain the host's shared mutation lock for a canonical workspace root.
pub type WorkspaceMutation = Arc<dyn Fn(&Path) -> Arc<Mutex<()>> + Send + Sync>;

/// Preserve proven effects even when JSON escaping or result framing exceeds
/// the output budget. The driver reserves receipt space before dispatch and
/// validates the exact receipts and complete result again before committing.
pub(crate) fn committed_result(
    text: String,
    receipt: EffectReceipt,
    result_bytes: u64,
) -> Result<ToolResult, ToolFailure> {
    let content = if text.len() > agl_core::MAX_TEXT_BYTES {
        Content::text(COMMITTED_RESULT_LIMIT).expect("bounded committed-result notice")
    } else {
        Content::text(text).map_err(|_| invalid_result())?
    };
    let mut result = ToolResult {
        content,
        effect_receipts: vec![receipt],
    };
    if serde_json::to_vec(&result)
        .map_err(|_| invalid_result())?
        .len() as u64
        > result_bytes.min(agl_core::agent::MAX_TOOL_RESULT_BYTES)
    {
        result.content =
            Content::text(COMMITTED_RESULT_LIMIT).expect("bounded committed-result notice");
    }
    Ok(result)
}

pub fn files_bindings(
    version: agl_core::package::PackageVersion,
    content_digest: agl_core::package::PackageTreeDigest,
    definition: agl_core::ExtensionDefinition,
    mutations: WorkspaceMutation,
) -> anyhow::Result<ExtensionBindings> {
    validate_declaration(
        &files_declaration(),
        &version,
        &content_digest,
        &definition,
        "agentlibre.files",
        &[filesystem::READ, filesystem::APPLY_PATCH],
    )?;
    let tools = definition
        .tools
        .iter()
        .map(|definition| {
            let tool_id = definition.id.clone();
            let handler: Arc<dyn ToolHandler> = Arc::new(FilesystemHandler {
                tool: tool_id.clone(),
                mutations: Arc::clone(&mutations),
            });
            ToolBinding {
                tool_id,
                definition_digest: definition_digest(definition),
                handler,
            }
        })
        .collect();
    Ok(ExtensionBindings {
        version,
        content_digest,
        definition,
        tools,
        allows_authority: Arc::new(|grant| {
            grant.effect.as_str() == filesystem::WRITE_EFFECT
                && grant
                    .scope
                    .as_value()
                    .get("root")
                    .and_then(Value::as_str)
                    .is_some_and(|root| Path::new(root).is_absolute())
        }),
    })
}

pub fn forge_bindings(
    version: agl_core::package::PackageVersion,
    content_digest: agl_core::package::PackageTreeDigest,
    definition: agl_core::ExtensionDefinition,
    artifacts: ArtifactResolver,
) -> anyhow::Result<ExtensionBindings> {
    validate_declaration(
        &forge_declaration(),
        &version,
        &content_digest,
        &definition,
        "agentlibre.forge",
        &[forge::READ],
    )?;
    let tools = definition
        .tools
        .iter()
        .map(|definition| ToolBinding {
            tool_id: definition.id.clone(),
            definition_digest: definition_digest(definition),
            handler: Arc::new(ForgeHandler {
                artifacts: Arc::clone(&artifacts),
            }),
        })
        .collect();
    Ok(ExtensionBindings {
        version,
        content_digest,
        definition,
        tools,
        allows_authority: Arc::new(|_| false),
    })
}

pub fn files_declaration() -> InMemoryPackageView {
    InMemoryPackageView::new([
        embedded(
            "EXTENSION.toml",
            include_bytes!("../../../extensions/agentlibre-files/EXTENSION.toml"),
        ),
        embedded(
            "schemas/file-write-scope.json",
            include_bytes!("../../../extensions/agentlibre-files/schemas/file-write-scope.json"),
        ),
        embedded(
            "schemas/file-read.json",
            include_bytes!("../../../extensions/agentlibre-files/schemas/file-read.json"),
        ),
        embedded(
            "schemas/file-patch.json",
            include_bytes!("../../../extensions/agentlibre-files/schemas/file-patch.json"),
        ),
    ])
    .expect("embedded Extension package")
}

pub fn forge_declaration() -> InMemoryPackageView {
    InMemoryPackageView::new([
        embedded(
            "EXTENSION.toml",
            include_bytes!("../../../extensions/agentlibre-forge/EXTENSION.toml"),
        ),
        embedded(
            "schemas/artifact-read.json",
            include_bytes!("../../../extensions/agentlibre-forge/schemas/artifact-read.json"),
        ),
    ])
    .expect("embedded Extension package")
}

fn embedded(path: &str, bytes: &[u8]) -> (PackageRelativePath, Vec<u8>) {
    (
        PackageRelativePath::new(path).expect("static package path"),
        bytes.to_vec(),
    )
}

fn definition_digest(definition: &ToolDefinition) -> ToolDefinitionDigest {
    let bytes = serde_json::to_vec(definition).expect("ToolDefinition serializes");
    ToolDefinitionDigest::from_bytes(Sha256::digest(bytes).into())
}

struct FilesystemHandler {
    tool: ToolId,
    mutations: WorkspaceMutation,
}

struct ForgeHandler {
    artifacts: ArtifactResolver,
}

impl ToolHandler for ForgeHandler {
    fn call(&self, context: ToolContext, input: Value) -> ToolFuture {
        let artifacts = Arc::clone(&self.artifacts);
        Box::pin(async move {
            let value = forge::execute(&context, input, &artifacts)?;
            Ok(ToolResult {
                content: Content::text(
                    serde_json::to_string(&value).map_err(|_| invalid_result())?,
                )
                .map_err(|_| invalid_result())?,
                effect_receipts: Vec::new(),
            })
        })
    }
}

impl ToolHandler for FilesystemHandler {
    fn call(&self, context: ToolContext, input: Value) -> ToolFuture {
        let tool = self.tool.clone();
        let mutations = Arc::clone(&self.mutations);
        Box::pin(async move {
            let (value, effect_receipts) =
                filesystem::execute(tool.as_str(), &context, input, &mutations)?;
            Ok(ToolResult {
                content: Content::text(
                    serde_json::to_string(&value).map_err(|_| invalid_result())?,
                )
                .map_err(|_| invalid_result())?,
                effect_receipts,
            })
        })
    }
}

fn execution_failure() -> ToolFailure {
    // Filesystem callers either have not committed changes or return this
    // error only after successful rollback. Failed rollback is OutcomeUnknown.
    ToolFailure::no_effect(
        ToolFailureKind::Execution,
        Some("resource"),
        &["Check that the workspace resource is available, then submit a corrected call."],
    )
}

fn invalid_result() -> ToolFailure {
    ToolFailure::unknown(ToolFailureKind::InvalidResult)
}

fn validate_declaration(
    declaration: &InMemoryPackageView,
    version: &agl_core::package::PackageVersion,
    content_digest: &agl_core::package::PackageTreeDigest,
    definition: &agl_core::ExtensionDefinition,
    extension: &str,
    tools: &[&str],
) -> anyhow::Result<()> {
    definition.validate().map_err(anyhow::Error::msg)?;
    #[derive(serde::Deserialize)]
    struct DeclarationVersion {
        version: agl_core::package::PackageVersion,
    }
    let document = declaration.read_file(&PackageRelativePath::new("EXTENSION.toml")?)?;
    let declared: DeclarationVersion = toml::from_str(std::str::from_utf8(&document)?)?;
    anyhow::ensure!(
        version == &declared.version,
        "native Extension version mismatch"
    );
    anyhow::ensure!(
        content_digest == &compute_package_digest(declaration)?,
        "native Extension package digest mismatch"
    );
    anyhow::ensure!(
        definition.id.as_str() == extension,
        "native Extension ID mismatch"
    );
    anyhow::ensure!(
        definition.tools.len() == tools.len()
            && tools
                .iter()
                .all(|id| definition.tools.iter().any(|tool| tool.id.as_str() == *id)),
        "native Extension Tool IDs mismatch"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agl_core::package::{PackageTreeDigest, PackageVersion};

    #[test]
    fn native_extension_uses_its_declared_version_and_exact_digest() {
        let declaration = searxng::declaration();
        let document = declaration
            .read_file(&PackageRelativePath::new("EXTENSION.toml").unwrap())
            .unwrap();
        let manifest: toml::Value =
            toml::from_str(std::str::from_utf8(&document).unwrap()).unwrap();
        let version = PackageVersion::new(manifest["version"].as_str().unwrap()).unwrap();
        assert_ne!(version.to_string(), env!("CARGO_PKG_VERSION"));
        let digest = compute_package_digest(&declaration).unwrap();
        let definition = agl_core::ExtensionDefinition {
            id: agl_core::ExtensionId::new("agentlibre.searxng").unwrap(),
            effects: vec![],
            tools: vec![],
        };
        validate_declaration(
            &declaration,
            &version,
            &digest,
            &definition,
            "agentlibre.searxng",
            &[],
        )
        .unwrap();
        let wrong_version = PackageVersion::new("99.0.0").unwrap();
        assert!(
            validate_declaration(
                &declaration,
                &wrong_version,
                &digest,
                &definition,
                "agentlibre.searxng",
                &[],
            )
            .unwrap_err()
            .to_string()
            .contains("version mismatch")
        );
        assert!(
            validate_declaration(
                &declaration,
                &version,
                &PackageTreeDigest::new(format!("sha256:{}", "0".repeat(64))).unwrap(),
                &definition,
                "agentlibre.searxng",
                &[],
            )
            .unwrap_err()
            .to_string()
            .contains("digest mismatch")
        );
    }
}
