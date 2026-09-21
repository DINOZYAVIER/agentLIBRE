use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextExhaustion {
    pub capacity: ContextCapacity,
    pub compaction: Option<Box<super::CompactionFailure>>,
}

impl From<ContextCapacity> for ContextExhaustion {
    fn from(capacity: ContextCapacity) -> Self {
        Self {
            capacity,
            compaction: None,
        }
    }
}

/// Counts measured from the serialized request using the active model tokenizer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextCapacity {
    pub prompt_tokens: u64,
    pub reserved_output_tokens: u64,
    pub context_capacity_tokens: u32,
    pub trigger_threshold_tokens: u64,
}

impl ContextCapacity {
    pub fn new(prompt_tokens: u64, reserved_output_tokens: u64, capacity: u32) -> Self {
        Self {
            prompt_tokens,
            reserved_output_tokens,
            context_capacity_tokens: capacity,
            trigger_threshold_tokens: (u64::from(capacity) * 7).div_ceil(10),
        }
    }

    pub fn fits(&self) -> bool {
        self.prompt_tokens
            .checked_add(self.reserved_output_tokens)
            .is_some_and(|total| total <= u64::from(self.context_capacity_tokens))
    }

    pub fn needs_compaction(&self) -> bool {
        self.prompt_tokens >= self.trigger_threshold_tokens || !self.fits()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionBudgets {
    pub summary_output_tokens: u64,
    pub summary_reasoning_tokens: u64,
    pub rebuilt_input_target: u64,
    pub recent_tail_limit: u64,
}

impl CompactionBudgets {
    pub fn new(capacity: u32, work_output_tokens: u64, supports_low: bool) -> Option<Self> {
        u64::from(capacity).checked_sub(work_output_tokens)?;
        let summary_output_tokens = (u64::from(capacity) / 8).min(16_384);
        // A successful compaction must create substantial headroom instead of
        // merely making the next request fit. This target includes the complete
        // serialized prompt: instructions, Tools, exact state, semantic summary,
        // retained checkpoints, and recent tail.
        let rebuilt_input_target = u64::from(capacity) / 10;
        Some(Self {
            summary_output_tokens,
            summary_reasoning_tokens: if supports_low {
                summary_output_tokens / 4
            } else {
                0
            },
            rebuilt_input_target,
            // Tail selection may use the whole target; the final rebuilt
            // request is measured again and remains the authoritative bound.
            recent_tail_limit: rebuilt_input_target,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_and_output_reservation_are_independent_exact_boundaries() {
        assert!(!ContextCapacity::new(45_875, 1, 65_536).needs_compaction());
        assert!(ContextCapacity::new(45_876, 1, 65_536).needs_compaction());
        assert!(!ContextCapacity::new(81_920, 49_152, 131_072).needs_compaction());
        let crossed = ContextCapacity::new(81_921, 49_152, 131_072);
        assert_eq!(crossed.trigger_threshold_tokens, 91_751);
        assert!(crossed.needs_compaction());
        assert!(!crossed.fits());
        assert!(!ContextCapacity::new(u64::MAX, 1, u32::MAX).fits());
    }

    #[test]
    fn summary_budgets_follow_capacity_and_low_capability_only() {
        let qwen = CompactionBudgets::new(131_072, 49_152, true).unwrap();
        assert_eq!(
            qwen,
            CompactionBudgets {
                summary_output_tokens: 16_384,
                summary_reasoning_tokens: 4_096,
                rebuilt_input_target: 13_107,
                recent_tail_limit: 13_107,
            }
        );
        assert_eq!(
            CompactionBudgets::new(131_072, 49_152, false).unwrap(),
            CompactionBudgets {
                summary_reasoning_tokens: 0,
                ..qwen
            }
        );
        assert_eq!(
            CompactionBudgets::new(4_096, 1_024, true).unwrap(),
            CompactionBudgets {
                summary_output_tokens: 512,
                summary_reasoning_tokens: 128,
                rebuilt_input_target: 409,
                recent_tail_limit: 409,
            }
        );
        assert!(CompactionBudgets::new(1_024, 1_025, true).is_none());
    }
}
