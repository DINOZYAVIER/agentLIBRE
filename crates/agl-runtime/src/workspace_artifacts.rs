use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
#[cfg(test)]
use ayeque_forge_core::resolve_project_at;
use ayeque_forge_core::{discover_workspace, resolve_entity, resolve_project, verify_lock};
use serde::{Deserialize, Serialize};

pub const WORKSPACE_CONFIG_FILE_NAME: &str = "agentLIBRE.toml";
const WORKSPACE_CONFIG_FORMAT: u32 = 2;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceConfig {
    format: u32,
    artifacts: ArtifactDeclarations,
    #[serde(default, skip_serializing_if = "WorkspaceSettings::is_empty")]
    workspace: WorkspaceSettings,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceSettings {
    #[serde(default)]
    output_directories: Vec<String>,
}

impl WorkspaceSettings {
    fn is_empty(&self) -> bool {
        self.output_directories.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ArtifactDeclarations {
    #[serde(default)]
    entity: Vec<ArtifactDeclaration>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDeclaration {
    pub id: String,
    pub kind: String,
    pub schema: String,
    pub git: String,
    pub revision: String,
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedWorkspaceArtifact {
    pub id: String,
    pub kind: String,
    pub schema: String,
    pub materialized_path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceArtifacts {
    pub workspace: PathBuf,
    pub artifacts: Vec<ResolvedWorkspaceArtifact>,
    pub memory: Option<ResolvedWorkspaceArtifact>,
    pub documents: Vec<ResolvedWorkspaceArtifact>,
}

/// Resolve the Forge-owned artifact roles declared by the workspace root.
pub fn resolve_workspace_artifacts(start: impl AsRef<Path>) -> Result<WorkspaceArtifacts> {
    let workspace = discover_workspace(start.as_ref())?;
    let project = resolve_project(&workspace)?;
    resolve_workspace_artifacts_from_project(&workspace, &project)
}

pub fn read_workspace_artifact_declarations(
    start: impl AsRef<Path>,
) -> Result<Vec<ArtifactDeclaration>> {
    let workspace = discover_workspace(start.as_ref())?;
    Ok(read_workspace_config(&workspace)?.artifacts.entity)
}

pub fn serialize_workspace_artifact_declarations(
    workspace: &Path,
    entities: Vec<ArtifactDeclaration>,
) -> Result<Vec<u8>> {
    let mut ids = BTreeSet::new();
    for entity in &entities {
        ensure!(
            ids.insert(entity.id.as_str()),
            "duplicate artifact id {:?}",
            entity.id
        );
        ensure!(
            is_git_object_id(&entity.revision),
            "artifact {:?} revision must be a full Git commit SHA",
            entity.id
        );
        ayeque_forge_core::EntityRegistration::new(
            entity.id.clone(),
            entity.kind.clone(),
            entity.schema.clone(),
            entity.git.clone(),
            entity.revision.clone(),
            entity.path.clone(),
        )?;
    }
    let mut config = if workspace.join(WORKSPACE_CONFIG_FILE_NAME).try_exists()? {
        read_workspace_config(workspace)?
    } else {
        WorkspaceConfig {
            format: WORKSPACE_CONFIG_FORMAT,
            artifacts: ArtifactDeclarations { entity: Vec::new() },
            workspace: WorkspaceSettings::default(),
        }
    };
    config.artifacts.entity = entities;
    Ok(toml::to_string_pretty(&config)
        .context("failed to serialize agentLIBRE.toml")?
        .into_bytes())
}

/// Read explicit build-output trees from the caller's workspace root.
pub fn read_workspace_output_directories(workspace: &Path) -> Result<Vec<String>> {
    let path = workspace.join(WORKSPACE_CONFIG_FILE_NAME);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(metadata.is_file(), "agentLIBRE.toml must be a regular file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    }
    Ok(read_workspace_config(workspace)?
        .workspace
        .output_directories)
}

#[cfg(test)]
fn resolve_workspace_artifacts_at(storage_root: &Path, start: &Path) -> Result<WorkspaceArtifacts> {
    let workspace = discover_workspace(start)?;
    let project = resolve_project_at(storage_root, &workspace)?;
    resolve_workspace_artifacts_from_project(&workspace, &project)
}

fn resolve_workspace_artifacts_from_project(
    workspace: &Path,
    project: &ayeque_forge_core::VerifiedProject,
) -> Result<WorkspaceArtifacts> {
    let declarations = read_workspace_config(workspace)?;
    let lock = verify_lock(project)
        .context("workspace artifact state is stale; run `agl artifact sync`")?;
    let mut ids = BTreeSet::new();
    let mut artifacts = Vec::with_capacity(declarations.artifacts.entity.len());
    let mut memory = None;
    let mut documents = Vec::new();
    ensure!(
        declarations.artifacts.entity.len() == project.manifest().entities().len(),
        "agentLIBRE.toml and the AGL artifact catalog contain different entity counts; run `agl artifact sync`"
    );
    for declaration in declarations.artifacts.entity {
        ensure!(
            ids.insert(declaration.id.clone()),
            "duplicate artifact id {:?}",
            declaration.id
        );
        ensure!(
            is_git_object_id(&declaration.revision),
            "artifact {:?} revision must be a full Git commit SHA",
            declaration.id
        );
        let manifest_entity = project.manifest_entity(&declaration.id)?;
        ensure!(
            manifest_entity.kind() == declaration.kind
                && manifest_entity.schema() == declaration.schema
                && manifest_entity.git() == declaration.git
                && manifest_entity.revision() == declaration.revision
                && manifest_entity.path() == declaration.path,
            "artifact {:?} differs between agentLIBRE.toml and the AGL artifact catalog; run `agl artifact sync`",
            declaration.id
        );
        let lock_entry = lock
            .entities()
            .iter()
            .find(|entry| entry.id() == declaration.id)
            .context("verified artifact lock is missing a declared entity")?;
        ensure!(
            lock_entry.commit() == declaration.revision,
            "artifact {:?} lock does not match its pinned commit; run `agl artifact sync`",
            declaration.id
        );
        let resolved: ResolvedWorkspaceArtifact = resolve_entity(project, &declaration.id)?.into();
        match declaration.kind.as_str() {
            "memory" => {
                ensure!(memory.is_none(), "only one memory artifact may be declared");
                memory = Some(resolved.clone());
            }
            "document" => documents.push(resolved.clone()),
            _ => {}
        }
        artifacts.push(resolved);
    }

    Ok(WorkspaceArtifacts {
        workspace: workspace.to_owned(),
        artifacts,
        memory,
        documents,
    })
}

fn read_workspace_config(workspace: &Path) -> Result<WorkspaceConfig> {
    let path = workspace.join(WORKSPACE_CONFIG_FILE_NAME);
    let bytes = fs::read(&path).with_context(|| {
        format!(
            "failed to read required workspace declaration {}",
            path.display()
        )
    })?;
    let config: WorkspaceConfig = toml::from_slice(&bytes)
        .with_context(|| format!("invalid workspace declaration {}", path.display()))?;
    ensure!(
        config.format == WORKSPACE_CONFIG_FORMAT,
        "unsupported workspace declaration format {}",
        config.format
    );
    for directory in &config.workspace.output_directories {
        ensure!(
            !directory.contains('\0'),
            "output directory must contain no NUL"
        );
        let path = agl_core::implementation_plan::WorkspacePath::new(directory.clone())
            .map_err(anyhow::Error::msg)?;
        ensure!(
            Path::new(path.as_str()) != Path::new(".")
                && Path::new(path.as_str()) != Path::new(WORKSPACE_CONFIG_FILE_NAME),
            "output directory cannot exclude the workspace root or agentLIBRE.toml"
        );
    }
    Ok(config)
}

fn is_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        && value.bytes().all(|byte| !byte.is_ascii_uppercase())
}

impl From<ayeque_forge_core::VerifiedEntity> for ResolvedWorkspaceArtifact {
    fn from(entity: ayeque_forge_core::VerifiedEntity) -> Self {
        Self {
            id: entity.id().to_owned(),
            kind: entity.kind().to_owned(),
            schema: entity.schema().to_owned(),
            materialized_path: entity.materialized_path().to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ayeque_forge_core::{LockEntry, serialize_lock, workspace_identity};
    use std::process::Command;

    const MEMORY_KIND: &str = "memory";
    const MEMORY_SCHEMA: &str = "agentlibre.memory/v1";
    const DOCUMENT_KIND: &str = "document";
    const DOCUMENT_SCHEMA: &str = "agentlibre.document/v1";

    const OBJECT: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn output_scope_rejects_traversal_and_root_manifest_exclusion() {
        let root = std::env::temp_dir().join(format!("agl-output-scope-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&root).unwrap();
        assert!(read_workspace_output_directories(&root).unwrap().is_empty());
        for directory in [
            ".",
            "../target",
            "/target",
            "agentLIBRE.toml",
            "agentLIBRE.toml/.",
            "",
        ] {
            fs::write(
                root.join(WORKSPACE_CONFIG_FILE_NAME),
                format!(
                    "format = 2\n[artifacts]\n[workspace]\noutput_directories = [{directory:?}]\n"
                ),
            )
            .unwrap();
            assert!(
                read_workspace_output_directories(&root).is_err(),
                "accepted output path {directory}"
            );
        }
        fs::write(
            root.join(WORKSPACE_CONFIG_FILE_NAME),
            "format = 2\n[artifacts]\n",
        )
        .unwrap();
        assert!(read_workspace_output_directories(&root).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolves_memory_and_documents_to_verified_materialized_paths() {
        let fixture = Fixture::new(&[
            ("agentlibre.memory", MEMORY_KIND, MEMORY_SCHEMA),
            ("agentlibre.doc.one", DOCUMENT_KIND, DOCUMENT_SCHEMA),
            ("agentlibre.doc.two", DOCUMENT_KIND, DOCUMENT_SCHEMA),
        ]);
        fixture.write_config(&fixture.declarations_config(&[
            ("agentlibre.memory", MEMORY_KIND, MEMORY_SCHEMA),
            ("agentlibre.doc.one", DOCUMENT_KIND, DOCUMENT_SCHEMA),
            ("agentlibre.doc.two", DOCUMENT_KIND, DOCUMENT_SCHEMA),
        ]));

        let resolved =
            resolve_workspace_artifacts_at(&fixture.storage, &fixture.workspace).unwrap();
        assert_eq!(
            resolved.workspace,
            fixture.workspace.canonicalize().unwrap()
        );
        let memory = resolved.memory.as_ref().unwrap();
        assert_eq!(memory.id, "agentlibre.memory");
        assert_eq!(
            memory.materialized_path,
            fixture.project.join("entities/agentlibre.memory")
        );
        assert_eq!(
            resolved
                .documents
                .iter()
                .map(|artifact| artifact.id.as_str())
                .collect::<Vec<_>>(),
            ["agentlibre.doc.one", "agentlibre.doc.two"]
        );
    }

    #[test]
    fn requires_the_workspace_root_declaration() {
        let fixture = Fixture::new(&[("agentlibre.memory", MEMORY_KIND, MEMORY_SCHEMA)]);

        let error =
            resolve_workspace_artifacts_at(&fixture.storage, &fixture.workspace).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("required workspace declaration"));
        assert!(message.contains(WORKSPACE_CONFIG_FILE_NAME));
    }

    #[test]
    fn rejects_wrong_format_and_unknown_fields() {
        let fixture = Fixture::new(&[("agentlibre.memory", MEMORY_KIND, MEMORY_SCHEMA)]);
        fixture.write_config(
            "format = 1\n\n[artifacts]\nmemory = \"agentlibre.memory\"\ndocuments = []\n",
        );
        assert!(resolve_workspace_artifacts_at(&fixture.storage, &fixture.workspace).is_err());

        fixture.write_config("format = 2\nextra = true\n\n[artifacts]\nentity = []\n");
        assert!(resolve_workspace_artifacts_at(&fixture.storage, &fixture.workspace).is_err());

        fixture.write_config("format = 2\n\n[artifacts]\nunknown = true\n");
        assert!(resolve_workspace_artifacts_at(&fixture.storage, &fixture.workspace).is_err());
    }

    #[test]
    fn rejects_missing_ids_and_mismatched_kind_or_schema() {
        let missing = Fixture::new(&[("agentlibre.memory", MEMORY_KIND, MEMORY_SCHEMA)]);
        missing.write_config(&missing.declarations_config(&[(
            "agentlibre.missing",
            MEMORY_KIND,
            MEMORY_SCHEMA,
        )]));
        assert!(resolve_workspace_artifacts_at(&missing.storage, &missing.workspace).is_err());

        let wrong_kind = Fixture::new(&[("agentlibre.memory", DOCUMENT_KIND, DOCUMENT_SCHEMA)]);
        wrong_kind.write_config(&wrong_kind.declarations_config(&[(
            "agentlibre.memory",
            MEMORY_KIND,
            MEMORY_SCHEMA,
        )]));
        assert!(
            resolve_workspace_artifacts_at(&wrong_kind.storage, &wrong_kind.workspace).is_err()
        );

        let wrong_schema =
            Fixture::new(&[("agentlibre.memory", MEMORY_KIND, "agentlibre.memory/v2")]);
        wrong_schema.write_config(&wrong_schema.declarations_config(&[(
            "agentlibre.memory",
            MEMORY_KIND,
            MEMORY_SCHEMA,
        )]));
        assert!(
            resolve_workspace_artifacts_at(&wrong_schema.storage, &wrong_schema.workspace).is_err()
        );
    }

    #[test]
    fn discovers_the_root_when_invoked_from_a_subdirectory() {
        let fixture = Fixture::new(&[
            ("agentlibre.memory", MEMORY_KIND, MEMORY_SCHEMA),
            ("agentlibre.doc.one", DOCUMENT_KIND, DOCUMENT_SCHEMA),
        ]);
        fixture.write_config(&fixture.declarations_config(&[
            ("agentlibre.memory", MEMORY_KIND, MEMORY_SCHEMA),
            ("agentlibre.doc.one", DOCUMENT_KIND, DOCUMENT_SCHEMA),
        ]));

        let resolved =
            resolve_workspace_artifacts_at(&fixture.storage, &fixture.workspace.join("src/nested"))
                .unwrap();
        assert_eq!(
            resolved.workspace,
            fixture.workspace.canonicalize().unwrap()
        );
        assert_eq!(resolved.memory.unwrap().id, "agentlibre.memory");
    }

    struct Fixture {
        root: PathBuf,
        workspace: PathBuf,
        storage: PathBuf,
        project: PathBuf,
    }

    impl Fixture {
        fn new(entities: &[(&str, &str, &str)]) -> Self {
            let root = std::env::temp_dir()
                .join(format!("agl-workspace-artifacts-{}", uuid::Uuid::now_v7()));
            let workspace = root.join("workspace");
            let storage = root.join("forge");
            fs::create_dir_all(workspace.join("src/nested")).unwrap();
            fs::create_dir_all(&storage).unwrap();
            let status = Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&workspace)
                .status()
                .unwrap();
            assert!(status.success());

            let workspace = workspace.canonicalize().unwrap();
            let project = storage
                .join("projects")
                .join(workspace.file_name().unwrap());
            fs::create_dir_all(project.join("entities")).unwrap();
            fs::write(
                project.join("project.toml"),
                format!(
                    "format = 1\nworkspace_sha256 = \"{}\"\n",
                    workspace_identity(&workspace)
                ),
            )
            .unwrap();

            let manifest = entities
                .iter()
                .map(|(id, kind, schema)| {
                    format!(
                        "[[entity]]\nid = \"{id}\"\nkind = \"{kind}\"\nschema = \"{schema}\"\ngit = \"/source/{id}\"\nrevision = \"{OBJECT}\"\npath = \".\"\n"
                    )
                })
                .collect::<String>();
            fs::write(
                project.join("FORGE.toml"),
                format!("format = 2\n{manifest}"),
            )
            .unwrap();
            let project_value = resolve_project_at(&storage, &workspace).unwrap();
            let entries = entities
                .iter()
                .map(|(id, _, _)| {
                    LockEntry::new(
                        (*id).to_owned(),
                        format!("/source/{id}"),
                        OBJECT.to_owned(),
                        ".".to_owned(),
                        OBJECT.to_owned(),
                    )
                    .unwrap()
                })
                .collect();
            fs::write(
                project.join("FORGE.lock"),
                serialize_lock(&project_value, entries).unwrap(),
            )
            .unwrap();
            for (id, _, _) in entities {
                fs::create_dir_all(project.join("entities").join(id)).unwrap();
            }

            Self {
                root,
                workspace,
                storage,
                project,
            }
        }

        fn write_config(&self, source: &str) {
            fs::write(self.workspace.join(WORKSPACE_CONFIG_FILE_NAME), source).unwrap();
        }

        fn declarations_config(&self, entities: &[(&str, &str, &str)]) -> String {
            let entities = entities
                .iter()
                .map(|(id, kind, schema)| {
                    format!(
                        "[[artifacts.entity]]\nid = \"{id}\"\nkind = \"{kind}\"\nschema = \"{schema}\"\ngit = \"/source/{id}\"\nrevision = \"{OBJECT}\"\npath = \".\"\n"
                    )
                })
                .collect::<String>();
            format!("format = 2\n\n[artifacts]\n\n{entities}")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
