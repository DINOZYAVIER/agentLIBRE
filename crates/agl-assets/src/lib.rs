#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltinAssetKind {
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
pub struct BuiltinSkill {
    pub id: &'static str,
    pub pack: &'static str,
    pub skill_md: &'static BuiltinAsset,
    pub references: &'static [&'static BuiltinAsset],
    pub assets: &'static [&'static BuiltinAsset],
    pub tree_sha256: &'static str,
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
    pub files: &'static [BuiltinArtifactFile],
    pub digest: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/builtin_assets.rs"));

pub fn builtin_asset(id: &str) -> Option<&'static BuiltinAsset> {
    BUILTIN_ASSETS.iter().copied().find(|asset| asset.id == id)
}

pub fn builtin_skill(id: &str) -> Option<&'static BuiltinSkill> {
    BUILTIN_SKILLS.iter().find(|skill| skill.id == id)
}

pub fn builtin_artifact_package(id: &str) -> Option<&'static BuiltinArtifactPackage> {
    BUILTIN_ARTIFACT_PACKAGES
        .iter()
        .find(|package| package.id == id)
}

pub fn builtin_skills_by_pack(pack: &str) -> impl Iterator<Item = &'static BuiltinSkill> + '_ {
    BUILTIN_SKILLS
        .iter()
        .filter(move |skill| skill.pack == pack)
}

pub fn default_system_prompt() -> &'static BuiltinAsset {
    builtin_asset("builtin:default").expect("builtin:default prompt must be embedded")
}

pub fn default_system_prompt_text() -> &'static str {
    default_system_prompt()
        .text()
        .expect("builtin:default prompt must be valid UTF-8")
}

pub fn model_catalog() -> &'static BuiltinAsset {
    builtin_asset("builtin:model-catalog").expect("builtin:model-catalog must be embedded")
}

pub fn model_catalog_text() -> &'static str {
    model_catalog()
        .text()
        .expect("builtin:model-catalog must be valid UTF-8")
}

pub fn model_benchmark_evidence(id: &str) -> Option<&'static BuiltinAsset> {
    builtin_asset(id).filter(|asset| asset.kind == BuiltinAssetKind::ModelBenchmarkEvidence)
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
    fn model_catalog_is_embedded() {
        let catalog = model_catalog();
        assert_eq!(catalog.kind, BuiltinAssetKind::ModelCatalog);
        assert_eq!(catalog.source_path, "assets/models/catalog.toml");
        assert!(model_catalog_text().contains("gemma4-e4b"));
    }

    #[test]
    fn model_benchmark_evidence_is_embedded() {
        let evidence = model_benchmark_evidence("model-benchmark:20260715-gemma4-qat-cpu")
            .expect("CPU benchmark evidence must be embedded");
        assert_eq!(evidence.kind, BuiltinAssetKind::ModelBenchmarkEvidence);
        assert!(evidence.text().unwrap().contains("MemorySwapMax=0"));
        let vulkan = model_benchmark_evidence("model-benchmark:20260715-gemma4-qat-vulkan")
            .expect("Vulkan benchmark evidence must be embedded");
        assert!(vulkan.text().unwrap().contains("Exact GPU layers"));
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
    fn skill_tree_hashes_are_present() {
        for skill in BUILTIN_SKILLS {
            assert_eq!(skill.tree_sha256.len(), 64);
            assert_eq!(skill.skill_md.kind, BuiltinAssetKind::Skill);
            assert_eq!(skill.skill_md.id, skill.id);
        }
    }

    #[test]
    fn builtin_functions_are_embedded_from_assets() {
        let functions = BUILTIN_ARTIFACT_PACKAGES
            .iter()
            .map(|function| function.id)
            .collect::<Vec<_>>();

        assert_eq!(
            functions,
            vec![
                "gemma4-12b",
                "gemma4-26b",
                "gemma4-31b",
                "gemma4-e2b",
                "gemma4-e4b"
            ]
        );
        for function in BUILTIN_ARTIFACT_PACKAGES {
            assert_eq!(function.type_id, "function");
            assert_eq!(function.version, "1.0.0");
            assert_eq!(function.entrypoint, "FUNCTION.md");
            assert_eq!(function.digest.len(), 64);
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
        for function in BUILTIN_ARTIFACT_PACKAGES {
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
    fn gemma4_e2b_and_12b_presets_match_builtin_policy() {
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
    }

    #[test]
    fn builtin_skills_are_embedded_from_core_repo_checkout() {
        let skills = BUILTIN_SKILLS
            .iter()
            .map(|skill| skill.id)
            .collect::<Vec<_>>();

        assert_eq!(skills, vec!["process", "repo-status", "skill"]);
        for skill in BUILTIN_SKILLS {
            assert!(
                skill
                    .skill_md
                    .source_path
                    .starts_with("assets/core-skills/"),
                "{} must be embedded from assets/core-skills, got {}",
                skill.id,
                skill.skill_md.source_path
            );
        }
    }

    #[test]
    fn lookup_helpers_return_none_for_missing_ids() {
        assert!(builtin_asset("missing:asset").is_none());
        assert!(builtin_skill("missing").is_none());
        assert!(builtin_artifact_package("missing").is_none());
        assert_eq!(builtin_skills_by_pack("missing").count(), 0);
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
