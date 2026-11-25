use super::*;

/// `message` written as a line and read back.
fn round<T: Serialize + for<'de> Deserialize<'de> + std::fmt::Debug>(message: &T) -> (String, T) {
    let mut out = Vec::new();
    send(&mut out, message).unwrap();
    let line = String::from_utf8(out.clone()).unwrap();
    assert!(line.ends_with('\n') && line.matches('\n').count() == 1, "{line}");
    let back = match Lines::new(&out[..]).read().unwrap() {
        Read::Message(message) => message,
        other => panic!("no message: {other:?}"),
    };
    (line, back)
}

#[test]
fn every_request_round_trips_as_one_line() {
    let requests = [
        Request::Hello {
            version: VERSION,
            human: true,
            lang: "uk".into(),
            pid: 42,
        },
        Request::Open(Open {
            dir: "/w".into(),
            plane: Some("work".into()),
            one_request: true,
            ..Open::default()
        }),
        Request::Sessions,
        Request::Subscribe { session: None },
        Request::Say {
            session: "s".into(),
            message: "hi\nthere".into(),
            attached: vec![Attached {
                number: 1,
                kind: MediaKind::Image,
                name: "a.png".into(),
                ext: "png".into(),
                base64: base64::encode(b"\x89PNG"),
            }],
            reasoning: Some("low".into()),
        },
        Request::Stop { session: "s".into() },
        Request::Remember {
            session: "s".into(),
            name: "n".into(),
            text: "t".into(),
            workspace: true,
        },
        Request::Remove {
            session: "s".into(),
            name: "n".into(),
        },
        Request::Look {
            session: "s".into(),
            look: Look::Show { name: "n".into() },
        },
        Request::Rename {
            session: "s".into(),
            name: "n".into(),
        },
        Request::New { session: "s".into() },
        Request::Compact {
            session: "s".into(),
            reasoning: None,
        },
        Request::Record {
            session: "s".into(),
            lines: vec![CommandLine {
                line: "/status".into(),
                failed: false,
                output: Some("…".into()),
            }],
        },
        Request::Transcribe {
            dir: "/w".into(),
            plane: None,
            audio: Attached {
                number: 1,
                kind: MediaKind::Audio,
                name: "voice.wav".into(),
                ext: "wav".into(),
                base64: String::new(),
            },
        },
        Request::Answer {
            question: 3,
            answer: Some("y".into()),
        },
        Request::Close { session: "s".into() },
        Request::History { session: "s".into() },
        Request::Flow {
            plane: "work".into(),
            verb: FlowVerb::Start { name: "build".into() },
        },
        Request::Flow {
            plane: "work".into(),
            verb: FlowVerb::List,
        },
        Request::Past {
            dir: Some("/w".into()),
            plane: None,
            limit: 20,
        },
        Request::Planes {
            dir: "/w".into(),
            name: Some("work".into()),
        },
        Request::Functions {
            dir: "/w".into(),
            plane: None,
        },
        Request::Function {
            dir: "/w".into(),
            plane: Some("work".into()),
            verb: FunctionVerb::New {
                name: "mine".into(),
                from: "stock".into(),
            },
        },
    ];
    for request in requests {
        let (line, back) = round(&request);
        assert_eq!(back, request, "{line}");
    }
    let (line, _) = round(&Request::Stop { session: "s".into() });
    assert_eq!(line, "{\"request\":\"stop\",\"session\":\"s\"}\n");
}

#[test]
fn what_a_plane_sends_round_trips() {
    let call = Event::Call {
        text: Some("let me look".into()),
        summary: Some("ls".into()),
        call: ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: vec![("command".into(), Value::Text("ls".into())), ("timeout".into(), Value::Int(5))],
        },
    };
    let replies = [
        Reply::Hello { version: VERSION, pid: 7 },
        Reply::Refused { error: "no".into() },
        Reply::Ok,
        Reply::Opened(Opened::default()),
        Reply::Past {
            sessions: vec![PastSession {
                id: "s".into(),
                first: "hi".into(),
                plane: Some("work".into()),
                ..PastSession::default()
            }],
        },
        Reply::Planes { planes: vec!["work".into()] },
        Reply::Functions {
            functions: vec![FunctionEntry {
                name: "mine".into(),
                source: FunctionSource::Workspace,
                shadows: vec![FunctionSource::BuiltIn],
                kind: "llama".into(),
                model: Some("m.gguf".into()),
                description: None,
                error: None,
                current: true,
            }],
        },
        Reply::Function(FunctionView {
            name: "mine".into(),
            path: "/w/mine.nix".into(),
            text: None,
            load: Some(FunctionLoad {
                kind: "llama".into(),
                model: None,
                context: Some(4096),
            }),
        }),
        Reply::History {
            session: "s".into(),
            events: vec![
                Event::User("hi".into()),
                Event::Answer("hello".into()),
                Event::System { text: "the server is ready".into(), failed: true },
            ],
        },
        Reply::Sent {
            session: "s".into(),
            sent: Sent::Report(Report::Event(call)),
        },
        Reply::Sent {
            session: "s".into(),
            sent: Sent::Report(Report::Transition(Transition {
                from: Phase::Running("bash".into()),
                to: Phase::Retry { attempt: 1, max: 3 },
                event: PhaseEvent::ToolEnd(Outcome::Timeout),
                micros: 1500,
                violation: false,
            })),
        },
        Reply::Sent {
            session: "s".into(),
            sent: Sent::Stored([1, 0, 2]),
        },
        Reply::Done {
            session: "s".into(),
            done: Done {
                turn: Some(Ok(TurnEnd::Stopped(StopReason::Interrupted))),
                why: Some("interrupted".into()),
                said: Some(Err("no store".into())),
                session: SessionView::default(),
            },
            yours: true,
        },
        Reply::Question {
            question: 1,
            session: "s".into(),
            ask: Ask::Yes,
            text: "restart? [y/N]".into(),
        },
        Reply::Text {
            plane: "work".into(),
            said: Text {
                lines: vec![Told::Output("build: done".into())],
                failed: false,
                ..Text::default()
            },
        },
    ];
    for reply in replies {
        let (line, back) = round(&reply);
        assert_eq!(back, reply, "{line}");
    }
}

#[test]
fn a_line_that_is_no_message_is_invalid_data() {
    let mut lines = Lines::new(&b"{\"request\":\"nope\"}\n"[..]);
    let error = lines.read::<Request>().unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(matches!(
        Lines::new(&b""[..]).read::<Request>().unwrap(),
        Read::End
    ));
}

#[test]
fn a_line_over_the_limit_is_read_to_its_newline_and_dropped() {
    // AGL-121, D27, D54: the daemon's limit; the line goes, the
    // connection stays, and its next line still reads.
    let data = b"{\"request\":\"sessions\"}\n{\"request\":\"nope\"}xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n{\"request\":\"sessions\"}\n";
    let mut lines = Lines::with_limit(&data[..], 32);
    assert!(matches!(lines.read::<Request>().unwrap(), Read::Message(_)));
    assert!(matches!(
        lines.read::<Request>().unwrap(),
        Read::TooLong
    ));
    assert!(matches!(lines.read::<Request>().unwrap(), Read::Message(_)));
    assert!(matches!(lines.read::<Request>().unwrap(), Read::End));
}

#[test]
fn a_limit_of_none_reads_what_it_likes() {
    let line = format!("{}\n", "x".repeat(1000));
    let mut lines = Lines::new(line.as_bytes());
    assert!(lines.read::<Request>().is_err());
}

#[test]
fn a_line_cut_by_the_limit_but_shorter_still_counts() {
    // 16 bytes with no newline in the first room of 9: over the limit,
    // dropped whole, and the stream after it is clean.
    let data = b"0123456789abcdef\n";
    let mut lines = Lines::with_limit(&data[..], 8);
    assert!(matches!(
        lines.read::<Request>().unwrap(),
        Read::TooLong
    ));
    assert!(matches!(lines.read::<Request>().unwrap(), Read::End));
}

#[test]
fn base64_goes_both_ways() {
    for bytes in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar", &[0, 255, 128, 7]] {
        assert_eq!(base64::decode(&base64::encode(bytes)).unwrap(), bytes);
    }
    assert_eq!(base64::encode(b"foobar"), "Zm9vYmFy");
    assert_eq!(base64::encode(b"fo"), "Zm8=");
    assert!(base64::decode("Zm8").is_none());
    assert!(base64::decode("Zm=8").is_none());
}
