use super::*;

/// A fresh directory, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let dir =
            std::env::temp_dir().join(format!("agl-lite-session-{name}-{}", std::process::id()));
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
    assert_eq!(lines.len(), 8);
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[0]["version"], 1);
    assert_eq!(lines[0]["workspace"], "/home/u/repos/a");
    assert_eq!(lines[4]["args"], json!({"command": "ls", "timeout": 30}));
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
    let mut session = Session::create(&dir.0, "default", Path::new("/w")).unwrap();
    session.append(edit.clone()).unwrap();
    session.append(result("c1", Outcome::Ok)).unwrap();
    assert_eq!(Session::open(&dir.0, &session.id).unwrap().events[0], edit);
}

#[test]
fn history_for_the_template() {
    let dir = Dir::new("history");
    let mut session = Session::create(&dir.0, "default", Path::new("/w")).unwrap();
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
            steps_in_visit: 2,
        })
    );
    session.append(Event::Answer("a3".into())).unwrap();
    assert_eq!(session.unfinished(), None);
}

#[test]
fn opening_repairs_an_interrupted_end() {
    let dir = Dir::new("repair");
    let mut session = Session::create(&dir.0, "default", Path::new("/w")).unwrap();
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
    let header = "{\"type\":\"session\",\"version\":1,\"function\":\"default\",\"workspace\":\"/w\",\"time\":\"t\"}\n";
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
