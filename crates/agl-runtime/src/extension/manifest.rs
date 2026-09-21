use crate::package::{PackageId, PackageRelativePath, PackageVersion, PackageView};
use agl_core::agent::DeliveryClass;
use agl_core::{
    EffectDefinition, EffectId, ExtensionDefinition, ExtensionId, JsonSchema, ToolDefinition,
    ToolId,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const EXTENSION_FILE_NAME: &str = "EXTENSION.toml";
pub const EXTENSION_SCHEMA: &str = "agentlibre.extension/v1";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionEffectManifest {
    pub id: EffectId,
    pub description: String,
    pub scope_schema: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionToolManifest {
    pub id: ToolId,
    pub description: String,
    pub input_schema: String,
    #[serde(default)]
    pub required_effects: Vec<EffectId>,
    pub delivery: DeliveryClass,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionManifest {
    pub schema: String,
    pub id: PackageId,
    pub version: PackageVersion,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub effects: Vec<ExtensionEffectManifest>,
    #[serde(default)]
    pub tools: Vec<ExtensionToolManifest>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionPackage {
    pub manifest: ExtensionManifest,
    pub definition: ExtensionDefinition,
}

impl ExtensionPackage {
    pub fn parse(package: &impl PackageView) -> Result<Self> {
        let document = package.read_file(&PackageRelativePath::new(EXTENSION_FILE_NAME)?)?;
        let mut manifest: ExtensionManifest =
            toml::from_str(std::str::from_utf8(&document)?).context("invalid EXTENSION.toml")?;
        validate_manifest(&manifest)?;
        manifest
            .effects
            .sort_by(|left, right| left.id.cmp(&right.id));
        manifest.tools.sort_by(|left, right| left.id.cmp(&right.id));
        for tool in &mut manifest.tools {
            tool.required_effects.sort();
        }
        let effects = manifest
            .effects
            .iter()
            .map(|effect| {
                Ok(EffectDefinition {
                    id: effect.id.clone(),
                    description: effect.description.clone(),
                    scope_schema: read_schema(package, &effect.scope_schema)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let tools = manifest
            .tools
            .iter()
            .map(|tool| {
                Ok(ToolDefinition {
                    id: tool.id.clone(),
                    description: tool.description.clone(),
                    input_schema: read_schema(package, &tool.input_schema)?,
                    required_effects: tool.required_effects.clone(),
                    delivery: tool.delivery,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let definition = ExtensionDefinition {
            id: ExtensionId::new(manifest.id.to_string())?,
            effects,
            tools,
        };
        definition.validate().map_err(anyhow::Error::msg)?;
        Ok(Self {
            manifest,
            definition,
        })
    }
}

pub fn parse_package_view(package: &impl PackageView) -> Result<ExtensionPackage> {
    ExtensionPackage::parse(package)
}

fn validate_manifest(manifest: &ExtensionManifest) -> Result<()> {
    ensure!(
        manifest.schema == EXTENSION_SCHEMA,
        "unsupported Extension schema"
    );
    if let Some(description) = &manifest.description {
        ensure!(
            !description.trim().is_empty(),
            "Extension description cannot be empty when present"
        );
    }
    let owner = manifest.id.to_string();
    let mut effect_ids = manifest
        .effects
        .iter()
        .map(|value| value.id.clone())
        .collect::<Vec<_>>();
    effect_ids.sort();
    ensure!(
        effect_ids.windows(2).all(|pair| pair[0] != pair[1]),
        "Extension Effects contain duplicates"
    );
    let mut tool_ids = manifest
        .tools
        .iter()
        .map(|value| value.id.clone())
        .collect::<Vec<_>>();
    tool_ids.sort();
    ensure!(
        tool_ids.windows(2).all(|pair| pair[0] != pair[1]),
        "Extension Tools contain duplicates"
    );
    for effect in &manifest.effects {
        ensure!(
            !effect.description.trim().is_empty(),
            "Effect description cannot be empty"
        );
        ensure!(
            effect.id.as_str().starts_with(&format!("{owner}:")),
            "Effect ID is not owned by its Extension"
        );
        PackageRelativePath::new(effect.scope_schema.clone())?;
    }
    for tool in &manifest.tools {
        ensure!(
            !tool.description.trim().is_empty(),
            "Tool description cannot be empty"
        );
        ensure!(
            tool.id.as_str().starts_with(&format!("{owner}:")),
            "Tool ID is not owned by its Extension"
        );
        PackageRelativePath::new(tool.input_schema.clone())?;
        let mut required = tool.required_effects.clone();
        required.sort();
        ensure!(
            required.windows(2).all(|pair| pair[0] != pair[1]),
            "Tool required Effects contain duplicates"
        );
        ensure!(
            required
                .iter()
                .all(|effect| effect_ids.binary_search(effect).is_ok()),
            "Tool requires an Effect not declared by its Extension"
        );
    }
    Ok(())
}

fn read_schema(package: &impl PackageView, path: &str) -> Result<JsonSchema> {
    let path = PackageRelativePath::new(path)?;
    let bytes = package.read_file(&path)?;
    ensure!(bytes.len() <= 64 * 1024, "JSON Schema exceeds 64 KiB");
    JsonSchema::new(serde_json::from_slice(&bytes).context("invalid JSON Schema JSON")?)
        .map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use crate::package::{DirectoryPackageView, InMemoryPackageView};

    use super::*;

    #[test]
    fn direct_extension_manifest_loads_contained_schemas() {
        let package = InMemoryPackageView::new([
            (
                PackageRelativePath::new(EXTENSION_FILE_NAME).unwrap(),
                br#"schema = "agentlibre.extension/v1"
id = "test.extension"
version = "1.0.0"

[[effects]]
id = "test.extension:write"
description = "Write"
scope_schema = "schemas/scope.json"

[[tools]]
id = "test.extension:write"
description = "Write"
input_schema = "schemas/input.json"
required_effects = ["test.extension:write"]
delivery = "at_most_once"
"#
                .to_vec(),
            ),
            (
                PackageRelativePath::new("schemas/scope.json").unwrap(),
                br#"{"type":"object","additionalProperties":false}"#.to_vec(),
            ),
            (
                PackageRelativePath::new("schemas/input.json").unwrap(),
                br#"{"type":"object","additionalProperties":false}"#.to_vec(),
            ),
        ])
        .unwrap();
        let parsed = ExtensionPackage::parse(&package).unwrap();
        assert_eq!(parsed.manifest.id.to_string(), "test.extension");
        assert_eq!(parsed.definition.tools.len(), 1);
    }

    #[test]
    fn neutral_execution_contract_is_parseable_and_exact() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../extensions/agentlibre-execution");
        let package = ExtensionPackage::parse(&DirectoryPackageView::new(root).unwrap()).unwrap();
        assert_eq!(package.manifest.id.as_str(), "agentlibre.execution");
        assert_eq!(package.definition.effects.len(), 2);
        assert_eq!(package.definition.tools.len(), 2);
        assert!(package.definition.tools.iter().all(|tool| {
            tool.delivery == DeliveryClass::AtMostOnce && tool.required_effects.len() == 1
        }));
        let command = package
            .definition
            .tools
            .iter()
            .find(|tool| tool.id.as_str() == "agentlibre.execution:command.exec")
            .unwrap();
        assert!(
            command
                .input_schema
                .validate(
                    &agl_core::CanonicalJson::new(serde_json::json!({"argv":["cargo","test"]}))
                        .unwrap()
                )
                .is_ok()
        );
        assert!(
            command
                .input_schema
                .validate(
                    &agl_core::CanonicalJson::new(
                        serde_json::json!({"argv":["/bin/sh","-c","true"]})
                    )
                    .unwrap()
                )
                .is_err()
        );
        let scope = &package.definition.effects[0].scope_schema;
        assert!(
            scope
                .validate(
                    &agl_core::CanonicalJson::new(serde_json::json!({
                        "root":"workspace", "executables":["cargo","git"]
                    }))
                    .unwrap()
                )
                .is_ok()
        );
        assert!(
            scope
                .validate(
                    &agl_core::CanonicalJson::new(serde_json::json!({
                        "root":"workspace", "executables":["cargo","cargo"]
                    }))
                    .unwrap()
                )
                .is_err()
        );
    }
}
