//! The requests a device may send (D9): what the first version of the apps
//! needs, and `open` only to resume a session — no new session in a
//! directory the phone names.

use agentlibre_proto::Request;

/// Whether `request` goes on to the daemon. Every request is named, so that
/// a new one is refused until it is decided here.
pub fn allowed(request: &Request) -> bool {
    match request {
        Request::Hello { .. }
        | Request::Sessions
        | Request::Subscribe { .. }
        | Request::History { .. }
        | Request::Say { .. }
        | Request::Stop { .. }
        | Request::Answer { .. }
        // A chunk of a live dictation is the REPL's: the draft is not for a
        // device (AGL-127, D5).
        | Request::Transcribe { draft: false, .. }
        | Request::Close { .. }
        // The list of the past alone, of AGL-126's requests (its D5).
        | Request::Past { .. } => true,
        Request::Open(open) => open.resume.is_some(),
        Request::Remember { .. }
        | Request::Remove { .. }
        | Request::Look { .. }
        | Request::Rename { .. }
        | Request::New { .. }
        | Request::Compact { .. }
        | Request::Record { .. }
        | Request::Transcribe { draft: true, .. }
        | Request::Flow { .. }
        | Request::Planes { .. }
        | Request::Functions { .. }
        | Request::Function { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentlibre_proto::{Attached, FunctionVerb, Look, MediaKind, Open, VERSION};

    fn audio() -> Attached {
        Attached {
            number: 1,
            kind: MediaKind::Audio,
            name: "voice.m4a".into(),
            ext: "m4a".into(),
            base64: String::new(),
        }
    }

    #[test]
    fn the_first_versions_requests_go_on() {
        let s = || "s".to_owned();
        for request in [
            Request::Hello {
                version: VERSION,
                human: true,
                lang: "en".into(),
                pid: 0,
            },
            Request::Sessions,
            Request::Subscribe { session: None },
            Request::Subscribe { session: Some(s()) },
            Request::History { session: s() },
            Request::Say {
                session: s(),
                message: "hi".into(),
                attached: vec![audio()],
                reasoning: None,
            },
            Request::Stop { session: s() },
            Request::Answer {
                question: 1,
                answer: Some("y".into()),
            },
            Request::Transcribe {
                session: s(),
                audio: audio(),
                draft: false,
            },
            Request::Close { session: s() },
            Request::Past {
                dir: None,
                plane: None,
                limit: 20,
            },
            Request::Open(Open {
                dir: "/w".into(),
                resume: Some("last".into()),
                ..Open::default()
            }),
        ] {
            assert!(allowed(&request), "{request:?}");
        }
    }

    #[test]
    fn a_chunk_of_a_draft_is_not_for_a_device() {
        // AGL-127, D5: the final pass goes on, the draft does not.
        let transcribe = |draft| Request::Transcribe {
            session: "s".into(),
            audio: audio(),
            draft,
        };
        assert!(allowed(&transcribe(false)));
        assert!(!allowed(&transcribe(true)));
    }

    #[test]
    fn a_new_session_and_the_memory_are_refused() {
        let s = || "s".to_owned();
        for request in [
            Request::Open(Open {
                dir: "/etc".into(),
                ..Open::default()
            }),
            Request::Remember {
                session: s(),
                name: "n".into(),
                text: "t".into(),
                workspace: false,
            },
            Request::Remove {
                session: s(),
                name: "n".into(),
            },
            Request::Look {
                session: s(),
                look: Look::List,
            },
            Request::Rename {
                session: s(),
                name: "n".into(),
            },
            Request::New { session: s() },
            Request::Compact {
                session: s(),
                reasoning: None,
            },
            Request::Record {
                session: s(),
                lines: Vec::new(),
            },
            Request::Planes {
                dir: "/w".into(),
                name: None,
            },
            Request::Functions {
                dir: "/w".into(),
                plane: None,
            },
            Request::Function {
                dir: "/w".into(),
                plane: None,
                verb: FunctionVerb::Delete { name: "f".into() },
            },
        ] {
            assert!(!allowed(&request), "{request:?}");
        }
    }
}
