//! The built-in tools: `bash` and `edit` (D16, D24-D28, D100-D103).

use std::fs;
use std::io::{self, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::action::{Outcome, Value};
use crate::i18n::Lang;
use crate::interrupt::{self, signal_group};
use crate::session::{ForgeKind, INTERRUPTED};

/// How much of a result the model sees (D28, D103).
const LIMIT: usize = 16 * 1024;

/// `timeout` when the call does not give one (D26).
const DEFAULT_TIMEOUT: u64 = 60;

/// How long a process group gets between SIGTERM and SIGKILL (D103).
const GRACE: Duration = Duration::from_secs(2);

/// What a tool call returns to the model.
#[derive(Debug, PartialEq, Eq)]
pub struct ToolResult {
    pub outcome: Outcome,
    pub content: String,
}

/// What the `forge` tool needs from the session (AGL-008): the
/// language of its refusals and `[forge] auto_rehome`.
#[derive(Clone, Copy, Debug)]
pub struct ForgeEnv {
    pub lang: Lang,
    pub auto_rehome: bool,
}

/// A successful write of the `forge` tool: the artifact it touched, as
/// the tool resolved it, and the kind of the write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeWrite {
    pub name: String,
    pub kind: ForgeKind,
    /// The section of an append.
    pub section: Option<String>,
}

/// Runs the call of `tool` with `args` in `workspace`. Parsing has
/// checked the arguments against the tool (task-03). A successful
/// `forge` write comes back with the result.
pub fn run(
    workspace: &Path,
    env: ForgeEnv,
    tool: &str,
    args: &[(String, Value)],
) -> (ToolResult, Option<ForgeWrite>) {
    let text = |name: &str| {
        args.iter()
            .find_map(|(n, value)| match value {
                Value::Text(text) if n == name => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or_default()
    };
    match tool {
        "bash" => {
            let timeout = args
                .iter()
                .find_map(|(n, value)| match value {
                    Value::Int(seconds) if n == "timeout" => Some(*seconds as u64),
                    _ => None,
                })
                .unwrap_or(DEFAULT_TIMEOUT);
            (bash(workspace, text("command"), Duration::from_secs(timeout)), None)
        }
        "edit" => (edit(workspace, text("path"), text("old"), text("new")), None),
        "forge" => forge(workspace, env, &forge_args(args)),
        _ => unreachable!("unknown tool {tool}"),
    }
}

/// The last [`LIMIT`] bytes of a stream and how many came before them.
#[derive(Default)]
struct Tail {
    bytes: Vec<u8>,
    dropped: usize,
}

impl Tail {
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > LIMIT {
            let excess = self.bytes.len() - LIMIT;
            self.bytes.drain(..excess);
            self.dropped += excess;
        }
    }

    /// The text with the truncation mark (D100). A character cut in half
    /// at the start counts as omitted.
    fn text(&self) -> String {
        let mut start = 0;
        if self.dropped > 0 {
            while start < self.bytes.len() && (self.bytes[start] & 0b1100_0000) == 0b1000_0000 {
                start += 1;
            }
        }
        let body = String::from_utf8_lossy(&self.bytes[start..]);
        match self.dropped + start {
            0 => body.into_owned(),
            omitted => format!("[output truncated: first {omitted} bytes omitted]\n{body}"),
        }
    }
}

/// Runs `bash -c command` in `workspace` and waits for it (D26, D100,
/// D102, D103).
pub fn bash(workspace: &Path, command: &str, timeout: Duration) -> ToolResult {
    let fail = |error: io::Error| ToolResult {
        outcome: Outcome::Fail,
        content: format!("Cannot run bash: {error}"),
    };
    // One pipe for stdout and stderr keeps their order.
    let (mut reader, writer) = match io::pipe() {
        Ok(pipe) => pipe,
        Err(error) => return fail(error),
    };
    let writer_err = match writer.try_clone() {
        Ok(writer) => writer,
        Err(error) => return fail(error),
    };
    let mut child = {
        let mut command_line = Command::new("bash");
        command_line
            .arg("-c")
            .arg(command)
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(writer)
            .stderr(writer_err)
            .process_group(0);
        // Dropping `command_line` closes this process's ends of the pipe.
        match command_line.spawn() {
            Ok(child) => child,
            Err(error) => return fail(error),
        }
    };

    let tail = Arc::new(Mutex::new(Tail::default()));
    let reading = {
        let tail = Arc::clone(&tail);
        thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(n) = reader.read(&mut buffer)
                && n > 0
            {
                tail.lock().unwrap().push(&buffer[..n]);
            }
        })
    };

    let group = child.id();
    let status = wait_until(&mut child, Instant::now() + timeout);
    // Ctrl-C stops the command like a timeout (D111).
    let interrupted = status.is_none() && interrupt::interrupted();
    let timed_out = status.is_none() && !interrupted;
    // End what the command left running, gently first (D102, D103).
    signal_group(group, libc::SIGTERM);
    let status = status.or_else(|| wait_until(&mut child, Instant::now() + GRACE));
    let deadline = Instant::now() + GRACE;
    while !reading.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if !reading.is_finished() || status.is_none() {
        signal_group(group, libc::SIGKILL);
    }
    let status = status.or_else(|| child.wait().ok());
    // A process that left the group may still hold the pipe; do not wait
    // for it.
    let deadline = Instant::now() + GRACE;
    while !reading.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    if interrupted {
        return ToolResult {
            outcome: Outcome::Fail,
            content: INTERRUPTED.into(),
        };
    }
    let mut content = tail.lock().unwrap().text();
    let mut footer = Vec::new();
    let outcome = if timed_out {
        footer.push(format!("timed out after {} s", timeout.as_secs()));
        Outcome::Timeout
    } else {
        match status.map(|status| (status.code(), status.signal())) {
            Some((Some(0), _)) => Outcome::Ok,
            Some((Some(code), _)) => {
                footer.push(format!("exit code: {code}"));
                Outcome::Fail
            }
            Some((None, Some(signal))) => {
                footer.push(format!("killed by signal {signal}"));
                Outcome::Fail
            }
            _ => Outcome::Fail,
        }
    };
    if content.is_empty() && footer.is_empty() {
        content.push_str("(no output)");
    }
    for line in footer {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&line);
    }
    ToolResult { outcome, content }
}

/// Waits for `child` until `deadline` or Ctrl-C; `None` if it is still
/// running.
fn wait_until(child: &mut Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline && !interrupt::interrupted() => {
                thread::sleep(Duration::from_millis(10))
            }
            _ => return None,
        }
    }
}

/// The byte offsets of every occurrence of `old` in `text` (overlapping
/// ones count too: `aa` in `aaa` is ambiguous).
pub(crate) fn occurrences(text: &str, old: &str) -> Vec<usize> {
    text.char_indices()
        .map(|(i, _)| i)
        .chain([text.len()])
        .filter(|&i| text[i..].starts_with(old))
        .collect()
}

/// Replaces the only occurrence of `old` in the file at `path` with `new`
/// (D25, D27, D101, D103).
pub fn edit(workspace: &Path, path: &str, old: &str, new: &str) -> ToolResult {
    let fail = |content: String| ToolResult {
        outcome: Outcome::Fail,
        content,
    };
    let file = workspace.join(path);
    let bytes = match fs::read(&file) {
        Ok(bytes) => bytes,
        Err(error) => return fail(format!("Cannot read {path}: {error}")),
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return fail(format!("{path} is not UTF-8 text."));
    };
    let starts = occurrences(&text, old);
    match starts.as_slice() {
        [] => fail(format!("`old` was not found in {path}.")),
        [start] => {
            let mut edited = String::with_capacity(text.len() + new.len());
            edited.push_str(&text[..*start]);
            edited.push_str(new);
            edited.push_str(&text[start + old.len()..]);
            match fs::write(&file, edited) {
                Ok(()) => ToolResult {
                    outcome: Outcome::Ok,
                    content: format!("Replaced 1 occurrence in {path}."),
                },
                Err(error) => fail(format!("Cannot write {path}: {error}")),
            }
        }
        starts => {
            let mut lines: Vec<String> = starts
                .iter()
                .take(10)
                .map(|&i| (text[..i].matches('\n').count() + 1).to_string())
                .collect();
            if starts.len() > 10 {
                lines.push("…".into());
            }
            fail(format!(
                "`old` appears {} times in {path} (lines {}); add context to make it unique.",
                starts.len(),
                lines.join(", ")
            ))
        }
    }
}

/// The arguments of the `forge` tool (AGL-008): one tool, the
/// `action` and the per-action fields; which fields an action needs is
/// the tool's check.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForgeArgs {
    pub action: String,
    pub name: Option<String>,
    pub title: Option<String>,
    pub old: Option<String>,
    pub new: Option<String>,
    pub section: Option<String>,
    pub text: Option<String>,
}

/// The `forge` tool's arguments from the parsed call (AGL-008): a field
/// that is absent or empty is none, except `new`, which may be empty —
/// an edit then deletes the text of `old`.
fn forge_args(args: &[(String, Value)]) -> ForgeArgs {
    let text = |name: &str| {
        args.iter().find_map(|(n, value)| match value {
            Value::Text(text) if n == name => Some(text.as_str()),
            _ => None,
        })
    };
    let field = |name: &str| text(name).filter(|t| !t.is_empty()).map(str::to_owned);
    ForgeArgs {
        action: text("action").unwrap_or_default().to_owned(),
        name: field("name"),
        title: field("title"),
        old: field("old"),
        new: text("new").map(str::to_owned),
        section: field("section"),
        text: field("text"),
    }
}

/// `forge` (AGL-008): the workspace forge, the store of the recorded
/// artifacts of the workspace of `workspace`, over the same engine as
/// the `agentLIBRE forge` CLI. The store is resolved as the CLI does,
/// with the session's `[forge] auto_rehome` and language; the caller
/// (the agent) cannot be asked, so a needed store choice is the
/// `RehomeNoStore` error.
pub fn forge(workspace: &Path, env: ForgeEnv, args: &ForgeArgs) -> (ToolResult, Option<ForgeWrite>) {
    match crate::paths::Paths::from_env(|name| std::env::var_os(name)) {
        Ok(paths) => forge_in_paths(&paths, workspace, env, args),
        Err(error) => (forge_fail(error.message(env.lang)), None),
    }
}

/// [`forge`] over the data directory of `paths`.
fn forge_in_paths(
    paths: &crate::paths::Paths,
    workspace: &Path,
    env: ForgeEnv,
    args: &ForgeArgs,
) -> (ToolResult, Option<ForgeWrite>) {
    let lang = env.lang;
    let (ws, lines) =
        match crate::forge::resolve_store(&paths.data, workspace, lang, env.auto_rehome) {
            Ok(resolved) => resolved,
            Err(error) => return (forge_fail(error.message(lang)), None),
        };
    let (mut result, write) = forge_run(&ws, args, lang);
    // A store that moved or was adopted on the way: say so, once.
    if !lines.is_empty() {
        let head = lines.join("\n") + "\n";
        result.content.insert_str(0, &head);
    }
    (result, write)
}

/// The `forge` tool over the resolved workspace `ws` (AGL-008): the
/// result and, for a successful write, the artifact it touched; the
/// entry of the tests.
pub fn forge_run(
    ws: &crate::forge::Workspace,
    args: &ForgeArgs,
    lang: crate::i18n::Lang,
) -> (ToolResult, Option<ForgeWrite>) {
    use crate::i18n::m;
    use crate::tr;
    // A write of `kind` to the artifact the call named: the name matched
    // an artifact's `name` exactly, so it is the artifact's.
    let write = |result: ToolResult, kind: ForgeKind, section: Option<String>| {
        let write = (result.outcome == Outcome::Ok)
            .then(|| {
                args.name.clone().map(|name| ForgeWrite {
                    name,
                    kind,
                    section,
                })
            })
            .flatten();
        (result, write)
    };
    match args.action.as_str() {
        "list" => (forge_list(ws, lang), None),
        "show" => (forge_show(ws, &args.name, lang), None),
        "new" => {
            // The name is the one the tool made, not an argument.
            let (result, name) = forge_new(ws, &args.title, lang);
            let write = name.map(|name| ForgeWrite {
                name,
                kind: ForgeKind::Created,
                section: None,
            });
            (result, write)
        }
        "edit" => write(
            forge_edit(ws, &args.name, &args.old, &args.new, lang),
            ForgeKind::Edited,
            None,
        ),
        "append" => write(
            forge_append(ws, &args.name, &args.section, &args.text, lang),
            ForgeKind::Appended,
            args.section.clone(),
        ),
        "remove" => write(forge_remove(ws, &args.name, lang), ForgeKind::Removed, None),
        other => (
            forge_fail(tr!(lang, m::FORGE_TOOL_UNKNOWN_ACTION, action = other)),
            None,
        ),
    }
}

/// The `forge` tool's clean failure with the message `content`.
fn forge_fail(content: String) -> ToolResult {
    ToolResult {
        outcome: Outcome::Fail,
        content,
    }
}

/// `forge list` (AGL-008): the store's artifacts, one line each — name,
/// type, status, updated, deps; no git flags (they are the human's).
fn forge_list(ws: &crate::forge::Workspace, lang: crate::i18n::Lang) -> ToolResult {
    use crate::forge::{self, Header};
    let files = match forge::artifact_files(&ws.dir) {
        Ok(files) => files,
        Err(error) => return forge_fail(error.message(lang)),
    };
    let mut lines = Vec::new();
    for path in files {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match forge::read_header(&path) {
            Ok(Header::Artifact(artifact)) => lines.push(format!(
                "{} {} {} {} deps=[{}]",
                artifact.name,
                artifact.type_name,
                artifact.status,
                artifact.updated,
                artifact.deps.join(", ")
            )),
            Ok(Header::UnknownSchema { schema }) => lines.push(format!("{stem} !schema {schema}")),
            Ok(Header::Broken { detail }) => lines.push(format!("{stem} !parse {detail}")),
            Err(error) => return forge_fail(error.message(lang)),
        }
    }
    let content = if lines.is_empty() {
        "(no artifacts)".to_owned()
    } else {
        lines.join("\n")
    };
    ToolResult {
        outcome: Outcome::Ok,
        content,
    }
}

/// `forge show` (AGL-008): the rendered artifact, through the standard
/// 16 KB tool-result tail (D28, D100).
fn forge_show(
    ws: &crate::forge::Workspace,
    name: &Option<String>,
    lang: crate::i18n::Lang,
) -> ToolResult {
    use crate::forge;
    let Some(name) = name else {
        return missing_field(lang, "show", "name");
    };
    match find_artifact(ws, name) {
        Ok(artifact) => {
            let mut tail = Tail::default();
            tail.push(forge::render(&artifact).as_bytes());
            ToolResult {
                outcome: Outcome::Ok,
                content: tail.text(),
            }
        }
        Err(error) => forge_fail(error.message(lang)),
    }
}

/// `forge new` (AGL-008): the scaffold of a new task-spec, the same
/// engine path as the CLI's `forge new`.
fn forge_new(
    ws: &crate::forge::Workspace,
    title: &Option<String>,
    lang: crate::i18n::Lang,
) -> (ToolResult, Option<String>) {
    use crate::forge::{self, task_spec};
    use crate::i18n::m;
    use crate::tr;
    let Some(title) = title else {
        return (missing_field(lang, "new", "title"), None);
    };
    let names = match store_names(ws) {
        Ok(names) => names,
        Err(error) => return (forge_fail(error.message(lang)), None),
    };
    let name = format!(
        "agl-{:03}-{}",
        task_spec::next_number(&names),
        task_spec::slug(title)
    );
    if names.contains(&name) {
        return (
            forge_fail(
                crate::error::Error::NameTaken {
                    name: name.clone(),
                    dir: ws.dir.join("forge"),
                }
                .message(lang),
            ),
            None,
        );
    }
    // The store is materialized on the first write; the marker line is
    // not tool output.
    if let Err(error) = forge::materialize(ws, lang) {
        return (forge_fail(error.message(lang)), None);
    }
    let path = ws
        .dir
        .join("forge")
        .join(task_spec::TYPE)
        .join(format!("{name}.md"));
    let mut artifact = task_spec::scaffold(&name, &task_spec::today());
    artifact.path = path.clone();
    if let Err(error) = forge::write_artifact(&artifact) {
        return (forge_fail(error.message(lang)), None);
    }
    let result = ToolResult {
        outcome: Outcome::Ok,
        content: tr!(lang, m::FORGE_NEW_CREATED, path = path.display()).to_owned(),
    };
    (result, Some(name))
}

/// `forge edit` (AGL-008): replaces one occurrence in the body, the
/// workspace `edit`'s semantics; allowed while `open` only; `updated`
/// is bumped and the file is re-rendered from the parsed form.
fn forge_edit(
    ws: &crate::forge::Workspace,
    name: &Option<String>,
    old: &Option<String>,
    new: &Option<String>,
    lang: crate::i18n::Lang,
) -> ToolResult {
    use crate::forge::task_spec::Status;
    use crate::forge::{self, task_spec};
    use crate::i18n::m;
    use crate::tr;
    let Some(name) = name else {
        return missing_field(lang, "edit", "name");
    };
    // An empty `old` would match everywhere: it is no `old`.
    let Some(old) = old.as_ref().filter(|old| !old.is_empty()) else {
        return missing_field(lang, "edit", "old");
    };
    let Some(new) = new else {
        return missing_field(lang, "edit", "new");
    };
    let mut artifact = match find_artifact(ws, name) {
        Ok(artifact) => artifact,
        Err(error) => return forge_fail(error.message(lang)),
    };
    if artifact.type_name != task_spec::TYPE {
        return not_task_spec(lang, name, artifact.type_name.clone(), "edit");
    }
    let status =
        Status::parse(&artifact.status).expect("a parsed task-spec carries a valid status");
    if status != Status::Open {
        return forge_fail(tr!(
            lang,
            m::FORGE_TOOL_EDIT_NOT_OPEN,
            name = name,
            status = status.name()
        ));
    }
    let body = &artifact.body;
    let starts = occurrences(body, old);
    let content = match starts.as_slice() {
        [] => {
            let content = format!("`old` was not found in {name}.");
            return forge_fail(content);
        }
        [start] => {
            let mut edited = String::with_capacity(body.len() + new.len());
            edited.push_str(&body[..*start]);
            edited.push_str(new);
            edited.push_str(&body[start + old.len()..]);
            artifact.body = edited;
            artifact.updated = task_spec::today();
            match forge::write_artifact(&artifact) {
                Ok(()) => format!("Replaced 1 occurrence in {name}."),
                Err(error) => return forge_fail(error.message(lang)),
            }
        }
        starts => {
            let mut lines: Vec<String> = starts
                .iter()
                .take(10)
                .map(|&i| (body[..i].matches('\n').count() + 1).to_string())
                .collect();
            if starts.len() > 10 {
                lines.push("…".into());
            }
            let content = format!(
                "`old` appears {} times in {name} (lines {}); add context to make it unique.",
                starts.len(),
                lines.join(", ")
            );
            return forge_fail(content);
        }
    };
    ToolResult {
        outcome: Outcome::Ok,
        content,
    }
}

/// `forge append` (AGL-008): appends `text` as a bullet at the end of
/// the `## section` of the body (newest last); the section must exist;
/// allowed while `in-progress` only; `updated` is bumped.
fn forge_append(
    ws: &crate::forge::Workspace,
    name: &Option<String>,
    section: &Option<String>,
    text: &Option<String>,
    lang: crate::i18n::Lang,
) -> ToolResult {
    use crate::forge::task_spec::Status;
    use crate::forge::{self, task_spec};
    use crate::i18n::m;
    use crate::tr;
    let Some(name) = name else {
        return missing_field(lang, "append", "name");
    };
    let Some(section) = section else {
        return missing_field(lang, "append", "section");
    };
    let Some(text) = text else {
        return missing_field(lang, "append", "text");
    };
    let mut artifact = match find_artifact(ws, name) {
        Ok(artifact) => artifact,
        Err(error) => return forge_fail(error.message(lang)),
    };
    if artifact.type_name != task_spec::TYPE {
        return not_task_spec(lang, name, artifact.type_name.clone(), "append");
    }
    let status =
        Status::parse(&artifact.status).expect("a parsed task-spec carries a valid status");
    if status != Status::InProgress {
        return forge_fail(tr!(
            lang,
            m::FORGE_TOOL_APPEND_NOT_IN_PROGRESS,
            name = name,
            status = status.name()
        ));
    }
    let bullet = format!("- {text}\n");
    let old_body = artifact.body.clone();
    let Some(new_body) = append_bullet_lines(&old_body, section, text) else {
        return forge_fail(tr!(
            lang,
            m::FORGE_TOOL_NO_SECTION,
            name = name,
            section = section
        ));
    };
    // The append-only check: the new body is the old body plus the one
    // bullet line; the re-render touches only that line and `updated`.
    if !one_line_inserted(&old_body, &new_body, &bullet) {
        return forge_fail(tr!(
            lang,
            m::FORGE_TOOL_APPEND_VIOLATION,
            name = name,
            section = section
        ));
    }
    artifact.body = new_body;
    artifact.updated = task_spec::today();
    if let Err(error) = forge::write_artifact(&artifact) {
        return forge_fail(error.message(lang));
    }
    ToolResult {
        outcome: Outcome::Ok,
        content: format!("appended a bullet to ## {section} in {name}"),
    }
}

/// `forge remove` (AGL-008): deletes the artifact file; allowed while
/// `open` only (a draft); `canceled` specs stay — they are records.
fn forge_remove(
    ws: &crate::forge::Workspace,
    name: &Option<String>,
    lang: crate::i18n::Lang,
) -> ToolResult {
    use crate::error::Error;
    use crate::forge::task_spec;
    use crate::forge::task_spec::Status;
    use crate::i18n::m;
    use crate::tr;
    let Some(name) = name else {
        return missing_field(lang, "remove", "name");
    };
    let artifact = match find_artifact(ws, name) {
        Ok(artifact) => artifact,
        Err(error) => return forge_fail(error.message(lang)),
    };
    if artifact.type_name != task_spec::TYPE {
        return not_task_spec(lang, name, artifact.type_name.clone(), "remove");
    }
    let status =
        Status::parse(&artifact.status).expect("a parsed task-spec carries a valid status");
    if status != Status::Open {
        return forge_fail(tr!(
            lang,
            m::FORGE_TOOL_REMOVE_NOT_OPEN,
            name = name,
            status = status.name()
        ));
    }
    if let Err(error) = fs::remove_file(&artifact.path) {
        return forge_fail(
            Error::Remove {
                path: artifact.path.clone(),
                source: error,
            }
            .message(lang),
        );
    }
    ToolResult {
        outcome: Outcome::Ok,
        content: format!("removed {name}"),
    }
}

/// The forge tool's refusal of a write on a non-`task-spec` artifact
/// (AGL-008): the writes apply to the `task-spec` type only.
fn not_task_spec(
    lang: crate::i18n::Lang,
    name: &str,
    type_name: String,
    action: &str,
) -> ToolResult {
    use crate::i18n::m;
    use crate::tr;
    forge_fail(tr!(
        lang,
        m::FORGE_TOOL_NOT_TASK_SPEC,
        name = name,
        type_name = type_name,
        action = action
    ))
}

/// The forge tool's `missing field` refusal.
fn missing_field(lang: crate::i18n::Lang, action: &str, field: &str) -> ToolResult {
    use crate::i18n::m;
    use crate::tr;
    // The fields the action takes: the model that sent a wrong one
    // learns the right ones.
    let fields = match action {
        "new" => "title",
        "edit" => "name, old, new",
        "append" => "name, section, text",
        _ => "name",
    };
    forge_fail(tr!(
        lang,
        m::FORGE_TOOL_MISSING_FIELD,
        action = action,
        field = field,
        fields = fields
    ))
}

/// The artifact of the store of `ws` named `name` (AGL-008): the files
/// are parsed one by one, the match is on the `name` field, the file is
/// `forge/<type>/<name>.md` (the CLI's `find_artifact`).
fn find_artifact(
    ws: &crate::forge::Workspace,
    name: &str,
) -> Result<crate::forge::Artifact, crate::error::Error> {
    use crate::error::Error;
    use crate::forge;
    for path in forge::artifact_files(&ws.dir)? {
        if let Ok(artifact) = forge::load_artifact(&path)
            && artifact.name == name
        {
            return Ok(artifact);
        }
    }
    Err(Error::ArtifactNotFound {
        name: name.to_owned(),
        dir: ws.dir.join("forge"),
    })
}

/// The `name`s of the artifacts of the store of `ws` (AGL-008).
fn store_names(ws: &crate::forge::Workspace) -> Result<Vec<String>, crate::error::Error> {
    use crate::forge::{self, Header};
    let mut names = Vec::new();
    for path in forge::artifact_files(&ws.dir)? {
        if let Header::Artifact(artifact) = forge::read_header(&path)? {
            names.push(artifact.name);
        }
    }
    Ok(names)
}

/// For each of `lines`, whether it is part of a fenced code block (the
/// fence lines included): a `## ` line there is code, not a heading.
fn fenced(lines: &[&str]) -> Vec<bool> {
    let mut inside = false;
    lines
        .iter()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                inside = !inside;
                return true;
            }
            inside
        })
        .collect()
}

/// The body with `- text` as a bullet at the end of the `## section`
/// (AGL-008): `None` when the section is not there. A `## ` line inside
/// a fenced code block is not a heading.
fn append_bullet_lines(body: &str, section: &str, text: &str) -> Option<String> {
    let heading = format!("## {section}");
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    let code = fenced(&lines);
    let start = lines
        .iter()
        .zip(&code)
        .position(|(line, code)| !code && line.trim_end() == heading)?;
    // The section ends at the next `## ` heading or the end of the body.
    let end = (start + 1..lines.len())
        .find(|&i| !code[i] && lines[i].starts_with("## "))
        .unwrap_or(lines.len());
    // The bullet goes after the last non-blank line of the section.
    let mut at = end;
    while at > start + 1 && lines[at - 1].trim().is_empty() {
        at -= 1;
    }
    let mut out: String = lines[..at].concat();
    if let Some(previous) = lines.get(at - 1)
        && !previous.ends_with('\n')
        && !previous.is_empty()
    {
        out.push('\n');
    }
    out.push_str(&format!("- {text}\n"));
    out.push_str(&lines[at..].concat());
    Some(out)
}

/// Whether `new` is `old` with exactly the one line `line` inserted
/// (AGL-008): the append-only check. A last line of `old` without its
/// newline gets one when the bullet follows it; that is not a change.
fn one_line_inserted(old: &str, new: &str, line: &str) -> bool {
    let old: Vec<&str> = old.split_inclusive('\n').collect();
    let new: Vec<&str> = new.split_inclusive('\n').collect();
    if new.len() != old.len() + 1 {
        return false;
    }
    (0..new.len()).any(|p| {
        if new[p] != line || old[p..] != new[p + 1..] {
            return false;
        }
        let same = old[..p] == new[..p];
        // The insertion at the end, after a last line that lacked its
        // newline.
        let terminated = p == old.len()
            && p > 0
            && !old[p - 1].ends_with('\n')
            && old[..p - 1] == new[..p - 1]
            && new[p - 1] == format!("{}\n", old[p - 1]);
        same || terminated
    })
}

#[cfg(test)]
mod tests;
