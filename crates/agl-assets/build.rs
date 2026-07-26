use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const BUILTIN_SKILL_PACKS: &[&str] = &["agl"];
const BUILTIN_CORE_SKILLS_DIR: &str = "core-skills";

#[derive(Clone, Copy)]
enum AssetKind {
    SystemPrompt,
    ModelCatalog,
    ModelBenchmarkEvidence,
    Skill,
    SkillReference,
    SkillAsset,
    FunctionManifest,
    FunctionSystemPrompt,
    FunctionInferenceConfig,
}

impl AssetKind {
    fn rust_variant(self) -> &'static str {
        match self {
            Self::SystemPrompt => "BuiltinAssetKind::SystemPrompt",
            Self::ModelCatalog => "BuiltinAssetKind::ModelCatalog",
            Self::ModelBenchmarkEvidence => "BuiltinAssetKind::ModelBenchmarkEvidence",
            Self::Skill => "BuiltinAssetKind::Skill",
            Self::SkillReference => "BuiltinAssetKind::SkillReference",
            Self::SkillAsset => "BuiltinAssetKind::SkillAsset",
            Self::FunctionManifest => "BuiltinAssetKind::FunctionManifest",
            Self::FunctionSystemPrompt => "BuiltinAssetKind::FunctionSystemPrompt",
            Self::FunctionInferenceConfig => "BuiltinAssetKind::FunctionInferenceConfig",
        }
    }
}

struct Asset {
    id: String,
    kind: AssetKind,
    source_path: String,
    absolute_path: PathBuf,
    sha256: String,
}

struct ArtifactPackage {
    type_id: String,
    id: String,
    version: String,
    entrypoint: String,
    files: Vec<(String, usize)>,
    digest: String,
}

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo"),
    );
    let repo_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("agl-assets must live under crates/");
    let assets_root = repo_root.join("assets");
    let builtin_skills_root = assets_root.join(BUILTIN_CORE_SKILLS_DIR);
    let builtin_functions_root = assets_root.join("functions");
    let mut assets = Vec::new();
    let mut packages = Vec::new();

    println!("cargo:rerun-if-changed={}", assets_root.display());

    add_system_prompt(&mut assets, repo_root, &assets_root);
    add_model_catalog(&mut assets, repo_root, &assets_root);
    add_model_benchmark_evidence(&mut assets, repo_root, &assets_root);
    add_skills(&mut assets, &mut packages, repo_root, &builtin_skills_root);
    add_functions(
        &mut assets,
        &mut packages,
        repo_root,
        &builtin_functions_root,
    );
    validate_unique_asset_ids(&assets);
    validate_unique_package_ids(&packages);
    write_registry(&assets, &packages);
}

fn add_system_prompt(assets: &mut Vec<Asset>, repo_root: &Path, assets_root: &Path) {
    let path = assets_root.join("prompts/system/default.md");
    if !path.is_file() {
        panic!("missing builtin system prompt {}", path.display());
    }
    assets.push(asset(
        "builtin:default",
        AssetKind::SystemPrompt,
        repo_root,
        &path,
    ));
}

fn add_model_catalog(assets: &mut Vec<Asset>, repo_root: &Path, assets_root: &Path) {
    let path = assets_root.join("models/catalog.toml");
    if !path.is_file() {
        panic!("missing builtin model catalog {}", path.display());
    }
    assets.push(asset(
        "builtin:model-catalog",
        AssetKind::ModelCatalog,
        repo_root,
        &path,
    ));
}

fn add_model_benchmark_evidence(assets: &mut Vec<Asset>, repo_root: &Path, assets_root: &Path) {
    let root = assets_root.join("models/evidence");
    if !root.is_dir() {
        panic!(
            "missing model benchmark evidence directory {}",
            root.display()
        );
    }
    let paths = read_dir_sorted(&root);
    if paths.is_empty() {
        panic!(
            "model benchmark evidence directory is empty: {}",
            root.display()
        );
    }
    for path in paths {
        if !path.is_file() || path.extension().and_then(|value| value.to_str()) != Some("md") {
            panic!(
                "unsupported model benchmark evidence asset {}",
                path.display()
            );
        }
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .expect("model benchmark evidence filename must be UTF-8");
        validate_name(stem, "model benchmark evidence filename");
        assets.push(asset(
            &format!("model-benchmark:{stem}"),
            AssetKind::ModelBenchmarkEvidence,
            repo_root,
            &path,
        ));
    }
}

fn add_skills(
    assets: &mut Vec<Asset>,
    packages: &mut Vec<ArtifactPackage>,
    repo_root: &Path,
    skills_root: &Path,
) {
    let package_count_before = packages.len();
    if !skills_root.is_dir() {
        panic!(
            "missing builtin core skills checkout {}; run `git submodule update --init assets/core-skills`",
            skills_root.display()
        );
    }
    reject_symlink(skills_root);
    for pack_root in read_dir_sorted(skills_root)
        .into_iter()
        .filter(|path| path.is_dir())
    {
        reject_symlink(&pack_root);
        let pack = pack_root
            .file_name()
            .and_then(|name| name.to_str())
            .expect("builtin skill pack directory must have a UTF-8 name");
        validate_name(pack, "builtin skill pack directory");
        if !BUILTIN_SKILL_PACKS.contains(&pack) {
            panic!(
                "unsupported builtin skill pack directory {}; supported packs: {}",
                pack_root.display(),
                BUILTIN_SKILL_PACKS.join(", ")
            );
        }
        let mut skill_dirs = read_dir_sorted(&pack_root)
            .into_iter()
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        skill_dirs.sort();
        for skill_dir in skill_dirs {
            reject_symlink(&skill_dir);
            let name = skill_dir
                .file_name()
                .and_then(|name| name.to_str())
                .expect("skill directory must have a UTF-8 name");
            validate_name(name, "builtin skill directory");
            let skill_md = skill_dir.join("SKILL.md");
            if !skill_md.is_file() {
                panic!("builtin skill {} is missing SKILL.md", skill_dir.display());
            }
            let id = name.to_string();
            let skill_asset_index = assets.len();
            assets.push(asset(&id, AssetKind::Skill, repo_root, &skill_md));

            let reference_asset_indices = add_resource_dir(
                assets,
                repo_root,
                &skill_dir.join("references"),
                &id,
                AssetKind::SkillReference,
                "references",
            );
            let asset_indices = add_resource_dir(
                assets,
                repo_root,
                &skill_dir.join("assets"),
                &id,
                AssetKind::SkillAsset,
                "assets",
            );
            if skill_dir.join("scripts").exists() {
                panic!(
                    "builtin agl/dev skill scripts are not executable assets: {}",
                    skill_dir.join("scripts").display()
                );
            }

            let mut files = vec![("SKILL.md".to_string(), skill_asset_index)];
            files.extend(package_resource_files(
                assets,
                &skill_dir,
                &reference_asset_indices,
            ));
            files.extend(package_resource_files(assets, &skill_dir, &asset_indices));
            let digest = package_tree_hash(assets, &files);
            packages.push(ArtifactPackage {
                type_id: "skill".to_string(),
                id,
                version: "1.0.0".to_string(),
                entrypoint: "SKILL.md".to_string(),
                files,
                digest,
            });
        }
    }
    if packages.len() == package_count_before {
        panic!(
            "builtin core skills checkout {} contains no skills",
            skills_root.display()
        );
    }
}

fn add_functions(
    assets: &mut Vec<Asset>,
    packages: &mut Vec<ArtifactPackage>,
    repo_root: &Path,
    functions_root: &Path,
) {
    if !functions_root.exists() {
        return;
    }
    if !functions_root.is_dir() {
        panic!(
            "builtin functions root is not a directory: {}",
            functions_root.display()
        );
    }
    reject_symlink(functions_root);
    for function_dir in read_dir_sorted(functions_root)
        .into_iter()
        .filter(|path| path.is_dir())
    {
        reject_symlink(&function_dir);
        let id = function_dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("function directory must have a UTF-8 name");
        validate_name(id, "builtin function directory");

        let function_md = function_dir.join("FUNCTION.md");
        let system_prompt = function_dir.join("SYSTEM.md");
        let inference_config = function_dir.join("inference.toml");
        for required in [&function_md, &system_prompt, &inference_config] {
            if !required.is_file() {
                panic!("builtin function {} is missing {}", id, required.display());
            }
        }
        let inference_text = fs::read_to_string(&inference_config).unwrap_or_else(|error| {
            panic!(
                "failed to read builtin function inference preset {}: {error}",
                inference_config.display()
            )
        });
        agl_config::load_inference_preset_from_str(
            &inference_config.display().to_string(),
            &inference_text,
        )
        .unwrap_or_else(|error| {
            panic!(
                "builtin function inference preset {} must use portable model ids: {error:#}",
                inference_config.display()
            )
        });

        let function_asset_index = assets.len();
        assets.push(asset(
            &format!("function:{id}/FUNCTION.md"),
            AssetKind::FunctionManifest,
            repo_root,
            &function_md,
        ));
        let system_prompt_asset_index = assets.len();
        assets.push(asset(
            &format!("function:{id}/SYSTEM.md"),
            AssetKind::FunctionSystemPrompt,
            repo_root,
            &system_prompt,
        ));
        let inference_config_asset_index = assets.len();
        assets.push(asset(
            &format!("function:{id}/inference.toml"),
            AssetKind::FunctionInferenceConfig,
            repo_root,
            &inference_config,
        ));

        let files = vec![
            ("FUNCTION.md".to_string(), function_asset_index),
            ("SYSTEM.md".to_string(), system_prompt_asset_index),
            ("inference.toml".to_string(), inference_config_asset_index),
        ];
        let digest = package_tree_hash(assets, &files);
        packages.push(ArtifactPackage {
            type_id: "function".to_string(),
            id: id.to_string(),
            version: "1.0.0".to_string(),
            entrypoint: "FUNCTION.md".to_string(),
            files,
            digest,
        });
    }
}

fn add_resource_dir(
    assets: &mut Vec<Asset>,
    repo_root: &Path,
    root: &Path,
    skill_id: &str,
    kind: AssetKind,
    prefix: &str,
) -> Vec<usize> {
    if !root.exists() {
        return Vec::new();
    }
    reject_symlink(root);
    let mut indices = Vec::new();
    for path in files_recursive_sorted(root) {
        reject_symlink(&path);
        let relative = path
            .strip_prefix(root)
            .expect("resource path must be under resource root")
            .to_string_lossy()
            .replace('\\', "/");
        let id = format!("{skill_id}:{prefix}/{relative}");
        indices.push(assets.len());
        assets.push(asset(&id, kind, repo_root, &path));
    }
    indices
}

fn asset(id: &str, kind: AssetKind, repo_root: &Path, path: &Path) -> Asset {
    reject_symlink(path);
    println!("cargo:rerun-if-changed={}", path.display());
    let bytes =
        fs::read(path).unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let source_path = path
        .strip_prefix(repo_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    Asset {
        id: id.to_string(),
        kind,
        source_path,
        absolute_path: path.to_path_buf(),
        sha256: sha256_hex(&bytes),
    }
}

fn package_resource_files(
    assets: &[Asset],
    package_root: &Path,
    indices: &[usize],
) -> Vec<(String, usize)> {
    indices
        .iter()
        .map(|index| {
            let path = assets[*index]
                .absolute_path
                .strip_prefix(package_root)
                .expect("package asset must be under package root")
                .to_string_lossy()
                .replace('\\', "/");
            (path, *index)
        })
        .collect()
}

fn package_tree_hash(assets: &[Asset], files: &[(String, usize)]) -> String {
    let mut hasher = Sha256::new();
    for (path, index) in files {
        let asset = &assets[*index];
        hasher.update(path.as_bytes());
        hasher.update([0]);
        let bytes = fs::read(&asset.absolute_path).unwrap_or_else(|err| {
            panic!("failed to read {}: {err}", asset.absolute_path.display())
        });
        hasher.update(bytes);
        hasher.update([0]);
    }
    hex(&hasher.finalize())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

fn read_dir_sorted(path: &Path) -> Vec<PathBuf> {
    println!("cargo:rerun-if-changed={}", path.display());
    let mut entries = fs::read_dir(path)
        .unwrap_or_else(|err| panic!("failed to read directory {}: {err}", path.display()))
        .map(|entry| {
            entry
                .unwrap_or_else(|err| {
                    panic!(
                        "failed to read directory entry in {}: {err}",
                        path.display()
                    )
                })
                .path()
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

fn files_recursive_sorted(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in read_dir_sorted(root) {
        reject_symlink(&path);
        if path.is_dir() {
            files.extend(files_recursive_sorted(&path));
        } else if path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    files
}

fn reject_symlink(path: &Path) {
    let metadata = fs::symlink_metadata(path)
        .unwrap_or_else(|err| panic!("failed to inspect {}: {err}", path.display()));
    if metadata.file_type().is_symlink() {
        panic!(
            "builtin assets may not contain symlinks: {}",
            path.display()
        );
    }
}

fn validate_name(value: &str, field: &str) {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        panic!("{field} must be lowercase ASCII, digits, or hyphens: {value}");
    }
}

fn validate_unique_asset_ids(assets: &[Asset]) {
    let mut ids = std::collections::BTreeSet::new();
    for asset in assets {
        if !ids.insert(asset.id.as_str()) {
            panic!("duplicate builtin asset id {}", asset.id);
        }
    }
}

fn validate_unique_package_ids(packages: &[ArtifactPackage]) {
    let mut ids = std::collections::BTreeSet::new();
    for package in packages {
        if !ids.insert((package.type_id.as_str(), package.id.as_str())) {
            panic!(
                "duplicate builtin artifact package {}:{}",
                package.type_id, package.id
            );
        }
    }
}

fn write_registry(assets: &[Asset], packages: &[ArtifactPackage]) {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR must be set by Cargo"));
    let destination = out_dir.join("builtin_assets.rs");
    let mut output = String::new();
    output.push_str("// @generated by crates/agl-assets/build.rs\n");

    for (index, asset) in assets.iter().enumerate() {
        output.push_str(&format!(
            "static ASSET_{index}: BuiltinAsset = BuiltinAsset {{ id: {}, kind: {}, source_path: {}, sha256: {}, bytes: include_bytes!({}) }};\n",
            rust_string(&asset.id),
            asset.kind.rust_variant(),
            rust_string(&asset.source_path),
            rust_string(&asset.sha256),
            rust_string(&asset.absolute_path.to_string_lossy()),
        ));
    }

    output.push_str("pub static BUILTIN_ASSETS: &[&BuiltinAsset] = &[\n");
    for index in 0..assets.len() {
        output.push_str(&format!("    &ASSET_{index},\n"));
    }
    output.push_str("];\n");

    for (index, package) in packages.iter().enumerate() {
        output.push_str(&format!(
            "static PACKAGE_{index}_FILES: &[BuiltinArtifactFile] = &[\n"
        ));
        for (path, asset_index) in &package.files {
            output.push_str(&format!(
                "    BuiltinArtifactFile {{ path: {}, bytes: ASSET_{}.bytes }},\n",
                rust_string(path),
                asset_index
            ));
        }
        output.push_str("];\n");
    }

    output.push_str("pub static BUILTIN_ARTIFACT_PACKAGES: &[BuiltinArtifactPackage] = &[\n");
    for (index, package) in packages.iter().enumerate() {
        output.push_str(&format!(
            "    BuiltinArtifactPackage {{ type_id: {}, id: {}, version: {}, entrypoint: {}, files: PACKAGE_{index}_FILES, digest: \"sha256:{}\" }},\n",
            rust_string(&package.type_id),
            rust_string(&package.id),
            rust_string(&package.version),
            rust_string(&package.entrypoint),
            package.digest,
        ));
    }
    output.push_str("];\n");

    fs::write(&destination, output)
        .unwrap_or_else(|err| panic!("failed to write {}: {err}", destination.display()));
}

fn rust_string(value: &str) -> String {
    format!("{value:?}")
}
