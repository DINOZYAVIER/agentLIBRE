//! The plane a Function runs on (AGL-067, F1, D10): what a session holds
//! beside its Function — the backend, the session, the memory, the searcher,
//! the engine's start. It outlives a session: `/new` and `/function` start a
//! new one on it. The surfaces — `-p`, the stdin session, the REPL — read
//! their input and draw what comes out; the plane takes a turn.

use std::path::{Path, PathBuf};

use crate::agent::{Agent, Sink, TurnEnd};
use crate::attach::{self, Pending, Transcriber};
use crate::backend::{Backend, ReasoningLevel};
use crate::engine::{Autostart, ServerProgress};
use crate::error::Error;
use crate::function::Function;
use crate::i18n::Lang;
use crate::memory::Home;
use crate::phase::{Observer, PhaseEvent};
use crate::search::Searcher;
use crate::session::Session;
use crate::tools::ForgeEnv;

/// What a surface gives a turn on the plane (AGL-067): where the reports go,
/// and the few things it shows of the turn's start.
pub trait Turn: Sink {
    /// A line the start of the server or the transcription says (AGL-015,
    /// AGL-043 F2).
    fn line(&mut self, line: &str);
    /// The server answered, with a context of `context` tokens.
    fn context(&mut self, _context: usize) {}
    /// The media the message names are stored in `session` (AGL-014).
    fn stored(&mut self, _session: &Session) {}
    /// The turn is over, before the FSM is closed.
    fn over(&mut self) {}
}

pub struct Plane<'a> {
    pub function: &'a Function,
    pub function_name: &'a str,
    pub backend: &'a dyn Backend,
    pub session: Session,
    /// The directory of the sessions of the user, for a new one.
    pub sessions_dir: &'a Path,
    /// The project of the session: a name is unique in it (AGL-062, D5).
    pub project: &'a Path,
    /// The workspace of the session.
    pub workspace: PathBuf,
    /// The context size of the server when it already answered at the
    /// start; read at the first request otherwise (AGL-015, D10).
    pub context: Option<usize>,
    /// Brings the server up when a request finds none (AGL-015).
    pub autostart: &'a Autostart<'a>,
    pub lang: Lang,
    /// What the agent's `forge` tool takes from the session (AGL-008).
    pub forge: ForgeEnv<'a>,
    /// The model's reasoning level (AGL-013); the REPL's `/think` changes it
    /// for the session.
    pub reasoning: Option<ReasoningLevel>,
    /// The most tokens one picture takes (AGL-014, D10).
    pub image_tokens: usize,
    /// Runs the `search` tool (AGL-019).
    pub search: &'a dyn Searcher,
    /// Where the memory of the workspace is (AGL-006).
    pub memory: &'a dyn Home,
    /// What the model is told of its environment and of this agl (AGL-048).
    pub about: Option<&'a str>,
}

impl Plane<'_> {
    /// The agent of a turn on this plane, for a server with a context of
    /// `context` tokens: the one place an [`Agent`] is built.
    pub fn agent(&mut self, context: usize) -> Agent<'_> {
        Agent {
            function: self.function,
            function_name: self.function_name,
            backend: self.backend,
            session: &mut self.session,
            workspace: &self.workspace,
            context,
            forge: self.forge,
            reasoning: self.reasoning,
            image_tokens: self.image_tokens,
            whisper: self.autostart as &dyn Transcriber,
            search: self.search,
            memory: self.memory,
            about: self.about,
        }
    }

    /// A turn for the message `message` and its `attached` media: the server
    /// is brought up when it does not answer, at the first request (AGL-015,
    /// D10, AGL-024), and its context size read; then [`Plane::turn_on`].
    pub fn turn(
        &mut self,
        message: &str,
        attached: Vec<Pending>,
        surface: &mut dyn Turn,
    ) -> Result<TurnEnd, Error> {
        // The FSM of the turn sees the start too (AGL-046, D10).
        let mut observer = Observer::new();
        observer.attach(&self.session);
        let ready = self
            .autostart
            .ensure(&mut |progress| match progress {
                ServerProgress::Starting => observer.enter(PhaseEvent::ServerStart, &mut *surface),
                ServerProgress::Line(line) => surface.line(line),
            })
            .and_then(|()| self.backend.context_size());
        match ready {
            Ok(size) => self.turn_on(size, message, attached, &mut observer, surface),
            Err(error) => {
                // A start that failed never reached the agent.
                observer.end(PhaseEvent::Failed, &mut *surface);
                surface.over();
                Err(error)
            }
        }
    }

    /// The turn on a server that is up, with a context of `context` tokens:
    /// the media the message names are stored, an audio is transcribed
    /// (AGL-014), the agent takes the turn. `observer` saw what came before.
    pub fn turn_on(
        &mut self,
        context: usize,
        message: &str,
        attached: Vec<Pending>,
        observer: &mut Observer,
        surface: &mut dyn Turn,
    ) -> Result<TurnEnd, Error> {
        self.context = Some(context);
        surface.context(context);
        let end = attach::store(
            &mut self.session,
            attached,
            message,
            self.backend,
            self.autostart,
            // A notice of the store is the whisper server's start or the
            // transcription (AGL-043 F2).
            &mut |line| {
                observer.enter(PhaseEvent::Transcribe, &mut *surface);
                surface.line(line);
            },
        )
        .and_then(|()| {
            surface.stored(&self.session);
            self.agent(context)
                .turn_observed(message, &mut *surface, observer)
        });
        surface.over();
        // A start that failed never reached the agent.
        observer.end(PhaseEvent::Failed, &mut *surface);
        end
    }
}
