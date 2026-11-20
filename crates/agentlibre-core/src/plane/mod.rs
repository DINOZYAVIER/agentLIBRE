//! The plane a Function runs on (AGL-067, F1, D10; AGL-075): what a session
//! holds beside its Function — the backend, the
//! session, the memory, the searcher, the engine's start. It outlives a
//! session: `/new` and `/function` start a new one on it. The surfaces —
//! `-p`, the stdin session, the REPL — read their input and draw what comes
//! out; the plane takes a turn.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::about;
use crate::agent::{Agent, Sink, TurnEnd};
use crate::attach::{self, Pending, Transcriber};
use crate::backend::llama::Llama;
use crate::backend::strata::Strata;
use crate::backend::{Backend, ReasoningLevel};
use crate::engine::{Autostart, Engine, Others, ServerProgress};
use crate::error::Error;
use crate::forge::Workspace;
use crate::function::Function;
use crate::i18n::{m, Lang};
use crate::machine::Machine;
use crate::memory::{self, Home, MemoryAction, Write};
use crate::nix;
use crate::paths::Paths;
use crate::phase::{Observer, PhaseEvent};
use crate::programs;
use crate::search::{self, Searcher};
use crate::session::{self, Event, Session};
use crate::settings::{self, ServerFile, ServerKind, Settings, UserFile, function_name};
use crate::status::compact_notice;
use crate::stop::Stop;
use crate::style::Role;
use crate::tools::ForgeEnv;
use crate::tr;
use crate::weights;
use crate::wsl;

pub mod flow;
mod store;
pub mod wire;
mod thread;

pub use flow::{FLOW_FILE, FlowDecl, FlowFile};
pub use store::{PLANE_FILE, PlaneFile};

pub use thread::{Command, Done, Handle, Parts, Sent, SessionView};

/// What a surface gives a turn on the plane (AGL-067): where the reports go,
/// and the few things it shows of the turn's start.
pub trait Turn: Sink {
    /// A line the start of the server or the transcription says (AGL-015,
    /// AGL-043 F2).
    fn line(&mut self, line: &str);
    /// The server answered, with a context of `context` tokens.
    fn context(&mut self, _context: usize) {}
    /// The media the message names are stored (AGL-014): the session's
    /// media of each kind now (`attach::media_counts`).
    fn stored(&mut self, _media: [u32; 3]) {}
    /// The turn is over, before the FSM is closed.
    fn over(&mut self) {}
}

/// A line a slash command says (AGL-067, F2). Its kinds are what a surface
/// needs: the stdin session writes `Output` to stdout, says `Fault` on
/// stderr with `agl: ` before it, and puts both in the session's record;
/// the REPL paints each in its role.
#[derive(Clone, Debug)]
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
    /// What the command said, its lines in order (AGL-118, D7): a surface
    /// that paints roles uses [`Said::lines`] itself.
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|told| match told {
                Told::Output(text)
                | Told::Notice(text)
                | Told::Refusal(text)
                | Told::Fault(text)
                | Told::Pending(text) => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

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

/// Who answers while a plane is built and while its server starts (AGL-075,
/// D8): the terminal asks the human; a caller with nobody to ask gives no
/// `line` and refuses or agrees by `others`.
#[derive(Clone)]
pub struct Asker {
    /// A line told: a notice of the building, a line of the server's start.
    pub say: Arc<dyn Fn(&str) + Send + Sync>,
    /// A line asked for, the folder of a weights file (AGL-039, D24); none
    /// when nobody answers, and a file found nowhere fails.
    pub line: Option<AskLine>,
    /// The consent to start or restart a server (AGL-033, D2).
    pub others: Others,
}

/// Asks the human for a line; `None` when there is no answer.
pub type AskLine = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// What a plane is opened with (AGL-075): where, with what, and who answers.
pub struct Opening<'a> {
    /// The environment: `PATH`, and what `about` reads.
    pub env: &'a dyn Fn(&str) -> Option<OsString>,
    pub paths: &'a Paths,
    pub user: &'a UserFile,
    /// The directory of the run of agl, the workspace of a new session.
    pub cwd: &'a Path,
    pub project: &'a Workspace,
    /// `--resume`: `last`, a name, an id or the start of one.
    pub resume: Option<&'a str>,
    /// `--function`.
    pub function: Option<&'a str>,
    /// `--plane`: the plane of the run (AGL-077, D6); a resumed session
    /// keeps its own.
    pub plane: Option<&'a str>,
    pub reasoning: Option<ReasoningLevel>,
    /// One request (`-p`): the server is brought up before the session is
    /// made, and its context size must be read (AGL-015, D10).
    pub one_request: bool,
    /// The engine agl launched, shared by the planes of the process (AGL-024).
    pub engine: Arc<Mutex<Option<Engine>>>,
    pub lang: Lang,
    pub stop: Stop,
    pub asker: Asker,
}

/// A plane opened, and what its surface needs of the opening.
pub struct Opened {
    pub plane: Plane,
    /// Whether the session existed before this plane (D126).
    pub resumed: bool,
    /// The settings the plane resolved: the surface's colors, its REPL.
    pub settings: Settings,
    /// The FSM of the first turn, which saw the server's start of a run of
    /// one request (AGL-046, D10).
    pub observer: Observer,
}

pub struct Plane {
    /// The plane's store, its id, name and directories, when the run is
    /// in one (AGL-077, D6, D15).
    pub file: Option<PlaneFile>,
    /// The plane's forge, which the forge tool acts on (AGL-077, D9).
    pub plane_forge: Option<Workspace>,
    pub function: Function,
    pub function_name: String,
    pub backend: Arc<dyn Backend + Send + Sync>,
    pub session: Session,
    /// The directory of the sessions of the user, for a new one.
    pub sessions_dir: PathBuf,
    /// The project of the session: a name is unique in it (AGL-062, D5).
    pub project: PathBuf,
    /// The workspace of the session.
    pub workspace: PathBuf,
    /// The context size of the server when it already answered at the
    /// start; read at the first request otherwise (AGL-015, D10).
    pub context: Option<usize>,
    /// Brings the server up when a request finds none (AGL-015).
    pub autostart: Autostart,
    pub lang: Lang,
    /// What the agent's `forge` tool takes from the session (AGL-008): the
    /// data directory and `[forge] auto_rehome`.
    pub data: PathBuf,
    pub auto_rehome: bool,
    /// The model's reasoning level (AGL-013); the REPL's `/think` changes it
    /// for the session.
    pub reasoning: Option<ReasoningLevel>,
    /// The most tokens one picture takes (AGL-014, D10).
    pub image_tokens: usize,
    /// Runs the `search` tool (AGL-019).
    pub search: Box<dyn Searcher + Send>,
    /// Where the memory of the workspace is (AGL-006).
    pub memory: Arc<dyn Home>,
    /// What the model is told of its environment and of this agl (AGL-048).
    pub about: Option<String>,
    /// What stops a turn on this plane (AGL-075): its turns run in its scope.
    pub stop: Stop,
}

impl Plane {
    /// Opens a plane (AGL-075): the session resumed or made, its Function, the
    /// settings, the weights, the backend, the search, the memory and what
    /// the model is told; a run of one request brings its server up first.
    pub fn open(opening: Opening) -> Result<Opened, Error> {
        let Opening {
            env,
            paths,
            user,
            cwd,
            project,
            resume,
            function: cli_function,
            plane: plane_name,
            reasoning,
            one_request,
            engine,
            lang,
            stop,
            asker,
        } = opening;
        let say = |line: &str| (asker.say)(line);
        // AGL-103, D2-D3: what the opening says of this session stands in
        // its record; the lines wait here until the session stands.
        let mut opened_lines: Vec<String> = Vec::new();
        let user_path = paths.settings_file();
        let sessions_dir = paths.sessions();
        // `last`: the last session of this project (AGL-058, D20).
        let resume = match resume {
            Some("last") => Some(
                Session::latest(&sessions_dir, &project.project, 1)
                    .into_iter()
                    .next()
                    .map(|session| session.id)
                    .ok_or_else(|| Error::SessionNotFound {
                        id: "last".into(),
                        dir: sessions_dir.clone(),
                    })?,
            ),
            // A name first, as it was typed, then an id or the start of one
            // (AGL-062, D1).
            Some(key) => Some(
                Session::id_named(&sessions_dir, &project.project, key)
                    .unwrap_or_else(|| key.to_owned()),
            ),
            None => None,
        };
        let session = resume
            .as_deref()
            .map(|id| Session::open(&sessions_dir, id))
            .transpose()?;
        // The plane of the run (AGL-077, D6): named at the start; a resumed
        // session keeps the plane its header records (D13), as it keeps its
        // Function (D106), and goes on in none when the plane is gone.
        if plane_name.is_some() && session.is_some() {
            return Err(Error::PlaneWithResume);
        }
        let plane_file = match (session.as_ref().and_then(|s| s.plane.as_ref()), plane_name) {
            (Some(recorded), _) => {
                let found = PlaneFile::with_id(paths, &recorded.id)?;
                if found.is_none() {
                    let line = tr!(lang, m::PLANE_GONE_LOOSE, name = &recorded.name);
                    say(&line);
                    opened_lines.push(line);
                }
                found
            }
            (None, Some(name)) => Some(PlaneFile::open(paths, name)?),
            (None, None) => None,
        };
        let plane_settings = plane_file
            .as_ref()
            .map(PlaneFile::settings)
            .transpose()?
            .unwrap_or_default();
        let workspace_settings = settings::ProjectFile {
            function: project.function.clone(),
            server: project.server.clone(),
            whisper: project.whisper.clone(),
            search: project.search.clone(),
        };
        // The plane's settings over the workspace's (D7).
        let layered = plane_settings.over(&workspace_settings);
        // The Functions of the plane, the workspace and the user (D7).
        let plane_functions = plane_file.as_ref().map(PlaneFile::functions_dir);
        let workspace_functions = paths.functions_dir(&project.key);
        let user_functions = paths.functions();
        let function_dirs: Vec<&Path> = plane_functions
            .iter()
            .map(PathBuf::as_path)
            .chain([workspace_functions.as_path(), user_functions.as_path()])
            .collect();
        // A resumed session keeps its Function (D106).
        let resumed = session.as_ref().map(|s| s.function.as_str());
        // The Function of a resumed session is the `run` snapshot of its
        // header (AGL-063, D4): the session runs on the configuration it was
        // made with, the Function file not needed. Without the snapshot (the
        // v1 files), the name lookup of today; a Function that is not found
        // continues loose.
        let mut function = match session.as_ref().and_then(|s| s.run.clone()) {
            Some(snapshot) => Function::from_snapshot(&snapshot)?,
            None => {
                // The Function's name (D58, D59, AGL-015); the Function loads
                // before the settings, because its `[server]` table is the
                // model side of the settings when the file declares it
                // (AGL-033, D1, D3).
                let name = function_name(
                    resumed.or(cli_function),
                    layered.function.as_deref(),
                    user.function.as_deref(),
                );
                // The Function's tiers (AGL-028, D3): the workspace store's
                // `functions/` directory, then the user's, then the built-ins.
                match Function::load(&function_dirs, &name) {
                    Ok(function) => function,
                    Err(Error::FunctionNotFound { .. }) if session.is_some() => {
                        // The Function the session was made with is gone: it
                        // continues loose (AGL-063, D4), with a notice.
                        let session = session.as_ref().expect("a resumed session");
                        let line = tr!(lang, m::LOOSE_CONTINUE, function = &session.function);
                        say(&line);
                        opened_lines.push(line);
                        Function::loose(&session.loose_tools(), session.last_budgets)
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        // The model the session was made with (AGL-063, D5): the snapshot's,
        // to compare with the model that runs; `None` without the snapshot.
        let session_model = session
            .as_ref()
            .and_then(|s| s.run.as_ref())
            .and_then(|_| model_name(&function.model));
        // A missing key of `search` names the file that has one, the plane's
        // first.
        let settings_path = match &plane_file {
            Some(plane) if !plane_settings.search.is_empty() => plane.settings_file(),
            _ => project.dir.join(settings::SETTINGS_FILE),
        };
        let mut settings = Settings::resolve(
            user,
            &user_path,
            (&settings_path, &layered),
            resumed.or(cli_function),
            &function.model,
        )?;
        // The weights files become absolute paths before anything starts; a
        // file found nowhere is asked for when somebody answers (AGL-039,
        // D22-D24), and offered for download; a run of one request asks
        // nobody (AGL-050, D10, D11).
        let missing = match &asker.line {
            Some(line) if !one_request => weights::Missing::Ask(&**line, &say),
            _ => weights::Missing::Fail,
        };
        let models = wsl::own_models(paths.models())?;
        weights::resolve(
            &mut settings.server,
            &mut settings.whisper,
            &weights::Folders {
                own: models.clone(),
                listed: &user.models,
                user_path: &user_path,
            },
            &function.sources,
            &missing,
            lang,
        )?;
        // The model of the session's snapshot against the model that runs
        // (AGL-063, D5): a difference is told, never refused.
        if let Some(recorded) = session_model
            && let Some(running) = model_name(&settings.server)
            && model_file(&recorded) != model_file(&running)
        {
            let line = tr!(lang, m::MODEL_DIFFERS, recorded = recorded, running = running);
            say(&line);
            opened_lines.push(line);
        }
        // The tool `search` is there only where `[search]` is complete: no
        // tool, no grammar for it and no word of it in the prompt otherwise
        // (AGL-019, D5). An expired client certificate leaves the tool out
        // too, with a notice (AGL-043 F20).
        let expired = settings.search.as_ref().is_some_and(|config| {
            programs::program("step")
                .is_ok_and(|step| search::certificate_expired(&step, &config.client_certificate))
        });
        let search: Box<dyn Searcher + Send> = match &settings.search {
            Some(config) if !expired => Box::new(search::Curl::new(config.clone())?),
            Some(config) => {
                let line = tr!(
                    lang,
                    m::SEARCH_CERT_EXPIRED,
                    cert = config.client_certificate.display()
                );
                say(&line);
                opened_lines.push(line);
                function.without_tool("search");
                Box::new(search::NoSearch)
            }
            None => {
                function.without_tool("search");
                Box::new(search::NoSearch)
            }
        };
        // A turn to continue needs its state in the Function (D105).
        if let Some(turn) = session.as_ref().and_then(Session::unfinished) {
            Machine::resume(function.table(), &settings.function, &turn)?;
        }

        let workspace = session
            .as_ref()
            .map_or_else(|| cwd.to_path_buf(), |s| s.workspace.clone());
        let auto_rehome = user.forge.auto_rehome.unwrap_or(true);
        let image_tokens = settings
            .server
            .image_max_tokens
            .unwrap_or(settings::DEFAULT_IMAGE_TOKENS) as usize;
        let store = memory::StoreHome {
            workspace: workspace.clone(),
            auto_rehome,
            lang,
        };
        // In a plane, the plane's memory beside the workspace's (D8).
        let memory: Arc<dyn Home> = match &plane_file {
            Some(plane) => Arc::new(memory::PlaneHome {
                name: plane.name.clone(),
                dir: plane.memory_dir(),
                workspace: Box::new(store),
            }),
            None => Arc::new(store),
        };
        // The settings' `server` is the effective one (the function's
        // `[server]` table when the file declares it, AGL-033, D1, D3), and
        // the backend and the url run on it. The backend is the Llama or the
        // Strata backend, by the effective settings' `[server] kind`
        // (AGL-030, D1); the stirred mode runs on Strata.
        let backend: Arc<dyn Backend + Send + Sync> = if settings.server.kind() == ServerKind::Strata {
            Arc::new(Strata::new(&settings.server_url))
        } else {
            Arc::new(Llama::with_ca(
                &settings.server_url,
                settings.server.llama_private_ca.as_deref(),
            )?)
        };
        let autostart = Autostart {
            llama: Arc::clone(&backend),
            project: project.clone(),
            effective: settings.server.clone(),
            whisper: settings.whisper.clone(),
            user_path: user_path.clone(),
            models,
            path_var: env("PATH"),
            runtime: paths.runtime.clone(),
            lang,
            // The consent of D2 (AGL-033): the restart asks, as the server
            // commands do.
            others: asker.others.clone(),
            engine,
        };
        // A run of one request brings the server up now; the others do it
        // at their first request (AGL-015, D10), or as the REPL starts
        // (AGL-046), and take the context size if the server answers. The
        // FSM of the turn sees the start too; its lines wait for the session
        // (AGL-046, D10).
        let mut observer = Observer::new();
        let context = if one_request {
            autostart.ensure(&mut |progress| match progress {
                ServerProgress::Starting => {
                    observer.on(PhaseEvent::ServerStart);
                }
                ServerProgress::Line(line) => {
                    say(line);
                    opened_lines.push(line.to_owned());
                }
            })?;
            Some(backend.context_size()?)
        } else {
            backend.context_size().ok()
        };
        let resumed = session.is_some();
        let mut session = match session {
            Some(session) => session,
            // A new session records the `run` snapshot of its Function
            // (AGL-063, D4).
            None => Session::create_in(
                &sessions_dir,
                &settings.function,
                &workspace,
                Some(&function.snapshot()),
                plane_file.as_ref().map(PlaneFile::of_session).as_ref(),
            )?,
        };
        // AGL-103, D2-D3: what the opening said of the session enters its
        // record in the order it was said.
        for line in opened_lines {
            let _ = session.append(Event::System { text: line, failed: false });
        }
        let about = settings
            .about
            .enabled
            .unwrap_or(true)
            .then(|| about_of(env, paths, project, plane_file.as_ref(), &settings, &workspace, &user_path));
        let plane = Plane {
            plane_forge: plane_file.as_ref().map(PlaneFile::forge_store),
            file: plane_file,
            function,
            function_name: settings.function.clone(),
            backend,
            session,
            sessions_dir,
            project: project.project.clone(),
            workspace,
            context,
            autostart,
            lang,
            data: paths.data.clone(),
            auto_rehome,
            reasoning,
            image_tokens,
            search,
            memory,
            about,
            stop,
        };
        Ok(Opened {
            plane,
            resumed,
            settings,
            observer,
        })
    }

    /// What the agent's `forge` tool takes from the session (AGL-008).
    pub fn forge(&self) -> ForgeEnv<'_> {
        ForgeEnv {
            lang: self.lang,
            auto_rehome: self.auto_rehome,
            data: &self.data,
            plane: self.plane_forge.as_ref(),
        }
    }

    /// The agent of a turn on this plane, for a server with a context of
    /// `context` tokens: the one place an [`Agent`] is built.
    pub fn agent(&mut self, context: usize) -> Agent<'_> {
        Agent {
            function: &self.function,
            function_name: &self.function_name,
            backend: &*self.backend,
            session: &mut self.session,
            workspace: &self.workspace,
            context,
            forge: ForgeEnv {
                lang: self.lang,
                auto_rehome: self.auto_rehome,
                data: &self.data,
                plane: self.plane_forge.as_ref(),
            },
            reasoning: self.reasoning,
            image_tokens: self.image_tokens,
            whisper: &self.autostart as &dyn Transcriber,
            search: &*self.search,
            memory: &*self.memory,
            about: self.about.as_deref(),
            flows: self
                .file
                .as_ref()
                .map(|plane| flow::host::Env { plane_dir: &plane.dir, workdir: &self.workspace }),
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
        let stop = self.stop.clone();
        stop.scope(|| self.turn_scoped(message, attached, surface))
    }

    fn turn_scoped(
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
                ServerProgress::Line(line) => {
                    // AGL-103, D2: the server's own line stands in the record.
                    let _ = self
                        .session
                        .append(Event::System { text: line.to_owned(), failed: false });
                    surface.line(line);
                }
            })
            .and_then(|()| self.backend.context_size());
        match ready {
            Ok(size) => self.turn_on_scoped(size, message, attached, &mut observer, surface),
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
        let stop = self.stop.clone();
        stop.scope(|| self.turn_on_scoped(context, message, attached, observer, surface))
    }

    fn turn_on_scoped(
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
            &*self.backend,
            &self.autostart,
            // A notice of the store is the whisper server's start or the
            // transcription (AGL-043 F2).
            &mut |line| {
                observer.enter(PhaseEvent::Transcribe, &mut *surface);
                surface.line(line);
            },
        )
        .and_then(|()| {
            surface.stored(attach::media_counts(&self.session));
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
    pub fn remember(&mut self, name: &str, text: &str, workspace: bool) -> Said {
        // The plane's memory, the workspace's by `--workspace` (AGL-077, D8).
        let found = if workspace {
            self.memory.write_workspace()
        } else {
            self.memory.write()
        };
        let (dir, lines) = match found {
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
        match memory::looked_at(&*self.memory, action, self.lang) {
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
        match self.session.rename(&self.project, name) {
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
            &self.sessions_dir,
            &self.function_name,
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
        let stop = self.stop.clone();
        let result = stop.scope(|| {
            self.backend
                .context_size()
                .and_then(|context| self.agent(context).compact(sink))
        });
        let interrupted = matches!(result, Err(Error::Interrupted));
        let mut said = Said {
            interrupted,
            ..Said::default()
        };
        if let Some((text, role)) = compact_notice(result, &self.function, self.lang) {
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

    /// A flow of the plane (AGL-118, D2, D7): `agl plane flow` and the agent's
    /// `flow` tool come here, the same verb of the same machine.
    pub fn flow(&mut self, verb: flow::Verb) -> Said {
        let Some(plane) = &self.file else {
            return Said::told(Told::Refusal(tr!(self.lang, m::FLOW_NO_PLANE).to_string()));
        };
        flow::host::run(&plane.dir, &self.workspace, &verb, self.lang)
    }

    /// The line of a command for the session (AGL-047, D5); a session that
    /// will not take it is the surface's trouble (D4).
    fn keep(&mut self, said: &mut Said, event: Event) {
        if let Err(error) = self.session.append(event) {
            said.record_error = Some(error);
        }
    }
}

/// What the model is told of its environment and of this agl (AGL-048).
fn about_of(
    env: &dyn Fn(&str) -> Option<OsString>,
    paths: &Paths,
    project: &Workspace,
    plane: Option<&PlaneFile>,
    settings: &Settings,
    workspace: &Path,
    user_path: &Path,
) -> String {
    let nix_dir = env(nix::NIX_VAR).map(PathBuf::from);
    let functions_dir = env(nix::FUNCTIONS_VAR).map(PathBuf::from);
    let name = format!("{}.nix", settings.function);
    let function_file = plane
        .map(|plane| plane.functions_dir().join(&name))
        .into_iter()
        .chain([
            paths.functions_dir(&project.key).join(&name),
            paths.functions().join(&name),
        ])
    .find(|file| file.is_file())
    .or_else(|| {
        functions_dir
            .as_ref()
            .map(|dir| dir.join("functions").join(&name))
            .filter(|file| file.is_file())
    });
    let session_format = nix_dir
        .as_ref()
        .map(|dir| dir.join("docs/session-file.md"))
        .filter(|file| file.is_file());
    let git_root = about::git_root(workspace);
    let in_wsl = std::fs::read_to_string("/proc/version")
        .is_ok_and(|version| version.to_ascii_lowercase().contains("microsoft"));
    let system = about::system(
        std::env::consts::OS,
        std::fs::read_to_string("/etc/os-release").ok().as_deref(),
        in_wsl,
        wsl::engine_is_windows(),
    );
    let now = session::utc_now();
    let version = about::version(env(about::REV_VAR).and_then(|rev| rev.into_string().ok()).as_deref());
    let (server_kind, model) = match settings.server.kind() {
        ServerKind::Strata => ("strata", settings.server.strata_name.as_deref()),
        ServerKind::Llama => ("llama", settings.server.model.as_deref()),
    };
    let sessions = paths.sessions();
    about::About {
        workspace,
        git_root: git_root.as_deref(),
        system: &system,
        date: &now[..10.min(now.len())],
        version: &version,
        function: &settings.function,
        function_file: function_file.as_deref(),
        server_kind,
        server_url: &settings.server_url,
        model,
        sessions: &sessions,
        session_format: session_format.as_deref(),
        user_settings: user_path,
        project_store: &project.dir,
        plane: plane.map(|plane| (plane.name.as_str(), plane.dir.as_path())),
        source: settings.about.source.as_deref(),
    }
    .render()
}

/// The model a server runs (AGL-063, D5): the weights of the llama kind,
/// the model's name of the strata kind (AGL-089, D5); `None` when the kind
/// sets no model.
fn model_name(server: &ServerFile) -> Option<String> {
    match server.kind() {
        ServerKind::Strata => server.strata_name.clone(),
        ServerKind::Llama => server.model.clone(),
    }
}

/// The name of a model file or config, for the comparison of D5: an
/// absolute path is compared by its file name, so the recorded name and the
/// resolved path of the same file agree.
fn model_file(model: &str) -> &str {
    Path::new(model)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_of_the_server_is_the_weights_or_the_config() {
        // The model that runs (AGL-063, D5): the weights of the llama kind,
        // the model's name of the strata kind (AGL-089, D5).
        let llama = ServerFile {
            kind: Some("llama".into()),
            model: Some("m.gguf".into()),
            ..ServerFile::default()
        };
        assert_eq!(model_name(&llama), Some("m.gguf".into()));
        let strata = ServerFile {
            kind: Some("strata".into()),
            strata_name: Some("tc".into()),
            ..ServerFile::default()
        };
        assert_eq!(model_name(&strata), Some("tc".into()));
        assert_eq!(model_name(&ServerFile::default()), None);
    }

    #[test]
    fn the_model_is_compared_by_its_file_name() {
        // The resolved absolute path and the recorded file name of the same
        // model agree (AGL-063, D5).
        assert_eq!(
            model_file("/home/u/models/ThinkingCap-Qwen3.8-27B-IQ4_XS.gguf"),
            "ThinkingCap-Qwen3.8-27B-IQ4_XS.gguf"
        );
        assert_eq!(model_file("strata.nix"), "strata.nix");
    }

    #[test]
    fn the_model_that_differs_is_told_not_refused() {
        // The difference is told, the resume goes on (AGL-063, D5).
        assert_eq!(
            tr!(Lang::En, m::MODEL_DIFFERS, recorded = "a.gguf", running = "b.gguf"),
            "this session was made with the model `a.gguf`, the model that runs is `b.gguf`"
        );
    }
}
