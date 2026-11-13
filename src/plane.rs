//! The plane a Function runs on (AGL-067, F1, D10): what a session holds
//! beside its Function — the backend, the session, the memory, the searcher,
//! the engine's start. It outlives a session: `/new` and `/function` start a
//! new one on it. The surfaces — `-p`, the stdin session, the REPL — read
//! their input and draw what comes out; the plane takes a turn.

use std::path::{Path, PathBuf};

use crate::agent::Agent;
use crate::attach::Transcriber;
use crate::backend::{Backend, ReasoningLevel};
use crate::cli::serve::Autostart;
use crate::function::Function;
use crate::i18n::Lang;
use crate::memory::Home;
use crate::search::Searcher;
use crate::session::Session;
use crate::tools::ForgeEnv;

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
}
