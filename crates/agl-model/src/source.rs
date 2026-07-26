use std::path::Component;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HfSourceKind {
    Repository,
    Tree,
    File,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HfSource {
    pub repository: String,
    pub revision: Option<String>,
    pub file: Option<String>,
    pub kind: HfSourceKind,
}

impl HfSource {
    pub fn parse(value: &str) -> Result<Self> {
        let url = Url::parse(value)?;
        ensure!(url.scheme() == "https", "Hugging Face URL must use HTTPS");
        ensure!(
            url.host_str() == Some("huggingface.co"),
            "model source host must be huggingface.co"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "credentials are not allowed in Hugging Face URLs"
        );
        ensure!(
            url.query().is_none() && url.fragment().is_none(),
            "Hugging Face model URL cannot contain a query or fragment"
        );
        let segments = url
            .path_segments()
            .expect("HTTPS URL has path segments")
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        ensure!(
            segments.len() >= 2,
            "Hugging Face URL must identify OWNER/REPO"
        );
        let owner = percent_decode_segment(segments[0])?;
        let name = percent_decode_segment(segments[1])?;
        ensure_repository_part(&owner)?;
        ensure_repository_part(&name)?;
        let repository = format!("{owner}/{name}");
        if segments.len() == 2 {
            return Ok(Self {
                repository,
                revision: None,
                file: None,
                kind: HfSourceKind::Repository,
            });
        }
        ensure!(
            segments.len() >= 4,
            "Hugging Face URL must use tree/REVISION or blob|resolve/REVISION/PATH"
        );
        let revision = percent_decode_segment(segments[3])?;
        ensure!(
            !revision.is_empty()
                && revision != "."
                && revision != ".."
                && !revision
                    .chars()
                    .any(|character| character.is_control() || character.is_whitespace())
                && !revision.contains('/')
                && !revision.contains('\\'),
            "Hugging Face revision is invalid"
        );
        match segments[2] {
            "tree" => {
                ensure!(
                    segments.len() == 4,
                    "tree URL must identify one repository revision"
                );
                Ok(Self {
                    repository,
                    revision: Some(revision),
                    file: None,
                    kind: HfSourceKind::Tree,
                })
            }
            "blob" | "resolve" => {
                ensure!(
                    segments.len() >= 5,
                    "file URL must include a repository-relative path"
                );
                let file = segments[4..]
                    .iter()
                    .map(|segment| percent_decode_segment(segment))
                    .collect::<Result<Vec<_>>>()?
                    .join("/");
                ensure!(
                    file.to_ascii_lowercase().ends_with(".gguf"),
                    "selected Hugging Face file must be GGUF"
                );
                ensure!(
                    std::path::Path::new(&file)
                        .components()
                        .all(|component| matches!(component, Component::Normal(_))),
                    "selected Hugging Face file path is invalid"
                );
                Ok(Self {
                    repository,
                    revision: Some(revision),
                    file: Some(file),
                    kind: HfSourceKind::File,
                })
            }
            surface => bail!(
                "unsupported Hugging Face URL surface `{surface}`; use repository, tree, blob, or resolve"
            ),
        }
    }

    pub fn exact_file_url(&self) -> Option<String> {
        Some(format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repository,
            self.revision.as_deref()?,
            self.file.as_deref()?
        ))
    }
}

fn ensure_repository_part(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') }),
        "Hugging Face repository owner and name must use ASCII letters, digits, '-', '_', or '.'"
    );
    Ok(())
}

fn percent_decode_segment(segment: &str) -> Result<String> {
    let bytes = percent_decode(segment.as_bytes())?;
    String::from_utf8(bytes).map_err(Into::into)
}

fn percent_decode(value: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] != b'%' {
            output.push(value[index]);
            index += 1;
            continue;
        }
        ensure!(index + 2 < value.len(), "invalid URL percent encoding");
        let high = hex_value(value[index + 1])?;
        let low = hex_value(value[index + 2])?;
        output.push(high * 16 + low);
        index += 3;
    }
    Ok(output)
}

fn hex_value(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => bail!("invalid URL percent encoding"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_repository_and_file_urls() {
        let repository = HfSource::parse("https://huggingface.co/unsloth/model").unwrap();
        assert_eq!(repository.kind, HfSourceKind::Repository);
        assert_eq!(repository.repository, "unsloth/model");

        let file = HfSource::parse(
            "https://huggingface.co/unsloth/model/blob/0123456789abcdef/model.gguf",
        )
        .unwrap();
        assert_eq!(file.kind, HfSourceKind::File);
        assert_eq!(file.file.as_deref(), Some("model.gguf"));
        assert_eq!(
            file.exact_file_url().as_deref(),
            Some("https://huggingface.co/unsloth/model/resolve/0123456789abcdef/model.gguf")
        );
    }

    #[test]
    fn rejects_credentials_hosts_and_ambiguous_surfaces() {
        assert!(HfSource::parse("http://huggingface.co/a/b").is_err());
        assert!(HfSource::parse("https://token@huggingface.co/a/b").is_err());
        assert!(HfSource::parse("https://example.com/a/b").is_err());
        assert!(HfSource::parse("https://huggingface.co/a/b/commits/main").is_err());
        assert!(HfSource::parse("https://huggingface.co/a%2Fb/model").is_err());
        assert!(HfSource::parse("https://huggingface.co/a/b/blob/%2E%2E/model.gguf").is_err());
        assert!(HfSource::parse("https://huggingface.co/a/b/blob/main/%2E%2E/model.gguf").is_err());
    }
}
