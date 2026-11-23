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

use crate::error;
use crate::forge::task_spec::slug;
use agentlibre_common::file::write_atomic;
use agentlibre_common::i18n::{Lang, Msg, m};
use agentlibre_common::tr;
use agentlibre_common::error::Error as CommonError;

/// A memory entry not found or not removed (AGL-060; AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// `agl memory` or `/memory`: no entry `name` (AGL-060).
    NoEntry { name: String },
    /// A removal with no terminal to ask on and no `--yes` (AGL-060, D3, D7).
    RemoveNeedsYes { name: String },
    /// A removal that could not be done (AGL-060).
    RemoveFailed { reason: String },
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::NoEntry { name } => tr!(lang, NO_ENTRY, name = name),
            Self::RemoveNeedsYes { name } => tr!(lang, REMOVE_NEEDS_YES, name = name),
            Self::RemoveFailed { reason } => tr!(lang, REMOVE_FAILED, reason = reason),
        }
    }
}

/// `Error::NoEntry`.
const NO_ENTRY: Msg = Msg {
    en: "no memory entry {name}",
    uk: "немає запису пам'яті {name}",
};

/// `Error::RemoveNeedsYes`.
const REMOVE_NEEDS_YES: Msg = Msg {
    en: "{name} not removed: there is no terminal to ask on; add --yes",
    uk: "{name} не видалено: немає термінала, щоб спитати; додай --yes",
};

/// `Error::RemoveFailed`.
const REMOVE_FAILED: Msg = Msg {
    en: "not removed: {reason}",
    uk: "не видалено: {reason}",
};

/// Why the memory did not do what it was asked: a refusal of the store,
/// in the words the model and the human both read (AGL-121, D80).
#[derive(Debug)]
pub enum Refusal {
    /// `add` without some of its fields.
    Missing { title: bool, hook: bool, text: bool },
    /// `update` with neither a title nor a hook.
    NoUpdate,
    /// No entry of that name.
    NoEntry { name: String },
    /// The memory's folder cannot be made.
    CreateDir { dir: PathBuf, source: std::io::Error },
    /// The memory's folder cannot be locked (D49).
    Lock { dir: PathBuf, source: std::io::Error },
    /// The title has no letters or digits to make a file name.
    NoFileName,
    /// The entry cannot be written.
    WriteEntry(CommonError),
    /// The index cannot be written.
    WriteIndex(CommonError),
    /// The index cannot be read.
    ReadIndex(std::io::Error),
    /// The entry cannot be read.
    ReadEntry { name: String },
    /// The entry is empty.
    EmptyEntry { name: String },
    /// The entry has no `# ` heading.
    NoHeading { name: String },
    /// No index line for the entry.
    NoIndexLine { name: String },
    /// The entry cannot be removed.
    RemoveEntry(std::io::Error),
    /// The replace in the entry was refused.
    Replace {
        name: String,
        refusal: crate::replace::Refusal,
    },
}

impl Refusal {
    /// The refusal in English, as the tool has always answered it.
    pub fn text(self) -> String {
        match self {
            Refusal::Missing { title, hook, text } => {
                let mut missing: Vec<&str> = Vec::new();
                if title {
                    missing.push("a title");
                }
                if hook {
                    missing.push("a hook (one line)");
                }
                if text {
                    missing.push("a text");
                }
                let list = match missing.split_last() {
                    Some((last, rest)) if !rest.is_empty() => {
                        format!("{} and {last}", rest.join(", "))
                    }
                    _ => missing.concat(),
                };
                format!("`add` needs {list}.")
            }
            Refusal::NoUpdate => "`update` needs a title and/or a hook.".into(),
            Refusal::NoEntry { name } => format!("No memory entry `{name}`."),
            Refusal::CreateDir { dir, source } => {
                format!("Cannot create {}: {source}", dir.display())
            }
            Refusal::Lock { dir, source } => format!("Cannot lock {}: {source}", dir.display()),
            Refusal::NoFileName => "The title needs letters or digits to make a file name.".into(),
            Refusal::WriteEntry(error) => format!("Cannot write the entry: {error:?}"),
            Refusal::WriteIndex(error) => format!("Cannot write the index: {error:?}"),
            Refusal::ReadIndex(error) => format!("Cannot read the index: {error}"),
            Refusal::ReadEntry { name } => format!("Cannot read the entry `{name}`."),
            Refusal::EmptyEntry { name } => format!("The entry `{name}` is empty."),
            Refusal::NoHeading { name } => format!("The entry `{name}` has no `# ` heading."),
            Refusal::NoIndexLine { name } => format!("No index line for `{name}` in {INDEX}."),
            Refusal::RemoveEntry(error) => format!("Cannot remove the entry: {error}"),
            Refusal::Replace { name, refusal } => refusal.text(&format!("{name}.md")),
        }
    }
}

/// The index file of the memory directory (M31).
pub const INDEX: &str = "MEMORY.md";

/// The directory of the compaction summaries (M9).
pub const SUMMARIES: &str = "summaries";

/// The most of the index that goes into a step's prompt (M33).
pub const INDEX_CAP: usize = 4 * 1024;

/// What is shown of a line that does not fit `INDEX_CAP` by itself (AGL-058,
/// D10): its name and the start of its text, leaving room for the other lines.
const LONG_LINE_SHOWN: usize = 1024;

/// The longest file name of an entry, without `.md`.
const NAME_LIMIT: usize = 64;

/// What starts the runtime message with the index of the memory.
pub const MEMORY_MESSAGE: &str = "workspace memory\n";

/// Where the memory of the workspace is (AGL-006): the agent asks it at
/// each step and for each write; the interactive session asks it for
/// `/remember`.
pub trait Home: Send + Sync {
    /// The memory directory when the workspace has a store; nothing is
    /// created.
    fn read(&self) -> Option<PathBuf>;
    /// [`Home::read`] that tells a store that cannot be resolved from a
    /// workspace with no store (AGL-060): `/memory` must not show a failure
    /// as an empty memory.
    fn look(&self) -> Result<Option<PathBuf>, error::Error> {
        Ok(self.read())
    }
    /// The memory directory, the store registered first when it is not
    /// yet (M23); the lines say what the resolution did (a move, an
    /// adoption, the store made).
    fn write(&self) -> Result<(PathBuf, Vec<String>), error::Error>;
    /// The workspace's memory to read, when a write or a look names it in
    /// a plane (AGL-077, D8); outside a plane, the same as [`Home::read`].
    fn read_workspace(&self) -> Option<PathBuf> {
        self.read()
    }
    /// The workspace's memory to write (AGL-077, D8).
    fn write_workspace(&self) -> Result<(PathBuf, Vec<String>), error::Error> {
        self.write()
    }
    /// The memory message the model is shown (AGL-006, M27): its heading
    /// line, then the index; in a plane, the plane's and the workspace's,
    /// each marked (AGL-077, D8). `None` when there is nothing.
    fn index(&self) -> Option<String> {
        self.read()
            .and_then(|dir| index_text(&dir))
            .map(|index| format!("{MEMORY_MESSAGE}{index}"))
    }
}

/// The memory of a run in a plane (AGL-077, D8): the plane's, which a write
/// goes to, and the workspace's beside it.
pub struct PlaneHome {
    /// The plane's name, for the mark of its index.
    pub name: String,
    /// The plane's `memory/`.
    pub dir: PathBuf,
    /// The workspace's memory.
    pub workspace: Box<dyn Home>,
}

impl Home for PlaneHome {
    fn read(&self) -> Option<PathBuf> {
        Some(self.dir.clone())
    }

    fn write(&self) -> Result<(PathBuf, Vec<String>), error::Error> {
        fs::create_dir_all(&self.dir).map_err(|source| error::Error::from(CommonError::Write {
            path: self.dir.clone(),
            source,
        }))?;
        Ok((self.dir.clone(), Vec::new()))
    }

    fn read_workspace(&self) -> Option<PathBuf> {
        self.workspace.read()
    }

    fn write_workspace(&self) -> Result<(PathBuf, Vec<String>), error::Error> {
        self.workspace.write()
    }

    fn index(&self) -> Option<String> {
        let plane = index_text(&self.dir);
        let workspace = self.workspace.read().and_then(|dir| index_text(&dir));
        if plane.is_none() && workspace.is_none() {
            return None;
        }
        let none = "(no entries)";
        Some(format!(
            "memory of the plane {name} and of the workspace\n\
             [the plane {name}; writes go here]\n{plane}\n\n\
             [the workspace; write here with store=workspace]\n{workspace}",
            name = self.name,
            plane = plane.as_deref().unwrap_or(none),
            workspace = workspace.as_deref().unwrap_or(none),
        ))
    }
}

/// The memory in the store of the project of `workspace`, found the way
/// the forge finds it (M17, M24).
pub struct StoreHome {
    pub workspace: PathBuf,
    pub auto_rehome: bool,
    pub lang: Lang,
}

impl StoreHome {
    fn resolved(&self) -> Result<(crate::forge::Workspace, Vec<String>), error::Error> {
        let paths = agentlibre_common::paths::Paths::from_env(|name| std::env::var_os(name))?;
        crate::forge::resolve_store(&paths.data, &self.workspace, self.lang, self.auto_rehome)
    }
}

impl Home for StoreHome {
    fn read(&self) -> Option<PathBuf> {
        self.look().ok().flatten()
    }

    fn look(&self) -> Result<Option<PathBuf>, error::Error> {
        let (ws, _) = self.resolved()?;
        Ok(of_store(&ws.dir))
    }

    fn write(&self) -> Result<(PathBuf, Vec<String>), error::Error> {
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

pub fn entry_path(dir: &Path, name: &str) -> PathBuf {
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

/// An advisory lock on a memory folder, held for one read-modify-write of
/// its index and its entries' files (AGL-121, D49): the `memory` tool and
/// `agl memory` both take it, so neither loses the other's line. It is on
/// the folder, not on `MEMORY.md`, which [`write_atomic`] replaces by a
/// rename; the kernel drops it when it is dropped or the process ends.
struct FolderLock(#[allow(dead_code)] fs::File);

fn lock_folder(dir: &Path) -> std::io::Result<FolderLock> {
    use std::os::fd::AsRawFd;
    let folder = fs::File::open(dir)?;
    loop {
        // SAFETY: `flock` on a descriptor this function owns.
        if unsafe { libc::flock(folder.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(FolderLock(folder));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// The lock of `dir` for a change that needs the folder to exist: `None`
/// when it does not, as there is nothing to change then.
fn lock_existing(dir: &Path) -> Result<Option<FolderLock>, Refusal> {
    match lock_folder(dir) {
        Ok(lock) => Ok(Some(lock)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Refusal::Lock {
            dir: dir.into(),
            source,
        }),
    }
}

/// `add title hook text` (M32): writes `<slug>.md` and appends its index
/// line. A refusal names the missing fields (AGL-029, D2).
pub fn add(dir: &Path, title: &str, hook: &str, body: &str) -> Result<Write, Refusal> {
    let title = clean_title(title);
    let hook = one_line(hook);
    let body = body.trim().trim_end().to_owned();
    if title.is_empty() || hook.is_empty() || body.is_empty() {
        return Err(Refusal::Missing {
            title: title.is_empty(),
            hook: hook.is_empty(),
            text: body.is_empty(),
        });
    }
    fs::create_dir_all(dir).map_err(|source| Refusal::CreateDir {
        dir: dir.into(),
        source,
    })?;
    // Held from the choice of the name to the index line (D49).
    let _lock = lock_folder(dir).map_err(|source| Refusal::Lock {
        dir: dir.into(),
        source,
    })?;
    let name = free_name(dir, &title).ok_or(Refusal::NoFileName)?;
    let entry = format!("# {title}\n\n{body}\n");
    write_atomic(&entry_path(dir, &name), &entry).map_err(Refusal::WriteEntry)?;
    let mut index = fs::read_to_string(dir.join(INDEX)).unwrap_or_default();
    if !index.is_empty() && !index.ends_with('\n') {
        index.push('\n');
    }
    index.push_str(&format!("- [{title}]({name}.md) — {hook}\n"));
    if let Err(error) = write_atomic(&dir.join(INDEX), &index) {
        let _ = fs::remove_file(entry_path(dir, &name));
        return Err(Refusal::WriteIndex(error));
    }
    Ok(Write {
        name,
        kind: Kind::Added,
        text: Some(body),
    })
}

/// The index hook of a `/remember` text: the whole text on one line, not
/// cut (M37: the hook is the text; AGL-043 F19).
pub fn hook_of(text: &str) -> String {
    one_line(text)
}


/// The text of the entry `name` (M32).
pub fn read(dir: &Path, name: &str) -> Result<String, Refusal> {
    let missing = || Refusal::NoEntry {
        name: name.to_owned(),
    };
    if !valid_name(name) {
        return Err(missing());
    }
    fs::read_to_string(entry_path(dir, name)).map_err(|_| missing())
}

/// `edit name old new` (M32): one unique occurrence in the entry's text, as
/// the workspace `edit` does it.
pub fn edit(dir: &Path, name: &str, old: &str, new: &str) -> Result<Write, Refusal> {
    if !valid_name(name) || !entry_path(dir, name).is_file() {
        return Err(Refusal::NoEntry {
            name: name.to_owned(),
        });
    }
    crate::replace::once(&entry_path(dir, name), old, new).map_err(|refusal| Refusal::Replace {
        name: name.to_owned(),
        refusal,
    })?;
    Ok(Write {
        name: name.to_owned(),
        kind: Kind::Edited,
        text: Some(new.to_owned()),
    })
}

/// `update name title hook` (D6, AGL-032): rewrites the entry's first
/// heading and its index line (title + hook) in one call; at least one
/// of `title` and `hook` is given, the other is kept. The name is not
/// touched — the file name is the entry's identity. The entry file is
/// written before the index, as `add` does it: an index failure leaves
/// the line stale, never an orphaned entry.
pub fn update(dir: &Path, name: &str, title: &str, hook: &str) -> Result<Write, Refusal> {
    let _lock = lock_existing(dir)?;
    let named = |refusal: fn(String) -> Refusal| refusal(name.to_owned());
    if !valid_name(name) || !entry_path(dir, name).is_file() {
        return Err(named(|name| Refusal::NoEntry { name }));
    }
    let mut title = clean_title(title);
    let hook = one_line(hook);
    if title.is_empty() && hook.is_empty() {
        return Err(Refusal::NoUpdate);
    }
    let Ok(text) = fs::read_to_string(entry_path(dir, name)) else {
        return Err(named(|name| Refusal::ReadEntry { name }));
    };
    let Some(first) = text.lines().next() else {
        return Err(named(|name| Refusal::EmptyEntry { name }));
    };
    let Some(old_title) = first.strip_prefix("# ") else {
        return Err(named(|name| Refusal::NoHeading { name }));
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
        return Err(named(|name| Refusal::NoIndexLine { name }));
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
        write_atomic(&entry_path(dir, name), &entry).map_err(Refusal::WriteEntry)?;
    }
    let mut new_index = String::with_capacity(index.len() + new_line.len());
    for l in index.lines() {
        new_index.push_str(if l == line { &new_line } else { l });
        new_index.push('\n');
    }
    write_atomic(&dir.join(INDEX), &new_index).map_err(Refusal::WriteIndex)?;
    Ok(Write {
        name: name.to_owned(),
        kind: Kind::Edited,
        text: Some(new_line),
    })
}

/// `remove name` (M32): deletes the file and its index line; either alone is
/// enough, as a hand edit can leave one without the other (AGL-060, D8).
pub fn remove(dir: &Path, name: &str) -> Result<Write, Refusal> {
    let no_entry = || Refusal::NoEntry {
        name: name.to_owned(),
    };
    if !valid_name(name) {
        return Err(no_entry());
    }
    // The entry's own line is the one whose link is its: a hook that quotes
    // another entry's link is not that entry's line (AGL-121, D7).
    let its = |line: &str| index_line(line).is_some_and(|(_, linked, _)| linked == name);
    let _lock = lock_existing(dir)?;
    // An index that cannot be read is not an empty one: writing the lines
    // kept would lose all of them (AGL-060).
    let index = match fs::read_to_string(dir.join(INDEX)) {
        Ok(index) => index,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => return Err(Refusal::ReadIndex(error)),
    };
    let file = entry_path(dir, name).is_file();
    if !file && !index.lines().any(its) {
        return Err(no_entry());
    }
    let kept: String = index
        .lines()
        .filter(|line| !its(line))
        .map(|line| format!("{line}\n"))
        .collect();
    write_atomic(&dir.join(INDEX), &kept).map_err(Refusal::WriteIndex)?;
    if file {
        fs::remove_file(entry_path(dir, name)).map_err(Refusal::RemoveEntry)?;
    }
    Ok(Write {
        name: name.to_owned(),
        kind: Kind::Removed,
        text: None,
    })
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
fn entry_files(dir: &Path) -> Result<BTreeSet<String>, error::Error> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(source) => {
            return Err(CommonError::Read {
                path: dir.to_path_buf(),
                source,
            }
            .into());
        }
    };
    let mut names = BTreeSet::new();
    for entry in entries {
        let path = entry
            .map_err(|source| error::Error::from(CommonError::Read {
                path: dir.to_path_buf(),
                source,
            }))?
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
pub fn list(dir: &Path) -> Result<Vec<Listed>, error::Error> {
    let path = dir.join(INDEX);
    let index = match fs::read_to_string(&path) {
        Ok(index) => index,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(source) => return Err(CommonError::Read { path, source }.into()),
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

/// Whether `name` is an entry: it has a file or a line of the index.
pub fn exists(dir: &Path, name: &str) -> bool {
    let link = format!("]({name}.md)");
    valid_name(name)
        && (entry_path(dir, name).is_file()
            || fs::read_to_string(dir.join(INDEX))
                .is_ok_and(|index| index.lines().any(|line| line.contains(&link))))
}

/// The whole text of an entry, for the human: [`show`] cuts a long one for
/// the model (AGL-060).
pub fn whole(dir: &Path, name: &str) -> Result<String, error::Error> {
    let missing = || {
        error::Error::from(Error::NoEntry {
            name: name.to_owned(),
        })
    };
    if !valid_name(name) {
        return Err(missing());
    }
    let path = entry_path(dir, name);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == ErrorKind::NotFound => Err(missing()),
        Err(source) => Err(CommonError::Read { path, source }.into()),
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

/// What `agl memory` and `/memory` ask for (AGL-060, D2, D5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryAction {
    List,
    Show { name: String },
    /// `yes`: no question (D3).
    Remove { name: String, yes: bool },
}

/// `/memory list` and `/memory show`, of the store `home` finds; a removal
/// is the caller's, which holds the session.
pub fn looked_at(home: &dyn Home, action: &MemoryAction, lang: Lang) -> Result<String, error::Error> {
    let dir = home.look()?;
    match action {
        MemoryAction::Show { name } => show_text(dir.as_deref(), name),
        _ => list_text(dir.as_deref(), lang),
    }
}

/// The lines of `list` (AGL-060, D4): `~` the prompt does not carry it, `?` a
/// file with no line, `!` a line with no file, and what the marks mean.
pub fn list_text(dir: Option<&Path>, lang: Lang) -> Result<String, error::Error> {
    let entries = dir.map(list).transpose()?.unwrap_or_default();
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
pub fn show_text(dir: Option<&Path>, name: &str) -> Result<String, error::Error> {
    whole(entry_dir(dir, name)?, name)
}

/// The memory directory, when `name` is an entry in it, by its file or by its
/// line (AGL-060, D8).
pub fn entry_dir<'a>(dir: Option<&'a Path>, name: &str) -> Result<&'a Path, error::Error> {
    dir.filter(|dir| exists(dir, name))
        .ok_or_else(|| {
            Error::NoEntry {
                name: name.to_owned(),
            }
            .into()
        })
}

/// Removes an entry the way the tool does (AGL-060): what the tool answers,
/// for the model, and the write, for the session.
pub fn remove_entry(dir: Option<&Path>, name: &str) -> Result<(String, Write), error::Error> {
    match remove(entry_dir(dir, name)?, name) {
        Ok(write) => Ok((format!("Removed `{name}`."), write)),
        Err(refusal) => Err(Error::RemoveFailed {
            reason: refusal.text(),
        }
        .into()),
    }
}

/// The removal of the command line (AGL-060, D3): `yes` asks nothing, `ask` is
/// the terminal, and with neither it refuses. What it returns is what to say.
/// A missing entry is told before any question.
pub fn remove_asked(
    dir: Option<&Path>,
    name: &str,
    yes: bool,
    ask: Option<&dyn Fn(&str) -> bool>,
    lang: Lang,
) -> Result<String, error::Error> {
    entry_dir(dir, name)?;
    if !yes {
        let Some(ask) = ask else {
            return Err(Error::RemoveNeedsYes {
                name: name.to_owned(),
            }
            .into());
        };
        if !ask(&tr!(lang, m::MEMORY_REMOVE_ASK, name = name)) {
            return Ok(tr!(lang, m::MEMORY_NOT_REMOVED, name = name));
        }
    }
    remove_entry(dir, name)?;
    Ok(tr!(lang, m::MEMORY_REMOVED, name = name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentlibre_common::testutil::TempDir;

    fn added(dir: &Path, title: &str, hook: &str, text: &str) -> String {
        add(dir, title, hook, text).expect("added").name
    }

    #[test]
    fn a_long_title_makes_a_short_name() {
        let dir = TempDir::new();
        let name = added(&dir.0, &"word ".repeat(40), "h", "t");
        assert!(name.chars().count() <= NAME_LIMIT, "{name}");
        assert!(!name.ends_with('-'));
        // Letters of any script make a name.
        assert_eq!(added(&dir.0, "Збірка і тести", "h", "t"), "збірка-і-тести");
    }

    #[test]
    fn the_index_for_the_prompt_is_whole_when_small_and_the_newest_when_cut() {
        let dir = TempDir::new();
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
    fn list_gives_the_lines_in_order_and_then_the_files_with_no_line() {
        // AGL-060, D4.
        let dir = TempDir::new();
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
        let dir = TempDir::new();
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
        let dir = TempDir::new();
        fs::create_dir_all(&dir.0).unwrap();
        fs::write(dir.0.join(INDEX), b"\xff\n").unwrap();
        assert!(matches!(list(&dir.0), Err(error::Error::Common(CommonError::Read { .. }))));
    }

    #[test]
    fn whole_gives_a_long_entry_uncut() {
        // AGL-060.
        let dir = TempDir::new();
        let long = "é".repeat(20_000);
        let name = added(&dir.0, "Long", "h", &long);
        assert_eq!(whole(&dir.0, &name).unwrap(), format!("# Long\n\n{long}\n"));
        for name in ["nope", "../etc/passwd", ""] {
            assert!(matches!(whole(&dir.0, name), Err(error::Error::Memory(Error::NoEntry { .. }))), "{name}");
        }
    }

    #[test]
    fn a_summary_goes_to_the_summaries_directory_by_date_and_session() {
        let dir = TempDir::new();
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

#[cfg(test)]
mod listing_tests {
    use std::cell::Cell;
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use agentlibre_common::testutil::TempDir;

    /// A memory directory with the entries `one` and `two`.
    fn two_entries(root: &Path) -> PathBuf {
        let dir = root.join("memory");
        add(&dir, "One", "first", "1").unwrap();
        add(&dir, "Two", "second", "2").unwrap();
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
            add(&dir, &format!("Entry {n}"), &"x".repeat(40), "t").unwrap();
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
        assert!(matches!(list_text(Some(&dir), Lang::En), Err(error::Error::Common(CommonError::Read { .. }))));
    }

    #[test]
    fn show_gives_the_whole_of_a_long_entry() {
        let root = TempDir::new();
        let dir = root.0.join("memory");
        let long = "word ".repeat(20_000);
        add(&dir, "Long", "h", &long).unwrap();
        let text = show_text(Some(&dir), "long").unwrap();
        assert!(text.contains(long.trim_end()));
        assert!(!text.contains("omitted"));
        assert!(matches!(show_text(Some(&dir), "nope"), Err(error::Error::Memory(Error::NoEntry { .. }))));
        assert!(matches!(show_text(None, "long"), Err(error::Error::Memory(Error::NoEntry { .. }))));
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
            Err(error::Error::Memory(Error::RemoveNeedsYes { name })) if name == "one"
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
        assert!(matches!(show_text(Some(&dir), "one"), Err(error::Error::Memory(Error::NoEntry { .. }))));
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
                    Err(error::Error::Memory(Error::NoEntry { .. }))
                ),
                "{name}"
            );
        }
        assert!(matches!(
            remove_asked(None, "one", true, None, Lang::En),
            Err(error::Error::Memory(Error::NoEntry { .. }))
        ));
    }
}

#[cfg(test)]
mod plane_tests {
    use super::*;
    use agentlibre_common::testutil::TempDir;

    /// The workspace's memory of a test: a directory, or none.
    struct Dir(Option<PathBuf>);

    impl Home for Dir {
        fn read(&self) -> Option<PathBuf> {
            self.0.clone()
        }

        fn write(&self) -> Result<(PathBuf, Vec<String>), error::Error> {
            Ok((self.0.clone().unwrap(), Vec::new()))
        }
    }

    #[test]
    fn in_a_plane_the_model_sees_both_indexes_and_writes_go_to_the_planes() {
        // AGL-077, D8.
        let dir = TempDir::new();
        let (plane_dir, workspace_dir) = (dir.0.join("plane"), dir.0.join("workspace"));
        let home = PlaneHome {
            name: "work".into(),
            dir: plane_dir.clone(),
            workspace: Box::new(Dir(Some(workspace_dir.clone()))),
        };
        assert_eq!(home.index(), None, "nothing in either");
        fs::create_dir_all(&workspace_dir).unwrap();
        add(&workspace_dir, "Build", "how to build", "cargo build").unwrap();
        let index = home.index().unwrap();
        assert!(index.starts_with("memory of the plane work and of the workspace\n"), "{index}");
        assert!(index.contains("[the plane work; writes go here]\n(no entries)"), "{index}");
        assert!(index.contains("[the workspace; write here with store=workspace]\n- [Build]"), "{index}");
        let (written, _) = home.write().unwrap();
        assert_eq!(written, plane_dir);
        assert!(plane_dir.is_dir());
        assert_eq!(home.write_workspace().unwrap().0, workspace_dir);
        // Outside a plane the heading is the workspace's, as before.
        assert!(Dir(Some(workspace_dir)).index().unwrap().starts_with(MEMORY_MESSAGE));
    }
}
