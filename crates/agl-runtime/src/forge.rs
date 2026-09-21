use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[cfg(test)]
use ayeque_forge_core::resolve_project_at;
use ayeque_forge_core::{
    VerifiedEntity, VerifiedProject, resolve_entity, resolve_project, verify_lock,
};
use semver::{Version, VersionReq};

use crate::agent::AGENT_PAYLOAD_SCHEMA;
use crate::extension::EXTENSION_SCHEMA;
use crate::function::{EntityRef, FUNCTION_FILE_NAME, FUNCTION_SCHEMA, FunctionManifest};
use crate::model::MODEL_SCHEMA;
use crate::package::{
    DirectoryPackageView, PackageId, PackageTreeDigest, PackageVersion, PackageView,
    compute_package_digest,
};
use crate::skill::SKILL_PAYLOAD_SCHEMA;
use anyhow::{Context, Result, ensure};

#[derive(Clone, Debug)]
pub struct FunctionIdentity {
    pub directory: PathBuf,
    pub id: PackageId,
    pub version: PackageVersion,
    pub digest: PackageTreeDigest,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedFunctionSource {
    pub manifest: FunctionManifest,
    pub function_digest: PackageTreeDigest,
    pub entities: Vec<ResolvedEntity>,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedEntity {
    pub id: PackageId,
    pub version: PackageVersion,
    pub kind: String,
    pub root: PathBuf,
    pub content_digest: PackageTreeDigest,
}

pub(crate) fn resolve_function(
    function_directory: impl AsRef<Path>,
    workspace: impl AsRef<Path>,
) -> Result<ResolvedFunctionSource> {
    let project = resolve_project(workspace.as_ref())?;
    resolve_function_from_project(&project, function_directory.as_ref())
}

#[cfg(test)]
fn resolve_function_at(
    storage_root: &Path,
    function_directory: impl AsRef<Path>,
    workspace: impl AsRef<Path>,
) -> Result<ResolvedFunctionSource> {
    let project = resolve_project_at(storage_root, workspace.as_ref())?;
    resolve_function_from_project(&project, function_directory.as_ref())
}

pub fn function_identity(
    function_directory: impl AsRef<Path>,
    workspace: impl AsRef<Path>,
) -> Result<FunctionIdentity> {
    let directory = function_directory.as_ref().canonicalize()?;
    let resolved = resolve_function(&directory, workspace)?;
    Ok(FunctionIdentity {
        directory,
        id: resolved.manifest.id,
        version: resolved.manifest.version,
        digest: resolved.function_digest,
    })
}

pub fn resolve_function_requirement(
    id: &PackageId,
    requirement: &VersionReq,
    workspace: impl AsRef<Path>,
) -> Result<FunctionIdentity> {
    let project = resolve_project(workspace.as_ref())?;
    let mut candidates = Vec::new();
    for declared in project
        .manifest()
        .entities()
        .iter()
        .filter(|entity| entity.kind() == "function" && entity.id() == id.as_str())
    {
        let entity = verified_entity(&project, declared.id(), "function", FUNCTION_SCHEMA)?;
        let directory = entity.materialized_path().canonicalize()?;
        let manifest = read_function(&directory)?;
        ensure!(
            manifest.id == *id,
            "Forge Function identity differs from FUNCTION.toml"
        );
        let version = Version::parse(&manifest.version.to_string())?;
        if requirement.matches(&version) {
            candidates.push((version, function_identity_at(&directory, manifest)?));
        }
    }
    candidates.sort_by(|left, right| right.0.cmp(&left.0));
    let best = candidates
        .first()
        .with_context(|| format!("no Forge Function {id} matching {requirement} exists"))?;
    ensure!(
        candidates
            .iter()
            .filter(|candidate| candidate.0 == best.0)
            .count()
            == 1,
        "Forge Function {id}@{requirement} resolves ambiguously"
    );
    Ok(best.1.clone())
}

fn resolve_function_from_project(
    project: &VerifiedProject,
    function_directory: &Path,
) -> Result<ResolvedFunctionSource> {
    let function_directory = function_directory.canonicalize().with_context(|| {
        format!(
            "failed to resolve Function {}",
            function_directory.display()
        )
    })?;
    verify_lock(project)?;
    let function = project
        .manifest()
        .entities()
        .iter()
        .filter(|entity| entity.kind() == "function")
        .map(|declared| verified_entity(project, declared.id(), "function", FUNCTION_SCHEMA))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .find(|entity| {
            entity
                .materialized_path()
                .canonicalize()
                .is_ok_and(|path| path == function_directory)
        })
        .context("Function is not a Forge-materialized Function entity")?;
    let manifest = read_function(&function_directory)?;
    ensure!(
        manifest.id.as_str() == function.id(),
        "Forge Function identity differs from FUNCTION.toml"
    );
    let function_digest =
        compute_package_digest(&DirectoryPackageView::new(function_directory.clone())?)?;
    let mut ids = BTreeSet::new();
    let mut entities = Vec::new();
    for (kind, reference) in manifest_references(&manifest)? {
        ensure!(
            ids.insert((kind, reference.id.clone())),
            "Function declares a dependency more than once"
        );
        let (forge_kind, schema) = kind_schema(kind);
        let verified = verified_entity(project, reference.id.as_str(), forge_kind, schema)?;
        let root = verified.materialized_path().canonicalize()?;
        let id = PackageId::new(verified.id().to_owned())?;
        let version = PackageVersion::new(package_version(&root, kind)?)?;
        ensure!(
            id == reference.id && version == reference.version,
            "Forge entity {} identity differs from Function reference",
            reference.id
        );
        let content_digest = compute_package_digest(&DirectoryPackageView::new(root.clone())?)?;
        entities.push(ResolvedEntity {
            id,
            version,
            kind: forge_kind.to_owned(),
            root,
            content_digest,
        });
    }
    Ok(ResolvedFunctionSource {
        manifest,
        function_digest,
        entities,
    })
}

fn function_identity_at(directory: &Path, manifest: FunctionManifest) -> Result<FunctionIdentity> {
    Ok(FunctionIdentity {
        directory: directory.to_owned(),
        id: manifest.id,
        version: manifest.version,
        digest: compute_package_digest(&DirectoryPackageView::new(directory.to_owned())?)?,
    })
}

fn verified_entity(
    project: &VerifiedProject,
    id: &str,
    expected_kind: &str,
    expected_schema: &str,
) -> Result<VerifiedEntity> {
    let entity = resolve_entity(project, id)?;
    ensure!(
        entity.kind() == expected_kind && entity.schema() == expected_schema,
        "Forge entity {id} has kind/schema {}/{}; expected {expected_kind}/{expected_schema}",
        entity.kind(),
        entity.schema()
    );
    Ok(entity)
}

fn read_function(directory: &Path) -> Result<FunctionManifest> {
    let package = DirectoryPackageView::new(directory.to_owned())?;
    let bytes = package.read_file(&crate::package::PackageRelativePath::new(
        FUNCTION_FILE_NAME,
    )?)?;
    FunctionManifest::parse(std::str::from_utf8(&bytes)?).context("invalid Forge Function")
}

fn manifest_references(manifest: &FunctionManifest) -> Result<Vec<(&'static str, EntityRef)>> {
    let mut references = vec![
        ("agent", manifest.agent.clone()),
        ("model", manifest.model.clone()),
    ];
    if let Some(recovery) = &manifest.recovery.invalid_model_output {
        references.push(("model", recovery.model.clone()));
    }
    references.extend(
        manifest
            .skills
            .iter()
            .cloned()
            .map(|value| ("skill", value)),
    );
    references.extend(
        manifest
            .extensions
            .iter()
            .cloned()
            .map(|value| ("extension", value)),
    );
    references.extend(
        manifest
            .inference
            .adapters
            .iter()
            .map(|value| ("model", value.model.clone())),
    );
    for (_, reference) in &references {
        ensure!(
            reference.git.is_none() && reference.rev.is_none() && reference.path.is_none(),
            "Forge Function references may contain only an entity id and version"
        );
        reference.validate()?;
    }
    Ok(references)
}

fn kind_schema(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "agent" => ("agent", AGENT_PAYLOAD_SCHEMA),
        "skill" => ("skill", SKILL_PAYLOAD_SCHEMA),
        "model" => ("model", MODEL_SCHEMA),
        "extension" => ("extension", EXTENSION_SCHEMA),
        _ => unreachable!("manifest_references only returns known kinds"),
    }
}

fn package_version(root: &Path, kind: &str) -> Result<String> {
    let package = DirectoryPackageView::new(root.to_owned())?;
    match kind {
        "agent" => Ok(crate::agent::parse_package_view(&package)?
            .manifest
            .version
            .to_string()),
        "skill" => Ok(crate::skill::parse_package_view(&package)?
            .manifest
            .version
            .to_string()),
        "model" => Ok(crate::model::parse_package_view(&package)?
            .version
            .to_string()),
        "extension" => Ok(crate::extension::parse_package_view(&package)?
            .manifest
            .version
            .to_string()),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ayeque_forge_core::LockEntry;
    use std::fs;

    #[test]
    fn resolves_function_and_dependencies_only_through_forge_catalog() {
        let root = std::env::temp_dir().join(format!("agl-runtime-forge-{}", uuid::Uuid::now_v7()));
        let workspace = root.join("workspace");
        let storage = root.join("forge");
        fs::create_dir_all(workspace.join(".git")).unwrap();
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::create_dir_all(storage.join("projects/workspace/entities")).unwrap();
        fs::write(workspace.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(workspace.join(".git/config"), "").unwrap();
        let project = storage.join("projects/workspace");
        let workspace_sha =
            ayeque_forge_core::workspace_identity(&workspace.canonicalize().unwrap());
        fs::write(
            project.join("project.toml"),
            format!("format = 1\nworkspace_sha256 = \"{workspace_sha}\"\n"),
        )
        .unwrap();
        let function = project.join("entities/coder");
        let agent = project.join("entities/coder-agent");
        let model = project.join("entities/test-model");
        for path in [&function, &agent, &model] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(function.join("FUNCTION.toml"), "schema = \"agentlibre.function/v2\"\nid = \"coder\"\nversion = \"1.0.0\"\nagent = { id = \"coder-agent\", version = \"1.0.0\" }\nmodel = { id = \"test-model\", version = \"1.0.0\" }\n[presentation.tool_output]\nlines = 10\nchars = 500\n[presentation.tool]\nframe = true\n[presentation.colors]\nrule = \"dim\"\nrun = \"bold #FF00FF\"\nrun_id = \"bold #FFFFFF\"\nstatus_success = \"bold #7BD88F\"\nstatus_failure = \"bold #FF0000\"\nstatus_pending = \"bold #FFFF00\"\noperation = \"bold #00FFFF\"\ntool = \"bold #00FFFF\"\nordinal = \"dim\"\nfield = \"bold #8AA2D8\"\nmuted = \"dim\"\njson_key = \"#00FFFF\"\njson_string = \"#7BD88F\"\njson_number = \"#FFFF00\"\njson_boolean = \"#FF00FF\"\njson_null = \"dim\"\nmarkdown_heading = \"bold #00FFFF\"\nmarkdown_code = \"dim\"\nmarkdown_inline_code = \"#FFFF00\"\nmarkdown_strong = \"bold\"\nmarkdown_emphasis = \"italic\"\nmarkdown_link = \"#8AA2D8\"\nmarkdown_quote = \"dim\"\nmarkdown_bullet = \"#00FFFF\"\nmarkdown_rule = \"dim\"\ninput_rule = \"oklch(0.439 0 0)\"\ninput_background = \"oklch(0.269 0 0)\"\ninput_prompt = \"bold oklch(0.718 0.202 349.761)\"\ninput_hint = \"dim oklch(0.823 0.12 346.018)\"\ninput_text = \"oklch(0.936 0.032 17.717)\"\ninput_activity = \"oklch(0.823 0.12 346.018)\"\ninput_selected = \"bold oklch(0.518 0.253 323.949)\"\n[presentation.model_generation]\ndetails = false\n").unwrap();
        fs::write(
            agent.join("AGENT.md"),
            "---\nschema: agentlibre.agent/v1\nid: coder-agent\nversion: 1.0.0\n---\n",
        )
        .unwrap();
        fs::write(agent.join("SYSTEM.md"), "Answer precisely.").unwrap();
        fs::write(model.join("MODEL.toml"), format!("schema = \"agentlibre.model/v1\"\nid = \"test-model\"\nversion = \"1.0.0\"\ndialect = \"generic\"\ntool_call_format = \"hermes_json\"\n\n[artifact]\nkind = \"gguf\"\nurl = \"https://example.invalid/model.gguf\"\nsha256 = \"sha256:{}\"\nbytes = 4\n", "01".repeat(32))).unwrap();
        let manifest = "format = 2\n\n[[entity]]\nid = \"coder\"\nkind = \"function\"\nschema = \"agentlibre.function/v2\"\ngit = \"https://example.invalid/coder.git\"\nrevision = \"main\"\n\n[[entity]]\nid = \"coder-agent\"\nkind = \"agent\"\nschema = \"agentlibre.agent/v1\"\ngit = \"https://example.invalid/agent.git\"\nrevision = \"main\"\n\n[[entity]]\nid = \"test-model\"\nkind = \"model\"\nschema = \"agentlibre.model/v1\"\ngit = \"https://example.invalid/model.git\"\nrevision = \"main\"\n"
            .to_owned();
        fs::write(project.join("FORGE.toml"), manifest).unwrap();
        let project_value = resolve_project_at(&storage, &workspace).unwrap();
        let entries = project_value
            .manifest()
            .entities()
            .iter()
            .map(|entity| {
                LockEntry::new(
                    entity.id().to_owned(),
                    entity.git().to_owned(),
                    "0123456789abcdef0123456789abcdef01234567".to_owned(),
                    ".".to_owned(),
                    "0123456789abcdef0123456789abcdef01234567".to_owned(),
                )
                .unwrap()
            })
            .collect();
        fs::write(
            project.join("FORGE.lock"),
            serialize_lock_for_test(&project_value, entries),
        )
        .unwrap();
        let resolved = resolve_function_at(&storage, &function, &workspace).unwrap();
        assert_eq!(resolved.manifest.id.as_str(), "coder");
        assert_eq!(resolved.entities.len(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    fn serialize_lock_for_test(
        project: &VerifiedProject,
        entries: Vec<ayeque_forge_core::LockEntry>,
    ) -> Vec<u8> {
        ayeque_forge_core::serialize_lock(project, entries).unwrap()
    }
}
