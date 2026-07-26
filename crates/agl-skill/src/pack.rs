use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use agl_artifact::{
    ArtifactAdapter, ArtifactCandidate, ArtifactPackageRef, ArtifactResolver, ArtifactSourceId,
    ArtifactSourceKind, ArtifactSourceTier, ArtifactTypeId, DirectoryPackageView,
    StaticArtifactSource,
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{SkillArtifactAdapter, SkillHarness, SkillSource, skill_adapter_registry};

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillPackManifest {
    pub version: u32,
    pub name: String,
    pub pack_version: String,
    pub profile: String,
    pub agentlibre_version: String,
    pub submodule: SkillPackSubmodule,
    #[serde(default)]
    pub skills: Vec<SkillPackEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillPackSubmodule {
    pub path: PathBuf,
    pub url: String,
    pub rev: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillPackEntry {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedSkillPack {
    pub manifest: SkillPackManifest,
    pub skills: Vec<SkillHarness>,
}

pub fn validate_skill_pack(root: impl AsRef<Path>) -> Result<ValidatedSkillPack> {
    let root = root.as_ref();
    let manifest = read_pack_manifest(root)?;
    validate_manifest_shape(&manifest)?;

    let mut seen = BTreeSet::new();
    let mut entries = Vec::with_capacity(manifest.skills.len());
    let adapter = SkillArtifactAdapter::default();
    let adapter_registry = skill_adapter_registry()?;
    let source_id: ArtifactSourceId = "skill-pack".parse()?;
    let skill_type: ArtifactTypeId = "skill".parse()?;
    for entry in &manifest.skills {
        validate_relative_path(&entry.path)
            .with_context(|| format!("invalid skill path for {}", entry.name))?;
        if !seen.insert(entry.name.clone()) {
            bail!("duplicate skill in pack manifest: {}", entry.name);
        }

        let view = DirectoryPackageView::new(root.join(&entry.path))
            .with_context(|| format!("failed to open pack skill {}", entry.name))?;
        let envelope = adapter
            .extract_envelope(&view)
            .with_context(|| format!("failed to parse pack skill {}", entry.name))?;
        if envelope.id.as_str() != entry.name {
            bail!(
                "pack skill name mismatch at {}: manifest={}, skill={}",
                entry.path.display(),
                entry.name,
                envelope.id
            );
        }
        entries.push((entry, envelope, view));
    }

    let candidates = entries
        .iter()
        .map(|(_, envelope, view)| {
            ArtifactCandidate::new(
                skill_type.clone(),
                envelope.id.clone(),
                envelope.version.clone(),
                source_id.clone(),
                ArtifactSourceTier::Explicit,
                ArtifactSourceKind::Directory,
                Arc::new(view.clone()),
            )
        })
        .collect::<Vec<_>>();
    let source = StaticArtifactSource::new(
        source_id,
        ArtifactSourceTier::Explicit,
        ArtifactSourceKind::Directory,
        candidates,
    )?;
    let resolver = ArtifactResolver::new(adapter_registry.clone(), vec![Arc::new(source)]);
    let mut skills = Vec::with_capacity(entries.len());
    for (entry, envelope, _) in entries {
        let root_ref = ArtifactPackageRef::parse(&format!("skill:{}@*", envelope.id))?;
        let graph = resolver.resolve_and_validate(&root_ref, None)?;
        let node = graph
            .nodes
            .get(&graph.root)
            .context("resolved pack skill root is missing")?;
        let payload = adapter_registry
            .lookup(&node.candidate.type_id)?
            .validate_payload(node.candidate.view(), &node.envelope)?;
        let mut harness = *payload
            .downcast::<SkillHarness>()
            .map_err(|_| anyhow::anyhow!("skill adapter returned an invalid payload"))?;
        harness.source = SkillSource::Local;
        harness.tree_sha256 = node.package_digest.to_string();
        harness.source_path = entry.path.join("SKILL.md").display().to_string();
        skills.push(harness);
    }

    Ok(ValidatedSkillPack { manifest, skills })
}

fn read_pack_manifest(root: &Path) -> Result<SkillPackManifest> {
    let path = root.join("pack.toml");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

fn validate_manifest_shape(manifest: &SkillPackManifest) -> Result<()> {
    if manifest.version != 1 {
        bail!(
            "unsupported skill pack manifest version: {}",
            manifest.version
        );
    }
    ensure_non_blank("name", &manifest.name)?;
    ensure_non_blank("pack_version", &manifest.pack_version)?;
    ensure_non_blank("profile", &manifest.profile)?;
    ensure_non_blank("agentlibre_version", &manifest.agentlibre_version)?;
    ensure_non_blank("submodule.url", &manifest.submodule.url)?;
    ensure_non_blank("submodule.rev", &manifest.submodule.rev)?;
    validate_relative_path(&manifest.submodule.path)?;
    if manifest.skills.is_empty() {
        bail!("skill pack must contain at least one skill");
    }
    Ok(())
}

fn ensure_non_blank(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("skill pack field `{field}` cannot be blank");
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("path cannot be empty");
    }
    if path.is_absolute() {
        bail!("path must be relative");
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => bail!("path cannot contain parent directory segments"),
            _ => bail!("path contains unsupported component"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_workflow_pack_manifest_is_valid() {
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/testdata/repo-workflow-pack");

        let pack = validate_skill_pack(&root).unwrap();

        assert_eq!(pack.manifest.name, "agl/repo-workflow");
        assert_eq!(pack.manifest.profile, "repo-workflow");
        assert_eq!(pack.skills.len(), 1);
        assert!(pack.skills.iter().any(|skill| skill.name == "repo-change"));
    }
}
