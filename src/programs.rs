//! The helper programs agl runs (AGL-040; AGL-039 D15): `bash`, `git`,
//! `curl`, `sha256sum` (AGL-050), `ffmpeg`, `ffprobe`, `wl-paste` and `step`
//! (AGL-043 F20), by
//! their absolute paths in the directory the package's wrapper names in
//! `AGENTLIBRE_TOOLS`, not by their names on `PATH`. `systemd-run` and `systemctl` are
//! not among them (AGL-039 D16), and the engine has a directory of its own (AGL-052).

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::Command;

use crate::error::Error;

/// The variable of the helper programs' directory (AGL-040, D2).
pub const TOOLS_VAR: &str = "AGENTLIBRE_TOOLS";

/// The variable of the directory of the engine that came with the package
/// (AGL-052): `llama-server` on Linux, `llama-server.exe` in WSL. Without it,
/// the engine is the system's.
pub const ENGINES_VAR: &str = "AGENTLIBRE_ENGINES";

/// The prefix of every variable the wrapper sets (AGL-040, D2, D3).
const VAR_PREFIX: &[u8] = b"AGENTLIBRE_";

/// The absolute path of the helper program `name`.
pub fn program(name: &str) -> Result<PathBuf, Error> {
    program_in(std::env::var_os(TOOLS_VAR), name)
}

/// `name` in the directory `dir` (the value of `AGENTLIBRE_TOOLS`): an
/// unset, empty or relative directory is `ToolsDirMissing`, a directory
/// without the program `ToolMissing`. No fall-back to `PATH` (AGL-039,
/// D6: Nix is required).
fn program_in(dir: Option<OsString>, name: &str) -> Result<PathBuf, Error> {
    let dir = dir
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .ok_or(Error::ToolsDirMissing)?;
    let path = dir.join(name);
    if path.is_file() {
        Ok(path)
    } else {
        Err(Error::ToolMissing {
            name: name.into(),
            dir,
        })
    }
}

/// Removes the wrapper's `AGENTLIBRE_*` variables from the environment of
/// `command`, so that a command the agent or the human asked for sees the
/// environment of the human's terminal (AGL-040, D3).
pub fn clean_env(command: &mut Command) -> &mut Command {
    for (key, _) in std::env::vars_os() {
        if key.as_bytes().starts_with(VAR_PREFIX) {
            command.env_remove(key);
        }
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::testutil::TempDir;
    use std::fs;

    #[test]
    fn a_program_comes_from_the_tools_directory_only() {
        let dir = TempDir::new();
        fs::write(dir.0.join("git"), "").unwrap();
        let found = program_in(Some(dir.0.clone().into()), "git").unwrap();
        assert_eq!(found, dir.0.join("git"));
        // Unset, empty and relative are the same missing directory.
        for unset in [None, Some(OsString::new()), Some("relative/bin".into())] {
            assert!(matches!(program_in(unset, "git"), Err(Error::ToolsDirMissing)));
        }
        // A directory without the program names both.
        match program_in(Some(dir.0.clone().into()), "curl") {
            Err(Error::ToolMissing { name, dir: at }) => {
                assert_eq!(name, "curl");
                assert_eq!(at, dir.0);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_cleaned_environment_has_no_agentlibre_variable() {
        // The test process may run under the wrapper or the dev shell: the
        // variables it has are the ones the child must not see.
        let mut command = Command::new("env");
        clean_env(&mut command);
        let removed: Vec<_> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_owned())
            .collect();
        for (key, _) in std::env::vars_os() {
            if key.as_bytes().starts_with(VAR_PREFIX) {
                assert!(removed.contains(&key), "{key:?} is kept");
            }
        }
        assert!(removed.iter().all(|key| key.as_bytes().starts_with(VAR_PREFIX)));
    }
}
