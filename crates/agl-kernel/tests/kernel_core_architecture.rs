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
            "pub struct PackageView",
            "pub use agl_package",
        ],
    );
    assert!(
        duplicate_package_owner.is_empty(),
        "agl-artifact duplicates/re-exports agl-package: {duplicate_package_owner:#?}"
    );
}

// AGL172-004, AGL172-008, AGL172-014, AGL172-015, AGL172-031,
// AGL172-037 and AGL172-056. The metadata graph is the executable ownership
// contract; source scans below cover vocabulary, not dependency direction.
#[test]
fn package_artifact_workspace_and_git_owners_have_exact_dependencies() {
    let metadata = metadata();
    let packages = metadata["packages"].as_array().unwrap();
    let by_name = |name: &str| {
        packages
            .iter()
            .find(|package| package["name"] == name)
            .unwrap_or_else(|| panic!("missing package {name}"))
    };
    let normal_dependencies = |name: &str| {
        by_name(name)["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|dependency| dependency["kind"].as_str().unwrap_or("normal") == "normal")
            .filter_map(|dependency| dependency["name"].as_str())
            .collect::<BTreeSet<_>>()
    };

    let names = packages
        .iter()
        .filter_map(|package| package["name"].as_str())
        .collect::<BTreeSet<_>>();
    assert!(!names.contains("agl-workspace"));

    assert_eq!(
        normal_dependencies("agl-artifact")
            .intersection(&["agl-kernel"].into_iter().collect())
            .copied()
            .collect::<BTreeSet<_>>(),
        ["agl-kernel"].into_iter().collect()
    );
    assert!(!normal_dependencies("agl-artifact").contains("agl-repo"));
    assert!(normal_dependencies("agl-repo").contains("agl-artifact"));
    assert!(normal_dependencies("agl-runtime").contains("agl-artifact"));
    assert!(normal_dependencies("agl-runtime").contains("agl-package"));
    assert!(!normal_dependencies("agl-runtime").contains("agl-repo"));
    assert!(!normal_dependencies("agl-kernel").contains("agl-artifact"));
    assert!(!normal_dependencies("agl-package").contains("agl-kernel"));

    let runtime_git_access = matches_in(
        production_rs_files(&workspace_root().join("crates/agl-runtime/src")),
        &["agl_repo::", "Command::new(\"git\")"],
    );
    assert!(
        runtime_git_access.is_empty(),
        "agl-runtime opens package repositories directly: {runtime_git_access:#?}"
    );
}

// AGL172-069. The product layer prepares repository-backed package sources
// before runtime composition. The host Tool implementation only consumes the
// typed value passed by agl-chat.
#[test]
fn product_composition_prepares_package_sources_before_runtime() {
    let crates = workspace_root().join("crates");
    for product in ["agl-cli", "agl-chat"] {
        let sources = production_rs_files(&crates.join(product).join("src"));
        let prepared = matches_in(sources, &["agl_repo::package_composition_input"]);
        assert!(
            !prepared.is_empty(),
            "{product} does not prepare package composition input through agl-repo"
        );
    }
    let host_repo_access = matches_in(
        production_rs_files(&crates.join("agl-host-tools/src")),
        &["agl_repo::"],
    );
    assert!(
        host_repo_access.is_empty(),
        "agl-host-tools opens repositories directly: {host_repo_access:#?}"
    );
}

// AGL172-005, AGL172-010, AGL172-032, AGL172-047, AGL172-051,
// AGL172-058, AGL172-059 and AGL172-063.
#[test]
fn breaking_cutover_leaves_no_old_package_component_profile_or_blob_names() {
    let found = matches_in(
        production_rs_files(&workspace_root().join("crates")),
        &[
            "RepoManifest",
            "WorkspaceFunctions",
            "from_v2_manifest",
            "to_v2_manifest",
            "WorkspaceProfile",
            "ArtifactPackageId",
            "ArtifactPackageRef",
            "ArtifactPackageView",
            "ArtifactAdapterRegistry",
            "ArtifactResolver",
            "ArtifactLock",
            "ResolvedArtifactGraph",
            "WorkspaceComponentKind",
            "WorkspaceComponent",
            "LockedWorkspaceComponent",
            "ArtifactDataClass",
            "ComponentPathHandleRequest",
            "ComponentHandle",
            "SkillArtifactDeclaration",
            "SkillArtifactKind",
            "SkillArtifactAccess",
            "SkillArtifactFolder",
            "StoredArtifact",
            "write_artifact",
        ],
    );
    assert!(
        found.is_empty(),
        "obsolete AGL-172 vocabulary remains: {found:#?}"
    );
}

#[test]
fn content_attachment_cutover_has_no_blob_artifact_api() {
    let crates = workspace_root().join("crates");
    let found = matches_in(
        [
            "agl-content",
            "agl-store",
            "agl-host-tools",
            "agl-inference",
        ]
        .into_iter()
        .flat_map(|name| production_rs_files(&crates.join(name).join("src"))),
        &[
            "StoredArtifact",
            "ResolvedArtifact",
            "write_artifact",
            "resolve_artifact",
        ],
    );
    assert!(
        found.is_empty(),
        "obsolete content attachment vocabulary remains: {found:#?}"
    );
}

// AGL172-048 and AGL172-059. This includes authored/generated fixtures and
// scripts, because a breaking wire cutover cannot leave a second accepted
// package schema outside Rust production source.
#[test]
fn package_wire_format_has_one_key_schema_and_lock_path() {
    fn visit(path: &Path, files: &mut Vec<PathBuf>) {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                if matches!(
                    entry.file_name().and_then(|name| name.to_str()),
                    Some(".agl" | ".git" | "target" | "vendor")
                ) {
                    continue;
                }
                visit(&entry, files);
            } else {
                files.push(entry);
            }
        }
    }

    let mut files = Vec::new();
    visit(&workspace_root(), &mut files);
    let old_schema = ["agentlibre.", "artifact", "/v1"].concat();
    let old_lock = ["artifact", "-lock.toml"].concat();
    let old_lock_path = [".agl/", old_lock.as_str()].concat();
    let old_frontmatter = ["artifact", ": agentlibre"].concat();
    let old_table = ["[", "artifact", "]"].concat();
    let found = matches_in(
        files,
        &[
            old_schema.as_str(),
            old_lock_path.as_str(),
            old_lock.as_str(),
            old_frontmatter.as_str(),
            old_table.as_str(),
        ],
    );
    assert!(
        found.is_empty(),
        "obsolete package wire material remains: {found:#?}"
    );
}

// AGL172-052, AGL172-054 and AGL172-063. Parser/help behavior is also tested
// through the real agl binary; this guard prevents hidden production aliases.
#[test]
fn removed_repo_component_profile_and_skill_folder_commands_have_no_owner() {
    let found = matches_in(
        production_rs_files(&workspace_root().join("crates")),
        &[
            "RepoImportProfile",
            "RepoExportProfile",
            "RepoInitComponent",
            "RepoComponent",
            "RepoStatus",
            "SyncFolders",
            "init_repo_component",
            "status_repo_workspace",
            "REPO_IMPORT_PROFILE_TOOL_ID",
            "REPO_EXPORT_PROFILE_TOOL_ID",
            "REPO_STATUS_TOOL_ID",
        ],
    );
    assert!(
        found.is_empty(),
        "removed command/tool owner remains: {found:#?}"
    );
}

// AGL172-002 and AGL172-004. Domain packages may decode their own payload,
// but runtime package selection has one production resolver/composition.
#[test]
fn runtime_package_selection_has_no_domain_local_resolver_or_registry() {
    let crates = workspace_root().join("crates");
    let found = matches_in(
        ["agl-function", "agl-model", "agl-skill"]
            .into_iter()
            .flat_map(|name| production_rs_files(&crates.join(name).join("src"))),
        &[
            "ArtifactResolver::new",
            "PackageResolver::new",
            "ArtifactAdapterRegistry::new",
            "PackageAdapterRegistry::new",
        ],
    );
    assert!(
        found.is_empty(),
        "domain-local runtime selection remains: {found:#?}"
    );
}
