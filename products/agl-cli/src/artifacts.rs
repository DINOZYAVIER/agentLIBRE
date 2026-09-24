use super::*;

pub(super) fn add_artifact(
    storage: &Path,
    workspace: &Path,
    registration: ayeque_forge_core::EntityRegistration,
) -> Result<ayeque_forge_core::VerifiedEntity> {
    let id = registration.id().to_owned();
    let mut declarations = load_declarations(workspace)?;
    ensure!(
        declarations.iter().all(|declaration| declaration.id != id),
        "artifact {id:?} is already declared"
    );
    declarations.push(agl_runtime::ArtifactDeclaration {
        id: id.clone(),
        kind: registration.kind().to_owned(),
        schema: registration.schema().to_owned(),
        git: registration.git().to_owned(),
        revision: registration.revision().to_owned(),
        path: registration.path().to_owned(),
    });
    write_and_sync_declarations(storage, workspace, declarations)?;
    let project = ayeque_forge_core::resolve_project_at(storage, workspace)?;
    ayeque_forge_core::resolve_entity(&project, &id)
}

pub(super) fn sync_artifacts(storage: &Path, workspace: &Path) -> Result<()> {
    let declarations = load_declarations(workspace)?;
    write_and_sync_declarations(storage, workspace, declarations.clone())?;
    println!("synchronized {} artifacts", declarations.len());
    Ok(())
}

pub(super) fn remove_artifact(storage: &Path, workspace: &Path, id: &str) -> Result<()> {
    let mut declarations = load_declarations(workspace)?;
    let old_len = declarations.len();
    declarations.retain(|declaration| declaration.id != id);
    ensure!(
        declarations.len() != old_len,
        "artifact {id:?} is not declared"
    );
    write_and_sync_declarations(storage, workspace, declarations)
}

pub(super) fn update_artifact(
    storage: &Path,
    workspace: &Path,
    id: &str,
    kind: Option<String>,
    schema: Option<String>,
    git: Option<String>,
    revision: Option<String>,
    path: Option<String>,
) -> Result<()> {
    ensure!(
        kind.is_some() || schema.is_some() || git.is_some() || revision.is_some() || path.is_some(),
        "provide at least one artifact field to update"
    );
    let mut declarations = load_declarations(workspace)?;
    let declaration = declarations
        .iter_mut()
        .find(|declaration| declaration.id == id)
        .with_context(|| format!("artifact {id:?} is not declared"))?;
    if let Some(value) = kind {
        declaration.kind = value;
    }
    if let Some(value) = schema {
        declaration.schema = value;
    }
    if let Some(value) = git {
        declaration.git = value;
    }
    if let Some(value) = revision {
        declaration.revision = value;
    }
    if let Some(value) = path {
        declaration.path = value;
    }
    write_and_sync_declarations(storage, workspace, declarations)
}

pub(super) fn list_artifacts(workspace: &Path) -> Result<()> {
    let declarations = load_declarations(workspace)?;
    let resolved = agl_runtime::resolve_workspace_artifacts(workspace)?;
    for declaration in declarations {
        let artifact = resolved
            .artifacts
            .iter()
            .find(|artifact| artifact.id == declaration.id)
            .context("resolved artifact is missing from the workspace catalog")?;
        println!(
            "{}\t{}\t{}\t{}\t{}",
            artifact.id,
            artifact.kind,
            declaration.revision,
            artifact.schema,
            artifact.materialized_path.display()
        );
    }
    Ok(())
}

pub(super) fn migrate_artifacts(storage: &Path, workspace: &Path) -> Result<()> {
    let project = ayeque_forge_core::resolve_project_at(storage, workspace)?;
    let lock = ayeque_forge_core::verify_lock(&project)?;
    let declarations = project
        .manifest()
        .entities()
        .iter()
        .map(|entity| {
            let locked = lock
                .entities()
                .iter()
                .find(|entry| entry.id() == entity.id())
                .context("verified Forge lock is missing a declared entity")?;
            Ok(agl_runtime::ArtifactDeclaration {
                id: entity.id().to_owned(),
                kind: entity.kind().to_owned(),
                schema: entity.schema().to_owned(),
                git: entity.git().to_owned(),
                revision: locked.commit().to_owned(),
                path: entity.path().to_owned(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    write_and_sync_declarations(storage, workspace, declarations)?;
    println!(
        "migrated {} artifacts into agentLIBRE.toml",
        lock.entities().len()
    );
    Ok(())
}

fn load_declarations(workspace: &Path) -> Result<Vec<agl_runtime::ArtifactDeclaration>> {
    let root = ayeque_forge_core::discover_workspace(workspace)?;
    let path = root.join(agl_runtime::WORKSPACE_CONFIG_FILE_NAME);
    if !path.exists() {
        return Ok(Vec::new());
    }
    agl_runtime::read_workspace_artifact_declarations(&root)
}

fn write_and_sync_declarations(
    storage: &Path,
    workspace: &Path,
    declarations: Vec<agl_runtime::ArtifactDeclaration>,
) -> Result<()> {
    let root = ayeque_forge_core::discover_workspace(workspace)?;
    let path = root.join(agl_runtime::WORKSPACE_CONFIG_FILE_NAME);
    let previous_bytes = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "agentLIBRE.toml must be a regular file"
            );
            Some(std::fs::read(&path)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let previous_declarations = load_declarations(&root).ok();
    let bytes = agl_runtime::serialize_workspace_artifact_declarations(declarations.clone())?;
    write_root_declaration(&path, &bytes)?;
    if let Err(sync_error) = sync_declarations(storage, &root, &declarations) {
        let root_rollback = match previous_bytes {
            Some(previous) => write_root_declaration(&path, &previous),
            None => std::fs::remove_file(&path).map_err(Into::into),
        };
        let backend_rollback = previous_declarations
            .as_ref()
            .map(|previous| sync_declarations(storage, &root, previous));
        if let Err(root_error) = root_rollback {
            return Err(sync_error).context(format!(
                "artifact sync failed and agentLIBRE.toml rollback failed: {root_error}"
            ));
        }
        if let Some(Err(backend_error)) = backend_rollback {
            return Err(sync_error).context(format!(
                "artifact sync failed and previous artifact state could not be restored: {backend_error}; retry with `agl artifact sync`"
            ));
        }
        return Err(sync_error).context("artifact sync failed after updating agentLIBRE.toml");
    }
    Ok(())
}

fn sync_declarations(
    storage: &Path,
    workspace: &Path,
    declarations: &[agl_runtime::ArtifactDeclaration],
) -> Result<()> {
    let registrations = declarations
        .iter()
        .map(|declaration| {
            ayeque_forge_core::EntityRegistration::new(
                declaration.id.clone(),
                declaration.kind.clone(),
                declaration.schema.clone(),
                declaration.git.clone(),
                declaration.revision.clone(),
                declaration.path.clone(),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    ayeque_forge_core::sync_workspace_entities(storage, workspace, registrations)?;
    Ok(())
}

fn write_root_declaration(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::fs::{self, OpenOptions};
    use std::io::Write as _;

    let parent = path.parent().context("agentLIBRE.toml has no parent")?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "agentLIBRE.toml must be a regular file"
        );
    }
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary =
        path.with_file_name(format!(".agentLIBRE.toml.tmp-{}-{id}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod artifact_tests {
    use super::*;

    #[test]
    fn artifact_add_initializes_catalog_and_resolves_materialization() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let source = temporary.path().join("source");
        let storage = temporary.path().join("forge-data");
        std::fs::create_dir_all(source.join("document")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(source.join("document/README.md"), "# Verified document\n").unwrap();
        assert!(
            ProcessCommand::new("git")
                .args(["init", "--quiet"])
                .current_dir(&source)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            ProcessCommand::new("git")
                .args(["add", "document/README.md"])
                .current_dir(&source)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            ProcessCommand::new("git")
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "fixture"
                ])
                .current_dir(&source)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            ProcessCommand::new("git")
                .args(["init", "--quiet"])
                .current_dir(&workspace)
                .status()
                .unwrap()
                .success()
        );

        let revision = ProcessCommand::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&source)
            .output()
            .unwrap();
        assert!(revision.status.success());
        let revision = String::from_utf8(revision.stdout)
            .unwrap()
            .trim()
            .to_owned();
        let registration = || {
            ayeque_forge_core::EntityRegistration::new(
                "test.document".into(),
                "document".into(),
                "agentlibre.document/v1".into(),
                source.to_string_lossy().into_owned(),
                revision.clone(),
                "document".into(),
            )
            .unwrap()
        };
        let entity = add_artifact(&storage, &workspace, registration()).unwrap();
        assert_eq!(entity.id(), "test.document");
        assert_eq!(
            std::fs::read_to_string(entity.materialized_path().join("README.md")).unwrap(),
            "# Verified document\n"
        );
        assert!(add_artifact(&storage, &workspace, registration()).is_err());
        let project = ayeque_forge_core::resolve_project_at(&storage, &workspace).unwrap();
        assert_eq!(
            ayeque_forge_core::verify_lock(&project)
                .unwrap()
                .entities()
                .len(),
            1
        );
    }
}
