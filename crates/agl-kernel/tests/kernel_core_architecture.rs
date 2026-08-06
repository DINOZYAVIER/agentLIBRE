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

// KCT-ARCH-001. Mutation: restore one removed package or source directory.
#[test]
fn obsolete_kernel_boundary_packages_are_absent() {
    let metadata = metadata();
    let names = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|package| package["name"].as_str())
        .collect::<BTreeSet<_>>();

    for removed in ["agl-extension", "agl-turn", "agl-loop"] {
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
