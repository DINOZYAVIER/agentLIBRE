//! XDG directories (D35, D39, D61, D64).

use std::ffi::OsString;
use std::path::PathBuf;

use crate::error::Error;

use crate::settings::SETTINGS_FILE;

/// Directory below each XDG base directory.
const APP_DIR: &str = "agentLIBRE";

/// The application directories.
#[derive(Debug)]
pub struct Paths {
    /// `$XDG_CONFIG_HOME/agentLIBRE`.
    pub config: PathBuf,
    /// `$XDG_DATA_HOME/agentLIBRE`.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "sessions are stored from task-07")
    )]
    pub data: PathBuf,
}

impl Paths {
    /// Resolves the directories from the environment. An XDG variable that
    /// is unset, empty or relative falls back to its default below `HOME`.
    pub fn from_env(env: impl Fn(&str) -> Option<OsString>) -> Result<Paths, Error> {
        let absolute = |var: &str| env(var).map(PathBuf::from).filter(|p| p.is_absolute());
        let home = absolute("HOME");
        let base = |var: &'static str, default: &str| {
            absolute(var)
                .or_else(|| home.as_ref().map(|home| home.join(default)))
                .ok_or(Error::NoHome { var })
        };
        Ok(Paths {
            config: base("XDG_CONFIG_HOME", ".config")?.join(APP_DIR),
            data: base("XDG_DATA_HOME", ".local/share")?.join(APP_DIR),
        })
    }

    /// The user settings file.
    pub fn settings_file(&self) -> PathBuf {
        self.config.join(SETTINGS_FILE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(vars: &[(&str, &str)]) -> Result<Paths, Error> {
        Paths::from_env(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.into())
        })
    }

    #[test]
    fn defaults_below_home() {
        let paths = paths(&[("HOME", "/home/u")]).unwrap();
        assert_eq!(paths.config, PathBuf::from("/home/u/.config/agentLIBRE"));
        assert_eq!(paths.data, PathBuf::from("/home/u/.local/share/agentLIBRE"));
        assert_eq!(
            paths.settings_file(),
            PathBuf::from("/home/u/.config/agentLIBRE/agentLIBRE.toml")
        );
    }

    #[test]
    fn absolute_xdg_variables_replace_defaults() {
        let paths = paths(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/c"),
            ("XDG_DATA_HOME", "/d"),
        ])
        .unwrap();
        assert_eq!(paths.config, PathBuf::from("/c/agentLIBRE"));
        assert_eq!(paths.data, PathBuf::from("/d/agentLIBRE"));
    }

    #[test]
    fn empty_and_relative_xdg_variables_are_ignored() {
        let paths = paths(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", ""),
            ("XDG_DATA_HOME", "data"),
        ])
        .unwrap();
        assert_eq!(paths.config, PathBuf::from("/home/u/.config/agentLIBRE"));
        assert_eq!(paths.data, PathBuf::from("/home/u/.local/share/agentLIBRE"));
    }

    #[test]
    fn xdg_variables_work_without_home() {
        let paths = paths(&[("XDG_CONFIG_HOME", "/c"), ("XDG_DATA_HOME", "/d")]).unwrap();
        assert_eq!(paths.config, PathBuf::from("/c/agentLIBRE"));
    }

    #[test]
    fn missing_home_is_an_error() {
        assert!(matches!(
            paths(&[]),
            Err(Error::NoHome {
                var: "XDG_CONFIG_HOME"
            })
        ));
        assert!(matches!(
            paths(&[("HOME", "relative"), ("XDG_CONFIG_HOME", "/c")]),
            Err(Error::NoHome {
                var: "XDG_DATA_HOME"
            })
        ));
    }
}
