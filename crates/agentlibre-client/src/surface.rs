//! What a surface gives a session it holds (AGL-130, D7): where the reports
//! of a turn go, and the few things it shows of the turn's start. It takes
//! the place, in a client, of the core's `Sink` and `Turn`, which are the
//! run's; the types are the protocol's.

use agentlibre_proto as proto;

/// A surface: the REPL, the stdin session, `-p`.
pub trait Surface {
    /// What the turn tells.
    fn report(&mut self, report: proto::Report);
    /// A line the start of the server or the transcription says.
    fn line(&mut self, _line: &str) {}
    /// The server answered, with a context of `context` tokens.
    fn context(&mut self, _context: usize) {}
    /// The media the message names are stored: the session's media of each
    /// kind (image, video, audio).
    fn stored(&mut self, _media: [u32; 3]) {}
    /// The turn is over, before its FSM is closed.
    fn over(&mut self) {}
}

/// Gives what the session sent to the surface of the turn.
pub fn give(sent: proto::Sent, surface: &mut dyn Surface) {
    match sent {
        proto::Sent::Report(report) => surface.report(report),
        proto::Sent::Line(line) => surface.line(&line),
        proto::Sent::Context(context) => surface.context(context),
        proto::Sent::Stored(media) => surface.stored(media),
        proto::Sent::Over => surface.over(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What reached the surface, as one word and its value.
    #[derive(Default)]
    struct Log(Vec<String>);

    impl Surface for Log {
        fn report(&mut self, report: proto::Report) {
            self.0.push(format!("report {report:?}"));
        }
        fn line(&mut self, line: &str) {
            self.0.push(format!("line {line}"));
        }
        fn context(&mut self, context: usize) {
            self.0.push(format!("context {context}"));
        }
        fn stored(&mut self, media: [u32; 3]) {
            self.0.push(format!("stored {media:?}"));
        }
        fn over(&mut self) {
            self.0.push("over".into());
        }
    }

    #[test]
    fn each_sent_reaches_its_method_once() {
        let mut log = Log::default();
        let sent = [
            proto::Sent::Report(proto::Report::Prefill),
            proto::Sent::Line("up".into()),
            proto::Sent::Context(4096),
            proto::Sent::Stored([1, 0, 2]),
            proto::Sent::Over,
        ];
        for sent in sent {
            give(sent, &mut log);
        }
        assert_eq!(
            log.0,
            ["report Prefill", "line up", "context 4096", "stored [1, 0, 2]", "over"]
        );
    }

    /// A surface that takes only reports ignores the rest.
    #[test]
    fn a_surface_with_the_defaults_takes_the_reports_alone() {
        struct Reports(usize);
        impl Surface for Reports {
            fn report(&mut self, _: proto::Report) {
                self.0 += 1;
            }
        }
        let mut surface = Reports(0);
        give(proto::Sent::Line("x".into()), &mut surface);
        give(proto::Sent::Context(1), &mut surface);
        give(proto::Sent::Stored([0; 3]), &mut surface);
        give(proto::Sent::Over, &mut surface);
        give(proto::Sent::Report(proto::Report::Compacting), &mut surface);
        assert_eq!(surface.0, 1);
    }
}
