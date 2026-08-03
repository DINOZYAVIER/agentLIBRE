use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub const RUNTIME_MANIFEST_FILE_NAME: &str = "runtime-manifest.json";
pub const RUNTIME_MANIFEST_SCHEMA: &str = "agentlibre.runtime-manifest/v1";
pub const RUNTIME_IDENTITY_SCHEMA: &str = "agentlibre.runtime-identity/v1";

const GENERATION_ID_DOMAIN: &[u8] = b"agentlibre.runtime-manifest.v1\0";
const DEVELOPMENT_ID_DOMAIN: &[u8] = b"agentlibre.development-runtime.v1\0";
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_COMPONENT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_NATIVE_FILES: usize = 64;
const RUNTIME_EXECUTABLES: [&str; 3] = ["agl", "agl-inference-worker", "agl-process-launcher"];

static CURRENT_RUNTIME_IDENTITY: OnceLock<std::result::Result<CurrentRuntimeIdentity, String>> =
    OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSourceState {
    Clean,
    Dirty,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSourceEvidence {
    pub state: RuntimeSourceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
}

impl RuntimeSourceEvidence {
    pub fn unavailable() -> Self {
        Self {
            state: RuntimeSourceState::Unavailable,
            revision: None,
            tree: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self.state {
            RuntimeSourceState::Unavailable => ensure!(
                self.revision.is_none() && self.tree.is_none(),
                "source-unavailable evidence cannot claim a Git revision or tree"
            ),
            RuntimeSourceState::Clean | RuntimeSourceState::Dirty => {
                validate_git_object(self.revision.as_deref(), "source revision")?;
                validate_git_object(self.tree.as_deref(), "source tree")?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeManifestFile {
    pub path: String,
    pub byte_size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBuiltinPackage {
    pub exact_reference: String,
    pub digest: String,
    pub entrypoint: String,
    pub requires: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBuiltinCatalog {
    pub digest: String,
    pub packages: Vec<RuntimeBuiltinPackage>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeNativeBundle {
    pub identity: String,
    pub files: Vec<RuntimeManifestFile>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeManifestContent {
    pub schema: String,
    pub product_version: String,
    pub source: RuntimeSourceEvidence,
    pub builtin_catalog: RuntimeBuiltinCatalog,
    pub executables: Vec<RuntimeManifestFile>,
    pub native_bundle: RuntimeNativeBundle,
    pub worker_build_id: String,
    pub composite_worker_build_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeArtifactManifest {
    pub generation_id: String,
    #[serde(flatten)]
    pub content: RuntimeManifestContent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeIdentityKind {
    Sealed,
    Development,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentRuntimeIdentity {
    pub schema: String,
    pub kind: RuntimeIdentityKind,
    pub generation_id: String,
    pub builtin_catalog_digest: String,
    pub executable_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<RuntimeArtifactManifest>,
}

impl CurrentRuntimeIdentity {
    pub fn sealed(&self) -> bool {
        self.kind == RuntimeIdentityKind::Sealed
    }

    pub fn worker_build_id(&self) -> Option<&str> {
        self.manifest
            .as_ref()
            .map(|manifest| manifest.content.worker_build_id.as_str())
    }

    pub fn native_bundle_id(&self) -> Option<&str> {
        self.manifest
            .as_ref()
            .map(|manifest| manifest.content.native_bundle.identity.as_str())
    }
}

pub fn seal_runtime_manifest(
    directory: impl AsRef<Path>,
    source: RuntimeSourceEvidence,
    worker_build_id: &str,
) -> Result<RuntimeArtifactManifest> {
    let directory = directory.as_ref();
    source.validate()?;
    validate_sha256(worker_build_id, "worker build ID")?;
    let manifest_path = directory.join(RUNTIME_MANIFEST_FILE_NAME);
    ensure!(
        !manifest_path.exists(),
        "runtime manifest already exists: {}",
        manifest_path.display()
    );

    let executables = RUNTIME_EXECUTABLES
        .into_iter()
        .map(|name| manifest_file(directory, name))
        .collect::<Result<Vec<_>>>()?;
    let native_bundle = inspect_native_bundle(directory)?;
    let content = RuntimeManifestContent {
        schema: RUNTIME_MANIFEST_SCHEMA.to_string(),
        product_version: env!("CARGO_PKG_VERSION").to_string(),
        source,
        builtin_catalog: compiled_builtin_catalog(),
        executables,
        composite_worker_build_id: format!("{worker_build_id}+{}", native_bundle.identity),
        native_bundle,
        worker_build_id: worker_build_id.to_string(),
    };
    let generation_id = generation_id(&content)?;
    let manifest = RuntimeArtifactManifest {
        generation_id,
        content,
    };
    validate_manifest_shape(&manifest)?;

    let mut bytes = serde_json::to_vec_pretty(&manifest)
        .context("failed to encode canonical runtime manifest")?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&manifest_path)
        .with_context(|| format!("failed to create {}", manifest_path.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", manifest_path.display()))?;
    drop(file);
    seal_manifest_permissions(&manifest_path)?;
    sync_directory(directory)?;
    Ok(manifest)
}

pub fn load_runtime_manifest(directory: impl AsRef<Path>) -> Result<RuntimeArtifactManifest> {
    let directory = directory.as_ref();
    let path = directory.join(RUNTIME_MANIFEST_FILE_NAME);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "runtime manifest is not a regular file: {}",
        path.display()
    );
    validate_manifest_permissions(&path, &metadata)?;
    ensure!(
        metadata.len() <= MAX_MANIFEST_BYTES,
        "runtime manifest exceeds its byte bound: {}",
        path.display()
    );
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let manifest = serde_json::from_slice::<RuntimeArtifactManifest>(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    validate_manifest_shape(&manifest)?;
    validate_manifest_components(directory, &manifest)?;
    Ok(manifest)
}

pub fn current_runtime_identity() -> Result<CurrentRuntimeIdentity> {
    CURRENT_RUNTIME_IDENTITY
        .get_or_init(|| compute_current_runtime_identity().map_err(|error| format!("{error:#}")))
        .as_ref()
        .cloned()
        .map_err(|error| anyhow!(error.clone()))
}

fn compute_current_runtime_identity() -> Result<CurrentRuntimeIdentity> {
    let executable = std::env::current_exe().context("failed to resolve current executable")?;
    let directory = executable
        .parent()
        .context("current executable has no parent directory")?;
    let manifest_path = directory.join(RUNTIME_MANIFEST_FILE_NAME);
    if manifest_path.exists() {
        let manifest = load_runtime_manifest(directory)?;
        let executable_digest = manifest
            .content
            .executables
            .iter()
            .find(|entry| entry.path == "agl")
            .context("runtime manifest has no agl executable")?
            .sha256
            .clone();
        return Ok(CurrentRuntimeIdentity {
            schema: RUNTIME_IDENTITY_SCHEMA.to_string(),
            kind: RuntimeIdentityKind::Sealed,
            generation_id: manifest.generation_id.clone(),
            builtin_catalog_digest: manifest.content.builtin_catalog.digest.clone(),
            executable_digest,
            manifest_path: Some(manifest_path),
            manifest: Some(manifest),
        });
    }

    let executable_digest = sha256_file(&executable)?;
    let mut digest = Sha256::new();
    digest.update(DEVELOPMENT_ID_DOMAIN);
    hash_field(&mut digest, env!("CARGO_PKG_VERSION").as_bytes());
    hash_field(
        &mut digest,
        agl_assets::BUILTIN_ARTIFACT_CATALOG_DIGEST.as_bytes(),
    );
    hash_field(&mut digest, executable_digest.as_bytes());
    Ok(CurrentRuntimeIdentity {
        schema: RUNTIME_IDENTITY_SCHEMA.to_string(),
        kind: RuntimeIdentityKind::Development,
        generation_id: format!("sha256:{}", lowercase_hex(&digest.finalize())),
        builtin_catalog_digest: agl_assets::BUILTIN_ARTIFACT_CATALOG_DIGEST.to_string(),
        executable_digest,
        manifest_path: None,
        manifest: None,
    })
}

pub fn current_executable_is_in(directory: impl AsRef<Path>) -> Result<()> {
    let current = std::env::current_exe().context("failed to resolve current executable")?;
    let expected = directory.as_ref().join("agl");
    ensure!(
        current == expected,
        "runtime manifest sealer must execute the exact staged agl binary: expected {}, got {}",
        expected.display(),
        current.display()
    );
    Ok(())
}

fn validate_manifest_shape(manifest: &RuntimeArtifactManifest) -> Result<()> {
    ensure!(
        manifest.content.schema == RUNTIME_MANIFEST_SCHEMA,
        "runtime manifest schema is not {RUNTIME_MANIFEST_SCHEMA}"
    );
    ensure!(
        manifest.content.product_version == env!("CARGO_PKG_VERSION"),
        "runtime manifest product version does not match this executable"
    );
    manifest.content.source.validate()?;
    validate_sha256(&manifest.generation_id, "runtime generation ID")?;
    ensure!(
        manifest.generation_id == generation_id(&manifest.content)?,
        "runtime manifest generation ID does not match its canonical content"
    );
    ensure!(
        manifest.content.builtin_catalog == compiled_builtin_catalog(),
        "runtime manifest builtin catalog does not match this executable"
    );
    validate_sha256(&manifest.content.worker_build_id, "worker build ID")?;
    validate_sha256(
        &manifest.content.native_bundle.identity,
        "native bundle identity",
    )?;
    ensure!(
        manifest.content.composite_worker_build_id
            == format!(
                "{}+{}",
                manifest.content.worker_build_id, manifest.content.native_bundle.identity
            ),
        "runtime manifest composite worker identity is inconsistent"
    );
    let executable_names = manifest
        .content
        .executables
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<Vec<_>>();
    ensure!(
        executable_names == RUNTIME_EXECUTABLES,
        "runtime manifest executable inventory is not canonical"
    );
    validate_file_inventory(&manifest.content.executables, "runtime executables")?;
    ensure!(
        !manifest.content.native_bundle.files.is_empty()
            && manifest.content.native_bundle.files.len() <= MAX_NATIVE_FILES,
        "runtime manifest native file inventory is outside its bound"
    );
    validate_file_inventory(&manifest.content.native_bundle.files, "native bundle files")?;
    Ok(())
}

fn validate_manifest_components(
    directory: &Path,
    manifest: &RuntimeArtifactManifest,
) -> Result<()> {
    for entry in manifest
        .content
        .executables
        .iter()
        .chain(&manifest.content.native_bundle.files)
    {
        let actual = manifest_file(directory, &entry.path)?;
        ensure!(
            &actual == entry,
            "runtime component identity drifted: {}",
            entry.path
        );
    }
    let actual_native = inspect_native_bundle(directory)?;
    ensure!(
        actual_native == manifest.content.native_bundle,
        "runtime native bundle identity drifted"
    );
    Ok(())
}

fn generation_id(content: &RuntimeManifestContent) -> Result<String> {
    let bytes = serde_json::to_vec(content).context("failed to encode runtime manifest content")?;
    let mut digest = Sha256::new();
    digest.update(GENERATION_ID_DOMAIN);
    hash_field(&mut digest, &bytes);
    Ok(format!("sha256:{}", lowercase_hex(&digest.finalize())))
}

fn compiled_builtin_catalog() -> RuntimeBuiltinCatalog {
    let mut packages = agl_assets::BUILTIN_ARTIFACT_PACKAGES
        .iter()
        .map(|package| {
            let mut requires = package
                .requires
                .iter()
                .map(|requirement| (*requirement).to_string())
                .collect::<Vec<_>>();
            requires.sort();
            RuntimeBuiltinPackage {
                exact_reference: format!("{}:{}@{}", package.type_id, package.id, package.version),
                digest: package.digest.to_string(),
                entrypoint: package.entrypoint.to_string(),
                requires,
            }
        })
        .collect::<Vec<_>>();
    packages.sort_by(|left, right| left.exact_reference.cmp(&right.exact_reference));
    RuntimeBuiltinCatalog {
        digest: agl_assets::BUILTIN_ARTIFACT_CATALOG_DIGEST.to_string(),
        packages,
    }
}

fn inspect_native_bundle(directory: &Path) -> Result<RuntimeNativeBundle> {
    let base = directory.join("agl-inference-native");
    let leaves = exact_directory_entries(&base)?;
    ensure!(
        leaves.len() == 1,
        "runtime native bundle must contain exactly one leaf"
    );
    let leaf = &leaves[0];
    let leaf_name = leaf
        .file_name()
        .and_then(|name| name.to_str())
        .context("runtime native bundle leaf is not UTF-8")?;
    let digest = leaf_name
        .strip_prefix("sha256-")
        .context("runtime native bundle leaf is not content addressed")?;
    let identity = format!("sha256:{digest}");
    validate_sha256(&identity, "native bundle identity")?;
    let leaf_metadata = fs::symlink_metadata(leaf)
        .with_context(|| format!("failed to inspect {}", leaf.display()))?;
    ensure!(
        leaf_metadata.is_dir() && !leaf_metadata.file_type().is_symlink(),
        "runtime native bundle leaf is not an exact directory"
    );
    let mut files = exact_directory_entries(leaf)?
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .context("runtime native bundle filename is not UTF-8")?;
            manifest_file(
                directory,
                &format!("agl-inference-native/{leaf_name}/{name}"),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    ensure!(
        !files.is_empty() && files.len() <= MAX_NATIVE_FILES,
        "runtime native bundle file inventory is outside its bound"
    );
    Ok(RuntimeNativeBundle { identity, files })
}

fn manifest_file(root: &Path, relative: &str) -> Result<RuntimeManifestFile> {
    validate_relative_path(relative)?;
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("failed to inspect runtime component {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "runtime component is not a regular file: {}",
        path.display()
    );
    ensure!(
        metadata.len() <= MAX_COMPONENT_BYTES,
        "runtime component exceeds its byte bound: {}",
        path.display()
    );
    Ok(RuntimeManifestFile {
        path: relative.to_string(),
        byte_size: metadata.len(),
        sha256: sha256_file(&path)?,
    })
}

fn exact_directory_entries(directory: &Path) -> Result<Vec<PathBuf>> {
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("failed to inspect {}", directory.display()))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "runtime path is not an exact directory: {}",
        directory.display()
    );
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("failed to list {}", directory.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    Ok(entries)
}

fn validate_file_inventory(files: &[RuntimeManifestFile], label: &str) -> Result<()> {
    let mut previous = None;
    let mut paths = BTreeSet::new();
    for file in files {
        validate_relative_path(&file.path)?;
        validate_sha256(&file.sha256, &format!("{label} SHA-256"))?;
        ensure!(
            file.byte_size <= MAX_COMPONENT_BYTES,
            "{label} file is oversized"
        );
        ensure!(
            paths.insert(file.path.as_str()),
            "{label} contains a duplicate path"
        );
        if let Some(previous) = previous {
            ensure!(
                previous < file.path.as_str(),
                "{label} paths are not ordered"
            );
        }
        previous = Some(file.path.as_str());
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && !path.starts_with('/')
            && !path.contains('\\')
            && path
                .split('/')
                .all(|segment| !segment.is_empty() && segment != "." && segment != ".."),
        "runtime manifest path is not canonical: {path}"
    );
    Ok(())
}

fn validate_git_object(value: Option<&str>, label: &str) -> Result<()> {
    let value = value.with_context(|| format!("{label} is missing"))?;
    ensure!(
        matches!(value.len(), 40 | 64)
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "{label} is not a canonical Git object ID"
    );
    Ok(())
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 71
            && value.starts_with("sha256:")
            && value[7..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "{label} is not a canonical SHA-256 identity"
    );
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open runtime component {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to hash runtime component {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("sha256:{}", lowercase_hex(&digest.finalize())))
}

fn hash_field(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

fn lowercase_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn sync_directory(directory: &Path) -> Result<()> {
    File::open(directory)
        .with_context(|| format!("failed to open directory {} for sync", directory.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync directory {}", directory.display()))
}

#[cfg(unix)]
fn seal_manifest_permissions(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o444))
        .with_context(|| format!("failed to seal runtime manifest {}", path.display()))
}

#[cfg(not(unix))]
fn seal_manifest_permissions(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path)
        .with_context(|| format!("failed to inspect runtime manifest {}", path.display()))?
        .permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to seal runtime manifest {}", path.display()))
}

#[cfg(unix)]
fn validate_manifest_permissions(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    ensure!(
        metadata.mode() & 0o777 == 0o444 && metadata.nlink() == 1,
        "runtime manifest is not an immutable single-link file: {}",
        path.display()
    );
    Ok(())
}

#[cfg(not(unix))]
fn validate_manifest_permissions(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    ensure!(
        metadata.permissions().readonly(),
        "runtime manifest is not read-only: {}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    fn fixture(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agl-runtime-manifest-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(format!("agl-inference-native/sha256-{}", "a".repeat(64))))
            .unwrap();
        for executable in RUNTIME_EXECUTABLES {
            fs::write(
                root.join(executable),
                format!("fixture executable: {executable}\n"),
            )
            .unwrap();
        }
        fs::write(
            root.join(format!(
                "agl-inference-native/sha256-{}/libfixture.so",
                "a".repeat(64)
            )),
            b"native fixture",
        )
        .unwrap();
        root
    }

    fn rewrite_manifest(root: &Path, manifest: &RuntimeArtifactManifest) {
        let path = root.join(RUNTIME_MANIFEST_FILE_NAME);
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        #[cfg(not(unix))]
        {
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(false);
            fs::set_permissions(&path, permissions).unwrap();
        }
        let mut bytes = serde_json::to_vec_pretty(manifest).unwrap();
        bytes.push(b'\n');
        fs::write(&path, bytes).unwrap();
        seal_manifest_permissions(&path).unwrap();
    }

    #[test]
    fn sealed_manifest_is_path_independent_and_detects_component_drift() {
        let first = fixture("first");
        let second = fixture("second");
        let worker = format!("sha256:{}", "b".repeat(64));
        let first_manifest =
            seal_runtime_manifest(&first, RuntimeSourceEvidence::unavailable(), &worker).unwrap();
        let second_manifest =
            seal_runtime_manifest(&second, RuntimeSourceEvidence::unavailable(), &worker).unwrap();

        assert_eq!(first_manifest.generation_id, second_manifest.generation_id);
        assert_eq!(load_runtime_manifest(&first).unwrap(), first_manifest);
        fs::write(first.join("agl-process-launcher"), b"tampered").unwrap();
        assert!(
            load_runtime_manifest(&first)
                .unwrap_err()
                .to_string()
                .contains("identity drifted")
        );
        let _ = fs::remove_dir_all(first);
        let _ = fs::remove_dir_all(second);
    }

    #[test]
    fn source_state_is_explicit_and_generation_identity_binds_it() {
        let unavailable = fixture("unavailable");
        let dirty = fixture("dirty");
        let worker = format!("sha256:{}", "b".repeat(64));
        let unavailable_manifest =
            seal_runtime_manifest(&unavailable, RuntimeSourceEvidence::unavailable(), &worker)
                .unwrap();
        let dirty_manifest = seal_runtime_manifest(
            &dirty,
            RuntimeSourceEvidence {
                state: RuntimeSourceState::Dirty,
                revision: Some("c".repeat(40)),
                tree: Some("d".repeat(40)),
            },
            &worker,
        )
        .unwrap();
        assert_ne!(
            unavailable_manifest.generation_id,
            dirty_manifest.generation_id
        );
        assert_eq!(
            dirty_manifest.content.builtin_catalog.digest,
            agl_assets::BUILTIN_ARTIFACT_CATALOG_DIGEST
        );
        let _ = fs::remove_dir_all(unavailable);
        let _ = fs::remove_dir_all(dirty);
    }

    #[test]
    fn sealed_manifest_rejects_native_file_drift() {
        let root = fixture("native-drift");
        let worker = format!("sha256:{}", "b".repeat(64));
        seal_runtime_manifest(&root, RuntimeSourceEvidence::unavailable(), &worker).unwrap();
        fs::write(
            root.join(format!(
                "agl-inference-native/sha256-{}/libfixture.so",
                "a".repeat(64)
            )),
            b"tampered native fixture",
        )
        .unwrap();

        assert!(
            load_runtime_manifest(&root)
                .unwrap_err()
                .to_string()
                .contains("identity drifted")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sealed_manifest_rejects_recomputed_builtin_inventory_drift() {
        let root = fixture("catalog-drift");
        let worker = format!("sha256:{}", "b".repeat(64));
        let mut manifest =
            seal_runtime_manifest(&root, RuntimeSourceEvidence::unavailable(), &worker).unwrap();
        manifest.content.builtin_catalog.packages[0].digest = format!("sha256:{}", "f".repeat(64));
        manifest.generation_id = generation_id(&manifest.content).unwrap();
        rewrite_manifest(&root, &manifest);

        assert!(
            load_runtime_manifest(&root)
                .unwrap_err()
                .to_string()
                .contains("builtin catalog does not match")
        );
        let _ = fs::remove_dir_all(root);
    }
}
