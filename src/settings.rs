//! The user `agentLIBRE.toml` file (D54-D60, D66, D148); the project
//! settings live in the project's store file (AGL-010, `forge::Marker`).

use std::path::Path;

use crate::error::Error;
use crate::i18n::Lang;
use crate::theme::{COLOR_KEYS, Role, Style};
use crate::tomlfile::{self, Section};

/// Name of the user settings file and of the project's file in its store.
pub const SETTINGS_FILE: &str = "agentLIBRE.toml";

/// Function used when no settings name one (D39, D58).
const DEFAULT_FUNCTION: &str = "default";

/// The user file, `$XDG_CONFIG_HOME/agentLIBRE/agentLIBRE.toml`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UserFile {
    pub function: Option<String>,
    pub language: Option<Lang>,
    /// `[server]`: the defaults of the llama server; a project's own
    /// `[server]` wins per key (AGL-015, D4, D11).
    pub server: ServerFile,
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
    pub url: Option<String>,
    /// A file name in `$XDG_DATA_HOME/agentLIBRE/models/` (D38).
    pub model: Option<String>,
    pub context: Option<u32>,
    /// More llama-server arguments.
    pub args: Option<Vec<String>>,
}

impl ServerFile {
    const KEYS: &'static [&'static str] = &["url", "model", "context", "args"];

    /// Reads the `[server]` table of the file `path`; `section` is what
    /// `Section::section("server")` returned.
    pub fn parse(section: Option<Section>, path: &Path) -> Result<ServerFile, Error> {
        let Some(mut server) = section else {
            return Ok(ServerFile::default());
        };
        server.check_keys(Self::KEYS)?;
        let url = server.string("url")?;
        if let Some(url) = &url
            && !url.starts_with("http://")
        {
            return Err(Error::UnsupportedUrl {
                path: path.into(),
                url: url.clone(),
            });
        }
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
        Ok(ServerFile {
            url,
            model,
            context,
            args,
        })
    }

    /// Whether no key is set.
    pub fn is_empty(&self) -> bool {
        *self == ServerFile::default()
    }

    /// The keys set here, the others from `defaults`; `args` is replaced
    /// as a whole, not merged (AGL-015, D11).
    pub fn over(&self, defaults: &ServerFile) -> ServerFile {
        ServerFile {
            url: self.url.clone().or_else(|| defaults.url.clone()),
            model: self.model.clone().or_else(|| defaults.model.clone()),
            context: self.context.or(defaults.context),
            args: self.args.clone().or_else(|| defaults.args.clone()),
        }
    }

    /// The `[server]` table as written to a file; empty when no key is
    /// set.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n[server]\n");
        if let Some(url) = &self.url {
            out.push_str(&format!("url = {}\n", tomlfile::quote(url)));
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
        file.check_keys(&["function", "language", "server", "colors", "repl", "forge"])?;
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
        project: (&Path, Option<&str>, &ServerFile),
        cli_function: Option<&str>,
    ) -> Result<Settings, Error> {
        let (project_path, project_function, project_server) = project;
        let server = project_server.over(&user.server);
        let server_url = server.url.clone().ok_or_else(|| Error::MissingServerUrl {
            user_path: user_path.into(),
            project_path: project_path.into(),
        })?;
        let function = cli_function
            .or(project_function)
            .or(user.function.as_deref())
            .unwrap_or(DEFAULT_FUNCTION)
            .to_owned();
        let defaults = ReplSettings::default();
        Ok(Settings {
            server_url,
            server,
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
            }
        );
    }

    /// `Settings::resolve` of a user file and a project's `[server]`.
    fn resolved(user: &UserFile, project: &ServerFile) -> Result<Settings, Error> {
        Settings::resolve(
            user,
            Path::new("/c/agentLIBRE.toml"),
            (Path::new("/d/agentLIBRE.toml"), None, project),
            None,
        )
    }

    fn server(url: &str, model: &str, context: u32, args: &[&str]) -> ServerFile {
        ServerFile {
            url: Some(url.into()),
            model: Some(model.into()),
            context: Some(context),
            args: Some(args.iter().map(|arg| (*arg).into()).collect()),
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
            (Path::new("/d/x.toml"), None, &ServerFile::default()),
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
                (Path::new("/d/agentLIBRE.toml"), project, &ServerFile::default()),
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
        assert_eq!(resolve(&no_function, None, None), "default");
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
}
