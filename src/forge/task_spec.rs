//! The `task-spec` type: the type table, the human-gated lifecycle,
//! the git range and its computed states.

use std::path::{Path, PathBuf};

use toml::Value;

use crate::error::{Error, Expected};
use crate::forge::{self, Artifact};

/// The name of the type.
pub const TYPE: &str = "task-spec";

/// The states of the lifecycle (AGL-008):
/// `open → in-progress → done | canceled`, with `backlog` the parked
/// state the work may come back from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Open,
    InProgress,
    Done,
    Backlog,
    Canceled,
}

impl Status {
    /// Parses a `status` value.
    pub fn parse(value: &str) -> Option<Status> {
        Some(match value {
            "open" => Status::Open,
            "in-progress" => Status::InProgress,
            "done" => Status::Done,
            "backlog" => Status::Backlog,
            "canceled" => Status::Canceled,
            _ => return None,
        })
    }

    /// The value as stored in the front-matter.
    pub fn name(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::InProgress => "in-progress",
            Status::Done => "done",
            Status::Backlog => "backlog",
            Status::Canceled => "canceled",
        }
    }

    /// The legal transitions (AGL-008): `open→in-progress`,
    /// `open→backlog`, `open→canceled`, `in-progress→done`,
    /// `in-progress→backlog`, `in-progress→canceled`,
    /// `backlog→in-progress`; `done` and `canceled` are terminal.
    pub fn allows(self, to: Status) -> bool {
        matches!(
            (self, to),
            (Status::Open, Status::InProgress)
                | (Status::Open, Status::Backlog)
                | (Status::Open, Status::Canceled)
                | (Status::InProgress, Status::Done)
                | (Status::InProgress, Status::Backlog)
                | (Status::InProgress, Status::Canceled)
                | (Status::Backlog, Status::InProgress)
        )
    }
}

/// The `[task-spec]` table: the git range of the work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    /// A commit id (anything `git rev-parse` resolves), empty until set.
    pub anchor: String,
    /// A commit id, empty until set. `anchor..last` is the review
    /// range.
    pub last: String,
}

/// Validates the `[task-spec]` table of `artifact` strictly and takes
/// it.
pub fn take(artifact: &Artifact) -> Result<TaskSpec, Error> {
    let Value::Table(table) = &artifact.type_table else {
        return Err(Error::WrongType {
            path: artifact.path.clone(),
            key: artifact.type_name.clone(),
            expected: Expected::Table,
        });
    };
    let mut section = crate::tomlfile::Section::at(
        &artifact.path,
        &artifact.type_name,
        table.clone(),
    );
    section.check_keys(&["anchor", "last"])?;
    let anchor = section.required_string("anchor")?;
    let last = section.required_string("last")?;
    Ok(TaskSpec { anchor, last })
}

/// Validates the task-spec parts of `artifact`: the `status` is a
/// lifecycle state and the `[task-spec]` table is strict.
pub fn check(artifact: &Artifact) -> Result<(), Error> {
    if Status::parse(&artifact.status).is_none() {
        return Err(Error::BadState {
            state: artifact.status.clone(),
        });
    }
    take(artifact)?;
    Ok(())
}

/// The scaffold of a new task-spec: `open`, both dates `today`, an
/// empty range and verification, and the body template (AGL-008):
/// Objective, Decisions, Open questions, Changes, Progress,
/// Verification, a one-line HTML-comment hint under each heading.
/// `path` is set by the caller.
pub fn scaffold(name: &str, today: &str) -> Artifact {
    let mut table = toml::Table::new();
    table.insert("anchor".into(), Value::String(String::new()));
    table.insert("last".into(), Value::String(String::new()));
    Artifact {
        path: PathBuf::new(),
        type_name: TYPE.into(),
        name: name.to_string(),
        status: Status::Open.name().into(),
        created: today.to_string(),
        updated: today.to_string(),
        deps: Vec::new(),
        supersedes: Vec::new(),
        cmds: Some(Vec::new()),
        type_table: Value::Table(table),
        body: "\n\
## Objective
<!-- What this task changes, and its scope. -->

## Decisions
<!-- Settled choices: the decision, its date, and who set it. -->

## Open questions
<!-- Questions whose answers change code, stored data or behavior; none when there are none. -->

## Changes
<!-- The concrete changes: files, types and behavior. -->

## Progress
<!-- Append-only, newest last: dated bullets with commit refs. -->

## Verification
<!-- What proves the task is done. -->
"
        .into(),
    }
}

/// A computed state of a task-spec (never stored).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitFlag {
    /// The range is valid and, when it applies, `last` is the HEAD.
    Ok,
    /// `last` is not the repository HEAD: work was committed without
    /// updating the header (only `open`/`in-progress` with `last` set).
    Lags,
    /// `anchor` is not an ancestor of `last` (any status, both ids
    /// set).
    Broken,
}

/// The full commit id that `id` resolves to in the repository at
/// `repo`; an error when it does not resolve.
fn resolve_commit(repo: &Path, id: &str) -> Result<String, Error> {
    let arg = format!("{id}^{{commit}}");
    forge::git(repo, &["rev-parse", "--verify", &arg]).map_err(|_| Error::CommitNotFound {
        id: id.to_string(),
        repo: repo.into(),
    })
}

/// Whether `ancestor` is an ancestor of `descendant` in the
/// repository at `repo`; both ids must resolve.
fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool, Error> {
    let output = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(repo)
        .output()
        .map_err(|source| Error::GitSpawn { source })?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(Error::GitFailed {
            args: "merge-base --is-ancestor".into(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        }),
    }
}

/// The computed git state of `spec` in the repository at `repo`:
/// `None` when `last` is empty (no range to check). `done`, `backlog`
/// and `canceled` specs keep `last` as history: the range validity
/// only, never flagged.
pub fn git_flag(
    spec: &TaskSpec,
    status: Status,
    repo: &Path,
) -> Result<Option<GitFlag>, Error> {
    if spec.last.is_empty() {
        return Ok(None);
    }
    let last = resolve_commit(repo, &spec.last)?;
    if !spec.anchor.is_empty() {
        let anchor = resolve_commit(repo, &spec.anchor)?;
        if !is_ancestor(repo, &anchor, &last)? {
            return Ok(Some(GitFlag::Broken));
        }
    }
    let head = forge::git(repo, &["rev-parse", "HEAD"])?;
    let lags = matches!(status, Status::Open | Status::InProgress) && last != head;
    Ok(Some(if lags { GitFlag::Lags } else { GitFlag::Ok }))
}

/// The slug of a title: lowercase alphanumerics, a run of anything
/// else as one `-`, trimmed.
pub fn slug(title: &str) -> String {
    let mapped: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    mapped
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// The next `agl-<NNN>` number: one past the max over the existing
/// task-spec names.
pub fn next_number(names: &[String]) -> u32 {
    let mut max = 0u32;
    for name in names {
        let Some(rest) = name.strip_prefix("agl-") else {
            continue;
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(n) = digits.parse::<u32>() {
            max = max.max(n);
        }
    }
    max + 1
}

/// Today's calendar date, `YYYY-MM-DD`, from the system clock.
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let z = (secs / 86400) as i64;
    // Civil from days (Howard Hinnant's algorithm).
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!(
        "{:04}-{:02}-{:02}",
        if m <= 2 { y + 1 } else { y },
        m,
        d
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::testutil::TempDir;
    use std::fs;

    /// A git repository at `root` with `count` commits.
    fn git_repo(root: &Path, count: u32) -> PathBuf {
        let dir = root.join("proj");
        fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
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
        for i in 0..count {
            fs::write(dir.join("f.txt"), format!("{i}\n")).unwrap();
            run(&["add", "f.txt"]);
            run(&["commit", "-q", "-m", &format!("commit {i}")]);
        }
        dir
    }

    /// The commit `back` before the HEAD (`0` is the HEAD).
    fn back(repo: &Path, back: u32) -> String {
        crate::forge::git(repo, &["rev-parse", &format!("HEAD~{back}")]).unwrap()
    }

    #[test]
    fn the_transition_table() {
        let all =
            [Status::Open, Status::InProgress, Status::Done, Status::Backlog, Status::Canceled];
        let legal = [
            (Status::Open, Status::InProgress),
            (Status::Open, Status::Backlog),
            (Status::Open, Status::Canceled),
            (Status::InProgress, Status::Done),
            (Status::InProgress, Status::Backlog),
            (Status::InProgress, Status::Canceled),
            (Status::Backlog, Status::InProgress),
        ];
        for from in all {
            for to in all {
                assert_eq!(
                    from.allows(to),
                    legal.contains(&(from, to)),
                    "{from:?} -> {to:?}"
                );
            }
        }
    }

    #[test]
    fn the_states_parse() {
        for state in [
            Status::Open,
            Status::InProgress,
            Status::Done,
            Status::Backlog,
            Status::Canceled,
        ] {
            assert_eq!(Status::parse(state.name()), Some(state));
        }
        assert_eq!(Status::parse("ready"), None);
        assert_eq!(Status::parse("draft"), None);
        assert_eq!(Status::parse("OPEN"), None);
    }

    #[test]
    fn the_slug() {
        assert_eq!(
            slug("Forge core: the task-spec store"),
            "forge-core-the-task-spec-store"
        );
        assert_eq!(slug("AGL 007"), "agl-007");
        assert_eq!(slug("--weird__name--"), "weird-name");
        assert_eq!(slug(""), "");
    }

    #[test]
    fn the_next_number_is_one_past_the_max() {
        assert_eq!(next_number(&[]), 1);
        assert_eq!(
            next_number(&["agl-003-x".into(), "agl-001-y".into()]),
            4
        );
        // Non agl- names and broken numbers do not count.
        let names = vec!["note-1".to_string(), "agl-x".to_string(), "agl-005-z".to_string()];
        assert_eq!(next_number(&names), 6);
    }

    #[test]
    fn the_scaffold_is_open_with_an_empty_range() {
        let artifact = scaffold("agl-008-x", "2026-10-17");
        assert_eq!(artifact.type_name, TYPE);
        assert_eq!(artifact.name, "agl-008-x");
        assert_eq!(artifact.status, "open");
        assert_eq!(artifact.created, "2026-10-17");
        assert_eq!(artifact.updated, "2026-10-17");
        assert!(artifact.deps.is_empty());
        assert!(artifact.supersedes.is_empty());
        assert_eq!(artifact.cmds, Some(Vec::new()));
        assert_eq!(
            take(&artifact).unwrap(),
            TaskSpec { anchor: String::new(), last: String::new() }
        );
        // The body template: the exact headings and hints (AGL-008).
        assert_eq!(
            artifact.body,
            "\n## Objective\n<!-- What this task changes, and its scope. -->\n\n## Decisions\n\
             <!-- Settled choices: the decision, its date, and who set it. -->\n\n## Open \
             questions\n<!-- Questions whose answers change code, stored data or behavior; none \
             when there are none. -->\n\n## Changes\n<!-- The concrete changes: files, types and \
             behavior. -->\n\n## Progress\n<!-- Append-only, newest last: dated bullets with \
             commit refs. -->\n\n## Verification\n<!-- What proves the task is done. -->\n"
        );
        // The scaffold parses as a file.
        let path = Path::new("/s/agl-008-x.md");
        let parsed =
            crate::forge::parse_artifact(path, &crate::forge::render(&artifact)).unwrap();
        assert_eq!(parsed.name, "agl-008-x");
        assert_eq!(parsed.status, "open");
    }

    #[test]
    fn take_is_strict() {
        // An unknown key.
        let mut artifact = scaffold("agl-001-x", "2026-10-17");
        let Value::Table(mut table) = artifact.type_table.clone() else {
            panic!("a table")
        };
        table.insert("branch".into(), Value::String("x".into()));
        artifact.type_table = Value::Table(table);
        assert!(matches!(
            take(&artifact),
            Err(Error::UnknownKey { key, .. }) if key == "task-spec.branch"
        ));
        // anchor is required.
        let mut artifact = scaffold("agl-001-x", "2026-10-17");
        let Value::Table(mut table) = artifact.type_table.clone() else {
            panic!("a table")
        };
        table.remove("anchor");
        artifact.type_table = Value::Table(table);
        assert!(matches!(
            take(&artifact),
            Err(Error::MissingKey { key, .. }) if key == "task-spec.anchor"
        ));
        // last is a string.
        let mut artifact = scaffold("agl-001-x", "2026-10-17");
        let Value::Table(mut table) = artifact.type_table.clone() else {
            panic!("a table")
        };
        table.insert("last".into(), Value::Integer(1));
        artifact.type_table = Value::Table(table);
        assert!(matches!(
            take(&artifact),
            Err(Error::WrongType { key, .. }) if key == "task-spec.last"
        ));
    }

    #[test]
    fn the_git_flags() {
        let dir = TempDir::new();
        let repo = git_repo(&dir.0, 4);
        let head = back(&repo, 0);
        // A valid range that does not reach the HEAD: lags while open
        // or in-progress.
        let spec = TaskSpec { anchor: back(&repo, 2), last: back(&repo, 1) };
        assert_eq!(git_flag(&spec, Status::Open, &repo).unwrap(), Some(GitFlag::Lags));
        assert_eq!(
            git_flag(&spec, Status::InProgress, &repo).unwrap(),
            Some(GitFlag::Lags)
        );
        // done, backlog and canceled keep last as history: never lags.
        assert_eq!(git_flag(&spec, Status::Done, &repo).unwrap(), Some(GitFlag::Ok));
        assert_eq!(git_flag(&spec, Status::Backlog, &repo).unwrap(), Some(GitFlag::Ok));
        assert_eq!(git_flag(&spec, Status::Canceled, &repo).unwrap(), Some(GitFlag::Ok));
        // last is the HEAD: ok.
        let spec = TaskSpec { anchor: back(&repo, 1), last: head.clone() };
        assert_eq!(git_flag(&spec, Status::Open, &repo).unwrap(), Some(GitFlag::Ok));
        // An empty anchor: the range is not checked.
        let spec = TaskSpec { anchor: String::new(), last: head };
        assert_eq!(git_flag(&spec, Status::Open, &repo).unwrap(), Some(GitFlag::Ok));
        // A broken range: the anchor is not an ancestor of the last, in
        // any status.
        let spec = TaskSpec { anchor: back(&repo, 1), last: back(&repo, 2) };
        assert_eq!(git_flag(&spec, Status::Open, &repo).unwrap(), Some(GitFlag::Broken));
        assert_eq!(git_flag(&spec, Status::Done, &repo).unwrap(), Some(GitFlag::Broken));
        // An empty last: no flag.
        let spec = TaskSpec { anchor: back(&repo, 2), last: String::new() };
        assert_eq!(git_flag(&spec, Status::Open, &repo).unwrap(), None);
        // An id that does not resolve.
        let spec = TaskSpec { anchor: String::new(), last: "nope".into() };
        assert!(matches!(
            git_flag(&spec, Status::Open, &repo),
            Err(Error::CommitNotFound { id, .. }) if id == "nope"
        ));
    }

    #[test]
    fn today_is_a_calendar_date() {
        let today = today();
        assert!(crate::forge::check_date("today", &today, Path::new("/x")).is_ok());
    }
}
