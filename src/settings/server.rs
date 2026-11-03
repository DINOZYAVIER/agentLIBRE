//! `server` of a settings file: where each kind listens, the engine, and
//! the model side an effective server carries (AGL-015; AGL-036; AGL-039,
//! D21).

use std::path::Path;

use serde_json::{Value, json};

use super::ServerKind;
use crate::error::Error;
use crate::nix::Section;

/// Where a Strata server listens when `server.strata.url` is not set
/// (AGL-030, D1; AGL-036, D6): its own run script binds port 18088
/// (8080 is held by qBittorrent).
pub const DEFAULT_STRATA_URL: &str = "http://127.0.0.1:18088";

/// `server` of a settings file, the user's or the project's (AGL-015;
/// AGL-039, D21): where each kind listens and the engine. The model keys
/// are the Function's `model` (AGL-039, D17): an effective `ServerFile`
/// carries both, the run's whole server. A key that is not set is `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerFile {
    /// The kind of inference server (AGL-030, D1); `None` is `llama`.
    pub kind: Option<String>,
    /// Where the server of each kind listens: `server.llama.url` and
    /// `server.strata.url` (AGL-036, D2).
    pub llama_url: Option<String>,
    pub strata_url: Option<String>,
    /// The inference engine's binary name, found on `PATH` (AGL-024, D5);
    /// `None` is the default `llama-server`.
    pub engine: Option<String>,
    /// The weights, `model.weights` of the Function (AGL-039, D19): a file
    /// name in the weights folders or an absolute path (D24); after
    /// `weights::resolve`, the absolute path.
    pub model: Option<String>,
    pub context: Option<u32>,
    /// The type of the speculative decoding, the value of `--spec-type`
    /// (AGL-025, D2); `None` is no speculative decoding.
    pub spec_type: Option<String>,
    /// A draft model for the speculative decoding, a weights file
    /// (AGL-025, D2).
    pub draft: Option<String>,
    /// More llama-server arguments.
    pub args: Option<Vec<String>>,
    /// The multimodal projector, a weights file (AGL-014, D23).
    pub mmproj: Option<String>,
    /// Whether the projector runs on the GPU; true when unset (D9, D23).
    pub mmproj_offload: Option<bool>,
    /// The most tokens one image takes: passed to llama-server and the
    /// bound of the context check (D10, D23).
    pub image_max_tokens: Option<u32>,
    /// The path of the Strata config JSON, for the `strata` kind
    /// (AGL-033, D2): it describes the launch fully.
    pub strata_config: Option<String>,
}

/// `image_max_tokens` when the model does not set it (AGL-014, D23).
pub const DEFAULT_IMAGE_TOKENS: u32 = 2048;

/// Whether `url` is an `http://` or an `https://` address: the schemes of
/// a server and of whisper (AGL-036, D2; AGL-043 F10).
pub(super) fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// The `url` of the set of a kind, an `http://` or `https://` URL
/// (AGL-036, D2; AGL-043 F10); `None` when absent.
fn read_url(section: &mut Section, path: &Path) -> Result<Option<String>, Error> {
    let url = section.string("url")?;
    if let Some(url) = &url
        && !is_http_url(url)
    {
        return Err(Error::UnsupportedUrl {
            path: path.into(),
            url: url.clone(),
        });
    }
    Ok(url)
}

impl ServerFile {
    /// The keys of `server` of a settings file (AGL-035, D4; AGL-039,
    /// D21): the engine and the set of each kind, which holds its `url`.
    const KEYS: &'static [&'static str] = &["engine", "llama", "strata"];

    /// Reads `server` of the settings file `path`.
    pub fn parse(section: Option<Section>, path: &Path) -> Result<ServerFile, Error> {
        let Some(mut server) = section else {
            return Ok(ServerFile::default());
        };
        server.check_keys(Self::KEYS)?;
        let engine = server.string("engine")?;
        let mut url_of = |kind: &str| -> Result<Option<String>, Error> {
            match server.section(kind)? {
                Some(mut table) => {
                    table.check_keys(&["url"])?;
                    read_url(&mut table, path)
                }
                None => Ok(None),
            }
        };
        let llama_url = url_of("llama")?;
        let strata_url = url_of("strata")?;
        Ok(ServerFile {
            llama_url,
            strata_url,
            engine,
            ..ServerFile::default()
        })
    }

    /// Whether no key is set.
    pub fn is_empty(&self) -> bool {
        *self == ServerFile::default()
    }

    /// The kind of the inference server: `llama` when `kind` is not set
    /// (AGL-030, D1).
    pub fn kind(&self) -> ServerKind {
        self.kind.as_deref().and_then(ServerKind::parse).unwrap_or_default()
    }

    /// Where the server of the kind listens (AGL-036, D2, D6): the `url` of
    /// the kind's set; the Strata server has a default address, the
    /// llama-server's is required (`None`).
    pub fn url(&self) -> Option<String> {
        match self.kind() {
            ServerKind::Llama => self.llama_url.clone(),
            ServerKind::Strata => Some(
                self.strata_url
                    .clone()
                    .unwrap_or_else(|| DEFAULT_STRATA_URL.to_owned()),
            ),
        }
    }

    /// The effective server (AGL-033, D1; AGL-036, D1; AGL-039, D21): the
    /// kind and the model keys of `model` (the Function's, checked against
    /// its kind when it was read), the url of that kind and `engine` of the
    /// settings (`self`).
    pub fn effective(&self, model: &ServerFile) -> ServerFile {
        let kind = model.kind();
        ServerFile {
            kind: Some(kind.name().to_owned()),
            llama_url: (kind == ServerKind::Llama)
                .then(|| self.llama_url.clone())
                .flatten(),
            strata_url: (kind == ServerKind::Strata)
                .then(|| self.strata_url.clone())
                .flatten(),
            engine: self.engine.clone(),
            ..model.clone()
        }
    }

    /// Whether the model side of `self` deviates from the model side of
    /// `other` (AGL-033, D2): any of the model-side keys. The check of
    /// D2 uses it: the settings' value is the only known state of a
    /// running server (the context is not observable).
    pub fn model_side_differs(&self, other: &ServerFile) -> bool {
        self.model != other.model
            || self.context != other.context
            || self.spec_type != other.spec_type
            || self.draft != other.draft
            || self.args != other.args
            || self.mmproj != other.mmproj
            || self.mmproj_offload != other.mmproj_offload
            || self.image_max_tokens != other.image_max_tokens
            || self.strata_config != other.strata_config
    }

    /// The keys set here, the others from `defaults`; `args` is replaced
    /// as a whole, not merged (AGL-015, D11).
    pub fn over(&self, defaults: &ServerFile) -> ServerFile {
        ServerFile {
            kind: self.kind.clone().or_else(|| defaults.kind.clone()),
            llama_url: self.llama_url.clone().or_else(|| defaults.llama_url.clone()),
            strata_url: self.strata_url.clone().or_else(|| defaults.strata_url.clone()),
            engine: self.engine.clone().or_else(|| defaults.engine.clone()),
            model: self.model.clone().or_else(|| defaults.model.clone()),
            context: self.context.or(defaults.context),
            spec_type: self
                .spec_type
                .clone()
                .or_else(|| defaults.spec_type.clone()),
            draft: self.draft.clone().or_else(|| defaults.draft.clone()),
            args: self.args.clone().or_else(|| defaults.args.clone()),
            mmproj: self.mmproj.clone().or_else(|| defaults.mmproj.clone()),
            mmproj_offload: self.mmproj_offload.or(defaults.mmproj_offload),
            image_max_tokens: self.image_max_tokens.or(defaults.image_max_tokens),
            strata_config: self
                .strata_config
                .clone()
                .or_else(|| defaults.strata_config.clone()),
        }
    }

    /// The server as the JSON of the record of a running server (AGL-035,
    /// D1; AGL-041, D2, done here because the model keys left the
    /// settings' format): every key, `null` when not set.
    pub fn to_json(&self) -> Value {
        json!({
            "kind": self.kind,
            "llama_url": self.llama_url,
            "strata_url": self.strata_url,
            "engine": self.engine,
            "model": self.model,
            "context": self.context,
            "spec_type": self.spec_type,
            "draft": self.draft,
            "args": self.args,
            "mmproj": self.mmproj,
            "mmproj_offload": self.mmproj_offload,
            "image_max_tokens": self.image_max_tokens,
            "strata_config": self.strata_config,
        })
    }

    /// The server of a record written by [`ServerFile::to_json`]; `None`
    /// when it is not one.
    pub fn from_json(value: &Value) -> Option<ServerFile> {
        let text = |key: &str| match value.get(key)? {
            Value::Null => Some(None),
            Value::String(text) => Some(Some(text.clone())),
            _ => None,
        };
        let number = |key: &str| match value.get(key)? {
            Value::Null => Some(None),
            value => value.as_u64().and_then(|n| u32::try_from(n).ok()).map(Some),
        };
        let args = match value.get("args")? {
            Value::Null => None,
            Value::Array(items) => Some(
                items
                    .iter()
                    .map(|item| item.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()?,
            ),
            _ => return None,
        };
        let offload = match value.get("mmproj_offload")? {
            Value::Null => None,
            Value::Bool(value) => Some(*value),
            _ => return None,
        };
        Some(ServerFile {
            kind: text("kind")?,
            llama_url: text("llama_url")?,
            strata_url: text("strata_url")?,
            engine: text("engine")?,
            model: text("model")?,
            context: number("context")?,
            spec_type: text("spec_type")?,
            draft: text("draft")?,
            args,
            mmproj: text("mmproj")?,
            mmproj_offload: offload,
            image_max_tokens: number("image_max_tokens")?,
            strata_config: text("strata_config")?,
        })
    }
}
