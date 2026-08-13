use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use agl_exec::AuthorityFingerprint;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::TERMINAL_PROTOCOL_VERSION;

pub const TERMINAL_GENERATION_MANIFEST_FILE_NAME: &str = "runtime-manifest.json";
pub const TERMINAL_GENERATION_MANIFEST_SCHEMA: &str = "agl-terminal.runtime-generation.v2";

const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_COMPONENT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalGenerationFileRole {
    Service,
    Launcher,
    Ui,
}

impl TerminalGenerationFileRole {
    fn path(self) -> &'static str {
        match self {
            Self::Service => "agl-terminald",
            Self::Launcher => "agl-process-launcher",
            Self::Ui => "agl-terminal",
        }
    }
}

const FILE_ROLES: [TerminalGenerationFileRole; 3] = [
    TerminalGenerationFileRole::Service,
    TerminalGenerationFileRole::Launcher,
    TerminalGenerationFileRole::Ui,
];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalGenerationFile {
    role: TerminalGenerationFileRole,
    path: String,
    byte_size: u64,
    sha256: AuthorityFingerprint,
}

impl TerminalGenerationFile {
    pub fn role(&self) -> TerminalGenerationFileRole {
        self.role
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn byte_size(&self) -> u64 {
        self.byte_size
    }

    pub fn sha256(&self) -> &AuthorityFingerprint {
        &self.sha256
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalGenerationManifest {
    schema: String,
    product_version: String,
    source_revision: String,
    protocol_version: u32,
    files: Vec<TerminalGenerationFile>,
}

impl TerminalGenerationManifest {
    pub fn seal(
        directory: impl AsRef<Path>,
        source_revision: &str,
    ) -> Result<VerifiedTerminalGeneration, TerminalGenerationError> {
        let directory = directory.as_ref();
        validate_source_revision(source_revision)?;
        validate_generation_directory(directory, false)?;
        validate_generation_inventory(directory, false)?;
        let manifest_path = directory.join(TERMINAL_GENERATION_MANIFEST_FILE_NAME);
        if manifest_path.exists() {
            return Err(TerminalGenerationError::ManifestExists(manifest_path));
        }

        let files = FILE_ROLES
            .into_iter()
            .map(|role| inspect_component(directory, role))
            .collect::<Result<Vec<_>, _>>()?;
        let manifest = Self {
            schema: TERMINAL_GENERATION_MANIFEST_SCHEMA.to_owned(),
            product_version: env!("CARGO_PKG_VERSION").to_owned(),
            source_revision: source_revision.to_owned(),
            protocol_version: TERMINAL_PROTOCOL_VERSION,
            files,
        };
        manifest.validate_shape()?;

        let mut bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|error| TerminalGenerationError::Json(error.to_string()))?;
        bytes.push(b'\n');
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&manifest_path)
            .map_err(|error| io_error(&manifest_path, error))?;
        file.write_all(&bytes)
            .map_err(|error| io_error(&manifest_path, error))?;
        file.sync_all()
            .map_err(|error| io_error(&manifest_path, error))?;
        drop(file);

        #[cfg(unix)]
        {
            fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o444))
                .map_err(|error| io_error(&manifest_path, error))?;
            for role in FILE_ROLES {
                let path = directory.join(role.path());
                fs::set_permissions(&path, fs::Permissions::from_mode(0o555))
                    .map_err(|error| io_error(&path, error))?;
            }
            fs::set_permissions(directory, fs::Permissions::from_mode(0o555))
                .map_err(|error| io_error(directory, error))?;
        }
        sync_directory(directory)?;
        VerifiedTerminalGeneration::load(directory)
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn product_version(&self) -> &str {
        &self.product_version
    }

    pub fn source_revision(&self) -> &str {
        &self.source_revision
    }

    pub fn protocol_version(&self) -> u32 {
        self.protocol_version
    }

    pub fn files(&self) -> impl Iterator<Item = &TerminalGenerationFile> {
        self.files.iter()
    }

    pub fn file(&self, role: TerminalGenerationFileRole) -> &TerminalGenerationFile {
        self.files
            .iter()
            .find(|file| file.role == role)
            .expect("validated terminal manifest has every canonical file role")
    }

    fn validate_shape(&self) -> Result<(), TerminalGenerationError> {
        if self.schema != TERMINAL_GENERATION_MANIFEST_SCHEMA {
            return Err(TerminalGenerationError::Schema);
        }
        if self.product_version != env!("CARGO_PKG_VERSION") {
            return Err(TerminalGenerationError::ProductVersion);
        }
        validate_source_revision(&self.source_revision)?;
        if self.protocol_version != TERMINAL_PROTOCOL_VERSION {
            return Err(TerminalGenerationError::ProtocolVersion);
        }
        if self.files.len() != FILE_ROLES.len() {
            return Err(TerminalGenerationError::FileInventory);
        }
        let mut paths = BTreeSet::new();
        for (file, role) in self.files.iter().zip(FILE_ROLES) {
            if file.role != role || file.path != role.path() || !paths.insert(file.path.as_str()) {
                return Err(TerminalGenerationError::FileInventory);
            }
            if file.byte_size == 0 || file.byte_size > MAX_COMPONENT_BYTES {
                return Err(TerminalGenerationError::ComponentSize(file.path.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedTerminalGeneration {
    directory: PathBuf,
    manifest: TerminalGenerationManifest,
    manifest_digest: AuthorityFingerprint,
    identity: TerminalGenerationIdentity,
}

impl VerifiedTerminalGeneration {
    pub fn load(directory: impl AsRef<Path>) -> Result<Self, TerminalGenerationError> {
        let directory = directory.as_ref();
        validate_generation_directory(directory, true)?;
        let manifest_path = directory.join(TERMINAL_GENERATION_MANIFEST_FILE_NAME);
        let metadata = fs::symlink_metadata(&manifest_path)
            .map_err(|error| io_error(&manifest_path, error))?;
        validate_regular_file(&manifest_path, &metadata, MAX_MANIFEST_BYTES)?;
        #[cfg(unix)]
        if metadata.mode() & 0o777 != 0o444 {
            return Err(TerminalGenerationError::UnsafePermissions(
                manifest_path.clone(),
            ));
        }
        let bytes = fs::read(&manifest_path).map_err(|error| io_error(&manifest_path, error))?;
        let manifest: TerminalGenerationManifest = serde_json::from_slice(&bytes)
            .map_err(|error| TerminalGenerationError::Json(error.to_string()))?;
        manifest.validate_shape()?;
        for role in FILE_ROLES {
            let actual = inspect_component(directory, role)?;
            if &actual != manifest.file(role) {
                return Err(TerminalGenerationError::ComponentDrift(
                    role.path().to_owned(),
                ));
            }
        }
        validate_generation_inventory(directory, true)?;
        let manifest_digest = fingerprint_bytes(&bytes)?;
        let service_build_id = manifest
            .file(TerminalGenerationFileRole::Service)
            .sha256
            .clone();
        let identity = TerminalGenerationIdentity::new(
            manifest_digest.clone(),
            manifest.source_revision.clone(),
            service_build_id,
            manifest.protocol_version,
        )?;
        Ok(Self {
            directory: directory.to_path_buf(),
            manifest,
            manifest_digest,
            identity,
        })
    }

    pub fn load_installed(directory: impl AsRef<Path>) -> Result<Self, TerminalGenerationError> {
        let verified = Self::load(directory)?;
        if verified
            .directory
            .file_name()
            .and_then(|name| name.to_str())
            != Some(verified.generation_directory_name().as_str())
        {
            return Err(TerminalGenerationError::GenerationDirectoryName);
        }
        let generations = verified
            .directory
            .parent()
            .ok_or_else(|| TerminalGenerationError::UnsafePath(verified.directory.clone()))?;
        if generations.file_name().and_then(|name| name.to_str()) != Some("generations") {
            return Err(TerminalGenerationError::InstalledLayout);
        }
        let product = generations
            .parent()
            .ok_or_else(|| TerminalGenerationError::UnsafePath(generations.to_path_buf()))?;
        validate_managed_ancestor(generations)?;
        validate_managed_ancestor(product)?;
        let canonical = verified
            .directory
            .canonicalize()
            .map_err(|error| io_error(&verified.directory, error))?;
        if canonical != verified.directory {
            return Err(TerminalGenerationError::UnsafePath(
                verified.directory.clone(),
            ));
        }
        Ok(verified)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn manifest(&self) -> &TerminalGenerationManifest {
        &self.manifest
    }

    pub fn manifest_digest(&self) -> &AuthorityFingerprint {
        &self.manifest_digest
    }

    pub fn identity(&self) -> &TerminalGenerationIdentity {
        &self.identity
    }

    pub fn generation_directory_name(&self) -> String {
        format!(
            "generation-{}",
            self.manifest_digest
                .as_str()
                .strip_prefix("sha256:")
                .expect("AuthorityFingerprint is always sha256")
        )
    }

    pub fn file_path(&self, role: TerminalGenerationFileRole) -> PathBuf {
        self.directory.join(self.manifest.file(role).path())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalGenerationIdentity {
    manifest_digest: AuthorityFingerprint,
    source_revision: String,
    service_build_id: AuthorityFingerprint,
    protocol_version: u32,
}

impl TerminalGenerationIdentity {
    pub fn new(
        manifest_digest: AuthorityFingerprint,
        source_revision: impl Into<String>,
        service_build_id: AuthorityFingerprint,
        protocol_version: u32,
    ) -> Result<Self, TerminalGenerationError> {
        let value = Self {
            manifest_digest,
            source_revision: source_revision.into(),
            service_build_id,
            protocol_version,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), TerminalGenerationError> {
        validate_source_revision(&self.source_revision)?;
        if self.protocol_version == 0 {
            return Err(TerminalGenerationError::ProtocolVersion);
        }
        Ok(())
    }

    pub fn require_exact(&self, actual: &Self) -> Result<(), TerminalGenerationError> {
        self.validate()?;
        actual.validate()?;
        if self == actual {
            Ok(())
        } else {
            Err(TerminalGenerationError::IdentityMismatch)
        }
    }

    pub fn manifest_digest(&self) -> &AuthorityFingerprint {
        &self.manifest_digest
    }

    pub fn source_revision(&self) -> &str {
        &self.source_revision
    }

    pub fn service_build_id(&self) -> &AuthorityFingerprint {
        &self.service_build_id
    }

    pub fn protocol_version(&self) -> u32 {
        self.protocol_version
    }
}

#[derive(Debug, Error)]
pub enum TerminalGenerationError {
    #[error("terminal generation I/O failed for {0}: {1}")]
    Io(PathBuf, String),
    #[error("terminal generation manifest already exists: {0}")]
    ManifestExists(PathBuf),
    #[error("terminal generation manifest JSON is invalid: {0}")]
    Json(String),
    #[error("terminal generation manifest schema is not v2")]
    Schema,
    #[error("terminal generation product version does not match")]
    ProductVersion,
    #[error("terminal generation protocol version does not match")]
    ProtocolVersion,
    #[error("terminal generation source revision is not lowercase 40-hex")]
    SourceRevision,
    #[error("terminal generation file inventory is not canonical")]
    FileInventory,
    #[error("terminal generation directory name does not match its manifest digest")]
    GenerationDirectoryName,
    #[error("terminal generation is not below one managed generations directory")]
    InstalledLayout,
    #[error("terminal generation path is unsafe: {0}")]
    UnsafePath(PathBuf),
    #[error("terminal generation path permissions are unsafe: {0}")]
    UnsafePermissions(PathBuf),
    #[error("terminal generation component has an invalid size: {0}")]
    ComponentSize(String),
    #[error("terminal generation component identity drifted: {0}")]
    ComponentDrift(String),
    #[error("terminal generation identity does not match")]
    IdentityMismatch,
    #[error("terminal generation fingerprint is invalid")]
    Fingerprint,
}

fn validate_source_revision(value: &str) -> Result<(), TerminalGenerationError> {
    if value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(TerminalGenerationError::SourceRevision)
    }
}

fn validate_generation_directory(
    directory: &Path,
    sealed: bool,
) -> Result<(), TerminalGenerationError> {
    if !directory.is_absolute() {
        return Err(TerminalGenerationError::UnsafePath(directory.to_path_buf()));
    }
    let metadata = fs::symlink_metadata(directory).map_err(|error| io_error(directory, error))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(TerminalGenerationError::UnsafePath(directory.to_path_buf()));
    }
    #[cfg(unix)]
    {
        let mode = metadata.mode() & 0o777;
        if sealed && mode != 0o555 {
            return Err(TerminalGenerationError::UnsafePermissions(
                directory.to_path_buf(),
            ));
        }
        if !sealed && mode & 0o022 != 0 {
            return Err(TerminalGenerationError::UnsafePermissions(
                directory.to_path_buf(),
            ));
        }
    }
    Ok(())
}

fn inspect_component(
    directory: &Path,
    role: TerminalGenerationFileRole,
) -> Result<TerminalGenerationFile, TerminalGenerationError> {
    let relative = role.path();
    let path = directory.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|error| io_error(&path, error))?;
    validate_regular_file(&path, &metadata, MAX_COMPONENT_BYTES)?;
    #[cfg(unix)]
    if metadata.mode() & 0o777 != 0o555 {
        return Err(TerminalGenerationError::UnsafePermissions(path));
    }
    Ok(TerminalGenerationFile {
        role,
        path: relative.to_owned(),
        byte_size: metadata.len(),
        sha256: fingerprint_file(&path)?,
    })
}

fn validate_regular_file(
    path: &Path,
    metadata: &fs::Metadata,
    maximum: u64,
) -> Result<(), TerminalGenerationError> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(TerminalGenerationError::UnsafePath(path.to_path_buf()));
    }
    if metadata.len() == 0 || metadata.len() > maximum {
        return Err(TerminalGenerationError::ComponentSize(
            path.display().to_string(),
        ));
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err(TerminalGenerationError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

fn validate_managed_ancestor(path: &Path) -> Result<(), TerminalGenerationError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(TerminalGenerationError::UnsafePath(path.to_path_buf()));
    }
    #[cfg(unix)]
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
        return Err(TerminalGenerationError::UnsafePermissions(
            path.to_path_buf(),
        ));
    }
    Ok(())
}

fn validate_generation_inventory(
    directory: &Path,
    sealed: bool,
) -> Result<(), TerminalGenerationError> {
    let mut actual = fs::read_dir(directory)
        .map_err(|error| io_error(directory, error))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name())
                .map_err(|error| io_error(directory, error))
        })
        .collect::<Result<Vec<_>, _>>()?;
    actual.sort();
    let mut expected = FILE_ROLES
        .into_iter()
        .map(|role| role.path().into())
        .collect::<Vec<std::ffi::OsString>>();
    if sealed {
        expected.push(TERMINAL_GENERATION_MANIFEST_FILE_NAME.into());
    }
    expected.sort();
    if actual == expected {
        Ok(())
    } else {
        Err(TerminalGenerationError::FileInventory)
    }
}

fn fingerprint_file(path: &Path) -> Result<AuthorityFingerprint, TerminalGenerationError> {
    let bytes = fs::read(path).map_err(|error| io_error(path, error))?;
    fingerprint_bytes(&bytes)
}

fn fingerprint_bytes(bytes: &[u8]) -> Result<AuthorityFingerprint, TerminalGenerationError> {
    let digest = Sha256::digest(bytes);
    AuthorityFingerprint::new(format!("sha256:{}", lowercase_hex(&digest)))
        .map_err(|_| TerminalGenerationError::Fingerprint)
}

fn lowercase_hex(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut value, "{byte:02x}").expect("writing digest to String cannot fail");
    }
    value
}

fn sync_directory(directory: &Path) -> Result<(), TerminalGenerationError> {
    let file = fs::File::open(directory).map_err(|error| io_error(directory, error))?;
    file.sync_all().map_err(|error| io_error(directory, error))
}

fn io_error(path: &Path, error: std::io::Error) -> TerminalGenerationError {
    TerminalGenerationError::Io(path.to_path_buf(), error.to_string())
}
