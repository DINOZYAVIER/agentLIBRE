use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Deserializer, Serialize};

use crate::agent::{AbsolutePath, PackageDigest, RelativePath};

pub const MAX_TEXT_BYTES: usize = 1_048_576;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageSource {
    File {
        path: AbsolutePath,
    },
    Forge {
        workspace: AbsolutePath,
        id: String,
        relative: RelativePath,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRef {
    source: ImageSource,
    digest: PackageDigest,
}

impl ImageRef {
    pub fn new(source: ImageSource, digest: PackageDigest) -> Result<Self, ContentError> {
        if let ImageSource::Forge { id, relative, .. } = &source {
            if id.is_empty()
                || !id.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'-')
                })
            {
                return Err(ContentError::InvalidImageSource("invalid Forge source id"));
            }
            if relative.as_path().as_os_str().is_empty()
                || relative
                    .as_path()
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                return Err(ContentError::InvalidImageSource(
                    "image path must identify a relative Forge file",
                ));
            }
        }
        Ok(Self { source, digest })
    }

    pub fn source(&self) -> &ImageSource {
        &self.source
    }
    pub fn digest(&self) -> PackageDigest {
        self.digest
    }
}

impl<'de> Deserialize<'de> for ImageRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            source: ImageSource,
            digest: PackageDigest,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.source, wire.digest).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Content {
    text: Box<str>,
    images: Box<[ImageRef]>,
}

impl Content {
    pub fn text(text: impl Into<String>) -> Result<Self, ContentError> {
        Self::with_images(text, Vec::new())
    }

    pub fn with_images(
        text: impl Into<String>,
        images: Vec<ImageRef>,
    ) -> Result<Self, ContentError> {
        let content = Self {
            text: text.into().into_boxed_str(),
            images: images.into_boxed_slice(),
        };
        content.validate()?;
        Ok(content)
    }

    pub fn validate(&self) -> Result<(), ContentError> {
        if self.text.is_empty() && self.images.is_empty() {
            return Err(ContentError::EmptyText);
        }
        if self.text.len() > MAX_TEXT_BYTES {
            return Err(ContentError::TextTooLarge(self.text.len()));
        }
        Ok(())
    }

    pub fn images(&self) -> &[ImageRef] {
        &self.images
    }

    pub fn as_text(&self) -> &str {
        &self.text
    }

    pub fn into_text(self) -> String {
        self.text.into()
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
            images: Vec<ImageRef>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::with_images(wire.text, wire.images).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentError {
    EmptyText,
    TextTooLarge(usize),
    InvalidImageSource(&'static str),
}

impl Display for ContentError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyText => formatter.write_str("content requires text or an image"),
            Self::InvalidImageSource(reason) => formatter.write_str(reason),
            Self::TextTooLarge(bytes) => write!(formatter, "content text is too large: {bytes}"),
        }
    }
}

impl std::error::Error for ContentError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_only_content_stores_validated_sources_and_full_digests() {
        let image = ImageRef::new(
            ImageSource::File {
                path: "/tmp/example.png".to_owned().try_into().unwrap(),
            },
            PackageDigest::from_bytes([4; 32]),
        )
        .unwrap();
        let content = Content::with_images("", vec![image.clone()]).unwrap();
        let wire = serde_json::to_value(&content).unwrap();
        assert_eq!(wire["images"][0]["source"]["type"], "file");
        assert_eq!(wire["images"][0]["digest"], image.digest().to_string());
        assert_eq!(serde_json::from_value::<Content>(wire).unwrap(), content);
        assert!(Content::with_images("", vec![]).is_err());
        assert!(Content::with_images("x".repeat(MAX_TEXT_BYTES + 1), vec![image]).is_err());
        assert!(serde_json::from_value::<Content>(serde_json::json!({"text":"old"})).is_err());
        assert!(
            ImageRef::new(
                ImageSource::Forge {
                    workspace: "/tmp/workspace".to_owned().try_into().unwrap(),
                    id: "".into(),
                    relative: "image.png".to_owned().try_into().unwrap(),
                },
                PackageDigest::from_bytes([4; 32])
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<ImageRef>(serde_json::json!({
                "source":{"type":"file","path":"relative.png"},
                "digest": PackageDigest::from_bytes([4;32]),
            }))
            .is_err()
        );
    }

    #[test]
    fn text_content_has_one_strict_wire_shape() {
        let content = Content::text("hello").unwrap();
        assert_eq!(content.as_text(), "hello");
        assert_eq!(
            serde_json::to_value(&content).unwrap(),
            serde_json::json!({"text": "hello", "images": []})
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
