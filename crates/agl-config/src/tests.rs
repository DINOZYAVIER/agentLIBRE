use std::collections::BTreeMap;

use super::*;

#[test]
fn model_bindings_reject_obsolete_or_blank_state() {
    let model = ModelId::new("gemma4-12b").unwrap();
    let blank = ModelBindings {
        version: 1,
        models: BTreeMap::from([(model.clone(), ModelBinding { path: " ".into() })]),
    };
    assert!(blank.validate().is_err());
    let obsolete = ModelBindings {
        version: 0,
        models: BTreeMap::new(),
    };
    assert!(obsolete.validate().is_err());
}

#[test]
fn model_ids_and_bindings_round_trip_strictly() {
    let id = ModelId::new("gemma4-12b.Q8_0").unwrap();
    let bindings = ModelBindings {
        version: 1,
        models: BTreeMap::from([(
            id.clone(),
            ModelBinding {
                path: "/models/gemma.gguf".into(),
            },
        )]),
    };
    let encoded = toml::to_string(&bindings).unwrap();
    let decoded: ModelBindings = toml::from_str(&encoded).unwrap();
    assert_eq!(decoded, bindings);
    assert_eq!(id.as_str(), "gemma4-12b.Q8_0");
    assert!(ModelId::new("bad model").is_err());
}

#[test]
fn model_dialect_and_tool_format_are_one_checked_pair() {
    ModelConfig {
        dialect: ModelDialect::Gemma4,
        tool_call_format: ToolCallFormat::GemmaFunctionCall,
    }
    .validate()
    .unwrap();
    assert!(
        ModelConfig {
            dialect: ModelDialect::Gemma4,
            tool_call_format: ToolCallFormat::HermesJson,
        }
        .validate()
        .is_err()
    );
}

#[test]
fn prompt_skill_ids_are_bounded_to_the_canonical_shape() {
    PromptConfig {
        system: SystemPrompt::BuiltinDefault,
        skills: vec!["core:repo-status".to_owned()],
    }
    .validate()
    .unwrap();
    assert!(
        PromptConfig {
            system: SystemPrompt::None,
            skills: vec!["Bad Skill".to_owned()],
        }
        .validate()
        .is_err()
    );
}

#[test]
fn kv_cache_type_rejects_unknown_values() {
    #[derive(serde::Deserialize)]
    struct Wrapper {
        cache: KvCacheType,
    }
    assert_eq!(
        toml::from_str::<Wrapper>("cache = \"q8_0\"").unwrap().cache,
        KvCacheType::Q8_0
    );
    assert!(toml::from_str::<Wrapper>("cache = \"auto\"").is_err());
}
