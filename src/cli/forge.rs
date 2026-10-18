//! `agentLIBRE forge list|show|new|status|check|rehome`: the artifact
//! store of the workspace of the current directory (AGL-007).

use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use crate::error::Error;
use crate::forge;
use crate::forge::task_spec;
use crate::forge::task_spec::{GitFlag, Status};
use crate::forge::{Artifact, Header, Workspace};
use crate::i18n::{Lang, Msg, m};
use crate::paths::Paths;
use crate::settings::UserFile;
use crate::tr;

/// Runs a `forge` command over the arguments after the word `forge`,
/// in the workspace of `cwd`.
pub fn run(
    args: &[OsString],
    user: &UserFile,
    paths: &Paths,
    cwd: &Path,
    lang: Lang,
) -> Result<ExitCode, Error> {
    // No subcommand: the help of the program.
    if args.is_empty() {
        return help(lang);
    }
    let auto_rehome = user.forge.auto_rehome.unwrap_or(true);
    match word(args, 0, "forge")? {
        "-h" | "--help" => help(lang),
        "list" => {
            no_extra(args, 1)?;
            let ws = resolve_workspace(paths, cwd, auto_rehome, lang)?;
            list(&ws)
        }
        "show" => {
            no_extra(args, 2)?;
            let name = word(args, 1, "show")?.to_string();
            let ws = resolve_workspace(paths, cwd, auto_rehome, lang)?;
            let artifact = find_artifact(paths, &ws, &name)?;
            // A closed pipe, as in `forge show x | head`, is not an error.
            let _ = write!(io::stdout(), "{}", forge::render(&artifact));
            Ok(ExitCode::SUCCESS)
        }
        "new" => {
            let ws = resolve_workspace(paths, cwd, auto_rehome, lang)?;
            new_task_spec(paths, &ws, lang, args)
        }
        "status" => {
            no_extra(args, 3)?;
            let name = word(args, 1, "status")?.to_string();
            let state = word(args, 2, "status")?;
            let ws = resolve_workspace(paths, cwd, auto_rehome, lang)?;
            status(paths, &ws, lang, &name, state)
        }
        "check" => {
            no_extra(args, 2)?;
            let name = word(args, 1, "check")?.to_string();
            let ws = resolve_workspace(paths, cwd, auto_rehome, lang)?;
            check(paths, &ws, cwd, lang, &name)
        }
        "rehome" => {
            let (store, new_store) = rehome_args(args)?;
            let ws = resolve_workspace(paths, cwd, auto_rehome, lang)?;
            rehome(paths, &ws, lang, store, new_store)
        }
        other => Err(Error::UnknownOption(other.to_string())),
    }
}

/// The `--help` of a `forge` command: the help of the program.
fn help(lang: Lang) -> Result<ExitCode, Error> {
    // A closed pipe, as in `agentLIBRE forge --help | head`, is not an
    // error.
    let _ = writeln!(io::stdout(), "{}", tr!(lang, m::HELP));
    Ok(ExitCode::SUCCESS)
}

/// The argument `i` as text; a missing argument is a `MissingValue` of
/// `what`.
fn word<'a>(args: &'a [OsString], i: usize, what: &'static str) -> Result<&'a str, Error> {
    let Some(arg) = args.get(i) else {
        return Err(Error::MissingValue(what.to_string()));
    };
    arg.to_str().ok_or(Error::InvalidUnicode)
}

/// Rejects an argument past the command's own.
fn no_extra(args: &[OsString], count: usize) -> Result<(), Error> {
    if let Some(extra) = args.get(count) {
        return Err(Error::UnknownOption(extra.to_string_lossy().into_owned()));
    }
    Ok(())
}

/// Resolves the workspace of `cwd` against the stores under the data
/// directory, printing the notification lines of the resolution.
fn resolve_workspace(
    paths: &Paths,
    cwd: &Path,
    auto_rehome: bool,
    lang: Lang,
) -> Result<Workspace, Error> {
    let (ws, lines) = forge::resolve(
        &paths.data,
        cwd,
        lang,
        auto_rehome,
        &|options| ask_store(m::FORGE_ASK_STORE, options, lang),
    )?;
    for line in lines {
        println!("{line}");
    }
    Ok(ws)
}

/// The store ask: the names of the stores plus a new store. On a
/// terminal the human picks one; off a terminal the error points at
/// `forge rehome`.
fn ask_store(question: Msg, options: &[String], lang: Lang) -> Result<usize, Error> {
    let stores = options[..options.len() - 1].to_vec();
    if !crate::repl::term::is_interactive() {
        return Err(Error::RehomeNoStore { stores });
    }
    println!("{}", tr!(lang, question));
    for (i, option) in options.iter().enumerate() {
        println!("  {}. {option}", i + 1);
    }
    let mut line = String::new();
    let stdin = io::stdin();
    let mut lock = stdin.lock();
    loop {
        line.clear();
        match lock.read_line(&mut line) {
            Ok(0) => return Err(Error::RehomeNoStore { stores }),
            Ok(_) => {
                let n = line.trim().parse::<usize>();
                if let Ok(n) = n && (1..=options.len()).contains(&n) {
                    return Ok(n - 1);
                }
                eprintln!("{}", tr!(lang, m::FORGE_ASK_INVALID, last = options.len()));
            }
            Err(source) => {
                return Err(Error::Read { path: PathBuf::from("<stdin>"), source });
            }
        }
    }
}

/// `forge list`: the table of the artifacts of the workspace.
fn list(ws: &Workspace) -> Result<ExitCode, Error> {
    println!(
        "{:<24} {:<10} {:<8} {:<10} {}",
        "NAME", "TYPE", "STATUS", "UPDATED", "FLAG"
    );
    for path in forge::artifact_files(&ws.dir)? {
        let stem = path
            .file_stem()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        match forge::read_header(&path)? {
            Header::Artifact(artifact) => {
                let flag = if artifact.type_name == task_spec::TYPE {
                    git_flag_cell(&artifact, &ws.toplevel)?
                } else {
                    String::new()
                };
                println!(
                    "{:<24} {:<10} {:<8} {:<10} {}",
                    artifact.name, artifact.type_name, artifact.status, artifact.updated, flag
                );
            }
            Header::UnknownSchema { schema, .. } => {
                println!(
                    "{:<24} {:<10} {:<8} {:<10} !schema {schema}",
                    stem, "", "", ""
                );
            }
            Header::Broken { detail, .. } => {
                println!(
                    "{:<24} {:<10} {:<8} {:<10} !parse {detail}",
                    stem, "", "", ""
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The git flag cell of the table for a task-spec: blank when `last`
/// is empty, `ok`, `lags` or `broken` (a range that cannot be
/// verified is broken too).
fn git_flag_cell(artifact: &Artifact, toplevel: &Path) -> Result<String, Error> {
    let spec = task_spec::take(artifact)?;
    let status = Status::parse(&artifact.status)
        .ok_or_else(|| Error::BadState { state: artifact.status.clone() })?;
    match task_spec::git_flag(&spec, status, toplevel) {
        Ok(None) => Ok(String::new()),
        Ok(Some(GitFlag::Ok)) => Ok("ok".into()),
        Ok(Some(GitFlag::Lags)) => Ok("lags".into()),
        Ok(Some(GitFlag::Broken)) | Err(_) => Ok("broken".into()),
    }
}

/// `forge new task-spec --title <t> [--name <n>]`: the scaffold of a
/// new task-spec, `agl-<max+1>-<slug(title)>` or `--name` verbatim.
fn new_task_spec(
    paths: &Paths,
    ws: &Workspace,
    lang: Lang,
    args: &[OsString],
) -> Result<ExitCode, Error> {
    let type_name = word(args, 1, "new")?.to_string();
    if type_name != task_spec::TYPE {
        return Err(Error::NewType { type_name });
    }
    let mut title: Option<String> = None;
    let mut name: Option<String> = None;
    let mut i = 2;
    while i < args.len() {
        let arg = word(args, i, "new")?;
        i += 1;
        match arg {
            "--title" => {
                if title.is_some() {
                    return Err(Error::RepeatedOption("--title".into()));
                }
                let value = word(args, i, "--title")?;
                i += 1;
                title = Some(value.to_string());
            }
            "--name" => {
                if name.is_some() {
                    return Err(Error::RepeatedOption("--name".into()));
                }
                let value = word(args, i, "--name")?;
                i += 1;
                name = Some(value.to_string());
            }
            _ => return Err(Error::UnknownOption(arg.to_string())),
        }
    }
    let Some(title) = title else {
        return Err(Error::MissingValue("--title".into()));
    };
    let existing = artifact_names(ws)?;
    let name = match name {
        Some(name) => {
            if !valid_name(&name) {
                return Err(Error::InvalidName(name));
            }
            name
        }
        None => format!(
            "agl-{:03}-{}",
            task_spec::next_number(&existing),
            task_spec::slug(&title)
        ),
    };
    if existing.contains(&name) {
        return Err(Error::NameTaken {
            name: name.clone(),
            dir: paths.forge_dir(&ws.key),
        });
    }
    let path = paths
        .forge_dir(&ws.key)
        .join(task_spec::TYPE)
        .join(format!("{name}.md"));
    let mut artifact = task_spec::scaffold(&name, &task_spec::today());
    artifact.path = path.clone();
    for line in forge::materialize(ws, lang)? {
        println!("{line}");
    }
    forge::write_artifact(&artifact)?;
    println!("{}", tr!(lang, m::FORGE_NEW_CREATED, path = path.display()));
    Ok(ExitCode::SUCCESS)
}

/// A name the human gave with `--name`: non-empty, no path separator.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\')
}

/// `forge status <name> <state>`: validates the transition, bumps
/// `updated`, rewrites the file with the body byte-identical.
fn status(
    paths: &Paths,
    ws: &Workspace,
    lang: Lang,
    name: &str,
    state: &str,
) -> Result<ExitCode, Error> {
    let artifact = find_artifact(paths, ws, name)?;
    if artifact.type_name != task_spec::TYPE {
        return Err(Error::NotTaskSpec {
            name: name.to_string(),
            type_name: artifact.type_name,
        });
    }
    let from = Status::parse(&artifact.status)
        .ok_or_else(|| Error::BadState { state: artifact.status.clone() })?;
    let to = Status::parse(state).ok_or_else(|| Error::BadState {
        state: state.to_string(),
    })?;
    if from == to || !from.allows(to) {
        return Err(Error::BadTransition {
            from: from.name().to_string(),
            to: to.name().to_string(),
        });
    }
    let mut artifact = artifact;
    artifact.status = to.name().to_string();
    artifact.updated = task_spec::today();
    forge::write_artifact(&artifact)?;
    println!(
        "{}",
        tr!(
            lang,
            m::FORGE_STATUS_CHANGED,
            name = name,
            from = from.name(),
            to = to.name()
        )
    );
    Ok(ExitCode::SUCCESS)
}

/// `forge check <name>`: the structural checks (the name matches the
/// file, the range, the `deps` and `supersedes` resolve), then the
/// `[verification]` cmds via `sh -c` in the cwd, streamed, the first
/// failing command stops the run. On a pass of an `open`/`ready` spec
/// that lags (or has no `last` yet), `last` = HEAD and `updated` are
/// recorded. Silent on success; the failures are lines.
fn check(
    paths: &Paths,
    ws: &Workspace,
    cwd: &Path,
    lang: Lang,
    name: &str,
) -> Result<ExitCode, Error> {
    let artifact = find_artifact(paths, ws, name)?;
    let mut failures: Vec<String> = Vec::new();
    // The name matches the file it is in.
    let stem = artifact
        .path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if artifact.name != stem {
        failures.push(tr!(
            lang,
            m::FORGE_CHECK_NAME_MISMATCH,
            name = artifact.name.as_str(),
            file = stem
        ));
    }
    if artifact.type_name == task_spec::TYPE {
        // The range: the set ids resolve, the anchor is an ancestor of
        // the last.
        let spec = task_spec::take(&artifact).unwrap_or_else(|error| {
            failures.push(error.to_string());
            task_spec::TaskSpec {
                anchor: String::new(),
                last: String::new(),
            }
        });
        if !spec.anchor.is_empty() && !id_resolves(cwd, &spec.anchor) {
            failures.push(tr!(
                lang,
                m::COMMIT_NOT_FOUND,
                id = spec.anchor.as_str(),
                repo = cwd.display()
            ));
        }
        if !spec.last.is_empty() && !id_resolves(cwd, &spec.last) {
            failures.push(tr!(
                lang,
                m::COMMIT_NOT_FOUND,
                id = spec.last.as_str(),
                repo = cwd.display()
            ));
        }
        if !spec.anchor.is_empty()
            && !spec.last.is_empty()
            && id_resolves(cwd, &spec.anchor)
            && id_resolves(cwd, &spec.last)
        {
            let status =
                Status::parse(&artifact.status).unwrap_or(Status::Open);
            if let Ok(Some(GitFlag::Broken)) = task_spec::git_flag(&spec, status, cwd) {
                failures.push(tr!(
                    lang,
                    m::BROKEN_RANGE,
                    anchor = spec.anchor.as_str(),
                    last = spec.last.as_str()
                ));
            }
        }
    }
    // `deps` and `supersedes` resolve in the store.
    let names = artifact_names(ws)?;
    for (kind, list) in [("dep", &artifact.deps), ("supersedes", &artifact.supersedes)] {
        for dep in list {
            if !names.contains(dep) {
                failures.push(tr!(
                    lang,
                    m::FORGE_CHECK_NOT_IN_STORE,
                    kind = kind,
                    name = dep.as_str()
                ));
            }
        }
    }
    if !failures.is_empty() {
        for failure in &failures {
            eprintln!("{failure}");
        }
        return Ok(ExitCode::FAILURE);
    }
    // The verification cmds: `sh -c` in the cwd, streamed, the first
    // failing command stops the run.
    if run_cmds(cwd, lang, &artifact)? {
        return Ok(ExitCode::FAILURE);
    }
    // A pass of an `open`/`ready` spec records a fresh `last` = HEAD
    // when the header lags or `last` is empty, and bumps `updated`.
    if artifact.type_name == task_spec::TYPE
        && let Ok(spec) = task_spec::take(&artifact)
        && let Some(status) = Status::parse(&artifact.status)
        && matches!(status, Status::Open | Status::Ready)
    {
        let head = forge::git(cwd, &["rev-parse", "HEAD"])?;
        if spec.last.is_empty() || spec.last != head {
            let mut spec = spec;
            spec.last = head;
            let mut artifact = artifact.clone();
            artifact.updated = task_spec::today();
            artifact.type_table = task_spec_table(spec);
            forge::write_artifact(&artifact)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The artifact of the store of `ws` named `name`; the files are
/// parsed one by one, the match is on the `name` field, the file is
/// `forge/<type>/<name>.md`.
fn find_artifact(paths: &Paths, ws: &Workspace, name: &str) -> Result<Artifact, Error> {
    for path in forge::artifact_files(&ws.dir)? {
        if let Ok(artifact) = forge::load_artifact(&path)
            && artifact.name == name
        {
            return Ok(artifact);
        }
    }
    Err(Error::ArtifactNotFound {
        name: name.to_string(),
        dir: paths.forge_dir(&ws.key),
    })
}

/// The `name`s of the artifacts of the store of `ws`.
fn artifact_names(ws: &Workspace) -> Result<Vec<String>, Error> {
    let mut names = Vec::new();
    for path in forge::artifact_files(&ws.dir)? {
        if let Header::Artifact(artifact) = forge::read_header(&path)? {
            names.push(artifact.name);
        }
    }
    Ok(names)
}

/// Whether `id` resolves to a commit in the repository at `cwd`.
fn id_resolves(cwd: &Path, id: &str) -> bool {
    forge::git(
        cwd,
        &["rev-parse", "--verify", &format!("{id}^{{commit}}")],
    )
    .is_ok()
}

/// The `[task-spec]` table of `spec`.
fn task_spec_table(spec: task_spec::TaskSpec) -> toml::Value {
    let mut table = toml::Table::new();
    table.insert("anchor".into(), toml::Value::String(spec.anchor));
    table.insert("last".into(), toml::Value::String(spec.last));
    toml::Value::Table(table)
}

/// Runs the `[verification]` cmds of `artifact` via `sh -c` in
/// `cwd`, their output on the parent's own streams (streamed, not
/// captured); `true` when one fails (the run stops at the first
/// failure).
fn run_cmds(cwd: &Path, lang: Lang, artifact: &Artifact) -> Result<bool, Error> {
    for cmd in artifact.cmds.as_deref().unwrap_or_default() {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|source| Error::CmdSpawn { source })?;
        if !status.success() {
            eprintln!("{}", tr!(lang, m::FORGE_CHECK_FAILED, cmd = cmd.as_str()));
            return Ok(true);
        }
    }
    Ok(false)
}

/// The arguments of `forge rehome`: a store name and/or `--new`;
/// parsed before the workspace, like the arguments of the other
/// subcommands.
fn rehome_args(args: &[OsString]) -> Result<(Option<String>, bool), Error> {
    let mut store: Option<String> = None;
    let mut new_store = false;
    let mut i = 1;
    while i < args.len() {
        let arg = word(args, i, "rehome")?;
        i += 1;
        match arg {
            "--new" => {
                if new_store {
                    return Err(Error::RepeatedOption("--new".into()));
                }
                if store.is_some() {
                    return Err(Error::UnknownOption("--new".into()));
                }
                new_store = true;
            }
            _ if store.is_none() => store = Some(arg.to_string()),
            _ => return Err(Error::UnknownOption(arg.to_string())),
        }
    }
    Ok((store, new_store))
}

/// `forge rehome [<store>] [--new]`: the explicit form of the
/// workspace migration.
fn rehome(
    paths: &Paths,
    ws: &Workspace,
    lang: Lang,
    store: Option<String>,
    new_store: bool,
) -> Result<ExitCode, Error> {
    let found = forge::stores(&paths.data)?;
    // The store the workspace already carries, when its marker is
    // written: re-homing to another store would put the identity in
    // two markers.
    let current = found
        .iter()
        .find(|store| store.hashes.iter().any(|hash| hash == &ws.identity));
    if new_store {
        return rehome_new(ws, lang, current);
    }
    let Some(name) = store else {
        if !crate::repl::term::is_interactive() {
            return Err(Error::RehomeNoStore {
                stores: found.iter().map(forge::Store::name).collect(),
            });
        }
        let mut options: Vec<String> = found.iter().map(forge::Store::name).collect();
        options.push(tr!(lang, m::FORGE_NEW_STORE).to_string());
        let choice = ask_store(m::FORGE_ASK_REHOME, &options, lang)?;
        if choice + 1 == options.len() {
            return rehome_new(ws, lang, current);
        }
        return rehome_to(paths, ws, lang, &found[choice].name(), current);
    };
    rehome_to(paths, ws, lang, &name, current)
}

/// `forge rehome --new`: a fresh single-hash store, refused while the
/// identity already carries a store (the two-markers trap).
fn rehome_new(
    ws: &Workspace,
    lang: Lang,
    current: Option<&forge::Store>,
) -> Result<ExitCode, Error> {
    if let Some(store) = current {
        return Err(Error::RehomeInUse {
            current: store.name(),
            target: tr!(lang, m::FORGE_NEW_STORE).to_string(),
        });
    }
    println!("{}", forge::rehome_new(ws, lang)?);
    Ok(ExitCode::SUCCESS)
}

/// `forge rehome <store>`: the named store becomes the store of the
/// workspace; refused while another store already carries it.
fn rehome_to(
    paths: &Paths,
    ws: &Workspace,
    lang: Lang,
    name: &str,
    current: Option<&forge::Store>,
) -> Result<ExitCode, Error> {
    if let Some(store) = current
        && &store.name() != name
    {
        return Err(Error::RehomeInUse {
            current: store.name(),
            target: name.to_string(),
        });
    }
    println!("{}", forge::rehome_store(&paths.data, ws, name, lang)?);
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::testutil::TempDir;
    use std::os::unix::ffi::OsStringExt;

    /// The `run` of `args`, the settings of an empty user file, the
    /// data directory `data`, the workspace of `cwd`.
    fn forge(data: &Path, cwd: &Path, args: &[&str]) -> Result<ExitCode, Error> {
        let paths = Paths {
            config: data.to_path_buf(),
            data: data.to_path_buf(),
            state: data.to_path_buf(),
        };
        run(
            &args.iter().map(OsString::from).collect::<Vec<_>>(),
            &UserFile::default(),
            &paths,
            cwd,
            Lang::En,
        )
    }

    /// A git repository at `dir/proj` with one commit.
    fn repo(dir: &TempDir) -> PathBuf {
        let repo = dir.0.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
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
        std::fs::write(repo.join("f.txt"), "f\n").unwrap();
        run(&["add", "f.txt"]);
        run(&["commit", "-q", "-m", "one"]);
        repo
    }

    /// The store key of `repo` and its `agl-001-first-task.md` file.
    fn key_and_file(data: &Path, repo: &Path) -> (String, PathBuf) {
        let identity = forge::identity_of(&std::fs::canonicalize(repo).unwrap());
        let key = format!("proj-{}", &identity["sha256:".len()..19]);
        (
            key.clone(),
            data.join(&key).join("forge/task-spec/agl-001-first-task.md"),
        )
    }

    /// The Markdown body of a store file: everything after the closing
    /// `---` of the front-matter.
    fn body_of(text: &str) -> String {
        let mut parts = text.split("---\n");
        parts.next().unwrap();
        parts.next().unwrap();
        parts.collect::<Vec<_>>().join("---\n")
    }

    #[test]
    fn no_subcommand_or_a_help_flag_is_the_help() {
        let data = TempDir::new();
        let cases: [&[&str]; 3] = [&[], &["-h"], &["--help"]];
        for args in cases {
            assert_eq!(
                forge(data.0.as_path(), Path::new("/nowhere"), args).unwrap(),
                ExitCode::SUCCESS
            );
        }
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        let data = TempDir::new();
        let cases: [(&[&str], &str); 5] = [
            (&["frobnicate"], "frobnicate"),
            (&["list", "extra"], "extra"),
            (&["show", "a", "b"], "b"),
            (&["check", "a", "b"], "b"),
            (&["rehome", "a", "b"], "b"),
        ];
        for (args, arg) in cases {
            assert!(
                matches!(
                    forge(data.0.as_path(), Path::new("/nowhere"), args),
                    Err(Error::UnknownOption(given)) if given == arg
                ),
                "{args:?} is not {arg}"
            );
        }
    }

    #[test]
    fn missing_arguments_are_named() {
        let data = TempDir::new();
        let cases: [(&[&str], &str); 2] = [
            (&["show"], "show"),
            (&["status", "a"], "status"),
        ];
        for (args, what) in cases {
            assert!(
                matches!(
                    forge(data.0.as_path(), Path::new("/nowhere"), args),
                    Err(Error::MissingValue(given)) if given == what
                ),
                "{args:?} is not {what}"
            );
        }
    }

    #[test]
    fn a_non_utf8_argument_is_rejected() {
        // The arguments are raw `OsString`s; a word that is not UTF-8
        // cannot be a subcommand.
        let data = TempDir::new();
        let paths = Paths {
            config: data.0.clone(),
            data: data.0.clone(),
            state: data.0.clone(),
        };
        let result = run(
            &[OsString::from_vec(vec![0xffu8])],
            &UserFile::default(),
            &paths,
            Path::new("/nowhere"),
            Lang::En,
        );
        assert!(matches!(result, Err(Error::InvalidUnicode)));
    }

    #[test]
    fn a_human_name_is_valid() {
        assert!(valid_name("agl-008-x"));
        assert!(!valid_name(""));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("a\\b"));
    }

    #[test]
    fn new_scaffolds_the_next_number() {
        let dir = TempDir::new();
        let data = dir.0.join("data");
        let repo = repo(&dir);
        let (key, file) = key_and_file(&data, &repo);
        assert_eq!(
            forge(&data, &repo, &["new", "task-spec", "--title", "First Task"]).unwrap(),
            ExitCode::SUCCESS
        );
        assert!(data.join(&key).join("agentLIBRE.toml").is_file());
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("---\nschema = \"forge/v1\"\ntype = \"task-spec\""));
        assert!(text.contains("## Objective"));
        // The next number is the max plus one.
        assert_eq!(
            forge(&data, &repo, &["new", "task-spec", "--title", "Second Task"]).unwrap(),
            ExitCode::SUCCESS
        );
        assert!(data
            .join(&key)
            .join("forge/task-spec/agl-002-second-task.md")
            .is_file());
        // Other types are not scaffolded.
        assert!(matches!(
            forge(&data, &repo, &["new", "note", "--title", "t"]),
            Err(Error::NewType { type_name }) if type_name == "note"
        ));
        // `--title` is required and unique.
        assert!(matches!(
            forge(&data, &repo, &["new", "task-spec"]),
            Err(Error::MissingValue(what)) if what == "--title"
        ));
        assert!(matches!(
            forge(&data, &repo, &["new", "task-spec", "--title", "t", "--title", "u"]),
            Err(Error::RepeatedOption(opt)) if opt == "--title"
        ));
    }

    #[test]
    fn new_rejects_an_existing_or_an_invalid_name() {
        let dir = TempDir::new();
        let data = dir.0.join("data");
        let repo = repo(&dir);
        let (_, file) = key_and_file(&data, &repo);
        forge(&data, &repo, &["new", "task-spec", "--title", "First Task"]).unwrap();
        assert!(file.is_file());
        assert!(matches!(
            forge(
                &data,
                &repo,
                &["new", "task-spec", "--title", "Again", "--name", "agl-001-first-task"]
            ),
            Err(Error::NameTaken { name, .. }) if name == "agl-001-first-task"
        ));
        assert!(matches!(
            forge(&data, &repo, &["new", "task-spec", "--title", "Again", "--name", "a/b"]),
            Err(Error::InvalidName(name)) if name == "a/b"
        ));
    }

    #[test]
    fn status_walks_the_lifecycle_and_refuses_the_rest() {
        let dir = TempDir::new();
        let data = dir.0.join("data");
        let repo = repo(&dir);
        let (key, file) = key_and_file(&data, &repo);
        forge(&data, &repo, &["new", "task-spec", "--title", "First Task"]).unwrap();
        let before = body_of(&std::fs::read_to_string(&file).unwrap());
        for state in ["ready", "done"] {
            assert_eq!(
                forge(&data, &repo, &["status", "agl-001-first-task", state]).unwrap(),
                ExitCode::SUCCESS
            );
        }
        // The body is byte-identical through the walk.
        assert_eq!(
            body_of(&std::fs::read_to_string(&file).unwrap()),
            before
        );
        assert!(matches!(
            forge(&data, &repo, &["status", "agl-001-first-task", "open"]),
            Err(Error::BadTransition { .. })
        ));
        assert!(matches!(
            forge(&data, &repo, &["status", "agl-001-first-task", "frozen"]),
            Err(Error::BadState { .. })
        ));
        // The lifecycle is the task-spec's.
        let mut note = task_spec::scaffold("note", &task_spec::today());
        note.type_name = "note".into();
        note.path = data.join(&key).join("forge/note/note.md");
        forge::write_artifact(&note).unwrap();
        assert!(matches!(
            forge(&data, &repo, &["status", "note", "ready"]),
            Err(Error::NotTaskSpec { name, .. }) if name == "note"
        ));
        // An absent artifact.
        assert!(matches!(
            forge(&data, &repo, &["status", "ghost", "ready"]),
            Err(Error::ArtifactNotFound { name, .. }) if name == "ghost"
        ));
    }

    #[test]
    fn check_passes_a_fresh_spec_and_records_head() {
        let dir = TempDir::new();
        let data = dir.0.join("data");
        let repo = repo(&dir);
        let (_, file) = key_and_file(&data, &repo);
        forge(&data, &repo, &["new", "task-spec", "--title", "First Task"]).unwrap();
        assert_eq!(
            forge(&data, &repo, &["check", "agl-001-first-task"]).unwrap(),
            ExitCode::SUCCESS
        );
        let head = forge::git(&repo, &["rev-parse", "HEAD"]).unwrap();
        assert!(std::fs::read_to_string(&file)
            .unwrap()
            .contains(&format!("last = \"{head}\"")));
        // A dep that is not in the store fails the check.
        let mut artifact = forge::load_artifact(&file).unwrap();
        artifact.deps.push("ghost".into());
        forge::write_artifact(&artifact).unwrap();
        assert_eq!(
            forge(&data, &repo, &["check", "agl-001-first-task"]).unwrap(),
            ExitCode::FAILURE
        );
    }

    #[test]
    fn rehome_new_makes_a_store_and_the_second_time_is_refused() {
        let dir = TempDir::new();
        let data = dir.0.join("data");
        let repo = repo(&dir);
        let (key, _) = key_and_file(&data, &repo);
        assert_eq!(
            forge(&data, &repo, &["rehome", "--new"]).unwrap(),
            ExitCode::SUCCESS
        );
        assert!(data.join(&key).join("agentLIBRE.toml").is_file());
        // The identity is in a store now: a second `--new` and a named
        // other store are refused; the same store is a no-op.
        assert!(matches!(
            forge(&data, &repo, &["rehome", "--new"]),
            Err(Error::RehomeInUse { .. })
        ));
        assert!(matches!(
            forge(&data, &repo, &["rehome", "somewhere-else"]),
            Err(Error::RehomeInUse { .. })
        ));
        assert_eq!(
            forge(&data, &repo, &["rehome", key.as_str()]).unwrap(),
            ExitCode::SUCCESS
        );
        // `--new` after a store name is not an option.
        assert!(matches!(
            forge(&data, &repo, &["rehome", key.as_str(), "--new"]),
            Err(Error::UnknownOption(opt)) if opt == "--new"
        ));
    }
}
