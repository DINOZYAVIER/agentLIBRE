//! First-party filesystem Tool bindings.
//!
//! Registering these handlers does not admit them to an AgentRun. The admitted
//! snapshot must carry the exact Tool and Extension definition digests.

pub(crate) mod execution;
mod filesystem;
mod forge;
pub(crate) mod searxng;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use agl_core::Content;
use agl_core::ToolId;
use agl_core::agent::{
    EffectReceipt, ToolDefinition, ToolDefinitionDigest, ToolFailure, ToolFailureKind, ToolResult,
};
use agl_runtime::extension::{
    ExtensionBindings, ToolBinding, ToolContext, ToolFuture, ToolHandler, parse_package_view,
};
use agl_runtime::package::{InMemoryPackageView, PackageRelativePath, compute_package_digest};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

pub(crate) const COMMITTED_RESULT_LIMIT: &str = r#"{"kind":"tool_result_limit","effect":"committed","output":"omitted","next_actions":["Inspect the committed effect; do not repeat this operation."]}"#;

static WORKSPACE_MUTATIONS: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

/// Return the process-local mutation lock for one canonical workspace root.
/// Memory commits and `fs_apply_patch` deliberately share this registry so a
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

pub(crate) fn filesystem_bindings() -> ExtensionBindings {
    let declaration = declaration_view();
    let package = parse_package_view(&declaration).expect("embedded Extension declaration");
    assert_eq!(
        package.manifest.version.to_string(),
        env!("CARGO_PKG_VERSION")
    );
    let mutation = Arc::new(Mutex::new(()));
    let tools = package
        .definition
        .tools
        .iter()
        .map(|definition| {
            let tool_id = definition.id.clone();
            let handler: Arc<dyn ToolHandler> = if tool_id.as_str() == forge::READ {
                Arc::new(ForgeHandler)
            } else {
                Arc::new(FilesystemHandler {
                    tool: tool_id.clone(),
                    mutation: Arc::clone(&mutation),
                })
            };
            ToolBinding {
                tool_id,
                definition_digest: definition_digest(definition),
                handler,
            }
        })
        .collect();
    ExtensionBindings {
        version: package.manifest.version,
        content_digest: compute_package_digest(&declaration)
            .expect("embedded Extension package digest"),
        definition: package.definition,
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
    }
}

fn declaration_view() -> InMemoryPackageView {
    InMemoryPackageView::new([
        embedded(
            "EXTENSION.toml",
            include_bytes!("../../../../../extensions/agentlibre-builtins/EXTENSION.toml"),
        ),
        embedded(
            "schemas/filesystem-write-scope.json",
            include_bytes!(
                "../../../../../extensions/agentlibre-builtins/schemas/filesystem-write-scope.json"
            ),
        ),
        embedded(
            "schemas/fs-read.json",
            include_bytes!("../../../../../extensions/agentlibre-builtins/schemas/fs-read.json"),
        ),
        embedded(
            "schemas/fs-apply-patch.json",
            include_bytes!(
                "../../../../../extensions/agentlibre-builtins/schemas/fs-apply-patch.json"
            ),
        ),
        embedded(
            "schemas/forge-read.json",
            include_bytes!("../../../../../extensions/agentlibre-builtins/schemas/forge-read.json"),
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
    mutation: Arc<Mutex<()>>,
}

struct ForgeHandler;

impl ToolHandler for ForgeHandler {
    fn call(&self, context: ToolContext, input: Value) -> ToolFuture {
        Box::pin(async move {
            let value = forge::execute(&context, input)?;
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
        let mutation = Arc::clone(&self.mutation);
        Box::pin(async move {
            let (value, effect_receipts) =
                filesystem::execute(tool.as_str(), &context, input, &mutation)?;
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

fn execution() -> ToolFailure {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_declarations_and_bindings_are_exact() {
        let bindings = filesystem_bindings();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../extensions/agentlibre-builtins");
        let declaration = agl_runtime::package::DirectoryPackageView::new(root).unwrap();
        let package = parse_package_view(&declaration).unwrap();
        bindings.definition.validate().unwrap();
        assert_eq!(bindings.version, package.manifest.version);
        assert_eq!(bindings.definition, package.definition);
        assert_eq!(
            bindings.content_digest,
            compute_package_digest(&declaration).unwrap()
        );
        assert_eq!(bindings.definition.tools.len(), 3);
        assert_eq!(bindings.definition.tools.len(), bindings.tools.len());
        assert!(bindings.definition.tools.iter().all(|definition| {
            bindings.tools.iter().any(|binding| {
                binding.tool_id == definition.id
                    && binding.definition_digest == definition_digest(definition)
            })
        }));
        assert!(
            bindings
                .tools
                .iter()
                .any(|binding| binding.tool_id.as_str() == forge::READ)
        );

        let absolute = agl_core::AuthorityGrant {
            effect: agl_core::EffectId::new(filesystem::WRITE_EFFECT).unwrap(),
            scope: agl_core::CanonicalJson::new(serde_json::json!({"root":"/workspace"})).unwrap(),
        };
        let relative = agl_core::AuthorityGrant {
            effect: absolute.effect.clone(),
            scope: agl_core::CanonicalJson::new(serde_json::json!({"root":"workspace"})).unwrap(),
        };
        assert!((bindings.allows_authority)(&absolute));
        assert!(!(bindings.allows_authority)(&relative));
    }
}
