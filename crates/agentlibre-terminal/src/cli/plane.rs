//! `agl plane …` (AGL-077, D11): the planes made, listed, shown, renamed,
//! given directories and deleted, with no session and no model.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::cli::args::PlaneAction;
use crate::error::Error;
use crate::i18n::{Lang, m};
use crate::outln;
use crate::paths::Paths;
use crate::plane::{self, PlaneFile};
use crate::session::Session;
use crate::tr;

pub fn run(action: PlaneAction, paths: &Paths, cwd: &Path, lang: Lang) -> Result<ExitCode, Error> {
    let ask = |question: &str| crate::engine::ask_terminal(question, lang);
    let ask: Option<&dyn Fn(&str) -> bool> = crate::tty::is_interactive().then_some(&ask);
    outln!("{}", act(action, paths, cwd, ask, lang)?);
    Ok(ExitCode::SUCCESS)
}

/// What the command does and says; `ask` is the terminal's question, none
/// without a terminal (D14).
fn act(
    action: PlaneAction,
    paths: &Paths,
    cwd: &Path,
    ask: Option<&dyn Fn(&str) -> bool>,
    lang: Lang,
) -> Result<String, Error> {
    let directory = |dir: Option<String>| dir.map_or_else(|| cwd.to_path_buf(), PathBuf::from);
    Ok(match action {
        PlaneAction::New { name, empty } => {
            let directories = if empty { Vec::new() } else { vec![cwd.to_path_buf()] };
            let plane = PlaneFile::create(paths, &name, &directories)?;
            tr!(lang, m::PLANE_MADE, name = plane.name, dir = plane.dir.display())
        }
        PlaneAction::List => {
            let planes = PlaneFile::list(paths)?;
            if planes.is_empty() {
                return Ok(tr!(lang, m::PLANE_NONE).to_string());
            }
            let here = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
            planes
                .iter()
                .map(|plane| {
                    let mark = if plane.directories.contains(&here) { "*" } else { " " };
                    format!("{mark} {}", plane.name)
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        PlaneAction::Show { name: Some(name) } => show(paths, &PlaneFile::open(paths, &name)?, lang),
        PlaneAction::Show { name: None } => {
            let planes = PlaneFile::of_directory(paths, cwd)?;
            if planes.is_empty() {
                return Ok(tr!(lang, m::PLANE_NONE_HERE).to_string());
            }
            planes
                .iter()
                .map(|plane| show(paths, plane, lang))
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        PlaneAction::Rename { old, new } => {
            let plane = PlaneFile::rename(paths, &old, &new)?;
            tr!(lang, m::PLANE_RENAMED, old = old, new = plane.name, dir = plane.dir.display())
        }
        PlaneAction::Add { name, dir } => {
            let mut plane = PlaneFile::open(paths, &name)?;
            let dir = directory(dir);
            let shown = std::fs::canonicalize(&dir).unwrap_or(dir.clone());
            if plane.add(&dir)? {
                tr!(lang, m::PLANE_ADDED, directory = shown.display(), name = name)
            } else {
                tr!(lang, m::PLANE_HAD, directory = shown.display(), name = name)
            }
        }
        PlaneAction::Remove { name, dir } => {
            let mut plane = PlaneFile::open(paths, &name)?;
            let dir = directory(dir);
            let shown = std::fs::canonicalize(&dir).unwrap_or(dir.clone());
            plane.remove(&dir)?;
            tr!(lang, m::PLANE_TAKEN_OUT, directory = shown.display(), name = name)
        }
        PlaneAction::Flow(verb) => flows(paths, cwd, &verb, lang)?,
        PlaneAction::Delete { name, yes } => {
            let plane = PlaneFile::open(paths, &name)?;
            if !yes {
                let ask = ask.ok_or_else(|| Error::PlaneDeleteNeedsYes { name: name.clone() })?;
                let question = tr!(lang, m::PLANE_DELETE_ASK, name = name, dir = plane.dir.display());
                if !ask(&question) {
                    return Ok(tr!(lang, m::PLANE_KEPT, name = name).to_string());
                }
            }
            plane.delete()?;
            tr!(lang, m::PLANE_DELETED, name = name)
        }
    })
}

/// A verb of the flows of the planes of `cwd` (AGL-118, D2, D7). A flow's
/// commands run in the plane's first directory, which is where its builds
/// are; a plane with no directory has none to run in (D9).
fn flows(
    paths: &Paths,
    cwd: &Path,
    verb: &plane::flow::Verb,
    lang: Lang,
) -> Result<String, Error> {
    let planes = PlaneFile::of_directory(paths, cwd)?;
    if planes.is_empty() {
        return Ok(tr!(lang, m::PLANE_NONE_HERE).to_string());
    }
    let many = planes.len() > 1;
    planes
        .iter()
        .map(|plane| {
            let workdir = plane
                .directories
                .first()
                .cloned()
                .unwrap_or_else(|| plane.dir.clone());
            let said = plane::flow::host::run(&plane.dir, &workdir, verb, lang);
            let text = said.text();
            if said.failed {
                return Err(Error::Said { text });
            }
            Ok(if many { format!("[{}] {text}", plane.name) } else { text })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|lines| lines.join("\n"))
}

/// A plane as `agl plane show` gives it: its name, id, store, sessions and
/// directories.
fn show(paths: &Paths, plane: &PlaneFile, lang: Lang) -> String {
    let sessions = Session::count_of_plane(&paths.sessions(), &plane.id);
    let mut text = tr!(
        lang,
        m::PLANE_SHOW,
        name = plane.name,
        id = plane.id,
        dir = plane.dir.display(),
        sessions = sessions
    );
    for directory in &plane.directories {
        text.push_str(&format!("\n  {}", directory.display()));
    }
    text
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::cli::args::{Command, parse};
    use crate::forge::testutil::TempDir;

    fn paths(dir: &TempDir) -> Paths {
        Paths {
            config: dir.0.join("config"),
            data: dir.0.join("data"),
            state: dir.0.join("state"),
            runtime: None,
        }
    }

    fn action(words: &[&str]) -> PlaneAction {
        match parse(words.iter().map(|w| w.into())).unwrap() {
            Command::Plane(action) => action,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_words_of_the_commands() {
        assert_eq!(
            action(&["plane", "new", "work", "--empty"]),
            PlaneAction::New {
                name: "work".into(),
                empty: true
            }
        );
        assert_eq!(action(&["plane", "show"]), PlaneAction::Show { name: None });
        assert_eq!(
            action(&["plane", "add", "work", "/x"]),
            PlaneAction::Add {
                name: "work".into(),
                dir: Some("/x".into())
            }
        );
        assert_eq!(
            action(&["plane", "delete", "--yes", "work"]),
            PlaneAction::Delete {
                name: "work".into(),
                yes: true
            }
        );
        assert!(matches!(parse(["plane"].map(Into::into)), Ok(Command::Help(_))));
        for words in [&["plane", "list", "x"][..], &["plane", "new", "a", "--yes"], &["plane", "rename", "a"]] {
            assert!(parse(words.iter().map(|w| w.into())).is_err(), "{words:?}");
        }
    }

    #[test]
    fn a_plane_is_made_shown_and_deleted_with_the_question() {
        let dir = TempDir::new();
        let paths = paths(&dir);
        let cwd = dir.0.join("project");
        fs::create_dir_all(&cwd).unwrap();
        let said = act(action(&["plane", "new", "work"]), &paths, &cwd, None, Lang::En).unwrap();
        assert!(said.starts_with("made the plane `work`"), "{said}");
        let listed = act(PlaneAction::List, &paths, &cwd, None, Lang::En).unwrap();
        assert_eq!(listed, "* work");
        let shown = act(PlaneAction::Show { name: None }, &paths, &cwd, None, Lang::En).unwrap();
        assert!(shown.contains("sessions: 0"), "{shown}");
        assert!(shown.ends_with(&format!("\n  {}", fs::canonicalize(&cwd).unwrap().display())));
        // No terminal and no --yes: refused.
        let delete = || action(&["plane", "delete", "work"]);
        assert!(matches!(
            act(delete(), &paths, &cwd, None, Lang::En),
            Err(Error::PlaneDeleteNeedsYes { .. })
        ));
        // A no keeps it, a yes deletes it.
        let no = |_: &str| false;
        assert!(act(delete(), &paths, &cwd, Some(&no), Lang::En).unwrap().contains("is kept"));
        let yes = |_: &str| true;
        act(delete(), &paths, &cwd, Some(&yes), Lang::En).unwrap();
        assert_eq!(
            act(PlaneAction::List, &paths, &cwd, None, Lang::En).unwrap(),
            "no plane; `agl plane new <name>` makes one"
        );
    }
}
