use serde::{Deserialize, Serialize};

use crate::CommandId;

pub const MAX_SUGGESTIONS: usize = 50;
pub const MAX_SUGGESTION_LABEL_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestionRequest {
    pub command_id: CommandId,
    pub argument_id: String,
    pub query: String,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suggestion {
    pub value: String,
    pub label: String,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestionPage {
    pub entries: Vec<Suggestion>,
    pub next_cursor: Option<String>,
}

impl SuggestionPage {
    pub fn validate(mut self) -> Self {
        self.entries.truncate(MAX_SUGGESTIONS);
        self.entries.retain(|entry| {
            !entry.value.is_empty()
                && entry.label.len() <= MAX_SUGGESTION_LABEL_BYTES
                && entry
                    .detail
                    .as_ref()
                    .is_none_or(|detail| detail.len() <= MAX_SUGGESTION_LABEL_BYTES)
        });
        self.entries.sort_by(|left, right| {
            left.label
                .to_lowercase()
                .cmp(&right.label.to_lowercase())
                .then_with(|| left.value.cmp(&right.value))
        });
        self
    }
}
