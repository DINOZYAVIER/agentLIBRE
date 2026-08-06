#[path = "core/support/mod.rs"]
mod support;
#[path = "core/support/turn.rs"]
mod turn_support;

use agl_kernel::{HookBatchOutcome, HookBatchSummary, TurnHookBatch};
use agl_kernel::{HookBatchResult, HookEvent, HookId, HookMessage, HookResult, HookStatus};
use agl_kernel::{TurnAdvanceState, TurnRequest};
use support::hook_id;
use turn_support::{
    context_hook_batch, first_hook_ids, resume_first_hook, turn_input, validate_turn_start,
};

fn permutations<T: Clone>(values: &[T]) -> Vec<Vec<T>> {
    fn visit<T: Clone>(prefix: &mut Vec<T>, rest: &mut Vec<T>, output: &mut Vec<Vec<T>>) {
        if rest.is_empty() {
            output.push(prefix.clone());
            return;
        }
        for index in 0..rest.len() {
            let value = rest.remove(index);
            prefix.push(value.clone());
            visit(prefix, rest, output);
            prefix.pop();
            rest.insert(index, value);
        }
    }

    let mut output = Vec::new();
    visit(&mut Vec::new(), &mut values.to_vec(), &mut output);
    output
}

// KCT-HOOK-001. Mutation: deduplicate a repeated HookId or let required win.
#[test]
fn every_duplicate_hook_requirement_is_rejected_before_turn_start() {
    let duplicate = "guard:duplicate";
    let cases = [
        context_hook_batch([duplicate, duplicate], []),
        context_hook_batch([], [duplicate, duplicate]),
        context_hook_batch([duplicate], [duplicate]),
    ];

    for batch in cases {
        let error = validate_turn_start(turn_input().with_hook_batch(batch))
            .expect_err("duplicate Hook requirement was admitted");
        assert!(!error.trim().is_empty());
    }

    let split_across_consumers = turn_input()
        .with_hook_batch(context_hook_batch([duplicate], []))
        .with_hook_batch(context_hook_batch([], [duplicate]));
    validate_turn_start(split_across_consumers)
        .expect_err("duplicate Hook requirement across batches was admitted");
}

// KCT-HOOK-002. Mutation: preserve Function, Skill or registration order.
#[test]
fn hook_request_order_is_canonical_for_every_input_permutation() {
    let requirements = [
        ("guard:zeta", true),
        ("guard:alpha", false),
        ("guard:mu", true),
    ];
    let expected = ["guard:alpha", "guard:mu", "guard:zeta"];

    for permutation in permutations(&requirements) {
        let input = permutation
            .into_iter()
            .fold(turn_input(), |input, (id, required)| {
                let batch = if required {
                    context_hook_batch([id], [])
                } else {
                    context_hook_batch([], [id])
                };
                input.with_hook_batch(batch)
            });
        let actual = first_hook_ids(input)
            .expect("ContextPrepare HookBatch is the first request")
            .into_iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }
}

fn result(id: &str, status: HookStatus) -> HookResult {
    HookResult {
        hook_id: hook_id(id),
        status,
        messages: Vec::new(),
    }
}

fn summary(
    required: &[&str],
    optional: &[&str],
    results: impl IntoIterator<Item = HookResult>,
) -> HookBatchSummary {
    let batch = required.iter().fold(
        TurnHookBatch::new(HookEvent::ContextPrepare),
        |batch, id| batch.with_required_hook(hook_id(id)),
    );
    let batch = optional
        .iter()
        .fold(batch, |batch, id| batch.with_optional_hook(hook_id(id)));
    HookBatchSummary::from_batch_result(
        &batch,
        HookBatchResult {
            event: HookEvent::ContextPrepare,
            results: results.into_iter().collect(),
        },
        Some(1),
    )
}

// KCT-HOOK-005. Mutation: swap any two selected precedence levels.
#[test]
fn hook_aggregation_uses_the_selected_precedence_independent_of_result_order() {
    let cases = [
        (HookStatus::Pass, HookStatus::Pass, HookBatchOutcome::Pass),
        (HookStatus::Pass, HookStatus::Warn, HookBatchOutcome::Warn),
        (HookStatus::Pass, HookStatus::Fail, HookBatchOutcome::Warn),
        (
            HookStatus::Pass,
            HookStatus::Repair,
            HookBatchOutcome::Repair,
        ),
        (
            HookStatus::Warn,
            HookStatus::Repair,
            HookBatchOutcome::Repair,
        ),
        (HookStatus::Fail, HookStatus::Pass, HookBatchOutcome::Fail),
        (HookStatus::Fail, HookStatus::Warn, HookBatchOutcome::Fail),
        (HookStatus::Fail, HookStatus::Repair, HookBatchOutcome::Fail),
    ];

    for (required, optional, expected) in cases {
        let values = [
            result("guard:required", required),
            result("guard:optional", optional),
        ];
        for permutation in permutations(&values) {
            assert_eq!(
                summary(&["guard:required"], &["guard:optional"], permutation).outcome(),
                expected,
                "required={required:?} optional={optional:?}"
            );
        }
    }
}

// KCT-HOOK-004. Mutation: aggregate a foreign, missing, extra or duplicate result.
#[test]
fn hook_batch_result_requires_exact_event_ids_and_cardinality() {
    let required = "guard:required";
    let optional = "guard:optional";
    let input = || turn_input().with_hook_batch(context_hook_batch([required], [optional]));

    let invalid = [
        HookBatchResult {
            event: HookEvent::ModelResponse,
            results: vec![
                result(required, HookStatus::Pass),
                result(optional, HookStatus::Pass),
            ],
        },
        HookBatchResult {
            event: HookEvent::ContextPrepare,
            results: vec![result(required, HookStatus::Pass)],
        },
        HookBatchResult {
            event: HookEvent::ContextPrepare,
            results: vec![
                result(required, HookStatus::Pass),
                result(optional, HookStatus::Pass),
                result("guard:extra", HookStatus::Pass),
            ],
        },
        HookBatchResult {
            event: HookEvent::ContextPrepare,
            results: vec![
                result(required, HookStatus::Pass),
                result(optional, HookStatus::Pass),
                result(optional, HookStatus::Pass),
            ],
        },
    ];

    for batch_result in invalid {
        assert!(
            resume_first_hook(input(), batch_result).is_err(),
            "invalid Hook result was accepted"
        );
    }

    let valid = HookBatchResult {
        event: HookEvent::ContextPrepare,
        results: vec![
            result(optional, HookStatus::Warn),
            result(required, HookStatus::Pass),
        ],
    };
    let advance = resume_first_hook(input(), valid).expect("exact Hook result is accepted");
    assert!(matches!(
        advance.state,
        TurnAdvanceState::Pending {
            request: TurnRequest::ModelGeneration { .. }
        }
    ));
}

// KCT-HOOK-003. Mutation: accept a result prefix after the first required Fail.
#[test]
fn required_failure_still_requires_and_preserves_the_complete_result_set() {
    let ids = ["guard:alpha", "guard:mu", "guard:zeta"];
    let input = || turn_input().with_hook_batch(context_hook_batch(ids, []));

    assert!(
        resume_first_hook(
            input(),
            HookBatchResult {
                event: HookEvent::ContextPrepare,
                results: vec![result(ids[0], HookStatus::Fail)],
            },
        )
        .is_err(),
        "a prefix ending at the first Fail was accepted"
    );

    let complete = resume_first_hook(
        input(),
        HookBatchResult {
            event: HookEvent::ContextPrepare,
            results: vec![
                result(ids[0], HookStatus::Fail),
                result(ids[1], HookStatus::Pass),
                result(ids[2], HookStatus::Warn),
            ],
        },
    )
    .expect("the complete result set is accepted before aggregation");
    let encoded = serde_json::to_string(&complete).unwrap();
    for id in ids {
        assert!(encoded.contains(id), "complete result lost {id}: {encoded}");
    }
}

// Compile-time guard for test data itself: canonical Hook IDs remain ordered.
#[test]
fn hook_id_order_used_by_the_suite_is_lexical_and_fully_qualified() {
    let mut ids = ["guard:zeta", "guard:alpha", "guard:mu"]
        .map(hook_id)
        .to_vec();
    ids.sort();
    assert_eq!(
        ids,
        ["guard:alpha", "guard:mu", "guard:zeta"]
            .map(hook_id)
            .to_vec()
    );
    assert!(HookId::new("unqualified").is_err());
}

// Retained event-evidence invariant. Mutation: leak Hook diagnostic text into the summary.
#[test]
fn hook_event_summary_keeps_codes_without_message_or_fix_text() {
    let batch =
        TurnHookBatch::new(HookEvent::ArtifactWrite).with_required_hook(hook_id("guard:artifact"));
    let summary = HookBatchSummary::from_batch_result(
        &batch,
        HookBatchResult {
            event: HookEvent::ArtifactWrite,
            results: vec![HookResult {
                hook_id: hook_id("guard:artifact"),
                status: HookStatus::Repair,
                messages: vec![HookMessage {
                    code: "guard.repair".to_string(),
                    message: "private diagnostic".to_string(),
                    fix: Some("private fix".to_string()),
                }],
            }],
        },
        Some(1),
    );
    let encoded = serde_json::to_string(&summary).unwrap();
    assert!(encoded.contains("guard.repair"));
    assert!(!encoded.contains("private diagnostic"));
    assert!(!encoded.contains("private fix"));
}
