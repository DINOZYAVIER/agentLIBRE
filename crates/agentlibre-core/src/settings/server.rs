//! `server` of a settings file: where each kind listens, the engine, and
//! the model side an effective server carries (AGL-015; AGL-036; AGL-039,
//! D21).

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};

use super::ServerKind;
use crate::error::Error;
use crate::nix::Section;

/// Where the llama-server listens when `server.llama.url` is not set (AGL-056, D5).
pub const DEFAULT_LLAMA_URL: &str = "http://127.0.0.1:18080";

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
    /// `server.llama.external`: the llama-server at the url runs outside agl's
    /// care, as on the host of a virtual machine (AGL-052, D4).
    pub llama_external: Option<bool>,
    /// `server.llama.private_ca`: the absolute path of the certificate of the
    /// CA that signs an `https://` server; `None` is the bundled roots (AGL-058,
    /// D12).
    pub llama_private_ca: Option<String>,
    /// `server.strata.private_ca`: the same for a Strata server (AGL-121, D2).
    pub strata_private_ca: Option<String>,
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
    /// The Strata engine of the `strata` kind, a variant of
    /// `programs.agl.strata`: `upstream`, `rdna3` (AGL-089, D5, D20).
    pub strata: Option<String>,
    /// The model's name of the `strata` kind: the server's `model_name` and
    /// the folder of its pack (AGL-089, D5).
    pub strata_name: Option<String>,
    /// The GGUF shards of the `strata` kind, the first one first: file names
    /// found as weights are (D24); after `weights::resolve`, absolute paths
    /// (AGL-089, D4).
    pub gguf: Option<Vec<String>>,
    /// The engine's environment of the `strata` kind (AGL-089, D5).
    pub env: Option<BTreeMap<String, String>>,
    /// `server.idle_stop`: the seconds without a turn after which the
    /// server stops (AGL-120, D2); `0` or unset means never.
    pub idle_stop: Option<u64>,
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

/// `private_ca` of a set, an absolute path (AGL-058, D12); `None` when absent.
pub(super) fn read_private_ca(section: &mut Section) -> Result<Option<String>, Error> {
    let ca = section.string("private_ca")?;
    if let Some(ca) = &ca
        && !Path::new(ca).is_absolute()
    {
        return Err(Error::ServerCa {
            path: ca.into(),
            detail: "it is not an absolute path".into(),
        });
    }
    Ok(ca)
}

impl ServerFile {
    /// The keys of `server` of a settings file (AGL-035, D4; AGL-039,
    /// D21): the engine and the set of each kind, which holds its `url`.
    const KEYS: &'static [&'static str] = &["engine", "llama", "strata", "idle_stop"];

    /// Reads `server` of the settings file `path`.
    pub fn parse(section: Option<Section>, path: &Path) -> Result<ServerFile, Error> {
        let Some(mut server) = section else {
            return Ok(ServerFile::default());
        };
        server.check_keys(Self::KEYS)?;
        let engine = server.string("engine")?;
        let idle_stop = server.integer("idle_stop")?.map(|seconds| {
            u64::try_from(seconds).unwrap_or(0)
        });
        let (llama_url, llama_external, llama_private_ca) = match server.section("llama")? {
            Some(mut table) => {
                table.check_keys(&["url", "external", "private_ca"])?;
                (
                    read_url(&mut table, path)?,
                    table.bool("external")?,
                    read_private_ca(&mut table)?,
                )
            }
            None => (None, None, None),
        };
        let (strata_url, strata_private_ca) = match server.section("strata")? {
            Some(mut table) => {
                table.check_keys(&["url", "private_ca"])?;
                (read_url(&mut table, path)?, read_private_ca(&mut table)?)
            }
            None => (None, None),
        };
        Ok(ServerFile {
            llama_url,
            strata_url,
            llama_external,
            llama_private_ca,
            strata_private_ca,
            engine,
            idle_stop,
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

    /// Whether agl leaves the server alone and needs none of its files
    /// (AGL-052, D4): the llama kind only.
    pub fn external(&self) -> bool {
        self.kind() == ServerKind::Llama && self.llama_external == Some(true)
    }

    /// The CA the server of the kind is trusted by: `private_ca` of its set
    /// (AGL-058, D12; AGL-121, D2); `None` is the bundled roots.
    pub fn private_ca(&self) -> Option<&str> {
        match self.kind() {
            ServerKind::Llama => self.llama_private_ca.as_deref(),
            ServerKind::Strata => self.strata_private_ca.as_deref(),
        }
    }

    /// Where the server of the kind listens (AGL-036, D2, D6; AGL-056, D5): the
    /// `url` of the kind's set, else the kind's default address.
    pub fn url(&self) -> String {
        match self.kind() {
            ServerKind::Llama => self.llama_url.as_deref().unwrap_or(DEFAULT_LLAMA_URL),
            ServerKind::Strata => self.strata_url.as_deref().unwrap_or(DEFAULT_STRATA_URL),
        }
        .to_owned()
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
            llama_external: (kind == ServerKind::Llama).then_some(self.llama_external).flatten(),
            llama_private_ca: (kind == ServerKind::Llama)
                .then(|| self.llama_private_ca.clone())
                .flatten(),
            strata_private_ca: (kind == ServerKind::Strata)
                .then(|| self.strata_private_ca.clone())
                .flatten(),
            engine: self.engine.clone(),
            idle_stop: self.idle_stop,
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
            || self.strata != other.strata
            || self.strata_name != other.strata_name
            || self.gguf != other.gguf
            || self.env != other.env
    }

    /// The keys set here, the others from `defaults`; `args` is replaced
    /// as a whole, not merged (AGL-015, D11).
    pub fn over(&self, defaults: &ServerFile) -> ServerFile {
        ServerFile {
            kind: self.kind.clone().or_else(|| defaults.kind.clone()),
            llama_url: self.llama_url.clone().or_else(|| defaults.llama_url.clone()),
            strata_url: self.strata_url.clone().or_else(|| defaults.strata_url.clone()),
            llama_external: self.llama_external.or(defaults.llama_external),
            llama_private_ca: self
                .llama_private_ca
                .clone()
                .or_else(|| defaults.llama_private_ca.clone()),
            strata_private_ca: self
                .strata_private_ca
                .clone()
                .or_else(|| defaults.strata_private_ca.clone()),
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
            strata: self.strata.clone().or_else(|| defaults.strata.clone()),
            strata_name: self.strata_name.clone().or_else(|| defaults.strata_name.clone()),
            gguf: self.gguf.clone().or_else(|| defaults.gguf.clone()),
            env: self.env.clone().or_else(|| defaults.env.clone()),
            idle_stop: self.idle_stop.or(defaults.idle_stop),
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
            "llama_external": self.llama_external,
            "engine": self.engine,
            "model": self.model,
            "context": self.context,
            "spec_type": self.spec_type,
            "draft": self.draft,
            "args": self.args,
            "mmproj": self.mmproj,
            "mmproj_offload": self.mmproj_offload,
            "image_max_tokens": self.image_max_tokens,
            "strata": self.strata,
            "strata_name": self.strata_name,
            "gguf": self.gguf,
            "env": self.env,
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
        let list = |key: &str| match value.get(key)? {
            Value::Null => Some(None),
            Value::Array(items) => items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .map(Some),
            _ => None,
        };
        let args = list("args")?;
        let gguf = list("gguf")?;
        let env = match value.get("env")? {
            Value::Null => None,
            Value::Object(map) => Some(
                map.iter()
                    .map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .collect::<Option<BTreeMap<_, _>>>()?,
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
            llama_external: value.get("llama_external").and_then(Value::as_bool),
            llama_private_ca: None,
            strata_private_ca: None,
            engine: text("engine")?,
            model: text("model")?,
            context: number("context")?,
            spec_type: text("spec_type")?,
            draft: text("draft")?,
            args,
            mmproj: text("mmproj")?,
            mmproj_offload: offload,
            image_max_tokens: number("image_max_tokens")?,
            strata: text("strata")?,
            strata_name: text("strata_name")?,
            gguf,
            env,
            // Operational, not part of what a server serves: not recorded.
            idle_stop: None,
        })
    }
}
