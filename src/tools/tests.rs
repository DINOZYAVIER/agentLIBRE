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
}
