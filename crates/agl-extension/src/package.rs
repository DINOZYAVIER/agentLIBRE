use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use agl_kernel::{
    ArtifactDeclaration, DeclarationDigest, EffectDeclaration, ExtensionId, ExtensionRequirement,
    ExtensionWorkflowFragment, HookDeclaration, HostBindingRequirement, ToolDeclaration,
};
use agl_package::{
    AglCompatibility, ArtifactAdapter, ArtifactAdapterDescriptor, ArtifactEntrypoint,
    ArtifactEnvelope, ArtifactError, ArtifactPackageId, ArtifactPackageRef, ArtifactPackageView,
    ArtifactRelativePath, ArtifactRequirement, ArtifactSchemaId, ArtifactTypeId, ArtifactVersion,
    ArtifactVersionReq, ErasedArtifactPayload, InMemoryPackageView, PackageTreeDigest,
    compute_package_digest,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ExtensionDefinition, ExtensionDefinitionBuilder};

pub const EXTENSION_ROOT_SCHEMA: &str = "agentlibre.extension-root/v1";
pub const TOOL_INDEX_SCHEMA: &str = "agentlibre.extension-tool-index/v1";
pub const HOOK_INDEX_SCHEMA: &str = "agentlibre.extension-hook-index/v1";
pub const EFFECT_INDEX_SCHEMA: &str = "agentlibre.extension-effect-index/v1";
pub const ARTIFACT_INDEX_SCHEMA: &str = "agentlibre.extension-artifact-index/v1";

#[derive(Clone, Debug)]
pub struct ExtensionPackageBuildInput {
    definition: ExtensionDefinition,
    source: PathBuf,
}

impl ExtensionPackageBuildInput {
    pub fn new(
        definition: ExtensionDefinition,
        source: impl Into<PathBuf>,
    ) -> Result<Self, ExtensionPackageError> {
        let source = source.into();
        let metadata =
            fs::symlink_metadata(&source).map_err(|error| ExtensionPackageError::Io {
                path: source.clone(),
                reason: error.to_string(),
            })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ExtensionPackageError::Io {
                path: source,
                reason: "source must be a real directory".to_owned(),
            });
        }
        Ok(Self { definition, source })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionPackageReport {
    pub extension_id: ExtensionId,
    pub declaration_digest: DeclarationDigest,
    pub package_tree_digest: PackageTreeDigest,
    pub files: Vec<ArtifactRelativePath>,
}

pub struct ExtensionPackageBuilder;

impl ExtensionPackageBuilder {
    pub fn build(
        input: &ExtensionPackageBuildInput,
        output: impl AsRef<Path>,
    ) -> Result<ExtensionPackageReport, ExtensionPackageError> {
        let output = output.as_ref();
        fs::create_dir_all(output).map_err(|error| ExtensionPackageError::Io {
            path: output.to_path_buf(),
            reason: error.to_string(),
        })?;
        if fs::read_dir(output)
            .map_err(|error| ExtensionPackageError::Io {
                path: output.to_path_buf(),
                reason: error.to_string(),
            })?
            .next()
            .is_some()
        {
            return Err(ExtensionPackageError::OutputNotEmpty {
                path: output.to_path_buf(),
            });
        }

        let mut files = generated_files(&input.definition)?;
        collect_source_files(&input.source, &input.source, &mut files)?;
        for (path, bytes) in &files {
            let target = output.join(path.as_str());
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| ExtensionPackageError::Io {
                    path: parent.to_path_buf(),
                    reason: error.to_string(),
                })?;
            }
            fs::write(&target, bytes).map_err(|error| ExtensionPackageError::Io {
                path: target,
                reason: error.to_string(),
            })?;
        }
        let view = InMemoryPackageView::new(files)?;
        report(&input.definition, &view)
    }

    pub fn build_to_memory(
        definition: ExtensionDefinition,
    ) -> Result<ExtensionPackage, ExtensionPackageError> {
        let files = generated_files(&definition)?;
        let view = InMemoryPackageView::new(files)?;
        ExtensionPackage::parse(&view)
    }
}

#[derive(Clone, Debug)]
pub struct ExtensionPackage {
    definition: Option<ExtensionDefinition>,
    tool_ids: Vec<String>,
    package_tree_digest: PackageTreeDigest,
}

impl ExtensionPackage {
    pub fn parse(package: &dyn ArtifactPackageView) -> Result<Self, ExtensionPackageError> {
        let root_path: ArtifactRelativePath = "extension-root.json".parse()?;
        let root_bytes =
            package
                .read_file(&root_path)
                .map_err(|error| ExtensionPackageError::Root {
                    path: root_path.clone(),
                    reason: error.to_string(),
                })?;
        let root: RootWire =
            serde_json::from_slice(&root_bytes).map_err(|error| ExtensionPackageError::Root {
                path: root_path.clone(),
                reason: error.to_string(),
            })?;
        if root.schema != EXTENSION_ROOT_SCHEMA {
            return Err(ExtensionPackageError::Root {
                path: root_path,
                reason: format!("unknown schema `{}`", root.schema),
            });
        }

        let kinds = [
            IndexKind::new("tools", TOOL_INDEX_SCHEMA, &root.indexes.tools),
            IndexKind::new("hooks", HOOK_INDEX_SCHEMA, &root.indexes.hooks),
            IndexKind::new("effects", EFFECT_INDEX_SCHEMA, &root.indexes.effects),
            IndexKind::new("artifacts", ARTIFACT_INDEX_SCHEMA, &root.indexes.artifacts),
        ];
        let mut declarations = BTreeMap::<String, Vec<Value>>::new();
        let mut listed_paths = BTreeSet::new();
        for kind in kinds {
            let values = parse_index(package, &kind, &mut listed_paths)?;
            declarations.insert(kind.directory.to_owned(), values);
        }
        for path in package.files()? {
            let value = path.as_str();
            if ["tools/", "hooks/", "effects/", "artifacts/"]
                .iter()
                .any(|prefix| value.starts_with(prefix))
                && !listed_paths.contains(&path)
            {
                let bytes = package.read_file(&path)?;
                let id = serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_default();
                return Err(ExtensionPackageError::UnlistedDeclaration { path, id });
            }
        }

        let definition = root.into_definition(&declarations)?;
        let tool_ids = declarations["tools"]
            .iter()
            .filter_map(|value| value.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        Ok(Self {
            definition,
            tool_ids,
            package_tree_digest: compute_package_digest(package)?,
        })
    }

    pub fn definition(&self) -> Result<&ExtensionDefinition, ExtensionPackageError> {
        self.definition
            .as_ref()
            .ok_or(ExtensionPackageError::IncompleteRoot)
    }

    pub fn tool_ids(&self) -> impl Iterator<Item = &str> {
        self.tool_ids.iter().map(String::as_str)
    }

    pub fn package_tree_digest(&self) -> &PackageTreeDigest {
        &self.package_tree_digest
    }
}

#[derive(Clone, Debug)]
pub struct ExtensionPackageAdapter {
    descriptor: ArtifactAdapterDescriptor,
}

impl ExtensionPackageAdapter {
    pub fn new() -> Result<Self, ArtifactError> {
        Ok(Self {
            descriptor: ArtifactAdapterDescriptor::new(
                ArtifactTypeId::extension(),
                agl_package::EXTENSION_ROOT,
                ArtifactEntrypoint::new("extension-root.json")?,
            )?,
        })
    }

    fn parse(&self, package: &dyn ArtifactPackageView) -> Result<ExtensionPackage, ArtifactError> {
        ExtensionPackage::parse(package).map_err(|error| ArtifactError::AdapterPayload {
            type_id: agl_package::EXTENSION_TYPE.to_owned(),
            reason: error.to_string(),
        })
    }

    fn envelope(&self, package: &ExtensionPackage) -> Result<ArtifactEnvelope, ArtifactError> {
        let definition = package
            .definition()
            .map_err(|error| ArtifactError::AdapterPayload {
                type_id: agl_package::EXTENSION_TYPE.to_owned(),
                reason: error.to_string(),
            })?;
        let current = ArtifactVersion::new(env!("CARGO_PKG_VERSION"))?;
        let compatibility = AglCompatibility::new(
            ArtifactVersionReq::new(format!(">={}", env!("CARGO_PKG_VERSION")))?,
            [current],
        )?;
        let requirements = definition
            .descriptor()
            .requirements
            .iter()
            .map(|requirement| {
                ArtifactPackageRef::parse(&format!(
                    "extension:{}@^{}.0",
                    requirement.extension_id, requirement.api_major
                ))
                .map(ArtifactRequirement::new)
            })
            .collect::<Result<Vec<_>, _>>()?;
        ArtifactEnvelope::new(
            ArtifactTypeId::extension(),
            ArtifactPackageId::new(definition.id.as_str())?,
            ArtifactVersion::new(&definition.version)?,
            ArtifactSchemaId::new(EXTENSION_ROOT_SCHEMA)?,
            compatibility,
            requirements,
        )
    }
}

impl Default for ExtensionPackageAdapter {
    fn default() -> Self {
        Self::new().expect("Extension package adapter descriptor is valid")
    }
}

impl ArtifactAdapter for ExtensionPackageAdapter {
    fn descriptor(&self) -> &ArtifactAdapterDescriptor {
        &self.descriptor
    }

    fn extract_envelope(
        &self,
        package: &dyn ArtifactPackageView,
    ) -> Result<ArtifactEnvelope, ArtifactError> {
        let package = self.parse(package)?;
        self.envelope(&package)
    }

    fn validate_payload(
        &self,
        package: &dyn ArtifactPackageView,
        envelope: &ArtifactEnvelope,
    ) -> Result<ErasedArtifactPayload, ArtifactError> {
        let package = self.parse(package)?;
        let actual = self.envelope(&package)?;
        if &actual != envelope {
            return Err(ArtifactError::AdapterPayload {
                type_id: agl_package::EXTENSION_TYPE.to_owned(),
                reason: "extension-root.json envelope changed during validation".to_owned(),
            });
        }
        Ok(Box::new(package))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootWire {
    schema: String,
    #[serde(default)]
    id: Option<ExtensionId>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    api_major: Option<u32>,
    #[serde(default)]
    declaration_digest: Option<DeclarationDigest>,
    #[serde(default)]
    requirements: Vec<ExtensionRequirement>,
    #[serde(default)]
    host_bindings: Vec<HostBindingRequirement>,
    #[serde(default)]
    workflow: Option<ExtensionWorkflowFragment>,
    indexes: IndexPaths,
}

impl RootWire {
    fn from_definition(definition: &ExtensionDefinition) -> Self {
        Self {
            schema: EXTENSION_ROOT_SCHEMA.to_owned(),
            id: Some(definition.id.clone()),
            name: Some(definition.name.clone()),
            version: Some(definition.version.clone()),
            api_major: Some(definition.api_major),
            declaration_digest: Some(definition.digest()),
            requirements: definition.descriptor().requirements.clone(),
            host_bindings: definition.descriptor().host_bindings.clone(),
            workflow: definition.descriptor().workflow.clone(),
            indexes: IndexPaths::default(),
        }
    }

    fn into_definition(
        self,
        declarations: &BTreeMap<String, Vec<Value>>,
    ) -> Result<Option<ExtensionDefinition>, ExtensionPackageError> {
        let (Some(id), Some(name), Some(version), Some(api_major), Some(expected_digest)) = (
            self.id,
            self.name,
            self.version,
            self.api_major,
            self.declaration_digest,
        ) else {
            return Ok(None);
        };
        let mut builder = ExtensionDefinition::builder(id, name, version, api_major);
        for requirement in self.requirements {
            builder = builder.require_extension(requirement.extension_id, requirement.api_major);
        }
        for requirement in self.host_bindings {
            builder = builder.require_host_binding(requirement.id, requirement.api_major);
        }
        if let Some(workflow) = self.workflow {
            builder = builder.workflow(workflow);
        }
        builder = add_declarations(builder, declarations)?;
        let definition = builder
            .build()
            .map_err(|error| ExtensionPackageError::Definition(error.to_string()))?;
        if definition.digest() != expected_digest {
            return Err(ExtensionPackageError::DeclarationDigestMismatch {
                expected: expected_digest,
                actual: definition.digest(),
            });
        }
        Ok(Some(definition))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexPaths {
    tools: String,
    hooks: String,
    effects: String,
    artifacts: String,
}

impl Default for IndexPaths {
    fn default() -> Self {
        Self {
            tools: "indexes/tools.json".to_owned(),
            hooks: "indexes/hooks.json".to_owned(),
            effects: "indexes/effects.json".to_owned(),
            artifacts: "indexes/artifacts.json".to_owned(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexWire {
    schema: String,
    entries: Vec<IndexEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexEntry {
    id: String,
    path: String,
}

struct IndexKind<'a> {
    directory: &'static str,
    schema: &'static str,
    index_path: &'a str,
}

impl<'a> IndexKind<'a> {
    fn new(directory: &'static str, schema: &'static str, index_path: &'a str) -> Self {
        Self {
            directory,
            schema,
            index_path,
        }
    }
}

fn parse_index(
    package: &dyn ArtifactPackageView,
    kind: &IndexKind<'_>,
    listed_paths: &mut BTreeSet<ArtifactRelativePath>,
) -> Result<Vec<Value>, ExtensionPackageError> {
    let index_path = kind
        .index_path
        .parse::<ArtifactRelativePath>()
        .map_err(|error| ExtensionPackageError::Index {
            path: "extension-root.json"
                .parse()
                .expect("constant path is safe"),
            reason: error.to_string(),
        })?;
    let bytes = package
        .read_file(&index_path)
        .map_err(|error| ExtensionPackageError::Index {
            path: index_path.clone(),
            reason: error.to_string(),
        })?;
    let index: IndexWire =
        serde_json::from_slice(&bytes).map_err(|error| ExtensionPackageError::Index {
            path: index_path.clone(),
            reason: error.to_string(),
        })?;
    if index.schema != kind.schema {
        return Err(ExtensionPackageError::Index {
            path: index_path,
            reason: format!("unknown schema `{}`", index.schema),
        });
    }
    let sorted = index
        .entries
        .iter()
        .map(|entry| (&entry.id, &entry.path))
        .collect::<Vec<_>>();
    let mut canonical = sorted.clone();
    canonical.sort();
    if sorted != canonical {
        return Err(ExtensionPackageError::Index {
            path: index_path,
            reason: "entries are not sorted by id/path".to_owned(),
        });
    }
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut validated_entries = Vec::new();
    for entry in index.entries {
        if !ids.insert(entry.id.clone()) {
            return Err(ExtensionPackageError::Index {
                path: index_path.clone(),
                reason: format!("duplicate ID `{}`", entry.id),
            });
        }
        let path = entry
            .path
            .parse::<ArtifactRelativePath>()
            .map_err(|error| ExtensionPackageError::Index {
                path: index_path.clone(),
                reason: error.to_string(),
            })?;
        if !path.as_str().starts_with(&format!("{}/", kind.directory)) {
            return Err(ExtensionPackageError::Index {
                path: index_path.clone(),
                reason: format!("declaration path `{path}` is outside {}/", kind.directory),
            });
        }
        if !paths.insert(path.clone()) {
            return Err(ExtensionPackageError::Index {
                path: index_path.clone(),
                reason: format!("duplicate path `{path}`"),
            });
        }
        validated_entries.push((entry, path));
    }
    let mut declarations = Vec::new();
    for (entry, path) in validated_entries {
        let bytes =
            package
                .read_file(&path)
                .map_err(|_| ExtensionPackageError::MissingDeclaration {
                    path: path.clone(),
                    id: entry.id.clone(),
                })?;
        let declaration: Value =
            serde_json::from_slice(&bytes).map_err(|error| ExtensionPackageError::Declaration {
                path: path.clone(),
                reason: error.to_string(),
            })?;
        let actual_id = declaration
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if actual_id != entry.id {
            return Err(ExtensionPackageError::DeclarationIdMismatch {
                path,
                expected_id: entry.id,
                actual_id: actual_id.to_owned(),
            });
        }
        listed_paths.insert(path);
        declarations.push(declaration);
    }
    Ok(declarations)
}

fn add_declarations(
    mut builder: ExtensionDefinitionBuilder,
    declarations: &BTreeMap<String, Vec<Value>>,
) -> Result<ExtensionDefinitionBuilder, ExtensionPackageError> {
    for value in &declarations["hooks"] {
        builder = builder.hook(
            serde_json::from_value::<HookDeclaration>(value.clone())
                .map_err(|error| ExtensionPackageError::Definition(error.to_string()))?,
        );
    }
    for value in &declarations["effects"] {
        builder = builder.effect(
            serde_json::from_value::<EffectDeclaration>(value.clone())
                .map_err(|error| ExtensionPackageError::Definition(error.to_string()))?,
        );
    }
    for value in &declarations["tools"] {
        builder = builder.tool(
            serde_json::from_value::<ToolDeclaration>(value.clone())
                .map_err(|error| ExtensionPackageError::Definition(error.to_string()))?,
        );
    }
    for value in &declarations["artifacts"] {
        builder = builder.artifact(
            serde_json::from_value::<ArtifactDeclaration>(value.clone())
                .map_err(|error| ExtensionPackageError::Definition(error.to_string()))?,
        );
    }
    Ok(builder)
}

fn generated_files(
    definition: &ExtensionDefinition,
) -> Result<BTreeMap<ArtifactRelativePath, Vec<u8>>, ExtensionPackageError> {
    let descriptor = definition.descriptor();
    let categories = [
        (
            "tools",
            TOOL_INDEX_SCHEMA,
            descriptor
                .tools
                .iter()
                .map(|value| (value.id.to_string(), serde_json::to_value(value).unwrap()))
                .collect::<Vec<_>>(),
        ),
        (
            "hooks",
            HOOK_INDEX_SCHEMA,
            descriptor
                .hooks
                .iter()
                .map(|value| (value.id.to_string(), serde_json::to_value(value).unwrap()))
                .collect::<Vec<_>>(),
        ),
        (
            "effects",
            EFFECT_INDEX_SCHEMA,
            descriptor
                .effects
                .iter()
                .map(|value| (value.id.to_string(), serde_json::to_value(value).unwrap()))
                .collect::<Vec<_>>(),
        ),
        (
            "artifacts",
            ARTIFACT_INDEX_SCHEMA,
            descriptor
                .artifacts
                .iter()
                .map(|value| (value.id.to_string(), serde_json::to_value(value).unwrap()))
                .collect::<Vec<_>>(),
        ),
    ];
    let mut files = BTreeMap::new();
    files.insert(
        "extension-root.json".parse()?,
        json_bytes(&RootWire::from_definition(definition))?,
    );
    for (directory, schema, mut values) in categories {
        values.sort_by(|left, right| left.0.cmp(&right.0));
        let mut entries = Vec::new();
        for (id, value) in values {
            let local = id.replace(':', "--");
            let path = format!("{directory}/{local}.json");
            entries.push(IndexEntry {
                id,
                path: path.clone(),
            });
            files.insert(path.parse()?, json_value_bytes(&value)?);
        }
        files.insert(
            format!("indexes/{directory}.json").parse()?,
            json_bytes(&IndexWire {
                schema: schema.to_owned(),
                entries,
            })?,
        );
    }
    Ok(files)
}

fn collect_source_files(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<ArtifactRelativePath, Vec<u8>>,
) -> Result<(), ExtensionPackageError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| ExtensionPackageError::Io {
            path: directory.to_path_buf(),
            reason: error.to_string(),
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ExtensionPackageError::Io {
            path: directory.to_path_buf(),
            reason: error.to_string(),
        })?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| ExtensionPackageError::Io {
            path: path.clone(),
            reason: error.to_string(),
        })?;
        if metadata.file_type().is_symlink() {
            return Err(ExtensionPackageError::Io {
                path,
                reason: "source symlinks are not package inputs".to_owned(),
            });
        }
        if metadata.is_dir() {
            collect_source_files(root, &path, files)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .expect("walked source path is below root")
                .to_string_lossy()
                .replace('\\', "/")
                .parse::<ArtifactRelativePath>()?;
            if files.contains_key(&relative) {
                return Err(ExtensionPackageError::GeneratedPathCollision { path: relative });
            }
            files.insert(
                relative,
                fs::read(&path).map_err(|error| ExtensionPackageError::Io {
                    path,
                    reason: error.to_string(),
                })?,
            );
        }
    }
    Ok(())
}

fn json_bytes(value: &impl Serialize) -> Result<Vec<u8>, ExtensionPackageError> {
    json_value_bytes(
        &serde_json::to_value(value)
            .map_err(|error| ExtensionPackageError::Definition(error.to_string()))?,
    )
}

fn json_value_bytes(value: &Value) -> Result<Vec<u8>, ExtensionPackageError> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| ExtensionPackageError::Definition(error.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn report(
    definition: &ExtensionDefinition,
    package: &dyn ArtifactPackageView,
) -> Result<ExtensionPackageReport, ExtensionPackageError> {
    Ok(ExtensionPackageReport {
        extension_id: definition.id.clone(),
        declaration_digest: definition.digest(),
        package_tree_digest: compute_package_digest(package)?,
        files: package.files()?,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ExtensionPackageError {
    #[error("Extension package root `{path}` is invalid: {reason}")]
    Root {
        path: ArtifactRelativePath,
        reason: String,
    },
    #[error("Extension index `{path}` is invalid: {reason}")]
    Index {
        path: ArtifactRelativePath,
        reason: String,
    },
    #[error("Extension declaration `{path}` for `{id}` is missing")]
    MissingDeclaration {
        path: ArtifactRelativePath,
        id: String,
    },
    #[error(
        "Extension declaration `{path}` ID mismatch: expected `{expected_id}`, got `{actual_id}`"
    )]
    DeclarationIdMismatch {
        path: ArtifactRelativePath,
        expected_id: String,
        actual_id: String,
    },
    #[error("Extension declaration `{path}` for `{id}` is not listed")]
    UnlistedDeclaration {
        path: ArtifactRelativePath,
        id: String,
    },
    #[error("Extension declaration `{path}` is invalid: {reason}")]
    Declaration {
        path: ArtifactRelativePath,
        reason: String,
    },
    #[error("Extension package root lacks authored identity fields")]
    IncompleteRoot,
    #[error("Extension definition is invalid: {0}")]
    Definition(String),
    #[error("Extension declaration digest mismatch: expected {expected}, got {actual}")]
    DeclarationDigestMismatch {
        expected: DeclarationDigest,
        actual: DeclarationDigest,
    },
    #[error("Extension source/output I/O failed at `{path}`: {reason}")]
    Io { path: PathBuf, reason: String },
    #[error("Extension output directory is not empty: `{path}`")]
    OutputNotEmpty { path: PathBuf },
    #[error("authored source collides with generated package path `{path}`")]
    GeneratedPathCollision { path: ArtifactRelativePath },
    #[error(transparent)]
    Package(#[from] ArtifactError),
}
