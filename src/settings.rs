//! User and project `agentLIBRE.toml` files (D54-D60, D66, D148).

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::i18n::Lang;
use crate::theme::{COLOR_KEYS, Role, Style};
use crate::tomlfile::{self, Section};

/// Name of the user and project settings files.
pub const SETTINGS_FILE: &str = "agentLIBRE.toml";

/// Function used when no settings name one (D39, D58).
const DEFAULT_FUNCTION: &str = "default";

/// The user file, `$XDG_CONFIG_HOME/agentLIBRE/agentLIBRE.toml`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UserFile {
    pub function: Option<String>,
    pub language: Option<Lang>,
    /// `[server] url`.
    pub server_url: Option<String>,
    /// What `agentLIBRE server start` runs (D148).
    pub launch: Launch,
    /// `[colors]` (D121).
    pub colors: Vec<(Role, Style)>,
    /// `[repl]` (D124, D130).
    pub repl: ReplFile,
}

/// `[server]` keys of `agentLIBRE server start` (D148).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Launch {
    /// A file name in `$XDG_DATA_HOME/agentLIBRE/models/` (D38).
    pub model: Option<String>,
    pub context: Option<u32>,
    /// More llama-server arguments.
    pub args: Vec<String>,
}

/// `[repl]`: the limits of tool cards and their frame (D130).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReplFile {
    pub tool_lines: Option<usize>,
    pub tool_chars: Option<usize>,
    pub frame: Option<bool>,
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
        file.check_keys(&["function", "language", "server", "colors", "repl"])?;
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
        let (server_url, launch) = match file.section("server")? {
            None => (None, Launch::default()),
            Some(mut server) => {
                server.check_keys(&["url", "model", "context", "args"])?;
                let url = server.string("url")?;
                let model = server.string("model")?;
                if let Some(model) = &model {
                    tomlfile::check_name(model)?;
                }
                let context = server
                    .integer("context")?
                    .map(|n| {
                        u32::try_from(n).ok().filter(|n| *n >= 1).ok_or_else(|| {
                            Error::NotPositive {
                                path: path.into(),
                                key: server.key("context"),
                            }
                        })
                    })
                    .transpose()?;
                let args = server.strings("args")?.unwrap_or_default();
                (
                    url,
                    Launch {
                        model,
                        context,
                        args,
                    },
                )
            }
        };
        if let Some(url) = &server_url
            && !url.starts_with("http://")
        {
            return Err(Error::UnsupportedUrl {
                path: path.into(),
                url: url.clone(),
            });
        }
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
        Ok(UserFile {
            function,
            language,
            server_url,
            launch,
            colors,
            repl,
        })
    }
}

/// A project file, found by [`find_project`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProjectFile {
    pub function: Option<String>,
}

impl ProjectFile {
    /// Loads `path`. `user_path` names the user file in the error that
    /// rejects `[server]` (D57).
    pub fn load(path: &Path, user_path: &Path) -> Result<ProjectFile, Error> {
        let mut file = Section::new(path, tomlfile::read(path)?.unwrap_or_default());
        if file.contains("server") {
            return Err(Error::ServerInProject {
                path: path.into(),
                user_path: user_path.into(),
            });
        }
        file.check_keys(&["function"])?;
        Ok(ProjectFile {
            function: file.string("function")?,
        })
    }
}

/// The nearest `agentLIBRE.toml` in `cwd` or its ancestors, skipping the
/// user file (D56).
pub fn find_project(cwd: &Path, user_path: &Path) -> Option<PathBuf> {
    let user_path = fs::canonicalize(user_path).ok();
    cwd.ancestors()
        .map(|dir| dir.join(SETTINGS_FILE))
        .filter(|path| path.is_file())
        .find(|path| user_path.is_none() || fs::canonicalize(path).ok() != user_path)
}

/// Settings of a run.
#[derive(Debug)]
pub struct Settings {
    pub server_url: String,
    pub function: String,
    pub colors: Vec<(Role, Style)>,
    pub repl: ReplSettings,
}

impl Settings {
    /// Combines the files and `--function` (D58, D59).
    pub fn resolve(
        user: &UserFile,
        user_path: &Path,
        project: Option<&ProjectFile>,
        cli_function: Option<&str>,
    ) -> Result<Settings, Error> {
        let server_url = user
            .server_url
            .clone()
            .ok_or_else(|| Error::MissingServerUrl {
                user_path: user_path.into(),
            })?;
        let function = cli_function
            .or(project.and_then(|p| p.function.as_deref()))
            .or(user.function.as_deref())
            .unwrap_or(DEFAULT_FUNCTION)
            .to_owned();
        let defaults = ReplSettings::default();
        Ok(Settings {
            server_url,
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
                server_url: Some("http://127.0.0.1:18080".into()),
                ..UserFile::default()
            }
        );
        let launch = user(
            "[server]\nurl = \"http://127.0.0.1:18080\"\nmodel = \"m.gguf\"\ncontext = 8192\n\
             args = [\"-ngl\", \"999\"]\n",
        )
        .unwrap()
        .launch;
        assert_eq!(
            launch,
            Launch {
                model: Some("m.gguf".into()),
                context: Some(8192),
                args: vec!["-ngl".into(), "999".into()],
            }
        );
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
            None,
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

    fn loaded_with_server(file: UserFile) -> UserFile {
        UserFile {
            server_url: Some("http://x".into()),
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
    fn project_file_allows_only_function() {
        let dir = TempDir::new();
        let user_path = dir.0.join("config").join(SETTINGS_FILE);
        let path = dir.write("p/agentLIBRE.toml", "function = \"coder\"");
        assert_eq!(
            ProjectFile::load(&path, &user_path)
                .unwrap()
                .function
                .as_deref(),
            Some("coder")
        );
        let path = dir.write("p/agentLIBRE.toml", "[server]\nurl = \"http://x\"");
        assert!(matches!(
            ProjectFile::load(&path, &user_path),
            Err(Error::ServerInProject { .. })
        ));
        let path = dir.write("p/agentLIBRE.toml", "language = \"uk\"");
        assert!(matches!(
            ProjectFile::load(&path, &user_path),
            Err(Error::UnknownKey { key, .. }) if key == "language"
        ));
    }

    #[test]
    fn project_file_is_found_in_ancestors() {
        let dir = TempDir::new();
        let user_path = dir.0.join("config").join(SETTINGS_FILE);
        let project = dir.write("repo/agentLIBRE.toml", "");
        fs::create_dir_all(dir.0.join("repo/src/deep")).unwrap();
        assert_eq!(
            find_project(&dir.0.join("repo/src/deep"), &user_path),
            Some(project.clone())
        );
        let nearer = dir.write("repo/src/agentLIBRE.toml", "");
        assert_eq!(
            find_project(&dir.0.join("repo/src/deep"), &user_path),
            Some(nearer)
        );
    }

    #[test]
    fn user_file_is_not_a_project_file() {
        let dir = TempDir::new();
        let user_path = dir.write("config/agentLIBRE/agentLIBRE.toml", "");
        let project = dir.write("agentLIBRE.toml", "");
        assert_eq!(
            find_project(&dir.0.join("config/agentLIBRE"), &user_path),
            Some(project)
        );
    }

    #[test]
    fn function_precedence() {
        let path = Path::new("/c/agentLIBRE.toml");
        let user = UserFile {
            function: Some("user".into()),
            server_url: Some("http://x".into()),
            ..UserFile::default()
        };
        let project = ProjectFile {
            function: Some("project".into()),
        };
        let resolve = |user: &UserFile, project, cli| {
            Settings::resolve(user, path, project, cli)
                .unwrap()
                .function
        };
        assert_eq!(resolve(&user, Some(&project), Some("cli")), "cli");
        assert_eq!(resolve(&user, Some(&project), None), "project");
        let empty = ProjectFile::default();
        assert_eq!(resolve(&user, Some(&empty), None), "user");
        assert_eq!(resolve(&user, None, None), "user");
        let no_function = UserFile {
            function: None,
            ..user
        };
        assert_eq!(resolve(&no_function, None, None), "default");
    }

    #[test]
    fn server_url_comes_from_the_user_file() {
        let user = UserFile {
            server_url: Some("http://127.0.0.1:18080".into()),
            ..UserFile::default()
        };
        let settings = Settings::resolve(&user, Path::new("/c/agentLIBRE.toml"), None, None);
        assert_eq!(settings.unwrap().server_url, "http://127.0.0.1:18080");
    }

    #[test]
    fn missing_server_url_is_an_error() {
        let path = Path::new("/c/agentLIBRE.toml");
        assert!(matches!(
            Settings::resolve(&UserFile::default(), path, None, None),
            Err(Error::MissingServerUrl { .. })
        ));
    }
}
