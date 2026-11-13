//! `agl memory list|show|remove` and what `/memory` shares with it (AGL-060):
//! the memory of the project looked at and cleaned with no session and no
//! model. Nothing here creates a store (M23 is about writes).

use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use crate::memory::MemoryAction;
use crate::error::Error;
use crate::i18n::Lang;
use crate::memory;
use crate::paths::Paths;
use crate::settings::UserFile;
use crate::outln;

pub fn run(
    action: MemoryAction,
    user: &UserFile,
    paths: &Paths,
    cwd: &Path,
    lang: Lang,
) -> Result<ExitCode, Error> {
    // The lookup of the forge, whose failure is an error here: a store that
    // cannot be resolved is not an empty memory.
    let auto_rehome = user.forge.auto_rehome.unwrap_or(true);
    let ws = crate::cli::forge::project_of(paths, cwd, auto_rehome, lang)?;
    let dir = memory::of_store(&ws.dir);
    let dir = dir.as_deref();
    match action {
        MemoryAction::List => outln!("{}", memory::list_text(dir, lang)?),
        // A closed pipe, as in `agl memory show x | head`, is not an error.
        MemoryAction::Show { name } => {
            let _ = write!(io::stdout(), "{}", memory::show_text(dir, &name)?);
        }
        MemoryAction::Remove { name, yes } => {
            let answer = |question: &str| crate::engine::ask_terminal(question, lang);
            let ask: Option<&dyn Fn(&str) -> bool> = if crate::tty::is_interactive() {
                Some(&answer)
            } else {
                None
            };
            outln!("{}", memory::remove_asked(dir, &name, yes, ask, lang)?);
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::forge::testutil::TempDir;
    use crate::settings::UserFile;
    use std::process::ExitCode;

    fn paths(data: &Path) -> Paths {
        Paths {
            config: data.to_path_buf(),
            data: data.to_path_buf(),
            state: data.to_path_buf(),
            runtime: None,
        }
    }

    #[test]
    fn a_directory_that_is_no_project_leaves_no_store_behind() {
        let root = TempDir::new();
        let data = root.0.join("data");
        let stray = root.0.join("stray");
        fs::create_dir_all(&stray).unwrap();
        let run_it = |action| run(action, &UserFile::default(), &paths(&data), &stray, Lang::En);
        assert_eq!(run_it(MemoryAction::List).unwrap(), ExitCode::SUCCESS);
        for action in [
            MemoryAction::Show { name: "x".into() },
            MemoryAction::Remove { name: "x".into(), yes: true },
        ] {
            assert!(matches!(run_it(action), Err(Error::MemoryNoEntry { .. })));
        }
        assert!(!data.exists());
    }

    #[test]
    fn a_broken_store_is_an_error_and_not_an_empty_memory() {
        let root = TempDir::new();
        let data = root.0.join("data");
        let project = root.0.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(data.join("broken")).unwrap();
        fs::write(
            data.join("broken").join(crate::forge::MARKER_FILE),
            "{ agl, ... }: { format = ; }\n",
        )
        .unwrap();
        assert!(
            run(MemoryAction::List, &UserFile::default(), &paths(&data), &project, Lang::En)
                .is_err()
        );
    }
}
