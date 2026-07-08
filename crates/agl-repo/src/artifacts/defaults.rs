use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::{
    ArtifactAccess, ArtifactConflictPolicy, ArtifactContract, ArtifactCreateRule, ArtifactKind,
    ArtifactSource, ArtifactSourceKind, ArtifactSourceRole,
};

pub(crate) fn default_artifact_sources() -> BTreeMap<String, ArtifactSource> {
    BTreeMap::from([
        (
            "skills".to_string(),
            default_single_artifact_source("skills", ArtifactSourceKind::Submodule),
        ),
        (
            "tasks".to_string(),
            default_single_artifact_source("tasks", ArtifactSourceKind::Local),
        ),
        (
            "reviews".to_string(),
            default_single_artifact_source("reviews", ArtifactSourceKind::Generated),
        ),
        (
            "decision-docs".to_string(),
            default_single_artifact_source("decision-docs", ArtifactSourceKind::Generated),
        ),
        (
            "smoke".to_string(),
            default_single_artifact_source("smoke", ArtifactSourceKind::Generated),
        ),
        (
            "handoffs".to_string(),
            default_single_artifact_source("handoffs", ArtifactSourceKind::Local),
        ),
        (
            "state".to_string(),
            default_single_artifact_source("state", ArtifactSourceKind::Ignored),
        ),
    ])
}

pub(crate) fn source_backed_artifact_source(
    id: &str,
    path: PathBuf,
    kind: ArtifactSourceKind,
    url: Option<String>,
    rev: Option<String>,
) -> ArtifactSource {
    let mut source = default_single_artifact_source_with_path(id, path, kind);
    source.url = url;
    source.rev = rev;
    source.required = true;
    if let Some(contract) = source.artifacts.first_mut() {
        contract.required = true;
    }
    source
}

fn default_single_artifact_source(id: &str, kind: ArtifactSourceKind) -> ArtifactSource {
    default_single_artifact_source_with_path(id, default_artifact_path(id), kind)
}

fn default_single_artifact_source_with_path(
    id: &str,
    path: PathBuf,
    kind: ArtifactSourceKind,
) -> ArtifactSource {
    ArtifactSource {
        role: default_artifact_source_role(id),
        kind,
        path: path.clone(),
        url: default_artifact_source_url(id),
        rev: default_artifact_source_rev(id),
        commit: None,
        tree: None,
        required: default_artifact_source_required(id),
        provides: default_artifact_provides(id),
        artifacts: vec![default_artifact_contract(id, path)],
    }
}

fn default_artifact_source_role(id: &str) -> ArtifactSourceRole {
    match id {
        "skills" => ArtifactSourceRole::Core,
        "reviews" | "decision-docs" | "smoke" => ArtifactSourceRole::Generated,
        "state" => ArtifactSourceRole::State,
        "tasks" | "handoffs" => ArtifactSourceRole::Planning,
        _ => ArtifactSourceRole::Local,
    }
}

fn default_artifact_source_url(id: &str) -> Option<String> {
    match id {
        "skills" => Some(crate::DEFAULT_SKILLS_URL.to_string()),
        _ => None,
    }
}

fn default_artifact_source_rev(id: &str) -> Option<String> {
    match id {
        "skills" => Some(crate::DEFAULT_SKILLS_REV.to_string()),
        _ => None,
    }
}

fn default_artifact_source_required(id: &str) -> bool {
    matches!(id, "tasks" | "reviews" | "state")
}

fn default_artifact_path(id: &str) -> PathBuf {
    PathBuf::from(format!(".agl/{id}"))
}

fn default_artifact_provides(id: &str) -> Vec<String> {
    match id {
        "skills" => vec!["skills".to_string(), "core-skills".to_string()],
        "tasks" => vec!["tasks".to_string(), "specs".to_string()],
        "reviews" => vec!["review-packs".to_string()],
        "decision-docs" => vec!["decision-docs".to_string(), "decisions".to_string()],
        "smoke" => vec!["smoke-artifacts".to_string(), "smoke-suites".to_string()],
        "handoffs" => vec!["handoffs".to_string()],
        "state" => vec![
            "local-state".to_string(),
            "notes".to_string(),
            "memory".to_string(),
            "matrix".to_string(),
            "cron".to_string(),
            "sessions".to_string(),
            "logs".to_string(),
            "cache".to_string(),
        ],
        other => vec![other.to_string()],
    }
}

fn default_artifact_contract(id: &str, path: PathBuf) -> ArtifactContract {
    ArtifactContract {
        id: id.to_string(),
        kind: default_artifact_kind(id),
        path,
        access: default_artifact_access(id),
        provides: default_artifact_provides(id),
        schema: default_artifact_schema(id),
        create: default_artifact_create_rules(id),
        required: default_artifact_contract_required(id),
        shared: true,
        conflict_policy: ArtifactConflictPolicy::Identical,
    }
}

fn default_artifact_kind(id: &str) -> ArtifactKind {
    match id {
        "reviews" | "decision-docs" | "smoke" => ArtifactKind::Generated,
        "state" => ArtifactKind::State,
        _ => ArtifactKind::Source,
    }
}

fn default_artifact_access(id: &str) -> ArtifactAccess {
    match id {
        "skills" => ArtifactAccess::Read,
        _ => ArtifactAccess::ReadWrite,
    }
}

fn default_artifact_schema(id: &str) -> Option<String> {
    match id {
        "skills" => Some("agl.skill_source.v1".to_string()),
        "tasks" => Some("agl.task_spec.v1".to_string()),
        "reviews" => Some("agl.review_pack.v1".to_string()),
        "decision-docs" => Some("agl.decision_doc.v1".to_string()),
        "smoke" => Some("agl.smoke.v1".to_string()),
        "handoffs" => Some("agl.handoff_markdown.v1".to_string()),
        _ => None,
    }
}

fn default_artifact_create_rules(id: &str) -> Vec<ArtifactCreateRule> {
    match id {
        "skills" => Vec::new(),
        _ => vec![ArtifactCreateRule {
            dir: PathBuf::from("."),
        }],
    }
}

fn default_artifact_contract_required(id: &str) -> bool {
    matches!(id, "tasks" | "reviews")
}
