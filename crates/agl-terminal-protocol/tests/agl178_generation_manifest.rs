use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agl_terminal_protocol::{
    TERMINAL_GENERATION_MANIFEST_FILE_NAME, TERMINAL_GENERATION_MANIFEST_SCHEMA,
    TerminalGenerationFileRole, TerminalGenerationManifest, VerifiedTerminalGeneration,
};
use sha2::{Digest as _, Sha256};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(label: &str, ui: &[u8]) -> Self {
        let root = std::env::temp_dir().join(format!(
            "agl178-terminal-manifest-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        for (name, bytes) in [
            ("agl-terminald", b"service-v1".as_slice()),
            ("agl-process-launcher", b"launcher-v1".as_slice()),
            ("agl-terminal", ui),
        ] {
            fs::write(root.join(name), bytes).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for name in ["agl-terminald", "agl-process-launcher", "agl-terminal"] {
                fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o555)).unwrap();
            }
        }
        Self { root }
    }

    fn seal(&self) -> VerifiedTerminalGeneration {
        TerminalGenerationManifest::seal(&self.root, &"a".repeat(40)).unwrap()
    }

    fn make_mutable(&self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o755)).unwrap();
            for name in ["agl-terminald", "agl-process-launcher", "agl-terminal"] {
                let path = self.root.join(name);
                if path.exists() {
                    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
                }
            }
            let manifest = self.root.join(TERMINAL_GENERATION_MANIFEST_FILE_NAME);
            if manifest.exists() {
                fs::set_permissions(manifest, fs::Permissions::from_mode(0o644)).unwrap();
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.make_mutable();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn sha256(path: &Path) -> String {
    let bytes = fs::read(path).unwrap();
    let digest = Sha256::digest(bytes);
    let mut identity = String::from("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut identity, "{byte:02x}").unwrap();
    }
    identity
}

// AGL178-TERM-MAN-001. One sealed v2 manifest authenticates the exact service,
// launcher and UI inventory and exposes a stable installed identity.
#[test]
fn v2_manifest_seals_the_complete_terminal_generation() {
    let fixture = Fixture::new("complete", b"ui-v1");
    let verified = fixture.seal();

    assert_eq!(
        verified.manifest().schema(),
        TERMINAL_GENERATION_MANIFEST_SCHEMA
    );
    assert_eq!(verified.manifest().source_revision(), "a".repeat(40));
    assert_eq!(verified.manifest().protocol_version(), 2);
    assert_eq!(
        verified
            .manifest()
            .files()
            .map(|file| file.role())
            .collect::<Vec<_>>(),
        vec![
            TerminalGenerationFileRole::Service,
            TerminalGenerationFileRole::Launcher,
            TerminalGenerationFileRole::Ui,
        ]
    );
    let manifest_path = fixture.root.join(TERMINAL_GENERATION_MANIFEST_FILE_NAME);
    assert_eq!(verified.manifest_digest().as_str(), sha256(&manifest_path));
    assert_eq!(
        verified.generation_directory_name(),
        format!(
            "generation-{}",
            verified
                .manifest_digest()
                .as_str()
                .strip_prefix("sha256:")
                .unwrap()
        )
    );
    assert_eq!(
        verified.identity().manifest_digest(),
        verified.manifest_digest()
    );
    assert_eq!(verified.identity().source_revision(), "a".repeat(40));
}

// AGL178-TERM-MAN-002. Any role change produces a distinct generation even
// when source and service bytes are identical.
#[test]
fn launcher_and_ui_bytes_participate_in_generation_identity() {
    let first_fixture = Fixture::new("identity-a", b"ui-a");
    let second_fixture = Fixture::new("identity-b", b"ui-b");
    let first = first_fixture.seal();
    let second = second_fixture.seal();
    assert_ne!(first.manifest_digest(), second.manifest_digest());
    assert_ne!(
        first.generation_directory_name(),
        second.generation_directory_name()
    );
}

// AGL178-TERM-MAN-003. A verified generation is fail-closed against changed
// bytes, legacy schemas, unsafe links and non-canonical source identity.
#[test]
fn generation_verification_rejects_every_identity_or_filesystem_drift() {
    let drifted = Fixture::new("drift", b"ui");
    drifted.seal();
    drifted.make_mutable();
    fs::write(drifted.root.join("agl-process-launcher"), b"changed").unwrap();
    assert!(VerifiedTerminalGeneration::load(&drifted.root).is_err());

    let legacy = Fixture::new("legacy", b"ui");
    fs::write(
        legacy.root.join(TERMINAL_GENERATION_MANIFEST_FILE_NAME),
        br#"{"schema":"agl-terminal.runtime-generation.v1"}"#,
    )
    .unwrap();
    assert!(VerifiedTerminalGeneration::load(&legacy.root).is_err());

    let invalid_source = Fixture::new("source", b"ui");
    assert!(TerminalGenerationManifest::seal(&invalid_source.root, "main").is_err());

    let extra = Fixture::new("extra", b"ui");
    fs::write(extra.root.join("unsealed-extra-file"), b"extra").unwrap();
    assert!(TerminalGenerationManifest::seal(&extra.root, &"d".repeat(40)).is_err());
    assert!(
        !extra
            .root
            .join(TERMINAL_GENERATION_MANIFEST_FILE_NAME)
            .exists()
    );

    #[cfg(unix)]
    {
        let symlinked = Fixture::new("symlink", b"ui");
        fs::remove_file(symlinked.root.join("agl-terminal")).unwrap();
        std::os::unix::fs::symlink(
            symlinked.root.join("agl-terminald"),
            symlinked.root.join("agl-terminal"),
        )
        .unwrap();
        assert!(TerminalGenerationManifest::seal(&symlinked.root, &"b".repeat(40)).is_err());

        let hardlinked = Fixture::new("hardlink", b"ui");
        fs::hard_link(
            hardlinked.root.join("agl-terminald"),
            hardlinked.root.join("extra-service-link"),
        )
        .unwrap();
        assert!(TerminalGenerationManifest::seal(&hardlinked.root, &"c".repeat(40)).is_err());
    }
}
