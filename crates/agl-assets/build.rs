use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use agl_package::{InMemoryPackageView, PackageEnvelope, compute_package_digest};
use sha2::{Digest, Sha256};

const BUILTIN_SKILL_PACKS: &[&str] = &["agl"];
const BUILTIN_CORE_SKILLS_DIR: &str = "core-skills";

#[derive(Clone, Copy)]
enum AssetKind {
    SystemPrompt,
    ModelDescriptor,
    ModelEvidence,
    Skill,
    SkillReference,
    SkillAsset,
    FunctionManifest,
    FunctionSystemPrompt,
    FunctionInferenceConfig,
    ExtensionPackageFile,
}

impl AssetKind {
    fn rust_variant(self) -> &'static str {
        match self {
            Self::SystemPrompt => "BuiltinAssetKind::SystemPrompt",
            Self::ModelDescriptor => "BuiltinAssetKind::ModelDescriptor",
            Self::ModelEvidence => "BuiltinAssetKind::ModelEvidence",
            Self::Skill => "BuiltinAssetKind::Skill",
            Self::SkillReference => "BuiltinAssetKind::SkillReference",
            Self::SkillAsset => "BuiltinAssetKind::SkillAsset",
            Self::FunctionManifest => "BuiltinAssetKind::FunctionManifest",
            Self::FunctionSystemPrompt => "BuiltinAssetKind::FunctionSystemPrompt",
            Self::FunctionInferenceConfig => "BuiltinAssetKind::FunctionInferenceConfig",
            Self::ExtensionPackageFile => "BuiltinAssetKind::ExtensionPackageFile",
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

struct Package {
    type_id: String,
    id: String,
    version: String,
    entrypoint: String,
    requires: Vec<String>,
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
    let builtin_extensions_root = assets_root.join("extensions");
    let mut assets = Vec::new();
    let mut packages = Vec::new();

    println!("cargo:rerun-if-changed={}", assets_root.display());

    add_system_prompt(&mut assets, repo_root, &assets_root);
    add_models(
        &mut assets,
        &mut packages,
        repo_root,
        &assets_root.join("models"),
    );
    add_skills(&mut assets, &mut packages, repo_root, &builtin_skills_root);
    add_functions(
        &mut assets,
        &mut packages,
        repo_root,
        &builtin_functions_root,
    );
    add_extensions(
        &mut assets,
        &mut packages,
        repo_root,
        &builtin_extensions_root,
    );
    validate_unique_asset_ids(&assets);
    validate_unique_package_ids(&packages);
    validate_builtin_catalog_baseline(
        &packages,
        &assets_root.join("builtin-catalog-baseline.toml"),
    );
    write_registry(&assets, &packages);
}

fn add_extensions(
    assets: &mut Vec<Asset>,
    packages: &mut Vec<Package>,
    repo_root: &Path,
    extensions_root: &Path,
) {
    if !extensions_root.is_dir() {
        panic!(
            "builtin extensions root is not a directory: {}",
            extensions_root.display()
        );
    }
    reject_symlink(extensions_root);
    for extension_dir in read_dir_sorted(extensions_root)
        .into_iter()
        .filter(|path| path.is_dir())
    {
        reject_symlink(&extension_dir);
        let id = extension_dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("extension directory must have a UTF-8 name");
        validate_extension_name(id);
        let root_path = extension_dir.join("extension-root.json");
        if !root_path.is_file() {
            panic!("builtin extension {id} is missing extension-root.json");
        }
        let root_bytes = fs::read(&root_path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", root_path.display()));
        let root: serde_json::Value = serde_json::from_slice(&root_bytes)
            .unwrap_or_else(|error| panic!("failed to parse {}: {error}", root_path.display()));
        let root_id = root
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("{} has no Extension id", root_path.display()));
        if root_id != id {
            panic!(
                "builtin extension directory {id} does not match Extension id {root_id} in {}",
                root_path.display()
            );
        }
        let version = root
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("{} has no Extension version", root_path.display()));
        let requires = root
            .get("requirements")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .map(|requirement| {
                let extension_id = requirement
                    .get("extension_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_else(|| panic!("invalid requirement in {}", root_path.display()));
                let api_major = requirement
                    .get("api_major")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_else(|| panic!("invalid requirement in {}", root_path.display()));
                format!("extension:{extension_id}@^{api_major}.0")
            })
            .collect::<Vec<_>>();
        let mut files = Vec::new();
        for path in files_recursive_sorted(&extension_dir) {
            reject_symlink(&path);
            let relative = path
                .strip_prefix(&extension_dir)
                .expect("Extension package file is under its package root")
                .to_string_lossy()
                .replace('\\', "/");
            let asset_index = assets.len();
            assets.push(asset(
                &format!("extension:{id}/{relative}"),
                AssetKind::ExtensionPackageFile,
                repo_root,
                &path,
            ));
            files.push((relative, asset_index));
        }
        packages.push(Package {
            type_id: "extension".to_owned(),
            id: id.to_owned(),
            version: version.to_owned(),
            entrypoint: "extension-root.json".to_owned(),
            requires,
            digest: package_tree_digest(assets, &files),
            files,
        });
    }
}

fn add_models(
    assets: &mut Vec<Asset>,
    packages: &mut Vec<Package>,
    repo_root: &Path,
    models_root: &Path,
) {
    for model_dir in read_dir_sorted(models_root)
        .into_iter()
        .filter(|path| path.is_dir() && path.join("MODEL.toml").is_file())
    {
        reject_symlink(&model_dir);
        let id = model_dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("model directory must have a UTF-8 name");
        validate_name(id, "builtin model directory");
        let descriptor = model_dir.join("MODEL.toml");
        if !descriptor.is_file() {
            panic!("builtin model {id} is missing MODEL.toml");
        }
        let descriptor_text = fs::read_to_string(&descriptor)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", descriptor.display()));
        let document: toml::Value = toml::from_str(&descriptor_text)
            .unwrap_or_else(|err| panic!("failed to parse {}: {err}", descriptor.display()));
        let artifact = document
            .get("package")
            .cloned()
            .unwrap_or_else(|| panic!("model {id} has no artifact table"));
        let envelope = artifact
            .try_into::<PackageEnvelope>()
            .unwrap_or_else(|error| {
                panic!(
                    "failed to parse package envelope from {}: {error}",
                    descriptor.display()
                )
            });
        validate_envelope(&envelope, "model", id, &descriptor);
        let descriptor_index = assets.len();
        assets.push(asset(
            &format!("model:{id}/MODEL.toml"),
            AssetKind::ModelDescriptor,
            repo_root,
            &descriptor,
        ));
        let mut files = vec![("MODEL.toml".to_string(), descriptor_index)];
        let evidence_root = model_dir.join("evidence");
        if evidence_root.is_dir() {
            let evidence_indices = add_resource_dir(
                assets,
                repo_root,
                &evidence_root,
                id,
                AssetKind::ModelEvidence,
                "evidence",
            );
            files.extend(package_resource_files(
                assets,
                &model_dir,
                &evidence_indices,
            ));
        }
        let digest = package_tree_digest(assets, &files);
        packages.push(Package {
            type_id: envelope.type_id.to_string(),
            id: envelope.id.to_string(),
            version: envelope.version.to_string(),
            entrypoint: "MODEL.toml".to_string(),
            requires: envelope.requires.iter().map(ToString::to_string).collect(),
            files,
            digest,
        });
    }
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

fn add_skills(
    assets: &mut Vec<Asset>,
    packages: &mut Vec<Package>,
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
        .filter(|path| path.file_name().is_none_or(|name| name != ".git"))
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
            let envelope = markdown_envelope(&skill_md);
            validate_envelope(&envelope, "skill", name, &skill_md);
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
            let digest = package_tree_digest(assets, &files);
            packages.push(Package {
                type_id: envelope.type_id.to_string(),
                id,
                version: envelope.version.to_string(),
                entrypoint: "SKILL.md".to_string(),
                requires: envelope.requires.iter().map(ToString::to_string).collect(),
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
    packages: &mut Vec<Package>,
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
        let envelope = markdown_envelope(&function_md);
        validate_envelope(&envelope, "function", id, &function_md);
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
        let digest = package_tree_digest(assets, &files);
        packages.push(Package {
            type_id: envelope.type_id.to_string(),
            id: envelope.id.to_string(),
            version: envelope.version.to_string(),
            entrypoint: "FUNCTION.md".to_string(),
            requires: envelope.requires.iter().map(ToString::to_string).collect(),
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

fn package_tree_digest(assets: &[Asset], files: &[(String, usize)]) -> String {
    let view = InMemoryPackageView::new(files.iter().map(|(path, index)| {
        let asset = &assets[*index];
        let path = path
            .parse()
            .unwrap_or_else(|error| panic!("invalid builtin package path {path:?}: {error}"));
        let bytes = fs::read(&asset.absolute_path).unwrap_or_else(|error| {
            panic!("failed to read {}: {error}", asset.absolute_path.display())
        });
        (path, bytes)
    }))
    .unwrap_or_else(|error| panic!("invalid builtin package tree: {error}"));
    compute_package_digest(&view)
        .unwrap_or_else(|error| panic!("failed to digest builtin package tree: {error}"))
        .to_string()
}

fn markdown_envelope(path: &Path) -> PackageEnvelope {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    let frontmatter = split_frontmatter(&text).unwrap_or_else(|| {
        panic!(
            "builtin artifact entrypoint {} has no terminated YAML frontmatter",
            path.display()
        )
    });
    let document = serde_yaml::from_str::<serde_yaml::Value>(frontmatter)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()));
    let artifact = document
        .get("package")
        .cloned()
        .unwrap_or_else(|| panic!("{} has no package envelope", path.display()));
    serde_yaml::from_value::<PackageEnvelope>(artifact).unwrap_or_else(|error| {
        panic!(
            "failed to parse package envelope from {}: {error}",
            path.display()
        )
    })
}

fn split_frontmatter(text: &str) -> Option<&str> {
    if let Some(rest) = text.strip_prefix("---\n") {
        return rest
            .split_once("\n---\n")
            .map(|(frontmatter, _)| frontmatter);
    }
    text.strip_prefix("---\r\n")?
        .split_once("\r\n---\r\n")
        .map(|(frontmatter, _)| frontmatter)
}

fn validate_envelope(envelope: &PackageEnvelope, type_id: &str, id: &str, path: &Path) {
    envelope
        .validate()
        .unwrap_or_else(|error| panic!("invalid package envelope in {}: {error}", path.display()));
    if envelope.type_id.as_str() != type_id {
        panic!(
            "builtin package {} has artifact type {}; expected {type_id}",
            path.display(),
            envelope.type_id
        );
    }
    if envelope.id.as_str() != id {
        panic!(
            "builtin package directory {id} does not match artifact id {} in {}",
            envelope.id,
            path.display()
        );
    }
}

fn exact_reference(package: &Package) -> String {
    format!("{}:{}@{}", package.type_id, package.id, package.version)
}

fn builtin_catalog_digest(packages: &[Package]) -> String {
    let mut packages = packages.iter().collect::<Vec<_>>();
    packages.sort_by_key(|package| exact_reference(package));
    let mut hasher = Sha256::new();
    hasher.update(b"agentlibre.builtin-catalog.v1\0");
    for package in packages {
        hash_catalog_field(&mut hasher, exact_reference(package).as_bytes());
        hash_catalog_field(&mut hasher, package.digest.as_bytes());
        hash_catalog_field(&mut hasher, package.entrypoint.as_bytes());
        let mut requires = package.requires.iter().collect::<Vec<_>>();
        requires.sort();
        hasher.update((requires.len() as u64).to_be_bytes());
        for requirement in requires {
            hash_catalog_field(&mut hasher, requirement.as_bytes());
        }
    }
    format!("sha256:{}", hex(&hasher.finalize()))
}

fn hash_catalog_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn validate_builtin_catalog_baseline(packages: &[Package], path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
    let text = fs::read_to_string(path).unwrap_or_else(|error| {
        panic!(
            "failed to read immutable builtin catalog baseline {}: {error}",
            path.display()
        )
    });
    let document = toml::from_str::<toml::Value>(&text)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()));
    let schema = document
        .get("schema")
        .and_then(toml::Value::as_str)
        .unwrap_or_default();
    if schema != "agentlibre.builtin-catalog-baseline/v1" {
        panic!(
            "{} schema must be agentlibre.builtin-catalog-baseline/v1",
            path.display()
        );
    }
    let entries = document
        .get("packages")
        .and_then(toml::Value::as_array)
        .unwrap_or_else(|| panic!("{} must contain [[packages]] entries", path.display()));
    let mut baseline = std::collections::BTreeMap::new();
    for entry in entries {
        let table = entry
            .as_table()
            .unwrap_or_else(|| panic!("{} package entry must be a table", path.display()));
        let reference = table
            .get("reference")
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| panic!("{} package entry has no reference", path.display()));
        let digest = table
            .get("digest")
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| panic!("{} package {reference} has no digest", path.display()));
        if baseline
            .insert(reference.to_string(), digest.to_string())
            .is_some()
        {
            panic!("{} contains duplicate package {reference}", path.display());
        }
    }

    let expected = packages
        .iter()
        .map(|package| (exact_reference(package), package.digest.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    if baseline != expected {
        panic!(
            "immutable builtin catalog baseline differs from embedded packages; change an envelope version for every changed payload and update {} intentionally\n{}",
            path.display(),
            render_builtin_catalog_baseline(&expected)
        );
    }
}

fn render_builtin_catalog_baseline(
    packages: &std::collections::BTreeMap<String, String>,
) -> String {
    let mut output = String::from("schema = \"agentlibre.builtin-catalog-baseline/v1\"\n");
    for (reference, digest) in packages {
        output.push_str(&format!(
            "\n[[packages]]\nreference = {reference:?}\ndigest = {digest:?}\n"
        ));
    }
    output
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

fn validate_extension_name(value: &str) {
    if value.is_empty()
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        panic!(
            "builtin extension directory must be lowercase ASCII, digits, hyphens, underscores, or dots: {value}"
        );
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

fn validate_unique_package_ids(packages: &[Package]) {
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

fn write_registry(assets: &[Asset], packages: &[Package]) {
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
        output.push_str(&format!("static PACKAGE_{index}_REQUIRES: &[&str] = &[\n"));
        for requirement in &package.requires {
            output.push_str(&format!("    {},\n", rust_string(requirement)));
        }
        output.push_str("];\n");
    }

    output.push_str("pub static BUILTIN_PACKAGES: &[BuiltinPackage] = &[\n");
    for (index, package) in packages.iter().enumerate() {
        output.push_str(&format!(
            "    BuiltinPackage {{ type_id: {}, id: {}, version: {}, entrypoint: {}, requires: PACKAGE_{index}_REQUIRES, files: PACKAGE_{index}_FILES, digest: {} }},\n",
            rust_string(&package.type_id),
            rust_string(&package.id),
            rust_string(&package.version),
            rust_string(&package.entrypoint),
            rust_string(&package.digest),
        ));
    }
    output.push_str("];\n");
    output.push_str(&format!(
        "pub const BUILTIN_PACKAGE_CATALOG_DIGEST: &str = {};\n",
        rust_string(&builtin_catalog_digest(packages))
    ));

    fs::write(&destination, output)
        .unwrap_or_else(|err| panic!("failed to write {}: {err}", destination.display()));
}

fn rust_string(value: &str) -> String {
    format!("{value:?}")
}
