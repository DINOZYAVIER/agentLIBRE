use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_CONTENT_PARTS: usize = 64;
pub const MAX_TEXT_PART_BYTES: usize = 1_048_576;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ArtifactId(String);

impl ArtifactId {
    pub fn generate() -> Self {
        Self(format!("art_{}", uuid::Uuid::now_v7()))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, ContentError> {
        let value = value.into();
        let uuid = value
            .strip_prefix("art_")
            .ok_or_else(|| ContentError::InvalidArtifactId(value.clone()))?;
        let parsed = uuid::Uuid::parse_str(uuid)
            .map_err(|_| ContentError::InvalidArtifactId(value.clone()))?;
        if parsed.get_version_num() != 7 || format!("art_{parsed}") != value {
            return Err(ContentError::InvalidArtifactId(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ArtifactId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ArtifactId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct BlobDigest(String);

impl BlobDigest {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        Self(format!("sha256:{digest:x}"))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, ContentError> {
        let value = value.into();
        let Some(hex) = value.strip_prefix("sha256:") else {
            return Err(ContentError::InvalidBlobDigest(value));
        };
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ContentError::InvalidBlobDigest(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn hex(&self) -> &str {
        self.0
            .strip_prefix("sha256:")
            .expect("validated digest has prefix")
    }
}

impl Display for BlobDigest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for BlobDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaType {
    ImagePng,
    ImageJpeg,
}

impl MediaType {
    pub fn mime(self) -> &'static str {
        match self {
            Self::ImagePng => "image/png",
            Self::ImageJpeg => "image/jpeg",
        }
    }

    pub fn parse_mime(value: &str) -> Result<Self, ContentError> {
        match value {
            "image/png" => Ok(Self::ImagePng),
            "image/jpeg" => Ok(Self::ImageJpeg),
            _ => Err(ContentError::UnsupportedMediaType(value.to_string())),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSensitivity {
    Private,
    Sensitive,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRetention {
    RunScoped,
    Persistent,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSourceKind {
    ScreenCapture,
    UserProvided,
    Generated,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSource {
    pub kind: ArtifactSourceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
}

impl ImageDimensions {
    pub fn new(width: u32, height: u32) -> Result<Self, ContentError> {
        if width == 0 || height == 0 {
            return Err(ContentError::InvalidImageDimensions { width, height });
        }
        Ok(Self { width, height })
    }

    pub fn pixels(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub artifact_id: ArtifactId,
    pub digest: BlobDigest,
    pub media_type: MediaType,
    pub byte_length: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageDimensions>,
    pub sensitivity: ArtifactSensitivity,
    pub source: ArtifactSource,
}

impl ArtifactRef {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        artifact_id: ArtifactId,
        digest: BlobDigest,
        media_type: MediaType,
        byte_length: u64,
        image: Option<ImageDimensions>,
        sensitivity: ArtifactSensitivity,
        source: ArtifactSource,
    ) -> Result<Self, ContentError> {
        let reference = Self {
            artifact_id,
            digest,
            media_type,
            byte_length,
            image,
            sensitivity,
            source,
        };
        reference.validate()?;
        Ok(reference)
    }

    pub fn validate(&self) -> Result<(), ContentError> {
        if self.byte_length == 0 {
            return Err(ContentError::EmptyArtifact);
        }
        if matches!(self.media_type, MediaType::ImagePng | MediaType::ImageJpeg)
            && self.image.is_none()
        {
            return Err(ContentError::MissingImageDimensions);
        }
        if self
            .source
            .provider
            .as_ref()
            .is_some_and(|provider| provider.trim().is_empty() || provider.len() > 128)
        {
            return Err(ContentError::InvalidSourceProvider);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ArtifactRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            artifact_id: ArtifactId,
            digest: BlobDigest,
            media_type: MediaType,
            byte_length: u64,
            image: Option<ImageDimensions>,
            sensitivity: ArtifactSensitivity,
            source: ArtifactSource,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(
            wire.artifact_id,
            wire.digest,
            wire.media_type,
            wire.byte_length,
            wire.image,
            wire.sensitivity,
            wire.source,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentPart {
    Text { text: String },
    Artifact { artifact: ArtifactRef },
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Result<Self, ContentError> {
        let text = text.into();
        validate_text(&text)?;
        Ok(Self::Text { text })
    }

    pub fn artifact(artifact: ArtifactRef) -> Self {
        Self::Artifact { artifact }
    }

    fn validate(&self) -> Result<(), ContentError> {
        match self {
            Self::Text { text } => validate_text(text),
            Self::Artifact { artifact } => artifact.validate(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Content {
    pub parts: Vec<ContentPart>,
}

impl Content {
    pub fn new(parts: impl IntoIterator<Item = ContentPart>) -> Result<Self, ContentError> {
        let content = Self {
            parts: parts.into_iter().collect(),
        };
        content.validate()?;
        Ok(content)
    }

    pub fn text(text: impl Into<String>) -> Result<Self, ContentError> {
        Self::new([ContentPart::text(text)?])
    }

    pub fn validate(&self) -> Result<(), ContentError> {
        if self.parts.is_empty() {
            return Err(ContentError::EmptyContent);
        }
        if self.parts.len() > MAX_CONTENT_PARTS {
            return Err(ContentError::TooManyParts(self.parts.len()));
        }
        for part in &self.parts {
            part.validate()?;
        }
        Ok(())
    }

    pub fn artifacts(&self) -> impl Iterator<Item = &ArtifactRef> {
        self.parts.iter().filter_map(|part| match part {
            ContentPart::Artifact { artifact } => Some(artifact),
            ContentPart::Text { .. } => None,
        })
    }

    pub fn has_artifacts(&self) -> bool {
        self.artifacts().next().is_some()
    }

    pub fn text_byte_len(&self) -> usize {
        self.parts
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => text.len(),
                ContentPart::Artifact { .. } => 0,
            })
            .sum()
    }

    pub fn artifact_count(&self) -> usize {
        self.artifacts().count()
    }

    pub fn text_only(&self) -> Option<String> {
        let mut output = String::new();
        for part in &self.parts {
            match part {
                ContentPart::Text { text } => output.push_str(text),
                ContentPart::Artifact { .. } => return None,
            }
        }
        Some(output)
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
            parts: Vec<ContentPart>,
        }
        Self::new(Wire::deserialize(deserializer)?.parts).map_err(serde::de::Error::custom)
    }
}

fn validate_text(text: &str) -> Result<(), ContentError> {
    if text.is_empty() {
        return Err(ContentError::EmptyText);
    }
    if text.len() > MAX_TEXT_PART_BYTES {
        return Err(ContentError::TextTooLarge(text.len()));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentError {
    InvalidArtifactId(String),
    InvalidBlobDigest(String),
    UnsupportedMediaType(String),
    InvalidImageDimensions { width: u32, height: u32 },
    MissingImageDimensions,
    EmptyArtifact,
    InvalidSourceProvider,
    EmptyContent,
    EmptyText,
    TooManyParts(usize),
    TextTooLarge(usize),
}

impl Display for ContentError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArtifactId(value) => write!(formatter, "invalid artifact ID {value:?}"),
            Self::InvalidBlobDigest(value) => write!(formatter, "invalid blob digest {value:?}"),
            Self::UnsupportedMediaType(value) => {
                write!(formatter, "unsupported media type {value:?}")
            }
            Self::InvalidImageDimensions { width, height } => {
                write!(formatter, "invalid image dimensions {width}x{height}")
            }
            Self::MissingImageDimensions => formatter.write_str("image dimensions are required"),
            Self::EmptyArtifact => formatter.write_str("artifact byte length must be positive"),
            Self::InvalidSourceProvider => formatter.write_str("invalid artifact source provider"),
            Self::EmptyContent => formatter.write_str("content must contain at least one part"),
            Self::EmptyText => formatter.write_str("text content part cannot be empty"),
            Self::TooManyParts(count) => write!(formatter, "content has too many parts: {count}"),
            Self::TextTooLarge(bytes) => {
                write!(formatter, "text content part is too large: {bytes}")
            }
        }
    }
}

impl std::error::Error for ContentError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_artifact_content_round_trip_without_bytes_or_paths() {
        let bytes = b"private image";
        let reference = ArtifactRef::new(
            ArtifactId::generate(),
            BlobDigest::from_bytes(bytes),
            MediaType::ImagePng,
            u64::try_from(bytes.len()).unwrap(),
            Some(ImageDimensions::new(2, 3).unwrap()),
            ArtifactSensitivity::Sensitive,
            ArtifactSource {
                kind: ArtifactSourceKind::ScreenCapture,
                provider: Some("xdg-desktop-portal".to_string()),
            },
        )
        .unwrap();
        let content = Content::new([
            ContentPart::text("Inspect this image: ").unwrap(),
            ContentPart::artifact(reference),
        ])
        .unwrap();
        let json = serde_json::to_string(&content).unwrap();
        assert!(!json.contains("private image"));
        assert!(!json.contains("/tmp"));
        assert!(!json.contains("base64"));
        assert_eq!(serde_json::from_str::<Content>(&json).unwrap(), content);
    }

    #[test]
    fn invalid_content_and_reference_shapes_fail_closed() {
        assert!(Content::new([]).is_err());
        assert!(ContentPart::text("").is_err());
        assert!(BlobDigest::parse("sha256:ABC").is_err());
        assert!(ArtifactId::parse("artifact-1").is_err());
        assert!(
            serde_json::from_value::<Content>(serde_json::json!({
                "parts": [{"kind": "text", "text": "ok", "bytes": "forbidden"}]
            }))
            .is_err()
        );
    }

    #[test]
    fn text_only_has_one_canonical_shape() {
        let content = Content::text("hello").unwrap();
        assert_eq!(content.text_only().as_deref(), Some("hello"));
        assert_eq!(
            serde_json::to_value(content).unwrap(),
            serde_json::json!({"parts": [{"kind": "text", "text": "hello"}]})
        );
    }
}
