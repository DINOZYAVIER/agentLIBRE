//! The settings, `settings.nix` of the user and of the project's store
//! (D54-D60, D66, D148; AGL-039, D11, D12, D20, D21, D24): what the
//! machine has — where each kind of server listens, the engine, whisper,
//! the search service, the weights folders — and the human's interface.
//! The model of a run is its Function's (AGL-039, D21). The sections of the
//! services are modules of their own (AGL-043 F23): `server`, `whisper`,
//! `search` and `repl`; the kind, the files and the assembly are here.

mod repl;
mod search;
mod server;
mod whisper;

pub use repl::{ReplFile, ReplSettings};
pub use search::{Search, SearchCa, SearchFile};
pub use server::{DEFAULT_IMAGE_TOKENS, ServerFile};
pub use whisper::WhisperFile;

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::Error;
use crate::i18n::Lang;
use crate::nix::{self, Section};
use crate::theme::{COLOR_KEYS, Role, Style};

/// Name of the settings file, the user's and the project store's
/// (AGL-039, D11).
pub const SETTINGS_FILE: &str = "settings.nix";

/// Function used when no settings name one (D39, D58; AGL-039, D32).
const DEFAULT_FUNCTION: &str = "agentLIBRE-gpu-low";

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
    /// `about` (AGL-048).
    pub about: AboutFile,
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
    /// The project's settings of the evaluated `file` (`path`), its
    /// identity taken out already.
    pub fn from_section(mut file: Section, path: &Path) -> Result<ProjectFile, Error> {
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

/// `[forge]` (AGL-007).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ForgeFile {
    /// Adopt the unique same-named store when the workspace is at an
    /// unregistered location (default on).
    pub auto_rehome: Option<bool>,
}

impl UserFile {
    /// Loads `path`; a missing file has no settings.
    pub fn load(path: &Path) -> Result<UserFile, Error> {
        let Some(mut file) = evaluate(path)? else {
            return Ok(UserFile::default());
        };
        file.check_keys(&[
            "function", "language", "server", "whisper", "search", "colors", "repl", "forge",
            "models", "about",
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
        let repl = ReplFile::parse(file.section("repl")?, path)?;
        let about = AboutFile::parse(file.section("about")?, path)?;
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
            about,
            models,
        })
    }
}

/// `about` of the user's settings (AGL-048): whether the model is told of its
/// environment and of this agl, and where agl's source is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AboutFile {
    /// `about.enabled`; unset is on. Off is for a clean comparison with a
    /// benchmark.
    pub enabled: Option<bool>,
    /// `about.source`: the absolute path of agl's repository. The package
    /// does not know it.
    pub source: Option<String>,
}

impl AboutFile {
    fn parse(section: Option<Section>, path: &Path) -> Result<AboutFile, Error> {
        let Some(mut about) = section else {
            return Ok(AboutFile::default());
        };
        about.check_keys(&["enabled", "source"])?;
        let source = about.string("source")?;
        if let Some(source) = source.as_ref().filter(|source| !Path::new(source).is_absolute()) {
            return Err(Error::SearchPathNotAbsolute {
                path: path.into(),
                key: about.key("source"),
                value: source.clone(),
            });
        }
        Ok(AboutFile {
            enabled: about.bool("enabled")?,
            source,
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
    /// `about` of the user's settings (AGL-048).
    pub about: AboutFile,
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
            about: user.about.clone(),
            repl: ReplSettings {
                tool_lines: user.repl.tool_lines.unwrap_or(defaults.tool_lines),
                tool_chars: user.repl.tool_chars.unwrap_or(defaults.tool_chars),
                frame: user.repl.frame.unwrap_or(defaults.frame),
                max_record_seconds: user
                    .repl
                    .max_record_seconds
                    .unwrap_or(defaults.max_record_seconds),
                start_server: user.repl.start_server.unwrap_or(defaults.start_server),
            },
        })
    }
}

#[cfg(test)]
mod tests;
