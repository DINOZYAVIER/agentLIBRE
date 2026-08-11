#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltinAssetKind {
    SystemPrompt,
    ModelDescriptor,
    ModelEvidence,
    Skill,
    SkillReference,
    SkillAsset,
    FunctionManifest,
    FunctionSystemPrompt,
    ExtensionPackageFile,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltinAsset {
    pub id: &'static str,
    pub kind: BuiltinAssetKind,
    pub source_path: &'static str,
    pub sha256: &'static str,
    pub bytes: &'static [u8],
}

impl BuiltinAsset {
    pub fn text(&self) -> Result<&'static str, std::str::Utf8Error> {
        std::str::from_utf8(self.bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltinArtifactFile {
    pub path: &'static str,
    pub bytes: &'static [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltinPackage {
    pub type_id: &'static str,
    pub id: &'static str,
    pub version: &'static str,
    pub entrypoint: &'static str,
    pub requires: &'static [&'static str],
    pub files: &'static [BuiltinArtifactFile],
    pub digest: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/builtin_assets.rs"));

pub fn builtin_asset(id: &str) -> Option<&'static BuiltinAsset> {
    BUILTIN_ASSETS.iter().copied().find(|asset| asset.id == id)
}

pub fn builtin_package(id: &str) -> Option<&'static BuiltinPackage> {
    BUILTIN_PACKAGES
        .iter()
        .find(|package| package.type_id == "function" && package.id == id)
}

pub fn default_system_prompt() -> &'static BuiltinAsset {
    builtin_asset("builtin:default").expect("builtin:default prompt must be embedded")
}

pub fn default_system_prompt_text() -> &'static str {
    default_system_prompt()
        .text()
        .expect("builtin:default prompt must be valid UTF-8")
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn default_system_prompt_is_embedded() {
        let prompt = default_system_prompt();

        assert_eq!(prompt.id, "builtin:default");
        assert_eq!(prompt.kind, BuiltinAssetKind::SystemPrompt);
        assert_eq!(prompt.source_path, "assets/prompts/system/default.md");
        assert!(prompt.text().unwrap().contains("{{AGL_VERSION}}"));
    }

    #[test]
    fn asset_hashes_match_embedded_bytes() {
        for asset in BUILTIN_ASSETS {
            assert_eq!(asset.sha256, sha256_hex(asset.bytes), "{}", asset.id);
            assert_eq!(asset.sha256.len(), 64);
        }
    }

    #[test]
    fn asset_ids_are_unique() {
        let mut ids = std::collections::BTreeSet::new();
        for asset in BUILTIN_ASSETS {
            assert!(ids.insert(asset.id), "duplicate asset id {}", asset.id);
        }
    }

    #[test]
    fn skill_package_digests_are_present() {
        for skill in BUILTIN_PACKAGES
            .iter()
            .filter(|package| package.type_id == "skill")
        {
            assert_eq!(skill.digest.len(), "sha256:".len() + 64);
            assert_eq!(skill.entrypoint, "SKILL.md");
            assert!(skill.files.iter().any(|file| file.path == "SKILL.md"));
        }
    }

    #[test]
    fn builtin_functions_are_embedded_from_assets() {
        let functions = BUILTIN_PACKAGES
            .iter()
            .filter(|package| package.type_id == "function")
            .map(|function| function.id)
            .collect::<Vec<_>>();

        assert_eq!(
            functions,
            vec![
                "gemma4-12b",
                "gemma4-26b",
                "gemma4-31b-32k",
                "gemma4-31b-64k",
                "gemma4-e2b",
                "gemma4-e4b"
            ]
        );
        for function in BUILTIN_PACKAGES
            .iter()
            .filter(|package| package.type_id == "function")
        {
            assert_eq!(function.type_id, "function");
            let expected_version = match function.id {
                "gemma4-31b-32k" | "gemma4-31b-64k" => "1.3.0",
                _ => "1.2.0",
            };
            assert_eq!(function.version, expected_version);
            assert_eq!(function.entrypoint, "FUNCTION.md");
            assert_eq!(function.digest.len(), "sha256:".len() + 64);
            assert!(function.files.iter().any(|file| file.path == "SYSTEM.md"));
            assert!(
                !function
                    .files
                    .iter()
                    .any(|file| file.path == "inference.toml")
            );
        }
    }

    #[test]
    fn builtin_functions_bind_a_package_model_profile() {
        for function in BUILTIN_PACKAGES
            .iter()
            .filter(|package| package.type_id == "function")
        {
            let text = function
                .files
                .iter()
                .find(|file| file.path == "FUNCTION.md")
                .unwrap()
                .bytes;
            let text = std::str::from_utf8(text).unwrap();
            assert!(text.contains("payload_schema: agentlibre.function/v3"));
            assert!(text.contains("  profile: "));
            assert!(!text.contains("  config: "));
            assert!(!text.contains("inference.toml"));
            assert!(!text.contains("/home/"));
            assert!(!text.contains(".dyno/models"));
            assert!(!text.contains(".lmstudio/models"));
        }
    }

    #[test]
    fn gemma4_functions_select_the_exact_builtin_profile() {
        for (function_id, expected_profile) in [
            ("gemma4-12b", "gpu-rx7900xtx-65536"),
            ("gemma4-26b", "gpu-rx7900xtx-32768"),
            ("gemma4-31b-32k", "gpu-rx7900xtx-32768"),
            ("gemma4-31b-64k", "gpu-rx7900xtx-65536"),
            ("gemma4-e2b", "gpu-rx7900xtx-32768"),
            ("gemma4-e4b", "gpu-rx7900xtx-32768"),
        ] {
            let package = builtin_package(function_id)
                .unwrap_or_else(|| panic!("{function_id} must be embedded"));
            let manifest = std::str::from_utf8(
                package
                    .files
                    .iter()
                    .find(|file| file.path == "FUNCTION.md")
                    .unwrap()
                    .bytes,
            )
            .unwrap();
            assert!(manifest.contains(&format!("  profile: {expected_profile}")));
        }
        assert!(builtin_package("gemma4-31b").is_none());
    }

    #[test]
    fn generated_package_digests_match_the_canonical_runtime_algorithm() {
        for package in BUILTIN_PACKAGES {
            let view = agl_package::InMemoryPackageView::new(package.files.iter().map(|file| {
                (
                    file.path.parse().expect("generated package path is valid"),
                    file.bytes.to_vec(),
                )
            }))
            .expect("generated package paths are unique");
            assert_eq!(
                package.digest,
                agl_package::compute_package_digest(&view)
                    .expect("generated package is canonical")
                    .as_str(),
                "{}:{}@{}",
                package.type_id,
                package.id,
                package.version
            );
        }
    }

    #[test]
    fn builtin_catalog_identity_is_canonical_sha256() {
        assert_eq!(
            BUILTIN_PACKAGE_CATALOG_DIGEST,
            "sha256:bcd78ff80570da84c27d70c3c1e6f49b152fd188f7752db114510f50dc2724a2"
        );
    }

    #[test]
    fn builtin_skills_are_embedded_from_core_repo_checkout() {
        let skills = BUILTIN_PACKAGES
            .iter()
            .filter(|package| package.type_id == "skill")
            .map(|skill| skill.id)
            .collect::<Vec<_>>();

        assert_eq!(skills, vec!["process", "repo-status", "skill"]);
        for skill in BUILTIN_PACKAGES
            .iter()
            .filter(|package| package.type_id == "skill")
        {
            assert!(
                skill.files.iter().any(|file| file.path == "SKILL.md"),
                "{} must include SKILL.md",
                skill.id
            );
        }
    }

    #[test]
    fn lookup_helpers_return_none_for_missing_ids() {
        assert!(builtin_asset("missing:asset").is_none());
        assert!(builtin_package("missing").is_none());
        assert_eq!(
            BUILTIN_PACKAGES
                .iter()
                .filter(|package| package.type_id == "skill" && package.id == "missing")
                .count(),
            0
        );
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        let mut out = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
        }
        out
    }
}
