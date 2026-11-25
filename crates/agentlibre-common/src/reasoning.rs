//! The model's reasoning level (AGL-013, AGL-130 D8), which the surfaces and
//! the run share.

/// The model's reasoning level (AGL-013): the values of the template's
/// `reasoning_effort`. `Off` silences the thinking with the template's
/// `enable_thinking = false` instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningLevel {
    Off,
    Low,
    Medium,
    Xhigh,
}

impl ReasoningLevel {
    /// The four values, as the human sets them.
    pub fn parse(value: &str) -> Option<ReasoningLevel> {
        match value {
            "off" => Some(ReasoningLevel::Off),
            "low" => Some(ReasoningLevel::Low),
            "medium" => Some(ReasoningLevel::Medium),
            "xhigh" => Some(ReasoningLevel::Xhigh),
            _ => None,
        }
    }

    /// The level's name: how the human sets it, what the template's
    /// `reasoning_effort` takes and what the surfaces show.
    pub fn name(self) -> &'static str {
        match self {
            ReasoningLevel::Off => "off",
            ReasoningLevel::Low => "low",
            ReasoningLevel::Medium => "medium",
            ReasoningLevel::Xhigh => "xhigh",
        }
    }
}
