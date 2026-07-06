use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::str;

use agl_assets::{BuiltinAsset, BuiltinSkill};
use agl_tools::{HookId, SkillId, ToolId, ToolOperationKind, ToolStateEffect};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    Builtin,
    Core,
    Community,
    Local,
    Workspace,
    User,
    ThirdParty,
}

impl SkillSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Core => "core",
            Self::Community => "community",
            Self::Local => "local",
            Self::Workspace => "workspace",
            Self::User => "user",
            Self::ThirdParty => "third_party",
        }
    }

    pub fn is_external_skill_source(self) -> bool {
        matches!(
            self,
            Self::Core | Self::Community | Self::Local | Self::Workspace
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillReference {
    pub path: String,
    pub sha256: String,
    pub content: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillReferencePolicy {
    pub include: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillHarness {
    pub id: SkillId,
    pub name: String,
    pub description: String,
    pub version: u64,
    pub source: SkillSource,
    pub pack: String,
    pub required_hooks: Vec<HookId>,
    pub allowed_tools: Vec<ToolId>,
    pub requestable_tools: Vec<ToolId>,
    pub denied_tools: Vec<ToolId>,
    pub permission_request_templates: Vec<SkillPermissionRequestTemplate>,
    pub permissions: SkillPermissions,
    pub context_budget_tokens: u32,
    pub reference_policy: SkillReferencePolicy,
    pub references: Vec<SkillReference>,
    pub artifacts: Vec<SkillArtifactDeclaration>,
    pub guarantees: Vec<String>,
    pub body: String,
    pub source_path: String,
    pub manifest_sha256: String,
    pub tree_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillArtifactDeclaration {
    pub id: String,
    pub kind: SkillArtifactKind,
    pub path: PathBuf,
    pub access: SkillArtifactAccess,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub create: Vec<SkillFolderCreateRule>,
    #[serde(default)]
    pub provides: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillFolderCreateRule {
    pub when: SkillFolderCreateSituation,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillFolderCreateSituation {
    #[default]
    SkillSync,
    RuntimePrepare,
    ArtifactWrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillArtifactKind {
    Source,
    Generated,
    State,
    Cache,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillArtifactAccess {
    Read,
    Write,
    ReadWrite,
}

impl SkillHarness {
    pub fn parse_builtin(skill: &'static BuiltinSkill) -> Result<Self, SkillManifestError> {
        let text = skill
            .skill_md
            .text()
            .map_err(|_| SkillManifestError::InvalidUtf8 {
                source_path: skill.skill_md.source_path.to_string(),
            })?;
        parse_skill_text(
            skill.id,
            skill.pack,
            skill.skill_md,
            skill.references,
            skill.tree_sha256,
            text,
        )
    }

    pub fn parse_workspace_dir(
        skill_dir: impl AsRef<Path>,
        component_root: impl AsRef<Path>,
        tree_sha256: &str,
    ) -> Result<Self, SkillManifestError> {
        let skill_dir = skill_dir.as_ref();
        let component_root = component_root.as_ref();
        let manifest_path = skill_dir.join("SKILL.md");
        let source_path = relative_source_path(&manifest_path, component_root);
        let bytes = fs::read(&manifest_path).map_err(|err| SkillManifestError::ReadManifest {
            source_path: source_path.clone(),
            message: err.to_string(),
        })?;
        let text = str::from_utf8(&bytes).map_err(|_| SkillManifestError::InvalidUtf8 {
            source_path: source_path.clone(),
        })?;
        parse_workspace_text(
            skill_dir,
            component_root,
            &source_path,
            &bytes,
            tree_sha256,
            text,
        )
    }

    pub fn is_trusted_source(&self) -> bool {
        self.source == SkillSource::Builtin
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillPermissionRequestTemplate {
    pub id: String,
    pub tools: Vec<ToolId>,
    #[serde(default)]
    pub max_operation_kind: Option<ToolOperationKind>,
    #[serde(default)]
    pub state_effects: Vec<ToolStateEffect>,
    pub default_duration: String,
    pub reason_template: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillPermissions {
    #[serde(default)]
    pub memory: SkillMemoryPermissions,
    #[serde(default)]
    pub notes: SkillNotesPermissions,
}

impl SkillPermissions {
    pub fn memory_read_scopes(&self) -> Vec<&'static str> {
        self.memory
            .read
            .iter()
            .map(|scope| scope.as_str())
            .collect()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillMemoryPermissions {
    #[serde(default)]
    pub read: Vec<MemoryPermissionScope>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPermissionScope {
    User,
    Repo,
    MatrixRoom,
    MatrixUser,
}

impl MemoryPermissionScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Repo => "repo",
            Self::MatrixRoom => "matrix_room",
            Self::MatrixUser => "matrix_user",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillNotesPermissions {
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub write: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSkillManifest {
    name: String,
    description: String,
    version: u64,
    source: SkillSource,
    pack: String,
    required_hooks: Vec<HookId>,
    allowed_tools: Vec<ToolId>,
    #[serde(default)]
    requestable_tools: Vec<ToolId>,
    #[serde(default)]
    denied_tools: Vec<ToolId>,
    #[serde(default, alias = "permission_requests")]
    permission_request_templates: Vec<SkillPermissionRequestTemplate>,
    #[serde(default)]
    permissions: SkillPermissions,
    context_budget_tokens: u32,
    references: RawReferencePolicy,
    #[serde(default, alias = "artifact_folders", alias = "folders")]
    artifacts: Vec<SkillArtifactDeclaration>,
    guarantees: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReferencePolicy {
    include: Vec<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum SkillManifestError {
    InvalidUtf8 {
        source_path: String,
    },
    ReadManifest {
        source_path: String,
        message: String,
    },
    MissingFrontmatter {
        source_path: String,
    },
    UnterminatedFrontmatter {
        source_path: String,
    },
    InvalidYaml {
        source_path: String,
        message: String,
    },
    BlankField {
        field: &'static str,
    },
    EmptyList {
        field: &'static str,
    },
    DuplicateValue {
        field: &'static str,
        value: String,
    },
    ConflictingToolRouting {
        tool: String,
        first_field: &'static str,
        second_field: &'static str,
    },
    TemplateToolNotRequestable {
        template_id: String,
        tool: String,
    },
    InvalidReferencePath {
        path: String,
    },
    MissingReference {
        path: String,
    },
    InvalidReferenceUtf8 {
        path: String,
    },
    BuiltinIdentityMismatch {
        expected: String,
        actual: String,
    },
    BuiltinSourceMismatch,
    WorkspaceSourceMismatch,
    ContextBudgetZero,
    EmptyBody,
    ReferenceEscapesSkill {
        path: String,
    },
    InvalidArtifactPath {
        path: String,
    },
}

impl std::fmt::Display for SkillManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUtf8 { source_path } => {
                write!(f, "skill manifest is not valid UTF-8: {source_path}")
            }
            Self::ReadManifest {
                source_path,
                message,
            } => write!(f, "failed to read skill manifest {source_path}: {message}"),
            Self::MissingFrontmatter { source_path } => {
                write!(
                    f,
                    "skill manifest is missing YAML frontmatter: {source_path}"
                )
            }
            Self::UnterminatedFrontmatter { source_path } => {
                write!(
                    f,
                    "skill manifest frontmatter is not terminated: {source_path}"
                )
            }
            Self::InvalidYaml {
                source_path,
                message,
            } => write!(
                f,
                "skill manifest YAML is invalid in {source_path}: {message}"
            ),
            Self::BlankField { field } => write!(f, "skill manifest field `{field}` is blank"),
            Self::EmptyList { field } => write!(f, "skill manifest list `{field}` is empty"),
            Self::DuplicateValue { field, value } => {
                write!(
                    f,
                    "skill manifest field `{field}` has duplicate value `{value}`"
                )
            }
            Self::ConflictingToolRouting {
                tool,
                first_field,
                second_field,
            } => write!(
                f,
                "skill manifest tool `{tool}` appears in both `{first_field}` and `{second_field}`"
            ),
            Self::TemplateToolNotRequestable { template_id, tool } => write!(
                f,
                "skill permission request template `{template_id}` references non-requestable tool `{tool}`"
            ),
            Self::InvalidReferencePath { path } => {
                write!(f, "skill manifest reference path is invalid: {path}")
            }
            Self::MissingReference { path } => {
                write!(
                    f,
                    "skill manifest includes missing builtin reference: {path}"
                )
            }
            Self::InvalidReferenceUtf8 { path } => {
                write!(f, "skill reference is not valid UTF-8: {path}")
            }
            Self::BuiltinIdentityMismatch { expected, actual } => {
                write!(
                    f,
                    "builtin skill identity mismatch: expected {expected}, got {actual}"
                )
            }
            Self::BuiltinSourceMismatch => {
                write!(f, "builtin skill manifest must use source=builtin")
            }
            Self::WorkspaceSourceMismatch => write!(
                f,
                "external skill manifest must use source=workspace, core, community, or local"
            ),
            Self::ContextBudgetZero => write!(f, "skill context budget must be greater than zero"),
            Self::EmptyBody => write!(f, "skill body cannot be empty"),
            Self::ReferenceEscapesSkill { path } => {
                write!(f, "skill reference escapes the skill directory: {path}")
            }
            Self::InvalidArtifactPath { path } => {
                write!(f, "skill artifact path is invalid: {path}")
            }
        }
    }
}

impl std::error::Error for SkillManifestError {}

fn parse_skill_text(
    expected_id: &str,
    expected_pack: &str,
    manifest_asset: &BuiltinAsset,
    reference_assets: &[&'static BuiltinAsset],
    tree_sha256: &str,
    text: &str,
) -> Result<SkillHarness, SkillManifestError> {
    let (mut raw, body) = parse_manifest_text(manifest_asset.source_path, text)?;
    let actual_id = raw.name.clone();
    if expected_id != actual_id {
        return Err(SkillManifestError::BuiltinIdentityMismatch {
            expected: expected_id.to_string(),
            actual: actual_id,
        });
    }
    if expected_pack != raw.pack.as_str() {
        return Err(SkillManifestError::BuiltinIdentityMismatch {
            expected: expected_pack.to_string(),
            actual: raw.pack.clone(),
        });
    }
    if raw.source != SkillSource::Builtin {
        return Err(SkillManifestError::BuiltinSourceMismatch);
    }

    let reference_policy = normalize_references(&raw.references)?;
    let references = resolve_references(
        expected_pack,
        &raw.name,
        reference_assets,
        &reference_policy.include,
    )?;
    normalize_raw_manifest(&mut raw, body)?;
    let id = SkillId::new(expected_id).map_err(|err| SkillManifestError::InvalidYaml {
        source_path: manifest_asset.source_path.to_string(),
        message: err.to_string(),
    })?;
    Ok(SkillHarness {
        id,
        name: raw.name,
        description: raw.description,
        version: raw.version,
        source: raw.source,
        pack: expected_pack.to_string(),
        required_hooks: raw.required_hooks,
        allowed_tools: raw.allowed_tools,
        requestable_tools: raw.requestable_tools,
        denied_tools: raw.denied_tools,
        permission_request_templates: raw.permission_request_templates,
        permissions: raw.permissions,
        context_budget_tokens: raw.context_budget_tokens,
        reference_policy,
        references,
        artifacts: raw.artifacts,
        guarantees: raw.guarantees,
        body: body.to_string(),
        source_path: manifest_asset.source_path.to_string(),
        manifest_sha256: manifest_asset.sha256.to_string(),
        tree_sha256: tree_sha256.to_string(),
    })
}

fn parse_workspace_text(
    skill_dir: &Path,
    component_root: &Path,
    source_path: &str,
    manifest_bytes: &[u8],
    tree_sha256: &str,
    text: &str,
) -> Result<SkillHarness, SkillManifestError> {
    let (mut raw, body) = parse_manifest_text(source_path, text)?;
    if !raw.source.is_external_skill_source() {
        return Err(SkillManifestError::WorkspaceSourceMismatch);
    }

    let reference_policy = normalize_references(&raw.references)?;
    let references = resolve_workspace_references(skill_dir, component_root, &reference_policy)?;
    normalize_raw_manifest(&mut raw, body)?;
    let id = SkillId::new(raw.name.clone()).map_err(|err| SkillManifestError::InvalidYaml {
        source_path: source_path.to_string(),
        message: err.to_string(),
    })?;
    Ok(SkillHarness {
        id,
        name: raw.name,
        description: raw.description,
        version: raw.version,
        source: raw.source,
        pack: raw.pack,
        required_hooks: raw.required_hooks,
        allowed_tools: raw.allowed_tools,
        requestable_tools: raw.requestable_tools,
        denied_tools: raw.denied_tools,
        permission_request_templates: raw.permission_request_templates,
        permissions: raw.permissions,
        context_budget_tokens: raw.context_budget_tokens,
        reference_policy,
        references,
        artifacts: raw.artifacts,
        guarantees: raw.guarantees,
        body: body.to_string(),
        source_path: source_path.to_string(),
        manifest_sha256: sha256_hex(manifest_bytes),
        tree_sha256: tree_sha256.to_string(),
    })
}

fn parse_manifest_text<'a>(
    source_path: &str,
    text: &'a str,
) -> Result<(RawSkillManifest, &'a str), SkillManifestError> {
    let (frontmatter, body) = split_frontmatter(source_path, text)?;
    let raw = serde_yaml::from_str::<RawSkillManifest>(frontmatter).map_err(|err| {
        SkillManifestError::InvalidYaml {
            source_path: source_path.to_string(),
            message: err.to_string(),
        }
    })?;
    validate_raw_manifest(&raw)?;
    Ok((raw, body))
}

fn normalize_raw_manifest(
    raw: &mut RawSkillManifest,
    body: &str,
) -> Result<(), SkillManifestError> {
    if body.trim().is_empty() {
        return Err(SkillManifestError::EmptyBody);
    }
    raw.allowed_tools = sort_unique_ids(std::mem::take(&mut raw.allowed_tools), "allowed_tools")?;
    raw.requestable_tools = sort_unique_ids(
        std::mem::take(&mut raw.requestable_tools),
        "requestable_tools",
    )?;
    raw.denied_tools = sort_unique_ids(std::mem::take(&mut raw.denied_tools), "denied_tools")?;
    validate_tool_routing(
        &raw.allowed_tools,
        &raw.requestable_tools,
        &raw.denied_tools,
    )?;
    raw.permission_request_templates = normalize_permission_request_templates(
        std::mem::take(&mut raw.permission_request_templates),
        &raw.requestable_tools,
    )?;
    raw.required_hooks =
        sort_unique_ids(std::mem::take(&mut raw.required_hooks), "required_hooks")?;
    raw.permissions = normalize_permissions(std::mem::take(&mut raw.permissions))?;
    normalize_artifacts(&raw.artifacts)?;
    raw.guarantees = sort_unique_strings(std::mem::take(&mut raw.guarantees), "guarantees")?;
    Ok(())
}

fn split_frontmatter<'a>(
    source_path: &str,
    text: &'a str,
) -> Result<(&'a str, &'a str), SkillManifestError> {
    let frontmatter_start = if text.starts_with("---\n") {
        4
    } else if text.starts_with("---\r\n") {
        5
    } else {
        return Err(SkillManifestError::MissingFrontmatter {
            source_path: source_path.to_string(),
        });
    };
    let rest = &text[frontmatter_start..];
    if let Some(offset) = rest.find("\n---\n") {
        let frontmatter = &rest[..offset];
        let body = &rest[offset + 5..];
        return Ok((frontmatter, body));
    }
    if let Some(offset) = rest.find("\r\n---\r\n") {
        let frontmatter = &rest[..offset];
        let body = &rest[offset + 7..];
        return Ok((frontmatter, body));
    }
    Err(SkillManifestError::UnterminatedFrontmatter {
        source_path: source_path.to_string(),
    })
}

fn validate_raw_manifest(raw: &RawSkillManifest) -> Result<(), SkillManifestError> {
    ensure_non_blank("name", &raw.name)?;
    ensure_non_blank("description", &raw.description)?;
    ensure_non_blank("pack", &raw.pack)?;
    if raw.context_budget_tokens == 0 {
        return Err(SkillManifestError::ContextBudgetZero);
    }
    if raw.required_hooks.is_empty() {
        return Err(SkillManifestError::EmptyList {
            field: "required_hooks",
        });
    }
    if raw.guarantees.is_empty() {
        return Err(SkillManifestError::EmptyList {
            field: "guarantees",
        });
    }
    for guarantee in &raw.guarantees {
        ensure_non_blank("guarantees", guarantee)?;
    }
    Ok(())
}

fn validate_tool_routing(
    allowed_tools: &[ToolId],
    requestable_tools: &[ToolId],
    denied_tools: &[ToolId],
) -> Result<(), SkillManifestError> {
    reject_tool_overlap(
        allowed_tools,
        requestable_tools,
        "allowed_tools",
        "requestable_tools",
    )?;
    reject_tool_overlap(allowed_tools, denied_tools, "allowed_tools", "denied_tools")?;
    reject_tool_overlap(
        requestable_tools,
        denied_tools,
        "requestable_tools",
        "denied_tools",
    )?;
    Ok(())
}

fn reject_tool_overlap(
    first: &[ToolId],
    second: &[ToolId],
    first_field: &'static str,
    second_field: &'static str,
) -> Result<(), SkillManifestError> {
    let first = first.iter().collect::<BTreeSet<_>>();
    for tool in second {
        if first.contains(tool) {
            return Err(SkillManifestError::ConflictingToolRouting {
                tool: tool.as_str().to_string(),
                first_field,
                second_field,
            });
        }
    }
    Ok(())
}

fn normalize_permission_request_templates(
    templates: Vec<SkillPermissionRequestTemplate>,
    requestable_tools: &[ToolId],
) -> Result<Vec<SkillPermissionRequestTemplate>, SkillManifestError> {
    let requestable = requestable_tools.iter().collect::<BTreeSet<_>>();
    let mut normalized = Vec::with_capacity(templates.len());
    let mut seen_ids = BTreeSet::new();
    for mut template in templates {
        ensure_non_blank("permission_request_templates.id", &template.id)?;
        ensure_non_blank(
            "permission_request_templates.default_duration",
            &template.default_duration,
        )?;
        ensure_non_blank(
            "permission_request_templates.reason_template",
            &template.reason_template,
        )?;
        if !seen_ids.insert(template.id.clone()) {
            return Err(SkillManifestError::DuplicateValue {
                field: "permission_request_templates.id",
                value: template.id,
            });
        }
        if template.tools.is_empty() {
            return Err(SkillManifestError::EmptyList {
                field: "permission_request_templates.tools",
            });
        }
        template.tools = sort_unique_ids(template.tools, "permission_request_templates.tools")?;
        for tool in &template.tools {
            if !requestable.contains(tool) {
                return Err(SkillManifestError::TemplateToolNotRequestable {
                    template_id: template.id.clone(),
                    tool: tool.as_str().to_string(),
                });
            }
        }
        template.state_effects.sort();
        reject_duplicate_values(
            template
                .state_effects
                .iter()
                .map(|effect| format!("{effect:?}")),
            "permission_request_templates.state_effects",
        )?;
        normalized.push(template);
    }
    normalized.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(normalized)
}

fn normalize_references(
    references: &RawReferencePolicy,
) -> Result<SkillReferencePolicy, SkillManifestError> {
    let include = sort_unique_strings(references.include.clone(), "references.include")?;
    for path in &include {
        validate_reference_path(path)?;
    }
    Ok(SkillReferencePolicy { include })
}

fn normalize_permissions(
    mut permissions: SkillPermissions,
) -> Result<SkillPermissions, SkillManifestError> {
    permissions.memory.read.sort();
    reject_duplicate_values(
        permissions
            .memory
            .read
            .iter()
            .map(|scope| scope.as_str().to_string()),
        "permissions.memory.read",
    )?;
    Ok(permissions)
}

fn normalize_artifacts(artifacts: &[SkillArtifactDeclaration]) -> Result<(), SkillManifestError> {
    let mut ids = BTreeSet::new();
    for artifact in artifacts {
        ensure_non_blank("artifacts.id", &artifact.id)?;
        if !ids.insert(artifact.id.clone()) {
            return Err(SkillManifestError::DuplicateValue {
                field: "artifacts.id",
                value: artifact.id.clone(),
            });
        }
        validate_artifact_path(&artifact.path)?;
        if artifact
            .provides
            .iter()
            .any(|value| value.trim().is_empty())
        {
            return Err(SkillManifestError::BlankField {
                field: "artifacts.provides",
            });
        }
        let mut create_situations = BTreeSet::new();
        for rule in &artifact.create {
            let value = format!("{:?}", rule.when);
            if !create_situations.insert(rule.when) {
                return Err(SkillManifestError::DuplicateValue {
                    field: "artifacts.create.when",
                    value,
                });
            }
        }
    }
    Ok(())
}

fn validate_artifact_path(path: &Path) -> Result<(), SkillManifestError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(SkillManifestError::InvalidArtifactPath {
            path: path.display().to_string(),
        });
    }
    let mut components = path.components();
    match components.next() {
        Some(std::path::Component::Normal(component)) if component == ".agl" => Ok(()),
        _ => Err(SkillManifestError::InvalidArtifactPath {
            path: path.display().to_string(),
        }),
    }
}

fn resolve_references(
    pack: &str,
    name: &str,
    reference_assets: &[&'static BuiltinAsset],
    includes: &[String],
) -> Result<Vec<SkillReference>, SkillManifestError> {
    let prefix = format!("assets/skills/{pack}/{name}/");
    let mut references_by_path = BTreeMap::new();
    for asset in reference_assets {
        let relative_path = asset
            .source_path
            .strip_prefix(&prefix)
            .unwrap_or(asset.source_path);
        references_by_path.insert(relative_path.to_string(), *asset);
    }
    let mut resolved = Vec::with_capacity(includes.len());
    for include in includes {
        let asset = references_by_path.get(include).ok_or_else(|| {
            SkillManifestError::MissingReference {
                path: include.clone(),
            }
        })?;
        let content = asset
            .text()
            .map_err(|_| SkillManifestError::InvalidReferenceUtf8 {
                path: include.clone(),
            })?
            .to_string();
        resolved.push(SkillReference {
            path: include.clone(),
            sha256: asset.sha256.to_string(),
            content,
        });
    }
    Ok(resolved)
}

fn resolve_workspace_references(
    skill_dir: &Path,
    component_root: &Path,
    policy: &SkillReferencePolicy,
) -> Result<Vec<SkillReference>, SkillManifestError> {
    let canonical_skill_dir =
        skill_dir
            .canonicalize()
            .map_err(|err| SkillManifestError::ReadManifest {
                source_path: relative_source_path(&skill_dir.join("SKILL.md"), component_root),
                message: err.to_string(),
            })?;
    let mut resolved = Vec::with_capacity(policy.include.len());
    for include in &policy.include {
        let path = skill_dir.join(include);
        let canonical_path =
            fs::canonicalize(&path).map_err(|_| SkillManifestError::MissingReference {
                path: include.clone(),
            })?;
        if !canonical_path.starts_with(&canonical_skill_dir) {
            return Err(SkillManifestError::ReferenceEscapesSkill {
                path: include.clone(),
            });
        }
        let bytes = fs::read(&path).map_err(|_| SkillManifestError::MissingReference {
            path: include.clone(),
        })?;
        let content = String::from_utf8(bytes.clone()).map_err(|_| {
            SkillManifestError::InvalidReferenceUtf8 {
                path: include.clone(),
            }
        })?;
        resolved.push(SkillReference {
            path: include.clone(),
            sha256: sha256_hex(&bytes),
            content,
        });
    }
    Ok(resolved)
}

fn validate_reference_path(path: &str) -> Result<(), SkillManifestError> {
    let invalid = path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || !path.starts_with("references/")
        || path
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."));
    if invalid {
        Err(SkillManifestError::InvalidReferencePath {
            path: path.to_string(),
        })
    } else {
        Ok(())
    }
}

fn ensure_non_blank(field: &'static str, value: &str) -> Result<(), SkillManifestError> {
    if value.trim().is_empty() {
        Err(SkillManifestError::BlankField { field })
    } else {
        Ok(())
    }
}

fn sort_unique_ids<T>(values: Vec<T>, field: &'static str) -> Result<Vec<T>, SkillManifestError>
where
    T: Ord + ToString,
{
    let mut sorted = values;
    sorted.sort();
    reject_duplicate_values(sorted.iter().map(ToString::to_string), field)?;
    Ok(sorted)
}

fn sort_unique_strings(
    values: Vec<String>,
    field: &'static str,
) -> Result<Vec<String>, SkillManifestError> {
    let mut sorted = values;
    sorted.sort();
    reject_duplicate_values(sorted.iter().cloned(), field)?;
    Ok(sorted)
}

fn reject_duplicate_values(
    values: impl IntoIterator<Item = String>,
    field: &'static str,
) -> Result<(), SkillManifestError> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value.clone()) {
            return Err(SkillManifestError::DuplicateValue { field, value });
        }
    }
    Ok(())
}

fn relative_source_path(path: &Path, component_root: &Path) -> String {
    let relative = path.strip_prefix(component_root).unwrap_or(path);
    slash_path(relative)
}

fn slash_path(path: &Path) -> String {
    let mut result = PathBuf::new();
    result.push(path);
    result.to_string_lossy().replace('\\', "/")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use agl_assets::builtin_skill;

    use super::*;

    #[test]
    fn parses_builtin_task_spec_skill() {
        let skill = SkillHarness::parse_builtin(builtin_skill("task-spec").unwrap()).unwrap();

        assert_eq!(skill.id.as_str(), "task-spec");
        assert_eq!(skill.source, SkillSource::Builtin);
        assert_eq!(skill.pack, "agl");
        assert_eq!(
            skill
                .required_hooks
                .iter()
                .map(|hook| hook.as_str())
                .collect::<Vec<_>>(),
            vec!["repo_path.validate", "task_spec.validate"]
        );
        assert_eq!(
            skill
                .allowed_tools
                .iter()
                .map(|tool| tool.as_str())
                .collect::<Vec<_>>(),
            vec!["fs.edit", "fs.list", "fs.read", "fs.search"]
        );
        assert!(skill.requestable_tools.is_empty());
        assert!(skill.denied_tools.is_empty());
        assert!(skill.permission_request_templates.is_empty());
        assert_eq!(skill.references[0].path, "references/task-spec-contract.md");
        assert_eq!(skill.tree_sha256.len(), 64);
        assert!(skill.body.contains("task spec"));
    }

    #[test]
    fn parses_builtin_tool_smoke_skill() {
        let skill = SkillHarness::parse_builtin(builtin_skill("tool-smoke").unwrap()).unwrap();

        assert_eq!(skill.id.as_str(), "tool-smoke");
        assert_eq!(skill.source, SkillSource::Builtin);
        assert_eq!(skill.pack, "agl");
        assert_eq!(
            skill
                .required_hooks
                .iter()
                .map(|hook| hook.as_str())
                .collect::<Vec<_>>(),
            vec!["repo_path.validate"]
        );
        assert_eq!(
            skill
                .allowed_tools
                .iter()
                .map(|tool| tool.as_str())
                .collect::<Vec<_>>(),
            vec!["fs.read"]
        );
        assert!(skill.references.is_empty());
        assert!(skill.body.contains("smoke tests"));
    }

    #[test]
    fn parses_builtin_repo_review_skill() {
        let skill = SkillHarness::parse_builtin(builtin_skill("repo-review").unwrap()).unwrap();

        assert_eq!(skill.id.as_str(), "repo-review");
        assert_eq!(skill.source, SkillSource::Builtin);
        assert_eq!(skill.pack, "agl");
        assert!(
            skill
                .required_hooks
                .iter()
                .any(|hook| hook.as_str() == "diff_scope.validate")
        );
        assert!(
            skill
                .allowed_tools
                .iter()
                .any(|tool| tool.as_str() == "fs.search")
        );
        assert!(skill.body.contains("review-first repository work"));
    }

    #[test]
    fn frontmatter_rejects_unknown_fields() {
        let err = parse_fixture(
            r#"---
name: task-spec
description: Write specs.
version: 1
source: builtin
pack: agl
required_hooks:
  - task_spec.validate
allowed_tools: []
context_budget_tokens: 128
references:
  include: []
guarantees:
  - specs are checked
scripts:
  - nope
---
Body.
"#,
        )
        .unwrap_err();

        assert!(matches!(err, SkillManifestError::InvalidYaml { .. }));
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn frontmatter_rejects_invalid_hook_ids() {
        let err = parse_fixture(
            r#"---
name: task-spec
description: Write specs.
version: 1
source: builtin
pack: agl
required_hooks:
  - Bad Hook
allowed_tools: []
context_budget_tokens: 128
references:
  include: []
guarantees:
  - specs are checked
---
Body.
"#,
        )
        .unwrap_err();

        assert!(matches!(err, SkillManifestError::InvalidYaml { .. }));
        assert!(err.to_string().contains("hook id must use lowercase"));
    }

    #[test]
    fn frontmatter_rejects_duplicate_references() {
        let err = parse_fixture(
            r#"---
name: task-spec
description: Write specs.
version: 1
source: builtin
pack: agl
required_hooks:
  - task_spec.validate
allowed_tools: []
context_budget_tokens: 128
references:
  include:
    - references/a.md
    - references/a.md
guarantees:
  - specs are checked
---
Body.
"#,
        )
        .unwrap_err();

        assert_eq!(
            err,
            SkillManifestError::DuplicateValue {
                field: "references.include",
                value: "references/a.md".to_string(),
            }
        );
    }

    #[test]
    fn frontmatter_parses_permissions() {
        let skill = parse_fixture(
            r#"---
name: task-spec
description: Write specs.
version: 1
source: builtin
pack: agl
required_hooks:
  - task_spec.validate
allowed_tools: []
permissions:
  memory:
    read:
      - repo
      - user
  notes:
    read: true
    write: false
context_budget_tokens: 128
references:
  include: []
guarantees:
  - specs are checked
---
Body.
"#,
        )
        .unwrap();

        assert_eq!(skill.permissions.memory_read_scopes(), vec!["user", "repo"]);
        assert!(skill.permissions.notes.read);
        assert!(!skill.permissions.notes.write);
    }

    #[test]
    fn frontmatter_parses_permission_routing_fields() {
        let skill = parse_fixture(
            r#"---
name: task-spec
description: Write specs.
version: 1
source: builtin
pack: agl
required_hooks:
  - task_spec.validate
allowed_tools:
  - fs.read
requestable_tools:
  - cron.add
  - matrix.outbox.enqueue
denied_tools:
  - matrix.outbox.deliver
permission_requests:
  - id: schedule-matrix-cron
    tools:
      - matrix.outbox.enqueue
      - cron.add
    max_operation_kind: write
    state_effects:
      - store_permission_requests
    default_duration: one_turn
    reason_template: Schedule a recurring Matrix notification.
context_budget_tokens: 128
references:
  include: []
guarantees:
  - specs are checked
---
Body.
"#,
        )
        .unwrap();

        assert_eq!(
            skill
                .allowed_tools
                .iter()
                .map(|tool| tool.as_str())
                .collect::<Vec<_>>(),
            vec!["fs.read"]
        );
        assert_eq!(
            skill
                .requestable_tools
                .iter()
                .map(|tool| tool.as_str())
                .collect::<Vec<_>>(),
            vec!["cron.add", "matrix.outbox.enqueue"]
        );
        assert_eq!(
            skill
                .denied_tools
                .iter()
                .map(|tool| tool.as_str())
                .collect::<Vec<_>>(),
            vec!["matrix.outbox.deliver"]
        );
        assert_eq!(skill.permission_request_templates.len(), 1);
        let template = &skill.permission_request_templates[0];
        assert_eq!(template.id, "schedule-matrix-cron");
        assert_eq!(
            template
                .tools
                .iter()
                .map(|tool| tool.as_str())
                .collect::<Vec<_>>(),
            vec!["cron.add", "matrix.outbox.enqueue"]
        );
        assert_eq!(template.max_operation_kind, Some(ToolOperationKind::Write));
    }

    #[test]
    fn frontmatter_rejects_allowed_requestable_overlap() {
        let err = parse_fixture(
            r#"---
name: task-spec
description: Write specs.
version: 1
source: builtin
pack: agl
required_hooks:
  - task_spec.validate
allowed_tools:
  - fs.read
requestable_tools:
  - fs.read
context_budget_tokens: 128
references:
  include: []
guarantees:
  - specs are checked
---
Body.
"#,
        )
        .unwrap_err();

        assert_eq!(
            err,
            SkillManifestError::ConflictingToolRouting {
                tool: "fs.read".to_string(),
                first_field: "allowed_tools",
                second_field: "requestable_tools",
            }
        );
    }

    #[test]
    fn frontmatter_rejects_template_tool_outside_requestable_set() {
        let err = parse_fixture(
            r#"---
name: task-spec
description: Write specs.
version: 1
source: builtin
pack: agl
required_hooks:
  - task_spec.validate
allowed_tools: []
requestable_tools:
  - cron.add
permission_request_templates:
  - id: bad-template
    tools:
      - matrix.outbox.enqueue
    default_duration: one_turn
    reason_template: Queue a Matrix message.
context_budget_tokens: 128
references:
  include: []
guarantees:
  - specs are checked
---
Body.
"#,
        )
        .unwrap_err();

        assert_eq!(
            err,
            SkillManifestError::TemplateToolNotRequestable {
                template_id: "bad-template".to_string(),
                tool: "matrix.outbox.enqueue".to_string(),
            }
        );
    }

    #[test]
    fn frontmatter_rejects_duplicate_permission_scopes() {
        let err = parse_fixture(
            r#"---
name: task-spec
description: Write specs.
version: 1
source: builtin
pack: agl
required_hooks:
  - task_spec.validate
allowed_tools: []
permissions:
  memory:
    read:
      - user
      - user
context_budget_tokens: 128
references:
  include: []
guarantees:
  - specs are checked
---
Body.
"#,
        )
        .unwrap_err();

        assert_eq!(
            err,
            SkillManifestError::DuplicateValue {
                field: "permissions.memory.read",
                value: "user".to_string(),
            }
        );
    }

    #[test]
    fn frontmatter_parses_artifact_declarations() {
        let skill = parse_fixture(
            r#"---
name: task-spec
description: Write specs.
version: 1
source: builtin
pack: agl
required_hooks:
  - task_spec.validate
allowed_tools: []
context_budget_tokens: 128
references:
  include: []
artifacts:
  - id: task-specs
    kind: generated
    path: .agl/tasks/generated
    access: read_write
    create:
      - when: skill_sync
      - when: runtime_prepare
    provides:
      - tasks
    schema: agl.task_spec_legacy.v1
guarantees:
  - specs are checked
---
Body.
"#,
        )
        .unwrap();

        assert_eq!(skill.artifacts.len(), 1);
        assert_eq!(skill.artifacts[0].id, "task-specs");
        assert_eq!(
            skill.artifacts[0].path,
            PathBuf::from(".agl/tasks/generated")
        );
        assert_eq!(skill.artifacts[0].access, SkillArtifactAccess::ReadWrite);
        assert_eq!(skill.artifacts[0].create.len(), 2);
        assert_eq!(
            skill.artifacts[0].create[0].when,
            SkillFolderCreateSituation::SkillSync
        );
    }

    #[test]
    fn frontmatter_rejects_artifact_paths_outside_agl() {
        let err = parse_fixture(
            r#"---
name: task-spec
description: Write specs.
version: 1
source: builtin
pack: agl
required_hooks:
  - task_spec.validate
allowed_tools: []
context_budget_tokens: 128
references:
  include: []
artifacts:
  - id: bad
    kind: source
    path: ../bad
    access: read
guarantees:
  - specs are checked
---
Body.
"#,
        )
        .unwrap_err();

        assert_eq!(
            err,
            SkillManifestError::InvalidArtifactPath {
                path: "../bad".to_string()
            }
        );
    }

    fn parse_fixture(text: &str) -> Result<SkillHarness, SkillManifestError> {
        let manifest_asset = BuiltinAsset {
            id: "task-spec",
            kind: agl_assets::BuiltinAssetKind::Skill,
            source_path: "assets/skills/agl/task-spec/SKILL.md",
            sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            bytes: b"",
        };
        parse_skill_text(
            "task-spec",
            "agl",
            &manifest_asset,
            &[],
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            text,
        )
    }

    #[test]
    fn parses_workspace_skill_from_directory() {
        let root = temp_root("workspace-skill");
        let skill_dir = root.join("agl/repo-change");
        fs::create_dir_all(skill_dir.join("references")).unwrap();
        fs::write(skill_dir.join("references/policy.md"), "Policy").unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: repo-change
description: Review repository changes.
version: 1
source: workspace
pack: agl
required_hooks:
  - repo_path.validate
allowed_tools: []
context_budget_tokens: 256
references:
  include:
    - references/policy.md
guarantees:
  - repository paths are checked
---
# Repo Change

Review changes.
"#,
        )
        .unwrap();

        let skill = SkillHarness::parse_workspace_dir(&skill_dir, &root, "tree-sha").unwrap();

        assert_eq!(skill.id.as_str(), "repo-change");
        assert_eq!(skill.source, SkillSource::Workspace);
        assert_eq!(skill.source_path, "agl/repo-change/SKILL.md");
        assert_eq!(skill.references[0].path, "references/policy.md");
        assert_eq!(skill.references[0].sha256.len(), 64);
        assert_eq!(skill.tree_sha256, "tree-sha");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_core_skill_from_external_directory() {
        let root = temp_root("core-skill");
        let skill_dir = root.join("agl/repo-review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: repo-review
description: Review repository changes.
version: 1
source: core
pack: agl
required_hooks:
  - repo_path.validate
allowed_tools: []
context_budget_tokens: 256
references:
  include: []
guarantees:
  - repository paths are checked
---
Body.
"#,
        )
        .unwrap();

        let skill = SkillHarness::parse_workspace_dir(&skill_dir, &root, "tree-sha").unwrap();

        assert_eq!(skill.id.as_str(), "repo-review");
        assert_eq!(skill.source, SkillSource::Core);
        assert_eq!(skill.tree_sha256, "tree-sha");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_parser_rejects_builtin_source() {
        let root = temp_root("workspace-source");
        let skill_dir = root.join("agl/repo-change");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: repo-change
description: Review repository changes.
version: 1
source: builtin
pack: agl
required_hooks:
  - repo_path.validate
allowed_tools: []
context_budget_tokens: 256
references:
  include: []
guarantees:
  - repository paths are checked
---
Body.
"#,
        )
        .unwrap();

        let err = SkillHarness::parse_workspace_dir(&skill_dir, &root, "tree-sha").unwrap_err();

        assert_eq!(err, SkillManifestError::WorkspaceSourceMismatch);

        fs::remove_dir_all(root).unwrap();
    }

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("agl-skills-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }
}
