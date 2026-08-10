use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("agl-kernel is under <workspace>/crates")
        .to_path_buf()
}

fn metadata() -> Value {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(workspace_root())
        .output()
        .expect("cargo metadata starts");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata returns JSON")
}

fn production_rs_files(root: &Path) -> Vec<PathBuf> {
    fn visit(path: &Path, output: &mut Vec<PathBuf>) {
        let mut entries = fs::read_dir(path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
            .map(|entry| entry.expect("directory entry is readable").path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                if entry.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                visit(&entry, output);
            } else if entry.extension().is_some_and(|extension| extension == "rs")
                && entry.file_name().is_none_or(|name| name != "tests.rs")
                && !entry
                    .components()
                    .any(|component| component.as_os_str() == "tests")
            {
                output.push(entry);
            }
        }
    }

    let mut output = Vec::new();
    visit(root, &mut output);
    output
}

fn matches_in(files: impl IntoIterator<Item = PathBuf>, needles: &[&str]) -> Vec<String> {
    let mut matches = Vec::new();
    for path in files {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        for (index, line) in source.lines().enumerate() {
            if let Some(needle) = needles.iter().find(|needle| line.contains(**needle)) {
                matches.push(format!("{}:{}:{needle}", path.display(), index + 1));
            }
        }
    }
    matches
}

// KCT-ARCH-001 / AGL171-024. Mutation: restore one removed kernel owner, or
// let the author SDK own/re-export generic kernel contracts.
#[test]
fn obsolete_kernel_boundary_packages_are_absent() {
    let metadata = metadata();
    let packages = metadata["packages"].as_array().unwrap();
    let names = packages
        .iter()
        .filter_map(|package| package["name"].as_str())
        .collect::<BTreeSet<_>>();

    for removed in ["agl-turn", "agl-loop"] {
        assert!(
            !names.contains(removed),
            "removed package still exists: {removed}"
        );
        assert!(
            !workspace_root().join("crates").join(removed).exists(),
            "removed source directory still exists: crates/{removed}"
        );
    }
    assert!(names.contains("agl-kernel"));

    let extension = packages
        .iter()
        .find(|package| package["name"] == "agl-extension")
        .expect("agl-extension author/package SDK exists");
    let extension_dependencies = extension["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|dependency| dependency["name"].as_str())
        .collect::<BTreeSet<_>>();
    assert!(extension_dependencies.contains("agl-kernel"));

    let extension_source = workspace_root().join("crates/agl-extension/src");
    let forbidden = matches_in(
        production_rs_files(&extension_source),
        &[
            "pub struct ExtensionDescriptor",
            "pub struct ToolDeclaration",
            "pub struct HookDeclaration",
            "pub struct EffectDeclaration",
            "pub use agl_kernel::{",
            "pub use agl_kernel::",
        ],
    );
    assert!(
        forbidden.is_empty(),
        "agl-extension owns or re-exports kernel contracts: {forbidden:#?}"
    );
}

// KCT-ARCH-002. Mutation: add one outward or platform dependency to agl-kernel.
#[test]
fn kernel_dependency_direction_remains_inward() {
    let metadata = metadata();
    let packages = metadata["packages"].as_array().unwrap();
    let kernel = packages
        .iter()
        .find(|package| package["name"] == "agl-kernel")
        .expect("agl-kernel package exists");
    let dependency_kinds = kernel["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|dependency| {
            (
                dependency["name"].as_str().unwrap().to_string(),
                dependency["kind"].as_str().unwrap_or("normal").to_string(),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let forbidden = [
        "agl-app",
        "agl-chat",
        "agl-cli",
        "agl-core-tools",
        "agl-daemon",
        "agl-extension",
        "agl-host-tools",
        "agl-inference",
        "agl-loop",
        "agl-matrix-bridge",
        "agl-runtime",
        "agl-session",
        "agl-store",
        "agl-supervisor",
        "agl-terminal",
        "agl-terminal-client",
        "agl-terminal-protocol",
        "agl-turn",
        "tokio",
    ];
    let found = forbidden
        .into_iter()
        .filter(|name| {
            dependency_kinds
                .get(*name)
                .is_some_and(|kind| kind != "dev")
        })
        .collect::<Vec<_>>();
    assert!(found.is_empty(), "forbidden kernel dependencies: {found:?}");
}

// KCT-ARCH-003. Mutation: move concrete delegation into generic kernel source.
#[test]
fn kernel_contains_no_concrete_delegation_extension() {
    let kernel_source = workspace_root().join("crates/agl-kernel/src");
    let found = matches_in(
        production_rs_files(&kernel_source),
        &[
            "agent.delegate",
            "DelegateActionArgs",
            "delegation_extension",
        ],
    );
    assert!(
        found.is_empty(),
        "concrete delegation leaked into kernel: {found:#?}"
    );
}

// KCT-ARCH-004. Mutation: construct ToolEffectJournalRecord outside effect transitions.
#[test]
fn tool_effect_records_are_not_constructed_outside_the_effect_machine() {
    let kernel_source = workspace_root().join("crates/agl-kernel/src");
    let files = production_rs_files(&kernel_source);
    let owner = files
        .iter()
        .find(|path| {
            fs::read_to_string(path)
                .is_ok_and(|source| source.contains("struct ToolEffectJournalRecord"))
        })
        .expect("ToolEffectJournalRecord has one kernel source owner")
        .clone();
    let owner_source = fs::read_to_string(&owner).unwrap();
    let record = owner_source
        .split_once("struct ToolEffectJournalRecord")
        .map(|(_, suffix)| suffix.split_once('}').map_or(suffix, |(body, _)| body))
        .expect("ToolEffectJournalRecord declaration is readable");
    assert!(
        !record
            .lines()
            .any(|line| line.trim_start().starts_with("pub ")),
        "ToolEffectJournalRecord fields are publicly constructible in {}",
        owner.display()
    );
    let files = files.into_iter().filter(|path| path != &owner);
    let found = matches_in(files, &["ToolEffectJournalRecord {"]);
    assert!(
        found.is_empty(),
        "Tool effect record bypasses effect.rs: {found:#?}"
    );
}

// KCT-API-001 and KCT-CHK-002. Mutation: retain old request or Hook-repair vocabulary.
#[test]
fn removed_turn_and_hook_repair_vocabulary_has_no_production_owner() {
    let crates = workspace_root().join("crates");
    let found = matches_in(
        production_rs_files(&crates),
        &[
            "TurnEffect",
            "EffectKey",
            "CapabilityDispatch",
            "ExecutorPhase",
            "max_hook_repair_attempts",
            "pending_repair_message",
            "hook_repair_attempts",
            "TurnExecutor",
            "CapabilityId",
            "CapabilityDeclaration",
            "CapabilityBinding",
            "ProviderDescriptor",
            "ProviderRegistration",
        ],
    );
    assert!(
        found.is_empty(),
        "removed production vocabulary remains: {found:#?}"
    );
}

// KCT-ID-003. Mutation: keep an obsolete first-party ID or *-tools owner.
#[test]
fn obsolete_first_party_identities_have_no_production_use() {
    let crates = workspace_root().join("crates");
    let found = matches_in(
        production_rs_files(&crates),
        &[
            "memory.search",
            "repo.status",
            "screen.capture",
            "agent.delegate",
            "memory-tools",
            "repo-tools",
        ],
    );
    assert!(
        found.is_empty(),
        "obsolete first-party identities remain: {found:#?}"
    );
}

// KCT-API-001. Mutation: omit one fixed request or machine type from kernel ownership.
#[test]
fn kernel_source_owns_the_fixed_request_and_machine_vocabulary() {
    let kernel_source = workspace_root().join("crates/agl-kernel/src");
    let source = production_rs_files(&kernel_source)
        .into_iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    for required in [
        "TurnRequestKey",
        "TurnRequestKind",
        "TurnRequest",
        "TurnRequestOutcome",
        "TurnRequestResult",
        "TurnRequestFailure",
        "TurnAdvance",
        "TurnTerminal",
        "TurnMachine",
        "ChatSessionMachine",
        "ToolEffectMachine",
        "ExtensionId",
        "ExtensionDescriptor",
        "ExtensionRegistration",
        "ToolId",
        "ToolDeclaration",
        "ToolBinding",
        "EffectId",
        "EffectDeclaration",
        "HookId",
        "HookDeclaration",
        "HookBinding",
    ] {
        assert!(
            source.contains(required),
            "agl-kernel does not own required type {required}"
        );
    }
    assert!(
        !source.contains("KernelState"),
        "one global KernelState was introduced"
    );
}

// KCT-RUNTIME-005. Mutation: introduce linker or process-global factory discovery.
#[test]
fn runtime_has_no_global_static_extension_inventory() {
    let runtime_source = workspace_root().join("crates/agl-runtime/src");
    let found = matches_in(
        production_rs_files(&runtime_source),
        &[
            "inventory::",
            "linkme::",
            "OnceLock<StaticExtensionRegistry",
            "static STATIC_EXTENSION_REGISTRY",
        ],
    );
    assert!(
        found.is_empty(),
        "global factory discovery exists: {found:#?}"
    );
}

// AGL171-008, AGL171-010, AGL171-015 and AGL171-021. These source guards are
// deletion/boundary contracts; behavioral contracts live with their owners.
#[test]
fn extension_cutover_has_one_owner_and_no_registration_or_hook_bypass() {
    let root = workspace_root();
    let crates = root.join("crates");
    let metadata = metadata();
    let packages = metadata["packages"].as_array().unwrap();
    let names = packages
        .iter()
        .filter_map(|package| package["name"].as_str())
        .collect::<BTreeSet<_>>();

    for required in ["agl-package", "agl-artifact", "agl-extension"] {
        assert!(
            names.contains(required),
            "required package is absent: {required}"
        );
    }
    for forbidden in ["agl-hooks", "agl-workspace"] {
        assert!(
            !names.contains(forbidden),
            "forbidden package exists: {forbidden}"
        );
        assert!(
            !crates.join(forbidden).exists(),
            "forbidden source directory exists: crates/{forbidden}"
        );
    }

    let production = production_rs_files(&crates);
    let forbidden_source = matches_in(
        production.clone(),
        &[
            "PROVIDER_ID",
            "register(&mut ToolCatalog)",
            "ScriptHookRuntime",
            "ScriptHookTrust",
            "ExtensionAdminPort",
            "VerifiedExtensionBinary",
            "verify_extension_binary",
        ],
    );
    assert!(
        forbidden_source.is_empty(),
        "obsolete Extension/Hook bypass remains: {forbidden_source:#?}"
    );

    let kernel_source = production_rs_files(&crates.join("agl-kernel/src"));
    let kernel_handle_leak = matches_in(kernel_source, &["ArtifactHandle"]);
    assert!(
        kernel_handle_leak.is_empty(),
        "kernel public/runtime contracts import ArtifactHandle: {kernel_handle_leak:#?}"
    );

    let runtime_extension_source = production_rs_files(&crates.join("agl-runtime/src"));
    let live_admin = matches_in(
        runtime_extension_source,
        &[
            "fn install_extension",
            "fn remove_extension",
            "fn reload_extension",
            "fn trust_extension",
        ],
    );
    assert!(
        live_admin.is_empty(),
        "live Extension administration exists: {live_admin:#?}"
    );

    let package = packages
        .iter()
        .find(|package| package["name"] == "agl-package")
        .unwrap();
    let artifact = packages
        .iter()
        .find(|package| package["name"] == "agl-artifact")
        .unwrap();
    let kernel = packages
        .iter()
        .find(|package| package["name"] == "agl-kernel")
        .unwrap();
    let normal_dependencies = |package: &Value| {
        package["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|dependency| dependency["kind"].as_str().unwrap_or("normal") == "normal")
            .filter_map(|dependency| dependency["name"].as_str().map(str::to_owned))
            .collect::<BTreeSet<_>>()
    };
    assert!(normal_dependencies(artifact).contains("agl-kernel"));
    assert!(!normal_dependencies(kernel).contains("agl-artifact"));
    assert!(!normal_dependencies(package).contains("agl-artifact"));

    let artifact_source = production_rs_files(&crates.join("agl-artifact/src"));
    let duplicate_package_owner = matches_in(
        artifact_source,
        &[
            "pub struct PackageTreeDigest",
            "pub struct ArtifactPackageView",
            "pub use agl_package",
        ],
    );
    assert!(
        duplicate_package_owner.is_empty(),
        "agl-artifact duplicates/re-exports agl-package: {duplicate_package_owner:#?}"
    );
}
