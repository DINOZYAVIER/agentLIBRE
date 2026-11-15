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
use crate::i18n::{m, Lang};
use crate::memory::{self, Home, MemoryAction, Write};
use crate::phase::{Observer, PhaseEvent};
use crate::search::Searcher;
use crate::session::{Event, Session};
use crate::status::compact_notice;
use crate::style::Role;
use crate::tools::ForgeEnv;
use crate::tr;

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

/// A line a slash command says (AGL-067, F2). Its kinds are what a surface
/// needs: the stdin session writes `Output` to stdout, says `Fault` on
/// stderr with `agl: ` before it, and puts both in the session's record;
/// the REPL paints each in its role.
#[derive(Debug)]
pub enum Told {
    /// The command's answer: `/memory list`, `/memory show`.
    Output(String),
    /// What the command did: a new session, a new name, a moved store.
    Notice(String),
    /// A refusal of the program's own: a wrong usage, an entry not written.
    Refusal(String),
    /// What went wrong, in an [`Error`]'s words.
    Fault(String),
    /// A command the human cut short.
    Pending(String),
}

/// What a slash command did (AGL-067, F2): the plane does the work once, and
/// each surface shows [`Told`] and writes the session's record its own way
/// (D4). What the record holds is `content`, or, for the stdin session, the
/// lines it was told.
#[derive(Default)]
pub struct Said {
    /// What to say, in its order.
    pub lines: Vec<Told>,
    /// A memory entry the command wrote, for the memory line of the surface.
    pub memory: Option<Write>,
    /// What the record of the command holds.
    pub content: Option<String>,
    /// A command that did not do what was asked.
    pub failed: bool,
    /// `/compact` cut short, which ends a stdin session with its code.
    pub interrupted: bool,
    /// A line the session would not take: a stdin session ends on it, the
    /// REPL says it and goes on (D4).
    pub record_error: Option<Error>,
}

impl Said {
    /// A command that met an error.
    fn fault(error: &Error, lang: Lang) -> Said {
        Said {
            lines: vec![Told::Fault(error.message(lang))],
            failed: true,
            ..Said::default()
        }
    }

    /// A command whose whole saying is one line, recorded as that line.
    fn told(told: Told) -> Said {
        let text = match &told {
            Told::Output(text)
            | Told::Notice(text)
            | Told::Refusal(text)
            | Told::Fault(text)
            | Told::Pending(text) => text.clone(),
        };
        Said {
            lines: vec![told],
            content: Some(text),
            ..Said::default()
        }
    }
}

/// The `Event` of a written memory entry (AGL-047, D5).
fn written(write: &Write) -> Event {
    Event::Memory {
        name: write.name.clone(),
        kind: write.kind,
        text: write.text.clone(),
    }
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

    /// `/remember name | text` (AGL-006, M34, M37): the entry is written with
    /// no turn; the model has it from the record of the command (AGL-047, D5).
    pub fn remember(&mut self, name: &str, text: &str) -> Said {
        let (dir, lines) = match self.memory.write() {
            Ok(both) => both,
            // A store that will not resolve is said as a failed remembering
            // in both surfaces, and the REPL records the error's text.
            Err(error) => {
                let reason = error.message(self.lang);
                return Said {
                    lines: vec![Told::Refusal(
                        tr!(self.lang, m::REMEMBER_FAILED, reason = reason.clone()).to_string(),
                    )],
                    content: Some(reason),
                    failed: true,
                    ..Said::default()
                };
            }
        };
        let (result, write) = memory::remember(&dir, name, text);
        let mut said = Said {
            lines: lines.into_iter().map(Told::Notice).collect(),
            content: Some(result.content),
            ..Said::default()
        };
        match write {
            Some(write) => {
                said.memory = Some(write);
                let event = written(said.memory.as_ref().unwrap());
                self.keep(&mut said, event);
            }
            None => {
                let content = said.content.clone().unwrap_or_default();
                said.lines.push(Told::Refusal(
                    tr!(self.lang, m::REMEMBER_FAILED, reason = content).to_string(),
                ));
                said.failed = true;
            }
        }
        said
    }

    /// `/memory remove <name>` once the human agreed (AGL-060, D7).
    pub fn remove(&mut self, name: &str) -> Said {
        let removed = self
            .memory
            .look()
            .and_then(|dir| memory::remove_entry(dir.as_deref(), name));
        match removed {
            Ok((content, write)) => {
                let event = written(&write);
                let mut said = Said {
                    memory: Some(write),
                    content: Some(content),
                    ..Said::default()
                };
                self.keep(&mut said, event);
                said
            }
            Err(error) => Said::fault(&error, self.lang),
        }
    }

    /// `/memory list`, `/memory show` (AGL-060, D5).
    pub fn looked_at(&mut self, action: &MemoryAction) -> Said {
        match memory::looked_at(self.memory, action, self.lang) {
            Ok(text) => {
                let text = text.trim_end().to_owned();
                Said {
                    lines: vec![Told::Output(text.clone())],
                    content: Some(text),
                    ..Said::default()
                }
            }
            Err(error) => Said::fault(&error, self.lang),
        }
    }

    /// `/rename <name>` (AGL-062, D7): the name is a line of the session's file.
    pub fn rename(&mut self, name: &str) -> Said {
        match self.session.rename(self.project, name) {
            Ok(()) => {
                let named = self.session.name.clone().unwrap_or_default();
                Said::told(Told::Notice(
                    tr!(self.lang, m::RENAMED, name = named).to_string(),
                ))
            }
            Err(error) => Said::fault(&error, self.lang),
        }
    }

    /// `/new`: a new session on the plane with the `run` snapshot of its
    /// Function (AGL-063, D4). A surface clears what the old session held.
    /// A session that cannot be made ends the run: nothing else works.
    pub fn new_session(&mut self) -> Result<Said, Error> {
        let new = Session::create_run(
            self.sessions_dir,
            self.function_name,
            &self.workspace,
            Some(&self.function.snapshot()),
        )?;
        self.session = new;
        Ok(Said::told(Told::Notice(
            tr!(self.lang, m::NEW_SESSION, id = self.session.id).to_string(),
        )))
    }

    /// `/compact`: the summary of the session's start; the reports of the
    /// turn that makes it go to `sink`.
    pub fn compact(&mut self, sink: &mut dyn Sink) -> Said {
        let result = self
            .backend
            .context_size()
            .and_then(|context| self.agent(context).compact(sink));
        let interrupted = matches!(result, Err(Error::Interrupted));
        let mut said = Said {
            interrupted,
            ..Said::default()
        };
        if let Some((text, role)) = compact_notice(result, self.function, self.lang) {
            said.failed = role != Role::Muted;
            // The interrupted one is pending, the rest of them a refusal:
            // a stdin session says none of these with `agl: ` before it.
            said.lines
                .push(match role {
                    Role::Muted => Told::Notice(text.clone()),
                    Role::StatusPending => Told::Pending(text.clone()),
                    _ => Told::Refusal(text.clone()),
                });
            said.content = Some(text);
        }
        said
    }

    /// The line of a command for the session (AGL-047, D5); a session that
    /// will not take it is the surface's trouble (D4).
    fn keep(&mut self, said: &mut Said, event: Event) {
        if let Err(error) = self.session.append(event) {
            said.record_error = Some(error);
        }
    }
}
