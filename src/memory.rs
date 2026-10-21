//! The memory of a workspace (AGL-006): entries kept across sessions in
//! `<store>/memory/`, one Markdown file each and an index, `MEMORY.md`,
//! one line per entry (M31): `- [title](name.md) — a one-line hook`. The
//! model writes them with the `memory` tool, the human with `/remember` or
//! by hand (M30, M32, M34).

use std::fs;
use std::path::{Path, PathBuf};

use crate::action::Outcome;
use crate::error::Error;
use crate::forge::task_spec::slug;
use crate::forge::write_atomic;
use crate::i18n::Lang;
use crate::tools::{ToolResult, edit};

/// The index file of the memory directory (M31).
pub const INDEX: &str = "MEMORY.md";

/// The directory of the compaction summaries (M9).
pub const SUMMARIES: &str = "summaries";

/// The most of the index that goes into a step's prompt (M33).
const INDEX_CAP: usize = 4 * 1024;

/// The most of an entry `show` returns, the end kept (D28, D100).
const SHOW_LIMIT: usize = 16 * 1024;

/// The longest file name of an entry, without `.md`.
const NAME_LIMIT: usize = 64;

/// Where the memory of the workspace is (AGL-006): the agent asks it at
/// each step and for each write; the interactive session asks it for
/// `/remember`.
pub trait Home: Send + Sync {
    /// The memory directory when the workspace has a store; nothing is
    /// created.
    fn read(&self) -> Option<PathBuf>;
    /// The memory directory, the store registered first when it is not
    /// yet (M23); the lines say what the resolution did (a move, an
    /// adoption, the store made).
    fn write(&self) -> Result<(PathBuf, Vec<String>), Error>;
}

/// The memory in the store of the project of `workspace`, found the way
/// the forge finds it (M17, M24).
pub struct StoreHome {
    pub workspace: PathBuf,
    pub auto_rehome: bool,
    pub lang: Lang,
}

impl StoreHome {
    fn resolved(&self) -> Result<(crate::forge::Workspace, Vec<String>), Error> {
        let paths = crate::paths::Paths::from_env(|name| std::env::var_os(name))?;
        crate::forge::resolve_store(&paths.data, &self.workspace, self.lang, self.auto_rehome)
    }
}

impl Home for StoreHome {
    fn read(&self) -> Option<PathBuf> {
        let (ws, _) = self.resolved().ok()?;
        ws.dir
            .join(crate::forge::MARKER_FILE)
            .is_file()
            .then(|| ws.dir.join("memory"))
    }

    fn write(&self) -> Result<(PathBuf, Vec<String>), Error> {
        let (ws, mut lines) = self.resolved()?;
        lines.extend(crate::forge::materialize(&ws, self.lang)?);
        Ok((ws.dir.join("memory"), lines))
    }
}

/// What the memory tool did, for the event and the marker (M28, M36).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Added,
    Edited,
    Removed,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Added, Kind::Edited, Kind::Removed];

    /// The word of the session event.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Added => "added",
            Kind::Edited => "edited",
            Kind::Removed => "removed",
        }
    }

    /// The sign in the marker, `[memory: +name]`.
    pub fn sign(self) -> char {
        match self {
            Kind::Added => '+',
            Kind::Edited => '~',
            Kind::Removed => '-',
        }
    }
}

/// A successful write of the `memory` tool or of `/remember`: the entry,
/// the kind, and the text that was written (none for a removal).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Write {
    pub name: String,
    pub kind: Kind,
    pub text: Option<String>,
}

/// The arguments of the `memory` tool (M32); a field that the action does
/// not use is empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
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
    pub fn from_call(args: &[(String, crate::action::Value)]) -> Args {
        let text = |name: &str| {
            args.iter()
                .find_map(|(n, value)| match value {
                    crate::action::Value::Text(text) if n == name => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        Args {
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

/// Runs one action of the tool over the memory directory `dir` (M32).
pub fn run(dir: &Path, args: &Args) -> (ToolResult, Option<Write>) {
    match args.action.as_str() {
        "add" => add(dir, &args.title, &args.hook, &args.text),
        "show" => (show(dir, &args.name), None),
        "edit" => edit_entry(dir, &args.name, &args.old, &args.new),
        "remove" => remove(dir, &args.name),
        other => fail(format!(
            "Unknown action `{other}`; use add, show, edit or remove."
        )),
    }
}

/// A line of the index, as one line of text.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A title for the index link: one line, without brackets.
fn clean_title(title: &str) -> String {
    one_line(title).replace('[', "(").replace(']', ")")
}

/// A name is the file name of an entry without `.md`: the letters and
/// digits of any script and `-`, as a slug is (M31).
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= NAME_LIMIT + 8
        && name.chars().all(|c| c.is_alphanumeric() || c == '-')
}

fn entry_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.md"))
}

/// The slug of `title` for a file name, `-2`, `-3` on a clash (M31, M36).
fn free_name(dir: &Path, title: &str) -> Option<String> {
    let mut base = slug(title);
    if base.chars().count() > NAME_LIMIT {
        base = base.chars().take(NAME_LIMIT).collect();
        base = base.trim_end_matches('-').to_owned();
    }
    if base.is_empty() {
        return None;
    }
    let mut name = base.clone();
    let mut n = 2;
    while entry_path(dir, &name).exists() {
        name = format!("{base}-{n}");
        n += 1;
    }
    Some(name)
}

/// `add title hook text` (M32): writes `<slug>.md` and appends its index
/// line.
pub fn add(dir: &Path, title: &str, hook: &str, text: &str) -> (ToolResult, Option<Write>) {
    let title = clean_title(title);
    let hook = one_line(hook);
    if title.is_empty() || hook.is_empty() || text.trim().is_empty() {
        return fail("`add` needs a title, a hook (one line) and a text.");
    }
    let Some(name) = free_name(dir, &title) else {
        return fail("The title needs letters or digits to make a file name.");
    };
    if let Err(error) = fs::create_dir_all(dir) {
        return fail(format!("Cannot create {}: {error}", dir.display()));
    }
    let body = format!("# {title}\n\n{}\n", text.trim_end());
    if let Err(error) = write_atomic(&entry_path(dir, &name), &body) {
        return fail(format!("Cannot write the entry: {error:?}"));
    }
    let mut index = fs::read_to_string(dir.join(INDEX)).unwrap_or_default();
    if !index.is_empty() && !index.ends_with('\n') {
        index.push('\n');
    }
    index.push_str(&format!("- [{title}]({name}.md) — {hook}\n"));
    if let Err(error) = write_atomic(&dir.join(INDEX), &index) {
        let _ = fs::remove_file(entry_path(dir, &name));
        return fail(format!("Cannot write the index: {error:?}"));
    }
    ok(
        format!("Remembered `{name}`."),
        Write {
            name,
            kind: Kind::Added,
            text: Some(text.trim_end().to_owned()),
        },
    )
}

/// The most of the text that goes into the index line of `/remember` (M37:
/// the hook is the text).
const HOOK_LIMIT: usize = 120;

/// The index hook of a `/remember` text: the text on one line, cut with
/// `…` when long.
pub fn hook_of(text: &str) -> String {
    let line = one_line(text);
    if line.chars().count() <= HOOK_LIMIT {
        return line;
    }
    let cut: String = line.chars().take(HOOK_LIMIT - 1).collect();
    format!("{}…", cut.trim_end())
}

/// `/remember name | text` (M34, M37): the program writes the entry,
/// its index hook being the text.
pub fn remember(dir: &Path, name: &str, text: &str) -> (ToolResult, Option<Write>) {
    add(dir, name, &hook_of(text), text)
}

/// `show name` (M32): the entry's full text, the end kept when it is long.
pub fn show(dir: &Path, name: &str) -> ToolResult {
    let missing = || ToolResult {
        outcome: Outcome::Fail,
        content: format!("No memory entry `{name}`."),
    };
    if !valid_name(name) {
        return missing();
    }
    let Ok(text) = fs::read_to_string(entry_path(dir, name)) else {
        return missing();
    };
    let content = if text.len() > SHOW_LIMIT {
        let mut start = text.len() - SHOW_LIMIT;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        format!(
            "[output truncated: first {start} bytes omitted]\n{}",
            &text[start..]
        )
    } else {
        text
    };
    ToolResult {
        outcome: Outcome::Ok,
        content,
    }
}

/// `edit name old new` (M32): one unique occurrence in the entry's text, as
/// the workspace `edit` does it.
pub fn edit_entry(dir: &Path, name: &str, old: &str, new: &str) -> (ToolResult, Option<Write>) {
    if !valid_name(name) || !entry_path(dir, name).is_file() {
        return fail(format!("No memory entry `{name}`."));
    }
    let result = edit(dir, &format!("{name}.md"), old, new);
    if result.outcome != Outcome::Ok {
        return (result, None);
    }
    let write = Write {
        name: name.to_owned(),
        kind: Kind::Edited,
        text: Some(new.to_owned()),
    };
    (result, Some(write))
}

/// `remove name` (M32): deletes the file and its index line.
pub fn remove(dir: &Path, name: &str) -> (ToolResult, Option<Write>) {
    if !valid_name(name) || !entry_path(dir, name).is_file() {
        return fail(format!("No memory entry `{name}`."));
    }
    let link = format!("]({name}.md)");
    let index = fs::read_to_string(dir.join(INDEX)).unwrap_or_default();
    let kept: String = index
        .lines()
        .filter(|line| !line.contains(&link))
        .map(|line| format!("{line}\n"))
        .collect();
    if let Err(error) = write_atomic(&dir.join(INDEX), &kept) {
        return fail(format!("Cannot write the index: {error:?}"));
    }
    if let Err(error) = fs::remove_file(entry_path(dir, name)) {
        return fail(format!("Cannot remove the entry: {error}"));
    }
    ok(
        format!("Removed `{name}`."),
        Write {
            name: name.to_owned(),
            kind: Kind::Removed,
            text: None,
        },
    )
}

/// The index as the prompt carries it (M33): the lines of `MEMORY.md`, at
/// most 4 KB, the newest kept, with `[memory truncated]` first when the
/// start was cut; `None` when there is nothing.
pub fn index_text(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join(INDEX)).ok()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.len() <= INDEX_CAP {
        return Some(text.to_owned());
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut size = 0;
    for line in text.lines().rev() {
        if size + line.len() + 1 > INDEX_CAP {
            break;
        }
        size += line.len() + 1;
        kept.push(line);
    }
    kept.reverse();
    Some(format!("[memory truncated]\n{}", kept.join("\n")))
}

/// Writes the summary of a compaction to `summaries/<date>-<session>.md`
/// (M9); a second summary of a session the same day is appended below the
/// first.
pub fn write_summary(dir: &Path, date: &str, session: &str, summary: &str) -> Result<(), String> {
    let folder = dir.join(SUMMARIES);
    fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    let path = folder.join(format!("{date}-{session}.md"));
    let mut text = fs::read_to_string(&path).unwrap_or_default();
    if !text.is_empty() {
        text.push_str("\n---\n\n");
    }
    text.push_str(summary.trim_end());
    text.push('\n');
    write_atomic(&path, &text).map_err(|e| format!("{}: {e:?}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let dir = std::env::temp_dir().join(format!("agentLIBRE-memory-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            Dir(dir)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn added(dir: &Path, title: &str, hook: &str, text: &str) -> String {
        let (result, write) = add(dir, title, hook, text);
        assert_eq!(result.outcome, Outcome::Ok, "{}", result.content);
        write.unwrap().name
    }

    #[test]
    fn add_writes_the_entry_and_its_index_line() {
        let dir = Dir::new("add");
        let (result, write) = add(&dir.0, "Build and test", "cargo build / cargo test", "Run `cargo build`.\nThen `cargo test`.\n");
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
        let dir = Dir::new("clash");
        assert_eq!(added(&dir.0, "Setup", "one", "a"), "setup");
        assert_eq!(added(&dir.0, "setup", "two", "b"), "setup-2");
        assert_eq!(added(&dir.0, "SETUP!", "three", "c"), "setup-3");
        // Brackets and newlines of a title and a hook do not break the line.
        let name = added(&dir.0, "A [odd]\ntitle", "a hook\nover lines", "t");
        assert_eq!(name, "a-odd-title");
        let index = fs::read_to_string(dir.0.join(INDEX)).unwrap();
        assert!(index.contains("- [A (odd) title](a-odd-title.md) — a hook over lines\n"));
        for (title, hook, text) in [("", "h", "t"), ("t", "", "t"), ("t", "h", "  "), ("!!!", "h", "t")] {
            let (result, write) = add(&dir.0, title, hook, text);
            assert_eq!(result.outcome, Outcome::Fail, "{title:?}");
            assert!(write.is_none());
        }
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 5);
    }

    #[test]
    fn a_long_title_makes_a_short_name() {
        let dir = Dir::new("long");
        let name = added(&dir.0, &"word ".repeat(40), "h", "t");
        assert!(name.chars().count() <= NAME_LIMIT, "{name}");
        assert!(!name.ends_with('-'));
        // Letters of any script make a name.
        assert_eq!(added(&dir.0, "Збірка і тести", "h", "t"), "збірка-і-тести");
    }

    #[test]
    fn show_returns_the_text_and_the_end_of_a_long_one() {
        let dir = Dir::new("show");
        let name = added(&dir.0, "Notes", "h", "short text");
        let shown = show(&dir.0, &name);
        assert_eq!(shown.outcome, Outcome::Ok);
        assert_eq!(shown.content, "# Notes\n\nshort text\n");
        assert_eq!(show(&dir.0, "nope").outcome, Outcome::Fail);
        assert_eq!(show(&dir.0, "../etc/passwd").outcome, Outcome::Fail);
        assert_eq!(show(&dir.0, "").outcome, Outcome::Fail);
        let long = added(&dir.0, "Long", "h", &"é".repeat(20_000));
        let shown = show(&dir.0, &long);
        assert!(shown.content.starts_with("[output truncated: first "));
        assert!(shown.content.len() <= SHOW_LIMIT + 60);
        assert!(shown.content.ends_with("é\n"));
    }

    #[test]
    fn edit_changes_one_unique_occurrence() {
        let dir = Dir::new("edit");
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
        assert!(show(&dir.0, &name).content.contains("port 18090 and port 18081"));
        // Twice, or not at all: a failed call, nothing written.
        let (twice, none) = edit_entry(&dir.0, &name, "port", "x");
        assert_eq!(twice.outcome, Outcome::Fail);
        assert!(none.is_none());
        assert_eq!(edit_entry(&dir.0, &name, "absent", "x").0.outcome, Outcome::Fail);
        assert_eq!(edit_entry(&dir.0, "nope", "a", "b").0.outcome, Outcome::Fail);
        assert_eq!(edit_entry(&dir.0, "../x", "a", "b").0.outcome, Outcome::Fail);
    }

    #[test]
    fn remove_deletes_the_file_and_only_its_index_line() {
        let dir = Dir::new("remove");
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
    fn run_picks_the_action_and_refuses_an_unknown_one() {
        let dir = Dir::new("run");
        let args = Args {
            action: "add".into(),
            title: "T".into(),
            hook: "h".into(),
            text: "x".into(),
            ..Args::default()
        };
        assert_eq!(run(&dir.0, &args).0.outcome, Outcome::Ok);
        let show_args = Args {
            action: "show".into(),
            name: "t".into(),
            ..Args::default()
        };
        assert_eq!(run(&dir.0, &show_args).0.content, "# T\n\nx\n");
        let bad = Args {
            action: "list".into(),
            ..Args::default()
        };
        let (result, write) = run(&dir.0, &bad);
        assert_eq!(result.outcome, Outcome::Fail);
        assert!(result.content.contains("Unknown action `list`"));
        assert!(write.is_none());
    }

    #[test]
    fn the_index_for_the_prompt_is_whole_when_small_and_the_newest_when_cut() {
        let dir = Dir::new("index");
        assert_eq!(index_text(&dir.0), None);
        fs::create_dir_all(&dir.0).unwrap();
        fs::write(dir.0.join(INDEX), "  \n\n").unwrap();
        assert_eq!(index_text(&dir.0), None);
        added(&dir.0, "One", "first", "1");
        added(&dir.0, "Two", "second", "2");
        assert_eq!(
            index_text(&dir.0).unwrap(),
            "- [One](one.md) — first\n- [Two](two.md) — second"
        );
        // A hand edit is seen at once (M34).
        fs::write(dir.0.join(INDEX), "- [Hand](hand.md) — by hand\n").unwrap();
        assert_eq!(index_text(&dir.0).unwrap(), "- [Hand](hand.md) — by hand");
        // Over 4 KB: the marker, then the newest lines that fit.
        let many: String = (0..200)
            .map(|n| format!("- [Entry {n}](entry-{n}.md) — {}\n", "x".repeat(40)))
            .collect();
        fs::write(dir.0.join(INDEX), &many).unwrap();
        let cut = index_text(&dir.0).unwrap();
        assert!(cut.starts_with("[memory truncated]\n"));
        assert!(cut.len() <= INDEX_CAP + "[memory truncated]\n".len());
        assert!(cut.ends_with("— xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"));
        assert!(cut.contains("Entry 199"));
        assert!(!cut.contains("Entry 0]"));
    }

    #[test]
    fn remember_writes_an_entry_whose_hook_is_the_text() {
        let dir = Dir::new("remember");
        let (result, write) = remember(&dir.0, "Build and test", "cargo build / cargo test");
        assert_eq!(result.outcome, Outcome::Ok);
        assert_eq!(write.unwrap().name, "build-and-test");
        assert_eq!(
            fs::read_to_string(dir.0.join(INDEX)).unwrap(),
            "- [Build and test](build-and-test.md) — cargo build / cargo test\n"
        );
        // A long text is cut in the index and whole in the entry.
        let long = "word ".repeat(100);
        remember(&dir.0, "Long one", &long);
        let index = fs::read_to_string(dir.0.join(INDEX)).unwrap();
        let line = index.lines().last().unwrap();
        assert!(line.ends_with('…'));
        assert!(line.chars().count() < 160);
        assert!(show(&dir.0, "long-one").content.contains(long.trim_end()));
        assert_eq!(hook_of("a\nb  c"), "a b c");
        assert_eq!(remember(&dir.0, "", "t").0.outcome, Outcome::Fail);
    }

    #[test]
    fn a_summary_goes_to_the_summaries_directory_by_date_and_session() {
        let dir = Dir::new("summary");
        write_summary(&dir.0, "2026-10-21", "01abc", "First summary.\n").unwrap();
        let path = dir.0.join(SUMMARIES).join("2026-10-21-01abc.md");
        assert_eq!(fs::read_to_string(&path).unwrap(), "First summary.\n");
        write_summary(&dir.0, "2026-10-21", "01abc", "Second.").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "First summary.\n\n---\n\nSecond.\n"
        );
        // Not in the index: the prompt never carries it (M16).
        assert_eq!(index_text(&dir.0), None);
    }
}
