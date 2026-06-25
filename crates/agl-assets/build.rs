use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

#[derive(Clone, Copy)]
enum AssetKind {
    SystemPrompt,
    Skill,
    SkillReference,
    SkillAsset,
}

impl AssetKind {
    fn rust_variant(self) -> &'static str {
        match self {
            Self::SystemPrompt => "BuiltinAssetKind::SystemPrompt",
            Self::Skill => "BuiltinAssetKind::Skill",
            Self::SkillReference => "BuiltinAssetKind::SkillReference",
            Self::SkillAsset => "BuiltinAssetKind::SkillAsset",
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

struct Skill {
    id: String,
    pack: String,
    skill_asset_index: usize,
    reference_asset_indices: Vec<usize>,
    asset_indices: Vec<usize>,
    tree_sha256: String,
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
    let mut assets = Vec::new();
    let mut skills = Vec::new();

    println!("cargo:rerun-if-changed={}", assets_root.display());

    add_system_prompt(&mut assets, repo_root, &assets_root);
    add_skills(&mut assets, &mut skills, repo_root, &assets_root);
    validate_unique_asset_ids(&assets);
    validate_unique_skill_ids(&skills);
    write_registry(&assets, &skills);
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
    skills: &mut Vec<Skill>,
    repo_root: &Path,
    assets_root: &Path,
) {
    let skills_root = assets_root.join("skills");
    if !skills_root.exists() {
        return;
    }
    reject_symlink(&skills_root);
    for pack in ["agl", "dev"] {
        let pack_root = skills_root.join(pack);
        if !pack_root.exists() {
            continue;
        }
        reject_symlink(&pack_root);
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

            let tree_sha256 = skill_tree_hash(
                assets,
                skill_asset_index,
                &reference_asset_indices,
                &asset_indices,
            );
            skills.push(Skill {
                id,
                pack: pack.to_string(),
                skill_asset_index,
                reference_asset_indices,
                asset_indices,
                tree_sha256,
            });
        }
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

fn skill_tree_hash(
    assets: &[Asset],
    skill_asset_index: usize,
    reference_asset_indices: &[usize],
    asset_indices: &[usize],
) -> String {
    let mut hasher = Sha256::new();
    for index in std::iter::once(&skill_asset_index)
        .chain(reference_asset_indices.iter())
        .chain(asset_indices.iter())
    {
        let asset = &assets[*index];
        hasher.update(asset.source_path.as_bytes());
        hasher.update([0]);
        hasher.update(asset.sha256.as_bytes());
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

fn validate_unique_skill_ids(skills: &[Skill]) {
    let mut ids = std::collections::BTreeSet::new();
    for skill in skills {
        if !ids.insert(skill.id.as_str()) {
            panic!("duplicate builtin skill id {}", skill.id);
        }
    }
}

fn write_registry(assets: &[Asset], skills: &[Skill]) {
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

    for (index, skill) in skills.iter().enumerate() {
        output.push_str(&format!(
            "static SKILL_{index}_REFERENCES: &[&BuiltinAsset] = &[{}];\n",
            asset_refs(&skill.reference_asset_indices)
        ));
        output.push_str(&format!(
            "static SKILL_{index}_ASSETS: &[&BuiltinAsset] = &[{}];\n",
            asset_refs(&skill.asset_indices)
        ));
    }

    output.push_str("pub static BUILTIN_ASSETS: &[&BuiltinAsset] = &[\n");
    for index in 0..assets.len() {
        output.push_str(&format!("    &ASSET_{index},\n"));
    }
    output.push_str("];\n");

    output.push_str("pub static BUILTIN_SKILLS: &[BuiltinSkill] = &[\n");
    for (index, skill) in skills.iter().enumerate() {
        output.push_str(&format!(
            "    BuiltinSkill {{ id: {}, pack: {}, skill_md: &ASSET_{}, references: SKILL_{}_REFERENCES, assets: SKILL_{}_ASSETS, tree_sha256: {} }},\n",
            rust_string(&skill.id),
            rust_string(&skill.pack),
            skill.skill_asset_index,
            index,
            index,
            rust_string(&skill.tree_sha256),
        ));
    }
    output.push_str("];\n");

    fs::write(&destination, output)
        .unwrap_or_else(|err| panic!("failed to write {}: {err}", destination.display()));
}

fn asset_refs(indices: &[usize]) -> String {
    indices
        .iter()
        .map(|index| format!("&ASSET_{index}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn rust_string(value: &str) -> String {
    format!("{value:?}")
}
