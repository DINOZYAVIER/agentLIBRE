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
    FunctionInferenceConfig,
    ExtensionManifest,
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
pub struct BuiltinArtifactPackage {
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

pub fn builtin_artifact_package(id: &str) -> Option<&'static BuiltinArtifactPackage> {
    BUILTIN_ARTIFACT_PACKAGES
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
        for skill in BUILTIN_ARTIFACT_PACKAGES
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
        let functions = BUILTIN_ARTIFACT_PACKAGES
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
        for function in BUILTIN_ARTIFACT_PACKAGES
            .iter()
            .filter(|package| package.type_id == "function")
        {
            assert_eq!(function.type_id, "function");
            let expected_version = match function.id {
                "gemma4-31b-32k" | "gemma4-31b-64k" => "1.1.0",
                _ => "1.0.0",
            };
            assert_eq!(function.version, expected_version);
            assert_eq!(function.entrypoint, "FUNCTION.md");
            assert_eq!(function.digest.len(), "sha256:".len() + 64);
            assert!(function.files.iter().any(|file| file.path == "SYSTEM.md"));
            assert!(
                function
                    .files
                    .iter()
                    .any(|file| file.path == "inference.toml")
            );
        }
    }

    #[test]
    fn builtin_function_presets_use_model_ids_only() {
        for function in BUILTIN_ARTIFACT_PACKAGES
            .iter()
            .filter(|package| package.type_id == "function")
        {
            let text = function
                .files
                .iter()
                .find(|file| file.path == "inference.toml")
                .unwrap()
                .bytes;
            let text = std::str::from_utf8(text).unwrap();
            let preset = agl_config::load_inference_preset_from_str(function.id, text).unwrap();
            assert!(!preset.backend.model_id.as_str().is_empty());
            assert!(!text.contains("/home/"));
            assert!(!text.contains(".dyno/models"));
            assert!(!text.contains(".lmstudio/models"));
        }

        let direct_path = r#"
[backend]
kind = "llama_cpp"
model = "/home/user/model.gguf"

[runtime]
gpu_layers = 0
context_tokens = 4096
threads = 2

[model]
dialect = "gemma4"
tool_call_format = "gemma_function_call"
"#;
        assert!(agl_config::load_inference_preset_from_str("direct path", direct_path).is_err());
    }

    #[test]
    fn gemma4_agent_presets_match_builtin_policy() {
        let e2b = builtin_artifact_package("gemma4-e2b").expect("Gemma 4 E2B must be embedded");
        let e2b_text = std::str::from_utf8(
            e2b.files
                .iter()
                .find(|file| file.path == "inference.toml")
                .unwrap()
                .bytes,
        )
        .unwrap();
        let e2b_preset =
            agl_config::load_inference_preset_from_str("gemma4-e2b", e2b_text).unwrap();
        let e2b_runtime = e2b_preset.runtime.auto_policy().unwrap();

        assert_eq!(e2b_preset.backend.model_id.as_str(), "gemma4-e2b");
        assert_eq!(e2b_preset.backend.multimodal_projector_id, None);
        assert_eq!(e2b_runtime.max_context_tokens, 32_768);
        assert_eq!(e2b_runtime.max_batch_size, 512);
        assert_eq!(e2b_runtime.max_ubatch_size, 256);
        assert_eq!(e2b_runtime.flash_attention, agl_config::RuntimeSwitch::On);
        assert_eq!(e2b_runtime.cache_type_k, agl_config::KvCacheType::Q8_0);
        assert_eq!(e2b_runtime.cache_type_v, agl_config::KvCacheType::Q8_0);
        assert!(!e2b_preset.runtime.mtp_enabled());

        let twelve_b =
            builtin_artifact_package("gemma4-12b").expect("Gemma 4 12B must be embedded");
        let twelve_b_preset = agl_config::load_inference_preset_from_str(
            "gemma4-12b",
            std::str::from_utf8(
                twelve_b
                    .files
                    .iter()
                    .find(|file| file.path == "inference.toml")
                    .unwrap()
                    .bytes,
            )
            .unwrap(),
        )
        .unwrap();
        let twelve_b_runtime = twelve_b_preset.runtime.auto_policy().unwrap();

        assert_eq!(twelve_b_runtime.max_context_tokens, 65_536);
        assert_eq!(twelve_b_runtime.max_batch_size, 512);
        assert_eq!(twelve_b_runtime.max_ubatch_size, 256);
        assert!(!twelve_b_preset.runtime.mtp_enabled());

        for (function_id, expected_context) in
            [("gemma4-31b-32k", 32_768), ("gemma4-31b-64k", 65_536)]
        {
            let thirty_one_b = builtin_artifact_package(function_id)
                .unwrap_or_else(|| panic!("{function_id} must be embedded"));
            let preset = agl_config::load_inference_preset_from_str(
                function_id,
                std::str::from_utf8(
                    thirty_one_b
                        .files
                        .iter()
                        .find(|file| file.path == "inference.toml")
                        .unwrap()
                        .bytes,
                )
                .unwrap(),
            )
            .unwrap();
            let runtime = preset.runtime.auto_policy().unwrap();

            assert_eq!(preset.backend.model_id.as_str(), "gemma4-31b");
            assert_eq!(runtime.max_context_tokens, expected_context);
            assert_eq!(runtime.max_batch_size, 512);
            assert_eq!(runtime.max_ubatch_size, 256);
            assert_eq!(
                runtime
                    .device
                    .map(agl_config::AutoRuntimeDevice::runtime_name),
                Some("Vulkan0")
            );
            assert_eq!(runtime.flash_attention, agl_config::RuntimeSwitch::On);
            assert_eq!(runtime.cache_type_k, agl_config::KvCacheType::Q8_0);
            assert_eq!(runtime.cache_type_v, agl_config::KvCacheType::Q8_0);
            assert!(!preset.runtime.mtp_enabled());
        }
        assert!(builtin_artifact_package("gemma4-31b").is_none());
    }

    #[test]
    fn generated_package_digests_match_the_canonical_runtime_algorithm() {
        for package in BUILTIN_ARTIFACT_PACKAGES {
            let view = agl_artifact::InMemoryPackageView::new(package.files.iter().map(|file| {
                (
                    file.path.parse().expect("generated package path is valid"),
                    file.bytes.to_vec(),
                )
            }))
            .expect("generated package paths are unique");
            assert_eq!(
                package.digest,
                agl_artifact::compute_package_digest(&view)
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
            BUILTIN_ARTIFACT_CATALOG_DIGEST,
            "sha256:ca020837f6c2d90cb568cd5bf0d32fc8e17696b73a094af9377982b91f05e9da"
        );
    }

    #[test]
    fn builtin_skills_are_embedded_from_core_repo_checkout() {
        let skills = BUILTIN_ARTIFACT_PACKAGES
            .iter()
            .filter(|package| package.type_id == "skill")
            .map(|skill| skill.id)
            .collect::<Vec<_>>();

        assert_eq!(skills, vec!["process", "repo-status", "skill"]);
        for skill in BUILTIN_ARTIFACT_PACKAGES
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
        assert!(builtin_artifact_package("missing").is_none());
        assert_eq!(
            BUILTIN_ARTIFACT_PACKAGES
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
