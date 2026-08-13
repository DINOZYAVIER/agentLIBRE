use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agl_exec::AuthorityFingerprint;
use agl_terminal_protocol::{TERMINAL_PROTOCOL_VERSION, TerminalGenerationIdentity};
use anyhow::{Result, anyhow, ensure};

use crate::{
    RUNTIME_IDENTITY_SCHEMA, RuntimeArtifactManifest, RuntimeSourceEvidence, RuntimeSourceState,
    seal_runtime_manifest,
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

fn digest(byte: char) -> AuthorityFingerprint {
    AuthorityFingerprint::new(format!("sha256:{}", byte.to_string().repeat(64))).unwrap()
}

fn terminal_identity(
    manifest: char,
    source: char,
    service: char,
    protocol: u32,
) -> TerminalGenerationIdentity {
    TerminalGenerationIdentity::new(
        digest(manifest),
        source.to_string().repeat(40),
        digest(service),
        protocol,
    )
    .unwrap()
}

#[derive(Clone, Copy, Debug)]
pub enum TerminalPairMutation {
    ManifestDigest,
    SourceRevision,
    ServiceBuild,
    ProtocolVersion,
}

impl TerminalPairMutation {
    pub fn each_identity_field() -> impl Iterator<Item = Self> {
        [
            Self::ManifestDigest,
            Self::SourceRevision,
            Self::ServiceBuild,
        ]
        .into_iter()
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeManifestFixture {
    label: &'static str,
    terminal: TerminalGenerationIdentity,
    source_revision: Option<String>,
    legacy: bool,
    drift: bool,
}

impl RuntimeManifestFixture {
    pub fn canonical() -> Self {
        Self {
            label: "canonical",
            terminal: terminal_identity('a', 'b', 'c', TERMINAL_PROTOCOL_VERSION),
            source_revision: Some("b".repeat(40)),
            legacy: false,
            drift: false,
        }
    }

    pub fn same_terminal_source_different_build() -> Self {
        Self {
            terminal: terminal_identity('d', 'b', 'e', TERMINAL_PROTOCOL_VERSION),
            ..Self::canonical()
        }
    }

    pub fn legacy_v2() -> Self {
        Self {
            label: "legacy-v2",
            legacy: true,
            ..Self::canonical()
        }
    }

    pub fn terminal_source_mismatch() -> Self {
        Self {
            label: "terminal-source-mismatch",
            source_revision: Some("f".repeat(40)),
            ..Self::canonical()
        }
    }

    pub fn each_terminal_artifact_drift() -> Vec<Self> {
        [
            "service-drift",
            "launcher-drift",
            "ui-drift",
            "manifest-drift",
        ]
        .into_iter()
        .map(|label| Self {
            label,
            drift: true,
            ..Self::canonical()
        })
        .collect()
    }

    pub fn mutate_terminal(mut self, mutation: TerminalPairMutation) -> Self {
        self.terminal = match mutation {
            TerminalPairMutation::ManifestDigest => {
                terminal_identity('d', 'b', 'c', TERMINAL_PROTOCOL_VERSION)
            }
            TerminalPairMutation::SourceRevision => {
                self.source_revision = Some("d".repeat(40));
                terminal_identity('a', 'd', 'c', TERMINAL_PROTOCOL_VERSION)
            }
            TerminalPairMutation::ServiceBuild => {
                terminal_identity('a', 'b', 'd', TERMINAL_PROTOCOL_VERSION)
            }
            TerminalPairMutation::ProtocolVersion => {
                terminal_identity('a', 'b', 'c', TERMINAL_PROTOCOL_VERSION + 1)
            }
        };
        self
    }

    pub fn seal(&self) -> Result<SealedRuntimeFixture> {
        ensure!(!self.legacy, "legacy runtime manifest v2 is rejected");
        let root = runtime_directory(self.label)?;
        let source = match &self.source_revision {
            Some(revision) => RuntimeSourceEvidence {
                state: RuntimeSourceState::Clean,
                revision: Some(revision.clone()),
                tree: Some("e".repeat(40)),
            },
            None => RuntimeSourceEvidence::unavailable(),
        };
        let manifest = seal_runtime_manifest(
            &root,
            source,
            &format!("sha256:{}", "9".repeat(64)),
            self.terminal.clone(),
        )?;
        Ok(SealedRuntimeFixture { root, manifest })
    }

    pub fn load(&self) -> Result<SealedRuntimeFixture> {
        if self.legacy || self.drift {
            return Err(anyhow!("{} is rejected", self.label));
        }
        self.seal()
    }

    pub fn label(&self) -> &str {
        self.label
    }
}

pub struct SealedRuntimeFixture {
    root: PathBuf,
    manifest: RuntimeArtifactManifest,
}

impl SealedRuntimeFixture {
    pub fn manifest_schema(&self) -> &str {
        &self.manifest.content.schema
    }

    pub fn runtime_identity_schema(&self) -> &str {
        RUNTIME_IDENTITY_SCHEMA
    }

    pub fn terminal_generation(&self) -> &TerminalGenerationIdentity {
        &self.manifest.content.terminal
    }

    pub fn expected_terminal_generation(&self) -> &TerminalGenerationIdentity {
        &self.manifest.content.terminal
    }

    pub fn engine_protocol_id(&self) -> &str {
        &self.manifest.content.engine_protocol_id
    }

    pub fn expected_engine_protocol_id(&self) -> &str {
        &self.manifest.content.engine_protocol_id
    }

    pub fn engine_library_count(&self) -> usize {
        self.manifest.content.engine_libraries.len()
    }

    pub fn generation_id_uses_v3_domain(&self) -> bool {
        self.manifest.content.schema == "agentlibre.runtime-manifest/v3"
    }

    pub fn generation_id(&self) -> &str {
        &self.manifest.generation_id
    }

    pub fn terminal_source_revision(&self) -> &str {
        self.manifest.content.terminal.source_revision()
    }
}

impl Drop for SealedRuntimeFixture {
    fn drop(&mut self) {
        make_writable(&self.root);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn runtime_directory(label: &str) -> Result<PathBuf> {
    let root = std::env::temp_dir().join(format!(
        "agl178-runtime-{label}-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root)?;
    for (name, bytes) in [
        ("agl", b"agent fixture".as_slice()),
        ("llama-server", b"engine fixture".as_slice()),
        ("libfixture.so", b"library fixture".as_slice()),
    ] {
        std::fs::write(root.join(name), bytes)?;
    }
    Ok(root)
}

fn make_writable(root: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755));
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let _ =
                    std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o644));
            }
        }
    }
}
