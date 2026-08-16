use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agl_extension::package::{
    ExtensionPackage, ExtensionPackageBuildInput, ExtensionPackageBuilder, ExtensionPackageError,
    ExtensionPackageReport,
};
use agl_kernel::{ArtifactAccess, ArtifactDeclaration, ArtifactId, ArtifactKindId, ExtensionId};
use agl_package::{InMemoryPackageView, PackageRelativePath, compute_package_digest};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const MINIMAL_ROOT: &str = r#"{"schema":"agentlibre.extension-root/v1","package":{"schema":"agentlibre.package/v1","type":"extension","id":"example.package","version":"1.0.0","payload_schema":"agentlibre.extension-root/v1","agl":{"compatible":"^1","tested":["1.0.0"]},"requires":[]},"indexes":{"tools":"indexes/tools.json","hooks":"indexes/hooks.json","effects":"indexes/effects.json","artifacts":"indexes/artifacts.json"}}"#;

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "agl171-extension-{label}-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    path
}

fn definition() -> agl_extension::ExtensionDefinition {
    agl_extension::ExtensionDefinition::builder(
        ExtensionId::new("example.package").unwrap(),
        "Package fixture",
        "1.0.0",
        1,
    )
    .artifact(
        ArtifactDeclaration::new(
            ArtifactId::new("example.package:data").unwrap(),
            ArtifactKindId::new("agl.file-tree").unwrap(),
            [ArtifactAccess::ReadTree],
        )
        .unwrap(),
    )
    .build()
    .unwrap()
}

fn read_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                visit(root, &entry, files);
            } else {
                files.insert(
                    entry.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(entry).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

// AGL171-005, AGL171-006, AGL171-017 and AGL171-019.
#[test]
fn generator_is_byte_deterministic_read_only_and_has_no_binary_identity() {
    let source = temp_dir("source");
    fs::write(source.join("README.md"), b"authored input").unwrap();
    let source_before = read_tree(&source);
    let first = temp_dir("first");
    let second = temp_dir("second");
    let input = ExtensionPackageBuildInput::new(definition(), &source).unwrap();

    let first_report: ExtensionPackageReport =
        ExtensionPackageBuilder::build(&input, &first).unwrap();
    let second_report = ExtensionPackageBuilder::build(&input, &second).unwrap();
    assert_eq!(read_tree(&first), read_tree(&second));
    assert_eq!(read_tree(&source), source_before);
    assert_eq!(first_report.declaration_digest, definition().digest());
    assert_eq!(
        first_report.package_tree_digest,
        second_report.package_tree_digest
    );

    let root: serde_json::Value =
        serde_json::from_slice(&fs::read(first.join("extension-root.json")).unwrap()).unwrap();
    assert!(root.get("implementation").is_none());
    assert!(root.get("binary").is_none());
    let encoded = serde_json::to_string(&root).unwrap();
    assert!(!encoded.contains(&source.display().to_string()));
    assert!(!encoded.contains("timestamp"));

    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(first).unwrap();
    fs::remove_dir_all(second).unwrap();
}

// AGL172-048. The Extension-specific root remains modular, while its
// top-level package envelope uses the one common package key/schema.
#[test]
fn generated_extension_root_uses_the_common_package_envelope_only() {
    let source = temp_dir("package-envelope-source");
    let output = temp_dir("package-envelope-output");
    let input = ExtensionPackageBuildInput::new(definition(), &source).unwrap();
    ExtensionPackageBuilder::build(&input, &output).unwrap();
    let root: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("extension-root.json")).unwrap()).unwrap();

    assert_eq!(root["schema"], "agentlibre.extension-root/v1");
    assert_eq!(root["package"]["schema"], "agentlibre.package/v1");
    assert_eq!(root["package"]["type"], "extension");
    assert!(root.get("artifact").is_none());
    let old_schema = ["agentlibre.", "artifact", "/v1"].concat();
    assert!(!serde_json::to_string(&root).unwrap().contains(&old_schema));

    fs::remove_dir_all(source).unwrap();
    fs::remove_dir_all(output).unwrap();
}

fn view(entries: &[(&str, &str)]) -> InMemoryPackageView {
    InMemoryPackageView::new(entries.iter().map(|(path, body)| {
        (
            path.parse::<PackageRelativePath>().unwrap(),
            body.as_bytes().to_vec(),
        )
    }))
    .unwrap()
}

// AGL171-019 and AGL171-026.
#[test]
fn parser_accepts_exact_sorted_id_path_indexes() {
    let package = view(&[
        ("extension-root.json", MINIMAL_ROOT),
        (
            "indexes/tools.json",
            r#"{"schema":"agentlibre.extension-tool-index/v1","entries":[{"id":"example.package:a","path":"tools/a.json"},{"id":"example.package:z","path":"tools/z.json"}]}"#,
        ),
        (
            "indexes/hooks.json",
            r#"{"schema":"agentlibre.extension-hook-index/v1","entries":[]}"#,
        ),
        (
            "indexes/effects.json",
            r#"{"schema":"agentlibre.extension-effect-index/v1","entries":[]}"#,
        ),
        (
            "indexes/artifacts.json",
            r#"{"schema":"agentlibre.extension-artifact-index/v1","entries":[]}"#,
        ),
        (
            "tools/a.json",
            r#"{"schema":"agentlibre.tool/v1","id":"example.package:a"}"#,
        ),
        (
            "tools/z.json",
            r#"{"schema":"agentlibre.tool/v1","id":"example.package:z"}"#,
        ),
    ]);

    let parsed = ExtensionPackage::parse(&package).unwrap();
    assert_eq!(
        parsed
            .tool_ids()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["example.package:a", "example.package:z"]
    );
    assert_eq!(
        parsed.package_tree_digest(),
        &compute_package_digest(&package).unwrap()
    );
}

// AGL171-013 and AGL171-026.
#[test]
fn parser_returns_typed_exact_index_errors() {
    let cases = [
        (r#"{"schema":"unknown","entries":[]}"#, "unknown-schema"),
        (
            r#"{"schema":"agentlibre.extension-tool-index/v1","entries":[{"id":"example.package:a","path":"tools/a.json"},{"id":"example.package:a","path":"tools/b.json"}]}"#,
            "duplicate-id",
        ),
        (
            r#"{"schema":"agentlibre.extension-tool-index/v1","entries":[{"id":"example.package:a","path":"tools/a.json"},{"id":"example.package:b","path":"tools/a.json"}]}"#,
            "duplicate-path",
        ),
        (
            r#"{"schema":"agentlibre.extension-tool-index/v1","entries":[{"id":"example.package:z","path":"tools/z.json"},{"id":"example.package:a","path":"tools/a.json"}]}"#,
            "unsorted",
        ),
        (
            r#"{"schema":"agentlibre.extension-tool-index/v1","entries":[{"id":"example.package:a","path":"../escape.json"}]}"#,
            "unsafe-path",
        ),
    ];
    for (tools_index, label) in cases {
        let package = view(&[
            ("extension-root.json", MINIMAL_ROOT),
            ("indexes/tools.json", tools_index),
            (
                "indexes/hooks.json",
                r#"{"schema":"agentlibre.extension-hook-index/v1","entries":[]}"#,
            ),
            (
                "indexes/effects.json",
                r#"{"schema":"agentlibre.extension-effect-index/v1","entries":[]}"#,
            ),
            (
                "indexes/artifacts.json",
                r#"{"schema":"agentlibre.extension-artifact-index/v1","entries":[]}"#,
            ),
        ]);
        let error = ExtensionPackage::parse(&package).unwrap_err();
        assert!(
            matches!(error, ExtensionPackageError::Index { ref path, .. } if path == &"indexes/tools.json".parse().unwrap()),
            "wrong error for {label}: {error:?}"
        );
    }
}

// AGL171-003, AGL171-013 and AGL171-026.
#[test]
fn parser_rejects_missing_unlisted_and_id_file_mismatch() {
    let missing = view(&[
        ("extension-root.json", MINIMAL_ROOT),
        (
            "indexes/tools.json",
            r#"{"schema":"agentlibre.extension-tool-index/v1","entries":[{"id":"example.package:a","path":"tools/a.json"}]}"#,
        ),
        (
            "indexes/hooks.json",
            r#"{"schema":"agentlibre.extension-hook-index/v1","entries":[]}"#,
        ),
        (
            "indexes/effects.json",
            r#"{"schema":"agentlibre.extension-effect-index/v1","entries":[]}"#,
        ),
        (
            "indexes/artifacts.json",
            r#"{"schema":"agentlibre.extension-artifact-index/v1","entries":[]}"#,
        ),
    ]);
    assert!(
        matches!(ExtensionPackage::parse(&missing), Err(ExtensionPackageError::MissingDeclaration { path, .. }) if path == "tools/a.json".parse().unwrap())
    );

    let mismatch = view(&[
        ("extension-root.json", MINIMAL_ROOT),
        (
            "indexes/tools.json",
            r#"{"schema":"agentlibre.extension-tool-index/v1","entries":[{"id":"example.package:a","path":"tools/a.json"}]}"#,
        ),
        (
            "indexes/hooks.json",
            r#"{"schema":"agentlibre.extension-hook-index/v1","entries":[]}"#,
        ),
        (
            "indexes/effects.json",
            r#"{"schema":"agentlibre.extension-effect-index/v1","entries":[]}"#,
        ),
        (
            "indexes/artifacts.json",
            r#"{"schema":"agentlibre.extension-artifact-index/v1","entries":[]}"#,
        ),
        (
            "tools/a.json",
            r#"{"schema":"agentlibre.tool/v1","id":"example.package:b"}"#,
        ),
        (
            "tools/unlisted.json",
            r#"{"schema":"agentlibre.tool/v1","id":"example.package:unlisted"}"#,
        ),
    ]);
    assert!(
        matches!(ExtensionPackage::parse(&mismatch), Err(ExtensionPackageError::DeclarationIdMismatch { path, .. }) if path == "tools/a.json".parse().unwrap())
    );

    let unlisted = view(&[
        ("extension-root.json", MINIMAL_ROOT),
        (
            "indexes/tools.json",
            r#"{"schema":"agentlibre.extension-tool-index/v1","entries":[]}"#,
        ),
        (
            "indexes/hooks.json",
            r#"{"schema":"agentlibre.extension-hook-index/v1","entries":[]}"#,
        ),
        (
            "indexes/effects.json",
            r#"{"schema":"agentlibre.extension-effect-index/v1","entries":[]}"#,
        ),
        (
            "indexes/artifacts.json",
            r#"{"schema":"agentlibre.extension-artifact-index/v1","entries":[]}"#,
        ),
        (
            "tools/unlisted.json",
            r#"{"schema":"agentlibre.tool/v1","id":"example.package:unlisted"}"#,
        ),
    ]);
    assert!(
        matches!(ExtensionPackage::parse(&unlisted), Err(ExtensionPackageError::UnlistedDeclaration { path, .. }) if path == "tools/unlisted.json".parse().unwrap())
    );
}
