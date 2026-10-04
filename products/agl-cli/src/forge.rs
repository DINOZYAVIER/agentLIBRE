use super::*;

pub(super) fn set_verification_choice(
    data_root: &Path,
    workspace: &Path,
    id: &str,
    file: &str,
    decision: ForgeVerificationDecision,
) -> Result<()> {
    use agl_runtime::forge_verification::{
        VerificationChoices, VerificationDecision, VerificationKey,
    };
    let workspace = workspace
        .canonicalize()
        .context("verification workspace is unavailable")?;
    let declarations = agl_runtime::read_workspace_forge_declarations(&workspace)?;
    let source = declarations
        .iter()
        .find(|source| source.id == id)
        .with_context(|| format!("Forge source {id:?} is not declared"))?;
    let key = VerificationKey::new(&workspace, source, file)?;
    let decision = match decision {
        ForgeVerificationDecision::Verify => VerificationDecision::Verify,
        ForgeVerificationDecision::Skip => VerificationDecision::Skip,
    };
    VerificationChoices::new(data_root)?.set(key, decision)?;
    println!(
        "{id}\t{file}\t{}",
        match decision {
            VerificationDecision::Verify => "verify",
            VerificationDecision::Skip => "skip",
        }
    );
    Ok(())
}

pub(super) struct ForgeOptions {
    pub data_root: PathBuf,
    pub timeout: std::time::Duration,
}

pub(super) fn options(config_root: &Path, data_root: &Path) -> Result<ForgeOptions> {
    let seconds = if config_root.join(config::CONFIG_FILE_NAME).try_exists()? {
        config::load_document(config_root)?
            .0
            .forge
            .sync_timeout_seconds
    } else {
        300
    };
    Ok(ForgeOptions {
        data_root: data_root.to_owned(),
        timeout: std::time::Duration::from_secs(seconds),
    })
}

pub(super) fn add_forge_declaration(
    storage: &Path,
    workspace: &Path,
    source: agl_runtime::ForgeDeclaration,
    options: &ForgeOptions,
) -> Result<PathBuf> {
    let root = ayeque_forge_core::discover_workspace(workspace)?;
    let mutation = ayeque_forge_core::ForgeMutation::begin(storage, &root)?;
    let mut declarations = load_declarations(&root)?;
    ensure!(
        declarations.iter().all(|entry| entry.id != source.id),
        "Forge source is already declared"
    );
    let id = source.id.clone();
    declarations.push(source);
    write_and_sync_declarations(mutation, storage, &root, declarations, options)?;
    let sources = agl_runtime::workspace_backend_declarations(&load_declarations(&root)?)?;
    let reader = ayeque_forge_core::ForgeWorkspace::open(storage, &root, &sources)?;
    Ok(reader.materialized_path(&id)?.to_owned())
}

pub(super) fn sync_forge_declarations(
    storage: &Path,
    workspace: &Path,
    options: &ForgeOptions,
) -> Result<()> {
    let root = ayeque_forge_core::discover_workspace(workspace)?;
    let mutation = ayeque_forge_core::ForgeMutation::begin(storage, &root)?;
    let declarations = load_declarations(&root)?;
    let count = declarations.len();
    sync_with_choices(mutation, storage, &root, declarations, options)?;
    println!("synchronized {count} artifacts");
    Ok(())
}

pub(super) fn remove_forge_declaration(
    storage: &Path,
    workspace: &Path,
    id: &str,
    options: &ForgeOptions,
) -> Result<()> {
    let root = ayeque_forge_core::discover_workspace(workspace)?;
    let mutation = ayeque_forge_core::ForgeMutation::begin(storage, &root)?;
    let mut declarations = load_declarations(&root)?;
    let old_len = declarations.len();
    declarations.retain(|entry| entry.id != id);
    ensure!(
        declarations.len() != old_len,
        "Forge source is not declared"
    );
    write_and_sync_declarations(mutation, storage, &root, declarations, options)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn update_forge_declaration(
    storage: &Path,
    workspace: &Path,
    id: &str,
    kind: Option<String>,
    schema: Option<String>,
    git: Option<String>,
    revision: Option<String>,
    path: Option<String>,
    options: &ForgeOptions,
) -> Result<()> {
    ensure!(
        kind.is_some() || schema.is_some() || git.is_some() || revision.is_some() || path.is_some(),
        "provide a field to update"
    );
    let root = ayeque_forge_core::discover_workspace(workspace)?;
    let mutation = ayeque_forge_core::ForgeMutation::begin(storage, &root)?;
    let mut declarations = load_declarations(&root)?;
    let source = declarations
        .iter_mut()
        .find(|entry| entry.id == id)
        .context("Forge source is not declared")?;
    if let Some(value) = kind {
        source.kind = value;
    }
    if let Some(value) = schema {
        source.schema = value;
    }
    if let Some(value) = git {
        source.git = value;
    }
    if let Some(value) = revision {
        source.revision = value;
    }
    if let Some(value) = path {
        source.path = value;
    }
    write_and_sync_declarations(mutation, storage, &root, declarations, options)
}

pub(super) fn list_forge_declarations(workspace: &Path) -> Result<()> {
    let root = ayeque_forge_core::discover_workspace(workspace)?;
    let declarations = load_declarations(&root)?;
    let sources = agl_runtime::workspace_backend_declarations(&declarations)?;
    let reader =
        ayeque_forge_core::ForgeWorkspace::open(&ayeque_forge_core::data_root()?, &root, &sources)?;
    for source in declarations {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            source.id,
            source.kind,
            source.revision,
            source.schema,
            reader.materialized_path(&source.id)?.display()
        );
    }
    Ok(())
}

fn load_declarations(workspace: &Path) -> Result<Vec<agl_runtime::ForgeDeclaration>> {
    if !workspace
        .join(agl_runtime::WORKSPACE_CONFIG_FILE_NAME)
        .try_exists()?
    {
        return Ok(Vec::new());
    }
    agl_runtime::read_workspace_forge_declarations(workspace)
}

fn write_and_sync_declarations(
    mutation: ayeque_forge_core::ForgeMutation,
    storage: &Path,
    root: &Path,
    declarations: Vec<agl_runtime::ForgeDeclaration>,
    options: &ForgeOptions,
) -> Result<()> {
    let bytes = agl_runtime::serialize_workspace_forge_declarations(root, declarations.clone())?;
    write_root_declaration(&root.join(agl_runtime::WORKSPACE_CONFIG_FILE_NAME), &bytes)?;
    sync_with_choices(mutation, storage, root, declarations, options).context("Forge sync failed; agentLIBRE.toml retains requested declarations; retry with agl forge sync")
}

fn sync_with_choices(
    mut mutation: ayeque_forge_core::ForgeMutation,
    storage: &Path,
    root: &Path,
    mut declarations: Vec<agl_runtime::ForgeDeclaration>,
    options: &ForgeOptions,
) -> Result<()> {
    use agl_runtime::forge_verification::{
        VerificationChoices, VerificationDecision, VerificationKey,
    };
    use std::io::{IsTerminal, Write};
    loop {
        mutation.set_sync_timeout(options.timeout)?;
        let saved = VerificationChoices::new(&options.data_root)?;
        let sources = declarations.clone();
        mutation.set_verification_choices(move |request| {
            let source = sources
                .iter()
                .find(|source| source.id == request.source.id())
                .context("verification source is missing")?;
            let key =
                VerificationKey::new(&request.workspace, source, &request.relative_file_path)?;
            Ok(saved.get(&key)?.map(|decision| match decision {
                VerificationDecision::Verify => ayeque_forge_core::VerificationDecision::Verify,
                VerificationDecision::Skip => ayeque_forge_core::VerificationDecision::Skip,
            }))
        });
        let result = mutation.sync(&agl_runtime::workspace_backend_declarations(&declarations)?);
        // No reader or writer guard survives an interactive decision.
        drop(mutation);
        match result {
            Ok(_) => return Ok(()),
            Err(error) => {
                let Some(request) = error.downcast_ref::<ayeque_forge_core::VerificationRequired>()
                else {
                    return Err(error);
                };
                eprintln!("{}", serde_json::to_string(request)?);
                if !std::io::stdin().is_terminal() {
                    return Err(error);
                }
                let decision = loop {
                    eprint!("Verify content or skip full hash? [verify/skip]: ");
                    std::io::stderr().flush()?;
                    let mut answer = String::new();
                    ensure!(
                        std::io::stdin().read_line(&mut answer)? != 0,
                        "verification decision was not supplied"
                    );
                    match answer.trim() {
                        "verify" => break VerificationDecision::Verify,
                        "skip" => break VerificationDecision::Skip,
                        _ => continue,
                    }
                };
                let source = declarations
                    .iter()
                    .find(|source| source.id == request.source.id())
                    .context("verification source is missing")?;
                VerificationChoices::new(&options.data_root)?.set(
                    VerificationKey::new(root, source, &request.relative_file_path)?,
                    decision,
                )?;
                // Another caller may have edited the declaration while no lock was held.
                mutation = ayeque_forge_core::ForgeMutation::begin(storage, root)?;
                declarations = load_declarations(root)?;
            }
        }
    }
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
mod forge_tests {
    use super::*;

    #[test]
    fn forge_add_initializes_catalog_and_resolves_materialization() {
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
        let options = ForgeOptions {
            data_root: temporary.path().join("data"),
            timeout: std::time::Duration::from_secs(300),
        };
        let registration = || agl_runtime::ForgeDeclaration {
            id: "test.document".into(),
            kind: "document".into(),
            schema: "agentlibre.document/v1".into(),
            git: source.to_string_lossy().into_owned(),
            revision: revision.clone(),
            path: "document".into(),
        };
        std::fs::write(
            workspace.join("agentLIBRE.toml"),
            "format = 2\nforge = []\n[workspace]\noutput_directories = [\"target\"]\n",
        )
        .unwrap();
        let entity = add_forge_declaration(&storage, &workspace, registration(), &options).unwrap();
        assert_eq!(
            agl_runtime::read_workspace_output_directories(&workspace).unwrap(),
            ["target"]
        );
        assert_eq!(
            std::fs::read_to_string(entity.join("README.md")).unwrap(),
            "# Verified document\n"
        );
        assert!(add_forge_declaration(&storage, &workspace, registration(), &options).is_err());
        let lock = ayeque_forge_core::parse_lock(
            &std::fs::read(workspace.join("agentLIBRE.lock")).unwrap(),
        )
        .unwrap();
        assert_eq!(lock.entries().len(), 1);
        let failed = update_forge_declaration(
            &storage,
            &workspace,
            "test.document",
            None,
            None,
            None,
            None,
            Some("missing-document".into()),
            &options,
        )
        .unwrap_err();
        assert!(failed.to_string().contains("agl forge sync"));
        assert_eq!(
            agl_runtime::read_workspace_output_directories(&workspace).unwrap(),
            ["target"]
        );
        assert_eq!(
            load_declarations(&workspace).unwrap()[0].path,
            "missing-document"
        );
        let requested = std::fs::read(workspace.join("agentLIBRE.toml")).unwrap();
        assert!(sync_forge_declarations(&storage, &workspace, &options).is_err());
        assert_eq!(
            std::fs::read(workspace.join("agentLIBRE.toml")).unwrap(),
            requested
        );
        let lock = ayeque_forge_core::parse_lock(
            &std::fs::read(workspace.join("agentLIBRE.lock")).unwrap(),
        )
        .unwrap();
        assert_eq!(lock.entries()[0].path(), "document");
        assert_eq!(
            std::fs::read_to_string(entity.join("README.md")).unwrap(),
            "# Verified document\n"
        );

        std::fs::create_dir_all(source.join("missing-document")).unwrap();
        std::fs::write(
            source.join("missing-document/README.md"),
            "# Retried document\n",
        )
        .unwrap();
        assert!(
            ProcessCommand::new("git")
                .args(["add", "missing-document/README.md"])
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
                    "retry fixture"
                ])
                .current_dir(&source)
                .status()
                .unwrap()
                .success()
        );
        let next_revision = ProcessCommand::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&source)
            .output()
            .unwrap();
        assert!(next_revision.status.success());
        let mut declarations = load_declarations(&workspace).unwrap();
        declarations[0].revision = String::from_utf8(next_revision.stdout)
            .unwrap()
            .trim()
            .into();
        let bytes =
            agl_runtime::serialize_workspace_forge_declarations(&workspace, declarations).unwrap();
        write_root_declaration(&workspace.join("agentLIBRE.toml"), &bytes).unwrap();
        sync_forge_declarations(&storage, &workspace, &options).unwrap();
        assert_eq!(
            agl_runtime::read_workspace_output_directories(&workspace).unwrap(),
            ["target"]
        );
        let sources =
            agl_runtime::workspace_backend_declarations(&load_declarations(&workspace).unwrap())
                .unwrap();
        let reader =
            ayeque_forge_core::ForgeWorkspace::open(&storage, &workspace, &sources).unwrap();
        let retried = reader.materialized_path("test.document").unwrap();
        assert_eq!(
            std::fs::read_to_string(retried.join("README.md")).unwrap(),
            "# Retried document\n"
        );
        drop(reader);
        ayeque_forge_core::ForgeMutation::begin(&storage, &workspace)
            .unwrap()
            .sync(&[])
            .unwrap();
    }
}
