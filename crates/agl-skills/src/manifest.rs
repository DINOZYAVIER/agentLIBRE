use std::collections::{BTreeMap, BTreeSet};

use agl_assets::{BuiltinAsset, BuiltinSkill};
use agl_extension::{HookId, SkillId, ToolId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    Builtin,
    Workspace,
    User,
    ThirdParty,
}

impl SkillSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Workspace => "workspace",
            Self::User => "user",
            Self::ThirdParty => "third_party",
        }
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
    pub context_budget_tokens: u32,
    pub reference_policy: SkillReferencePolicy,
    pub references: Vec<SkillReference>,
    pub guarantees: Vec<String>,
    pub body: String,
    pub source_path: String,
    pub manifest_sha256: String,
    pub tree_sha256: String,
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

    pub fn is_trusted_source(&self) -> bool {
        self.source == SkillSource::Builtin
    }
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
    context_budget_tokens: u32,
    references: RawReferencePolicy,
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
    ContextBudgetZero,
    EmptyBody,
}

impl std::fmt::Display for SkillManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUtf8 { source_path } => {
                write!(f, "skill manifest is not valid UTF-8: {source_path}")
            }
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
            Self::ContextBudgetZero => write!(f, "skill context budget must be greater than zero"),
            Self::EmptyBody => write!(f, "skill body cannot be empty"),
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
    let (frontmatter, body) = split_frontmatter(manifest_asset.source_path, text)?;
    let raw = serde_yaml::from_str::<RawSkillManifest>(frontmatter).map_err(|err| {
        SkillManifestError::InvalidYaml {
            source_path: manifest_asset.source_path.to_string(),
            message: err.to_string(),
        }
    })?;
    validate_raw_manifest(&raw)?;

    let actual_id = format!("{}:{}", raw.pack, raw.name);
    if expected_id != actual_id {
        return Err(SkillManifestError::BuiltinIdentityMismatch {
            expected: expected_id.to_string(),
            actual: actual_id,
        });
    }
    if expected_pack != raw.pack {
        return Err(SkillManifestError::BuiltinIdentityMismatch {
            expected: expected_pack.to_string(),
            actual: raw.pack,
        });
    }
    if raw.source != SkillSource::Builtin {
        return Err(SkillManifestError::BuiltinSourceMismatch);
    }

    let reference_policy = normalize_references(raw.references)?;
    let references = resolve_references(expected_id, reference_assets, &reference_policy.include)?;
    if body.trim().is_empty() {
        return Err(SkillManifestError::EmptyBody);
    }

    Ok(SkillHarness {
        id: SkillId::new(expected_id).map_err(|err| SkillManifestError::InvalidYaml {
            source_path: manifest_asset.source_path.to_string(),
            message: err.to_string(),
        })?,
        name: raw.name,
        description: raw.description,
        version: raw.version,
        source: raw.source,
        pack: expected_pack.to_string(),
        required_hooks: sort_unique_ids(raw.required_hooks, "required_hooks")?,
        allowed_tools: sort_unique_ids(raw.allowed_tools, "allowed_tools")?,
        context_budget_tokens: raw.context_budget_tokens,
        reference_policy,
        references,
        guarantees: sort_unique_strings(raw.guarantees, "guarantees")?,
        body: body.to_string(),
        source_path: manifest_asset.source_path.to_string(),
        manifest_sha256: manifest_asset.sha256.to_string(),
        tree_sha256: tree_sha256.to_string(),
    })
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

fn normalize_references(
    references: RawReferencePolicy,
) -> Result<SkillReferencePolicy, SkillManifestError> {
    let include = sort_unique_strings(references.include, "references.include")?;
    for path in &include {
        validate_reference_path(path)?;
    }
    Ok(SkillReferencePolicy { include })
}

fn resolve_references(
    skill_id: &str,
    reference_assets: &[&'static BuiltinAsset],
    includes: &[String],
) -> Result<Vec<SkillReference>, SkillManifestError> {
    let prefix = format!(
        "assets/skills/{}/{}/",
        pack_name(skill_id).0,
        pack_name(skill_id).1
    );
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

fn pack_name(skill_id: &str) -> (&str, &str) {
    skill_id
        .split_once(':')
        .expect("builtin skill ids must contain a pack separator")
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

#[cfg(test)]
mod tests {
    use agl_assets::builtin_skill;

    use super::*;

    #[test]
    fn parses_builtin_task_spec_skill() {
        let skill = SkillHarness::parse_builtin(builtin_skill("core:task-spec").unwrap()).unwrap();

        assert_eq!(skill.id.as_str(), "core:task-spec");
        assert_eq!(skill.source, SkillSource::Builtin);
        assert_eq!(skill.pack, "core");
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
        assert_eq!(skill.references[0].path, "references/task-spec-contract.md");
        assert_eq!(skill.tree_sha256.len(), 64);
        assert!(skill.body.contains("task spec"));
    }

    #[test]
    fn frontmatter_rejects_unknown_fields() {
        let err = parse_fixture(
            r#"---
name: task-spec
description: Write specs.
version: 1
source: builtin
pack: core
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
pack: core
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
pack: core
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

    fn parse_fixture(text: &str) -> Result<SkillHarness, SkillManifestError> {
        let manifest_asset = BuiltinAsset {
            id: "core:task-spec",
            kind: agl_assets::BuiltinAssetKind::Skill,
            source_path: "assets/skills/core/task-spec/SKILL.md",
            sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            bytes: b"",
        };
        parse_skill_text(
            "core:task-spec",
            "core",
            &manifest_asset,
            &[],
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            text,
        )
    }
}
