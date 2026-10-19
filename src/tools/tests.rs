use std::process::Command;

use super::*;

/// A fresh directory, removed when dropped.
struct Dir(std::path::PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let dir =
            std::env::temp_dir().join(format!("agentLIBRE-tools-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Whether process `pid` runs. A killed process may stay a zombie until
/// its new parent reaps it.
fn running(pid: &str) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            let state = stat.rsplit_once(") ").unwrap().1.chars().next();
            state != Some('Z')
        }
        Err(_) => false,
    }
}

fn sh(dir: &Dir, command: &str) -> ToolResult {
    bash(&dir.0, command, Duration::from_secs(10))
}

#[test]
fn bash_output_and_exit_codes() {
    let dir = Dir::new("bash");
    assert_eq!(
        sh(&dir, "echo out; echo err >&2; echo again"),
        ToolResult {
            outcome: Outcome::Ok,
            content: "out\nerr\nagain\n".into()
        }
    );
    assert_eq!(
        sh(&dir, "echo -n partial; exit 3"),
        ToolResult {
            outcome: Outcome::Fail,
            content: "partial\nexit code: 3".into()
        }
    );
    assert_eq!(sh(&dir, "true").content, "(no output)");
    assert_eq!(sh(&dir, "exit 1").content, "exit code: 1");
    assert_eq!(sh(&dir, "kill -9 $$").content, "killed by signal 9");
    assert_eq!(sh(&dir, "pwd").content.trim(), dir.0.to_str().unwrap());
    // stdin is empty, so reading it does not block.
    assert_eq!(sh(&dir, "cat").outcome, Outcome::Ok);
}

#[test]
fn bash_keeps_the_end_of_long_output() {
    let dir = Dir::new("long");
    let result = sh(&dir, "seq 1 100000; echo LAST");
    let content = &result.content;
    assert!(
        content.starts_with("[output truncated: first "),
        "{}",
        &content[..80]
    );
    assert!(content.ends_with("99999\n100000\nLAST\n"));
    let body = content.split_once('\n').unwrap().1;
    assert!(body.len() <= LIMIT);
    // `seq 1 100000` prints 588895 bytes, then "LAST\n".
    let omitted: usize = content["[output truncated: first ".len()..]
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(omitted + body.len(), 588_895 + 5);
}

#[test]
fn truncation_keeps_whole_characters() {
    let mut tail = Tail::default();
    tail.push("я".repeat(LIMIT).as_bytes());
    tail.push(b"x");
    let text = tail.text();
    let body = text.split_once('\n').unwrap().1;
    assert!(body.starts_with('я') && body.ends_with('x'));
    assert!(!text.contains('\u{fffd}'));
}

#[test]
fn bash_timeout() {
    let dir = Dir::new("timeout");
    let started = Instant::now();
    let result = bash(&dir.0, "echo before; sleep 30", Duration::from_secs(1));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        result,
        ToolResult {
            outcome: Outcome::Timeout,
            content: "before\ntimed out after 1 s".into()
        }
    );
    // A command that ignores SIGTERM is killed after the grace time.
    let started = Instant::now();
    let result = bash(&dir.0, "trap '' TERM; sleep 30", Duration::from_secs(1));
    assert_eq!(result.outcome, Outcome::Timeout);
    assert!(started.elapsed() < Duration::from_secs(8));
}

#[test]
fn background_processes_end_with_bash() {
    let dir = Dir::new("background");
    let started = Instant::now();
    let result = sh(&dir, "sleep 30 & echo $! > pid; echo started");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(result.content, "started\n");
    let pid = fs::read_to_string(dir.0.join("pid")).unwrap();
    assert!(!running(pid.trim()), "the background sleep survived");
}

#[test]
fn tmux_sessions_survive() {
    let dir = Dir::new("tmux");
    let socket = dir.0.join("tmux.sock");
    let socket = socket.to_str().unwrap();
    let result = sh(
        &dir,
        &format!("tmux -S {socket} new-session -d -s agl 'sleep 60'; echo ok"),
    );
    assert_eq!(result.content, "ok\n");
    let timed_out = bash(
        &dir.0,
        &format!("tmux -S {socket} new-session -d -s agl2 'sleep 60'; sleep 30"),
        Duration::from_secs(1),
    );
    assert_eq!(timed_out.outcome, Outcome::Timeout);
    let sessions = Command::new("tmux")
        .args(["-S", socket, "list-sessions", "-F", "#S"])
        .output()
        .unwrap();
    let _ = Command::new("tmux")
        .args(["-S", socket, "kill-server"])
        .status();
    assert_eq!(String::from_utf8_lossy(&sessions.stdout), "agl\nagl2\n");
}

#[test]
fn edit_results() {
    let dir = Dir::new("edit");
    let file = dir.0.join("lib.rs");
    fs::write(&file, "fn a() {}\nfn b() {}\nfn b() {}\n").unwrap();
    let edit = |path: &str, old: &str, new: &str| edit(&dir.0, path, old, new);
    assert_eq!(
        edit("lib.rs", "fn a() {}", "pub fn a() {}"),
        ToolResult {
            outcome: Outcome::Ok,
            content: "Replaced 1 occurrence in lib.rs.".into()
        }
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "pub fn a() {}\nfn b() {}\nfn b() {}\n"
    );
    assert_eq!(
        edit("lib.rs", "fn c", "x").content,
        "`old` was not found in lib.rs."
    );
    let twice = edit("lib.rs", "fn b() {}", "x");
    assert_eq!(twice.outcome, Outcome::Fail);
    assert_eq!(
        twice.content,
        "`old` appears 2 times in lib.rs (lines 2, 3); add context to make it unique."
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "pub fn a() {}\nfn b() {}\nfn b() {}\n"
    );
    // Deleting text, and overlapping occurrences.
    assert_eq!(edit("lib.rs", "pub ", "").outcome, Outcome::Ok);
    fs::write(&file, "aaa").unwrap();
    assert!(
        edit("lib.rs", "aa", "b")
            .content
            .starts_with("`old` appears 2 times")
    );
    let missing = edit("nope.rs", "a", "b");
    assert!(
        missing
            .content
            .starts_with("Cannot read nope.rs: No such file or directory")
    );
    assert!(!dir.0.join("nope.rs").exists());
    fs::write(dir.0.join("bin"), [0xff, 0xfe]).unwrap();
    assert_eq!(edit("bin", "a", "b").content, "bin is not UTF-8 text.");
    let absolute = file.to_str().unwrap();
    assert_eq!(edit(absolute, "aaa", "z").outcome, Outcome::Ok);
}

#[test]
fn run_dispatches_calls() {
    let dir = Dir::new("run");
    let result = run(
        &dir.0,
        "bash",
        &[
            ("command".into(), Value::Text("echo hi".into())),
            ("timeout".into(), Value::Int(5)),
        ],
    );
    assert_eq!(result.content, "hi\n");
    fs::write(dir.0.join("f"), "x").unwrap();
    let result = run(
        &dir.0,
        "edit",
        &[
            ("path".into(), Value::Text("f".into())),
            ("old".into(), Value::Text("x".into())),
            ("new".into(), Value::Text("y".into())),
        ],
    );
    assert_eq!(result.outcome, Outcome::Ok);
    // forge: the store of the workspace, resolved from the environment
    // (a fresh store, an empty list; a hint line when other stores exist).
    git_init(&dir.0);
    let result = run(
        &dir.0,
        "forge",
        &[("action".into(), Value::Text("list".into()))],
    );
    assert_eq!(result.outcome, Outcome::Ok);
    assert!(
        result.content.ends_with("(no artifacts)"),
        "{}",
        result.content
    );
}

// The in-turn `forge` tool (AGL-008).

use crate::forge::{self, Artifact, Workspace, task_spec};
use crate::i18n::Lang;

/// A git repository initialized at `dir`.
fn git_init(dir: &Path) {
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
}

/// The forge tool fixture: a git workspace, and its store under a
/// temporary data directory, resolved the way the tool resolves it.
struct ForgeStore {
    _data: Dir,
    _repo: Dir,
    ws: Workspace,
}

impl ForgeStore {
    fn new(name: &str) -> ForgeStore {
        let data = Dir::new(name);
        let repo = Dir::new(&format!("{name}-repo"));
        git_init(&repo.0);
        let (ws, lines) = forge::resolve_store(&data.0, &repo.0, Lang::En, true).unwrap();
        assert!(lines.is_empty(), "a fresh store does not notify: {lines:?}");
        ForgeStore {
            _data: data,
            _repo: repo,
            ws,
        }
    }

    /// The task-spec `name` in `status`, the dates `2026-01-01`, the
    /// scaffold body.
    fn spec(&self, name: &str, status: &str) -> Artifact {
        let mut artifact = task_spec::scaffold(name, "2026-01-01");
        artifact.status = status.to_string();
        artifact.path = self
            .ws
            .dir
            .join("forge")
            .join(task_spec::TYPE)
            .join(format!("{name}.md"));
        artifact
    }

    /// An artifact of the `note` type: a type without a lifecycle.
    fn note(&self, name: &str) -> Artifact {
        let mut table = toml::Table::new();
        table.insert("text".into(), toml::Value::String("a note".into()));
        Artifact {
            path: self
                .ws
                .dir
                .join("forge")
                .join("note")
                .join(format!("{name}.md")),
            type_name: "note".into(),
            name: name.to_string(),
            status: "open".into(),
            created: "2026-01-01".into(),
            updated: "2026-01-01".into(),
            deps: Vec::new(),
            supersedes: Vec::new(),
            cmds: None,
            type_table: toml::Value::Table(table),
            body: "\na note\n".into(),
        }
    }

    fn write(&self, artifact: &Artifact) {
        forge::write_artifact(artifact).unwrap();
    }

    /// A `forge` tool call over the fixture store; `fields` are the
    /// per-action fields.
    fn call(&self, action: &str, fields: &[(&str, &str)]) -> ToolResult {
        let field = |name: &str| {
            fields
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        };
        let args = ForgeArgs {
            action: action.to_string(),
            name: field("name"),
            title: field("title"),
            old: field("old"),
            new: field("new"),
            section: field("section"),
            text: field("text"),
        };
        forge_in(&self.ws, &args, Lang::En)
    }
}

#[test]
fn forge_new_is_the_engine_scaffold() {
    let f = ForgeStore::new("forge-new");
    let path =
        f.ws.dir
            .join("forge")
            .join(task_spec::TYPE)
            .join("agl-001-my-first-task.md");
    let result = f.call("new", &[("title", "My First Task")]);
    assert_eq!(
        result,
        ToolResult {
            outcome: Outcome::Ok,
            content: format!("created {}", path.display())
        }
    );
    let artifact = forge::load_artifact(&path).unwrap();
    assert_eq!(artifact.status, "open");
    assert_eq!(artifact.created, task_spec::today());
    assert!(artifact.body.starts_with("\n## Objective\n"));
    // The first write materialized the store and its marker.
    assert!(f.ws.dir.join(forge::MARKER_FILE).is_file());
    // The number is one past the max over the existing names; the same
    // title again advances the number (a slug is not a unique name).
    f.write(&f.spec("agl-003-mid", "open"));
    let result = f.call("new", &[("title", "Second Task")]);
    assert!(
        result.content.ends_with("agl-004-second-task.md"),
        "{}",
        result.content
    );
    let result = f.call("new", &[("title", "Second Task")]);
    assert!(
        result.content.ends_with("agl-005-second-task.md"),
        "{}",
        result.content
    );
    // The title is required.
    assert_eq!(
        f.call("new", &[]),
        ToolResult {
            outcome: Outcome::Fail,
            content: "forge new: title is missing".into()
        }
    );
}

#[test]
fn forge_list_is_one_line_per_artifact() {
    let f = ForgeStore::new("forge-list");
    assert_eq!(f.call("list", &[]).content, "(no artifacts)");
    f.write(&f.spec("agl-002-b", "in-progress"));
    f.write(&f.spec("agl-001-a", "open"));
    assert_eq!(
        f.call("list", &[]).content,
        "agl-001-a task-spec open 2026-01-01 deps=[]\n\
         agl-002-b task-spec in-progress 2026-01-01 deps=[]"
    );
    // An action outside the six is refused.
    assert_eq!(
        f.call("fly", &[]),
        ToolResult {
            outcome: Outcome::Fail,
            content: "unknown action fly; the actions are \
                      list, show, new, edit, append, remove"
                .into()
        }
    );
}

#[test]
fn forge_show_is_the_rendered_artifact() {
    let f = ForgeStore::new("forge-show");
    let artifact = f.spec("agl-001-x", "open");
    f.write(&artifact);
    let result = f.call("show", &[("name", "agl-001-x")]);
    assert_eq!(result.outcome, Outcome::Ok);
    assert_eq!(result.content, forge::render(&artifact));
    // The name is required.
    assert_eq!(f.call("show", &[]).content, "forge show: name is missing");
    // A name without an artifact.
    assert_eq!(
        f.call("show", &[("name", "nope")]).content,
        format!(
            "no artifact is named nope in {}",
            f.ws.dir.join("forge").display()
        )
    );
    // A long body goes through the standard 16 KB tail.
    let mut big = f.spec("agl-002-big", "open");
    big.body = "x".repeat(40_000);
    f.write(&big);
    let content = f.call("show", &[("name", "agl-002-big")]).content;
    assert!(
        content.starts_with("[output truncated: first "),
        "{}",
        &content[..40]
    );
    assert!(content.ends_with("xxx"));
    let body = content.split_once('\n').unwrap().1;
    assert!(body.len() <= LIMIT);
}

#[test]
fn the_status_keyed_rule() {
    let statuses = ["open", "in-progress", "backlog", "done", "canceled"];
    // edit is allowed while open only.
    let f = ForgeStore::new("rule-edit");
    for (i, status) in statuses.iter().enumerate() {
        let name = format!("agl-{i:03}-x");
        f.write(&f.spec(&name, status));
        let result = f.call(
            "edit",
            &[("name", &name), ("old", "## Objective"), ("new", "## Goal")],
        );
        if *status == "open" {
            assert_eq!(
                result,
                ToolResult {
                    outcome: Outcome::Ok,
                    content: format!("Replaced 1 occurrence in {name}.")
                }
            );
        } else {
            assert_eq!(
                result,
                ToolResult {
                    outcome: Outcome::Fail,
                    content: format!("{name} is {status}; edit is allowed while open (the draft)")
                }
            );
        }
    }
    // append is allowed while in-progress only.
    let f = ForgeStore::new("rule-append");
    for (i, status) in statuses.iter().enumerate() {
        let name = format!("agl-{i:03}-x");
        f.write(&f.spec(&name, status));
        let result = f.call(
            "append",
            &[
                ("name", &name),
                ("section", "Progress"),
                ("text", "a bullet"),
            ],
        );
        if *status == "in-progress" {
            assert_eq!(
                result,
                ToolResult {
                    outcome: Outcome::Ok,
                    content: format!("appended a bullet to ## Progress in {name}")
                }
            );
        } else {
            assert_eq!(
                result,
                ToolResult {
                    outcome: Outcome::Fail,
                    content: format!("{name} is {status}; append is allowed while in-progress")
                }
            );
        }
    }
    // remove is allowed while open only; a refused file stays.
    let f = ForgeStore::new("rule-remove");
    for (i, status) in statuses.iter().enumerate() {
        let name = format!("agl-{i:03}-x");
        let path =
            f.ws.dir
                .join("forge")
                .join(task_spec::TYPE)
                .join(format!("{name}.md"));
        f.write(&f.spec(&name, status));
        let result = f.call("remove", &[("name", &name)]);
        if *status == "open" {
            assert_eq!(
                result,
                ToolResult {
                    outcome: Outcome::Ok,
                    content: format!("removed {name}")
                }
            );
            assert!(!path.exists());
        } else {
            assert_eq!(
                result,
                ToolResult {
                    outcome: Outcome::Fail,
                    content: format!("{name} is {status}; remove is allowed while open (a draft)")
                }
            );
            assert!(path.exists());
        }
    }
}

#[test]
fn forge_edit_is_the_workspace_edit_on_the_body() {
    let f = ForgeStore::new("forge-edit");
    let artifact = f.spec("agl-001-d", "open");
    f.write(&artifact);
    // A unique `old`: the body is edited, `updated` is bumped.
    let result = f.call(
        "edit",
        &[
            ("name", "agl-001-d"),
            ("old", "## Objective"),
            ("new", "## Goal"),
        ],
    );
    assert_eq!(
        result,
        ToolResult {
            outcome: Outcome::Ok,
            content: "Replaced 1 occurrence in agl-001-d.".into()
        }
    );
    let edited = forge::load_artifact(&artifact.path).unwrap();
    assert!(edited.body.contains("## Goal"));
    assert!(!edited.body.contains("## Objective"));
    assert_eq!(edited.updated, task_spec::today());
    assert_eq!(edited.created, "2026-01-01");
    // A missing `old` and an ambiguous one are clean failures.
    assert_eq!(
        f.call(
            "edit",
            &[("name", "agl-001-d"), ("old", "nope"), ("new", "x")]
        )
        .content,
        "`old` was not found in agl-001-d."
    );
    assert_eq!(
        f.call(
            "edit",
            &[("name", "agl-001-d"), ("old", "## "), ("new", "x")]
        )
        .content,
        "`old` appears 6 times in agl-001-d (lines 2, 5, 8, 11, 14, 17); \
         add context to make it unique."
    );
    // The refusals did not touch the file.
    assert_eq!(
        forge::load_artifact(&artifact.path).unwrap().body,
        edited.body
    );
    // The per-action fields are checked.
    assert_eq!(
        f.call("edit", &[("old", "a"), ("new", "b")]).content,
        "forge edit: name is missing"
    );
    assert_eq!(
        f.call("edit", &[("name", "agl-001-d"), ("new", "b")])
            .content,
        "forge edit: old is missing"
    );
    assert_eq!(
        f.call("edit", &[("name", "agl-001-d"), ("old", "a")])
            .content,
        "forge edit: new is missing"
    );
}

#[test]
fn forge_append_is_a_bullet_under_the_section() {
    let f = ForgeStore::new("forge-append");
    let artifact = f.spec("agl-001-p", "in-progress");
    f.write(&artifact);
    // A mid-body section: the bullet goes after the section's last
    // non-blank line, before the next heading.
    let result = f.call(
        "append",
        &[
            ("name", "agl-001-p"),
            ("section", "Progress"),
            ("text", "a bullet"),
        ],
    );
    assert_eq!(
        result,
        ToolResult {
            outcome: Outcome::Ok,
            content: "appended a bullet to ## Progress in agl-001-p".into()
        }
    );
    let appended = forge::load_artifact(&artifact.path).unwrap();
    assert!(
        appended.body.contains(
            "## Progress\n\
             <!-- Append-only, newest last: dated bullets with commit refs. -->\n\
             - a bullet\n\n## Verification"
        ),
        "{}",
        appended.body
    );
    assert_eq!(appended.updated, task_spec::today());
    // The last section, at the end of the body.
    let result = f.call(
        "append",
        &[
            ("name", "agl-001-p"),
            ("section", "Verification"),
            ("text", "a check"),
        ],
    );
    assert_eq!(result.outcome, Outcome::Ok);
    assert!(
        forge::load_artifact(&artifact.path)
            .unwrap()
            .body
            .ends_with("- a check\n")
    );
    // A section that is not in the body is refused, nothing is written.
    let before = forge::load_artifact(&artifact.path).unwrap();
    let result = f.call(
        "append",
        &[("name", "agl-001-p"), ("section", "Nope"), ("text", "x")],
    );
    assert_eq!(
        result,
        ToolResult {
            outcome: Outcome::Fail,
            content: "the body of agl-001-p has no `## Nope` heading".into()
        }
    );
    assert_eq!(
        forge::load_artifact(&artifact.path).unwrap().body,
        before.body
    );
    // The section is an open name: any `## ` heading in the body.
    let mut custom = f.spec("agl-002-c", "in-progress");
    custom.body = "## Progress\n- one\n## Verification\n- two\n".into();
    f.write(&custom);
    let result = f.call(
        "append",
        &[
            ("name", "agl-002-c"),
            ("section", "Progress"),
            ("text", "added"),
        ],
    );
    assert_eq!(result.outcome, Outcome::Ok);
    assert_eq!(
        forge::load_artifact(&custom.path).unwrap().body,
        "## Progress\n- one\n- added\n## Verification\n- two\n"
    );
    // The per-action fields are checked.
    assert_eq!(
        f.call("append", &[("section", "Progress"), ("text", "x")])
            .content,
        "forge append: name is missing"
    );
    assert_eq!(
        f.call("append", &[("name", "agl-001-p"), ("text", "x")])
            .content,
        "forge append: section is missing"
    );
    assert_eq!(
        f.call("append", &[("name", "agl-001-p"), ("section", "Progress")])
            .content,
        "forge append: text is missing"
    );
}

#[test]
fn forge_writes_are_task_spec_only_and_reads_are_generic() {
    let f = ForgeStore::new("forge-types");
    let note = f.note("note-1");
    f.write(&note);
    // The reads work on any type.
    assert_eq!(
        f.call("list", &[]).content,
        "note-1 note open 2026-01-01 deps=[]"
    );
    assert_eq!(
        f.call("show", &[("name", "note-1")]).content,
        forge::render(&note)
    );
    // The writes are task-spec only: the refusal names the action.
    assert_eq!(
        f.call(
            "edit",
            &[("name", "note-1"), ("old", "a note"), ("new", "b note")]
        )
        .content,
        "note-1 is a note, not a task-spec; edit applies to task-specs only"
    );
    assert_eq!(
        f.call(
            "append",
            &[("name", "note-1"), ("section", "Progress"), ("text", "x")]
        )
        .content,
        "note-1 is a note, not a task-spec; append applies to task-specs only"
    );
    assert_eq!(
        f.call("remove", &[("name", "note-1")]).content,
        "note-1 is a note, not a task-spec; remove applies to task-specs only"
    );
    // The refusals did not touch the file.
    assert_eq!(forge::load_artifact(&note.path).unwrap().body, "\na note\n");
    // A missing name, on a write that has one.
    assert_eq!(
        f.call("remove", &[]).content,
        "forge remove: name is missing"
    );
}

#[test]
fn the_store_is_the_workspace_git_repository() {
    let f = ForgeStore::new("forge-git");
    // The human's flow: the store directory is a git repository. A fresh
    // store is materialized on its first write; the human inits it first.
    fs::create_dir_all(&f.ws.dir).unwrap();
    git_init(&f.ws.dir);
    assert_eq!(
        forge::git(&f.ws.dir, &["rev-parse", "--show-toplevel"]).unwrap(),
        f.ws.dir.to_str().unwrap()
    );
    f.write(&f.spec("agl-001-g", "open"));
    forge::git(&f.ws.dir, &["add", "-A"]).unwrap();
    forge::git(&f.ws.dir, &["commit", "-q", "-m", "record"]).unwrap();
    // The tool's write lands in the working tree; the human commits.
    let result = f.call(
        "edit",
        &[
            ("name", "agl-001-g"),
            ("old", "## Objective"),
            ("new", "## Goal"),
        ],
    );
    assert_eq!(result.outcome, Outcome::Ok);
    let diff = forge::git(&f.ws.dir, &["diff", "--", "forge/task-spec/agl-001-g.md"]).unwrap();
    assert!(diff.contains("## Goal"), "{diff}");
    assert!(diff.contains("## Objective"), "{diff}");
    forge::git(&f.ws.dir, &["add", "-A"]).unwrap();
    forge::git(&f.ws.dir, &["commit", "-q", "-m", "edit the draft"]).unwrap();
    let log = forge::git(&f.ws.dir, &["log", "--oneline"]).unwrap();
    assert!(log.contains("edit the draft"), "{log}");
    assert!(log.contains("record"), "{log}");
    assert_eq!(
        forge::git(&f.ws.dir, &["diff", "--", "forge/task-spec/agl-001-g.md"]).unwrap(),
        ""
    );
}
