//! The memory of a workspace (AGL-006): entries kept across sessions in
//! `<store>/memory/`, one Markdown file each and an index, `MEMORY.md`,
//! one line per entry (M31): `- [title](name.md) — a one-line hook`. The
//! model writes them with the `memory` tool, the human with `/remember` or
//! by hand (M30, M32, M34); the human looks at them and removes them with
//! `agl memory` and `/memory` (AGL-060).

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::action::Outcome;
use crate::error::Error;
use crate::forge::task_spec::slug;
use crate::forge::write_atomic;
use crate::i18n::Lang;
use crate::tools::{Output, ToolResult, edit, limited};

/// The index file of the memory directory (M31).
pub const INDEX: &str = "MEMORY.md";

/// The directory of the compaction summaries (M9).
pub const SUMMARIES: &str = "summaries";

/// The most of the index that goes into a step's prompt (M33).
const INDEX_CAP: usize = 4 * 1024;

/// What is shown of a line that does not fit `INDEX_CAP` by itself (AGL-058,
/// D10): its name and the start of its text, leaving room for the other lines.
const LONG_LINE_SHOWN: usize = 1024;

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
        of_store(&ws.dir)
    }

    fn write(&self) -> Result<(PathBuf, Vec<String>), Error> {
        let (ws, mut lines) = self.resolved()?;
        lines.extend(crate::forge::materialize(&ws, self.lang)?);
        Ok((ws.dir.join("memory"), lines))
    }
}

/// The memory directory of the store at `store`, when it is a store; nothing
/// is created.
pub fn of_store(store: &Path) -> Option<PathBuf> {
    store
        .join(crate::forge::MARKER_FILE)
        .is_file()
        .then(|| store.join("memory"))
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

/// A line of the index, as one line of text.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A title for the index link: one line, without brackets.
fn clean_title(title: &str) -> String {
    one_line(title).replace('[', "(").replace(']', ")")
}

/// A name is the file name of an entry without `.md`: the letters and
/// digits of any script and `-`, as a slug is (M31). The index is no entry:
/// removing it would take every line with it (AGL-060).
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= NAME_LIMIT + 8
        && name.chars().all(|c| c.is_alphanumeric() || c == '-')
        && format!("{name}.md") != INDEX
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
/// line. The body is the `text`; when it is empty, the `call_text` that
/// preceded the call (AGL-029, D1). A failure names the missing fields
/// (AGL-029, D2).
pub fn add(
    dir: &Path,
    title: &str,
    hook: &str,
    text: &str,
    call_text: Option<&str>,
) -> (ToolResult, Option<Write>) {
    let title = clean_title(title);
    let hook = one_line(hook);
    let call_body = call_text.map(str::trim).filter(|text| !text.is_empty());
    let (body, from_call) = match text.trim() {
        text if !text.is_empty() => (text.trim_end().to_owned(), false),
        _ => match call_body {
            Some(body) => (body.to_owned(), true),
            None => (String::new(), false),
        },
    };
    let mut missing: Vec<String> = Vec::new();
    if title.is_empty() {
        missing.push("a title".into());
    }
    if hook.is_empty() {
        missing.push("a hook (one line)".into());
    }
    if body.is_empty() {
        missing.push("a text".into());
    }
    if !missing.is_empty() {
        let list = if missing.len() > 1 {
            let last = missing.pop().unwrap();
            format!("{} and {}", missing.join(", "), last)
        } else {
            missing.pop().unwrap()
        };
        return fail(format!("`add` needs {list}."));
    }
    let Some(name) = free_name(dir, &title) else {
        return fail("The title needs letters or digits to make a file name.");
    };
    if let Err(error) = fs::create_dir_all(dir) {
        return fail(format!("Cannot create {}: {error}", dir.display()));
    }
    let entry = format!("# {title}\n\n{body}\n");
    if let Err(error) = write_atomic(&entry_path(dir, &name), &entry) {
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
    let note = if from_call {
        " (the body was taken from the text of the call)"
    } else {
        ""
    };
    ok(
        format!("Remembered `{name}`{note}."),
        Write {
            name,
            kind: Kind::Added,
            text: Some(body),
        },
    )
}

/// The index hook of a `/remember` text: the whole text on one line, not
/// cut (M37: the hook is the text; AGL-043 F19).
pub fn hook_of(text: &str) -> String {
    one_line(text)
}

/// `/remember name | text` (M34, M37): the program writes the entry,
/// its index hook being the text.
pub fn remember(dir: &Path, name: &str, text: &str) -> (ToolResult, Option<Write>) {
    add(dir, name, &hook_of(text), text, None)
}

/// `show name` (M32): the entry's text, cut like any tool's output when it is
/// long (AGL-049); the whole is the entry's own file.
pub fn show(dir: &Path, name: &str, output: &Output<'_>) -> ToolResult {
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
    let content = limited(&text, output.tokens, &entry_path(dir, name), output.count);
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

/// `update name title hook` (D6, AGL-032): rewrites the entry's first
/// heading and its index line (title + hook) in one call; at least one
/// of `title` and `hook` is given, the other is kept. The name is not
/// touched — the file name is the entry's identity. The entry file is
/// written before the index, as `add` does it: an index failure leaves
/// the line stale, never an orphaned entry.
pub fn update(dir: &Path, name: &str, title: &str, hook: &str) -> (ToolResult, Option<Write>) {
    if !valid_name(name) || !entry_path(dir, name).is_file() {
        return fail(format!("No memory entry `{name}`."));
    }
    let mut title = clean_title(title);
    let hook = one_line(hook);
    if title.is_empty() && hook.is_empty() {
        return fail("`update` needs a title and/or a hook.");
    }
    let Ok(text) = fs::read_to_string(entry_path(dir, name)) else {
        return fail(format!("Cannot read the entry `{name}`."));
    };
    let Some(first) = text.lines().next() else {
        return fail(format!("The entry `{name}` is empty."));
    };
    let Some(old_title) = first.strip_prefix("# ") else {
        return fail(format!("The entry `{name}` has no `# ` heading."));
    };
    if title.is_empty() {
        title = old_title.to_string();
    }
    // The index line of the entry: `- [title](name.md) — hook`; the
    // hook is everything after the first ` — ` past the link, so a hook
    // with ` — ` of its own is kept whole.
    let index = fs::read_to_string(dir.join(INDEX)).unwrap_or_default();
    let link = format!("]({name}.md)");
    let Some(line) = index.lines().find(|line| line.contains(&link)) else {
        return fail(format!("No index line for `{name}` in {INDEX}."));
    };
    let after = &line[line.find(&link).unwrap() + link.len()..];
    let old_hook = after.strip_prefix(" — ").unwrap_or(after);
    let hook = if hook.is_empty() {
        old_hook.to_string()
    } else {
        hook
    };
    let new_line = format!("- [{title}]({name}.md) — {hook}");
    if title != old_title {
        // `first` is the heading without its newline; the rest of the
        // text keeps it, so no newline is added here.
        let entry = format!("# {title}{}", &text[first.len()..]);
        if let Err(error) = write_atomic(&entry_path(dir, name), &entry) {
            return fail(format!("Cannot write the entry: {error:?}"));
        }
    }
    let mut new_index = String::with_capacity(index.len() + new_line.len());
    for l in index.lines() {
        new_index.push_str(if l == line { &new_line } else { l });
        new_index.push('\n');
    }
    if let Err(error) = write_atomic(&dir.join(INDEX), &new_index) {
        return fail(format!("Cannot write the index: {error:?}"));
    }
    ok(
        format!("Updated `{name}`."),
        Write {
            name: name.to_owned(),
            kind: Kind::Edited,
            text: Some(new_line),
        },
    )
}

/// `remove name` (M32): deletes the file and its index line.
pub fn remove(dir: &Path, name: &str) -> (ToolResult, Option<Write>) {
    if !valid_name(name) || !entry_path(dir, name).is_file() {
        return fail(format!("No memory entry `{name}`."));
    }
    let link = format!("]({name}.md)");
    // An index that cannot be read is not an empty one: writing the lines
    // kept would lose all of them (AGL-060).
    let index = match fs::read_to_string(dir.join(INDEX)) {
        Ok(index) => index,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => return fail(format!("Cannot read the index: {error}")),
    };
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
    let mut kept = newest_lines(text);
    kept.reverse();
    Some(format!("[memory truncated]\n{}", kept.join("\n")))
}

/// The lines of an index over the cap that the prompt carries, newest first.
fn newest_lines(text: &str) -> Vec<std::borrow::Cow<'_, str>> {
    let mut kept = Vec::new();
    let mut size = 0;
    for line in text.lines().rev() {
        let line = shown_line(line);
        if size + line.len() + 1 > INDEX_CAP {
            break;
        }
        size += line.len() + 1;
        kept.push(line);
    }
    kept
}

/// A line of the index as the prompt carries it: whole, unless it alone is over
/// the cap, when it would take every other line out with it (AGL-058, D10).
/// `MEMORY.md` and the entry keep all of it.
fn shown_line(line: &str) -> std::borrow::Cow<'_, str> {
    if line.len() < INDEX_CAP {
        return line.into();
    }
    let mut end = LONG_LINE_SHOWN;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end]).into()
}

/// An entry as `agl memory list` shows it (AGL-060, D4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub title: String,
    /// Empty for a file with no line.
    pub hook: String,
    /// The prompt carries its line (M33).
    pub in_prompt: bool,
    /// Its file exists.
    pub file: bool,
    /// `MEMORY.md` has its line.
    pub line: bool,
}

/// The title, the name and the hook of a line of the index; `None` for a line
/// that is no entry's, as a hand-written heading.
fn index_line(line: &str) -> Option<(&str, &str, &str)> {
    let open = line.find('[')?;
    let link = open + line[open..].find("](")?;
    let target = &line[link + 2..];
    let close = target.find(".md)")?;
    let name = &target[..close];
    if !valid_name(name) {
        return None;
    }
    let after = &target[close + ".md)".len()..];
    let hook = after.strip_prefix(" — ").unwrap_or(after).trim();
    Some((&line[open + 1..link], name, hook))
}

/// The names of the entry files of `dir`; none when `dir` does not exist.
fn entry_files(dir: &Path) -> Result<BTreeSet<String>, Error> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(source) => {
            return Err(Error::Read {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    let mut names = BTreeSet::new();
    for entry in entries {
        let path = entry
            .map_err(|source| Error::Read {
                path: dir.to_path_buf(),
                source,
            })?
            .path();
        if path.is_file()
            && path.extension().is_some_and(|ext| ext == "md")
            && let Some(name) = path.file_stem().and_then(|stem| stem.to_str())
            && valid_name(name)
        {
            names.insert(name.to_owned());
        }
    }
    Ok(names)
}

/// Every entry of `dir` (AGL-060, D4): the lines of the index in their order,
/// then the files with no line by name. An index that cannot be read is an
/// error, not an empty memory.
pub fn list(dir: &Path) -> Result<Vec<Listed>, Error> {
    let path = dir.join(INDEX);
    let index = match fs::read_to_string(&path) {
        Ok(index) => index,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(source) => return Err(Error::Read { path, source }),
    };
    let index = index.trim();
    let total = index.lines().count();
    let carried = if index.len() <= INDEX_CAP {
        total
    } else {
        newest_lines(index).len()
    };
    let mut files = entry_files(dir)?;
    let mut entries = Vec::new();
    let mut lined = BTreeSet::new();
    for (number, line) in index.lines().enumerate() {
        let Some((title, name, hook)) = index_line(line) else {
            continue;
        };
        lined.insert(name.to_owned());
        entries.push(Listed {
            name: name.to_owned(),
            title: title.to_owned(),
            hook: hook.to_owned(),
            in_prompt: number + carried >= total,
            file: files.contains(name),
            line: true,
        });
    }
    files.retain(|name| !lined.contains(name));
    for name in files {
        let heading = fs::read_to_string(entry_path(dir, &name)).ok();
        let title = heading
            .as_deref()
            .and_then(|text| text.lines().next())
            .and_then(|first| first.strip_prefix("# "))
            .unwrap_or(&name)
            .to_owned();
        entries.push(Listed {
            name,
            title,
            hook: String::new(),
            in_prompt: false,
            file: true,
            line: false,
        });
    }
    Ok(entries)
}

/// Whether `name` is an entry with a file.
pub fn exists(dir: &Path, name: &str) -> bool {
    valid_name(name) && entry_path(dir, name).is_file()
}

/// The whole text of an entry, for the human: [`show`] cuts a long one for
/// the model (AGL-060).
pub fn whole(dir: &Path, name: &str) -> Result<String, Error> {
    let missing = || Error::MemoryNoEntry {
        name: name.to_owned(),
    };
    if !valid_name(name) {
        return Err(missing());
    }
    let path = entry_path(dir, name);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == ErrorKind::NotFound => Err(missing()),
        Err(source) => Err(Error::Read { path, source }),
    }
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
        let (result, write) = add(dir, title, hook, text, None);
        assert_eq!(result.outcome, Outcome::Ok, "{}", result.content);
        write.unwrap().name
    }

    #[test]
    fn add_writes_the_entry_and_its_index_line() {
        let dir = Dir::new("add");
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
            let (result, write) = add(&dir.0, title, hook, text, None);
            assert_eq!(result.outcome, Outcome::Fail, "{title:?}");
            assert!(write.is_none());
        }
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 5);
    }

    #[test]
    fn add_takes_the_body_from_the_text_of_the_call_when_the_parameter_is_empty() {
        let dir = Dir::new("call-body");
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
        let dir = Dir::new("missing-fields");
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
    fn a_long_title_makes_a_short_name() {
        let dir = Dir::new("long");
        let name = added(&dir.0, &"word ".repeat(40), "h", "t");
        assert!(name.chars().count() <= NAME_LIMIT, "{name}");
        assert!(!name.ends_with('-'));
        // Letters of any script make a name.
        assert_eq!(added(&dir.0, "Збірка і тести", "h", "t"), "збірка-і-тести");
    }

    #[test]
    fn show_returns_the_text_and_cuts_a_long_one() {
        let dir = Dir::new("show");
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
        let dir = Dir::new("update");
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
        let dir = Dir::new("update-misfires");
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
    fn a_line_over_the_cap_does_not_take_the_others_out() {
        // AGL-058, D10.
        let dir = Dir::new("long-line");
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
        let dir = Dir::new("remember");
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
        let dir = Dir::new("index-no-entry");
        added(&dir.0, "One", "first", "1");
        assert!(!exists(&dir.0, "MEMORY"));
        assert_eq!(remove(&dir.0, "MEMORY").0.outcome, Outcome::Fail);
        assert!(matches!(whole(&dir.0, "MEMORY"), Err(Error::MemoryNoEntry { .. })));
        assert_eq!(list(&dir.0).unwrap().len(), 1);
        assert!(dir.0.join(INDEX).is_file());
    }

    #[test]
    fn remove_does_not_empty_an_index_it_cannot_read() {
        // AGL-060.
        let dir = Dir::new("remove-bad-index");
        let one = added(&dir.0, "One", "first", "1");
        fs::write(dir.0.join(INDEX), b"- [One](one.md) \xff\n").unwrap();
        let (result, write) = remove(&dir.0, &one);
        assert_eq!(result.outcome, Outcome::Fail);
        assert!(write.is_none());
        assert_eq!(fs::read(dir.0.join(INDEX)).unwrap(), b"- [One](one.md) \xff\n");
        assert!(entry_path(&dir.0, &one).exists());
    }

    #[test]
    fn list_gives_the_lines_in_order_and_then_the_files_with_no_line() {
        // AGL-060, D4.
        let dir = Dir::new("list");
        assert_eq!(list(&dir.0).unwrap(), Vec::new());
        added(&dir.0, "One", "first — with a dash", "1");
        added(&dir.0, "Gone", "second", "2");
        added(&dir.0, "Stray", "third", "3");
        fs::remove_file(entry_path(&dir.0, "gone")).unwrap();
        let mut index = fs::read_to_string(dir.0.join(INDEX)).unwrap();
        index = index.replace("- [Stray](stray.md) — third\n", "");
        fs::write(dir.0.join(INDEX), format!("# A heading by hand\n{index}")).unwrap();
        fs::write(entry_path(&dir.0, "bare"), "no heading\n").unwrap();
        fs::create_dir_all(dir.0.join(SUMMARIES)).unwrap();
        let entries = list(&dir.0).unwrap();
        let shape: Vec<(&str, &str, &str, bool, bool, bool)> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.title.as_str(), e.hook.as_str(), e.in_prompt, e.file, e.line))
            .collect();
        assert_eq!(
            shape,
            [
                ("one", "One", "first — with a dash", true, true, true),
                ("gone", "Gone", "second", true, false, true),
                ("bare", "bare", "", false, true, false),
                ("stray", "Stray", "", false, true, false),
            ]
        );
    }

    #[test]
    fn list_marks_the_lines_the_prompt_does_not_carry() {
        // AGL-060, D4: the same lines as the index of the prompt (M33).
        let dir = Dir::new("list-cut");
        for n in 0..200 {
            added(&dir.0, &format!("Entry {n}"), &"x".repeat(40), "t");
        }
        let entries = list(&dir.0).unwrap();
        assert_eq!(entries.len(), 200);
        let prompt = index_text(&dir.0).unwrap();
        for entry in &entries {
            let link = format!("]({}.md)", entry.name);
            assert_eq!(entry.in_prompt, prompt.contains(&link), "{}", entry.name);
        }
        assert!(!entries[0].in_prompt);
        assert!(entries[199].in_prompt);
    }

    #[test]
    fn list_tells_an_index_it_cannot_read_from_an_empty_memory() {
        // AGL-060.
        let dir = Dir::new("list-bad-index");
        fs::create_dir_all(&dir.0).unwrap();
        fs::write(dir.0.join(INDEX), b"\xff\n").unwrap();
        assert!(matches!(list(&dir.0), Err(Error::Read { .. })));
    }

    #[test]
    fn whole_gives_a_long_entry_uncut() {
        // AGL-060.
        let dir = Dir::new("whole");
        let long = "é".repeat(20_000);
        let name = added(&dir.0, "Long", "h", &long);
        assert_eq!(whole(&dir.0, &name).unwrap(), format!("# Long\n\n{long}\n"));
        for name in ["nope", "../etc/passwd", ""] {
            assert!(matches!(whole(&dir.0, name), Err(Error::MemoryNoEntry { .. })), "{name}");
        }
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

    fn out() -> Output<'static> {
        Output {
            tokens: 4096,
            file: Default::default(),
            count: &|text| Some(text.len() / 4),
        }
    }
}
