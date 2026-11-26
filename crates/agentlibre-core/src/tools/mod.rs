//! The built-in tools: `bash` and `edit` (D16, D24-D28, D100-D103).

use std::fs;
use std::io::{self, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::action::Outcome;
use agentlibre_common::clock;
use crate::backend::ToolCall;
use agentlibre_common::i18n::Lang;
use crate::session::{ForgeKind, INTERRUPTED};

pub mod memory;
mod output;
mod reach;
pub mod spec;

use output::Capture;
pub use output::{Output, limited, shown};
pub use reach::{Reach, Written, run};
pub use spec::{SPECS, Spec, outcomes, position, spec};

/// `timeout` when the call does not give one (D26).
const DEFAULT_TIMEOUT: u64 = 60;

/// How long a process group gets between SIGTERM and SIGKILL (D103).
const GRACE: Duration = Duration::from_secs(2);

/// What a tool call returns to the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResult {
    pub outcome: Outcome,
    pub content: String,
}

/// What the `forge` tool needs from the session (AGL-008): the
/// language of its refusals, `forge.auto_rehome` and the data directory
/// of the stores, from the caller and not from the environment (AGL-038,
/// F19).
#[derive(Clone, Copy, Debug)]
pub struct ForgeEnv<'a> {
    pub lang: Lang,
    pub auto_rehome: bool,
    pub data: &'a Path,
    /// The plane's forge, which the tool acts on in a plane; the
    /// workspace's by `store` (AGL-077, D9).
    pub plane: Option<&'a agentlibre_forge::forge::Workspace>,
}

/// A successful write of the `forge` tool: the artifact it touched, as
/// the tool resolved it, and the kind of the write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeWrite {
    pub name: String,
    pub kind: ForgeKind,
    /// The section of an append or a status change.
    pub section: Option<String>,
}

/// Runs `call` of `bash`, `edit` or `forge` in `workspace`. Parsing has
/// checked the arguments against the tool (task-03). A successful `forge`
/// write comes back with the result.
fn run_local(
    workspace: &Path,
    env: ForgeEnv<'_>,
    call: &ToolCall,
    output: &Output<'_>,
) -> (ToolResult, Option<ForgeWrite>) {
    let text = |name: &str| call.text(name).unwrap_or_default();
    match call.name.as_str() {
        "bash" => {
            let timeout = call.int("timeout").map_or(DEFAULT_TIMEOUT, |seconds| seconds as u64);
            (
                bash(workspace, text("command"), Duration::from_secs(timeout), output),
                None,
            )
        }
        "edit" => (edit(workspace, text("path"), text("old"), text("new")), None),
        "forge" => forge(workspace, env, &forge_args(call), output),
        tool => unreachable!("{tool} is not run in the workspace"),
    }
}

/// Runs `bash -c command` in `workspace` and waits for it (D26, D100,
/// D102, D103).
pub fn bash(workspace: &Path, command: &str, timeout: Duration, output: &Output<'_>) -> ToolResult {
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
    // The Nix `bash` of the tools, the human's environment without the
    // wrapper's variables (AGL-040, D3; AGL-039, D15).
    let program = match agentlibre_common::programs::program("bash") {
        Ok(program) => program,
        Err(error) => {
            return ToolResult {
                outcome: Outcome::Fail,
                content: format!("Cannot run bash: {}", error.message(Lang::En)),
            };
        }
    };
    let mut child = {
        let mut command_line = Command::new(program);
        agentlibre_common::programs::clean_env(&mut command_line)
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

    let capture = Arc::new(Mutex::new(Capture::new(output.tokens, output.file.clone())));
    let reading = {
        let capture = Arc::clone(&capture);
        thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(n) = reader.read(&mut buffer)
                && n > 0
            {
                capture.lock().unwrap().push(&buffer[..n]);
            }
        })
    };

    let group = child.id();
    let status = wait_until(&mut child, Instant::now() + timeout);
    // Ctrl-C stops the command like a timeout (D111).
    let interrupted = status.is_none() && crate::stop::requested();
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
        // The human cut the call off: an interruption, not a failure
        // (AGL-063, D6).
        return ToolResult {
            outcome: Outcome::Interrupted,
            content: INTERRUPTED.into(),
        };
    }
    let mut content = capture.lock().unwrap().text(output.count);
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

/// Sends `signal` to the process group `group`; an empty group is fine.
pub fn signal_group(group: u32, signal: libc::c_int) {
    unsafe {
        libc::kill(-(group as libc::pid_t), signal);
    }
}

/// Waits for `child` until `deadline` or Ctrl-C; `None` if it is still
/// running.
fn wait_until(child: &mut Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline && !crate::stop::requested() => {
                thread::sleep(Duration::from_millis(10))
            }
            _ => return None,
        }
    }
}

/// Replaces the only occurrence of `old` in the file at `path` with `new`
/// (D25, D27, D101, D103).
pub fn edit(workspace: &Path, path: &str, old: &str, new: &str) -> ToolResult {
    match agentlibre_forge::replace::once(&workspace.join(path), old, new) {
        Ok(()) => ToolResult {
            outcome: Outcome::Ok,
            content: format!("Replaced 1 occurrence in {path}."),
        },
        Err(refusal) => ToolResult {
            outcome: Outcome::Fail,
            content: refusal.text(path),
        },
    }
}

/// The arguments of the `forge` tool (AGL-008): one tool, the
/// `action` and the per-action fields; which fields an action needs is
/// the tool's check.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForgeArgs {
    /// `workspace`: in a plane, the workspace's forge (AGL-077, D9).
    pub store: Option<String>,
    pub action: String,
    pub name: Option<String>,
    /// For `new`: the kind of the artifact; `task-spec` when absent
    /// (AGL-124, D17).
    pub kind: Option<String>,
    pub title: Option<String>,
    pub old: Option<String>,
    pub new: Option<String>,
    pub section: Option<String>,
    pub state: Option<String>,
    pub text: Option<String>,
}

/// The `forge` tool's arguments from the parsed call (AGL-008): a field
/// that is absent or empty is none, except `new`, which may be empty —
/// an edit then deletes the text of `old`.
fn forge_args(call: &ToolCall) -> ForgeArgs {
    let field = |name: &str| call.text(name).filter(|t| !t.is_empty()).map(str::to_owned);
    ForgeArgs {
        store: field("store"),
        action: call.text("action").unwrap_or_default().to_owned(),
        name: field("name"),
        kind: field("type"),
        title: field("title"),
        old: field("old"),
        new: call.text("new").map(str::to_owned),
        section: field("section"),
        state: field("state"),
        text: field("text"),
    }
}

/// `forge` (AGL-008): the workspace forge, the store of the recorded
/// artifacts of the workspace of `workspace`, over the same engine as
/// the `agl forge` CLI. The store is resolved as the CLI does,
/// with the session's `forge.auto_rehome`, language and data
/// directory; the caller (the agent) cannot be asked, so a needed store
/// choice is the `RehomeNoStore` error.
pub fn forge(
    workspace: &Path,
    env: ForgeEnv<'_>,
    args: &ForgeArgs,
    output: &Output<'_>,
) -> (ToolResult, Option<ForgeWrite>) {
    let lang = env.lang;
    // In a plane, its forge; the workspace's by `store` (AGL-077, D9).
    if let Some(plane) = env.plane
        && args.store.as_deref() != Some("workspace")
    {
        return forge_run(plane, args, lang, output);
    }
    match agentlibre_forge::forge::resolve_store(env.data, workspace, lang, env.auto_rehome) {
        Ok((ws, lines)) => {
            let (mut result, write) = forge_run(&ws, args, lang, output);
            // A store that moved or was adopted on the way: say so, once.
            if !lines.is_empty() {
                let head = lines.join("\n") + "\n";
                result.content.insert_str(0, &head);
            }
            (result, write)
        }
        Err(error) => (forge_fail(error.message(lang)), None),
    }
}

/// The `forge` tool over the resolved workspace `ws` (AGL-008): the
/// result and, for a successful write, the artifact it touched; the
/// entry of the tests.
pub fn forge_run(
    ws: &agentlibre_forge::forge::Workspace,
    args: &ForgeArgs,
    lang: agentlibre_common::i18n::Lang,
    output: &Output<'_>,
) -> (ToolResult, Option<ForgeWrite>) {
    use agentlibre_common::i18n::m;
    use agentlibre_common::tr;
    // A write of `kind` to the artifact the call named: the name matched
    // an artifact's `name` exactly, so it is the artifact's.
    let write = |result: Result<String, Refusal>, kind: ForgeKind, section: Option<String>| {
        match result {
            Ok(content) => {
                let write = args.name.clone().map(|name| ForgeWrite {
                    name,
                    kind,
                    section,
                });
                (done(content), write)
            }
            Err(refusal) => (refusal.told(lang), None),
        }
    };
    match args.action.as_str() {
        "list" => (told(forge_list(ws), lang), None),
        "show" => (told(forge_show(ws, &args.name, lang, output), lang), None),
        "new" => match forge_new(ws, &args.kind, &args.title, lang) {
            // The name is the one the tool made, not an argument.
            Ok((content, name)) => (
                done(content),
                Some(ForgeWrite {
                    name,
                    kind: ForgeKind::Created,
                    section: None,
                }),
            ),
            Err(refusal) => (refusal.told(lang), None),
        },
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
        "status" => write(
            forge_status(ws, &args.name, &args.state, lang),
            ForgeKind::StatusChanged,
            args.state.clone(),
        ),
        "rename" => match forge_rename(ws, &args.name, &args.new, lang) {
            // The event carries the new name; the old one rides the
            // section slot of the marker line.
            Ok((content, name)) => (
                done(content),
                Some(ForgeWrite {
                    name,
                    kind: ForgeKind::Renamed,
                    section: args.name.clone(),
                }),
            ),
            Err(refusal) => (refusal.told(lang), None),
        },
        "remove" => write(forge_remove(ws, &args.name, lang), ForgeKind::Removed, None),
        other => (
            Refusal::from(tr!(lang, m::FORGE_TOOL_UNKNOWN_ACTION, action = other)).told(lang),
            None,
        ),
    }
}

/// The `forge` tool's description for a session (AGL-124, D17): the
/// actions, then each kind found for the session's store — the plane's
/// in a plane, else the workspace's — with its statuses and their edges,
/// its fields and what each is, and the statuses each action is allowed
/// in. It only looks for the store: it never adopts or renames one.
pub fn forge_description(env: &ForgeEnv<'_>, workspace: &Path) -> String {
    use agentlibre_forge::forge::{self, Kinds};
    let kinds = match env.plane {
        Some(plane) => Kinds::of(&plane.dir),
        None => match forge::locate(env.data, workspace) {
            Ok(ws) => Kinds::of(&ws.dir),
            Err(_) => Kinds::shared(),
        },
    };
    let mut text = String::from(spec::FORGE);
    for kind in kinds.all() {
        let statuses: Vec<String> = kind
            .statuses
            .iter()
            .map(|status| match kind.edges.get(status).filter(|to| !to.is_empty()) {
                Some(to) => format!("{status} (to {})", to.join(", ")),
                None => status.clone(),
            })
            .collect();
        let fields: Vec<String> = kind
            .fields
            .iter()
            .map(|(name, field)| format!("{name} ({}): {}", field.kind.name(), field.description))
            .collect();
        let while_ = |action: &str| match kind.allowed(action) {
            [] => "never".to_owned(),
            statuses => statuses.join(", "),
        };
        text.push_str(&format!(
            " Kind {}: statuses {}, starting {}; fields: {}; edit while {}; append while {}; remove while {}",
            kind.name,
            statuses.join(", "),
            kind.initial,
            if fields.is_empty() { "none".to_owned() } else { fields.join("; ") },
            while_("edit"),
            while_("append"),
            while_("remove"),
        ));
        let append_only: Vec<&str> = kind.append_only().collect();
        if !append_only.is_empty() {
            text.push_str(&format!("; append-only: {}", append_only.join(", ")));
        }
        text.push('.');
    }
    text
}

/// The `forge` tool's clean failure with the message `content`.
fn forge_fail(content: String) -> ToolResult {
    ToolResult {
        outcome: Outcome::Fail,
        content,
    }
}

/// The `forge` tool's success with the message `content`.
fn done(content: String) -> ToolResult {
    ToolResult {
        outcome: Outcome::Ok,
        content,
    }
}

/// Why an action of the `forge` tool did not do what it was asked (AGL-067,
/// F7): an error of the forge, told in the session's language, or a refusal
/// whose text is the catalog's or the tool's own.
enum Refusal {
    Error(crate::error::Error),
    Text(String),
}

impl From<crate::error::Error> for Refusal {
    fn from(error: crate::error::Error) -> Refusal {
        Refusal::Error(error)
    }
}

impl From<agentlibre_forge::Error> for Refusal {
    fn from(error: agentlibre_forge::Error) -> Refusal {
        Refusal::Error(error.into())
    }
}

impl From<String> for Refusal {
    fn from(text: String) -> Refusal {
        Refusal::Text(text)
    }
}

impl Refusal {
    /// The failed result the model reads.
    fn told(self, lang: agentlibre_common::i18n::Lang) -> ToolResult {
        match self {
            Refusal::Error(error) => forge_fail(error.message(lang)),
            Refusal::Text(text) => forge_fail(text),
        }
    }
}

/// The result of an action that gives its message or refuses.
fn told(result: Result<String, Refusal>, lang: agentlibre_common::i18n::Lang) -> ToolResult {
    match result {
        Ok(content) => done(content),
        Err(refusal) => refusal.told(lang),
    }
}

/// `forge list` (AGL-008): the store's artifacts, one line each — name,
/// type, status, updated, deps; no git flags (they are the human's).
fn forge_list(ws: &agentlibre_forge::forge::Workspace) -> Result<String, Refusal> {
    use agentlibre_forge::forge::{self, Header, Kinds};
    let files = forge::artifact_files(&ws.dir)?;
    // One evaluation of all the front matters (AGL-041).
    let headers = forge::read_headers(&files, &Kinds::of(&ws.dir))?;
    let mut lines = Vec::new();
    for (path, header) in files.iter().zip(headers) {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        lines.push(match header {
            Header::Artifact(artifact) => format!(
                "{} {} {} {} deps=[{}]",
                artifact.name,
                artifact.type_name,
                artifact.status,
                artifact.updated,
                artifact.strings("deps").join(", ")
            ),
            Header::UnknownSchema { schema } => format!("{stem} !schema {schema}"),
            Header::Broken { detail } => format!("{stem} !parse {detail}"),
        });
    }
    Ok(if lines.is_empty() {
        "(no artifacts)".to_owned()
    } else {
        lines.join("\n")
    })
}

/// `forge show` (AGL-008): the rendered artifact, cut like any tool's output
/// (AGL-049); the whole is the artifact's own file.
fn forge_show(
    ws: &agentlibre_forge::forge::Workspace,
    name: &Option<String>,
    lang: agentlibre_common::i18n::Lang,
    output: &Output<'_>,
) -> Result<String, Refusal> {
    use agentlibre_forge::forge::{self, Kinds};
    let name = name.as_deref().ok_or_else(|| missing_field(lang, "show", "name"))?;
    let artifact = forge::find_artifact(ws, &Kinds::of(&ws.dir), name)?;
    let text = forge::render(&artifact);
    Ok(output::limited(&text, output.tokens, &artifact.path, output.count))
}

/// `forge new` (AGL-008): the scaffold of a new artifact of `kind`,
/// `task-spec` when absent (AGL-124, D17), the same engine path as the
/// CLI's `forge new`; its name is the one the tool made.
fn forge_new(
    ws: &agentlibre_forge::forge::Workspace,
    kind: &Option<String>,
    title: &Option<String>,
    lang: agentlibre_common::i18n::Lang,
) -> Result<(String, String), Refusal> {
    use agentlibre_common::i18n::m;
    use agentlibre_common::tr;
    use agentlibre_forge::forge::Kinds;
    let title = title.as_deref().ok_or_else(|| missing_field(lang, "new", "title"))?;
    let kinds = Kinds::of(&ws.dir);
    let kind = kinds.get(kind.as_deref().unwrap_or("task-spec"))?;
    // The marker line of a store materialized by the write is not tool
    // output.
    let created = agentlibre_forge::forge::new_artifact(ws, &kinds, kind, lang, title, None)?;
    let content = tr!(lang, m::FORGE_NEW_CREATED, path = created.path.display()).to_owned();
    Ok((content, created.name))
}

/// The refusal of `action` on `artifact` in a status its kind does not
/// allow it in (AGL-124, D13).
fn not_allowed(
    lang: agentlibre_common::i18n::Lang,
    artifact: &agentlibre_forge::forge::Artifact,
    kind: &agentlibre_forge::forge::Kind,
    action: &str,
) -> Option<Refusal> {
    use agentlibre_common::i18n::m;
    use agentlibre_common::tr;
    let allowed = kind.allowed(action);
    if allowed.contains(&artifact.status) {
        return None;
    }
    Some(match allowed {
        [] => tr!(lang, m::FORGE_TOOL_NEVER_ALLOWED, action = action, type_name = kind.name.as_str()).into(),
        [one] => tr!(lang, m::FORGE_TOOL_NOT_ALLOWED, name = artifact.name.as_str(), status = artifact.status.as_str(), action = action, statuses = one.as_str()).into(),
        [first @ .., last] => {
            let statuses = format!("{} {} {last}", first.join(", "), tr!(lang, m::OR));
            tr!(lang, m::FORGE_TOOL_NOT_ALLOWED, name = artifact.name.as_str(), status = artifact.status.as_str(), action = action, statuses = statuses).into()
        }
    })
}

/// `forge edit` (AGL-008): replaces one occurrence in the body, the
/// workspace `edit`'s semantics; allowed in the statuses the kind's
/// `actions.edit` lists, for any section but the append-only ones, which
/// grow through `append` (AGL-124, D13); `updated` is bumped and the file
/// is re-rendered from the parsed form.
fn forge_edit(
    ws: &agentlibre_forge::forge::Workspace,
    name: &Option<String>,
    old: &Option<String>,
    new: &Option<String>,
    lang: agentlibre_common::i18n::Lang,
) -> Result<String, Refusal> {
    use agentlibre_forge::forge;
    use agentlibre_common::i18n::m;
    use agentlibre_common::tr;
    let name = name.as_deref().ok_or_else(|| missing_field(lang, "edit", "name"))?;
    // An empty `old` would match everywhere: it is no `old`.
    let old = old
        .as_deref()
        .filter(|old| !old.is_empty())
        .ok_or_else(|| missing_field(lang, "edit", "old"))?;
    let new = new.as_deref().ok_or_else(|| missing_field(lang, "edit", "new"))?;
    let kinds = forge::Kinds::of(&ws.dir);
    let mut artifact = forge::find_artifact(ws, &kinds, name)?;
    let kind = kinds.get(&artifact.type_name)?;
    if let Some(refusal) = not_allowed(lang, &artifact, kind, "edit") {
        return Err(refusal);
    }
    let body = &artifact.body;
    let starts = agentlibre_forge::replace::occurrences(body, old);
    match starts.as_slice() {
        [] => Err(format!("`old` was not found in {name}.").into()),
        [start] => {
            // An append-only section stays so (D13): a unique `old` that
            // touches it is refused, in any status.
            for heading in kind.append_only() {
                if let Some((from, to)) = section_range(body, heading)
                    && *start < to
                    && *start + old.len() > from
                {
                    return Err(tr!(lang, m::FORGE_TOOL_EDIT_APPEND_ONLY, name = name, section = heading).into());
                }
            }
            let mut edited = String::with_capacity(body.len() + new.len());
            edited.push_str(&body[..*start]);
            edited.push_str(new);
            edited.push_str(&body[start + old.len()..]);
            artifact.body = edited;
            artifact.updated = clock::today();
            forge::write_artifact(&artifact)?;
            Ok(format!("Replaced 1 occurrence in {name}."))
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
            Err(format!(
                "`old` appears {} times in {name} (lines {}); add context to make it unique.",
                starts.len(),
                lines.join(", ")
            )
            .into())
        }
    }
}

/// The range of the `## <heading>` section of a body: from the start of
/// the heading line to the next `## ` heading line or the end of the
/// body; `None` when the section is absent.
fn section_range(body: &str, heading: &str) -> Option<(usize, usize)> {
    let marker = format!("## {heading}");
    let mut from = 0;
    while let Some(rel) = body[from..].find(&marker) {
        let at = from + rel;
        let starts_line = at == 0 || body.as_bytes()[at - 1] == b'\n';
        let after = at + marker.len();
        if starts_line && body[after..].starts_with('\n') {
            let rest = &body[after + 1..];
            let end = rest
                .find("\n## ")
                .map(|i| after + 1 + i)
                .unwrap_or(body.len());
            return Some((at, end));
        }
        from = at + 1;
    }
    None
}

/// `forge append` (AGL-008): appends `text` as a bullet at the end of
/// the `## section` of the body (newest last); the section must exist;
/// allowed in the statuses the kind's `actions.append` lists (AGL-124,
/// D13); `updated` is bumped.
fn forge_append(
    ws: &agentlibre_forge::forge::Workspace,
    name: &Option<String>,
    section: &Option<String>,
    text: &Option<String>,
    lang: agentlibre_common::i18n::Lang,
) -> Result<String, Refusal> {
    use agentlibre_forge::forge;
    use agentlibre_common::i18n::m;
    use agentlibre_common::tr;
    let name = name.as_deref().ok_or_else(|| missing_field(lang, "append", "name"))?;
    let section = section
        .as_deref()
        .ok_or_else(|| missing_field(lang, "append", "section"))?;
    let text = text.as_deref().ok_or_else(|| missing_field(lang, "append", "text"))?;
    let kinds = forge::Kinds::of(&ws.dir);
    let mut artifact = forge::find_artifact(ws, &kinds, name)?;
    let kind = kinds.get(&artifact.type_name)?;
    if let Some(refusal) = not_allowed(lang, &artifact, kind, "append") {
        return Err(refusal);
    }
    // A bullet is one line (AGL-031): a multi-line text would fail the
    // append-only check below, so name the rule and the count up front.
    let newlines = text.matches('\n').count();
    if newlines > 0 {
        return Err(tr!(
            lang,
            m::FORGE_TOOL_APPEND_MULTI_LINE,
            name = name,
            n = newlines
        )
        .into());
    }
    let bullet = format!("- {text}\n");
    let old_body = artifact.body.clone();
    let Some(new_body) = append_bullet_lines(&old_body, section, text) else {
        return Err(tr!(
            lang,
            m::FORGE_TOOL_NO_SECTION,
            name = name,
            section = section
        )
        .into());
    };
    // The append-only check: the new body is the old body plus the one
    // bullet line; the re-render touches only that line and `updated`.
    if !one_line_inserted(&old_body, &new_body, &bullet) {
        return Err(tr!(
            lang,
            m::FORGE_TOOL_APPEND_VIOLATION,
            name = name,
            section = section
        )
        .into());
    }
    artifact.body = new_body;
    artifact.updated = clock::today();
    forge::write_artifact(&artifact)?;
    Ok(format!("appended a bullet to ## {section} in {name}"))
}

/// `forge status` (AGL-023): moves the artifact through its kind's
/// lifecycle, the CLI's `forge status` in the tool's shape: the same
/// transition validation, the same `updated` bump. The refusal from a
/// terminal state points to a new artifact with a `supersedes` link (D3,
/// AGL-032).
fn forge_status(
    ws: &agentlibre_forge::forge::Workspace,
    name: &Option<String>,
    state: &Option<String>,
    lang: agentlibre_common::i18n::Lang,
) -> Result<String, Refusal> {
    use agentlibre_common::i18n::m;
    use agentlibre_common::tr;
    let name = name.as_deref().ok_or_else(|| missing_field(lang, "status", "name"))?;
    let state = state.as_deref().ok_or_else(|| missing_field(lang, "status", "state"))?;
    let kinds = agentlibre_forge::forge::Kinds::of(&ws.dir);
    let changed = agentlibre_forge::forge::change_status(ws, &kinds, name, state)?;
    Ok(tr!(
        lang,
        m::FORGE_STATUS_CHANGED,
        name = name,
        from = changed.from.as_str(),
        to = changed.to.as_str()
    )
    .to_owned())
}

/// `forge remove` (AGL-008): deletes the artifact file; allowed in the
/// statuses the kind's `actions.remove` lists (AGL-124, D13).
fn forge_remove(
    ws: &agentlibre_forge::forge::Workspace,
    name: &Option<String>,
    lang: agentlibre_common::i18n::Lang,
) -> Result<String, Refusal> {
    use agentlibre_forge::forge;
    let name = name.as_deref().ok_or_else(|| missing_field(lang, "remove", "name"))?;
    let kinds = forge::Kinds::of(&ws.dir);
    let artifact = forge::find_artifact(ws, &kinds, name)?;
    let kind = kinds.get(&artifact.type_name)?;
    if let Some(refusal) = not_allowed(lang, &artifact, kind, "remove") {
        return Err(refusal);
    }
    fs::remove_file(&artifact.path).map_err(|source| crate::error::Error::from(agentlibre_forge::forge::error::Error::Remove {
        path: artifact.path.clone(),
        source,
    }))?;
    Ok(format!("removed {name}"))
}

/// A name the agent gives with `forge rename` (D6, AGL-032): the slug
/// charset — the letters and digits of any script and `-`, not empty,
/// no leading or trailing `-` — so the file name stays where the store
/// looks for it.
fn rename_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_alphanumeric() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// `forge rename` (D6, AGL-032): renames the artifact's file and its
/// front-matter name and rewrites every reference to it in the store —
/// the `ref` and `refs` fields of each artifact's kind (AGL-124, D8) — in
/// any status: identity maintenance, not a lifecycle change. The new file
/// is written, the references rewritten, and only then the old file
/// removed, so a failure never leaves a dangling reference. The success
/// reports the new name for the event.
fn forge_rename(
    ws: &agentlibre_forge::forge::Workspace,
    name: &Option<String>,
    new: &Option<String>,
    lang: agentlibre_common::i18n::Lang,
) -> Result<(String, String), Refusal> {
    use crate::error::Error;
    use agentlibre_forge::forge;
    use agentlibre_common::i18n::m;
    use agentlibre_common::tr;
    let name = name.as_deref().ok_or_else(|| missing_field(lang, "rename", "name"))?;
    // `new` may be empty (the `edit` deletion) — empty is no name.
    let new = new
        .as_deref()
        .filter(|new| !new.is_empty())
        .ok_or_else(|| missing_field(lang, "rename", "new"))?;
    if !rename_valid_name(new) {
        return Err(tr!(lang, m::FORGE_TOOL_RENAME_INVALID_NAME, new = new).into());
    }
    if new == name {
        return Err(tr!(lang, m::FORGE_TOOL_RENAME_SAME_NAME, name = name, new = new).into());
    }
    let kinds = forge::Kinds::of(&ws.dir);
    let artifact = forge::find_artifact(ws, &kinds, name)?;
    // A type with no description is shown, not written (D18).
    kinds.get(&artifact.type_name)?;
    // The name is the front-matter field; a stray file with the new
    // stem in its type directory would be clobbered by the write.
    let taken = forge::store_names(ws, &kinds)?;
    let new_path = artifact.path.with_file_name(format!("{new}.md"));
    if taken.iter().any(|taken| taken == new) || new_path.exists() {
        return Err(Error::from(agentlibre_forge::forge::error::Error::NameTaken {
            name: new.to_owned(),
            dir: ws.dir.join("forge"),
        })
        .into());
    }
    let mut renamed = artifact.clone();
    renamed.name = new.to_owned();
    renamed.path = new_path;
    renamed.updated = clock::today();
    forge::write_artifact(&renamed)?;
    // Every reference to the old name in the store points to the new
    // one; an artifact is rewritten only when it changed.
    for path in forge::artifact_files(&ws.dir)? {
        if path == artifact.path {
            continue;
        }
        let Ok(mut other) = forge::load_artifact(&path, &kinds) else {
            continue;
        };
        let Ok(kind) = kinds.get(&other.type_name) else {
            continue;
        };
        let fields: Vec<String> = kind.ref_fields().map(|(field, _)| field.to_owned()).collect();
        let mut changed = false;
        for field in fields {
            match other.fields.get_mut(&field) {
                Some(serde_json::Value::String(item)) if item == name => {
                    *item = new.to_owned();
                    changed = true;
                }
                Some(serde_json::Value::Array(items)) => {
                    for item in items.iter_mut() {
                        if item.as_str() == Some(name) {
                            *item = serde_json::Value::String(new.to_owned());
                            changed = true;
                        }
                    }
                }
                _ => {}
            }
        }
        if changed {
            other.updated = clock::today();
            forge::write_artifact(&other)?;
        }
    }
    fs::remove_file(&artifact.path).map_err(|source| crate::error::Error::from(agentlibre_forge::forge::error::Error::Remove {
        path: artifact.path.clone(),
        source,
    }))?;
    Ok((format!("renamed {name} to {new}"), new.to_owned()))
}

/// The forge tool's `missing field` refusal.
fn missing_field(lang: agentlibre_common::i18n::Lang, action: &str, field: &str) -> Refusal {
    use agentlibre_common::i18n::m;
    use agentlibre_common::tr;
    // The fields the action takes: the model that sent a wrong one
    // learns the right ones.
    let fields = match action {
        "new" => "title, type",
        "edit" => "name, old, new",
        "append" => "name, section, text",
        "status" => "name, state",
        "rename" => "name, new",
        _ => "name",
    };
    tr!(
        lang,
        m::FORGE_TOOL_MISSING_FIELD,
        action = action,
        field = field,
        fields = fields
    )
    .into()
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
