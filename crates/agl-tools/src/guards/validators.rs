use std::collections::BTreeSet;

use agl_capabilities::{HookId, HookInput, HookMessage, HookResult, HookStatus};

pub(crate) fn validate_json(input: HookInput) -> HookResult {
    let Some(text) = payload_text(&input.payload) else {
        return fail(
            input.hook_id,
            "missing_json_text",
            "json.validate requires a string payload field named text, content, json, or artifact",
            None,
        );
    };
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(_) => pass(input.hook_id),
        Err(err) => fail(
            input.hook_id,
            "invalid_json",
            "payload is not valid JSON",
            Some(err.to_string()),
        ),
    }
}

pub(crate) fn validate_repo_path(input: HookInput) -> HookResult {
    let paths = payload_paths(&input.payload);

    let invalid = paths
        .iter()
        .filter_map(|path| {
            validate_single_repo_path(path)
                .err()
                .map(|reason| (path, reason))
        })
        .collect::<Vec<_>>();
    if invalid.is_empty() {
        pass(input.hook_id)
    } else {
        let details = invalid
            .into_iter()
            .map(|(path, reason)| format!("{path}: {reason}"))
            .collect::<Vec<_>>()
            .join("; ");
        fail(
            input.hook_id,
            "invalid_repo_path",
            "one or more repository paths are invalid",
            Some(details),
        )
    }
}

pub(crate) fn validate_task_spec(input: HookInput) -> HookResult {
    let Some(markdown) = payload_text(&input.payload) else {
        return fail(
            input.hook_id,
            "missing_task_spec_text",
            "task_spec.validate requires a string payload field named text, content, markdown, or artifact",
            None,
        );
    };
    let validation = agl_repo::validate_task_spec_markdown(markdown);
    if validation.is_valid() {
        pass(input.hook_id)
    } else {
        fail(
            input.hook_id,
            "task_spec_missing_sections",
            "task spec is missing required sections",
            Some(validation.missing_sections.join(", ")),
        )
    }
}

pub(crate) fn validate_secret_scan(input: HookInput) -> HookResult {
    let Some(text) = payload_text(&input.payload) else {
        return pass(input.hook_id);
    };
    let lower = text.to_ascii_lowercase();
    let findings = [
        (
            lower.contains("-----begin ") && lower.contains(" private key-----"),
            "private key material",
        ),
        (
            contains_token_prefix(text, "github_pat_", 20),
            "GitHub token",
        ),
        (contains_token_prefix(text, "ghp_", 20), "GitHub token"),
        (contains_token_prefix(text, "glpat-", 20), "GitLab token"),
        (contains_token_prefix(text, "xoxb-", 20), "Slack token"),
        (contains_token_prefix(text, "xoxp-", 20), "Slack token"),
        (contains_token_prefix(text, "sk-", 24), "API key"),
        (
            contains_token_prefix(text, "syt_", 20),
            "Matrix access token",
        ),
        (contains_token_prefix(text, "AKIA", 12), "AWS access key id"),
    ];
    let found = findings
        .into_iter()
        .filter_map(|(found, label)| found.then_some(label))
        .collect::<Vec<_>>();
    if found.is_empty() {
        pass(input.hook_id)
    } else {
        fail(
            input.hook_id,
            "secret_scan_findings",
            "artifact appears to contain secret material",
            Some(found.join(", ")),
        )
    }
}

pub(crate) fn validate_diff_scope(input: HookInput) -> HookResult {
    let Some(text) = payload_text(&input.payload) else {
        return pass(input.hook_id);
    };
    let paths = extract_diff_paths(text);
    let blocked = paths
        .iter()
        .filter(|path| is_blocked_diff_path(path))
        .cloned()
        .collect::<Vec<_>>();
    if blocked.is_empty() {
        pass(input.hook_id)
    } else {
        fail(
            input.hook_id,
            "diff_scope_blocked_paths",
            "diff includes paths that should not be part of a source change",
            Some(blocked.join(", ")),
        )
    }
}

pub(crate) fn validate_verification(input: HookInput) -> HookResult {
    let Some(text) = payload_text(&input.payload) else {
        return fail(
            input.hook_id,
            "missing_verification_text",
            "verification.validate requires text, content, markdown, or artifact payload",
            None,
        );
    };
    let lower = text.to_ascii_lowercase();
    let has_evidence = [
        "verification",
        "verified",
        "tests",
        "tested",
        "cargo test",
        "cargo check",
        "not run",
        "not executed",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if has_evidence {
        pass(input.hook_id)
    } else {
        fail(
            input.hook_id,
            "verification_missing",
            "artifact must state what verification was run or why it was not run",
            None,
        )
    }
}

pub(crate) fn validate_commit_message(input: HookInput) -> HookResult {
    let Some(text) = payload_text(&input.payload) else {
        return pass(input.hook_id);
    };
    let mut findings = Vec::new();
    for line in text.lines().map(str::trim) {
        let lower = line.to_ascii_lowercase();
        if (lower.starts_with("signed-off-by:") || lower.starts_with("co-authored-by:"))
            && mentions_llm_agent(&lower)
        {
            findings.push(format!("LLM attestation trailer is not allowed: {line}"));
        }
        if lower.starts_with("assisted-by:") && !assisted_by_has_agent_and_model(line) {
            findings.push(format!("Assisted-by trailer is incomplete: {line}"));
        }
    }
    if findings.is_empty() {
        pass(input.hook_id)
    } else {
        fail(
            input.hook_id,
            "commit_message_invalid_trailers",
            "commit message contains invalid LLM-assistance trailers",
            Some(findings.join("; ")),
        )
    }
}

pub(crate) fn validate_skill_manifest(input: HookInput) -> HookResult {
    let Some(text) = payload_text(&input.payload) else {
        return pass(input.hook_id);
    };
    let Some(frontmatter) = frontmatter_block(text) else {
        return pass(input.hook_id);
    };
    let required_fields = [
        "name:",
        "description:",
        "version:",
        "source:",
        "pack:",
        "required_hooks:",
        "allowed_tools:",
        "context_budget_tokens:",
        "references:",
        "guarantees:",
    ];
    let missing = required_fields
        .iter()
        .filter(|field| {
            !frontmatter
                .lines()
                .any(|line| line.trim_start().starts_with(**field))
        })
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return fail(
            input.hook_id,
            "skill_manifest_missing_fields",
            "skill manifest frontmatter is missing required agentLIBRE fields",
            Some(missing.join(", ")),
        );
    }
    if frontmatter
        .lines()
        .any(|line| line.trim_start().starts_with("scripts:"))
    {
        return fail(
            input.hook_id,
            "skill_manifest_builtin_scripts",
            "builtin skills may not include executable scripts",
            None,
        );
    }
    pass(input.hook_id)
}

pub(crate) fn validate_review_pack(input: HookInput) -> HookResult {
    let Some(text) = payload_text(&input.payload) else {
        return pass(input.hook_id);
    };
    if !looks_like_review_pack_output(text) {
        return pass(input.hook_id);
    }
    let required = [
        "review-manifest.json",
        "payload.json",
        "pr.html",
        "index.html",
    ];
    let missing = required
        .iter()
        .filter(|name| !text.contains(**name))
        .copied()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        pass(input.hook_id)
    } else {
        fail(
            input.hook_id,
            "review_pack_missing_artifacts",
            "review pack output is missing expected generated artifacts",
            Some(missing.join(", ")),
        )
    }
}

pub(crate) fn validate_runtime_identity(input: HookInput, require_hook: bool) -> HookResult {
    let Some(text) = payload_text(&input.payload) else {
        return fail(
            input.hook_id,
            "runtime_identity_missing_text",
            "runtime identity validation requires text, content, markdown, or artifact payload",
            None,
        );
    };
    let Some(identity) = input.payload.get("runtime_identity") else {
        return fail(
            input.hook_id,
            "runtime_identity_unavailable",
            "runtime identity evidence is unavailable",
            None,
        );
    };

    let validation = input.payload.get("runtime_identity_validation");
    let fields = runtime_identity_validation_fields(validation);
    let require_every_field = require_hook || runtime_identity_validation_required(validation);
    let repair = runtime_identity_validation_repair_enabled(validation);
    let claims = extract_identity_claims(text);
    let mut messages = Vec::new();
    let mut unavailable = false;

    for field in &fields {
        match field.as_str() {
            "function" | "model_profile" => {
                let expected = expected_identity_scalar(identity, field);
                let claim = claims.scalar(field);
                collect_scalar_identity_messages(
                    field,
                    expected,
                    claim,
                    require_every_field,
                    &mut unavailable,
                    &mut messages,
                );
            }
            "skills" | "subagents" => {
                let expected = expected_identity_list(identity, field);
                let claim = claims.list(field);
                collect_list_identity_messages(
                    field,
                    expected,
                    claim,
                    require_every_field,
                    &mut unavailable,
                    &mut messages,
                );
            }
            _ => messages.push(HookMessage {
                code: "runtime_identity_unknown_field".to_string(),
                message: format!("runtime identity field `{field}` is not supported"),
                fix: None,
            }),
        }
    }

    if messages.is_empty() {
        pass(input.hook_id)
    } else {
        let status = if repair && !unavailable {
            HookStatus::Repair
        } else {
            HookStatus::Fail
        };
        let fix = expected_identity_fix(identity, &fields);
        for message in &mut messages {
            if message.fix.is_none() {
                message.fix = Some(fix.clone());
            }
        }
        HookResult {
            hook_id: input.hook_id,
            status,
            messages,
        }
    }
}

#[derive(Default)]
struct IdentityClaims {
    function: Option<String>,
    skills: Option<Vec<String>>,
    subagents: Option<Vec<String>>,
    model_profile: Option<String>,
}

impl IdentityClaims {
    fn scalar(&self, field: &str) -> Option<&str> {
        match field {
            "function" => self.function.as_deref(),
            "model_profile" => self.model_profile.as_deref(),
            _ => None,
        }
    }

    fn list(&self, field: &str) -> Option<&[String]> {
        match field {
            "skills" => self.skills.as_deref(),
            "subagents" => self.subagents.as_deref(),
            _ => None,
        }
    }
}

fn extract_identity_claims(text: &str) -> IdentityClaims {
    let mut claims = IdentityClaims::default();
    for segment in text.split(['\n', ';']) {
        let Some((raw_key, raw_value)) = split_claim(segment) else {
            continue;
        };
        let Some(field) = normalize_identity_claim_key(raw_key) else {
            continue;
        };
        match field {
            "function" => claims.function = clean_identity_scalar(raw_value),
            "skills" => claims.skills = Some(clean_identity_list(raw_value)),
            "subagents" => claims.subagents = Some(clean_identity_list(raw_value)),
            "model_profile" => claims.model_profile = clean_identity_scalar(raw_value),
            _ => {}
        }
    }
    claims
}

fn split_claim(segment: &str) -> Option<(&str, &str)> {
    segment
        .split_once('=')
        .or_else(|| segment.split_once(':'))
        .filter(|(_, value)| !value.trim().is_empty())
}

fn normalize_identity_claim_key(raw: &str) -> Option<&'static str> {
    let trimmed = raw
        .trim()
        .trim_start_matches(|ch: char| {
            ch.is_ascii_digit() || matches!(ch, '.' | ')' | '-' | '*' | ' ')
        })
        .trim_matches(|ch| matches!(ch, '`' | '*' | '_' | ' '));
    let lower = trimmed.to_ascii_lowercase().replace('_', " ");
    let key = lower.strip_suffix(" id").unwrap_or(&lower).trim();
    match key {
        "function" => Some("function"),
        "skill" | "skills" => Some("skills"),
        "subagent" | "subagents" => Some("subagents"),
        "model profile" | "model" | "profile" => Some("model_profile"),
        _ => None,
    }
}

fn clean_identity_scalar(raw: &str) -> Option<String> {
    let value = clean_identity_token(raw);
    (!value.is_empty()).then_some(value)
}

fn clean_identity_list(raw: &str) -> Vec<String> {
    let separators: &[char] = if raw.contains(',') { &[','] } else { &[' '] };
    raw.split(separators)
        .map(clean_identity_token)
        .filter(|value| !value.is_empty() && value != "and" && value != "и")
        .collect()
}

fn clean_identity_token(raw: &str) -> String {
    raw.trim()
        .trim_matches(|ch: char| {
            matches!(
                ch,
                '`' | '"' | '\'' | ':' | '.' | '!' | '?' | '<' | '>' | '[' | ']' | '(' | ')' | ','
            )
        })
        .to_string()
}

fn runtime_identity_validation_fields(validation: Option<&serde_json::Value>) -> Vec<String> {
    let fields = validation
        .and_then(|validation| validation.get("fields"))
        .and_then(serde_json::Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if fields.is_empty() {
        vec![
            "function".to_string(),
            "skills".to_string(),
            "subagents".to_string(),
        ]
    } else {
        fields
    }
}

fn runtime_identity_validation_required(validation: Option<&serde_json::Value>) -> bool {
    validation
        .and_then(|validation| validation.get("required"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn runtime_identity_validation_repair_enabled(validation: Option<&serde_json::Value>) -> bool {
    validation
        .and_then(|validation| validation.get("repair_attempts"))
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|attempts| attempts > 0)
}

fn expected_identity_scalar<'a>(identity: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    match field {
        "function" => identity
            .get("function")
            .and_then(|function| function.get("id"))
            .and_then(serde_json::Value::as_str),
        "model_profile" => identity
            .get("model_profile")
            .and_then(serde_json::Value::as_str),
        _ => None,
    }
}

fn expected_identity_list(identity: &serde_json::Value, field: &str) -> Option<Vec<String>> {
    identity
        .get(field)
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
}

fn collect_scalar_identity_messages(
    field: &str,
    expected: Option<&str>,
    claim: Option<&str>,
    require_every_field: bool,
    unavailable: &mut bool,
    messages: &mut Vec<HookMessage>,
) {
    match (expected, claim) {
        (None, Some(_)) => {
            *unavailable = true;
            messages.push(HookMessage {
                code: "runtime_identity_unavailable".to_string(),
                message: format!("runtime identity field `{field}` is unavailable"),
                fix: None,
            });
        }
        (None, None) if require_every_field => {
            *unavailable = true;
            messages.push(HookMessage {
                code: "runtime_identity_unavailable".to_string(),
                message: format!("runtime identity field `{field}` is unavailable"),
                fix: None,
            });
        }
        (Some(_), None) if require_every_field => messages.push(HookMessage {
            code: "runtime_identity_missing".to_string(),
            message: format!("answer must claim runtime identity field `{field}`"),
            fix: None,
        }),
        (Some(expected), Some(claim)) if expected != claim => messages.push(HookMessage {
            code: "runtime_identity_mismatch".to_string(),
            message: format!(
                "answer claimed `{field}={claim}` but runtime identity has `{field}={expected}`"
            ),
            fix: None,
        }),
        _ => {}
    }
}

fn collect_list_identity_messages(
    field: &str,
    expected: Option<Vec<String>>,
    claim: Option<&[String]>,
    require_every_field: bool,
    unavailable: &mut bool,
    messages: &mut Vec<HookMessage>,
) {
    match (expected, claim) {
        (None, Some(_)) => {
            *unavailable = true;
            messages.push(HookMessage {
                code: "runtime_identity_unavailable".to_string(),
                message: format!("runtime identity field `{field}` is unavailable"),
                fix: None,
            });
        }
        (None, None) if require_every_field => {
            *unavailable = true;
            messages.push(HookMessage {
                code: "runtime_identity_unavailable".to_string(),
                message: format!("runtime identity field `{field}` is unavailable"),
                fix: None,
            });
        }
        (Some(_), None) if require_every_field => messages.push(HookMessage {
            code: "runtime_identity_missing".to_string(),
            message: format!("answer must claim runtime identity field `{field}`"),
            fix: None,
        }),
        (Some(expected), Some(claim)) if string_set(&expected) != string_set(claim) => {
            messages.push(HookMessage {
                code: "runtime_identity_mismatch".to_string(),
                message: format!(
                    "answer claimed `{field}={}` but runtime identity has `{field}={}`",
                    claim.join(","),
                    expected.join(",")
                ),
                fix: None,
            });
        }
        _ => {}
    }
}

fn string_set(values: &[String]) -> BTreeSet<String> {
    values.iter().cloned().collect()
}

fn expected_identity_fix(identity: &serde_json::Value, fields: &[String]) -> String {
    let mut parts = Vec::new();
    for field in fields {
        match field.as_str() {
            "function" => {
                if let Some(value) = expected_identity_scalar(identity, field) {
                    parts.push(format!("function={value}"));
                }
            }
            "skills" | "subagents" => {
                if let Some(values) = expected_identity_list(identity, field) {
                    parts.push(format!("{field}={}", values.join(",")));
                }
            }
            "model_profile" => {
                if let Some(value) = expected_identity_scalar(identity, field) {
                    parts.push(format!("model_profile={value}"));
                }
            }
            _ => {}
        }
    }
    format!("Use {}.", parts.join("; "))
}

fn contains_token_prefix(text: &str, prefix: &str, min_suffix_chars: usize) -> bool {
    let text_bytes = text.as_bytes();
    let prefix_bytes = prefix.as_bytes();
    if prefix_bytes.is_empty() || text_bytes.len() < prefix_bytes.len() + min_suffix_chars {
        return false;
    }
    for index in 0..=text_bytes.len() - prefix_bytes.len() {
        if !text_bytes[index..].starts_with(prefix_bytes) {
            continue;
        }
        let suffix_start = index + prefix_bytes.len();
        let suffix_len = text_bytes[suffix_start..]
            .iter()
            .take_while(|byte| is_token_suffix_byte(**byte))
            .count();
        if suffix_len >= min_suffix_chars {
            return true;
        }
    }
    false
}

fn is_token_suffix_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn extract_diff_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in text.lines().map(str::trim_start) {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            for part in rest.split_whitespace().take(2) {
                push_diff_path(&mut paths, part);
            }
        } else if let Some(path) = line.strip_prefix("+++ ") {
            push_diff_path(&mut paths, path);
        } else if let Some(path) = line.strip_prefix("--- ") {
            push_diff_path(&mut paths, path);
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn push_diff_path(paths: &mut Vec<String>, raw: &str) {
    if let Some(path) = normalize_diff_path(raw) {
        paths.push(path);
    }
}

fn normalize_diff_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim_matches(|ch| matches!(ch, '"' | '`' | '\''));
    if trimmed == "/dev/null" {
        return None;
    }
    trimmed
        .strip_prefix("a/")
        .or_else(|| trimmed.strip_prefix("b/"))
        .map(ToOwned::to_owned)
}

fn is_blocked_diff_path(path: &str) -> bool {
    path == ".DS_Store"
        || path.ends_with("/.DS_Store")
        || path.starts_with(".agl/")
        || path.starts_with(".git/")
        || path.starts_with("target/")
        || path.starts_with("node_modules/")
        || path.contains("/__pycache__/")
        || path.ends_with("/__pycache__")
}

fn mentions_llm_agent(lower: &str) -> bool {
    ["codex", "openai", "gpt", "llm", "assistant"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn assisted_by_has_agent_and_model(line: &str) -> bool {
    let Some((_, value)) = line.split_once(':') else {
        return false;
    };
    value
        .split_whitespace()
        .next()
        .is_some_and(|agent_model| agent_model.contains(':') && !agent_model.ends_with(':'))
}

fn frontmatter_block(text: &str) -> Option<&str> {
    let mut lines = text.lines();
    let first = lines.next()?.trim();
    if first != "---" {
        return None;
    }
    let start = text.find('\n')? + 1;
    let rest = &text[start..];
    let end = rest
        .lines()
        .scan(0usize, |offset, line| {
            let current = *offset;
            *offset += line.len() + 1;
            Some((current, line))
        })
        .find_map(|(offset, line)| (line.trim() == "---").then_some(offset))?;
    Some(&rest[..end])
}

fn looks_like_review_pack_output(text: &str) -> bool {
    [
        ".agl/reviews",
        "review-manifest.json",
        "diff_review.html",
        "implementation_review.html",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn payload_text(payload: &serde_json::Value) -> Option<&str> {
    for field in ["text", "content", "json", "markdown", "artifact"] {
        if let Some(value) = payload.get(field).and_then(serde_json::Value::as_str) {
            return Some(value);
        }
    }
    payload.as_str()
}

fn payload_paths(payload: &serde_json::Value) -> Vec<String> {
    if let Some(path) = payload.get("path").and_then(serde_json::Value::as_str) {
        return vec![path.to_string()];
    }
    if let Some(paths) = payload.get("paths").and_then(serde_json::Value::as_array) {
        return paths
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
    }
    if let Some(content) = payload_text(payload) {
        return extract_markdown_repo_paths(content);
    }
    payload
        .as_str()
        .map(|path| vec![path.to_string()])
        .unwrap_or_default()
}

fn extract_markdown_repo_paths(content: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for candidate in content
        .split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | ')' | '(' | '[' | ']'))
        .map(|candidate| {
            candidate.trim_matches(|ch: char| {
                matches!(ch, '`' | '"' | '\'' | ':' | '.' | '!' | '?' | '<' | '>')
            })
        })
        .filter(|candidate| candidate.contains('/'))
    {
        if looks_like_repo_path(candidate) {
            paths.push(candidate.to_string());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn looks_like_repo_path(candidate: &str) -> bool {
    !is_known_chat_slash_command(candidate)
        && !candidate.contains("://")
        && !candidate.starts_with('#')
        && !candidate.starts_with('@')
        && candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
}

fn is_known_chat_slash_command(candidate: &str) -> bool {
    matches!(
        candidate,
        "/help" | "/session" | "/workspace" | "/reload" | "/clear" | "/exit" | "/quit"
    )
}

fn validate_single_repo_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("empty");
    }
    if path.starts_with('/') {
        return Err("absolute path");
    }
    if path.contains('\\') {
        return Err("backslashes are not accepted");
    }
    if path.contains('\0') {
        return Err("NUL byte");
    }
    if path
        .split('/')
        .any(|segment| segment.is_empty() || matches!(segment, "." | ".." | ".git"))
    {
        return Err("contains empty, dot, parent, or .git segment");
    }
    Ok(())
}

fn pass(hook_id: HookId) -> HookResult {
    HookResult {
        hook_id,
        status: HookStatus::Pass,
        messages: Vec::new(),
    }
}

pub(crate) fn fail(hook_id: HookId, code: &str, message: &str, fix: Option<String>) -> HookResult {
    HookResult {
        hook_id,
        status: HookStatus::Fail,
        messages: vec![HookMessage {
            code: code.to_string(),
            message: message.to_string(),
            fix,
        }],
    }
}
