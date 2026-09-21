use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Deserializer, Serialize};

pub const MAX_TEXT_BYTES: usize = 1_048_576;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Content {
    text: String,
}

impl Content {
    pub fn text(text: impl Into<String>) -> Result<Self, ContentError> {
        let text = text.into();
        validate_text(&text)?;
        Ok(Self { text })
    }

    pub fn validate(&self) -> Result<(), ContentError> {
        validate_text(&self.text)
    }

    pub fn as_text(&self) -> &str {
        &self.text
    }

    pub fn into_text(self) -> String {
        self.text
    }

    pub fn text_byte_len(&self) -> usize {
        self.text.len()
    }
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            text: String,
        }

        Self::text(Wire::deserialize(deserializer)?.text).map_err(serde::de::Error::custom)
    }
}

fn validate_text(text: &str) -> Result<(), ContentError> {
    if text.is_empty() {
        return Err(ContentError::EmptyText);
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(ContentError::TextTooLarge(text.len()));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentError {
    EmptyText,
    TextTooLarge(usize),
}

impl Display for ContentError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyText => formatter.write_str("content text cannot be empty"),
            Self::TextTooLarge(bytes) => write!(formatter, "content text is too large: {bytes}"),
        }
    }
}

impl std::error::Error for ContentError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_content_has_one_strict_wire_shape() {
        let content = Content::text("hello").unwrap();
        assert_eq!(content.as_text(), "hello");
        assert_eq!(
            serde_json::to_value(&content).unwrap(),
            serde_json::json!({"text": "hello"})
        );
        assert_eq!(
            serde_json::from_value::<Content>(serde_json::to_value(&content).unwrap()).unwrap(),
            content
        );
        assert!(Content::text("").is_err());
        assert!(
            serde_json::from_value::<Content>(serde_json::json!({
                "parts": [{"kind": "text", "text": "hello"}]
            }))
            .is_err()
        );
    }
}
