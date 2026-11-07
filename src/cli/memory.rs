//! `agl memory list|show|remove` and what `/memory` shares with it (AGL-060):
//! the memory of the project looked at and cleaned with no session and no
//! model. Nothing here creates a store (M23 is about writes).

use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use crate::action::Outcome;
use crate::cli::args::MemoryAction;
use crate::error::Error;
use crate::i18n::{Lang, m};
use crate::memory::{self, Home, Listed};
use crate::paths::Paths;
use crate::settings::UserFile;
use crate::{outln, tr};

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
        MemoryAction::List => outln!("{}", list_text(dir, lang)?),
        // A closed pipe, as in `agl memory show x | head`, is not an error.
        MemoryAction::Show { name } => {
            let _ = write!(io::stdout(), "{}", show_text(dir, &name)?);
        }
        MemoryAction::Remove { name, yes } => {
            let answer = |question: &str| crate::cli::serve::ask_terminal(question, lang);
            let ask: Option<&dyn Fn(&str) -> bool> = if crate::repl::term::is_interactive() {
                Some(&answer)
            } else {
                None
            };
            outln!("{}", remove_asked(dir, &name, yes, ask, lang)?);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `/memory list` and `/memory show`, of the store `home` finds; a removal
/// is the caller's, which holds the session.
pub fn looked_at(home: &dyn Home, action: &MemoryAction, lang: Lang) -> Result<String, Error> {
    let dir = home.look()?;
    match action {
        MemoryAction::Show { name } => show_text(dir.as_deref(), name),
        _ => list_text(dir.as_deref(), lang),
    }
}

/// The lines of `list` (AGL-060, D4): `~` the prompt does not carry it, `?` a
/// file with no line, `!` a line with no file, and what the marks mean.
pub fn list_text(dir: Option<&Path>, lang: Lang) -> Result<String, Error> {
    let entries = dir.map(memory::list).transpose()?.unwrap_or_default();
    if entries.is_empty() {
        return Ok(tr!(lang, m::MEMORY_EMPTY).to_string());
    }
    let width = entries
        .iter()
        .map(|entry| entry.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines: Vec<String> = entries
        .iter()
        .map(|entry| {
            let about = if entry.hook.is_empty() {
                entry.title.clone()
            } else {
                format!("{} — {}", entry.title, entry.hook)
            };
            format!("{} {:<width$}  {about}", marks(entry), entry.name)
        })
        .collect();
    let legend = [
        (entries.iter().any(|entry| !entry.in_prompt), tr!(lang, m::MEMORY_MARK_CUT)),
        (entries.iter().any(|entry| !entry.line), tr!(lang, m::MEMORY_MARK_NO_LINE)),
        (entries.iter().any(|entry| !entry.file), tr!(lang, m::MEMORY_MARK_NO_FILE)),
    ];
    let legend: Vec<String> = legend
        .into_iter()
        .filter(|(shown, _)| *shown)
        .map(|(_, text)| text.to_string())
        .collect();
    if !legend.is_empty() {
        lines.push(String::new());
        lines.extend(legend);
    }
    Ok(lines.join("\n"))
}

/// The two marks of an entry: whether the prompt carries it, and whether its
/// file and its line agree.
fn marks(entry: &Listed) -> String {
    let cut = if entry.in_prompt { ' ' } else { '~' };
    let step = match (entry.file, entry.line) {
        (true, false) => '?',
        (false, _) => '!',
        (true, true) => ' ',
    };
    format!("{cut}{step}")
}

/// The whole text of an entry; a workspace with no store has none.
pub fn show_text(dir: Option<&Path>, name: &str) -> Result<String, Error> {
    memory::whole(entry_dir(dir, name)?, name)
}

/// The memory directory, when `name` is an entry in it, by its file or by its
/// line (AGL-060, D8).
pub fn entry_dir<'a>(dir: Option<&'a Path>, name: &str) -> Result<&'a Path, Error> {
    dir.filter(|dir| memory::exists(dir, name))
        .ok_or_else(|| Error::MemoryNoEntry {
            name: name.to_owned(),
        })
}

/// Removes an entry the way the tool does (AGL-060): what the tool answers,
/// for the model, and the write, for the session.
pub fn remove(dir: Option<&Path>, name: &str) -> Result<(String, memory::Write), Error> {
    match memory::remove(entry_dir(dir, name)?, name) {
        (result, Some(write)) if result.outcome == Outcome::Ok => Ok((result.content, write)),
        (result, _) => Err(Error::MemoryRemoveFailed {
            reason: result.content,
        }),
    }
}

/// The removal of the command line (AGL-060, D3): `yes` asks nothing, `ask` is
/// the terminal, and with neither it refuses. What it returns is what to say.
/// A missing entry is told before any question.
fn remove_asked(
    dir: Option<&Path>,
    name: &str,
    yes: bool,
    ask: Option<&dyn Fn(&str) -> bool>,
    lang: Lang,
) -> Result<String, Error> {
    entry_dir(dir, name)?;
    if !yes {
        let Some(ask) = ask else {
            return Err(Error::MemoryRemoveNeedsYes {
                name: name.to_owned(),
            });
        };
        if !ask(&tr!(lang, m::MEMORY_REMOVE_ASK, name = name)) {
            return Ok(tr!(lang, m::MEMORY_NOT_REMOVED, name = name));
        }
    }
    remove(dir, name)?;
    Ok(tr!(lang, m::MEMORY_REMOVED, name = name))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::forge::testutil::TempDir;
    use crate::memory::INDEX;

    /// A memory directory with the entries `one` and `two`.
    fn two_entries(root: &Path) -> PathBuf {
        let dir = root.join("memory");
        memory::add(&dir, "One", "first", "1", None);
        memory::add(&dir, "Two", "second", "2", None);
        dir
    }

    #[test]
    fn list_shows_the_name_title_and_hook_and_explains_only_the_marks_it_uses() {
        let root = TempDir::new();
        let dir = two_entries(&root.0);
        assert_eq!(
            list_text(Some(&dir), Lang::En).unwrap(),
            "   one  One — first\n   two  Two — second"
        );
        fs::remove_file(dir.join("two.md")).unwrap();
        fs::write(dir.join("stray.md"), "# Stray\n\ntext\n").unwrap();
        assert_eq!(
            list_text(Some(&dir), Lang::En).unwrap(),
            format!(
                "   one    One — first\n ! two    Two — second\n~? stray  Stray\n\n{}\n{}\n{}",
                tr!(Lang::En, m::MEMORY_MARK_CUT),
                tr!(Lang::En, m::MEMORY_MARK_NO_LINE),
                tr!(Lang::En, m::MEMORY_MARK_NO_FILE),
            )
        );
    }

    #[test]
    fn list_marks_what_the_prompt_does_not_carry() {
        let root = TempDir::new();
        let dir = root.0.join("memory");
        for n in 0..200 {
            memory::add(&dir, &format!("Entry {n}"), &"x".repeat(40), "t", None);
        }
        let text = list_text(Some(&dir), Lang::En).unwrap();
        let line = |name: &str| {
            text.lines()
                .find(|line| line.contains(&format!(" {name} ")))
                .unwrap()
                .to_owned()
        };
        assert!(line("entry-0").starts_with('~'), "{}", line("entry-0"));
        assert!(line("entry-199").starts_with(' '), "{}", line("entry-199"));
        assert!(text.ends_with(tr!(Lang::En, m::MEMORY_MARK_CUT)), "{text}");
    }

    #[test]
    fn no_store_and_an_empty_memory_say_so_and_a_broken_index_is_an_error() {
        let root = TempDir::new();
        let dir = root.0.join("memory");
        let empty = tr!(Lang::En, m::MEMORY_EMPTY);
        assert_eq!(list_text(None, Lang::En).unwrap(), empty);
        assert_eq!(list_text(Some(&dir), Lang::En).unwrap(), empty);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(INDEX), b"\xff\n").unwrap();
        assert!(matches!(list_text(Some(&dir), Lang::En), Err(Error::Read { .. })));
    }

    #[test]
    fn show_gives_the_whole_of_a_long_entry() {
        let root = TempDir::new();
        let dir = root.0.join("memory");
        let long = "word ".repeat(20_000);
        memory::add(&dir, "Long", "h", &long, None);
        let text = show_text(Some(&dir), "long").unwrap();
        assert!(text.contains(long.trim_end()));
        assert!(!text.contains("omitted"));
        assert!(matches!(show_text(Some(&dir), "nope"), Err(Error::MemoryNoEntry { .. })));
        assert!(matches!(show_text(None, "long"), Err(Error::MemoryNoEntry { .. })));
    }

    #[test]
    fn remove_asks_and_goes_by_the_answer() {
        let root = TempDir::new();
        let dir = two_entries(&root.0);
        let asked = Cell::new(0);
        // N keeps it.
        let no = |_: &str| {
            asked.set(asked.get() + 1);
            false
        };
        let said = remove_asked(Some(&dir), "one", false, Some(&no), Lang::En).unwrap();
        assert_eq!(said, "not removed: one");
        assert_eq!(asked.get(), 1);
        assert!(dir.join("one.md").is_file());
        assert!(fs::read_to_string(dir.join(INDEX)).unwrap().contains("one.md"));
        // y takes the file and only its line.
        let question = std::cell::RefCell::new(String::new());
        let yes = |text: &str| {
            *question.borrow_mut() = text.to_owned();
            true
        };
        let said = remove_asked(Some(&dir), "one", false, Some(&yes), Lang::En).unwrap();
        assert_eq!(said, "removed: one");
        assert_eq!(*question.borrow(), "Remove the memory entry one? [y/N]");
        assert!(!dir.join("one.md").exists());
        assert_eq!(
            fs::read_to_string(dir.join(INDEX)).unwrap(),
            "- [Two](two.md) — second\n"
        );
    }

    #[test]
    fn remove_with_yes_asks_nothing_and_with_no_terminal_it_refuses() {
        let root = TempDir::new();
        let dir = two_entries(&root.0);
        let never = |_: &str| -> bool { panic!("asked") };
        // No terminal and no `--yes`: refused, nothing touched.
        assert!(matches!(
            remove_asked(Some(&dir), "one", false, None, Lang::En),
            Err(Error::MemoryRemoveNeedsYes { name }) if name == "one"
        ));
        assert!(dir.join("one.md").is_file());
        // `--yes`, with a terminal and without.
        remove_asked(Some(&dir), "one", true, Some(&never), Lang::En).unwrap();
        remove_asked(Some(&dir), "two", true, None, Lang::En).unwrap();
        assert!(!dir.join("one.md").exists() && !dir.join("two.md").exists());
    }

    #[test]
    fn a_line_with_no_file_can_be_removed_and_has_no_text_to_show() {
        // AGL-060, D8.
        let root = TempDir::new();
        let dir = two_entries(&root.0);
        fs::remove_file(dir.join("one.md")).unwrap();
        assert!(matches!(show_text(Some(&dir), "one"), Err(Error::MemoryNoEntry { .. })));
        assert_eq!(
            remove_asked(Some(&dir), "one", true, None, Lang::En).unwrap(),
            "removed: one"
        );
        assert_eq!(
            fs::read_to_string(dir.join(INDEX)).unwrap(),
            "- [Two](two.md) — second\n"
        );
    }

    #[test]
    fn a_missing_entry_is_told_before_any_question() {
        let root = TempDir::new();
        let dir = two_entries(&root.0);
        let never = |_: &str| -> bool { panic!("asked") };
        for name in ["nope", "MEMORY", "../one"] {
            assert!(
                matches!(
                    remove_asked(Some(&dir), name, false, Some(&never), Lang::En),
                    Err(Error::MemoryNoEntry { .. })
                ),
                "{name}"
            );
        }
        assert!(matches!(
            remove_asked(None, "one", true, None, Lang::En),
            Err(Error::MemoryNoEntry { .. })
        ));
    }

    fn paths(data: &Path) -> Paths {
        Paths {
            config: data.to_path_buf(),
            data: data.to_path_buf(),
            state: data.to_path_buf(),
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
