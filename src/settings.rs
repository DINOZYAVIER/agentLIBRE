//! The user `agentLIBRE.toml` file (D54-D60, D66, D148); the project
//! settings live in the project's store file (AGL-010, `forge::Marker`).

use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::i18n::Lang;
use crate::theme::{COLOR_KEYS, Role, Style};
use crate::tomlfile::{self, Section};

/// Name of the user settings file and of the project's file in its store.
pub const SETTINGS_FILE: &str = "agentLIBRE.toml";

/// Function used when no settings name one (D39, D58, AGL-020, D3).
const DEFAULT_FUNCTION: &str = "minimal";

/// Where a Strata server listens when `[server]` does not set `url`
/// (AGL-030, D1): its own run script binds port 18088 (8080 is held by qBittorrent).
pub const DEFAULT_STRATA_URL: &str = "http://127.0.0.1:18088";

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
}

/// The user file, `$XDG_CONFIG_HOME/agentLIBRE/agentLIBRE.toml`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UserFile {
    pub function: Option<String>,
    pub language: Option<Lang>,
    /// `[server]`: the defaults of the llama server; a project's own
    /// `[server]` wins per key (AGL-015, D4, D11).
    pub server: ServerFile,
    /// `[whisper]`: the defaults of the whisper server (AGL-014, D22).
    pub whisper: WhisperFile,
    /// `[search]`: the human's search service (AGL-019, D3).
    pub search: SearchFile,
    /// `[colors]` (D121).
    pub colors: Vec<(Role, Style)>,
    /// `[repl]` (D124, D130).
    pub repl: ReplFile,
    /// `[forge]` (AGL-007).
    pub forge: ForgeFile,
}

/// `[server]` of a settings file, the user's or the project's (AGL-015):
/// where the llama server listens and what `agl server start` runs
/// (D148). A key that is not set is `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerFile {
    /// The kind of inference server (AGL-030, D1); `None` is `llama`.
    pub kind: Option<String>,
    pub url: Option<String>,
    /// The inference engine's binary name, found on `PATH` (AGL-024, D5);
    /// `None` is the default `llama-server`.
    pub engine: Option<String>,
    /// A file name in `$XDG_DATA_HOME/agentLIBRE/models/` (D38).
    pub model: Option<String>,
    pub context: Option<u32>,
    /// More llama-server arguments.
    pub args: Option<Vec<String>>,
    /// The multimodal projector, a file name in the models directory
    /// (AGL-014, D23).
    pub mmproj: Option<String>,
    /// Whether the projector runs on the GPU; true when unset (D9, D23).
    pub mmproj_offload: Option<bool>,
    /// The most tokens one image takes: passed to llama-server and the
    /// bound of the context check (D10, D23).
    pub image_max_tokens: Option<u32>,
}

/// `[whisper]` of a settings file, the user's or the project's
/// (AGL-014, D22): the whisper.cpp server that turns an audio into text
/// when the model does not hear it. A key that is not set is `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WhisperFile {
    pub url: Option<String>,
    /// A file name in `$XDG_DATA_HOME/agentLIBRE/models/`.
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
            tomlfile::check_name(model)?;
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

    /// The `[whisper]` table as written to a file; empty when no key is
    /// set.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n[whisper]\n");
        if let Some(url) = &self.url {
            out.push_str(&format!("url = {}\n", tomlfile::quote(url)));
        }
        if let Some(model) = &self.model {
            out.push_str(&format!("model = {}\n", tomlfile::quote(model)));
        }
        if let Some(args) = &self.args {
            let items: Vec<String> = args.iter().map(|arg| tomlfile::quote(arg)).collect();
            out.push_str(&format!("args = [{}]\n", items.join(", ")));
        }
        out
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

    /// The `[search]` table as written to a file; empty when no key is
    /// set.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n[search]\n");
        for (key, value) in [
            ("url", &self.url),
            ("client_certificate", &self.client_certificate),
            ("client_private_key", &self.client_private_key),
            ("private_ca", &self.private_ca),
        ] {
            if let Some(value) = value {
                out.push_str(&format!("{key} = {}\n", tomlfile::quote(value)));
            }
        }
        out
    }
}

/// `image_max_tokens` when `[server]` does not set it (AGL-014, D23).
pub const DEFAULT_IMAGE_TOKENS: u32 = 2048;

impl ServerFile {
    const KEYS: &'static [&'static str] = &[
        "kind",
        "url",
        "engine",
        "model",
        "context",
        "args",
        "mmproj",
        "mmproj_offload",
        "image_max_tokens",
    ];

    /// Reads the `[server]` table of the file `path`; `section` is what
    /// `Section::section("server")` returned.
    pub fn parse(section: Option<Section>, path: &Path) -> Result<ServerFile, Error> {
        let Some(mut server) = section else {
            return Ok(ServerFile::default());
        };
        server.check_keys(Self::KEYS)?;
        let kind = server.string("kind")?;
        if let Some(kind) = &kind
            && ServerKind::parse(kind).is_none()
        {
            return Err(Error::UnknownServerKind {
                path: path.into(),
                value: kind.clone(),
            });
        }
        let url = server.string("url")?;
        if let Some(url) = &url
            && !url.starts_with("http://")
        {
            return Err(Error::UnsupportedUrl {
                path: path.into(),
                url: url.clone(),
            });
        }
        let engine = server.string("engine")?;
        let model = server.string("model")?;
        if let Some(model) = &model {
            tomlfile::check_name(model)?;
        }
        let context = server
            .integer("context")?
            .map(|n| {
                u32::try_from(n)
                    .ok()
                    .filter(|n| *n >= 1)
                    .ok_or_else(|| Error::NotPositive {
                        path: path.into(),
                        key: server.key("context"),
                    })
            })
            .transpose()?;
        let args = server.strings("args")?;
        let mmproj = server.string("mmproj")?;
        if let Some(mmproj) = &mmproj {
            tomlfile::check_name(mmproj)?;
        }
        let mmproj_offload = server.bool("mmproj_offload")?;
        let image_max_tokens = server
            .integer("image_max_tokens")?
            .map(|n| {
                u32::try_from(n)
                    .ok()
                    .filter(|n| *n >= 1)
                    .ok_or_else(|| Error::NotPositive {
                        path: path.into(),
                        key: server.key("image_max_tokens"),
                    })
            })
            .transpose()?;
        Ok(ServerFile {
            kind,
            url,
            engine,
            model,
            context,
            args,
            mmproj,
            mmproj_offload,
            image_max_tokens,
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

    /// The keys set here, the others from `defaults`; `args` is replaced
    /// as a whole, not merged (AGL-015, D11).
    pub fn over(&self, defaults: &ServerFile) -> ServerFile {
        ServerFile {
            kind: self.kind.clone().or_else(|| defaults.kind.clone()),
            url: self.url.clone().or_else(|| defaults.url.clone()),
            engine: self.engine.clone().or_else(|| defaults.engine.clone()),
            model: self.model.clone().or_else(|| defaults.model.clone()),
            context: self.context.or(defaults.context),
            args: self.args.clone().or_else(|| defaults.args.clone()),
            mmproj: self.mmproj.clone().or_else(|| defaults.mmproj.clone()),
            mmproj_offload: self.mmproj_offload.or(defaults.mmproj_offload),
            image_max_tokens: self.image_max_tokens.or(defaults.image_max_tokens),
        }
    }

    /// The `[server]` table as written to a file; empty when no key is
    /// set.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n[server]\n");
        if let Some(kind) = &self.kind {
            out.push_str(&format!("kind = {}\n", tomlfile::quote(kind)));
        }
        if let Some(url) = &self.url {
            out.push_str(&format!("url = {}\n", tomlfile::quote(url)));
        }
        if let Some(engine) = &self.engine {
            out.push_str(&format!("engine = {}\n", tomlfile::quote(engine)));
        }
        if let Some(model) = &self.model {
            out.push_str(&format!("model = {}\n", tomlfile::quote(model)));
        }
        if let Some(context) = self.context {
            out.push_str(&format!("context = {context}\n"));
        }
        if let Some(args) = &self.args {
            let items: Vec<String> = args.iter().map(|arg| tomlfile::quote(arg)).collect();
            out.push_str(&format!("args = [{}]\n", items.join(", ")));
        }
        if let Some(mmproj) = &self.mmproj {
            out.push_str(&format!("mmproj = {}\n", tomlfile::quote(mmproj)));
        }
        if let Some(offload) = self.mmproj_offload {
            out.push_str(&format!("mmproj_offload = {offload}\n"));
        }
        if let Some(tokens) = self.image_max_tokens {
            out.push_str(&format!("image_max_tokens = {tokens}\n"));
        }
        out
    }
}

/// `[repl]`: the limits of tool cards and their frame (D130).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReplFile {
    pub tool_lines: Option<usize>,
    pub tool_chars: Option<usize>,
    pub frame: Option<bool>,
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
}

impl Default for ReplSettings {
    fn default() -> ReplSettings {
        ReplSettings {
            tool_lines: 10,
            tool_chars: 500,
            frame: true,
        }
    }
}

impl UserFile {
    /// Loads `path`; a missing file has no settings.
    pub fn load(path: &Path) -> Result<UserFile, Error> {
        let Some(table) = tomlfile::read(path)? else {
            return Ok(UserFile::default());
        };
        let mut file = Section::new(path, table);
        file.check_keys(&[
            "function", "language", "server", "whisper", "search", "colors", "repl", "forge",
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
                repl.check_keys(&["tool_lines", "tool_chars", "frame"])?;
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
                ReplFile {
                    tool_lines: count("tool_lines")?,
                    tool_chars: count("tool_chars")?,
                    frame: repl.bool("frame")?,
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
        Ok(UserFile {
            function,
            language,
            server,
            whisper,
            search,
            colors,
            repl,
            forge,
        })
    }
}

/// Settings of a run.
#[derive(Debug)]
pub struct Settings {
    pub server_url: String,
    /// The `[server]` of the project over the user's defaults
    /// (AGL-015).
    pub server: ServerFile,
    /// The `[whisper]` of the project over the user's.
    pub whisper: WhisperFile,
    /// The `[search]` of the project over the user's, when it is
    /// complete; the tool `search` exists only then (AGL-019, D5).
    pub search: Option<Search>,
    pub function: String,
    pub colors: Vec<(Role, Style)>,
    pub repl: ReplSettings,
}

impl Settings {
    /// Combines the user file, the project's Function and `[server]`
    /// (`project_path` is its file, for hints) and `--function` (D58, D59,
    /// AGL-015).
    pub fn resolve(
        user: &UserFile,
        user_path: &Path,
        project: (&Path, Option<&str>, &ServerFile, &WhisperFile, &SearchFile),
        cli_function: Option<&str>,
    ) -> Result<Settings, Error> {
        let (project_path, project_function, project_server, project_whisper, project_search) =
            project;
        let server = project_server.over(&user.server);
        let whisper = project_whisper.over(&user.whisper);
        let search_file = project_search.over(&user.search);
        let search = search_file.complete().map_err(|key| Error::MissingKey {
            // The file that has a `[search]`, the project's first.
            path: if project_search.is_empty() {
                user_path.into()
            } else {
                project_path.into()
            },
            key: format!("search.{key}"),
        })?;
        // A Strata server has a default address (AGL-030, D1); the
        // llama-server address stays required.
        let server_url = match &server.url {
            Some(url) => url.clone(),
            None if server.kind() == ServerKind::Strata => DEFAULT_STRATA_URL.to_owned(),
            None => {
                return Err(Error::MissingServerUrl {
                    user_path: user_path.into(),
                    project_path: project_path.into(),
                })
            }
        };
        let function = cli_function
            .or(project_function)
            .or(user.function.as_deref())
            .unwrap_or(DEFAULT_FUNCTION)
            .to_owned();
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
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Expected;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A directory removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("agentLIBRE-test-{}-{n}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn write(&self, relative: &str, text: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn user(text: &str) -> Result<UserFile, Error> {
        let dir = TempDir::new();
        UserFile::load(&dir.write(SETTINGS_FILE, text))
    }

    #[test]
    fn user_file() {
        let text =
            "function = \"coder\"\nlanguage = \"uk\"\n[server]\nurl = \"http://127.0.0.1:18080\"\n";
        assert_eq!(
            user(text).unwrap(),
            UserFile {
                function: Some("coder".into()),
                language: Some(Lang::Uk),
                server: ServerFile {
                    url: Some("http://127.0.0.1:18080".into()),
                    ..ServerFile::default()
                },
                ..UserFile::default()
            }
        );
        let server = user(
            "[server]\nurl = \"http://127.0.0.1:18080\"\nmodel = \"m.gguf\"\ncontext = 8192\n\
             args = [\"-ngl\", \"999\"]\n",
        )
        .unwrap()
        .server;
        assert_eq!(
            server,
            ServerFile {
                url: Some("http://127.0.0.1:18080".into()),
                model: Some("m.gguf".into()),
                context: Some(8192),
                args: Some(vec!["-ngl".into(), "999".into()]),
                ..ServerFile::default()
            }
        );
    }

    #[test]
    fn the_whisper_table() {
        let loaded = user(
            "[whisper]\nurl = \"http://127.0.0.1:18082\"\nmodel = \"w.bin\"\nargs = [\"-t\", \"12\"]\n",
        )
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
        // The project wins per key, `args` as a whole; render and parse back.
        let project = WhisperFile {
            model: Some("small.bin".into()),
            args: Some(Vec::new()),
            ..WhisperFile::default()
        };
        let merged = project.over(&loaded);
        assert_eq!(merged.url, loaded.url);
        assert_eq!(merged.model.as_deref(), Some("small.bin"));
        assert_eq!(merged.args, Some(Vec::new()));
        let dir = TempDir::new();
        let path = dir.write(SETTINGS_FILE, &merged.render());
        assert_eq!(UserFile::load(&path).unwrap().whisper, merged);
        assert_eq!(WhisperFile::default().render(), "");
        assert!(matches!(
            user("[whisper]\nport = 1"),
            Err(Error::UnknownKey { key, .. }) if key == "whisper.port"
        ));
        assert!(matches!(
            user("[whisper]\nurl = \"https://x\""),
            Err(Error::UnsupportedUrl { .. })
        ));
        // `resolve` hands the merged table over.
        let settings = Settings::resolve(
            &UserFile {
                whisper: loaded,
                server: ServerFile {
                    url: Some("http://x".into()),
                    ..ServerFile::default()
                },
                ..UserFile::default()
            },
            Path::new("/c/agentLIBRE.toml"),
            (
                Path::new("/d/agentLIBRE.toml"),
                None,
                &ServerFile::default(),
                &project,
                &SearchFile::default(),
            ),
            None,
        )
        .unwrap();
        assert_eq!(settings.whisper, merged);
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
        let loaded = user(
            "[search]\nurl = \"https://search.example/v1/search\"\n\
             client_certificate = \"/c/client.crt\"\nclient_private_key = \"/c/client.pk8\"\n\
             private_ca = \"/c/ca.crt\"\n",
        )
        .unwrap()
        .search;
        assert_eq!(loaded, full_search());
        // The project wins per key; render and parse back.
        let project = SearchFile {
            url: Some("https://other.example/v1/search".into()),
            ..SearchFile::default()
        };
        let merged = project.over(&loaded);
        assert_eq!(merged.url.as_deref(), Some("https://other.example/v1/search"));
        assert_eq!(merged.private_ca, loaded.private_ca);
        let dir = TempDir::new();
        let path = dir.write(SETTINGS_FILE, &merged.render());
        assert_eq!(UserFile::load(&path).unwrap().search, merged);
        assert_eq!(SearchFile::default().render(), "");
        assert!(matches!(
            user("[search]\nport = 1"),
            Err(Error::UnknownKey { key, .. }) if key == "search.port"
        ));
        assert!(matches!(
            user("[search]\nurl = \"http://x\""),
            Err(Error::UnsupportedSearchUrl { url, .. }) if url == "http://x"
        ));
        assert!(matches!(
            user("[search]\nprivate_ca = \"ca.crt\""),
            Err(Error::SearchPathNotAbsolute { key, value, .. })
                if key == "search.private_ca" && value == "ca.crt"
        ));
    }

    #[test]
    fn the_search_is_set_up_only_by_a_complete_table() {
        let resolve = |user_search: SearchFile, project_search: SearchFile| {
            Settings::resolve(
                &UserFile {
                    search: user_search,
                    server: ServerFile {
                        url: Some("http://x".into()),
                        ..ServerFile::default()
                    },
                    ..UserFile::default()
                },
                Path::new("/c/agentLIBRE.toml"),
                (
                    Path::new("/d/agentLIBRE.toml"),
                    None,
                    &ServerFile::default(),
                    &WhisperFile::default(),
                    &project_search,
                ),
                None,
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
                if key == "search.client_certificate" && path == Path::new("/c/agentLIBRE.toml")
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
                if key == "search.client_certificate" && path == Path::new("/d/agentLIBRE.toml")
        ));
    }

    #[test]
    fn the_projector_keys() {
        let file = user(
            "[server]\nurl = \"http://x\"\nmmproj = \"p.gguf\"\nmmproj_offload = false\n\
             image_max_tokens = 1024\n",
        )
        .unwrap()
        .server;
        assert_eq!(file.mmproj.as_deref(), Some("p.gguf"));
        assert_eq!(file.mmproj_offload, Some(false));
        assert_eq!(file.image_max_tokens, Some(1024));
        // The project's keys win per key, and they render and parse back.
        let defaults = ServerFile {
            mmproj: Some("d.gguf".into()),
            mmproj_offload: Some(true),
            image_max_tokens: Some(512),
            ..ServerFile::default()
        };
        let merged = file.over(&defaults);
        assert_eq!(merged.mmproj.as_deref(), Some("p.gguf"));
        assert_eq!(merged.mmproj_offload, Some(false));
        assert_eq!(merged.image_max_tokens, Some(1024));
        let dir = TempDir::new();
        let path = dir.write(SETTINGS_FILE, &file.render());
        assert_eq!(UserFile::load(&path).unwrap().server, file);
        // Only an unset key falls back.
        let bare = ServerFile::default().over(&defaults);
        assert_eq!(bare, defaults);
        // A name that is not a file name, or a cap below 1, is an error.
        assert!(matches!(
            user("[server]\nmmproj = \"a/b.gguf\""),
            Err(Error::InvalidName(_))
        ));
        assert!(matches!(
            user("[server]\nimage_max_tokens = 0"),
            Err(Error::NotPositive { key, .. }) if key == "server.image_max_tokens"
        ));
        assert!(matches!(
            user("[server]\nmmproj_offload = \"yes\""),
            Err(Error::WrongType { key, .. }) if key == "server.mmproj_offload"
        ));
    }

    #[test]
    fn the_engine_key() {
        let file = user("[server]\nurl = \"http://x\"\nengine = \"vulkan-server\"\n")
            .unwrap()
            .server;
        assert_eq!(file.engine.as_deref(), Some("vulkan-server"));
        // The project's key wins per key, and it renders and parses back.
        let defaults = ServerFile {
            engine: Some("d-engine".into()),
            ..ServerFile::default()
        };
        let merged = file.over(&defaults);
        assert_eq!(merged.engine.as_deref(), Some("vulkan-server"));
        let dir = TempDir::new();
        let path = dir.write(SETTINGS_FILE, &file.render());
        assert_eq!(UserFile::load(&path).unwrap().server, file);
        // Only an unset key falls back.
        let bare = ServerFile::default().over(&defaults);
        assert_eq!(bare.engine.as_deref(), Some("d-engine"));
    }

    /// `Settings::resolve` of a user file and a project's `[server]`.
    fn resolved(user: &UserFile, project: &ServerFile) -> Result<Settings, Error> {
        Settings::resolve(
            user,
            Path::new("/c/agentLIBRE.toml"),
            (
                Path::new("/d/agentLIBRE.toml"),
                None,
                project,
                &WhisperFile::default(),
                &SearchFile::default(),
            ),
            None,
        )
    }

    fn server(url: &str, model: &str, context: u32, args: &[&str]) -> ServerFile {
        ServerFile {
            url: Some(url.into()),
            model: Some(model.into()),
            context: Some(context),
            args: Some(args.iter().map(|arg| (*arg).into()).collect()),
            ..ServerFile::default()
        }
    }

    #[test]
    fn the_project_server_wins_per_key() {
        let user = UserFile {
            server: server("http://127.0.0.1:18080", "big.gguf", 131072, &["-ngl", "999"]),
            ..UserFile::default()
        };
        // Nothing set in the project: the user's defaults.
        let settings = resolved(&user, &ServerFile::default()).unwrap();
        assert_eq!(settings.server, user.server);
        assert_eq!(settings.server_url, "http://127.0.0.1:18080");
        // Some keys set: those win, the others come from the user file.
        let project = ServerFile {
            url: Some("http://127.0.0.1:18081".into()),
            context: Some(65536),
            ..ServerFile::default()
        };
        let settings = resolved(&user, &project).unwrap();
        assert_eq!(settings.server_url, "http://127.0.0.1:18081");
        assert_eq!(
            settings.server,
            server("http://127.0.0.1:18081", "big.gguf", 65536, &["-ngl", "999"])
        );
    }

    #[test]
    fn the_project_args_replace_the_user_args_as_a_whole() {
        let user = UserFile {
            server: server("http://x", "m.gguf", 8192, &["-ngl", "999", "-fa", "on"]),
            ..UserFile::default()
        };
        let project = ServerFile {
            args: Some(vec!["-ngl".into(), "10".into()]),
            ..ServerFile::default()
        };
        let args = resolved(&user, &project).unwrap().server.args;
        assert_eq!(args, Some(vec!["-ngl".into(), "10".into()]));
        // An empty list is a value too: it replaces, it is not "unset".
        let empty = ServerFile {
            args: Some(Vec::new()),
            ..ServerFile::default()
        };
        assert_eq!(resolved(&user, &empty).unwrap().server.args, Some(Vec::new()));
    }

    #[test]
    fn the_server_table_renders_and_parses_back() {
        let file = server("http://127.0.0.1:18081", "q.gguf", 65536, &["-ngl", "99\"x"]);
        let text = file.render();
        assert_eq!(
            text,
            "\n[server]\nurl = \"http://127.0.0.1:18081\"\nmodel = \"q.gguf\"\ncontext = 65536\n\
             args = [\"-ngl\", \"99\\\"x\"]\n"
        );
        let dir = TempDir::new();
        let path = dir.write(SETTINGS_FILE, &text);
        let parsed = UserFile::load(&path).unwrap();
        assert_eq!(parsed.server, file);
        // Nothing set: no table at all.
        assert_eq!(ServerFile::default().render(), "");
    }

    #[test]
    fn colors_and_repl_tables() {
        let loaded = user(
            "[colors]\nmuted = \"italic #010203\"\ninput_background = \"#000000\"\n\
             [repl]\ntool_lines = 3\nframe = false\n",
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
            }
        );
        let settings = Settings::resolve(
            &loaded_with_server(loaded),
            Path::new("/c/x.toml"),
            (
                Path::new("/d/x.toml"),
                None,
                &ServerFile::default(),
                &WhisperFile::default(),
                &SearchFile::default(),
            ),
            None,
        )
        .unwrap();
        assert_eq!(
            settings.repl,
            ReplSettings {
                tool_lines: 3,
                tool_chars: 500,
                frame: false,
            }
        );
    }

    #[test]
    fn the_forge_table() {
        // Absent: the defaults.
        assert_eq!(user("").unwrap().forge, ForgeFile::default());
        assert_eq!(
            user("[forge]\nauto_rehome = false\n").unwrap().forge,
            ForgeFile {
                auto_rehome: Some(false)
            }
        );
        assert_eq!(
            user("[forge]\nauto_rehome = true\n").unwrap().forge,
            ForgeFile {
                auto_rehome: Some(true)
            }
        );
        // Strict: an unknown key and a wrong type are rejected.
        assert!(matches!(
            user("[forge]\ndir = \"x\""),
            Err(Error::UnknownKey { key, .. }) if key == "forge.dir"
        ));
        assert!(matches!(
            user("[forge]\nauto_rehome = \"on\""),
            Err(Error::WrongType { key, .. }) if key == "forge.auto_rehome"
        ));
    }

    fn loaded_with_server(file: UserFile) -> UserFile {
        UserFile {
            server: ServerFile {
                url: Some("http://x".into()),
                ..ServerFile::default()
            },
            ..file
        }
    }

    #[test]
    fn invalid_colors_and_repl_values() {
        assert!(matches!(
            user("[colors]\nmuted = \"red\""),
            Err(Error::InvalidStyle { key, .. }) if key == "colors.muted"
        ));
        assert!(matches!(
            user("[colors]\nmutd = \"dim\""),
            Err(Error::UnknownKey { key, .. }) if key == "colors.mutd"
        ));
        assert!(matches!(
            user("[colors]\nmuted = 1"),
            Err(Error::WrongType { key, .. }) if key == "colors.muted"
        ));
        assert!(matches!(
            user("[repl]\ntool_lines = 0"),
            Err(Error::NotPositive { key, .. }) if key == "repl.tool_lines"
        ));
        assert!(matches!(
            user("[repl]\ntool_chars = -5"),
            Err(Error::NotPositive { key, .. }) if key == "repl.tool_chars"
        ));
        assert!(matches!(
            user("[repl]\nlines = 5"),
            Err(Error::UnknownKey { key, .. }) if key == "repl.lines"
        ));
    }

    #[test]
    fn missing_user_file_is_empty() {
        let dir = TempDir::new();
        let loaded = UserFile::load(&dir.0.join(SETTINGS_FILE)).unwrap();
        assert_eq!(loaded, UserFile::default());
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(matches!(
            user("functon = \"coder\""),
            Err(Error::UnknownKey { key, .. }) if key == "functon"
        ));
        assert!(matches!(
            user("[server]\nurl = \"http://x\"\nport = 1"),
            Err(Error::UnknownKey { key, .. }) if key == "server.port"
        ));
    }

    #[test]
    fn wrong_types_are_errors() {
        assert!(matches!(
            user("function = 1"),
            Err(Error::WrongType { key, expected: Expected::String, .. }) if key == "function"
        ));
        assert!(matches!(
            user("server = \"http://x\""),
            Err(Error::WrongType { key, expected: Expected::Table, .. }) if key == "server"
        ));
        assert!(matches!(
            user("[server]\nurl = 1"),
            Err(Error::WrongType { key, .. }) if key == "server.url"
        ));
    }

    #[test]
    fn invalid_values_are_errors() {
        assert!(matches!(
            user("language = \"de\""),
            Err(Error::UnsupportedLanguage { value, .. }) if value == "de"
        ));
        assert!(matches!(
            user("[server]\nurl = \"https://x\""),
            Err(Error::UnsupportedUrl { url, .. }) if url == "https://x"
        ));
        assert!(matches!(user("function ="), Err(Error::Parse { .. })));
        assert!(matches!(
            user("[server]\nmodel = \"/tmp/m.gguf\""),
            Err(Error::InvalidName(name)) if name == "/tmp/m.gguf"
        ));
        assert!(matches!(
            user("[server]\ncontext = 0"),
            Err(Error::NotPositive { key, .. }) if key == "server.context"
        ));
        assert!(matches!(
            user("[server]\nargs = \"-ngl 999\""),
            Err(Error::WrongType { key, .. }) if key == "server.args"
        ));
    }

    #[test]
    fn function_precedence() {
        let path = Path::new("/c/agentLIBRE.toml");
        let user = UserFile {
            function: Some("user".into()),
            server: ServerFile {
                url: Some("http://x".into()),
                ..ServerFile::default()
            },
            ..UserFile::default()
        };
        let resolve = |user: &UserFile, project, cli| {
            Settings::resolve(
                user,
                path,
                (
                    Path::new("/d/agentLIBRE.toml"),
                    project,
                    &ServerFile::default(),
                    &WhisperFile::default(),
                    &SearchFile::default(),
                ),
                cli,
            )
            .unwrap()
            .function
        };
        assert_eq!(resolve(&user, Some("project"), Some("cli")), "cli");
        assert_eq!(resolve(&user, Some("project"), None), "project");
        assert_eq!(resolve(&user, None, None), "user");
        let no_function = UserFile {
            function: None,
            ..user
        };
        assert_eq!(resolve(&no_function, None, None), "minimal");
    }

    #[test]
    fn server_url_comes_from_the_user_file() {
        let user = UserFile {
            server: ServerFile {
                url: Some("http://127.0.0.1:18080".into()),
                ..ServerFile::default()
            },
            ..UserFile::default()
        };
        let settings = resolved(&user, &ServerFile::default());
        assert_eq!(settings.unwrap().server_url, "http://127.0.0.1:18080");
    }

    #[test]
    fn missing_server_url_is_an_error() {
        assert!(matches!(
            resolved(&UserFile::default(), &ServerFile::default()),
            Err(Error::MissingServerUrl { .. })
        ));
    }

    #[test]
    fn the_server_kind() {
        assert_eq!(ServerKind::parse("llama"), Some(ServerKind::Llama));
        assert_eq!(ServerKind::parse("strata"), Some(ServerKind::Strata));
        assert_eq!(ServerKind::parse("mistral"), None);
        // The absent key is llama (AGL-030, D1).
        let server = user("[server]\nurl = \"http://127.0.0.1:18080\"\n")
            .unwrap()
            .server;
        assert_eq!(server.kind(), ServerKind::Llama);
        let server = user("[server]\nkind = \"strata\"\nurl = \"http://127.0.0.1:18088\"\n")
            .unwrap()
            .server;
        assert_eq!(server.kind(), ServerKind::Strata);
        // An unknown kind is an error that names the value (AGL-030, D1).
        assert!(matches!(
            user("[server]\nkind = \"mistral\"\n"),
            Err(Error::UnknownServerKind { value, .. }) if value == "mistral"
        ));
    }

    #[test]
    fn the_strata_kind_has_a_default_url() {
        // A Strata server has a default address, so `url` is not required
        // (AGL-030, D1); the llama-server address stays required.
        let user = UserFile {
            server: ServerFile {
                kind: Some("strata".into()),
                ..ServerFile::default()
            },
            ..UserFile::default()
        };
        let settings = resolved(&user, &ServerFile::default());
        assert_eq!(settings.unwrap().server_url, DEFAULT_STRATA_URL);
    }
}
