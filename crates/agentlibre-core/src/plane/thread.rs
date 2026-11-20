//! A plane on a thread of its own (AGL-075, D7): a surface holds a [`Handle`]
//! that sends the plane its [`Command`]s and takes what it [`Sent`] by a
//! channel, the shape of a socket. A command sent while a turn runs waits
//! for the turn's end. A turn is stopped by the plane's stop, which the handle
//! holds too.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use super::{Plane, Said, Turn};
use crate::agent::{Report, Sink, TurnEnd};
use crate::attach::{self, Pending};
use crate::backend::ReasoningLevel;
use crate::error::Error;
use crate::memory::MemoryAction;
use crate::phase::Observer;
use crate::session::{Event, Session};
use crate::stop::Stop;

/// What a surface asks of its plane.
pub enum Command {
    /// A message and the media it names, at the reasoning level of the
    /// surface (AGL-013). `observer` saw the server's start of a run of one
    /// request, whose context is read (AGL-046, D10); the turn goes on from
    /// there.
    Say {
        message: String,
        attached: Vec<Pending>,
        reasoning: Option<ReasoningLevel>,
        observer: Option<Observer>,
    },
    /// `/remember [--workspace] name | text` (AGL-006; AGL-077, D8).
    Remember { name: String, text: String, workspace: bool },
    /// `/memory remove <name>` once agreed (AGL-060, D7).
    Remove { name: String },
    /// `/memory list`, `/memory show` (AGL-060, D5).
    LookAt(MemoryAction),
    /// `/rename <name>` (AGL-062, D7).
    Rename(String),
    /// `/new`: a new session of the plane.
    New,
    /// `/compact`, at the reasoning level of the surface.
    Compact { reasoning: Option<ReasoningLevel> },
    /// Lines of the session's record a surface writes: its commands
    /// (AGL-047, D5).
    Append(Vec<Event>),
    /// The session's events, its past (AGL-073, D10), sent to the channel.
    History(std::sync::mpsc::Sender<Vec<Event>>),
    /// The server comes up as the session starts (AGL-046), as a turn: its
    /// lines and its phases are sent, then the context as `Context`; a
    /// server that fails ends the command with its error in `said`.
    Start,
    /// A verb of a flow of the plane (AGL-118, D2, D7): what the human's CLI
    /// and the agent's tool ask, answered on `reply` with what the flow's
    /// place says — the one-shot client's reply, not a turn's `Done`.
    Flow {
        verb: crate::plane::flow::Verb,
        reply: std::sync::mpsc::Sender<Said>,
    },
}

/// What a plane sends while it takes a command, and at its end.
pub enum Sent {
    /// What the turn tells (AGL-067, D3).
    Report(Report),
    /// A line of the server's start or of the transcription.
    Line(String),
    /// The server answered, with a context of this many tokens.
    Context(usize),
    /// The media of the message are stored: the session's media of each
    /// kind (`attach::media_counts`).
    Stored([u32; 3]),
    /// The turn is over, before its FSM is closed.
    Over,
    /// The command is done.
    Done(Box<Done>),
}

impl Sent {
    /// Gives what the plane sent to the surface of the turn; a command's end
    /// is not for it.
    pub fn give(self, surface: &mut dyn Turn) {
        match self {
            Sent::Report(report) => surface.send(report),
            Sent::Line(line) => surface.line(&line),
            Sent::Context(context) => surface.context(context),
            Sent::Stored(media) => surface.stored(media),
            Sent::Over => surface.over(),
            Sent::Done(_) => {}
        }
    }
}

/// The end of a command.
pub struct Done {
    /// A message's turn.
    pub turn: Option<Result<TurnEnd, Error>>,
    /// Why a turn stopped, in the words of its Function (`i18n::repl::stopped`).
    pub why: Option<String>,
    /// What a slash command said; `/new` fails as an error.
    pub said: Option<Result<Said, Error>>,
    /// The session as it is after the command.
    pub session: SessionView,
}

/// What a surface holds of its plane's session after a command.
#[derive(Clone, Debug, Default)]
pub struct SessionView {
    pub id: String,
    pub name: Option<String>,
    pub events: Vec<Event>,
    pub outputs: PathBuf,
    /// The state of an unfinished turn (D105).
    pub unfinished: Option<String>,
    /// The media of each kind (`attach::media_counts`).
    pub media: [u32; 3],
}

impl SessionView {
    pub fn of(session: &Session) -> SessionView {
        SessionView {
            id: session.id.clone(),
            name: session.name.clone(),
            events: session.events.clone(),
            outputs: session.outputs_dir(),
            unfinished: session.unfinished().and_then(|turn| turn.state),
            media: attach::media_counts(session),
        }
    }
}

/// The surface's end of a plane on its thread. Dropping it ends the thread
/// once its command is done.
pub struct Handle {
    commands: Option<Sender<Command>>,
    sent: Option<Receiver<Sent>>,
    stop: Stop,
    thread: Option<JoinHandle<()>>,
}

/// A plane's handle taken apart (AGL-076): its commands, what it sends and
/// its thread, for a daemon that sends from one thread and listens on
/// another. Dropping the commands ends the thread once its command is done.
pub struct Parts {
    pub commands: Sender<Command>,
    pub sent: Receiver<Sent>,
    pub stop: Stop,
    pub thread: JoinHandle<()>,
}

impl Handle {
    /// The stop of the plane's turns.
    pub fn stop(&self) -> &Stop {
        &self.stop
    }

    /// Sends a command; false when the plane's thread is gone.
    pub fn send(&self, command: Command) -> bool {
        self.commands
            .as_ref()
            .is_some_and(|commands| commands.send(command).is_ok())
    }

    /// What the plane sent next; `None` when its thread is gone.
    pub fn recv(&self) -> Option<Sent> {
        self.sent.as_ref()?.recv().ok()
    }

    /// The handle taken apart.
    pub fn into_parts(mut self) -> Parts {
        Parts {
            commands: self.commands.take().expect("a handle's commands"),
            sent: self.sent.take().expect("a handle's receiver"),
            stop: self.stop.clone(),
            thread: self.thread.take().expect("a handle's thread"),
        }
    }

    /// Sends `command` and waits for its end; what the plane sends before it
    /// goes to `each`. `None` when the plane's thread is gone.
    pub fn ask(&self, command: Command, each: &mut dyn FnMut(Sent)) -> Option<Done> {
        if !self.send(command) {
            return None;
        }
        loop {
            match self.recv()? {
                Sent::Done(done) => return Some(*done),
                sent => each(sent),
            }
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.commands.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Plane {
    /// The server's start of a session (AGL-046): what the REPL did as it
    /// started, on the plane's thread.
    fn start_server(&mut self, forward: &mut Forward) -> Result<(), Error> {
        use crate::engine::ServerProgress;
        use crate::phase::{Observer, PhaseEvent};
        let mut observer = Observer::new();
        observer.attach_between_turns(&self.session);
        let ready = self
            .autostart
            .ensure(&mut |progress| match progress {
                ServerProgress::Starting => observer.enter(PhaseEvent::ServerStart, &mut *forward),
                ServerProgress::Line(line) => {
                    // AGL-103, D2: the server's own line stands in the record.
                    let _ = self
                        .session
                        .append(Event::System { text: line.to_owned(), failed: false });
                    forward.line(line);
                }
            })
            .and_then(|()| self.backend.context_size());
        match ready {
            Ok(size) => {
                observer.end(PhaseEvent::Idle, &mut *forward);
                self.context = Some(size);
                forward.context(size);
                Ok(())
            }
            Err(error) => {
                observer.end(PhaseEvent::Failed, &mut *forward);
                Err(error)
            }
        }
    }

    /// Moves the plane to a thread of its own (D7).
    pub fn start(self) -> Handle {
        let (commands, inbox) = mpsc::channel();
        let (outbox, sent) = mpsc::channel();
        let stop = self.stop.clone();
        let thread = thread::Builder::new()
            .name(format!("run {}", self.session.id))
            .spawn(move || self.serve(inbox, outbox))
            .expect("a thread for the run");
        Handle {
            commands: Some(commands),
            sent: Some(sent),
            stop,
            thread: Some(thread),
        }
    }

    fn serve(mut self, inbox: Receiver<Command>, outbox: Sender<Sent>) {
        for command in inbox {
            let done = self.take(command, &outbox);
            if outbox.send(Sent::Done(Box::new(done))).is_err() {
                break;
            }
        }
    }

    fn take(&mut self, command: Command, outbox: &Sender<Sent>) -> Done {
        let mut forward = Forward(outbox.clone());
        let (turn, said) = match command {
            Command::Say {
                message,
                attached,
                reasoning,
                observer,
            } => {
                // A stop asked before this turn is forgotten: it stopped
                // the turn it was for (AGL-096, D1).
                self.stop.reset();
                self.reasoning = reasoning;
                let end = match (observer, self.context) {
                    (Some(mut observer), Some(context)) => {
                        self.turn_on(context, &message, attached, &mut observer, &mut forward)
                    }
                    _ => self.turn(&message, attached, &mut forward),
                };
                (Some(end), None)
            }
            Command::Remember {
                name,
                text,
                workspace,
            } => (None, Some(Ok(self.remember(&name, &text, workspace)))),
            Command::Remove { name } => (None, Some(Ok(self.remove(&name)))),
            Command::LookAt(action) => (None, Some(Ok(self.looked_at(&action)))),
            Command::Rename(name) => (None, Some(Ok(self.rename(&name)))),
            Command::New => (None, Some(self.new_session())),
            Command::Compact { reasoning } => {
                self.stop.reset();
                self.reasoning = reasoning;
                (None, Some(Ok(self.compact(&mut forward))))
            }
            Command::Append(events) => {
                for event in events {
                    let _ = self.session.append(event);
                }
                (None, None)
            }
            Command::History(to) => {
                let _ = to.send(self.session.events.clone());
                (None, None)
            }
            Command::Flow { verb, reply } => {
                let said = self.flow(verb);
                let _ = reply.send(said);
                (None, None)
            }
            Command::Start => {
                let said = self.start_server(&mut forward).err().map(|error| {
                    // AGL-103, D2: a start that failed stands in the record, red.
                    let _ = self.session.append(Event::System {
                        text: error.message(self.lang),
                        failed: true,
                    });
                    Err(error)
                });
                (None, said)
            }
        };
        let why = match &turn {
            Some(Ok(TurnEnd::Stopped(reason))) => {
                Some(crate::i18n::repl::stopped(*reason, &self.function, self.lang))
            }
            _ => None,
        };
        Done {
            turn,
            why,
            said,
            session: SessionView::of(&self.session),
        }
    }
}

/// The surface of a turn on the plane's thread: what it is told goes to the
/// handle.
struct Forward(Sender<Sent>);

impl Sink for Forward {
    fn send(&mut self, report: Report) {
        let _ = self.0.send(Sent::Report(report));
    }
}

impl Turn for Forward {
    fn line(&mut self, line: &str) {
        let _ = self.0.send(Sent::Line(line.to_owned()));
    }

    fn context(&mut self, context: usize) {
        let _ = self.0.send(Sent::Context(context));
    }

    fn stored(&mut self, media: [u32; 3]) {
        let _ = self.0.send(Sent::Stored(media));
    }

    fn over(&mut self) {
        let _ = self.0.send(Sent::Over);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::backend::{Backend, Generated, Prompt, Request};
    use crate::engine::{Autostart, Others};
    use crate::forge::testutil::TempDir;
    use crate::function::Function;
    use crate::i18n::Lang;
    use crate::memory::StoreHome;
    use crate::search::NoSearch;
    use crate::session::StopReason;

    /// A server that thinks, then answers; while `hold` is set it writes
    /// until its turn is stopped.
    #[derive(Default)]
    struct Fake {
        hold: AtomicBool,
        calls: AtomicUsize,
    }

    impl Backend for Fake {
        fn render(&self, _prompt: &Prompt) -> Result<String, Error> {
            Ok("prompt".into())
        }

        fn generate(
            &self,
            _request: &Request,
            _on_thinking: &mut dyn FnMut(&str),
            _on_text: &mut dyn FnMut(&str),
        ) -> Result<Generated, Error> {
            // As the servers' streams do: the stop is asked before every read.
            loop {
                if crate::stop::requested() {
                    return Err(Error::Interrupted);
                }
                if !self.hold.load(Ordering::SeqCst) {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Generated {
                text: if call.is_multiple_of(2) { "Let me answer." } else { "Done." }.into(),
                stop: crate::backend::Stop::End,
                thinking: None,
                tool_calls: Vec::new(),
                prompt_tokens: None,
                completion_tokens: None,
                prefill: None,
            })
        }

        fn count_tokens(&self, text: &str) -> Result<usize, Error> {
            Ok(text.len())
        }

        fn context_size(&self) -> Result<usize, Error> {
            Ok(16384)
        }

        fn health(&self) -> Option<u16> {
            Some(200)
        }

        fn url(&self) -> &str {
            "http://127.0.0.1:1"
        }
    }

    /// A plane on `fake`, its session in `dir`, its server answering.
    fn plane(dir: &TempDir, fake: Arc<Fake>) -> Plane {
        let cwd = dir.0.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let (project, _) = crate::forge::resolve(&dir.0.join("data"), &cwd, Lang::En, true, &|_| {
            Err(Error::Interrupted)
        })
        .unwrap();
        let function =
            Function::load(&[Path::new("/none"), Path::new("/none")], "fixture-minimal").unwrap();
        let sessions = dir.0.join("sessions");
        let session =
            Session::create_run(&sessions, "fixture-minimal", &cwd, Some(&function.snapshot()))
                .unwrap();
        let backend: Arc<dyn Backend + Send + Sync> = fake;
        Plane {
            file: None,
            plane_forge: None,
            function,
            function_name: "fixture-minimal".into(),
            backend: Arc::clone(&backend),
            session,
            sessions_dir: sessions,
            project: project.project.clone(),
            workspace: cwd.clone(),
            context: Some(16384),
            autostart: Autostart {
                llama: backend,
                project,
                effective: Default::default(),
                whisper: Default::default(),
                user_path: dir.0.join("settings.nix"),
                models: dir.0.join("models"),
                path_var: None,
                runtime: None,
                lang: Lang::En,
                others: Others::Refuse,
                engine: Arc::new(Mutex::new(None)),
            },
            lang: Lang::En,
            data: dir.0.join("data"),
            auto_rehome: true,
            reasoning: None,
            image_tokens: 1000,
            search: Box::new(NoSearch),
            memory: Arc::new(StoreHome {
                workspace: cwd,
                auto_rehome: true,
                lang: Lang::En,
            }),
            about: None,
            stop: Stop::new(),
        }
    }

    fn say(message: &str) -> Command {
        Command::Say {
            message: message.into(),
            attached: Vec::new(),
            reasoning: None,
            observer: None,
        }
    }

    #[test]
    fn a_message_is_answered_on_the_planes_thread() {
        let dir = TempDir::new();
        let handle = plane(&dir, Arc::default()).start();
        let mut reports = 0;
        let done = handle
            .ask(say("Hello"), &mut |sent| {
                if matches!(sent, Sent::Report(_)) {
                    reports += 1;
                }
            })
            .unwrap();
        assert!(matches!(done.turn, Some(Ok(TurnEnd::Answer(ref text))) if text == "Done."));
        assert!(reports > 0);
        assert!(done.session.events.iter().any(|e| matches!(e, Event::User(text) if text == "Hello")));
    }

    #[test]
    fn each_plane_is_stopped_by_its_own_stop() {
        let dir = TempDir::new();
        let (held, free) = (Arc::new(Fake::default()), Arc::new(Fake::default()));
        held.hold.store(true, Ordering::SeqCst);
        free.hold.store(true, Ordering::SeqCst);
        let (first, second) = (plane(&dir, held).start(), plane(&dir, free.clone()).start());
        assert!(first.send(say("one")) && second.send(say("two")));
        thread::sleep(Duration::from_millis(50));
        first.stop().request();
        let first_end = loop {
            if let Sent::Done(done) = first.recv().unwrap() {
                break done;
            }
        };
        assert!(matches!(
            first_end.turn,
            Some(Ok(TurnEnd::Stopped(StopReason::Interrupted)))
        ));
        // The other turn goes on: it ends when its server lets it.
        assert!(!second.stop().requested());
        free.hold.store(false, Ordering::SeqCst);
        let second_end = loop {
            if let Sent::Done(done) = second.recv().unwrap() {
                break done;
            }
        };
        assert!(matches!(second_end.turn, Some(Ok(TurnEnd::Answer(_)))));
    }

    #[test]
    fn a_stop_ends_its_turn_and_not_the_next() {
        let dir = TempDir::new();
        let fake = Arc::new(Fake::default());
        fake.hold.store(true, Ordering::SeqCst);
        let handle = plane(&dir, fake.clone()).start();
        assert!(handle.send(say("one")));
        thread::sleep(Duration::from_millis(50));
        handle.stop().request();
        let first = loop {
            if let Sent::Done(done) = handle.recv().unwrap() {
                break done;
            }
        };
        assert!(matches!(
            first.turn,
            Some(Ok(TurnEnd::Stopped(StopReason::Interrupted)))
        ));
        fake.hold.store(false, Ordering::SeqCst);
        let second = handle.ask(say("two"), &mut |_| {}).unwrap();
        assert!(matches!(second.turn, Some(Ok(TurnEnd::Answer(_)))));
    }

    #[test]
    fn a_message_sent_while_a_turn_runs_waits_for_its_end() {
        let dir = TempDir::new();
        let fake = Arc::new(Fake::default());
        fake.hold.store(true, Ordering::SeqCst);
        let handle = plane(&dir, fake.clone()).start();
        assert!(handle.send(say("first")) && handle.send(say("second")));
        let release = {
            let fake = fake.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                fake.hold.store(false, Ordering::SeqCst);
            })
        };
        let started = Instant::now();
        let mut ends = Vec::new();
        while ends.len() < 2 {
            if let Sent::Done(done) = handle.recv().unwrap() {
                let users: Vec<String> = done
                    .session
                    .events
                    .iter()
                    .filter_map(|e| match e {
                        Event::User(text) => Some(text.clone()),
                        _ => None,
                    })
                    .collect();
                ends.push(users);
            }
        }
        release.join().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(40));
        assert_eq!(ends, [vec!["first".to_owned()], vec!["first".into(), "second".into()]]);
    }
}
