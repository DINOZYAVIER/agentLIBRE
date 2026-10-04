use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::workspace_forge::{ResolvedForgeDeclaration, WorkspaceForge};
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
    _forge: WorkspaceForge,
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
    catalog_root: impl AsRef<Path>,
) -> Result<ResolvedFunctionSource> {
    let workspace = workspace.as_ref();
    crate::resolve_workspace_forge(workspace)?;
    let project = resolve_agl_project(catalog_root.as_ref())?;
    resolve_function_from_project(&project, function_directory.as_ref())
}

fn resolve_agl_project(catalog_root: &Path) -> Result<WorkspaceForge> {
    ensure!(
        catalog_root.is_absolute(),
        "Forge catalog root must be absolute"
    );
    let root = catalog_root
        .canonicalize()
        .with_context(|| format!("Forge catalog {} is unavailable", catalog_root.display()))?;
    crate::resolve_workspace_forge(&root)
}

#[cfg(test)]
fn resolve_function_at(
    storage_root: &Path,
    function_directory: impl AsRef<Path>,
    workspace: impl AsRef<Path>,
) -> Result<ResolvedFunctionSource> {
    let project =
        crate::workspace_forge::resolve_workspace_forge_at(storage_root, workspace.as_ref())?;
    resolve_function_from_project(&project, function_directory.as_ref())
}

pub fn function_identity(
    function_directory: impl AsRef<Path>,
    workspace: impl AsRef<Path>,
    catalog_root: impl AsRef<Path>,
) -> Result<FunctionIdentity> {
    let directory = function_directory.as_ref().canonicalize()?;
    let resolved = resolve_function(&directory, workspace, catalog_root)?;
    Ok(FunctionIdentity {
        directory,
        id: resolved.manifest.id,
        version: resolved.manifest.version,
        digest: resolved.function_digest,
    })
}

#[derive(Clone, Debug)]
pub struct ScheduledFunction {
    pub identity: FunctionIdentity,
    pub schedule: crate::function::FunctionSchedule,
    pub permissions: crate::function::FunctionPermissions,
}

pub fn scheduled_functions(catalog_root: impl AsRef<Path>) -> Result<Vec<ScheduledFunction>> {
    let project = resolve_agl_project(catalog_root.as_ref())?;
    let mut scheduled = Vec::new();
    for declaration in project
        .declarations
        .iter()
        .filter(|entity| entity.kind == "function")
    {
        let entity = verified_entity(&project, &declaration.id, "function", FUNCTION_SCHEMA)?;
        let directory = entity.materialized_path.canonicalize()?;
        let manifest = read_function(&directory)?;
        ensure!(
            manifest.id.as_str() == declaration.id,
            "Forge Function identity differs from FUNCTION.toml"
        );
        if let Some(schedule) = manifest.schedule.clone() {
            let permissions = manifest.permissions.clone();
            scheduled.push(ScheduledFunction {
                identity: function_identity_at(&directory, manifest)?,
                schedule,
                permissions,
            });
        }
    }
    scheduled.sort_by(|a, b| a.identity.id.as_str().cmp(b.identity.id.as_str()));
    Ok(scheduled)
}

pub fn resolve_function_requirement(
    id: &PackageId,
    requirement: &VersionReq,
    catalog_root: impl AsRef<Path>,
) -> Result<FunctionIdentity> {
    let project = resolve_agl_project(catalog_root.as_ref())?;
    let mut candidates = Vec::new();
    for declared in project
        .declarations
        .iter()
        .filter(|entity| entity.kind == "function" && entity.id == id.as_str())
    {
        let entity = verified_entity(&project, &declared.id, "function", FUNCTION_SCHEMA)?;
        let directory = entity.materialized_path.canonicalize()?;
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
    let best = candidates.first().with_context(|| {
        format!("no Function {id} matching {requirement} is declared in the AGL workspace")
    })?;
    ensure!(
        candidates
            .iter()
            .filter(|candidate| candidate.0 == best.0)
            .count()
            == 1,
        "Function {id}@{requirement} resolves ambiguously in the AGL workspace"
    );
    Ok(best.1.clone())
}

/// Resolve the model manifest of the best Function matching a requirement.
pub fn resolve_function_model(
    id: &PackageId,
    requirement: &VersionReq,
    catalog_root: impl AsRef<Path>,
) -> Result<crate::model::ModelManifest> {
    let identity = resolve_function_requirement(id, requirement, catalog_root.as_ref())?;
    let project = resolve_agl_project(catalog_root.as_ref())?;
    let manifest = read_function(&identity.directory)?;
    let model = verified_entity(&project, manifest.model.id.as_str(), "model", MODEL_SCHEMA)?;
    crate::model::parse_package_view(&DirectoryPackageView::new(
        model.materialized_path.canonicalize()?,
    )?)
}

fn resolve_function_from_project(
    project: &WorkspaceForge,
    function_directory: &Path,
) -> Result<ResolvedFunctionSource> {
    let function_directory = function_directory.canonicalize().with_context(|| {
        format!(
            "failed to resolve Function {}",
            function_directory.display()
        )
    })?;
    let declaration = project
        .declarations
        .iter()
        .filter(|entity| entity.kind == "function")
        .find(|entity| {
            entity
                .materialized_path
                .canonicalize()
                .is_ok_and(|path| path == function_directory)
        })
        .context("Function is not materialized from the AGL workspace artifact catalog")?;
    let function = verified_entity(project, &declaration.id, "function", FUNCTION_SCHEMA)?;
    let manifest = read_function(&function_directory)?;
    ensure!(
        manifest.id.as_str() == function.id,
        "Function artifact identity differs from FUNCTION.toml"
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
        let root = verified.materialized_path.canonicalize()?;
        let id = PackageId::new(verified.id.to_owned())?;
        let version = PackageVersion::new(package_version(&root, kind)?)?;
        ensure!(
            id == reference.id && version == reference.version,
            "AGL artifact {} identity differs from Function reference",
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
        _forge: project.clone(),
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

fn verified_entity<'a>(
    project: &'a WorkspaceForge,
    id: &str,
    expected_kind: &str,
    expected_schema: &str,
) -> Result<&'a ResolvedForgeDeclaration> {
    let entity = project
        .declarations
        .iter()
        .find(|entity| entity.id == id)
        .context("Forge source is not declared")?;
    ensure!(
        entity.kind == expected_kind && entity.schema == expected_schema,
        "Forge entity {id} has kind/schema {}/{}; expected {expected_kind}/{expected_schema}",
        entity.kind,
        entity.schema
    );
    Ok(entity)
}

fn read_function(directory: &Path) -> Result<FunctionManifest> {
    let package = DirectoryPackageView::new(directory.to_owned())?;
    let bytes = package.read_file(&crate::package::PackageRelativePath::new(
        FUNCTION_FILE_NAME,
    )?)?;
    FunctionManifest::parse(std::str::from_utf8(&bytes)?).context("invalid Function artifact")
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
    use std::fs;

    #[test]
    fn requirement_resolution_rejects_missing_or_relative_catalog() {
        let id = PackageId::new("test".to_owned()).unwrap();
        let requirement = VersionReq::STAR;
        assert!(
            resolve_function_requirement(&id, &requirement, "relative")
                .unwrap_err()
                .to_string()
                .contains("must be absolute")
        );
        let missing =
            std::env::temp_dir().join(format!("agl-missing-catalog-{}", uuid::Uuid::now_v7()));
        assert!(
            resolve_function_requirement(&id, &requirement, missing)
                .unwrap_err()
                .to_string()
                .contains("unavailable")
        );
    }

    #[test]
    fn resolves_function_and_dependencies_only_through_forge_catalog() {
        let root = std::env::temp_dir().join(format!("agl-runtime-forge-{}", uuid::Uuid::now_v7()));
        let workspace = root.join("workspace");
        let storage = root.join("forge");
        fs::create_dir_all(workspace.join("src")).unwrap();
        let project = root.join("source");
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
        // An unrelated invalid Function package must not prevent this activation.
        fs::create_dir_all(project.join("entities/unrelated")).unwrap();
        fs::write(
            project.join("entities/unrelated/FUNCTION.toml"),
            "not a Function",
        )
        .unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&project)
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@invalid",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "--quiet"]);
        git(&["add", "."]);
        git(&["commit", "--quiet", "-m", "fixture"]);
        let revision = git(&["rev-parse", "HEAD"]);
        let declarations = [
            ("coder", "function", FUNCTION_SCHEMA),
            ("coder-agent", "agent", AGENT_PAYLOAD_SCHEMA),
            ("test-model", "model", MODEL_SCHEMA),
            ("unrelated", "function", "not-the-function-schema"),
        ]
        .into_iter()
        .map(|(id, kind, schema)| crate::ForgeDeclaration {
            id: id.into(),
            kind: kind.into(),
            schema: schema.into(),
            git: project.to_string_lossy().into_owned(),
            revision: revision.clone(),
            path: format!("entities/{id}"),
        })
        .collect::<Vec<_>>();
        fs::write(
            workspace.join("agentLIBRE.toml"),
            crate::serialize_workspace_forge_declarations(&workspace, declarations.clone())
                .unwrap(),
        )
        .unwrap();
        let mut mutation = ayeque_forge_core::ForgeMutation::begin(&storage, &workspace).unwrap();
        mutation
            .sync(&crate::workspace_forge::backend_declarations(&declarations).unwrap())
            .unwrap();
        drop(mutation);
        let function = storage.join("projects/workspace/entities/coder");
        let resolved = resolve_function_at(&storage, &function, &workspace).unwrap();
        assert_eq!(resolved.manifest.id.as_str(), "coder");
        assert_eq!(resolved.entities.len(), 2);
        drop(resolved);
        ayeque_forge_core::ForgeMutation::begin(&storage, &workspace)
            .unwrap()
            .sync(&[])
            .unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
