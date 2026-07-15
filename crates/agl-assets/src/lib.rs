#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltinAssetKind {
    SystemPrompt,
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
pub struct BuiltinFunction {
    pub id: &'static str,
    pub function_md: &'static BuiltinAsset,
    pub system_prompt: &'static BuiltinAsset,
    pub inference_config: &'static BuiltinAsset,
    pub tree_sha256: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/builtin_assets.rs"));

pub fn builtin_asset(id: &str) -> Option<&'static BuiltinAsset> {
    BUILTIN_ASSETS.iter().copied().find(|asset| asset.id == id)
}

pub fn builtin_skill(id: &str) -> Option<&'static BuiltinSkill> {
    BUILTIN_SKILLS.iter().find(|skill| skill.id == id)
}

pub fn builtin_function(id: &str) -> Option<&'static BuiltinFunction> {
    BUILTIN_FUNCTIONS.iter().find(|function| function.id == id)
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
    fn skill_tree_hashes_are_present() {
        for skill in BUILTIN_SKILLS {
            assert_eq!(skill.tree_sha256.len(), 64);
            assert_eq!(skill.skill_md.kind, BuiltinAssetKind::Skill);
            assert_eq!(skill.skill_md.id, skill.id);
        }
    }

    #[test]
    fn builtin_functions_are_embedded_from_assets() {
        let functions = BUILTIN_FUNCTIONS
            .iter()
            .map(|function| function.id)
            .collect::<Vec<_>>();

        assert_eq!(functions, vec!["gemma4-12b", "gemma4-26b", "gemma4-31b"]);
        for function in BUILTIN_FUNCTIONS {
            assert_eq!(function.tree_sha256.len(), 64);
            assert_eq!(
                function.function_md.kind,
                BuiltinAssetKind::FunctionManifest
            );
            assert_eq!(
                function.system_prompt.kind,
                BuiltinAssetKind::FunctionSystemPrompt
            );
            assert_eq!(
                function.inference_config.kind,
                BuiltinAssetKind::FunctionInferenceConfig
            );
            assert!(
                function
                    .function_md
                    .source_path
                    .starts_with("assets/functions/"),
                "{} must be embedded from assets/functions, got {}",
                function.id,
                function.function_md.source_path
            );
        }
    }

    #[test]
    fn builtin_function_presets_use_model_ids_only() {
        for function in BUILTIN_FUNCTIONS {
            let text = function.inference_config.text().unwrap();
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
    fn builtin_skills_are_embedded_from_core_repo_checkout() {
        let skills = BUILTIN_SKILLS
            .iter()
            .map(|skill| skill.id)
            .collect::<Vec<_>>();

        assert_eq!(skills, vec!["repo-status", "skill"]);
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
        assert!(builtin_function("missing").is_none());
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
