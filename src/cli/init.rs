//! `agl init`: registers the current directory as a project now
//! (AGL-010), the explicit form of the lazy registration.

use std::path::Path;
use std::process::ExitCode;

use crate::error::Error;
use crate::forge;
use crate::i18n::{Lang, m};
use crate::paths::Paths;
use crate::settings::UserFile;
use crate::tr;
use crate::outln;

use super::forge::ask_store;

/// Registers `cwd` as a project: its store directory and
/// `agentLIBRE.toml` are written when missing. An ancestor that is a
/// project is not looked at: `init` makes `cwd` its own project.
pub fn run(user: &UserFile, paths: &Paths, cwd: &Path, lang: Lang) -> Result<ExitCode, Error> {
    let auto_rehome = user.forge.auto_rehome.unwrap_or(true);
    let (ws, mut lines) = forge::resolve_dir(&paths.data, cwd, lang, auto_rehome, &|options| {
        ask_store(m::FORGE_ASK_STORE, options, lang)
    })?;
    let written = forge::materialize(&ws, lang)?;
    if lines.is_empty() && written.is_empty() {
        lines.push(tr!(
            lang,
            m::INIT_REGISTERED,
            project = ws.project.display(),
            dir = ws.dir.display()
        ));
    }
    lines.extend(written);
    for line in lines {
        outln!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::testutil::TempDir;

    fn paths(data: &Path) -> Paths {
        Paths {
            config: data.to_path_buf(),
            data: data.to_path_buf(),
            state: data.to_path_buf(),
        }
    }

    fn init(data: &Path, cwd: &Path) -> ExitCode {
        run(&UserFile::default(), &paths(data), cwd, Lang::En).unwrap()
    }

    #[test]
    fn init_registers_the_directory_and_is_quiet_the_second_time() {
        let dir = TempDir::new();
        let data = dir.0.join("data");
        let project = dir.0.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        assert_eq!(init(&data, &project), ExitCode::SUCCESS);
        let stores = forge::stores(&data).unwrap();
        assert_eq!(stores.len(), 1);
        let canonical = std::fs::canonicalize(&project).unwrap();
        assert_eq!(stores[0].hashes, vec![forge::identity_of(&canonical)]);
        // Again: nothing new.
        assert_eq!(init(&data, &project), ExitCode::SUCCESS);
        assert_eq!(forge::stores(&data).unwrap().len(), 1);
    }

    #[test]
    fn init_makes_a_subdirectory_its_own_project() {
        let dir = TempDir::new();
        let data = dir.0.join("data");
        let project = dir.0.join("proj");
        let sub = project.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        init(&data, &project);
        init(&data, &sub);
        assert_eq!(forge::stores(&data).unwrap().len(), 2);
        // From the subdirectory, the nearer project is found.
        let (ws, _) = forge::resolve(&data, &sub, Lang::En, true, &|_| {
            panic!("no store to choose")
        })
        .unwrap();
        assert_eq!(ws.project, std::fs::canonicalize(&sub).unwrap());
    }
}
