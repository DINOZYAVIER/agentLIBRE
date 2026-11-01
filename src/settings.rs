//! The settings, `settings.nix` of the user and of the project's store
//! (D54-D60, D66, D148; AGL-039, D11, D12, D20, D21, D24): what the
//! machine has — where each kind of server listens, the engine, whisper,
//! the search service, the weights folders — and the human's interface.
//! The model of a run is its Function's (AGL-039, D21).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::error::Error;
use crate::i18n::Lang;
use crate::nix::{self, Section};
use crate::theme::{COLOR_KEYS, Role, Style};

/// Name of the settings file, the user's and the project store's
/// (AGL-039, D11).
pub const SETTINGS_FILE: &str = "settings.nix";

/// Function used when no settings name one (D39, D58; AGL-039, D32).
const DEFAULT_FUNCTION: &str = "agentLIBRE-gpu-high";

/// Where a Strata server listens when `server.strata.url` is not set
/// (AGL-030, D1; AGL-036, D6): its own run script binds port 18088
/// (8080 is held by qBittorrent).
pub const DEFAULT_STRATA_URL: &str = "http://127.0.0.1:18088";

/// The longest voice recording of the REPL's Ctrl-R, in seconds, when the
/// settings do not set `repl.max_record_seconds` (AGL-016, D5, D8;
/// AGL-039, D20).
const DEFAULT_MAX_RECORD_SECONDS: u32 = 60;

/// The kind of inference server (AGL-030, D1): the llama.cpp
/// `llama-server` with grammar-constrained decoding, or a Strata server
/// with no grammar, the state machine stirring the model instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ServerKind {
    #[default]
    Llama,
    Strata,
}

impl ServerKind {
    /// The two kinds, as the human sets them.
    pub fn parse(value: &str) -> Option<ServerKind> {
        match value {
            "llama" => Some(ServerKind::Llama),
            "strata" => Some(ServerKind::Strata),
            _ => None,
        }
    }

    /// The kind's name: how the human sets it and what the surfaces show.
    pub fn name(self) -> &'static str {
        match self {
            ServerKind::Llama => "llama",
            ServerKind::Strata => "strata",
        }
    }

    /// The server of the kind, in English (AGL-038, D1): what the plain
    /// rendering of an error names; the messages take `i18n::server::name`.
    pub fn server(self) -> &'static str {
        match self {
            ServerKind::Llama => "llama-server",
            ServerKind::Strata => "the Strata server",
        }
    }
}

/// The user file, `$XDG_CONFIG_HOME/agentLIBRE/settings.nix`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UserFile {
    pub function: Option<String>,
    pub language: Option<Lang>,
    /// `server`: where each kind listens and the engine; a project's own
    /// `server` wins per key (AGL-015, D4, D11).
    pub server: ServerFile,
    /// `whisper`: the defaults of the whisper server (AGL-014, D22).
    pub whisper: WhisperFile,
    /// `search`: the human's search service (AGL-019, D3).
    pub search: SearchFile,
    /// `colors` (D121).
    pub colors: Vec<(Role, Style)>,
    /// `repl` (D124, D130; AGL-039, D20).
    pub repl: ReplFile,
    /// `forge` (AGL-007).
    pub forge: ForgeFile,
    /// `models`: the weights folders after agl's own (AGL-039, D24b),
    /// absolute paths, looked in in order.
    pub models: Vec<PathBuf>,
}

/// The project's settings, `settings.nix` of its store (AGL-039, D11):
/// the Function and the per-key overrides of the user's `server`,
/// `whisper` and `search`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectFile {
    pub function: Option<String>,
    pub server: ServerFile,
    pub whisper: WhisperFile,
    pub search: SearchFile,
}

impl ProjectFile {
    /// Loads the project's `path`; a missing file has no settings.
    pub fn load(path: &Path) -> Result<ProjectFile, Error> {
        let Some(mut file) = evaluate(path)? else {
            return Ok(ProjectFile::default());
        };
        // The identity is the marker's until AGL-041 moves it here.
        file.check_keys(&["function", "server", "whisper", "search"])?;
        Ok(ProjectFile {
            function: file.string("function")?,
            server: ServerFile::parse(file.section("server")?, path)?,
            whisper: WhisperFile::parse(file.section("whisper")?, path)?,
            search: SearchFile::parse(file.section("search")?, path)?,
        })
    }
}

/// Evaluates the settings file `path`; `None` when it does not exist.
fn evaluate(path: &Path) -> Result<Option<Section>, Error> {
    if !path.exists() {
        return Ok(None);
    }
    Section::top(path, nix::eval_file(path)?).map(Some)
}

/// Reads the settings file `path` as a value and sets the top-level `key`
/// to `value`, then writes the file anew (AGL-039, D11, D12): what
/// `agl function use` and the weights question do. A missing file is
/// created.
pub fn set_top(path: &Path, key: &str, value: Value) -> Result<(), Error> {
    let mut map = if path.exists() {
        match nix::eval_file(path)? {
            Value::Object(map) => map,
            _ => {
                return Err(Error::WrongType {
                    path: path.into(),
                    key: String::new(),
                    expected: crate::error::Expected::Table,
                });
            }
        }
    } else {
        Map::new()
    };
    map.insert(key.into(), value);
    nix::write_settings(path, &map)
}

/// A weights file of the settings (the whisper model, AGL-039, D24a, D24e):
/// a file name or an absolute path.
fn check_weights_name(name: &str) -> Result<(), Error> {
    if Path::new(name).is_absolute() || !(name.is_empty() || name.contains('/') || name.starts_with('.')) {
        Ok(())
    } else {
        Err(Error::InvalidName(name.into()))
    }
}

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

/// `whisper` of a settings file, the user's or the project's
/// (AGL-014, D22): the whisper.cpp server that turns an audio into text
/// when the model does not hear it. A key that is not set is `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WhisperFile {
    pub url: Option<String>,
    /// A weights file (AGL-039, D24): a file name in the weights folders
    /// or an absolute path.
    pub model: Option<String>,
    /// More whisper-server arguments.
    pub args: Option<Vec<String>>,
}

impl WhisperFile {
    const KEYS: &'static [&'static str] = &["url", "model", "args"];

    pub fn parse(section: Option<Section>, path: &Path) -> Result<WhisperFile, Error> {
        let Some(mut whisper) = section else {
            return Ok(WhisperFile::default());
        };
        whisper.check_keys(Self::KEYS)?;
        let url = whisper.string("url")?;
        if let Some(url) = &url
            && !url.starts_with("http://")
        {
            return Err(Error::UnsupportedUrl {
                path: path.into(),
                url: url.clone(),
            });
        }
        let model = whisper.string("model")?;
        if let Some(model) = &model {
            check_weights_name(model)?;
        }
        Ok(WhisperFile {
            url,
            model,
            args: whisper.strings("args")?,
        })
    }

    pub fn is_empty(&self) -> bool {
        *self == WhisperFile::default()
    }

    /// The keys set here, the others from `defaults`; `args` is replaced
    /// as a whole (AGL-015, D11).
    pub fn over(&self, defaults: &WhisperFile) -> WhisperFile {
        WhisperFile {
            url: self.url.clone().or_else(|| defaults.url.clone()),
            model: self.model.clone().or_else(|| defaults.model.clone()),
            args: self.args.clone().or_else(|| defaults.args.clone()),
        }
    }
}

/// `[search]` of a settings file, the user's or the project's
/// (AGL-019, D3): the search service and the credentials of its mutual
/// TLS. A key that is not set is `None`; the table is usable when the
/// merged one sets all four (D5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchFile {
    /// An `https://` address (D3).
    pub url: Option<String>,
    /// Absolute paths of the client certificate, its private key (PKCS#8
    /// PEM) and the private CA bundle (D3, D4).
    pub client_certificate: Option<String>,
    pub client_private_key: Option<String>,
    pub private_ca: Option<String>,
}

/// A complete `[search]`: the search is set up (AGL-019, D5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Search {
    pub url: String,
    pub client_certificate: PathBuf,
    pub client_private_key: PathBuf,
    pub private_ca: PathBuf,
}

impl SearchFile {
    const KEYS: &'static [&'static str] =
        &["url", "client_certificate", "client_private_key", "private_ca"];

    pub fn parse(section: Option<Section>, path: &Path) -> Result<SearchFile, Error> {
        let Some(mut search) = section else {
            return Ok(SearchFile::default());
        };
        search.check_keys(Self::KEYS)?;
        let url = search.string("url")?;
        if let Some(url) = &url
            && !url.starts_with("https://")
        {
            return Err(Error::UnsupportedSearchUrl {
                path: path.into(),
                url: url.clone(),
            });
        }
        let mut file = |key: &str| -> Result<Option<String>, Error> {
            let value = search.string(key)?;
            if let Some(value) = &value
                && !Path::new(value).is_absolute()
            {
                return Err(Error::SearchPathNotAbsolute {
                    path: path.into(),
                    key: search.key(key),
                    value: value.clone(),
                });
            }
            Ok(value)
        };
        Ok(SearchFile {
            url,
            client_certificate: file("client_certificate")?,
            client_private_key: file("client_private_key")?,
            private_ca: file("private_ca")?,
        })
    }

    pub fn is_empty(&self) -> bool {
        *self == SearchFile::default()
    }

    /// The keys set here, the others from `defaults`.
    pub fn over(&self, defaults: &SearchFile) -> SearchFile {
        SearchFile {
            url: self.url.clone().or_else(|| defaults.url.clone()),
            client_certificate: self
                .client_certificate
                .clone()
                .or_else(|| defaults.client_certificate.clone()),
            client_private_key: self
                .client_private_key
                .clone()
                .or_else(|| defaults.client_private_key.clone()),
            private_ca: self.private_ca.clone().or_else(|| defaults.private_ca.clone()),
        }
    }

    /// The search when all four keys are set; `Ok(None)` when none is
    /// (no search); the first missing key's name when some are (D5).
    pub fn complete(&self) -> Result<Option<Search>, &'static str> {
        if self.is_empty() {
            return Ok(None);
        }
        Ok(Some(Search {
            url: self.url.clone().ok_or("url")?,
            client_certificate: self
                .client_certificate
                .clone()
                .ok_or("client_certificate")?
                .into(),
            client_private_key: self
                .client_private_key
                .clone()
                .ok_or("client_private_key")?
                .into(),
            private_ca: self.private_ca.clone().ok_or("private_ca")?.into(),
        }))
    }
}

/// `image_max_tokens` when the model does not set it (AGL-014, D23).
pub const DEFAULT_IMAGE_TOKENS: u32 = 2048;

/// The `url` of the set of a kind, an `http://` URL (AGL-036, D2);
/// `None` when absent.
fn read_url(section: &mut Section, path: &Path) -> Result<Option<String>, Error> {
    let url = section.string("url")?;
    if let Some(url) = &url
        && !url.starts_with("http://")
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

/// `[repl]`: the limits of tool cards and their frame (D130).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReplFile {
    pub tool_lines: Option<usize>,
    pub tool_chars: Option<usize>,
    pub frame: Option<bool>,
    /// The longest voice recording, in seconds (AGL-039, D20).
    pub max_record_seconds: Option<u32>,
}

/// `[forge]` (AGL-007).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ForgeFile {
    /// Adopt the unique same-named store when the workspace is at an
    /// unregistered location (default on).
    pub auto_rehome: Option<bool>,
}

/// `[repl]` with the defaults of the old agl (D130).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplSettings {
    pub tool_lines: usize,
    pub tool_chars: usize,
    pub frame: bool,
    /// The longest voice recording of Ctrl-R, in seconds (AGL-016;
    /// AGL-039, D20).
    pub max_record_seconds: u32,
}

impl Default for ReplSettings {
    fn default() -> ReplSettings {
        ReplSettings {
            tool_lines: 10,
            tool_chars: 500,
            frame: true,
            max_record_seconds: DEFAULT_MAX_RECORD_SECONDS,
        }
    }
}

impl UserFile {
    /// Loads `path`; a missing file has no settings.
    pub fn load(path: &Path) -> Result<UserFile, Error> {
        let Some(mut file) = evaluate(path)? else {
            return Ok(UserFile::default());
        };
        file.check_keys(&[
            "function", "language", "server", "whisper", "search", "colors", "repl", "forge",
            "models",
        ])?;
        let function = file.string("function")?;
        let language = file
            .string("language")?
            .map(|code| {
                Lang::from_code(&code).ok_or_else(|| Error::UnsupportedLanguage {
                    path: path.into(),
                    value: code,
                })
            })
            .transpose()?;
        let server = ServerFile::parse(file.section("server")?, path)?;
        let whisper = WhisperFile::parse(file.section("whisper")?, path)?;
        let search = SearchFile::parse(file.section("search")?, path)?;
        let colors = match file.section("colors")? {
            None => Vec::new(),
            Some(mut colors) => {
                colors.check_keys(COLOR_KEYS)?;
                let mut parsed = Vec::new();
                for (key, value) in colors.string_entries()? {
                    let role = Role::from_key(&key).expect("keys are checked");
                    let style = Style::parse(&value, role.is_background()).map_err(|problem| {
                        Error::InvalidStyle {
                            path: path.into(),
                            key: colors.key(&key),
                            value,
                            problem,
                        }
                    })?;
                    parsed.push((role, style));
                }
                parsed
            }
        };
        let repl = match file.section("repl")? {
            None => ReplFile::default(),
            Some(mut repl) => {
                repl.check_keys(&["tool_lines", "tool_chars", "frame", "max_record_seconds"])?;
                let mut count = |key: &str| -> Result<Option<usize>, Error> {
                    repl.integer(key)?
                        .map(|n| {
                            usize::try_from(n).ok().filter(|n| *n >= 1).ok_or_else(|| {
                                Error::NotPositive {
                                    path: path.into(),
                                    key: repl.key(key),
                                }
                            })
                        })
                        .transpose()
                };
                let tool_lines = count("tool_lines")?;
                let tool_chars = count("tool_chars")?;
                let max_record_seconds = count("max_record_seconds")?
                    .map(|n| u32::try_from(n).unwrap_or(u32::MAX));
                ReplFile {
                    tool_lines,
                    tool_chars,
                    frame: repl.bool("frame")?,
                    max_record_seconds,
                }
            }
        };
        let forge = match file.section("forge")? {
            None => ForgeFile::default(),
            Some(mut forge) => {
                forge.check_keys(&["auto_rehome"])?;
                ForgeFile {
                    auto_rehome: forge.bool("auto_rehome")?,
                }
            }
        };
        let models = file
            .strings("models")?
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        if let Some(folder) = models.iter().find(|folder| !folder.is_absolute()) {
            return Err(Error::SearchPathNotAbsolute {
                path: path.into(),
                key: "models".into(),
                value: folder.display().to_string(),
            });
        }
        Ok(UserFile {
            function,
            language,
            server,
            whisper,
            search,
            colors,
            repl,
            forge,
            models,
        })
    }
}

/// The name of the Function of the run: `--function`, then the project's
/// file, then the user's, then the default (D58, D59, AGL-015).
pub fn function_name(
    cli: Option<&str>,
    project: Option<&str>,
    user: Option<&str>,
) -> String {
    cli.or(project)
        .or(user)
        .unwrap_or(DEFAULT_FUNCTION)
        .to_owned()
}

/// Settings of a run.
#[derive(Debug)]
pub struct Settings {
    pub server_url: String,
    /// The effective server: the Function's model with the settings' url
    /// and engine (AGL-039, D21).
    pub server: ServerFile,
    /// The `whisper` of the project over the user's.
    pub whisper: WhisperFile,
    /// The `search` of the project over the user's, when it is
    /// complete; the tool `search` exists only then (AGL-019, D5).
    pub search: Option<Search>,
    pub function: String,
    pub colors: Vec<(Role, Style)>,
    pub repl: ReplSettings,
}

impl Settings {
    /// Combines the user file, the project's (`project_path` is its file,
    /// for hints), `--function` (D58, D59, AGL-015) and the model of the
    /// loaded Function (AGL-039, D21).
    pub fn resolve(
        user: &UserFile,
        user_path: &Path,
        project: (&Path, &ProjectFile),
        cli_function: Option<&str>,
        model: &ServerFile,
    ) -> Result<Settings, Error> {
        let (project_path, project) = project;
        let server = project.server.over(&user.server).effective(model);
        let whisper = project.whisper.over(&user.whisper);
        let search_file = project.search.over(&user.search);
        let search = search_file.complete().map_err(|key| Error::MissingKey {
            // The file that has a `search`, the project's first.
            path: if project.search.is_empty() {
                user_path.into()
            } else {
                project_path.into()
            },
            key: format!("search.{key}"),
        })?;
        // The url of the kind (AGL-036, D2, D6).
        let server_url = server.url().ok_or_else(|| Error::MissingServerUrl {
            user_path: user_path.into(),
            project_path: project_path.into(),
        })?;
        let function = function_name(cli_function, project.function.as_deref(), user.function.as_deref());
        let defaults = ReplSettings::default();
        Ok(Settings {
            server_url,
            server,
            whisper,
            search,
            function,
            colors: user.colors.clone(),
            repl: ReplSettings {
                tool_lines: user.repl.tool_lines.unwrap_or(defaults.tool_lines),
                tool_chars: user.repl.tool_chars.unwrap_or(defaults.tool_chars),
                frame: user.repl.frame.unwrap_or(defaults.frame),
                max_record_seconds: user
                    .repl
                    .max_record_seconds
                    .unwrap_or(defaults.max_record_seconds),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Expected;
    use crate::forge::testutil::TempDir;
    use std::fs;

    /// Writes `text` as a settings file and loads it.
    fn user(text: &str) -> Result<UserFile, Error> {
        let dir = TempDir::new();
        let path = dir.0.join(SETTINGS_FILE);
        fs::write(&path, text).unwrap();
        UserFile::load(&path)
    }

    /// A user file of the attributes `body`.
    fn user_of(body: &str) -> Result<UserFile, Error> {
        user(&format!("{{ agl, ... }}: {{ {body} }}"))
    }

    fn with_url() -> UserFile {
        UserFile {
            server: ServerFile {
                llama_url: Some("http://x".into()),
                ..ServerFile::default()
            },
            ..UserFile::default()
        }
    }

    /// The model of a llama Function.
    fn llama_model() -> ServerFile {
        ServerFile {
            kind: Some("llama".into()),
            model: Some("m.gguf".into()),
            context: Some(8192),
            ..ServerFile::default()
        }
    }

    fn resolve_with(user: &UserFile, project: &ProjectFile, cli: Option<&str>, model: &ServerFile) -> Result<Settings, Error> {
        Settings::resolve(
            user,
            Path::new("/c/settings.nix"),
            (Path::new("/d/settings.nix"), project),
            cli,
            model,
        )
    }

    #[test]
    fn user_file() {
        let loaded = user_of(
            r#"function = "coder"; language = "uk";
               server = { engine = "vulkan-server"; llama.url = "http://127.0.0.1:18080"; strata.url = "http://127.0.0.1:18088"; };
               models = [ "/mnt/gguf" ];"#,
        )
        .unwrap();
        assert_eq!(
            loaded,
            UserFile {
                function: Some("coder".into()),
                language: Some(Lang::Uk),
                server: ServerFile {
                    llama_url: Some("http://127.0.0.1:18080".into()),
                    strata_url: Some("http://127.0.0.1:18088".into()),
                    engine: Some("vulkan-server".into()),
                    ..ServerFile::default()
                },
                models: vec![PathBuf::from("/mnt/gguf")],
                ..UserFile::default()
            }
        );
        // The model keys are the Function's (AGL-039, D21).
        for key in ["model", "context", "kind"] {
            assert!(
                matches!(user_of(&format!("server.llama.{key} = 1;")), Err(Error::UnknownKey { .. })),
                "{key}"
            );
        }
        assert!(matches!(user_of("server.kind = \"llama\";"), Err(Error::UnknownKey { key, .. }) if key == "server.kind"));
        // The weights folders are absolute (D24b).
        assert!(matches!(user_of(r#"models = [ "gguf" ];"#), Err(Error::SearchPathNotAbsolute { .. })));
        // A settings file is a function of `{ agl, ... }` (D8, D12).
        assert!(matches!(user("{ function = \"x\"; }"), Err(Error::NixNotAFunction { .. })));
        assert!(matches!(user("{ agl, ... }: { function = ; }"), Err(Error::NixEval { .. })));
    }

    #[test]
    fn the_whisper_table() {
        let loaded = user_of(r#"whisper = { url = "http://127.0.0.1:18082"; model = "w.bin"; args = [ "-t" "12" ]; };"#)
            .unwrap()
            .whisper;
        assert_eq!(
            loaded,
            WhisperFile {
                url: Some("http://127.0.0.1:18082".into()),
                model: Some("w.bin".into()),
                args: Some(vec!["-t".into(), "12".into()]),
            }
        );
        // The project wins per key, `args` as a whole.
        let project = ProjectFile {
            whisper: WhisperFile {
                model: Some("small.bin".into()),
                args: Some(Vec::new()),
                ..WhisperFile::default()
            },
            ..ProjectFile::default()
        };
        let settings = resolve_with(
            &UserFile { whisper: loaded.clone(), ..with_url() },
            &project,
            None,
            &llama_model(),
        )
        .unwrap();
        assert_eq!(settings.whisper.url, loaded.url);
        assert_eq!(settings.whisper.model.as_deref(), Some("small.bin"));
        assert_eq!(settings.whisper.args, Some(Vec::new()));
        assert!(matches!(
            user_of("whisper.port = 1;"),
            Err(Error::UnknownKey { key, .. }) if key == "whisper.port"
        ));
        assert!(matches!(user_of(r#"whisper.url = "https://x";"#), Err(Error::UnsupportedUrl { .. })));
        // The model is a file name or an absolute path (AGL-039, D24a).
        assert!(user_of(r#"whisper.model = "/w/w.bin";"#).is_ok());
        assert!(matches!(user_of(r#"whisper.model = "a/w.bin";"#), Err(Error::InvalidName(_))));
    }

    fn full_search() -> SearchFile {
        SearchFile {
            url: Some("https://search.example/v1/search".into()),
            client_certificate: Some("/c/client.crt".into()),
            client_private_key: Some("/c/client.pk8".into()),
            private_ca: Some("/c/ca.crt".into()),
        }
    }

    #[test]
    fn the_search_table() {
        let loaded = user_of(
            r#"search = {
                 url = "https://search.example/v1/search";
                 client_certificate = "/c/client.crt";
                 client_private_key = "/c/client.pk8";
                 private_ca = "/c/ca.crt";
               };"#,
        )
        .unwrap()
        .search;
        assert_eq!(loaded, full_search());
        assert!(matches!(
            user_of("search.port = 1;"),
            Err(Error::UnknownKey { key, .. }) if key == "search.port"
        ));
        assert!(matches!(
            user_of(r#"search.url = "http://x";"#),
            Err(Error::UnsupportedSearchUrl { url, .. }) if url == "http://x"
        ));
        assert!(matches!(
            user_of(r#"search.private_ca = "ca.crt";"#),
            Err(Error::SearchPathNotAbsolute { key, value, .. })
                if key == "search.private_ca" && value == "ca.crt"
        ));
    }

    #[test]
    fn the_search_is_set_up_only_by_a_complete_table() {
        let resolve = |user_search: SearchFile, project_search: SearchFile| {
            resolve_with(
                &UserFile { search: user_search, ..with_url() },
                &ProjectFile { search: project_search, ..ProjectFile::default() },
                None,
                &llama_model(),
            )
        };
        // None: no search, no error.
        assert_eq!(resolve(SearchFile::default(), SearchFile::default()).unwrap().search, None);
        // All four: the search.
        let search = resolve(full_search(), SearchFile::default()).unwrap().search.unwrap();
        assert_eq!(search.url, "https://search.example/v1/search");
        assert_eq!(search.private_ca, Path::new("/c/ca.crt"));
        // Some, as the user's: the first missing key, in the user's file.
        let partial = SearchFile {
            url: full_search().url,
            client_private_key: full_search().client_private_key,
            ..SearchFile::default()
        };
        assert!(matches!(
            resolve(partial.clone(), SearchFile::default()),
            Err(Error::MissingKey { path, key })
                if key == "search.client_certificate" && path == Path::new("/c/settings.nix")
        ));
        // The project completes the user's table per key...
        let rest = SearchFile {
            client_certificate: full_search().client_certificate,
            private_ca: full_search().private_ca,
            ..SearchFile::default()
        };
        assert!(resolve(partial, rest).unwrap().search.is_some());
        // ...and a project's own partial table names its own file.
        assert!(matches!(
            resolve(SearchFile::default(), SearchFile { url: full_search().url, ..SearchFile::default() }),
            Err(Error::MissingKey { path, key })
                if key == "search.client_certificate" && path == Path::new("/d/settings.nix")
        ));
    }

    #[test]
    fn the_effective_server_is_the_model_with_the_url_of_its_kind() {
        // AGL-039, D21: the kind and the model keys are the Function's, the
        // url of that kind and the engine are the settings'.
        let user = UserFile {
            server: ServerFile {
                llama_url: Some("http://127.0.0.1:18080".into()),
                strata_url: Some("http://127.0.0.1:19000".into()),
                engine: Some("e".into()),
                ..ServerFile::default()
            },
            ..UserFile::default()
        };
        let llama = resolve_with(&user, &ProjectFile::default(), None, &llama_model()).unwrap();
        assert_eq!(llama.server_url, "http://127.0.0.1:18080");
        assert_eq!(
            llama.server,
            ServerFile {
                kind: Some("llama".into()),
                llama_url: Some("http://127.0.0.1:18080".into()),
                engine: Some("e".into()),
                ..llama_model()
            }
        );
        let strata_model = ServerFile {
            kind: Some("strata".into()),
            strata_config: Some("/s.json".into()),
            ..ServerFile::default()
        };
        let strata = resolve_with(&user, &ProjectFile::default(), None, &strata_model).unwrap();
        assert_eq!(strata.server_url, "http://127.0.0.1:19000");
        assert_eq!(strata.server.llama_url, None);
        // The Strata server has a default address (AGL-036, D6); the
        // llama-server's is required.
        let strata = resolve_with(&with_url(), &ProjectFile::default(), None, &strata_model).unwrap();
        assert_eq!(strata.server_url, DEFAULT_STRATA_URL);
        assert!(matches!(
            resolve_with(&UserFile::default(), &ProjectFile::default(), None, &llama_model()),
            Err(Error::MissingServerUrl { .. })
        ));
        // The project's url wins per key.
        let project = ProjectFile {
            server: ServerFile { llama_url: Some("http://p".into()), ..ServerFile::default() },
            ..ProjectFile::default()
        };
        assert_eq!(resolve_with(&user, &project, None, &llama_model()).unwrap().server_url, "http://p");
    }

    #[test]
    fn colors_repl_and_forge_tables() {
        let loaded = user_of(
            r##"colors = { muted = "italic #010203"; input_background = "#000000"; };
                repl = { tool_lines = 3; frame = false; max_record_seconds = 15; };
                forge.auto_rehome = false;"##,
        )
        .unwrap();
        assert_eq!(loaded.colors.len(), 2);
        assert_eq!(loaded.colors[0].0, Role::InputBackground);
        assert_eq!(loaded.colors[1].0, Role::Muted);
        assert_eq!(
            loaded.repl,
            ReplFile {
                tool_lines: Some(3),
                tool_chars: None,
                frame: Some(false),
                max_record_seconds: Some(15),
            }
        );
        assert_eq!(loaded.forge, ForgeFile { auto_rehome: Some(false) });
        let settings = resolve_with(
            &UserFile { server: with_url().server, ..user_of(r#"repl = { tool_lines = 3; frame = false; max_record_seconds = 15; };"#).unwrap() },
            &ProjectFile::default(),
            None,
            &llama_model(),
        )
        .unwrap();
        assert_eq!(
            settings.repl,
            ReplSettings {
                tool_lines: 3,
                tool_chars: 500,
                frame: false,
                max_record_seconds: 15,
            }
        );
        // The voice limit is 60 s unless set (AGL-016, D5; AGL-039, D20).
        let plain = resolve_with(&with_url(), &ProjectFile::default(), None, &llama_model()).unwrap();
        assert_eq!(plain.repl.max_record_seconds, 60);
    }

    #[test]
    fn invalid_values_are_errors() {
        assert!(matches!(
            user_of(r#"colors.muted = "red";"#),
            Err(Error::InvalidStyle { key, .. }) if key == "colors.muted"
        ));
        assert!(matches!(
            user_of(r#"colors.mutd = "dim";"#),
            Err(Error::UnknownKey { key, .. }) if key == "colors.mutd"
        ));
        for (body, key) in [
            ("repl.tool_lines = 0;", "repl.tool_lines"),
            ("repl.tool_chars = -5;", "repl.tool_chars"),
            ("repl.max_record_seconds = 0;", "repl.max_record_seconds"),
        ] {
            assert!(
                matches!(user_of(body), Err(Error::NotPositive { key: k, .. }) if k == key),
                "{body}"
            );
        }
        assert!(matches!(
            user_of("forge.auto_rehome = \"on\";"),
            Err(Error::WrongType { key, .. }) if key == "forge.auto_rehome"
        ));
        assert!(matches!(
            user_of(r#"functon = "coder";"#),
            Err(Error::UnknownKey { key, .. }) if key == "functon"
        ));
        assert!(matches!(
            user_of("function = 1;"),
            Err(Error::WrongType { key, expected: Expected::String, .. }) if key == "function"
        ));
        assert!(matches!(
            user_of(r#"server = "http://x";"#),
            Err(Error::WrongType { key, expected: Expected::Table, .. }) if key == "server"
        ));
        assert!(matches!(
            user_of(r#"language = "de";"#),
            Err(Error::UnsupportedLanguage { value, .. }) if value == "de"
        ));
        assert!(matches!(
            user_of(r#"server.strata.url = "https://x";"#),
            Err(Error::UnsupportedUrl { url, .. }) if url == "https://x"
        ));
    }

    #[test]
    fn missing_user_file_is_empty() {
        let dir = TempDir::new();
        let loaded = UserFile::load(&dir.0.join(SETTINGS_FILE)).unwrap();
        assert_eq!(loaded, UserFile::default());
    }

    #[test]
    fn the_project_file() {
        let dir = TempDir::new();
        let path = dir.0.join(SETTINGS_FILE);
        assert_eq!(ProjectFile::load(&path).unwrap(), ProjectFile::default());
        fs::write(&path, r#"{ agl, ... }: { function = "f"; server.llama.url = "http://p"; whisper.model = "w.bin"; }"#).unwrap();
        let project = ProjectFile::load(&path).unwrap();
        assert_eq!(project.function.as_deref(), Some("f"));
        assert_eq!(project.server.llama_url.as_deref(), Some("http://p"));
        assert_eq!(project.whisper.model.as_deref(), Some("w.bin"));
        // The interface and the weights folders are the user's.
        fs::write(&path, "{ agl, ... }: { language = \"uk\"; }").unwrap();
        assert!(matches!(ProjectFile::load(&path), Err(Error::UnknownKey { key, .. }) if key == "language"));
    }

    #[test]
    fn set_top_writes_the_file_anew_and_keeps_the_values() {
        // AGL-039, D11, D12.
        let dir = TempDir::new();
        let path = dir.0.join("config/settings.nix");
        set_top(&path, "function", Value::String("a".into())).unwrap();
        assert_eq!(UserFile::load(&path).unwrap().function.as_deref(), Some("a"));
        fs::write(
            &path,
            "# a comment\n{ agl, ... }: let url = \"http://x\"; in { function = \"a\"; server.llama.url = url; }\n",
        )
        .unwrap();
        set_top(&path, "function", Value::String("b".into())).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{ agl, ... }: {\n"), "{text}");
        assert!(!text.contains("# a comment") && !text.contains("let"));
        let loaded = UserFile::load(&path).unwrap();
        assert_eq!(loaded.function.as_deref(), Some("b"));
        assert_eq!(loaded.server.llama_url.as_deref(), Some("http://x"));
    }

    #[test]
    fn function_precedence() {
        let user = UserFile { function: Some("user".into()), ..with_url() };
        let resolve = |user: &UserFile, project: Option<&str>, cli| {
            resolve_with(
                user,
                &ProjectFile { function: project.map(str::to_owned), ..ProjectFile::default() },
                cli,
                &llama_model(),
            )
            .unwrap()
            .function
        };
        assert_eq!(resolve(&user, Some("project"), Some("cli")), "cli");
        assert_eq!(resolve(&user, Some("project"), None), "project");
        assert_eq!(resolve(&user, None, None), "user");
        // The default (AGL-039, D32).
        assert_eq!(resolve(&with_url(), None, None), "agentLIBRE-gpu-high");
    }

    #[test]
    fn the_record_round_trips() {
        let server = ServerFile {
            kind: Some("llama".into()),
            llama_url: Some("http://x".into()),
            model: Some("/m/m.gguf".into()),
            context: Some(131072),
            args: Some(vec!["-ngl".into(), "999".into()]),
            mmproj_offload: Some(false),
            ..ServerFile::default()
        };
        assert_eq!(ServerFile::from_json(&server.to_json()), Some(server));
        assert_eq!(ServerFile::from_json(&json!({ "kind": 1 })), None);
    }
}
