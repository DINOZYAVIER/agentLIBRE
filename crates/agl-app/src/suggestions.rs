use serde::{Deserialize, Serialize};

use agl_ids::SessionId;

use crate::{ApplicationError, ApplicationErrorCode, CommandId};

pub const MAX_SUGGESTIONS: usize = 50;
pub const MAX_SUGGESTION_LABEL_BYTES: usize = 8 * 1024;
pub const MAX_SUGGESTION_CURSOR_BYTES: usize = 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestionRequest {
    pub session_id: Option<SessionId>,
    pub command_id: CommandId,
    pub argument_id: String,
    pub query: String,
    pub cursor: Option<String>,
}

impl SuggestionRequest {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.argument_id.is_empty()
            || self.argument_id.len() > 128
            || self.argument_id.contains(['\0', '\n', '\r'])
            || self.query.len() > MAX_SUGGESTION_LABEL_BYTES
            || self.query.contains('\0')
            || self.cursor.as_ref().is_some_and(|cursor| {
                cursor.is_empty()
                    || cursor.len() > MAX_SUGGESTION_CURSOR_BYTES
                    || cursor.contains(['\0', '\n', '\r'])
            })
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "suggestion request fields exceed their bounds",
            ));
        }
        Ok(())
    }
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
        if self.next_cursor.as_ref().is_some_and(|cursor| {
            cursor.is_empty()
                || cursor.len() > MAX_SUGGESTION_CURSOR_BYTES
                || cursor.contains(['\0', '\n', '\r'])
        }) {
            self.next_cursor = None;
        }
        self
    }
}
