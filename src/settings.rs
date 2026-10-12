//! User and project `agentLIBRE.toml` files (D54-D60, D66).

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::i18n::Lang;
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
}

impl UserFile {
    /// Loads `path`; a missing file has no settings.
    pub fn load(path: &Path) -> Result<UserFile, Error> {
        let Some(table) = tomlfile::read(path)? else {
            return Ok(UserFile::default());
        };
        let mut file = Section::new(path, table);
        file.check_keys(&["function", "language", "server"])?;
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
        let server_url = match file.section("server")? {
            None => None,
            Some(mut server) => {
                server.check_keys(&["url"])?;
                server.string("url")?
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
        Ok(UserFile {
            function,
            language,
            server_url,
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
        Ok(Settings {
            server_url,
            function,
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
                std::env::temp_dir().join(format!("agl-lite-test-{}-{n}", std::process::id()));
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
            }
        );
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
            language: None,
            server_url: Some("http://x".into()),
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
