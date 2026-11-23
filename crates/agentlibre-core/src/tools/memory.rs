//! The `memory` tool (M32): the model's arguments and answers over the
//! memory's store (AGL-121, D80).

use std::path::Path;

use crate::action::Outcome;
use agentlibre_forge::memory::{self, Refusal, Write, hook_of};
use crate::tools::{Output, ToolResult, limited};

/// The arguments of the `memory` tool (M32); a field that the action does
/// not use is empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// `workspace`: in a plane, the workspace's memory (AGL-077, D8).
    pub store: String,
    pub action: String,
    pub name: String,
    pub title: String,
    pub hook: String,
    pub text: String,
    pub old: String,
    pub new: String,
}

impl Args {
    /// The tool's arguments from a parsed call: a field that is absent is
    /// empty.
    pub fn from_call(call: &crate::backend::ToolCall) -> Args {
        let text = |name: &str| call.text(name).unwrap_or_default().to_owned();
        Args {
            store: text("store"),
            action: text("action"),
            name: text("name"),
            title: text("title"),
            hook: text("hook"),
            text: text("text"),
            old: text("old"),
            new: text("new"),
        }
    }
}

fn fail(content: impl Into<String>) -> (ToolResult, Option<Write>) {
    (
        ToolResult {
            outcome: Outcome::Fail,
            content: content.into(),
        },
        None,
    )
}

fn ok(content: impl Into<String>, write: Write) -> (ToolResult, Option<Write>) {
    (
        ToolResult {
            outcome: Outcome::Ok,
            content: content.into(),
        },
        Some(write),
    )
}

/// The store's answer as the tool's: `done` names a write that was made.
fn answer(
    result: Result<Write, Refusal>,
    done: impl FnOnce(&Write) -> String,
) -> (ToolResult, Option<Write>) {
    match result {
        Ok(write) => ok(done(&write), write),
        Err(refusal) => fail(refusal.text()),
    }
}

/// Runs one action of the tool over the memory directory `dir` (M32).
/// `call_text` is the text of the call that preceded it (AGL-029, D1): the
/// body of `add` when its `text` field is empty; the other actions ignore it.
pub fn run(
    dir: &Path,
    args: &Args,
    call_text: Option<&str>,
    output: &Output<'_>,
) -> (ToolResult, Option<Write>) {
    match args.action.as_str() {
        "add" => add(dir, &args.title, &args.hook, &args.text, call_text),
        "show" => (show(dir, &args.name, output), None),
        "edit" => edit_entry(dir, &args.name, &args.old, &args.new),
        "update" => update(dir, &args.name, &args.title, &args.hook),
        "remove" => remove(dir, &args.name),
        other => fail(format!(
            "Unknown action `{other}`; use add, show, edit, update or remove."
        )),
    }
}

/// `add title hook text` (M32): the body is the `text`; when it is empty,
/// the `call_text` that preceded the call (AGL-029, D1).
pub fn add(
    dir: &Path,
    title: &str,
    hook: &str,
    text: &str,
    call_text: Option<&str>,
) -> (ToolResult, Option<Write>) {
    let call_body = call_text.map(str::trim).filter(|text| !text.is_empty());
    let (body, from_call) = match text.trim() {
        text if !text.is_empty() => (text, false),
        _ => match call_body {
            Some(body) => (body, true),
            None => ("", false),
        },
    };
    let note = if from_call {
        " (the body was taken from the text of the call)"
    } else {
        ""
    };
    answer(memory::add(dir, title, hook, body), |write| {
        format!("Remembered `{}`{note}.", write.name)
    })
}

/// `/remember name | text` (M34, M37): the program writes the entry,
/// its index hook being the text.
pub fn remember(dir: &Path, name: &str, text: &str) -> (ToolResult, Option<Write>) {
    add(dir, name, &hook_of(text), text, None)
}

/// `show name` (M32): the entry's text, cut like any tool's output when it is
/// long (AGL-049); the whole is the entry's own file.
pub fn show(dir: &Path, name: &str, output: &Output<'_>) -> ToolResult {
    match memory::read(dir, name) {
        Ok(text) => ToolResult {
            outcome: Outcome::Ok,
            content: limited(&text, output.tokens, &memory::entry_path(dir, name), output.count),
        },
        Err(refusal) => ToolResult {
            outcome: Outcome::Fail,
            content: refusal.text(),
        },
    }
}

/// `edit name old new` (M32): one unique occurrence in the entry's text, as
/// the workspace `edit` does it.
pub fn edit_entry(dir: &Path, name: &str, old: &str, new: &str) -> (ToolResult, Option<Write>) {
    answer(memory::edit(dir, name, old, new), |write| {
        format!("Replaced 1 occurrence in {}.md.", write.name)
    })
}

/// `update name title hook` (D6, AGL-032).
pub fn update(dir: &Path, name: &str, title: &str, hook: &str) -> (ToolResult, Option<Write>) {
    answer(memory::update(dir, name, title, hook), |write| {
        format!("Updated `{}`.", write.name)
    })
}

/// `remove name` (M32, AGL-060, D8).
pub fn remove(dir: &Path, name: &str) -> (ToolResult, Option<Write>) {
    answer(memory::remove(dir, name), |write| format!("Removed `{}`.", write.name))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{Args, add, edit_entry, remember, remove, run, show, update};
    use crate::action::Outcome;
    use agentlibre_forge::memory::*;
    use crate::tools::Output;
    use agentlibre_common::testutil::TempDir;

    fn added(dir: &Path, title: &str, hook: &str, text: &str) -> String {
        let (result, write) = add(dir, title, hook, text, None);
        assert_eq!(result.outcome, Outcome::Ok, "{}", result.content);
        write.unwrap().name
    }

    #[test]
    fn add_writes_the_entry_and_its_index_line() {
        let dir = TempDir::new();
        let (result, write) = add(&dir.0, "Build and test", "cargo build / cargo test", "Run `cargo build`.\nThen `cargo test`.\n", None);
        assert_eq!(result.content, "Remembered `build-and-test`.");
        assert_eq!(
            write,
            Some(Write {
                name: "build-and-test".into(),
                kind: Kind::Added,
                text: Some("Run `cargo build`.\nThen `cargo test`.".into()),
            })
        );
        // The entry: `# title`, a blank line, the text (M36).
        assert_eq!(
            fs::read_to_string(dir.0.join("build-and-test.md")).unwrap(),
            "# Build and test\n\nRun `cargo build`.\nThen `cargo test`.\n"
        );
        // The index: one line per entry (M31).
        assert_eq!(
            fs::read_to_string(dir.0.join(INDEX)).unwrap(),
            "- [Build and test](build-and-test.md) — cargo build / cargo test\n"
        );
        added(&dir.0, "llama-server", "127.0.0.1:18080", "Qwen3.8-27B.");
        let index = fs::read_to_string(dir.0.join(INDEX)).unwrap();
        assert_eq!(index.lines().count(), 2);
        assert!(index.ends_with("- [llama-server](llama-server.md) — 127.0.0.1:18080\n"));
    }

    #[test]
    fn a_clash_gets_a_number_and_a_bad_title_or_empty_field_is_refused() {
        let dir = TempDir::new();
        assert_eq!(added(&dir.0, "Setup", "one", "a"), "setup");
        assert_eq!(added(&dir.0, "setup", "two", "b"), "setup-2");
        assert_eq!(added(&dir.0, "SETUP!", "three", "c"), "setup-3");
        // Brackets and newlines of a title and a hook do not break the line.
        let name = added(&dir.0, "A [odd]\ntitle", "a hook\nover lines", "t");
        assert_eq!(name, "a-odd-title");
        let index = fs::read_to_string(dir.0.join(INDEX)).unwrap();
        assert!(index.contains("- [A (odd) title](a-odd-title.md) — a hook over lines\n"));
        for (title, hook, text) in [("", "h", "t"), ("t", "", "t"), ("t", "h", "  "), ("!!!", "h", "t")] {
            let (result, write) = add(&dir.0, title, hook, text, None);
            assert_eq!(result.outcome, Outcome::Fail, "{title:?}");
            assert!(write.is_none());
        }
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 5);
    }

    #[test]
    fn add_takes_the_body_from_the_text_of_the_call_when_the_parameter_is_empty() {
        let dir = TempDir::new();
        let (result, write) = add(&dir.0, "Note", "the hook", "", Some("The body text.\nMore."));
        assert_eq!(
            result.content,
            "Remembered `note` (the body was taken from the text of the call)."
        );
        assert_eq!(
            write,
            Some(Write {
                name: "note".into(),
                kind: Kind::Added,
                text: Some("The body text.\nMore.".into()),
            })
        );
        assert_eq!(
            fs::read_to_string(dir.0.join("note.md")).unwrap(),
            "# Note\n\nThe body text.\nMore.\n"
        );
        // A non-empty parameter wins over the text of the call.
        let (result, write) = add(&dir.0, "Other", "h", "The parameter body.", Some("ignored"));
        assert_eq!(result.content, "Remembered `other`.");
        assert_eq!(write.unwrap().text.as_deref(), Some("The parameter body."));
        // A whitespace-only text of the call is no body.
        let (result, write) = add(&dir.0, "Blank", "h", "", Some("   \n "));
        assert_eq!(result.content, "`add` needs a text.");
        assert!(write.is_none());
    }

    #[test]
    fn add_names_the_missing_fields() {
        let dir = TempDir::new();
        fs::create_dir_all(&dir.0).unwrap();
        for (title, hook, text, want) in [
            ("", "h", "t", "`add` needs a title."),
            ("t", "", "t", "`add` needs a hook (one line)."),
            ("t", "h", "", "`add` needs a text."),
            ("", "h", "", "`add` needs a title and a text."),
            ("", "", "t", "`add` needs a title and a hook (one line)."),
            ("", "", "", "`add` needs a title, a hook (one line) and a text."),
        ] {
            let (result, write) = add(&dir.0, title, hook, text, None);
            assert_eq!(result.outcome, Outcome::Fail);
            assert_eq!(result.content, want, "{title:?} {hook:?} {text:?}");
            assert!(write.is_none());
        }
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
    }

    #[test]
    fn show_returns_the_text_and_cuts_a_long_one() {
        let dir = TempDir::new();
        let name = added(&dir.0, "Notes", "h", "short text");
        let shown = show(&dir.0, &name, &out());
        assert_eq!(shown.outcome, Outcome::Ok);
        assert_eq!(shown.content, "# Notes\n\nshort text\n");
        assert_eq!(show(&dir.0, "nope", &out()).outcome, Outcome::Fail);
        assert_eq!(show(&dir.0, "../etc/passwd", &out()).outcome, Outcome::Fail);
        assert_eq!(show(&dir.0, "", &out()).outcome, Outcome::Fail);
        let long = added(&dir.0, "Long", "h", &"é".repeat(20_000));
        let shown = show(&dir.0, &long, &out());
        assert!(shown.content.contains("bytes omitted; the full output: "));
        assert!(shown.content.len() <= 4096 * 4 + 300);
        assert!(shown.content.ends_with("é\n"));
    }

    #[test]
    fn edit_changes_one_unique_occurrence() {
        let dir = TempDir::new();
        let name = added(&dir.0, "Servers", "h", "port 18080 and port 18081");
        let (result, write) = edit_entry(&dir.0, &name, "18080", "18090");
        assert_eq!(result.outcome, Outcome::Ok, "{}", result.content);
        assert_eq!(
            write,
            Some(Write {
                name: name.clone(),
                kind: Kind::Edited,
                text: Some("18090".into()),
            })
        );
        assert!(show(&dir.0, &name, &out()).content.contains("port 18090 and port 18081"));
        // Twice, or not at all: a failed call, nothing written.
        let (twice, none) = edit_entry(&dir.0, &name, "port", "x");
        assert_eq!(twice.outcome, Outcome::Fail);
        assert!(none.is_none());
        assert_eq!(edit_entry(&dir.0, &name, "absent", "x").0.outcome, Outcome::Fail);
        assert_eq!(edit_entry(&dir.0, "nope", "a", "b").0.outcome, Outcome::Fail);
        assert_eq!(edit_entry(&dir.0, "../x", "a", "b").0.outcome, Outcome::Fail);
    }

    #[test]
    fn update_rewrites_the_heading_and_the_index_line() {
        let dir = TempDir::new();
        let name = added(&dir.0, "One", "the old hook", "body text");
        let other = added(&dir.0, "Two", "second", "2");
        // Title only: the hook is kept, the other entry's line is untouched.
        let (result, write) = update(&dir.0, &name, "One, revised", "");
        assert_eq!(result.outcome, Outcome::Ok, "{}", result.content);
        assert_eq!(result.content, "Updated `one`.");
        assert_eq!(
            write,
            Some(Write {
                name: name.clone(),
                kind: Kind::Edited,
                text: Some("- [One, revised](one.md) — the old hook".into()),
            })
        );
        assert_eq!(
            fs::read_to_string(entry_path(&dir.0, &name)).unwrap(),
            "# One, revised\n\nbody text\n"
        );
        let index = fs::read_to_string(dir.0.join(INDEX)).unwrap();
        assert_eq!(
            index,
            "- [One, revised](one.md) — the old hook\n- [Two](two.md) — second\n".to_string()
        );
        assert_eq!(other, "two");
        // Hook only: the title is kept, a hook over lines is one line.
        let (result, write) = update(&dir.0, &name, "", "a hook\nover lines");
        assert_eq!(result.outcome, Outcome::Ok, "{}", result.content);
        assert_eq!(
            write.unwrap().text.as_deref(),
            Some("- [One, revised](one.md) — a hook over lines")
        );
        assert_eq!(
            fs::read_to_string(dir.0.join(INDEX)).unwrap().lines().next().unwrap(),
            "- [One, revised](one.md) — a hook over lines"
        );
        // Both at once: title and hook.
        let (result, _) = update(&dir.0, &name, "Final [Title]", "the final hook");
        assert_eq!(result.outcome, Outcome::Ok, "{}", result.content);
        let index = fs::read_to_string(dir.0.join(INDEX)).unwrap();
        assert!(index.contains("- [Final (Title)](one.md) — the final hook\n"));
        assert!(fs::read_to_string(entry_path(&dir.0, &name)).unwrap().starts_with("# Final (Title)\n"));
        // A hook with ` — ` of its own is kept whole.
        added(&dir.0, "Dash", "a — b — c", "d");
        let (result, _) = update(&dir.0, "dash", "", "");
        assert_eq!(result.outcome, Outcome::Fail, "{}", result.content);
        let (result, write) = update(&dir.0, "dash", "Dash, v2", "");
        assert_eq!(result.outcome, Outcome::Ok, "{}", result.content);
        assert_eq!(write.unwrap().text.as_deref(), Some("- [Dash, v2](dash.md) — a — b — c"));
    }

    #[test]
    fn update_refuses_the_misfires() {
        let dir = TempDir::new();
        let name = added(&dir.0, "One", "the hook", "body");
        // Neither field: nothing is written.
        let (result, write) = update(&dir.0, &name, "", "");
        assert_eq!(result.outcome, Outcome::Fail);
        assert_eq!(result.content, "`update` needs a title and/or a hook.");
        assert!(write.is_none());
        assert_eq!(
            fs::read_to_string(dir.0.join(INDEX)).unwrap(),
            "- [One](one.md) — the hook\n"
        );
        // No entry, or a bad name: the refusal of the other actions.
        assert_eq!(update(&dir.0, "nope", "t", "").0.content, "No memory entry `nope`.");
        assert_eq!(update(&dir.0, "../etc", "t", "").0.content, "No memory entry `../etc`.");
        // An entry without its index line: the index is the record of the
        // line, the refusal names the file.
        let two = added(&dir.0, "Two", "h", "t");
        fs::write(dir.0.join(INDEX), "- [One](one.md) — the hook\n").unwrap();
        assert_eq!(
            update(&dir.0, &two, "t", "").0.content,
            "No index line for `two` in MEMORY.md."
        );
        // An entry without a `# ` heading: the first line is no heading.
        let three = added(&dir.0, "Three", "h", "t");
        fs::write(entry_path(&dir.0, &three), "no heading\n").unwrap();
        assert_eq!(
            update(&dir.0, &three, "t", "").0.content,
            "The entry `three` has no `# ` heading."
        );
        // An empty entry file: no first line.
        let four = added(&dir.0, "Four", "h", "t");
        fs::write(entry_path(&dir.0, &four), "").unwrap();
        assert_eq!(
            update(&dir.0, &four, "t", "").0.content,
            "The entry `four` is empty."
        );
    }

    #[test]
    fn remove_deletes_the_file_and_only_its_index_line() {
        let dir = TempDir::new();
        let one = added(&dir.0, "One", "first", "1");
        let two = added(&dir.0, "Two", "second", "2");
        let (result, write) = remove(&dir.0, &one);
        assert_eq!(result.outcome, Outcome::Ok);
        assert_eq!(write.unwrap().kind, Kind::Removed);
        assert!(!entry_path(&dir.0, &one).exists());
        assert!(entry_path(&dir.0, &two).exists());
        assert_eq!(
            fs::read_to_string(dir.0.join(INDEX)).unwrap(),
            "- [Two](two.md) — second\n"
        );
        assert_eq!(remove(&dir.0, &one).0.outcome, Outcome::Fail);
        remove(&dir.0, &two);
        assert_eq!(fs::read_to_string(dir.0.join(INDEX)).unwrap(), "");
    }

    #[test]
    fn writers_at_once_keep_every_line_of_the_index() {
        let dir = TempDir::new();
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let dir = dir.0.clone();
                std::thread::spawn(move || {
                    for i in 0..10 {
                        let (result, _) =
                            add(&dir, &format!("Entry {t} {i}"), "a hook", "a text", None);
                        assert_eq!(result.outcome, Outcome::Ok);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let index = fs::read_to_string(dir.0.join(INDEX)).unwrap();
        assert_eq!(index.lines().count(), 80);
        for t in 0..8 {
            for i in 0..10 {
                assert!(index.contains(&format!("[Entry {t} {i}](entry-{t}-{i}.md)")));
            }
        }
    }

    #[test]
    fn remove_keeps_a_hook_that_quotes_another_entrys_link() {
        let dir = TempDir::new();
        let one = added(&dir.0, "One", "first", "1");
        let two = added(&dir.0, "Two", "see [One](one.md) first", "2");
        let (result, _) = remove(&dir.0, &one);
        assert_eq!(result.outcome, Outcome::Ok);
        assert!(entry_path(&dir.0, &two).exists());
        assert_eq!(
            fs::read_to_string(dir.0.join(INDEX)).unwrap(),
            format!("- [Two]({two}.md) — see [One](one.md) first\n")
        );
    }

    #[test]
    fn run_picks_the_action_and_refuses_an_unknown_one() {
        let dir = TempDir::new();
        let args = Args {
            action: "add".into(),
            title: "T".into(),
            hook: "h".into(),
            text: "x".into(),
            ..Args::default()
        };
        assert_eq!(run(&dir.0, &args, None, &out()).0.outcome, Outcome::Ok);
        let show_args = Args {
            action: "show".into(),
            name: "t".into(),
            ..Args::default()
        };
        assert_eq!(run(&dir.0, &show_args, None, &out()).0.content, "# T\n\nx\n");
        let bad = Args {
            action: "list".into(),
            ..Args::default()
        };
        let (result, write) = run(&dir.0, &bad, None, &out());
        assert_eq!(result.outcome, Outcome::Fail);
        assert!(result.content.contains("Unknown action `list`"));
        assert!(write.is_none());
    }

    #[test]
    fn a_line_over_the_cap_does_not_take_the_others_out() {
        // AGL-058, D10.
        let dir = TempDir::new();
        remember(&dir.0, "Old one", "an old note");
        let long = "пам'ять ".repeat(1500);
        remember(&dir.0, "Long one", &long);
        let index = index_text(&dir.0).unwrap();
        assert!(index.starts_with("[memory truncated]\n"), "{}", &index[..80]);
        assert!(index.contains("[Old one](old-one.md) — an old note"));
        let line = index.lines().last().unwrap();
        assert!(line.starts_with("- [Long one](long-one.md) — пам'ять"));
        assert!(line.ends_with('…'));
        assert!(index.len() <= INDEX_CAP);
        // The file and the entry keep all of it.
        assert!(fs::read_to_string(dir.0.join(INDEX)).unwrap().contains(long.trim_end()));
    }

    #[test]
    fn remember_writes_an_entry_whose_hook_is_the_text() {
        let dir = TempDir::new();
        let (result, write) = remember(&dir.0, "Build and test", "cargo build / cargo test");
        assert_eq!(result.outcome, Outcome::Ok);
        assert_eq!(write.unwrap().name, "build-and-test");
        assert_eq!(
            fs::read_to_string(dir.0.join(INDEX)).unwrap(),
            "- [Build and test](build-and-test.md) — cargo build / cargo test\n"
        );
        // A long text is whole in the index too, on one line (AGL-043 F19).
        let long = "word ".repeat(100);
        remember(&dir.0, "Long one", &long);
        let index = fs::read_to_string(dir.0.join(INDEX)).unwrap();
        let line = index.lines().last().unwrap();
        assert!(line.ends_with(long.trim_end()));
        assert!(show(&dir.0, "long-one", &out()).content.contains(long.trim_end()));
        assert_eq!(hook_of("a\nb  c"), "a b c");
        assert_eq!(remember(&dir.0, "", "t").0.outcome, Outcome::Fail);
    }

    #[test]
    fn the_index_is_no_entry() {
        // AGL-060: `remove MEMORY` would take every line with it.
        let dir = TempDir::new();
        added(&dir.0, "One", "first", "1");
        assert!(!exists(&dir.0, "MEMORY"));
        assert_eq!(remove(&dir.0, "MEMORY").0.outcome, Outcome::Fail);
        assert!(matches!(whole(&dir.0, "MEMORY"), Err(agentlibre_forge::Error::Memory(Error::NoEntry { .. }))));
        assert_eq!(list(&dir.0).unwrap().len(), 1);
        assert!(dir.0.join(INDEX).is_file());
    }

    #[test]
    fn remove_takes_a_line_with_no_file_and_a_file_with_no_line() {
        // AGL-060, D8.
        let dir = TempDir::new();
        added(&dir.0, "Gone", "first", "1");
        added(&dir.0, "Kept", "second", "2");
        added(&dir.0, "Stray", "third", "3");
        fs::remove_file(entry_path(&dir.0, "gone")).unwrap();
        let index = fs::read_to_string(dir.0.join(INDEX)).unwrap();
        fs::write(dir.0.join(INDEX), index.replace("- [Stray](stray.md) — third\n", "")).unwrap();
        assert!(exists(&dir.0, "gone") && exists(&dir.0, "stray"));
        for name in ["gone", "stray"] {
            let (result, write) = remove(&dir.0, name);
            assert_eq!(result.content, format!("Removed `{name}`."));
            assert_eq!(write.unwrap().kind, Kind::Removed);
            assert!(!exists(&dir.0, name), "{name}");
            assert_eq!(remove(&dir.0, name).0.outcome, Outcome::Fail, "{name}");
        }
        assert_eq!(
            fs::read_to_string(dir.0.join(INDEX)).unwrap(),
            "- [Kept](kept.md) — second\n"
        );
    }

    #[test]
    fn remove_does_not_empty_an_index_it_cannot_read() {
        // AGL-060.
        let dir = TempDir::new();
        let one = added(&dir.0, "One", "first", "1");
        fs::write(dir.0.join(INDEX), b"- [One](one.md) \xff\n").unwrap();
        let (result, write) = remove(&dir.0, &one);
        assert_eq!(result.outcome, Outcome::Fail);
        assert!(write.is_none());
        assert_eq!(fs::read(dir.0.join(INDEX)).unwrap(), b"- [One](one.md) \xff\n");
        assert!(entry_path(&dir.0, &one).exists());
    }

    fn out() -> Output<'static> {
        Output {
            tokens: 4096,
            file: Default::default(),
            count: &|text| Some(text.len() / 4),
        }
    }

}
