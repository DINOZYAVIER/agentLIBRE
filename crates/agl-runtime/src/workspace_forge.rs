use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use ayeque_forge_core::{ForgeWorkspace, discover_workspace};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const WORKSPACE_CONFIG_FILE_NAME: &str = "agentLIBRE.toml";
const WORKSPACE_CONFIG_FORMAT: u32 = 2;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceConfig {
    format: u32,
    #[serde(default)]
    forge: Vec<ForgeDeclaration>,
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

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ForgeDeclaration {
    pub id: String,
    pub kind: String,
    pub schema: String,
    pub git: String,
    pub revision: String,
    pub path: String,
}

#[derive(Clone)]
pub struct ResolvedForgeDeclaration {
    pub id: String,
    pub kind: String,
    pub schema: String,
    pub materialized_path: PathBuf,
    reader: Arc<ForgeWorkspace>,
}

impl std::fmt::Debug for ResolvedForgeDeclaration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedForgeDeclaration")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("schema", &self.schema)
            .field("materialized_path", &self.materialized_path)
            .finish()
    }
}

impl ResolvedForgeDeclaration {
    pub fn open_file(&self, relative: &Path) -> Result<agl_tool_api::ForgeFile> {
        let (file, guard, status) = self.reader.open_file(&self.id, relative)?;
        let status = match status {
            ayeque_forge_core::VerificationStatus::Verified => {
                agl_tool_api::VerificationStatus::Verified
            }
            ayeque_forge_core::VerificationStatus::Unchecked => {
                agl_tool_api::VerificationStatus::Unchecked
            }
        };
        Ok(agl_tool_api::ForgeFile::new(file, guard, status))
    }
}

#[derive(Clone, Debug)]
pub struct WorkspaceForge {
    pub workspace: PathBuf,
    pub declarations: Vec<ResolvedForgeDeclaration>,
    pub memory: Option<ResolvedForgeDeclaration>,
    pub documents: Vec<ResolvedForgeDeclaration>,
}

/// Resolve the Forge-owned artifact roles declared by the workspace root.
pub fn resolve_workspace_forge(start: impl AsRef<Path>) -> Result<WorkspaceForge> {
    let workspace = discover_workspace(start.as_ref())?;
    resolve_workspace_forge_at(&ayeque_forge_core::data_root()?, &workspace)
}

pub fn read_workspace_forge_declarations(start: impl AsRef<Path>) -> Result<Vec<ForgeDeclaration>> {
    let workspace = discover_workspace(start.as_ref())?;
    read_workspace_forge_declarations_at(&workspace)
}

/// Read declarations at an already selected workspace root without Git discovery.
pub fn read_workspace_forge_declarations_at(workspace: &Path) -> Result<Vec<ForgeDeclaration>> {
    Ok(read_workspace_config(workspace)?.forge)
}

pub fn serialize_workspace_forge_declarations(
    workspace: &Path,
    entities: Vec<ForgeDeclaration>,
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
        ensure!(
            !entity.kind.is_empty() && !entity.schema.is_empty(),
            "Forge kind/schema must be nonempty AGL metadata"
        );
        ayeque_forge_core::ForgeDeclaration::new(
            entity.id.clone(),
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
            forge: Vec::new(),
            workspace: WorkspaceSettings::default(),
        }
    };
    config.forge = entities;
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

pub(crate) fn resolve_workspace_forge_at(
    storage_root: &Path,
    start: &Path,
) -> Result<WorkspaceForge> {
    let workspace = discover_workspace(start)?;
    let declarations = read_workspace_config(&workspace)?.forge;
    let sources = backend_declarations(&declarations)?;
    let reader = Arc::new(ForgeWorkspace::open(storage_root, &workspace, &sources)?);
    let mut artifacts = Vec::new();
    let mut memory = None;
    let mut documents = Vec::new();
    for declaration in declarations {
        let resolved = ResolvedForgeDeclaration {
            materialized_path: reader.materialized_path(&declaration.id)?.to_owned(),
            id: declaration.id,
            kind: declaration.kind,
            schema: declaration.schema,
            reader: reader.clone(),
        };
        match resolved.kind.as_str() {
            "memory" => {
                ensure!(memory.is_none(), "only one memory artifact may be declared");
                memory = Some(resolved.clone());
            }
            "document" => documents.push(resolved.clone()),
            _ => {}
        }
        artifacts.push(resolved);
    }
    Ok(WorkspaceForge {
        workspace,
        declarations: artifacts,
        memory,
        documents,
    })
}

pub fn backend_declarations(
    declarations: &[ForgeDeclaration],
) -> Result<Vec<ayeque_forge_core::ForgeDeclaration>> {
    let mut ids = BTreeSet::new();
    declarations
        .iter()
        .map(|source| {
            ensure!(ids.insert(&source.id), "duplicate Forge id");
            ensure!(
                is_git_object_id(&source.revision),
                "Forge revision must be a full Git commit"
            );
            ensure!(
                !source.kind.is_empty() && !source.schema.is_empty(),
                "Forge kind/schema must be nonempty AGL metadata"
            );
            ayeque_forge_core::ForgeDeclaration::new(
                source.id.clone(),
                source.git.clone(),
                source.revision.clone(),
                source.path.clone(),
            )
        })
        .collect()
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

#[cfg(test)]
mod tests {
    #[test]
    fn exact_root_reader_does_not_require_git_discovery() {
        let root = std::env::temp_dir().join(format!(
            "agl-forge-exact-root-{}",
            agl_core::RequestId::generate()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(super::WORKSPACE_CONFIG_FILE_NAME), "format = 2\n").unwrap();
        assert!(
            super::read_workspace_forge_declarations_at(&root)
                .unwrap()
                .is_empty()
        );
        assert!(!root.join(".git").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    use super::*;
    use std::process::Command;

    const MEMORY_KIND: &str = "memory";
    const MEMORY_SCHEMA: &str = "agentlibre.memory/v1";
    const DOCUMENT_KIND: &str = "document";
    const DOCUMENT_SCHEMA: &str = "agentlibre.document/v1";

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
                    "format = 2\nforge = []\n[workspace]\noutput_directories = [{directory:?}]\n"
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
            "format = 2\nforge = []\n",
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

        let resolved = resolve_workspace_forge_at(&fixture.storage, &fixture.workspace).unwrap();
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

        let error = resolve_workspace_forge_at(&fixture.storage, &fixture.workspace).unwrap_err();
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
        assert!(resolve_workspace_forge_at(&fixture.storage, &fixture.workspace).is_err());

        fixture.write_config("format = 2\n\n[[artifacts.entity]]\nid = \"test\"\n");
        assert!(resolve_workspace_forge_at(&fixture.storage, &fixture.workspace).is_err());
        fixture.write_config("format = 2\nextra = true\nforge = []\n");
        assert!(resolve_workspace_forge_at(&fixture.storage, &fixture.workspace).is_err());

        fixture.write_config("format = 2\n\n[artifacts]\nunknown = true\n");
        assert!(resolve_workspace_forge_at(&fixture.storage, &fixture.workspace).is_err());
    }

    #[test]
    fn missing_source_is_rejected_and_kind_schema_belong_to_agl() {
        let fixture = Fixture::new(&[("agentlibre.memory", MEMORY_KIND, MEMORY_SCHEMA)]);
        fixture.write_config(&fixture.declarations_config(&[(
            "missing",
            MEMORY_KIND,
            MEMORY_SCHEMA,
        )]));
        assert!(resolve_workspace_forge_at(&fixture.storage, &fixture.workspace).is_err());
        fixture.write_config(&fixture.declarations_config(&[(
            "agentlibre.memory",
            DOCUMENT_KIND,
            DOCUMENT_SCHEMA,
        )]));
        let resolved = resolve_workspace_forge_at(&fixture.storage, &fixture.workspace).unwrap();
        assert!(resolved.memory.is_none());
        assert_eq!(resolved.documents.len(), 1);
        assert_eq!(resolved.documents[0].schema, DOCUMENT_SCHEMA);
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
            resolve_workspace_forge_at(&fixture.storage, &fixture.workspace.join("src/nested"))
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
        source: PathBuf,
        revision: String,
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
            let source = root.join("source");
            fs::create_dir(&source).unwrap();
            for (id, _, _) in entities {
                fs::create_dir(source.join(id)).unwrap();
                fs::write(source.join(id).join("README.md"), "fixture").unwrap();
            }
            let git = |args: &[&str]| {
                let output = Command::new("git")
                    .arg("-C")
                    .arg(&source)
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
            let declarations = entities
                .iter()
                .map(|(id, kind, schema)| ForgeDeclaration {
                    id: (*id).into(),
                    kind: (*kind).into(),
                    schema: (*schema).into(),
                    git: source.to_string_lossy().into_owned(),
                    revision: revision.clone(),
                    path: (*id).into(),
                })
                .collect::<Vec<_>>();
            let mut mutation =
                ayeque_forge_core::ForgeMutation::begin(&storage, &workspace).unwrap();
            mutation
                .sync(&backend_declarations(&declarations).unwrap())
                .unwrap();
            drop(mutation);
            let project = storage.join("projects/workspace");
            Self {
                root,
                workspace,
                storage,
                project,
                source,
                revision,
            }
        }

        fn write_config(&self, source: &str) {
            fs::write(self.workspace.join(WORKSPACE_CONFIG_FILE_NAME), source).unwrap();
        }

        fn declarations_config(&self, entities: &[(&str, &str, &str)]) -> String {
            let git = self.source.to_string_lossy();
            let revision = &self.revision;
            let entities = entities
                .iter()
                .map(|(id, kind, schema)| {
                    format!(
                        "[[forge]]\nid = \"{id}\"\nkind = \"{kind}\"\nschema = \"{schema}\"\ngit = \"{git}\"\nrevision = \"{revision}\"\npath = \"{id}\"\n"
                    )
                })
                .collect::<String>();
            format!("format = 2\n\n{entities}")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Ok(mut mutation) =
                ayeque_forge_core::ForgeMutation::begin(&self.storage, &self.workspace)
            {
                let _ = mutation.sync(&[]);
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
