use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
#[cfg(test)]
use ayeque_forge_core::resolve_project_at;
use ayeque_forge_core::{discover_workspace, resolve_entity, resolve_project};
use serde::Deserialize;

pub const WORKSPACE_CONFIG_FILE_NAME: &str = "agentLIBRE.toml";
const WORKSPACE_CONFIG_FORMAT: u32 = 1;
const MEMORY_KIND: &str = "memory";
const MEMORY_SCHEMA: &str = "agentlibre.memory/v1";
const DOCUMENT_KIND: &str = "document";
const DOCUMENT_SCHEMA: &str = "agentlibre.document/v1";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceConfig {
    format: u32,
    artifacts: ArtifactDeclarations,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactDeclarations {
    memory: String,
    documents: Vec<String>,
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
    pub memory: ResolvedWorkspaceArtifact,
    pub documents: Vec<ResolvedWorkspaceArtifact>,
}

/// Resolve the Forge-owned artifact roles declared by the workspace root.
pub fn resolve_workspace_artifacts(start: impl AsRef<Path>) -> Result<WorkspaceArtifacts> {
    let workspace = discover_workspace(start.as_ref())?;
    let project = resolve_project(&workspace)?;
    resolve_workspace_artifacts_from_project(&workspace, &project)
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
    let memory = resolve_artifact(
        project,
        &declarations.artifacts.memory,
        MEMORY_KIND,
        MEMORY_SCHEMA,
    )?;
    let documents = declarations
        .artifacts
        .documents
        .iter()
        .map(|id| resolve_artifact(project, id, DOCUMENT_KIND, DOCUMENT_SCHEMA))
        .collect::<Result<Vec<_>>>()?;

    Ok(WorkspaceArtifacts {
        workspace: workspace.to_owned(),
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
    Ok(config)
}

fn resolve_artifact(
    project: &ayeque_forge_core::VerifiedProject,
    id: &str,
    expected_kind: &str,
    expected_schema: &str,
) -> Result<ResolvedWorkspaceArtifact> {
    let entity = resolve_entity(project, id)
        .with_context(|| format!("failed to resolve workspace artifact {id:?}"))?;
    ensure!(
        entity.kind() == expected_kind && entity.schema() == expected_schema,
        "workspace artifact {id:?} has kind/schema {}/{}; expected {expected_kind}/{expected_schema}",
        entity.kind(),
        entity.schema()
    );
    Ok(ResolvedWorkspaceArtifact {
        id: entity.id().to_owned(),
        kind: entity.kind().to_owned(),
        schema: entity.schema().to_owned(),
        materialized_path: entity.materialized_path().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ayeque_forge_core::{LockEntry, serialize_lock, workspace_identity};
    use std::process::Command;

    const OBJECT: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn resolves_memory_and_documents_to_verified_materialized_paths() {
        let fixture = Fixture::new(&[
            ("agentlibre.memory", MEMORY_KIND, MEMORY_SCHEMA),
            ("agentlibre.doc.one", DOCUMENT_KIND, DOCUMENT_SCHEMA),
            ("agentlibre.doc.two", DOCUMENT_KIND, DOCUMENT_SCHEMA),
        ]);
        fixture.write_config(
            "format = 1\n\n[artifacts]\nmemory = \"agentlibre.memory\"\ndocuments = [\"agentlibre.doc.one\", \"agentlibre.doc.two\"]\n",
        );

        let resolved =
            resolve_workspace_artifacts_at(&fixture.storage, &fixture.workspace).unwrap();
        assert_eq!(
            resolved.workspace,
            fixture.workspace.canonicalize().unwrap()
        );
        assert_eq!(resolved.memory.id, "agentlibre.memory");
        assert_eq!(
            resolved.memory.materialized_path,
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
            "format = 2\n\n[artifacts]\nmemory = \"agentlibre.memory\"\ndocuments = []\n",
        );
        assert!(resolve_workspace_artifacts_at(&fixture.storage, &fixture.workspace).is_err());

        fixture.write_config("format = 1\nextra = true\n\n[artifacts]\nmemory = \"agentlibre.memory\"\ndocuments = []\n");
        assert!(resolve_workspace_artifacts_at(&fixture.storage, &fixture.workspace).is_err());

        fixture.write_config("format = 1\n\n[artifacts]\nmemory = \"agentlibre.memory\"\ndocuments = []\nunknown = true\n");
        assert!(resolve_workspace_artifacts_at(&fixture.storage, &fixture.workspace).is_err());
    }

    #[test]
    fn rejects_missing_ids_and_mismatched_kind_or_schema() {
        let missing = Fixture::new(&[("agentlibre.memory", MEMORY_KIND, MEMORY_SCHEMA)]);
        missing.write_config(
            "format = 1\n\n[artifacts]\nmemory = \"agentlibre.missing\"\ndocuments = []\n",
        );
        assert!(resolve_workspace_artifacts_at(&missing.storage, &missing.workspace).is_err());

        let wrong_kind = Fixture::new(&[("agentlibre.memory", DOCUMENT_KIND, DOCUMENT_SCHEMA)]);
        wrong_kind.write_config(
            "format = 1\n\n[artifacts]\nmemory = \"agentlibre.memory\"\ndocuments = []\n",
        );
        assert!(
            resolve_workspace_artifacts_at(&wrong_kind.storage, &wrong_kind.workspace).is_err()
        );

        let wrong_schema =
            Fixture::new(&[("agentlibre.memory", MEMORY_KIND, "agentlibre.memory/v2")]);
        wrong_schema.write_config(
            "format = 1\n\n[artifacts]\nmemory = \"agentlibre.memory\"\ndocuments = []\n",
        );
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
        fixture.write_config(
            "format = 1\n\n[artifacts]\nmemory = \"agentlibre.memory\"\ndocuments = [\"agentlibre.doc.one\"]\n",
        );

        let resolved =
            resolve_workspace_artifacts_at(&fixture.storage, &fixture.workspace.join("src/nested"))
                .unwrap();
        assert_eq!(
            resolved.workspace,
            fixture.workspace.canonicalize().unwrap()
        );
        assert_eq!(resolved.memory.id, "agentlibre.memory");
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
                        "[[entity]]\nid = \"{id}\"\nkind = \"{kind}\"\nschema = \"{schema}\"\ngit = \"/source/{id}\"\nrevision = \"main\"\n"
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
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
