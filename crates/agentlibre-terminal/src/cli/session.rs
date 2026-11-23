//! `agl session show <id|name|last>` (AGL-063, D3): the transcript of a
//! session file, with no server and no model: the events in the order of
//! the file; `--verbose` prints the lines the transcript skips.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use crate::action::Value;
use crate::cli::args::SessionShow;
use crate::error::Error;
use crate::error::CoreError;
use agentlibre_common::i18n::Lang;
use crate::paths::Paths;
use crate::session::{CommandRun, Event, Session};
use crate::settings::UserFile;

/// The id `key` names (AGL-063, D3): `last` is the newest session of the
/// project, a name is looked up in the project first, and an id or the
/// start of one reaches any project.
pub fn id_of(dir: &Path, project: &Path, key: &str) -> Result<String, Error> {
    if key == "last" {
        Session::latest(dir, project, 1)
            .into_iter()
            .next()
            .map(|info| info.id)
            .ok_or_else(|| {
                crate::session::error::Error::NotFound {
                    id: "last".into(),
                    dir: dir.to_path_buf(),
                }
                .into()
            })
    } else {
        Ok(Session::id_named(dir, project, key).unwrap_or_else(|| key.to_owned()))
    }
}

/// The transcript of the session (AGL-063, D3): the header's words, then
/// the events in the order of the file; `verbose` appends the raw lines,
/// so the observer's and the non-events are there too.
/// [`id_of`] among the sessions of the plane whose id is `plane`.
pub fn id_of_plane(dir: &Path, plane: &str, key: &str) -> Result<String, Error> {
    if key == "last" {
        Session::latest_of_plane(dir, plane, 1)
            .into_iter()
            .next()
            .map(|info| info.id)
            .ok_or_else(|| {
                crate::session::error::Error::NotFound {
                    id: "last".into(),
                    dir: dir.to_path_buf(),
                }
                .into()
            })
    } else {
        Ok(Session::id_named_of_plane(dir, plane, key).unwrap_or_else(|| key.to_owned()))
    }
}

pub fn transcript(session: &Session, path: &Path, verbose: bool) -> Result<String, Error> {
    let mut out = String::new();
    out.push_str(&format!("session {}\n", session.id));
    if let Some(name) = &session.name {
        out.push_str(&format!("name {name}\n"));
    }
    out.push_str(&format!("function {}\n", session.function));
    out.push_str(&format!("workspace {}\n", session.workspace.display()));
    // The plane of the session (AGL-077, D10).
    if let Some(plane) = &session.plane {
        out.push_str(&format!("plane {} {}\n", plane.name, plane.id));
    }
    out.push('\n');
    for event in &session.events {
        event_text(event, &mut out);
    }
    if verbose {
        out.push_str("file\n");
        let text = fs::read_to_string(path).map_err(|source| Error::from(CoreError::Read {
            path: path.to_path_buf(),
            source,
        }))?;
        for line in text.lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }
    Ok(out)
}

/// One event of the transcript: the word, then the text; a text's later
/// lines go under their first.
fn event_text(event: &Event, out: &mut String) {
    match event {
        Event::User(text) => labeled(out, "user", text),
        Event::Wake { flow, text } => labeled(out, &format!("wake {flow}"), text),
        Event::State(name) => out.push_str(&format!("state {name}\n")),
        Event::Thinking(text) => labeled(out, "thinking", text),
        Event::Call { text, call } => {
            if let Some(before) = text {
                labeled(out, "text", before);
            }
            out.push_str(&format!("call {} {}\n", call.id, call.name));
            for (name, value) in &call.args {
                out.push_str(&format!("  {name} = {}\n", value_text(value)));
            }
        }
        Event::Result { id, outcome, content } => {
            labeled(out, &format!("result {id} {}", outcome.name()), content);
        }
        Event::Forge { name, kind, section } => match section {
            Some(section) => out.push_str(&format!("forge {name} {} {section}\n", kind.name())),
            None => out.push_str(&format!("forge {name} {}\n", kind.name())),
        },
        Event::Answer(text) => labeled(out, "answer", text),
        Event::CutOff(cut) => {
            let mut line = format!("cut-off limit={} discarded={}", cut.limit, cut.discarded);
            if let Some(thinking) = cut.thinking {
                line.push_str(&format!(" thinking={thinking}"));
            }
            if cut.call {
                line.push_str(" call");
            }
            out.push_str(&line);
            out.push('\n');
        }
        Event::Stir { attempt, error } => out.push_str(&format!("stir {attempt} {error}\n")),
        Event::Stop(reason) => out.push_str(&format!("stop {}\n", reason.name())),
        Event::Memory { name, kind, text } => match text {
            Some(text) => labeled(out, &format!("memory {name} {}", kind.name()), text),
            None => out.push_str(&format!("memory {name} {}\n", kind.name())),
        },
        Event::Media(media) => {
            out.push_str(&format!(
                "media {}#{} {}\n",
                media.kind.label(),
                media.number,
                media.name
            ));
            if let Some(transcript) = &media.transcript {
                labeled(out, "transcript", transcript);
            }
        }
        Event::Compact { summary, question } => {
            labeled(out, "compact", summary);
            if let Some(question) = question {
                labeled(out, "kept-question", question);
            }
        }
        Event::Command(CommandRun { line, failed, output }) => {
            out.push_str(&format!("command {line}{}\n", if *failed { " failed" } else { "" }));
            if let Some(output) = output {
                labeled(out, "output", output);
            }
        }
        Event::System { text, failed } => {
            out.push_str(&format!("system{} {text}\n", if *failed { " failed" } else { "" }));
        }
    }
}

/// The argument of a call in the transcript.
fn value_text(value: &Value) -> String {
    match value {
        Value::Text(text) => text.clone(),
        Value::Int(int) => int.to_string(),
    }
}

/// `label` on the first line of `text`, its later lines under it.
fn labeled(out: &mut String, label: &str, text: &str) {
    let mut first = true;
    for line in text.lines() {
        if first {
            out.push_str(&format!("{label} {line}\n"));
            first = false;
        } else {
            out.push_str(&format!("    {line}\n"));
        }
    }
    if first {
        out.push_str(&format!("{label}\n"));
    }
}

pub fn run(
    show: SessionShow,
    plane: Option<&str>,
    user: &UserFile,
    paths: &Paths,
    cwd: &Path,
    lang: Lang,
) -> Result<ExitCode, Error> {
    let dir = paths.sessions();
    // `--plane`: `last` and a name among the plane's sessions (AGL-077,
    // D13).
    let id = match plane {
        Some(name) => {
            let plane = crate::plane::PlaneFile::open(paths, name)?;
            id_of_plane(&dir, &plane.id, &show.key)?
        }
        None => {
            let auto_rehome = user.forge.auto_rehome.unwrap_or(true);
            let ws = crate::cli::forge::project_of(paths, cwd, auto_rehome, lang)?;
            id_of(&dir, &ws.project, &show.key)?
        }
    };
    let session = Session::open(&dir, &id)?;
    let path = dir.join(format!("{}.jsonl", session.id));
    // A closed pipe, as in `agl session show last | head`, is not an error.
    let _ = write!(io::stdout(), "{}", transcript(&session, &path, show.verbose)?);
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Outcome;
    use crate::backend::ToolCall;
    use agentlibre_common::testutil::TempDir;
    use crate::media::{Kind as MediaKind, Media};
    use crate::memory::Kind as MemoryKind;
    use crate::session::{CutOff, ForgeKind, StopReason};
    use serde_json::json;

    fn paths(data: &Path) -> Paths {
        Paths {
            config: data.to_path_buf(),
            data: data.to_path_buf(),
            state: data.to_path_buf(),
            runtime: None,
        }
    }

    fn a_session(data: &Path, project: &Path) -> Session {
        crate::cli::forge::project_of(&paths(data), project, true, Lang::En).unwrap();
        let dir = paths(data).sessions();
        let mut session = Session::create(&dir, "coder", project).unwrap();
        session.append(Event::User("Fix it\nsecond line".into())).unwrap();
        session.append(Event::State("explore".into())).unwrap();
        session.append(Event::Thinking("Let me look.".into())).unwrap();
        session
            .append(Event::Call {
                text: Some("I will list the files.".into()),
                call: ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    args: vec![
                        ("command".into(), Value::Text("ls".into())),
                        ("timeout".into(), Value::Int(30)),
                    ],
                },
            })
            .unwrap();
        session
            .append(Event::Result {
                id: "c1".into(),
                outcome: Outcome::Ok,
                content: "out\n".into(),
            })
            .unwrap();
        session
            .append(Event::Compact {
                summary: "The test is fixed.".into(),
                question: Some("Fix it".into()),
            })
            .unwrap();
        session
            .append(Event::CutOff(CutOff {
                limit: 12288,
                thinking: Some(4000),
                discarded: 8288,
                call: true,
            }))
            .unwrap();
        session
            .append(Event::Stir { attempt: 1, error: "no action".into() })
            .unwrap();
        session
            .append(Event::Command(CommandRun {
                line: "/status".into(),
                failed: false,
                output: Some("ok\n".into()),
            }))
            .unwrap();
        session
            .append(Event::Forge {
                name: "agl-063-x".into(),
                kind: ForgeKind::Appended,
                section: Some("Progress".into()),
            })
            .unwrap();
        session
            .append(Event::Memory {
                name: "tea".into(),
                kind: MemoryKind::Added,
                text: Some("the hook".into()),
            })
            .unwrap();
        session
            .append(Event::Media(Media {
                kind: MediaKind::Image,
                number: 1,
                sha256: "x".into(),
                ext: "png".into(),
                name: "a.png".into(),
                transcript: None,
                frames: vec![],
            }))
            .unwrap();
        session.append(Event::Answer("Done.".into())).unwrap();
        session.append(Event::Stop(StopReason::StepLimit)).unwrap();
        session
    }

    #[test]
    fn the_transcript_is_the_header_and_the_events_in_order() {
        let root = TempDir::new();
        let data = root.0.join("data");
        let project = root.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let session = a_session(&data, &project);
        let text = transcript(
            &session,
            &data.join("sessions").join(format!("{}.jsonl", session.id)),
            false,
        )
        .unwrap();
        assert!(text.starts_with(&format!("session {}\n", session.id)), "{text}");
        assert!(text.contains("function coder\n"), "{text}");
        // The events in the order of the file, each under its word.
        let marks = [
            "user Fix it\n    second line",
            "state explore",
            "thinking Let me look.",
            "text I will list the files.",
            "call c1 bash\n  command = ls\n  timeout = 30",
            "result c1 ok out",
            "compact The test is fixed.\nkept-question Fix it",
            "cut-off limit=12288 discarded=8288 thinking=4000 call",
            "stir 1 no action",
            "command /status\noutput ok",
            "forge agl-063-x appended Progress",
            "memory tea added the hook",
            "media Image#1 a.png",
            "answer Done.",
            "stop step_limit",
        ];
        let mut at = 0;
        for mark in marks {
            let found = text[at..].find(mark).unwrap_or_else(|| {
                panic!("the transcript lost `{mark}`:\n{text}")
            });
            at += found;
        }
        // Without `--verbose` the file's raw lines are not there.
        assert!(!text.contains("\"type\""), "{text}");
    }

    #[test]
    fn verbose_prints_the_lines_the_transcript_skips() {
        let root = TempDir::new();
        let data = root.0.join("data");
        let project = root.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let created = a_session(&data, &project);
        // The observer's line and the name line, as the writer puts them.
        let path = data.join("sessions").join(format!("{}.jsonl", created.id));
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"type\":\"step\",\"think_budget\":111,\"action_budget\":222}\n");
        raw.push_str(&format!(
            "{}\n",
            json!({ "type": "name", "schema": 1, "name": "way" })
        ));
        std::fs::write(&path, raw).unwrap();
        let session = Session::open(&data.join("sessions"), &created.id).unwrap();
        assert_eq!(session.name.as_deref(), Some("way"));
        assert_eq!(session.last_budgets, Some((111, 222)));
        let text = transcript(&session, &path, true).unwrap();
        assert!(text.contains("name way\n"), "{text}");
        assert!(text.contains("file\n"), "{text}");
        assert!(
            text.contains("  {\"type\":\"step\",\"think_budget\":111,\"action_budget\":222}\n"),
            "{text}"
        );
        assert!(text.contains("\"type\":\"session\""), "{text}");
        let plain = transcript(&session, &path, false).unwrap();
        assert!(!plain.contains("file\n"), "{plain}");
        assert!(!plain.contains("think_budget"), "{plain}");
    }

    #[test]
    fn the_lookup_is_last_then_name_then_id() {
        let root = TempDir::new();
        let data = root.0.join("data");
        let project = root.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let dir = data.join("sessions");
        // A session of another project is not the `last` of this one.
        let other = root.0.join("other");
        std::fs::create_dir_all(&other).unwrap();
        // A millisecond between the sessions: the uuid v7 orders by its
        // time, and `latest` reads that order.
        let outsider = a_session(&data, &other);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let first = a_session(&data, &project);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = a_session(&data, &project);
        assert_eq!(id_of(&dir, &project, "last").unwrap(), second.id);
        // A name is looked up in the project as it was typed (AGL-062, D1).
        let mut second = Session::open(&dir, &second.id).unwrap();
        second.rename(&project, "way").unwrap();
        assert_eq!(id_of(&dir, &project, "way").unwrap(), second.id);
        // An id, or the start of one, reaches the session of any project.
        assert_eq!(id_of(&dir, &project, &first.id).unwrap(), first.id);
        // The shortest prefix of the id that no other file shares.
        let others = [second.id.clone(), outsider.id.clone()];
        let mut len = 1;
        while first.id.len() > len
            && others
                .iter()
                .any(|id| id.starts_with(&first.id[..len]))
        {
            len += 1;
        }
        let prefix = &first.id[..len];
        assert_eq!(id_of(&dir, &project, prefix).unwrap(), *prefix);
        assert_eq!(Session::open(&dir, prefix).unwrap().id, first.id);
    }

    #[test]
    fn no_session_of_the_project_is_named() {
        let root = TempDir::new();
        let data = root.0.join("data");
        let project = root.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let dir = data.join("sessions");
        match id_of(&dir, &project, "last") {
            Err(Error::Core(CoreError::Session(crate::session::error::Error::NotFound { id, .. }))) => assert_eq!(id, "last"),
            other => panic!("expected the not-found, got {other:?}"),
        }
    }
}
