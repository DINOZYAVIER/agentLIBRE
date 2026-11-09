use super::*;

/// A fresh directory, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let dir =
            std::env::temp_dir().join(format!("agentLIBRE-session-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Dir(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn call(id: &str, command: &str) -> Event {
    Event::Call {
        text: None,
        call: ToolCall {
            id: id.into(),
            name: "bash".into(),
            args: vec![
                ("command".into(), Value::Text(command.into())),
                ("timeout".into(), Value::Int(30)),
            ],
        },
    }
}

fn result(id: &str, outcome: Outcome) -> Event {
    Event::Result {
        id: id.into(),
        outcome,
        content: "out\n".into(),
    }
}

#[test]
fn utc_times() {
    assert_eq!(utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(utc(1_760_205_731), "2025-10-11T18:02:11Z");
    assert_eq!(utc(4_102_444_799), "2099-12-31T23:59:59Z");
}

#[test]
fn uuid_v7_layout() {
    let a = uuid_v7().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let b = uuid_v7().unwrap();
    for id in [&a, &b] {
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('7'));
        assert!(matches!(
            parts[3].chars().next(),
            Some('8' | '9' | 'a' | 'b')
        ));
    }
    // The time comes first, so ids sort by creation.
    assert!(a < b);
}

#[test]
fn write_and_read_back() {
    let dir = Dir::new("roundtrip");
    let events = [
        Event::User("Fix the test".into()),
        Event::State("explore".into()),
        Event::Thinking("Let me look".into()),
        call("c1", "ls"),
        result("c1", Outcome::Ok),
        Event::Stop(StopReason::StepLimit),
        Event::Answer("Done: `Vec<&str>`\nok".into()),
        Event::Compact { summary: "The test is fixed.".into(), question: None },
        Event::CutOff(CutOff {
            limit: 12288,
            thinking: Some(4000),
            discarded: 8288,
            call: true,
        }),
        Event::Compact {
            summary: "It is fixed for good.".into(),
            question: Some("Fix the test".into()),
        },
    ];
    let mut session = Session::create(&dir.0, "coder", Path::new("/home/u/repos/a")).unwrap();
    for event in &events {
        session.append(event.clone()).unwrap();
    }
    let text = fs::read_to_string(dir.0.join(format!("{}.jsonl", session.id))).unwrap();
    let lines: Vec<Json> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // The header, then one line per event in order (AGL-043 F3): the
    // structure, not a count to bump by hand.
    let types: Vec<&str> = lines
        .iter()
        .map(|line| line["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "session", "user", "state", "thinking", "call", "result", "stop", "answer", "compact",
            "cut_off", "compact"
        ]
    );
    assert_eq!(lines.len(), events.len() + 1);
    assert_eq!(lines[0]["version"], 1);
    assert_eq!(lines[0]["workspace"], "/home/u/repos/a");
    assert_eq!(lines[4]["args"], json!({"command": "ls", "timeout": 30}));
    assert_eq!(lines[8]["summary"], "The test is fixed.");
    assert_eq!(
        lines[9],
        json!({
            "type": "cut_off",
            "limit": 12288,
            "thinking": 4000,
            "discarded": 8288,
            "call": true,
            "time": lines[9]["time"]
        })
    );
    // The kept question rides the summary and round-trips (F31).
    assert_eq!(lines[10]["summary"], "It is fixed for good.");
    assert_eq!(lines[10]["question"], "Fix the test");
    assert!(
        lines
            .iter()
            .all(|l| l["time"].as_str().unwrap().ends_with('Z'))
    );

    let reopened = Session::open(&dir.0, &session.id).unwrap();
    assert_eq!(reopened.id, session.id);
    assert_eq!(reopened.function, "coder");
    assert_eq!(reopened.workspace, PathBuf::from("/home/u/repos/a"));
    assert_eq!(reopened.events, events);
    assert_eq!(reopened.next_call_id(), "c2");
}

#[test]
fn edit_arguments_keep_their_order() {
    let dir = Dir::new("order");
    let edit = Event::Call {
        text: Some("Let me fix it.".into()),
        call: ToolCall {
            id: "c1".into(),
            name: "edit".into(),
            args: ["path", "old", "new"]
                .iter()
                .map(|n| (n.to_string(), Value::Text(n.to_string())))
                .collect(),
        },
    };
    let mut session = Session::create(&dir.0, "minimal", Path::new("/w")).unwrap();
    session.append(edit.clone()).unwrap();
    session.append(result("c1", Outcome::Ok)).unwrap();
    assert_eq!(Session::open(&dir.0, &session.id).unwrap().events[0], edit);
}

#[test]
fn the_forge_event_round_trips() {
    let dir = Dir::new("forge-event");
    let events = [
        Event::User("Track it in the forge".into()),
        Event::Forge {
            name: "agl-009".into(),
            kind: ForgeKind::Created,
            section: None,
        },
        Event::Forge {
            name: "agl-009".into(),
            kind: ForgeKind::Appended,
            section: Some("Progress".into()),
        },
    ];
    let mut session = Session::create(&dir.0, "minimal", Path::new("/w")).unwrap();
    for event in &events {
        session.append(event.clone()).unwrap();
    }
    let text = fs::read_to_string(dir.0.join(format!("{}.jsonl", session.id))).unwrap();
    let lines: Vec<Json> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        lines[2],
        json!({"type": "forge", "name": "agl-009", "kind": "created", "time": lines[2]["time"]})
    );
    assert_eq!(
        lines[3],
        json!({
            "type": "forge",
            "name": "agl-009",
            "kind": "appended",
            "section": "Progress",
            "time": lines[3]["time"]
        })
    );
    let reopened = Session::open(&dir.0, &session.id).unwrap();
    assert_eq!(reopened.events, events);
    // The event is a marker, not part of the model's history.
    assert_eq!(
        reopened.history(),
        vec![Message::User("Track it in the forge".into())]
    );
}

#[test]
fn history_for_the_template() {
    let dir = Dir::new("history");
    let mut session = Session::create(&dir.0, "minimal", Path::new("/w")).unwrap();
    for event in [
        Event::User("u1".into()),
        Event::State("work".into()),
        Event::Thinking("t1".into()),
        call("c1", "ls"),
        result("c1", Outcome::Fail),
        Event::Answer("a1".into()),
        Event::User("u2".into()),
        Event::Thinking("dangling".into()),
        Event::Stop(StopReason::ServerError),
    ] {
        session.append(event).unwrap();
    }
    assert_eq!(
        session.history(),
        vec![
            Message::User("u1".into()),
            Message::Assistant {
                thinking: Some("t1".into()),
                text: None,
                call: Some(match &session.events[3] {
                    Event::Call { call, .. } => call.clone(),
                    _ => unreachable!(),
                }),
            },
            Message::Tool {
                id: "c1".into(),
                content: "out\n".into()
            },
            Message::Assistant {
                thinking: None,
                text: Some("a1".into()),
                call: None,
            },
            Message::User("u2".into()),
        ]
    );
}

#[test]
fn history_after_a_summary() {
    let dir = Dir::new("compact");
    let mut session = Session::create(&dir.0, "minimal", Path::new("/w")).unwrap();
    assert!(!session.compactable());
    for event in [
        Event::User("u1".into()),
        Event::State("work".into()),
        Event::Thinking("t1".into()),
        call("c1", "ls"),
        result("c1", Outcome::Ok),
    ] {
        session.append(event).unwrap();
    }
    assert!(session.compactable());
    session
        .append(Event::Compact {
            summary: "s1".into(),
            question: None,
        })
        .unwrap();
    assert!(!session.compactable());
    assert_eq!(session.history(), vec![Message::Runtime("s1".into())]);
    // A summary in the middle of a turn keeps the turn going (D140).
    assert_eq!(
        session.unfinished(),
        Some(Unfinished {
            state: Some("work".into()),
            visits: vec![("work".into(), 1)],
            steps_in_visit: 1,
        })
    );
    session.append(Event::Answer("a1".into())).unwrap();
    assert!(session.compactable());
    assert_eq!(
        session.history(),
        vec![
            Message::Runtime("s1".into()),
            Message::Assistant {
                thinking: None,
                text: Some("a1".into()),
                call: None,
            },
        ]
    );
    session
        .append(Event::Compact {
            summary: "s2".into(),
            question: None,
        })
        .unwrap();
    session.append(Event::User("u2".into())).unwrap();
    assert_eq!(
        session.history(),
        vec![Message::Runtime("s2".into()), Message::User("u2".into())]
    );
}

#[test]
fn unfinished_turns() {
    let dir = Dir::new("unfinished");
    let mut session = Session::create(&dir.0, "coder", Path::new("/w")).unwrap();
    assert_eq!(session.unfinished(), None);
    for event in [
        Event::User("u1".into()),
        Event::State("explore".into()),
        Event::Answer("a1".into()),
    ] {
        session.append(event).unwrap();
    }
    assert_eq!(session.unfinished(), None);
    for event in [
        Event::User("u2".into()),
        Event::State("explore".into()),
        call("c1", "ls"),
        result("c1", Outcome::Ok),
        Event::State("verify".into()),
        call("c2", "cargo test"),
        result("c2", Outcome::Fail),
        Event::State("explore".into()),
        call("c3", "ls"),
        result("c3", Outcome::Ok),
        call("c4", "ls"),
        result("c4", Outcome::Ok),
        // A discarded action is a step of the visit (D154).
        Event::CutOff(CutOff {
            limit: 8192,
            thinking: None,
            discarded: 8192,
            call: false,
        }),
        Event::Stop(StopReason::Interrupted),
        // The next message continues the same turn (D105).
        Event::User("u3".into()),
    ] {
        session.append(event).unwrap();
    }
    assert_eq!(
        session.unfinished(),
        Some(Unfinished {
            state: Some("explore".into()),
            visits: vec![("explore".into(), 2), ("verify".into(), 1)],
            steps_in_visit: 3,
        })
    );
    session.append(Event::Answer("a3".into())).unwrap();
    assert_eq!(session.unfinished(), None);
}

#[test]
fn opening_repairs_an_interrupted_end() {
    let dir = Dir::new("repair");
    let mut session = Session::create(&dir.0, "minimal", Path::new("/w")).unwrap();
    session.append(Event::User("u".into())).unwrap();
    session.append(call("c1", "sleep 100")).unwrap();
    let path = dir.0.join(format!("{}.jsonl", session.id));
    // A crash in the middle of a line.
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"type\":\"thin").unwrap();
    drop(file);

    let mut reopened = Session::open(&dir.0, &session.id[..8]).unwrap();
    assert_eq!(
        reopened.events.last(),
        Some(&Event::Result {
            id: "c1".into(),
            outcome: Outcome::Fail,
            content: INTERRUPTED.into(),
        })
    );
    reopened.append(Event::Answer("a".into())).unwrap();
    let again = Session::open(&dir.0, &session.id).unwrap();
    assert_eq!(again.events.len(), 4);
    assert_eq!(again.events[3], Event::Answer("a".into()));
}

#[test]
fn finding_sessions() {
    let dir = Dir::new("find");
    assert!(matches!(
        Session::open(&dir.0, "0199"),
        Err(Error::SessionNotFound { .. })
    ));
    fs::create_dir_all(&dir.0).unwrap();
    let header = "{\"type\":\"session\",\"version\":1,\"function\":\"minimal\",\"workspace\":\"/w\",\"time\":\"t\"}\n";
    fs::write(
        dir.0.join("0199aaaa-0000-7000-8000-000000000001.jsonl"),
        header,
    )
    .unwrap();
    fs::write(
        dir.0.join("0199aaab-0000-7000-8000-000000000002.jsonl"),
        header,
    )
    .unwrap();
    assert!(matches!(
        Session::open(&dir.0, "0199aaa"),
        Err(Error::SessionAmbiguous { count: 2, .. })
    ));
    assert_eq!(
        Session::open(&dir.0, "0199aaab").unwrap().id,
        "0199aaab-0000-7000-8000-000000000002"
    );
    for bad in ["", "../x", "zz"] {
        assert!(matches!(
            Session::open(&dir.0, bad),
            Err(Error::SessionNotFound { .. })
        ));
    }
}

#[test]
fn bad_files() {
    let dir = Dir::new("bad");
    fs::create_dir_all(&dir.0).unwrap();
    let write = |id: &str, text: &str| fs::write(dir.0.join(format!("{id}.jsonl")), text).unwrap();
    write(
        "aa",
        "{\"type\":\"session\",\"version\":2,\"function\":\"f\",\"workspace\":\"/w\"}\n",
    );
    write(
        "bb",
        "{\"type\":\"session\",\"version\":1,\"function\":\"f\",\"workspace\":\"/w\"}\n{\"type\":\"dance\"}\n",
    );
    write("cc", "not json\n");
    assert!(matches!(
        Session::open(&dir.0, "aa"),
        Err(Error::SessionLine { line: 1, detail, .. }) if detail.contains("version")
    ));
    assert!(matches!(
        Session::open(&dir.0, "bb"),
        Err(Error::SessionLine { line: 2, .. })
    ));
    assert!(matches!(
        Session::open(&dir.0, "cc"),
        Err(Error::SessionLine { line: 1, .. })
    ));
}

fn source(kind: Kind, bytes: &[u8], ext: &str, name: &str) -> Source {
    Source {
        kind,
        bytes: bytes.to_vec(),
        ext: ext.into(),
        name: name.into(),
    }
}

#[test]
fn the_media_of_a_session_are_kept_numbered_by_kind_and_read_back() {
    let dir = Dir::new("media");
    let mut session = Session::create(&dir.0, "minimal", Path::new("/w")).unwrap();
    assert_eq!(session.media_dir(), dir.0.join("media"));
    assert_eq!(session.next_media_number(Kind::Image), 1);
    let shot = source(Kind::Image, b"png-bytes", "png", "shot.png");
    let first = session.store_media(&shot, 1, None).unwrap();
    assert_eq!(first.sha256, media::sha256_hex(b"png-bytes"));
    assert_eq!(first.placeholder(), "[Image#1]");
    assert!(first.path(&session.media_dir()).is_file());
    let voice = source(Kind::Audio, b"wav", "wav", "voice.wav");
    session
        .store_media(&voice, 1, Some("hello".into()))
        .unwrap();
    // Numbers count by kind.
    assert_eq!(session.next_media_number(Kind::Image), 2);
    assert_eq!(session.next_media_number(Kind::Audio), 2);
    assert_eq!(session.next_media_number(Kind::Video), 1);
    // The same bytes again are one file.
    session.store_media(&shot, 2, None).unwrap();
    let files = fs::read_dir(session.media_dir()).unwrap().count();
    assert_eq!(files, 2);
    session.append(Event::User("see [Image#1]".into())).unwrap();
    // A media event is not a message.
    assert_eq!(session.history().len(), 1);
    let id = session.id.clone();
    drop(session);
    let reopened = Session::open(&dir.0, &id).unwrap();
    let media = reopened.media();
    assert_eq!(media.len(), 3);
    assert_eq!(media[0], &first);
    assert_eq!(media[1].transcript.as_deref(), Some("hello"));
    assert_eq!(media[1].kind, Kind::Audio);
    assert_eq!(reopened.next_media_number(Kind::Image), 3);
    // The line names no marker string: the marker is the server's (D6).
    let text = fs::read_to_string(dir.0.join(format!("{id}.jsonl"))).unwrap();
    assert!(text.contains("\"type\":\"media\""));
    assert!(!text.contains("__media"));
}

#[test]
fn the_frames_of_a_video_are_kept_with_their_times() {
    // The tools' ffmpeg, as the code under test runs it (AGL-040).
    let Ok(ffmpeg) = crate::programs::program("ffmpeg") else {
        return;
    };
    let dir = Dir::new("video");
    fs::create_dir_all(&dir.0).unwrap();
    let clip = dir.0.join("c.mp4");
    let made = std::process::Command::new(&ffmpeg)
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=10", "-t", "2", "-pix_fmt", "yuv420p"])
        .arg(&clip)
        .status()
        .unwrap();
    assert!(made.success());
    let mut session = Session::create(&dir.0.join("s"), "minimal", Path::new("/w")).unwrap();
    let kept = session
        .store_media(&Source::from_file(&clip).unwrap(), 1, None)
        .unwrap();
    assert_eq!(kept.kind, Kind::Video);
    assert_eq!(kept.frames.iter().map(|f| f.at).collect::<Vec<_>>(), [0.0, 1.0]);
    for frame in &kept.frames {
        assert!(media::read(&session.media_dir(), &frame.sha256, "jpg").is_ok());
    }
    let id = session.id.clone();
    drop(session);
    let reopened = Session::open(&dir.0.join("s"), &id).unwrap();
    assert_eq!(reopened.media()[0], &kept);
}

#[test]
fn a_bad_media_event_is_a_bad_line() {
    let dir = Dir::new("badmedia");
    let session = Session::create(&dir.0, "minimal", Path::new("/w")).unwrap();
    let id = session.id.clone();
    drop(session);
    let path = dir.0.join(format!("{id}.jsonl"));
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("{\"type\":\"media\",\"kind\":\"sound\",\"number\":1}\n");
    fs::write(&path, text).unwrap();
    assert!(matches!(Session::open(&dir.0, &id), Err(Error::SessionLine { .. })));
}

#[test]
fn the_memory_event_round_trips_and_is_not_part_of_the_history() {
    use crate::memory::Kind as MemoryKind;
    let dir = Dir::new("memory-event");
    let events = [
        Event::User("Remember the tests".into()),
        Event::Memory {
            name: "build-and-test".into(),
            kind: MemoryKind::Added,
            text: Some("Run cargo test.".into()),
        },
        Event::Memory {
            name: "build-and-test".into(),
            kind: MemoryKind::Edited,
            text: Some("cargo nextest".into()),
        },
        Event::Memory {
            name: "build-and-test".into(),
            kind: MemoryKind::Removed,
            text: None,
        },
    ];
    let mut session = Session::create(&dir.0, "default", Path::new("/w")).unwrap();
    for event in &events {
        session.append(event.clone()).unwrap();
    }
    let text = fs::read_to_string(dir.0.join(format!("{}.jsonl", session.id))).unwrap();
    let lines: Vec<Json> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        lines[2],
        json!({
            "type": "memory",
            "name": "build-and-test",
            "kind": "added",
            "text": "Run cargo test.",
            "time": lines[2]["time"]
        })
    );
    // A removal carries no text.
    assert_eq!(
        lines[4],
        json!({"type": "memory", "name": "build-and-test", "kind": "removed", "time": lines[4]["time"]})
    );
    let reopened = Session::open(&dir.0, &session.id).unwrap();
    assert_eq!(reopened.events, events);
    assert_eq!(reopened.history().len(), 1);
}

fn command(line: &str, failed: bool, output: Option<&str>) -> Event {
    Event::Command(CommandRun {
        line: line.into(),
        failed,
        output: output.map(str::to_owned),
    })
}

#[test]
fn a_command_is_kept_whole_and_the_model_reads_what_helps() {
    // AGL-047, D5.
    let dir = std::env::temp_dir().join(format!("agentLIBRE-commands-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut session = Session::create(&dir, "f", Path::new("/w")).unwrap();
    let events = [
        command("/help", false, Some("a long help")),
        command("/status", false, Some("the status")),
        command("/think low", false, Some("level: low")),
        command("/remember tea | Green.", false, Some("Remembered `tea`.")),
        command("/memory list", false, Some("the whole list")),
        command("/memory show tea", false, Some("# tea\n\nGreen.")),
        command("/memory remove tea", false, Some("Removed `tea`.")),
        command("/memory show gone", true, Some("no memory entry gone")),
        command("/attach gone.png", true, Some("gone.png does not exist")),
        command("/nope", true, None),
        command("/new", false, None),
        command("/quit", false, None),
    ];
    for event in events.clone() {
        session.append(event).unwrap();
    }
    let runtime: Vec<String> = session
        .history()
        .into_iter()
        .map(|message| match message {
            Message::Runtime(text) => text,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        runtime,
        [
            "the human ran: /help",
            "the human ran: /status",
            "the human ran: /think low\nlevel: low",
            "the human ran: /remember tea | Green.\nRemembered `tea`.",
            "the human ran: /memory list",
            "the human ran: /memory show tea",
            "the human ran: /memory remove tea\nRemoved `tea`.",
            "the human ran: /memory show gone\nfailed: no memory entry gone",
            "the human ran: /attach gone.png\nfailed: gone.png does not exist",
            "the human ran: /nope\nfailed",
        ]
    );
    // The file holds all of it, what the prompt leaves out too.
    let reopened = Session::open(&dir, &session.id).unwrap();
    assert_eq!(reopened.events, events);
    // A command is no reason to compact.
    assert!(!session.compactable());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_session_is_the_users_alone_and_the_latest_are_listed() {
    // AGL-058, D19, D20.
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("agl-session-latest-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut first = Session::create(&dir, "f", Path::new("/w/project")).unwrap();
    first.append(Event::User("the first one\nits second line".into())).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let mut second = Session::create(&dir, "f", Path::new("/w/project/sub")).unwrap();
    second.append(Event::User("x".repeat(100))).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let mut other = Session::create(&dir, "f", Path::new("/w/other")).unwrap();
    other.append(Event::User("another project".into())).unwrap();
    assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    let mode = fs::metadata(first.file_path()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let latest = Session::latest(&dir, Path::new("/w/project"), 20);
    assert_eq!(latest.len(), 2, "{latest:?}");
    assert_eq!(latest[0].id, second.id);
    assert_eq!(latest[0].first, format!("{}…", "x".repeat(70)));
    assert_eq!(latest[1].id, first.id);
    assert_eq!(latest[1].first, "the first one");
    assert_eq!(Session::latest(&dir, Path::new("/w/project"), 1).len(), 1);
    assert!(Session::latest(&dir, Path::new("/none"), 20).is_empty());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_name_is_a_line_of_the_file_and_the_last_one_counts() {
    // AGL-062, D4.
    let dir = Dir::new("name-line");
    let project = Path::new("/w/project");
    let mut session = Session::create(&dir.0, "f", project).unwrap();
    session.append(Event::User("fix the grammar".into())).unwrap();
    assert_eq!(session.name, None);
    session.rename(project, "  gemma grammar  ").unwrap();
    assert_eq!(session.name.as_deref(), Some("gemma grammar"));
    session.rename(project, "grammar").unwrap();
    let reopened = Session::open(&dir.0, &session.id).unwrap();
    assert_eq!(reopened.name.as_deref(), Some("grammar"));
    // The line is no event: the history is what it was.
    assert_eq!(reopened.events, session.events);
    let text = fs::read_to_string(session.file_path()).unwrap();
    assert_eq!(text.matches(r#""type":"name""#).count(), 2);
    // The same name again writes nothing.
    session.rename(project, "grammar").unwrap();
    assert_eq!(fs::read_to_string(session.file_path()).unwrap(), text);
}

#[test]
fn the_list_shows_the_name_in_place_of_the_first_line() {
    // AGL-062, D3.
    let dir = Dir::new("name-list");
    let project = Path::new("/w/project");
    let mut named = Session::create(&dir.0, "f", project).unwrap();
    named.append(Event::User("fix the grammar".into())).unwrap();
    named.rename(project, "grammar").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let mut plain = Session::create(&dir.0, "f", project).unwrap();
    plain.append(Event::User("look at the build".into())).unwrap();
    let latest = Session::latest(&dir.0, project, 20);
    assert_eq!(latest[0].id, plain.id);
    assert_eq!(latest[0].name, None);
    assert_eq!(latest[0].label(), "look at the build");
    assert_eq!(latest[1].id, named.id);
    assert_eq!(latest[1].first, "fix the grammar");
    assert_eq!(latest[1].label(), "grammar");
}

#[test]
fn a_name_is_looked_up_exactly_in_the_project_before_any_id() {
    // AGL-062, D1.
    let dir = Dir::new("name-lookup");
    let project = Path::new("/w/project");
    let mut a = Session::create(&dir.0, "f", project).unwrap();
    a.append(Event::User("a".into())).unwrap();
    // A name that is not hex, a name that is, and one with a space.
    a.rename(project, "refactor").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let mut b = Session::create(&dir.0, "f", project).unwrap();
    b.append(Event::User("b".into())).unwrap();
    b.rename(project, &a.id[..4]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let mut c = Session::create(&dir.0, "f", Path::new("/w/other")).unwrap();
    c.rename(Path::new("/w/other"), "other work").unwrap();
    assert_eq!(Session::id_named(&dir.0, project, "refactor"), Some(a.id.clone()));
    assert_eq!(Session::id_named(&dir.0, project, "Refactor"), None);
    // The name that looks like the start of an id is the name's session.
    assert_eq!(Session::id_named(&dir.0, project, &a.id[..4]), Some(b.id.clone()));
    // Another project's name is not found here.
    assert_eq!(Session::id_named(&dir.0, project, "other work"), None);
    assert_eq!(
        Session::id_named(&dir.0, Path::new("/w/other"), "other work"),
        Some(c.id.clone())
    );
}

#[test]
fn a_name_that_another_session_of_the_project_has_is_refused() {
    // AGL-062, D5.
    let dir = Dir::new("name-taken");
    let project = Path::new("/w/project");
    let mut first = Session::create(&dir.0, "f", project).unwrap();
    first.rename(project, "work").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let mut second = Session::create(&dir.0, "f", project).unwrap();
    match second.rename(project, "work") {
        Err(Error::SessionNameTaken { name, id }) => {
            assert_eq!(name, "work");
            assert_eq!(id, first.id);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(second.name, None);
    // A name the first one left is free again.
    first.rename(project, "other").unwrap();
    second.rename(project, "work").unwrap();
    // Another project may have the same name.
    let mut elsewhere = Session::create(&dir.0, "f", Path::new("/w/elsewhere")).unwrap();
    elsewhere.rename(Path::new("/w/elsewhere"), "work").unwrap();
}

#[test]
fn what_is_no_name_is_refused() {
    // AGL-062, D6.
    assert_eq!(check_name("  a name "), Ok("a name"));
    assert_eq!(check_name(&"x".repeat(NAME_MAX)), Ok("x".repeat(NAME_MAX).as_str()));
    assert_eq!(check_name(" \t "), Err(NameProblem::Empty));
    assert_eq!(check_name("two\nlines"), Err(NameProblem::Lines));
    assert_eq!(check_name(&"x".repeat(NAME_MAX + 1)), Err(NameProblem::TooLong));
    assert_eq!(check_name("last"), Err(NameProblem::Reserved));
    assert_eq!(check_name("last one"), Ok("last one"));
    let dir = Dir::new("name-refused");
    let mut session = Session::create(&dir.0, "f", Path::new("/w/project")).unwrap();
    assert!(matches!(
        session.rename(Path::new("/w/project"), "last"),
        Err(Error::SessionName { problem: NameProblem::Reserved })
    ));
    assert_eq!(session.name, None);
}

#[test]
fn text_that_looks_like_a_name_line_is_no_name() {
    // AGL-062, D8: a user message holds the words, escaped.
    let dir = Dir::new("name-quoted");
    let project = Path::new("/w/project");
    let mut session = Session::create(&dir.0, "f", project).unwrap();
    session
        .append(Event::User(r#"{"type":"name","name":"sneaky"}"#.into()))
        .unwrap();
    assert_eq!(Session::latest(&dir.0, project, 1)[0].name, None);
    assert_eq!(Session::open(&dir.0, &session.id).unwrap().name, None);
    assert_eq!(Session::id_named(&dir.0, project, "sneaky"), None);
}
