use serde::{Deserialize, Serialize};

use crate::MessageId;

/// One durable fact extracted from the compaction semantic summary (M1 D2).
///
/// `files` carries workspace-relative paths without digests: the daemon stamps
/// `@sha256-12` at write time (M1 D3). `supersedes` is optional; each reference
/// is `slug#claim-n`, where `n` is the claim ordinal in `## Claims`, not a
/// physical file line (Q1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryClaim {
    pub text: String,
    pub sources: Vec<MessageId>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<Vec<String>>,
}

/// A memory topic as carried in the model payload: one entry file per slug.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryTopic {
    pub slug: String,
    pub claims: Vec<MemoryClaim>,
}

impl MemoryTopic {
    /// Validate every claim of this topic against the summarized source.
    ///
    /// Only type-level rules are enforced here: slug form, nonempty single-line
    /// text, source membership, file path shape, and supersession reference
    /// form. File I/O, digest stamping, workspace containment, and the
    /// durability judgment are not type validation.
    pub fn validate(&self, source: &[MessageId]) -> Vec<MemoryClaimOutcome> {
        let slug = is_valid_slug(&self.slug);
        self.claims
            .iter()
            .enumerate()
            .map(|(index, claim)| {
                let reason = if !slug {
                    Some(MemoryClaimRejection::InvalidSlug)
                } else {
                    validate_claim(claim, source)
                };
                match reason {
                    Some(reason) => MemoryClaimOutcome::Rejected { index, reason },
                    None => MemoryClaimOutcome::Accepted {
                        index,
                        claim: claim.clone(),
                    },
                }
            })
            .collect()
    }
}

/// Per-claim validation outcome. The rejection recorder (M1 D4) consumes the
/// `Rejected` variants together with the topic slug and the claim at `index`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryClaimOutcome {
    Accepted {
        index: usize,
        claim: MemoryClaim,
    },
    Rejected {
        index: usize,
        reason: MemoryClaimRejection,
    },
}

/// Memory claim rejection reasons (M1 D4 plus the Q4 additions).
///
/// Type validation produces `InvalidSlug`, `InvalidShape`, `InvalidSources`,
/// `MissingSources` and `InvalidSupersession`; `UnreadableOrEscapingPath`,
/// `IndexCapReached` and `NotDurable` are decided at write time, not here.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryClaimRejection {
    InvalidSlug,
    MissingSources,
    UnreadableOrEscapingPath,
    IndexCapReached,
    InvalidShape,
    InvalidSources,
    InvalidSupersession,
    NotDurable,
}

/// Slugs are ASCII kebab-case from the codebase vocabulary: lowercase ASCII
/// segments separated by single hyphens. No length limit is imposed.
pub fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.split('-').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

fn validate_claim(claim: &MemoryClaim, source: &[MessageId]) -> Option<MemoryClaimRejection> {
    if claim.text.trim().is_empty() || claim.text.contains('\n') || claim.text.contains('\r') {
        return Some(MemoryClaimRejection::InvalidShape);
    }
    if claim.sources.iter().any(|id| !source.contains(id)) {
        return Some(MemoryClaimRejection::InvalidSources);
    }
    // A claim with no message source is still sourceful when it cites a file
    // path; a claim with neither is not written (spec 3.1).
    if claim.sources.is_empty() && claim.files.is_empty() {
        return Some(MemoryClaimRejection::MissingSources);
    }
    if claim.files.iter().any(|path| !is_valid_file_path(path)) {
        return Some(MemoryClaimRejection::InvalidShape);
    }
    if claim
        .supersedes
        .iter()
        .flatten()
        .any(|reference| !is_valid_supersession_ref(reference))
    {
        return Some(MemoryClaimRejection::InvalidSupersession);
    }
    None
}

/// File paths are supplied workspace-relative without digests. Only shape is
/// checked here; escaping paths (`..`, symlink escape) and readability are
/// contained at write time (task 4).
fn is_valid_file_path(path: &str) -> bool {
    !path.is_empty() && !path.contains('\0') && !path.starts_with('/')
}

/// Supersession references are `slug#claim-n` where `n` is the 1-based claim
/// ordinal in `## Claims`, not a physical file line (Q1).
pub(crate) fn is_valid_supersession_ref(reference: &str) -> bool {
    let Some((slug, ordinal)) = reference.split_once('#') else {
        return false;
    };
    if !is_valid_slug(slug) {
        return false;
    }
    let Some(number) = ordinal.strip_prefix("claim-") else {
        return false;
    };
    let bytes: Vec<u8> = number.bytes().collect();
    !bytes.is_empty() && bytes.first() != Some(&b'0') && bytes.iter().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(value: &str) -> MessageId {
        MessageId::parse(value).unwrap()
    }

    fn claim(text: &str, sources: Vec<MessageId>, files: Vec<String>) -> MemoryClaim {
        MemoryClaim {
            text: text.to_owned(),
            sources,
            files,
            supersedes: None,
        }
    }

    fn source() -> Vec<MessageId> {
        vec![
            msg("msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9"),
            msg("msg_01a090d5-4b9b-71b0-aae8-d5421f5c3513"),
        ]
    }

    fn rejected(outcomes: &[MemoryClaimOutcome], index: usize, reason: MemoryClaimRejection) {
        assert_eq!(
            outcomes.len(),
            1,
            "expected exactly one outcome, got {outcomes:?}"
        );
        assert_eq!(
            outcomes[0],
            MemoryClaimOutcome::Rejected { index, reason },
            "unexpected outcome: {:?}",
            outcomes[0]
        );
    }

    #[test]
    fn payload_round_trip() {
        let source = source();
        let topic = MemoryTopic {
            slug: "repl".to_owned(),
            claims: vec![
                MemoryClaim {
                    text: "The REPL is the default (no-subcommand) mode of the `agl` CLI."
                        .to_owned(),
                    sources: vec![source[0].clone()],
                    files: vec!["products/agl-cli/src/lib.rs".to_owned()],
                    supersedes: None,
                },
                MemoryClaim {
                    text: "Input uses reedline with SlashCompleter.".to_owned(),
                    sources: vec![source[0].clone(), source[1].clone()],
                    files: vec![],
                    supersedes: Some(vec!["repl#claim-1".to_owned()]),
                },
            ],
        };
        let json = serde_json::to_string(&topic).unwrap();
        let restored: MemoryTopic = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, topic);
        assert_eq!(
            restored.validate(&source),
            vec![
                MemoryClaimOutcome::Accepted {
                    index: 0,
                    claim: topic.claims[0].clone(),
                },
                MemoryClaimOutcome::Accepted {
                    index: 1,
                    claim: topic.claims[1].clone(),
                },
            ]
        );
    }

    #[test]
    fn optional_fields_default_when_absent() {
        let json = r#"{"slug":"compaction","claims":[{"text":"Compaction triggers at 70% of capacity.","sources":["msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9"]}]}"#;
        let topic: MemoryTopic = serde_json::from_str(json).unwrap();
        assert!(topic.claims[0].files.is_empty());
        assert!(topic.claims[0].supersedes.is_none());
        assert_eq!(
            topic.validate(&source()),
            vec![MemoryClaimOutcome::Accepted {
                index: 0,
                claim: topic.claims[0].clone(),
            }]
        );
    }

    #[test]
    fn valid_slug_forms() {
        let source = source();
        for slug in [
            "repl".to_owned(),
            "compaction".to_owned(),
            "build-release".to_owned(),
            "m1a".to_owned(),
            "a".repeat(64),
        ] {
            assert!(is_valid_slug(&slug), "slug {slug:?} must be valid");
            let topic = MemoryTopic {
                slug: slug.clone(),
                claims: vec![claim("A durable fact.", vec![source[0].clone()], vec![])],
            };
            assert_eq!(
                topic.validate(&source),
                vec![MemoryClaimOutcome::Accepted {
                    index: 0,
                    claim: topic.claims[0].clone(),
                }]
            );
        }
    }

    #[test]
    fn invalid_slug_rejects_every_claim() {
        let source = source();
        for slug in [
            "",
            "Repl",
            "build release",
            "-repl",
            "repl-",
            "re--pl",
            "repl_x",
        ] {
            assert!(!is_valid_slug(slug), "slug {slug:?} must be invalid");
            let topic = MemoryTopic {
                slug: slug.to_owned(),
                claims: vec![
                    claim("First fact.", vec![source[0].clone()], vec![]),
                    claim("Second fact.", vec![source[1].clone()], vec![]),
                ],
            };
            assert_eq!(
                topic.validate(&source),
                vec![
                    MemoryClaimOutcome::Rejected {
                        index: 0,
                        reason: MemoryClaimRejection::InvalidSlug,
                    },
                    MemoryClaimOutcome::Rejected {
                        index: 1,
                        reason: MemoryClaimRejection::InvalidSlug,
                    },
                ]
            );
        }
    }

    #[test]
    fn empty_or_multiline_text_is_invalid_shape() {
        let source = source();
        for text in ["", "   ", "a\nb", "a\rb"] {
            let topic = MemoryTopic {
                slug: "repl".to_owned(),
                claims: vec![claim(text, vec![source[0].clone()], vec![])],
            };
            rejected(
                &topic.validate(&source),
                0,
                MemoryClaimRejection::InvalidShape,
            );
        }
    }

    #[test]
    fn source_membership() {
        let source = source();
        let topic = MemoryTopic {
            slug: "repl".to_owned(),
            claims: vec![claim(
                "Fact.",
                vec![msg("msg_01a090d5-9999-7a01-ac4c-8c7b192c76e9")],
                vec![],
            )],
        };
        rejected(
            &topic.validate(&source),
            0,
            MemoryClaimRejection::InvalidSources,
        );
    }

    #[test]
    fn claim_without_any_source_is_missing_sources() {
        let source = source();
        let topic = MemoryTopic {
            slug: "repl".to_owned(),
            claims: vec![claim("Fact.", vec![], vec![])],
        };
        rejected(
            &topic.validate(&source),
            0,
            MemoryClaimRejection::MissingSources,
        );
    }

    #[test]
    fn optional_file_paths() {
        let source = source();
        let topic = MemoryTopic {
            slug: "repl".to_owned(),
            claims: vec![
                claim(
                    "Fact with a relative path.",
                    vec![source[0].clone()],
                    vec!["crates/agl-core/src/lib.rs".to_owned()],
                ),
                claim(
                    "Absolute path.",
                    vec![source[0].clone()],
                    vec!["/etc/passwd".to_owned()],
                ),
                claim("Empty path.", vec![source[0].clone()], vec!["".to_owned()]),
            ],
        };
        let outcomes = topic.validate(&source);
        assert!(matches!(
            outcomes[0],
            MemoryClaimOutcome::Accepted { index: 0, .. }
        ));
        assert_eq!(
            outcomes[1],
            MemoryClaimOutcome::Rejected {
                index: 1,
                reason: MemoryClaimRejection::InvalidShape,
            }
        );
        assert_eq!(
            outcomes[2],
            MemoryClaimOutcome::Rejected {
                index: 2,
                reason: MemoryClaimRejection::InvalidShape,
            }
        );

        // A claim citing only a file path is sourceful at the type level; the
        // daemon decides readability at write time.
        let file_only = MemoryTopic {
            slug: "repl".to_owned(),
            claims: vec![claim(
                "File-only fact.",
                vec![],
                vec!["memory/INDEX.md".to_owned()],
            )],
        };
        assert!(matches!(
            file_only.validate(&source)[0],
            MemoryClaimOutcome::Accepted { index: 0, .. }
        ));
    }

    #[test]
    fn supersedes_reference_form() {
        let source = source();
        let mut claim = claim("New fact.", vec![source[0].clone()], vec![]);
        claim.supersedes = Some(vec!["repl#claim-2".to_owned()]);
        let topic = MemoryTopic {
            slug: "repl".to_owned(),
            claims: vec![claim],
        };
        assert!(matches!(
            topic.validate(&source)[0],
            MemoryClaimOutcome::Accepted { index: 0, .. }
        ));

        for reference in [
            "repl#claim-0",
            "repl#claim-01",
            "repl#3",
            "#claim-1",
            "Repl#claim-1",
            "repl#claim-",
            "repl#claim-a",
            "replclaim-1",
        ] {
            let topic = MemoryTopic {
                slug: "repl".to_owned(),
                claims: vec![MemoryClaim {
                    text: "New fact.".to_owned(),
                    sources: vec![source[0].clone()],
                    files: vec![],
                    supersedes: Some(vec![reference.to_owned()]),
                }],
            };
            rejected(
                &topic.validate(&source),
                0,
                MemoryClaimRejection::InvalidSupersession,
            );
        }
    }

    #[test]
    fn claim_wording_language_is_not_checked() {
        let source = source();
        let topic = MemoryTopic {
            slug: "repl".to_owned(),
            claims: vec![claim(
                "Використовується reedline для вводу.",
                vec![source[0].clone()],
                vec![],
            )],
        };
        assert!(matches!(
            topic.validate(&source)[0],
            MemoryClaimOutcome::Accepted { index: 0, .. }
        ));
    }

    #[test]
    fn topic_without_claims_has_no_outcomes() {
        let topic = MemoryTopic {
            slug: "repl".to_owned(),
            claims: vec![],
        };
        assert!(topic.validate(&source()).is_empty());
    }
}
