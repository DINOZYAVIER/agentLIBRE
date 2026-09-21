use super::*;

pub(super) fn add_artifact(
    storage: &Path,
    workspace: &Path,
    registration: ayeque_forge_core::EntityRegistration,
) -> Result<ayeque_forge_core::VerifiedEntity> {
    let location = ayeque_forge_core::initialize_project(storage, workspace)?;
    let manifest_path = location.project_path().join("FORGE.toml");
    create_forge_initial_file(&manifest_path, b"format = 2\n")?;
    let project = ayeque_forge_core::resolve_project_at(storage, workspace)?;
    let lock_path = location.project_path().join("FORGE.lock");
    if !lock_path.exists() {
        let initial_lock = ayeque_forge_core::serialize_lock(&project, Vec::new())?;
        create_forge_initial_file(&lock_path, &initial_lock)?;
    }
    let id = registration.id().to_owned();
    let updated = ayeque_forge_core::register_entity(&project, registration)?;
    ayeque_forge_core::resolve_entity(&updated, &id)
}

fn create_forge_initial_file(path: &Path, bytes: &[u8]) -> Result<()> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            let written = file.write_all(bytes).and_then(|_| file.sync_all());
            if let Err(error) = written {
                drop(file);
                let _ = std::fs::remove_file(path);
                return Err(error)
                    .with_context(|| format!("failed to initialize {}", path.display()));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to create {}", path.display())),
    }
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

        let registration = || {
            ayeque_forge_core::EntityRegistration::new(
                "test.document".into(),
                "document".into(),
                "agentlibre.document/v1".into(),
                source.to_string_lossy().into_owned(),
                "HEAD".into(),
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
