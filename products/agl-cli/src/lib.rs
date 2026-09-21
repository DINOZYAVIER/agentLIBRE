mod cli;
mod config;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agl_core::Content;
use agl_core::agent::{
    AgentEvent, AgentEventData, AgentOperationKind, AgentRunOrigin, AgentRunSpec, AgentRunStatus,
    ConversationView, MessageRole,
};
use agl_core::{AgentRunId, ConversationId, MessageId};
use agl_daemon::{DaemonConfig, DaemonServer, IntegrationConfig, ListenerSource, SearxngConfig};
use agl_daemon_api::{AgentClient, AgentProgress, AgentProgressStatus, AgentSubscriptionFrame};
use agl_runtime::inference::{InferenceConfig, LlamaServerConfig};
use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use cli::{
    APPLICATION_DESCRIPTION, ArtifactCommand, Cli, Command, ConfigCommand, ConversationCommand,
    FunctionCommand, PlanCommand, ResumeArgs, StoreCommand, parse_reasoning,
};
use config::Decorations;
use crossterm::{
    cursor,
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event as CtermEvent,
        KeyCode as CtermKeyCode, KeyEvent, KeyEventKind, KeyModifiers as CtermKeyModifiers,
    },
    execute, queue,
    style::Print,
    terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
};
use nu_ansi_term::{Color as AnsiColor, Style as AnsiStyle};
use pulldown_cmark::{Event, Options, Parser as MarkdownParser, Tag, TagEnd};
use reedline::{
    Completer, Emacs, FileBackedHistory, Highlighter, Hinter, KeyCode, KeyModifiers, MenuBuilder,
    Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline,
    ReedlineEvent, Signal, Span, StyledText, Suggestion, default_emacs_keybindings,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

static TTY_OUTPUT: std::sync::OnceLock<Mutex<Option<reedline::ExternalPrinter<String>>>> =
    std::sync::OnceLock::new();
static TTY_RENDERER: std::sync::OnceLock<Mutex<Option<mpsc::UnboundedSender<TtyMessage>>>> =
    std::sync::OnceLock::new();

#[derive(Debug)]
enum TtyMessage {
    Text(String),
    ModelDelta(String),
    AnswerFinal { text: String, elapsed: Duration },
    Activity(String),
}

fn send_tty(message: TtyMessage) -> bool {
    let Some(renderer) = TTY_RENDERER.get() else {
        return false;
    };
    let sender = renderer
        .lock()
        .expect("TTY renderer lock poisoned")
        .as_ref()
        .cloned();
    sender.is_some_and(|sender| sender.send(message).is_ok())
}
/// Output is handed to Reedline's external printer. Reedline already owns the
/// cursor and repaint lifecycle while the editor is active; adding cursor
/// movement here races its painter and makes adjacent blocks overwrite each
/// other.
fn begin_external_block() {}

struct InputDockGuard {
    stop_reader: Option<Arc<std::sync::atomic::AtomicBool>>,
    reader: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for InputDockGuard {
    fn drop(&mut self) {
        if let Some(stop) = self.stop_reader.take() {
            stop.store(true, std::sync::atomic::Ordering::Release);
        }
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        let _ = execute!(std::io::stderr(), DisableBracketedPaste);
        let _ = terminal::disable_raw_mode();
        let mut output = std::io::stderr().lock();
        let _ = output.write_all(b"\x1b[0m\x1b[?25h\n");
        let _ = output.flush();
        if let Some(renderer) = TTY_RENDERER.get() {
            *renderer.lock().expect("TTY renderer lock poisoned") = None;
        }
    }
}

fn emit_terminal(text: String, stderr: bool) {
    if let Some(renderer) = TTY_RENDERER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("TTY renderer lock poisoned")
        .as_ref()
    {
        let _ = renderer.send(TtyMessage::Text(text));
        return;
    }
    let sink = TTY_OUTPUT.get_or_init(|| Mutex::new(None));
    if let Some(printer) = sink.lock().expect("TTY output lock poisoned").as_ref() {
        let width = terminal::size()
            .map(|(columns, _)| usize::from(columns))
            .ok()
            .filter(|columns| *columns > 0)
            .unwrap_or(80);
        let _ = printer.print(wrap_external_text(&text, width));
    } else if stderr {
        let mut output = std::io::stderr().lock();
        let _ = output.write_all(text.as_bytes());
        let _ = output.flush();
    } else {
        let mut output = std::io::stdout().lock();
        let _ = output.write_all(text.as_bytes());
        let _ = output.flush();
    }
}

/// ExternalPrinter treats each `\n` as a logical line, while the terminal can
/// wrap a long line without emitting a newline. Insert explicit breaks first
/// so the following prompt repaint cannot land on a wrapped continuation.
fn wrap_external_text(text: &str, width: usize) -> String {
    let width = width.max(1);
    text.split('\n')
        .enumerate()
        .fold(String::new(), |mut output, (index, line)| {
            if index > 0 {
                output.push('\n');
            }
            let mut visible = 0usize;
            let mut cursor = 0usize;
            while cursor < line.len() {
                if line.as_bytes()[cursor] == 0x1b {
                    let end = ansi_escape_end(line, cursor);
                    output.push_str(&line[cursor..end]);
                    cursor = end;
                    continue;
                }
                let ch = line[cursor..]
                    .chars()
                    .next()
                    .expect("cursor always points at a character boundary");
                let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if visible > 0 && visible + char_width > width {
                    output.push('\n');
                    visible = 0;
                }
                output.push(ch);
                visible += char_width;
                cursor += ch.len_utf8();
            }
            output
        })
}

fn terminal_crlf(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut previous_cr = false;
    for ch in text.chars() {
        if ch == '\n' && !previous_cr {
            output.push('\r');
        }
        output.push(ch);
        previous_cr = ch == '\r';
    }
    output
}

fn display_path_with_home(path: &Path) -> String {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return path.display().to_string();
    };
    if path == home {
        return "~".to_owned();
    }
    path.strip_prefix(&home)
        .map(|relative| format!("~/{}", relative.display()))
        .unwrap_or_else(|_| path.display().to_string())
}

macro_rules! print {
    ($($arg:tt)*) => { $crate::emit_terminal(format!("{}", format_args!($($arg)*)), false) };
}
macro_rules! println {
    () => { $crate::emit_terminal("\n".to_owned(), false) };
    ($($arg:tt)*) => { $crate::emit_terminal(format!("{}\n", format_args!($($arg)*)), false) };
}
macro_rules! eprintln {
    () => { $crate::emit_terminal("\n".to_owned(), true) };
    ($($arg:tt)*) => { $crate::emit_terminal(format!("{}\n", format_args!($($arg)*)), true) };
}

const FALLBACK_ANSWER_WIDTH: usize = 64;
const INPUT_HINT: &str = "Type a message or / for commands";
const FULL_SPLASH: &str = include_str!("../../../splash");
const SHORT_SPLASH_RANGES: [(usize, usize); 3] = [(1, 8), (11, 18), (51, 58)];

fn full_splash() -> String {
    FULL_SPLASH.trim_end_matches('\n').to_owned()
}

fn short_splash() -> String {
    FULL_SPLASH
        .lines()
        .map(|line| {
            SHORT_SPLASH_RANGES
                .iter()
                .map(|&(start, end)| line.chars().skip(start).take(end - start + 1).collect())
                .collect::<Vec<String>>()
                .join("  ")
        })
        .collect::<Vec<String>>()
        .join("\n")
}

/// Keep the dock geometry in one place. Reedline owns the editable buffer;
/// this prompt only supplies the stable separator and indicator around it.
#[derive(Clone, Debug)]
struct DockPrompt {
    left: String,
    indicator: String,
    multiline_indicator: String,
}

impl DockPrompt {
    fn new(activity: impl Into<String>, continuation: bool, theme: &TerminalTheme) -> Self {
        let terminal_width = terminal::size().map(|(width, _)| width).unwrap_or(80);
        let width = usize::from(terminal_width).max(1);
        let activity = format!("{} ", activity.into());
        let lead = truncate_chars("─ ", width);
        let activity = truncate_chars(&activity, width.saturating_sub(lead.chars().count()));
        let used = lead.chars().count() + activity.chars().count();
        let separator = format!(
            "{}{}{}",
            paint_on_background(theme, "input_rule", &lead),
            paint_on_background(theme, "input_activity", &activity),
            paint_on_background(theme, "input_rule", &"─".repeat(width.saturating_sub(used)))
        );
        let input_fill = fill_background_row(theme, width);
        let indicator_text = if continuation { "… " } else { "› " };
        let indicator = theme.paint("input_prompt", indicator_text);
        Self {
            // Reedline's painter is the sole owner of cursor movement. Keep
            // the prompt as ordinary multiline text so its external printer
            // can move transcript messages above it atomically.
            left: format!("{separator}\n{input_fill}\n"),
            indicator,
            multiline_indicator: theme.paint("input_prompt", "… "),
        }
    }
}

fn fill_background_row(theme: &TerminalTheme, width: usize) -> String {
    let width = width.max(1);
    let repeat = width.saturating_sub(1);
    format!(
        "{} \x1b[{repeat}b\r{}",
        theme.start("input_background"),
        theme.reset()
    )
}

fn paint_on_background(theme: &TerminalTheme, role: &'static str, text: &str) -> String {
    format!(
        "{}{}{}",
        theme.start("input_background"),
        theme.paint(role, text),
        theme.start("input_background")
    )
}

impl Prompt for DockPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.left)
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed(&self.indicator)
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.multiline_indicator)
    }
    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Owned(format!(
            "(history {}) ",
            match search.status {
                PromptHistorySearchStatus::Passing => "",
                PromptHistorySearchStatus::Failing => "failing",
            }
        ))
    }
}

struct DockHinter {
    style: TerminalStyle,
    background: TerminalStyle,
}

impl DockHinter {
    fn new(theme: &TerminalTheme) -> Self {
        Self {
            style: theme
                .styles
                .get("input_hint")
                .cloned()
                .expect("known input hint style"),
            background: theme
                .styles
                .get("input_background")
                .cloned()
                .expect("known input background style"),
        }
    }
}

impl Hinter for DockHinter {
    fn handle(
        &mut self,
        line: &str,
        _pos: usize,
        _history: &dyn reedline::History,
        _ansi: bool,
        _cwd: &str,
    ) -> String {
        if line.is_empty() {
            if _ansi {
                format!(
                    "{}{}",
                    self.background.start(true),
                    self.style.paint(true, INPUT_HINT)
                )
            } else {
                INPUT_HINT.to_owned()
            }
        } else {
            String::new()
        }
    }
    fn complete_hint(&self) -> String {
        INPUT_HINT.to_owned()
    }
    fn next_hint_token(&self) -> String {
        INPUT_HINT.to_owned()
    }
}

struct InputHighlighter {
    style: AnsiStyle,
}

impl Highlighter for InputHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut text = StyledText::new();
        text.push((self.style, line.to_owned()));
        text
    }
}

fn truncate_chars(value: &str, width: usize) -> String {
    value.chars().take(width).collect()
}

#[derive(Debug, Default)]
struct RunQueue {
    messages: VecDeque<String>,
}

impl RunQueue {
    fn push(&mut self, message: String) {
        self.messages.push_back(message);
    }
    fn pop(&mut self) -> Option<String> {
        self.messages.pop_front()
    }
    fn len(&self) -> usize {
        self.messages.len()
    }
    fn clear(&mut self) {
        self.messages.clear();
    }
}

fn activity_text(active: bool, tool: Option<&str>, retrying: bool, queued: usize) -> String {
    let state = if retrying {
        "Retrying".to_owned()
    } else if let Some(tool) = tool {
        format!("Running {tool}")
    } else if active {
        "Generating".to_owned()
    } else {
        "Ready".to_owned()
    };
    if queued == 0 {
        state
    } else {
        format!("{state} · queued {queued}")
    }
}

pub fn run_cli() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(if agl_daemon::is_permanent_startup_error(&error) {
            78
        } else {
            1
        });
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.reasoning.is_some()
        && !matches!(
            &cli.command,
            None | Some(Command::Chat(_)) | Some(Command::Run { .. }) | Some(Command::Resume(_))
        )
    {
        bail!("--reasoning is valid only for a new chat, resume, or run");
    }
    if cli.function.is_some()
        && cli.command.is_some()
        && !matches!(&cli.command, Some(Command::Chat(_)))
    {
        bail!("--function is valid only when opening a new root chat");
    }
    let roots = application_roots()?;
    let socket = agl_daemon::default_socket_path(&roots.state);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start CLI runtime")?;
    runtime.block_on(async move {
        let client = AgentClient::new(socket.clone());
        match cli.command {
            Some(Command::Plan { command }) => match command {
                PlanCommand::Create { function, prompt } => {
                    let workspace = std::env::current_dir()?.canonicalize()?;
                    let active = config::read_active(&roots.data)
                        .context("no generated configuration; run `agl config apply` first")?;
                    let function = match function {
                        Some(locator) => resolve_function_locator(
                            &config::parse_locator(&locator, &workspace)?,
                            &active,
                        )?,
                        None => active
                            .functions
                            .first()
                            .context("generated configuration has no planner Function")?
                            .directory
                            .clone(),
                    };
                    let plan = client
                        .create_plan(
                            utf8_path(&function.canonicalize()?, "Planner Function")?,
                            utf8_path(&workspace, "workspace")?,
                            Content::text(prompt)?,
                        )
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&plan.0)?);
                    eprintln!("conversation={}", plan.1.id);
                    Ok(())
                }
                PlanCommand::View { plan_id } => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&client.view_plan(plan_id).await?)?
                    );
                    Ok(())
                }
                PlanCommand::Approve { plan_id, digest } => {
                    let approved = client.approve_plan(plan_id, digest).await?;
                    println!(
                        "plan={}\tdigest={}\tstate={}",
                        approved.plan.id,
                        approved.digest,
                        serde_json::to_string(&approved.state)?
                    );
                    Ok(())
                }
                PlanCommand::Implement {
                    plan_id,
                    digest,
                    function,
                } => {
                    let workspace = std::env::current_dir()?.canonicalize()?;
                    let active = config::read_active(&roots.data)
                        .context("no generated configuration; run `agl config apply` first")?;
                    let function = resolve_function_locator(
                        &config::parse_locator(&function, &workspace)?,
                        &active,
                    )?;
                    let result = client
                        .implement_plan(
                            plan_id,
                            digest,
                            utf8_path(&function.canonicalize()?, "Coder Function")?,
                        )
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&result)?);
                    Ok(())
                }
                PlanCommand::Status { plan_id } => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&client.plan_status(plan_id).await?)?
                    );
                    Ok(())
                }
            },
            Some(Command::Store {
                command: StoreCommand::Rotate,
            }) => {
                let rotated = agl_daemon::rotate_store(&roots.data)?;
                println!(
                    "retained: {}\nactive: {}",
                    rotated.retained.display(),
                    rotated.active.display()
                );
                Ok(())
            }
            Some(Command::Config { command }) => {
                let extra = match &command {
                    ConfigCommand::Check { function } => function.clone(),
                    ConfigCommand::Apply { function } => function.clone(),
                };
                let plan = config::plan(&roots.config, &extra)?;
                agl_daemon::preflight_store(&roots.data)
                    .context("Store preflight failed; active configuration was not changed")?;
                match command {
                    ConfigCommand::Check { .. } => {
                        print_config_plan(&plan);
                        Ok(())
                    }
                    ConfigCommand::Apply { .. } => apply_config(&roots, &socket, &plan),
                }
            }
            Some(Command::Doctor) => {
                print_doctor(&roots)?;
                Ok(())
            }
            Some(Command::Chat(args)) => {
                ensure!(
                    cli.function.is_none()
                        || (args.function.is_none() && args.function_directory.is_none()),
                    "specify the chat Function only once"
                );
                let function = args.function.or(args.function_directory).or(cli.function);
                interactive_chat(&client, &roots, function, cli.reasoning).await
            }
            None => interactive_chat(&client, &roots, cli.function, cli.reasoning).await,
            Some(Command::Resume(args)) => {
                let conversation = select_resume_conversation(&client, args).await?;
                let workspace = std::env::current_dir()?.canonicalize()?;
                let (human, _) = config::load_document(&roots.config)?;
                let theme = TerminalTheme::from_colors(&conversation.presentation.colors)?;
                render_conversation_header(&conversation, human.repl.decorations_default, &theme);
                chat_loop(
                    &client,
                    conversation,
                    &workspace,
                    cli.reasoning,
                    human.repl.decorations_default,
                    &theme,
                )
                .await
            }
            Some(Command::Conversation { command }) => match command {
                ConversationCommand::Rename {
                    conversation,
                    new_name,
                } => {
                    let renamed = client.rename_conversation(conversation, new_name).await?;
                    println!(
                        "{}\t{}",
                        renamed.id,
                        renamed.display_name.unwrap_or_default()
                    );
                    Ok(())
                }
            },
            Some(Command::Serve) => {
                let active = config::read_active(&roots.data)
                    .context("no generated configuration; run `agl config apply` first")?;
                let searxng = active.search.as_ref().map(|search| IntegrationConfig {
                    required: search.required,
                    binding: SearxngConfig {
                        client_certificate: search.client_certificate.clone(),
                        client_private_key: search.client_private_key.clone(),
                        private_ca: search.private_ca.clone(),
                    },
                });
                let server = DaemonServer::start(DaemonConfig {
                    data_root: roots.data,
                    execution_socket: agl_execd_socket(&roots.state),
                    inference: InferenceConfig::llama_server(
                        LlamaServerConfig {
                            executable: active.executable,
                        },
                        agl_execution_api::BlockingExecutionClient::new(agl_execd_socket(
                            &roots.state,
                        )),
                    )?,
                    extensions: vec![],
                    searxng,
                    max_resident_bytes: active
                        .keep_free_ram
                        .map(|reserve| agl_runtime::host_memory_budget(Some(reserve))),
                })?;
                let listener = if std::env::var_os("LISTEN_FDS").is_some() {
                    ListenerSource::Systemd
                } else {
                    ListenerSource::Bind(socket)
                };
                server.serve(listener).await
            }
            Some(Command::Function { command }) => match command {
                FunctionCommand::Lock { function_directory } => {
                    let workspace = std::env::current_dir()?.canonicalize()?;
                    let identity = agl_runtime::function_identity(function_directory, &workspace)?;
                    println!(
                        "{}@{}\t{}\tdigest={}",
                        identity.id,
                        identity.version,
                        identity.directory.display(),
                        identity.digest,
                    );
                    Ok(())
                }
            },
            Some(Command::Artifact { command }) => match command {
                ArtifactCommand::Add {
                    id,
                    kind,
                    schema,
                    git_source,
                    revision,
                    source_dir,
                } => {
                    let workspace = std::env::current_dir()?;
                    let storage = ayeque_forge_core::data_root()?;
                    let registration = ayeque_forge_core::EntityRegistration::new(
                        id, kind, schema, git_source, revision, source_dir,
                    )?;
                    let entity = add_artifact(&storage, &workspace, registration)?;
                    println!("{}\t{}", entity.id(), entity.materialized_path().display());
                    Ok(())
                }
            },
            Some(Command::Run {
                function_directory,
                prompt,
            }) => {
                let workspace = std::env::current_dir()?.canonicalize()?;
                let active = config::read_active(&roots.data)
                    .context("no generated configuration; run `agl config apply` first")?;
                let function_directory = resolve_function_locator(
                    &config::parse_locator(&function_directory, &workspace)?,
                    &active,
                )?;
                let conversation =
                    open_conversation(&client, &function_directory, &workspace, true)
                        .await?
                        .context("Function activation interrupted")?;
                let theme = TerminalTheme::from_colors(&conversation.presentation.colors)?;
                let view = foreground_turn(
                    &client,
                    conversation.id,
                    prompt,
                    true,
                    cli.reasoning,
                    Decorations::Off,
                    conversation.presentation.tool_output,
                    conversation.presentation.tool.frame,
                    &theme,
                    conversation.presentation.model_generation.details,
                    None,
                )
                .await?;
                ensure!(
                    view.status == AgentRunStatus::Completed,
                    "{}",
                    terminal_summary(&view)
                );
                Ok(())
            }
            Some(Command::View { run_id }) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&client.run_view(run_id).await?)?
                );
                Ok(())
            }
            Some(Command::Cancel { run_id }) => {
                client.cancel(run_id).await?;
                println!("{run_id}");
                Ok(())
            }
        }
    })
}

fn add_artifact(
    storage: &Path,
    workspace: &Path,
    registration: ayeque_forge_core::EntityRegistration,
) -> Result<ayeque_forge_core::VerifiedEntity> {
    let location = ayeque_forge_core::initialize_project(storage, workspace)?;
    let manifest_path = location.project_path().join("FORGE.toml");
    create_forge_initial_file(&manifest_path, b"format = 2\n")?;
    let project = ayeque_forge_core::resolve_project_at(storage, workspace)?;
    let lock_path = location.project_path().join("FORGE.lock");
    if !lock_path.exists() {
        let initial_lock = ayeque_forge_core::serialize_lock(&project, Vec::new())?;
        create_forge_initial_file(&lock_path, &initial_lock)?;
    }
    let id = registration.id().to_owned();
    let updated = ayeque_forge_core::register_entity(&project, registration)?;
    ayeque_forge_core::resolve_entity(&updated, &id)
}

fn create_forge_initial_file(path: &Path, bytes: &[u8]) -> Result<()> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            let written = file.write_all(bytes).and_then(|_| file.sync_all());
            if let Err(error) = written {
                drop(file);
                let _ = std::fs::remove_file(path);
                return Err(error)
                    .with_context(|| format!("failed to initialize {}", path.display()));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to create {}", path.display())),
    }
}

#[cfg(test)]
mod artifact_tests {
    use super::*;

    #[test]
    fn artifact_add_initializes_catalog_and_resolves_materialization() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let source = temporary.path().join("source");
        let storage = temporary.path().join("forge-data");
        std::fs::create_dir_all(source.join("document")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(source.join("document/README.md"), "# Verified document\n").unwrap();
        assert!(
            ProcessCommand::new("git")
                .args(["init", "--quiet"])
                .current_dir(&source)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            ProcessCommand::new("git")
                .args(["add", "document/README.md"])
                .current_dir(&source)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            ProcessCommand::new("git")
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "fixture"
                ])
                .current_dir(&source)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            ProcessCommand::new("git")
                .args(["init", "--quiet"])
                .current_dir(&workspace)
                .status()
                .unwrap()
                .success()
        );

        let registration = || {
            ayeque_forge_core::EntityRegistration::new(
                "test.document".into(),
                "document".into(),
                "agentlibre.document/v1".into(),
                source.to_string_lossy().into_owned(),
                "HEAD".into(),
                "document".into(),
            )
            .unwrap()
        };
        let entity = add_artifact(&storage, &workspace, registration()).unwrap();
        assert_eq!(entity.id(), "test.document");
        assert_eq!(
            std::fs::read_to_string(entity.materialized_path().join("README.md")).unwrap(),
            "# Verified document\n"
        );
        assert!(add_artifact(&storage, &workspace, registration()).is_err());
        let project = ayeque_forge_core::resolve_project_at(&storage, &workspace).unwrap();
        assert_eq!(
            ayeque_forge_core::verify_lock(&project)
                .unwrap()
                .entities()
                .len(),
            1
        );
    }
}

fn agl_execd_socket(state_root: &Path) -> PathBuf {
    state_root.join("execd").join("execd.sock")
}

async fn interactive_chat(
    client: &AgentClient,
    roots: &ApplicationRoots,
    function_locator: Option<String>,
    reasoning: Option<agl_core::agent::ReasoningEffort>,
) -> Result<()> {
    let workspace = std::env::current_dir()?.canonicalize()?;
    let active = config::read_active(&roots.data)
        .context("no generated configuration; run `agl config apply` first")?;
    let function = match function_locator {
        Some(locator) => {
            resolve_function_locator(&config::parse_locator(&locator, &workspace)?, &active)?
        }
        None => active
            .functions
            .first()
            .context("generated configuration has no default Function")?
            .directory
            .clone(),
    };
    let conversation = open_conversation(client, &function, &workspace, false)
        .await?
        .context("chat activation interrupted")?;
    let theme = TerminalTheme::from_colors(&conversation.presentation.colors)?;
    let (human, _) = config::load_document(&roots.config)?;
    render_conversation_header(&conversation, human.repl.decorations_default, &theme);
    chat_loop(
        client,
        conversation,
        &workspace,
        reasoning,
        human.repl.decorations_default,
        &theme,
    )
    .await
}

fn render_conversation_header(
    conversation: &ConversationView,
    decorations: Decorations,
    theme: &TerminalTheme,
) {
    let splash = match terminal::size() {
        Ok((width, _)) if usize::from(width) < terminal_visible_width(FULL_SPLASH) => {
            short_splash()
        }
        _ => full_splash(),
    };
    println!("{}", theme.paint("input_prompt", &splash));
    println!("{APPLICATION_DESCRIPTION}");
    if decorations != Decorations::Off {
        println!(
            "{} {}",
            theme.paint("run", "CONVERSATION"),
            theme.paint("run_id", &conversation.id.to_string())
        );
    } else {
        println!("conversation {}", conversation.id);
    }
}

async fn open_conversation(
    client: &AgentClient,
    function: &Path,
    workspace: &Path,
    report: bool,
) -> Result<Option<ConversationView>> {
    let function = function.canonicalize().with_context(|| {
        format!(
            "failed to resolve Function directory {}",
            function.display()
        )
    })?;
    let function = utf8_path(&function, "Function directory")?;
    let workspace = utf8_path(workspace, "workspace")?;
    let conversation_id = ConversationId::generate();
    let request = client.open_conversation(conversation_id, function, workspace);
    tokio::pin!(request);
    tokio::select! {
        result = &mut request => {
            let (conversation, activation) = result?;
            if report {
                println!("function={}", activation.function.digest);
                println!("dependencies={}", activation.dependencies);
                println!("model_artifact={}", activation.model_artifact);
                println!("model_service={}", activation.model_service);
                println!("runtime_profile={}", activation.runtime_profile);
                println!("active_slots={}", activation.active_slots);
                println!("batching={}", if activation.continuous_batching { "continuous" } else { "disabled" });
            }
            Ok(Some(conversation))
        }
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for Ctrl-C")?;
            Ok(None)
        }
    }
}

async fn chat_loop(
    client: &AgentClient,
    conversation: ConversationView,
    workspace: &Path,
    reasoning: Option<agl_core::agent::ReasoningEffort>,
    decorations_default: Decorations,
    theme: &TerminalTheme,
) -> Result<()> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return chat_loop_tty(
            client,
            conversation,
            workspace,
            reasoning,
            decorations_default,
            theme,
        )
        .await;
    }
    chat_loop_linear(client, conversation, reasoning, decorations_default, theme).await
}

#[derive(Debug)]
enum TtyInputEvent {
    Key(KeyEvent),
    Paste(String),
    Resize,
}

#[derive(Debug)]
enum TtyEditorAction {
    Continue,
    Submit(String),
    Eof,
    Interrupted,
}

struct TtyEditor {
    buffer: String,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    completion: Option<(Vec<Suggestion>, usize)>,
    draft: Option<String>,
}

impl TtyEditor {
    fn new(history_path: &Path) -> Result<Self> {
        let history = if history_path.is_file() {
            std::fs::read_to_string(history_path)?
                .lines()
                .filter_map(|line| serde_json::from_str::<String>(line).ok())
                .filter(|line| !line.is_empty())
                .collect()
        } else {
            Vec::new()
        };
        Ok(Self {
            buffer: String::new(),
            cursor: 0,
            history,
            history_index: None,
            completion: None,
            draft: None,
        })
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.history_index = None;
        self.completion = None;
        self.draft = None;
    }

    fn insert(&mut self, text: &str) {
        self.buffer.insert_str(self.cursor, text);
        self.cursor += text.len();
        if self.buffer.starts_with('/') {
            self.open_completion();
        } else {
            self.completion = None;
        }
    }

    fn insert_paste(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let safe = normalized
            .chars()
            .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
            .collect::<String>();
        self.insert(&safe);
    }

    fn handle_key(&mut self, key: KeyEvent) -> TtyEditorAction {
        if matches!(key.kind, KeyEventKind::Release) {
            return TtyEditorAction::Continue;
        }
        if key.modifiers.contains(CtermKeyModifiers::CONTROL) {
            match key.code {
                CtermKeyCode::Char('c') => return TtyEditorAction::Interrupted,
                CtermKeyCode::Char('d') if self.buffer.is_empty() => return TtyEditorAction::Eof,
                CtermKeyCode::Char('a') => self.cursor = 0,
                CtermKeyCode::Char('e') => self.cursor = self.buffer.len(),
                CtermKeyCode::Char('u') => {
                    self.buffer.drain(..self.cursor);
                    self.cursor = 0;
                }
                CtermKeyCode::Char('w') => {
                    let before = self.buffer[..self.cursor].trim_end();
                    let start = before.rfind(char::is_whitespace).map_or(0, |i| i + 1);
                    self.buffer.drain(start..self.cursor);
                    self.cursor = start;
                }
                CtermKeyCode::Char('n') => self.next_history(),
                CtermKeyCode::Char('p') => self.previous_history(),
                _ => {}
            }
            return TtyEditorAction::Continue;
        }
        match key.code {
            CtermKeyCode::Char('/') if self.buffer.is_empty() => {
                self.insert("/");
                self.open_completion();
            }
            CtermKeyCode::Char(ch) => self.insert(&ch.to_string()),
            CtermKeyCode::Backspace => {
                if self.cursor > 0 {
                    let start = self.buffer[..self.cursor]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(index, _)| index);
                    self.buffer.drain(start..self.cursor);
                    self.cursor = start;
                }
                self.completion = None;
            }
            CtermKeyCode::Delete => {
                if self.cursor < self.buffer.len() {
                    let end = self.buffer[self.cursor..]
                        .char_indices()
                        .nth(1)
                        .map_or(self.buffer.len(), |(index, _)| self.cursor + index);
                    self.buffer.drain(self.cursor..end);
                }
            }
            CtermKeyCode::Left => {
                self.cursor = self.buffer[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map_or(0, |(index, _)| index);
            }
            CtermKeyCode::Right => {
                self.cursor = self.buffer[self.cursor..]
                    .char_indices()
                    .nth(1)
                    .map_or(self.buffer.len(), |(index, _)| self.cursor + index);
            }
            CtermKeyCode::Home => self.cursor = 0,
            CtermKeyCode::End => self.cursor = self.buffer.len(),
            CtermKeyCode::Up if self.completion.is_some() => self.cycle_completion(false),
            CtermKeyCode::Down if self.completion.is_some() => self.cycle_completion(true),
            CtermKeyCode::Up => self.previous_history(),
            CtermKeyCode::Down => self.next_history(),
            CtermKeyCode::Tab => self.complete(),
            CtermKeyCode::Enter => {
                if self.completion.is_some() {
                    self.accept_completion();
                    return TtyEditorAction::Continue;
                }
                let line = std::mem::take(&mut self.buffer);
                self.cursor = 0;
                self.history_index = None;
                self.completion = None;
                self.draft = None;
                return TtyEditorAction::Submit(line);
            }
            CtermKeyCode::Esc => self.completion = None,
            _ => {}
        }
        TtyEditorAction::Continue
    }

    fn previous_history(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            self.draft = Some(self.buffer.clone());
        }
        let index = self
            .history_index
            .unwrap_or(self.history.len())
            .saturating_sub(1);
        self.history_index = Some(index);
        self.buffer = self.history[index].clone();
        self.cursor = self.buffer.len();
        self.completion = None;
    }

    fn next_history(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 >= self.history.len() {
            self.history_index = None;
            self.buffer = self.draft.take().unwrap_or_default();
            self.cursor = 0;
        } else {
            self.history_index = Some(index + 1);
            self.buffer = self.history[index + 1].clone();
            self.cursor = self.buffer.len();
        }
        self.completion = None;
    }

    fn complete(&mut self) {
        let mut completer = SlashCompleter;
        let suggestions = completer.complete(&self.buffer, self.cursor);
        if suggestions.is_empty() {
            return;
        }
        let next = self.completion.as_ref().map_or(0, |(_, index)| index + 1);
        let index = next % suggestions.len();
        let suggestion = &suggestions[index];
        self.buffer.replace_range(
            suggestion.span.start..suggestion.span.end,
            &suggestion.value,
        );
        self.cursor = suggestion.span.start + suggestion.value.len();
        self.completion = Some((suggestions, index));
    }

    fn open_completion(&mut self) {
        let mut completer = SlashCompleter;
        let suggestions = completer.complete(&self.buffer, self.cursor);
        if !suggestions.is_empty() {
            self.completion = Some((suggestions, 0));
        }
    }

    fn accept_completion(&mut self) {
        let Some((suggestions, index)) = self.completion.take() else {
            return;
        };
        let Some(suggestion) = suggestions.get(index) else {
            return;
        };
        self.buffer.replace_range(
            suggestion.span.start..suggestion.span.end,
            &suggestion.value,
        );
        self.cursor = suggestion.span.start + suggestion.value.len();
    }

    fn cycle_completion(&mut self, forward: bool) {
        let Some((suggestions, index)) = self.completion.as_mut() else {
            return;
        };
        if suggestions.is_empty() {
            return;
        }
        *index = if forward {
            (*index + 1) % suggestions.len()
        } else {
            (*index + suggestions.len() - 1) % suggestions.len()
        };
    }

    fn save_history(&mut self, path: &Path, line: &str) -> Result<()> {
        if line.is_empty() {
            return Ok(());
        }
        self.history.push(line.to_owned());
        if self.history.len() > 1000 {
            let excess = self.history.len() - 1000;
            self.history.drain(..excess);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let serialized = self
            .history
            .iter()
            .map(|entry| serde_json::to_string(entry).expect("history strings are serializable"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, serialized + "\n")?;
        Ok(())
    }
}

struct TtyRenderer {
    theme: TerminalTheme,
    activity: String,
    continuation: bool,
    editor: TtyEditor,
    function: String,
    cwd: String,
    initialized: bool,
    streaming: String,
}

impl TtyRenderer {
    fn new(
        theme: TerminalTheme,
        editor: TtyEditor,
        function: impl Into<String>,
        cwd: impl Into<String>,
    ) -> Self {
        Self {
            theme,
            activity: "Ready".to_owned(),
            continuation: false,
            editor,
            function: function.into(),
            cwd: cwd.into(),
            initialized: false,
            streaming: String::new(),
        }
    }

    fn repaint(&mut self) -> Result<()> {
        let (width, _) = terminal::size().unwrap_or((80, 24));
        let width = usize::from(width.max(1));
        let mut output = std::io::stderr();
        queue!(output, BeginSynchronizedUpdate)?;
        if self.initialized {
            queue!(
                output,
                cursor::MoveToColumn(0),
                cursor::MoveUp(2),
                Clear(ClearType::CurrentLine),
                Print(self.separator(width)),
                Print("\r\n"),
                Clear(ClearType::CurrentLine),
                Print(self.input_padding(width)),
                Print("\r\n"),
                Clear(ClearType::CurrentLine),
                Print(self.prompt_line(width)),
                Print("\r\n"),
                Clear(ClearType::CurrentLine),
                Print(self.input_padding(width)),
                Print("\r\n"),
                Clear(ClearType::CurrentLine),
                Print(self.status_row(width)),
                cursor::MoveUp(2),
                cursor::MoveToColumn(self.cursor_column(width))
            )?;
        } else {
            queue!(
                output,
                Print(self.separator(width)),
                Print("\r\n"),
                Print(self.input_padding(width)),
                Print("\r\n"),
                Print(self.prompt_line(width)),
                Print("\r\n"),
                Print(self.input_padding(width)),
                Print("\r\n"),
                Print(self.status_row(width)),
                cursor::MoveUp(2),
                cursor::MoveToColumn(self.cursor_column(width))
            )?;
            self.initialized = true;
        }
        queue!(output, EndSynchronizedUpdate)?;
        output.flush()?;
        Ok(())
    }

    fn input_padding(&self, width: usize) -> String {
        format!(
            "{}{}{}",
            self.theme.start("input_background"),
            " ".repeat(width),
            self.theme.reset()
        )
    }

    fn prompt_line(&self, width: usize) -> String {
        let bg = self.theme.start("input_background");
        let reset = self.theme.reset();
        let indicator = truncate_by_cells(if self.continuation { "… " } else { "› " }, width);
        let available = width.saturating_sub(UnicodeWidthStr::width(indicator.as_str()));
        let buffer = visible_tail(&self.editor.buffer, self.editor.cursor, available);
        let content = if buffer.text.is_empty() {
            self.theme
                .paint("input_hint", &truncate_by_cells(INPUT_HINT, available))
        } else {
            self.theme.paint("input_text", &buffer.text)
        };
        let content_width = terminal_visible_width(&content);
        format!(
            "{bg}{}{bg}{}{bg}{}{}",
            self.theme.paint("input_prompt", &indicator),
            content,
            " ".repeat(available.saturating_sub(content_width)),
            reset
        )
    }

    fn cursor_column(&self, width: usize) -> u16 {
        let indicator = truncate_by_cells(if self.continuation { "… " } else { "› " }, width);
        let available = width.saturating_sub(UnicodeWidthStr::width(indicator.as_str()));
        let buffer = visible_tail(&self.editor.buffer, self.editor.cursor, available);
        (UnicodeWidthStr::width(indicator.as_str()) + buffer.cursor_cells)
            .min(width.saturating_sub(1)) as u16
    }

    fn status_row(&self, width: usize) -> String {
        let left = format!("  {}", self.function);
        let right = format!("{}  ", self.cwd);
        let (left, right) = fit_status_values(&left, &right, width);
        let gap = width.saturating_sub(
            UnicodeWidthStr::width(left.as_str()) + UnicodeWidthStr::width(right.as_str()),
        );
        format!(
            "{}{}{}",
            self.theme.paint("muted", &left),
            " ".repeat(gap),
            self.theme.paint("muted", &right)
        )
    }

    fn separator(&self, width: usize) -> String {
        let activity = truncate_by_cells(&format!(" {} ", self.activity), width.saturating_sub(1));
        let used = UnicodeWidthStr::width(activity.as_str());
        format!(
            "{}{}{}",
            self.theme.paint("input_rule", "─"),
            self.theme.paint("input_activity", &activity),
            self.theme
                .paint("input_rule", &"─".repeat(width.saturating_sub(1 + used)))
        )
    }

    fn append_text(&mut self, text: &str) -> Result<()> {
        if !self.streaming.is_empty() {
            self.streaming.clear();
        }
        self.replace_input_with_transcript(text)
    }

    fn append_stream(&mut self, delta: &str) -> Result<()> {
        let first = self.streaming.is_empty();
        self.streaming.push_str(delta);
        let text = if first {
            format!("{}\r\n{}", answer_rule(&self.theme), delta)
        } else {
            delta.to_owned()
        };
        self.replace_input_with_transcript(&text)
    }

    fn finalize_answer(&mut self, text: &str, elapsed: Duration) -> Result<()> {
        if self.streaming.is_empty() && !text.is_empty() {
            return self.append_text(&format!(
                "\n{}\n{}\n{}\n",
                answer_rule(&self.theme),
                markdown_to_terminal(text, &self.theme),
                answer_footer(&self.theme, elapsed)
            ));
        }
        // The model deltas already rendered the answer body. Only commit the
        // footer here; reprinting `text` would duplicate the streamed answer.
        let _ = text;
        self.streaming.clear();
        self.replace_input_with_transcript(&format!(
            "\r\n{}\r\n",
            answer_footer(&self.theme, elapsed)
        ))
    }

    fn append_submitted(&mut self, text: &str) -> Result<()> {
        let width = usize::from(terminal::size().unwrap_or((80, 24)).0.max(1));
        self.replace_input_with_transcript(&submitted_message_frame(&self.theme, text, width))
    }

    fn replace_input_with_transcript(&mut self, text: &str) -> Result<()> {
        let width = usize::from(terminal::size().unwrap_or((80, 24)).0.max(1));
        let mut output = std::io::stderr();
        queue!(output, BeginSynchronizedUpdate, cursor::MoveToColumn(0))?;
        if self.initialized {
            queue!(
                output,
                cursor::MoveUp(2),
                Clear(ClearType::CurrentLine),
                cursor::MoveDown(1),
                Clear(ClearType::CurrentLine),
                cursor::MoveDown(1),
                Clear(ClearType::CurrentLine),
                cursor::MoveDown(1),
                Clear(ClearType::CurrentLine),
                cursor::MoveDown(1),
                Clear(ClearType::CurrentLine),
                cursor::MoveUp(4),
                cursor::MoveToColumn(0)
            )?;
        }
        let mut transcript = wrap_external_text(text, width);
        if !transcript.ends_with('\n') {
            transcript.push('\n');
        }
        queue!(
            output,
            Print(terminal_crlf(&transcript)),
            Print(self.separator(width)),
            Print("\r\n"),
            Print(self.input_padding(width)),
            Print("\r\n"),
            Print(self.prompt_line(width)),
            Print("\r\n"),
            Print(self.input_padding(width)),
            Print("\r\n"),
            Print(self.status_row(width)),
            cursor::MoveUp(2),
            cursor::MoveToColumn(self.cursor_column(width)),
            EndSynchronizedUpdate
        )?;
        self.initialized = true;
        output.flush()?;
        Ok(())
    }
}

struct VisibleTail {
    text: String,
    cursor_cells: usize,
}

fn visible_tail(text: &str, cursor: usize, width: usize) -> VisibleTail {
    let cursor = cursor.min(text.len());
    let line_start = text[..cursor].rfind('\n').map_or(0, |index| index + 1);
    let line_end = text[cursor..]
        .find('\n')
        .map_or(text.len(), |index| cursor + index);
    let line = text[line_start..line_end].replace('\t', "    ");
    let line_cursor = text[line_start..cursor].replace('\t', "    ").len();
    visible_single_line_tail(&line, line_cursor, width)
}

fn visible_single_line_tail(text: &str, cursor: usize, width: usize) -> VisibleTail {
    let before = &text[..cursor];
    let mut start = 0;
    let mut cells = 0;
    for (index, ch) in before.char_indices().rev() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cells + w > width {
            break;
        }
        cells += w;
        start = index;
    }
    let mut visible = String::new();
    let mut used = 0;
    for ch in text[start..].chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            break;
        }
        visible.push(ch);
        used += w;
    }
    VisibleTail {
        text: visible,
        cursor_cells: UnicodeWidthStr::width(&text[start..cursor]),
    }
}

fn truncate_by_cells(text: &str, width: usize) -> String {
    let mut cells = 0;
    text.chars()
        .take_while(|ch| {
            let next = cells + UnicodeWidthChar::width(*ch).unwrap_or(0);
            if next > width {
                false
            } else {
                cells = next;
                true
            }
        })
        .collect()
}

fn truncate_from_end_by_cells(text: &str, width: usize) -> String {
    let mut used = 0;
    let mut start = text.len();
    for (index, ch) in text.char_indices().rev() {
        let next = used + UnicodeWidthChar::width(ch).unwrap_or(0);
        if next > width {
            break;
        }
        used = next;
        start = index;
    }
    text[start..].to_owned()
}

fn fit_status_values(left: &str, right: &str, width: usize) -> (String, String) {
    let left_width = UnicodeWidthStr::width(left);
    let right_width = UnicodeWidthStr::width(right);
    if left_width + right_width <= width {
        return (left.to_owned(), right.to_owned());
    }
    let left_budget = width / 2;
    let right_budget = width.saturating_sub(left_budget);
    (
        truncate_by_cells(left, left_budget),
        truncate_from_end_by_cells(right, right_budget),
    )
}

fn wrapped_input_lines(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            rows.push(String::new());
            continue;
        }
        let mut row = String::new();
        let mut used = 0;
        for ch in line.chars() {
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if char_width > width {
                if !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                }
                row.push('…');
                used = 1;
                continue;
            }
            if !row.is_empty() && used + char_width > width {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push(ch);
            used += char_width;
        }
        rows.push(row);
    }
    rows
}

fn submitted_message_frame(theme: &TerminalTheme, text: &str, width: usize) -> String {
    let width = width.max(1);
    let text = safe_terminal_text(text);
    let marker = truncate_by_cells("› ", width);
    let marker_width = UnicodeWidthStr::width(marker.as_str());
    if marker_width >= width {
        let background = theme.start("input_background");
        let reset = theme.reset();
        let mut rows = vec![format!(
            "{background}{}{reset}",
            theme.paint("input_prompt", &marker)
        )];
        rows.extend(wrapped_input_lines(&text, width).into_iter().map(|row| {
            format!(
                "{background}{}{background}{}{reset}",
                theme.paint("input_text", &row),
                " ".repeat(width.saturating_sub(UnicodeWidthStr::width(row.as_str())))
            )
        }));
        return rows.join("\r\n");
    }
    let continuation = " ".repeat(marker_width);
    let rows = wrapped_input_lines(&text, width.saturating_sub(marker_width));
    let background = theme.start("input_background");
    let reset = theme.reset();
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            let prefix = if index == 0 {
                marker.as_str()
            } else {
                &continuation
            };
            let prefix_width = UnicodeWidthStr::width(prefix);
            let row_width = prefix_width + UnicodeWidthStr::width(row.as_str());
            format!(
                "{background}{}{background}{}{background}{}{reset}",
                theme.paint(
                    "input_prompt",
                    if index == 0 { prefix } else { &continuation }
                ),
                theme.paint("input_text", &row),
                " ".repeat(width.saturating_sub(row_width))
            )
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

async fn chat_loop_tty(
    client: &AgentClient,
    conversation: ConversationView,
    workspace: &Path,
    mut reasoning: Option<agl_core::agent::ReasoningEffort>,
    decorations_default: Decorations,
    theme: &TerminalTheme,
) -> Result<()> {
    let stop_reader = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut input_dock_guard = InputDockGuard {
        stop_reader: Some(stop_reader.clone()),
        reader: None,
    };
    terminal::enable_raw_mode().context("failed to enable terminal raw mode")?;
    execute!(std::io::stderr(), EnableBracketedPaste)
        .context("failed to enable terminal bracketed paste")?;
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<TtyInputEvent>();
    let (renderer_tx, mut renderer_rx) = mpsc::unbounded_channel::<TtyMessage>();
    *TTY_RENDERER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("TTY renderer lock poisoned") = Some(renderer_tx);
    let history_path = application_roots()?.state.join("repl/history.reedline");
    let function = format!(
        "{}@{}",
        conversation.function.id, conversation.function.version
    );
    let mut renderer = TtyRenderer::new(
        theme.clone(),
        TtyEditor::new(&history_path)?,
        function,
        display_path_with_home(workspace),
    );
    renderer.repaint()?;
    input_dock_guard.reader = Some(tokio::task::spawn_blocking(move || {
        while !stop_reader.load(std::sync::atomic::Ordering::Acquire) {
            if !event::poll(Duration::from_millis(100)).unwrap_or(false) {
                continue;
            }
            match event::read() {
                Ok(CtermEvent::Key(key)) => {
                    if input_tx.send(TtyInputEvent::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(CtermEvent::Paste(text)) => {
                    if input_tx.send(TtyInputEvent::Paste(text)).is_err() {
                        break;
                    }
                }
                Ok(CtermEvent::Resize(_, _)) => {
                    if input_tx.send(TtyInputEvent::Resize).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }));
    let mut queue = RunQueue::default();
    let mut decorations_override = None;
    let tool_output_defaults = conversation.presentation.tool_output;
    let mut tool_output = tool_output_defaults;
    let tool_frame_default = conversation.presentation.tool.frame;
    let mut tool_frame = tool_frame_default;
    let model_generation_default = conversation.presentation.model_generation.details;
    let mut model_generation_details = model_generation_default;
    let active_id = Arc::new(Mutex::new(None::<AgentRunId>));
    let mut active: Option<tokio::task::JoinHandle<Result<agl_core::agent::AgentRunView>>> = None;
    let mut last_run: Option<AgentRunId> = None;
    let mut continuation = String::new();
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c(), if active.is_some() => {
                signal.context("failed to listen for Ctrl-C")?;
                let run_id = *active_id.lock().expect("active run lock poisoned");
                if let Some(run_id) = run_id {
                    client.cancel(run_id).await.context("failed to cancel active Run")?;
                }
                queue.clear();
                renderer.activity = "Ready".to_owned();
                renderer.append_text("cancellation requested; queued input cleared\n")?;
            }
            result = async { active.as_mut().expect("guarded").await }, if active.is_some() => {
                active = None;
                *active_id.lock().expect("active run lock poisoned") = None;
                match result {
                    Ok(Ok(view)) => { last_run = Some(view.id); }
                    Ok(Err(error)) => eprintln!("turn failed: {error:#}"),
                    Err(error) => eprintln!("turn task failed: {error}"),
                }
                renderer.activity = "Ready".to_owned();
                if let Some(next) = queue.pop() {
                    renderer.activity = "Generating".to_owned();
                    active = Some(spawn_tty_run(client, conversation.id, next, reasoning, decorations_override.unwrap_or(decorations_default), theme, tool_output, tool_frame, model_generation_details, active_id.clone()));
                }
                renderer.repaint()?;
            }
            message = renderer_rx.recv() => if let Some(message) = message {
                match message {
                    TtyMessage::Text(text) => renderer.append_text(&text)?,
                    TtyMessage::ModelDelta(delta) => renderer.append_stream(&delta)?,
                    TtyMessage::AnswerFinal { text, elapsed } => renderer.finalize_answer(&text, elapsed)?,
                    TtyMessage::Activity(activity) => {
                        renderer.activity = activity;
                        renderer.repaint()?
                    }
                }
            },
            event = input_rx.recv() => match event {
                Some(TtyInputEvent::Resize) => renderer.repaint()?,
                Some(TtyInputEvent::Paste(text)) => {
                    renderer.editor.insert_paste(&text);
                    renderer.repaint()?;
                }
                Some(TtyInputEvent::Key(key)) => match renderer.editor.handle_key(key) {
                    TtyEditorAction::Continue => renderer.repaint()?,
                    TtyEditorAction::Eof => return Ok(()),
                    TtyEditorAction::Interrupted => {
                        if active.is_none() { return Ok(()); }
                        let run_id = *active_id.lock().expect("active run lock poisoned");
                        if let Some(run_id) = run_id { client.cancel(run_id).await.context("failed to cancel active Run")?; }
                        queue.clear();
                        renderer.editor.reset();
                        renderer.activity = "Ready".to_owned();
                        renderer.append_text("cancellation requested; queued input cleared\n")?;
                    }
                    TtyEditorAction::Submit(mut line) => {
                    line = line.trim_end().to_owned();
                    if line.ends_with('\\') && !line.ends_with("\\\\") {
                        line.pop();
                        continuation.push_str(&line);
                        continuation.push('\n');
                        renderer.continuation = true;
                        renderer.repaint()?;
                        continue;
                    }
                    if !continuation.is_empty() {
                        continuation.push_str(&line);
                        line = std::mem::take(&mut continuation);
                    }
                    renderer.continuation = false;
                    if line.trim().is_empty() { continue; }
                    renderer.editor.save_history(&history_path, &line)?;
                    renderer.append_submitted(&line)?;
                    if is_quit_command(&line) { return Ok(()); }
                    if line.trim() == "/help" {
                        println!("/help  /status  /reasoning [default|low|medium|xhigh]  /decorations [off|default|full|reset]  /tool-output [lines N|chars N|reset]  /tool-frame [on|off|reset]  /model-generation [details on|off|reset]  /tool N [input|result]  /quit  /exit");
                        continue;
                    }
                    if line.trim() == "/status" {
                        let active_run = active_id.lock().expect("active run lock poisoned");
                        println!("conversation_id={} active_run={} queued={}", conversation.id, active_run.map_or_else(|| "none".to_owned(), |id| id.to_string()), queue.len());
                        continue;
                    }
                    if line == "/tool" || line.starts_with("/tool ") {
                        let active_run = (*active_id.lock().expect("active run lock poisoned")).or(last_run);
                        if let Err(error) = view_tool(client, active_run, line.strip_prefix("/tool").unwrap_or("").trim(), theme).await {
                            eprintln!("tool viewer: {error:#}");
                        }
                        continue;
                    }
                    match tool_output_command(&line, &mut tool_output, tool_output_defaults) {
                        Ok(true) => { println!("tool-output: lines={}, chars={}", tool_output.lines, tool_output.chars); continue; }
                        Err(error) => { eprintln!("{error}"); continue; }
                        Ok(false) => {}
                    }
                    match tool_frame_command(&line, &mut tool_frame, tool_frame_default) {
                        Ok(true) => { println!("tool-frame: enabled={tool_frame}"); continue; }
                        Err(error) => { eprintln!("{error}"); continue; }
                        Ok(false) => {}
                    }
                    match model_generation_command(&line, &mut model_generation_details, model_generation_default) {
                        Ok(true) => { println!("model-generation: details={model_generation_details}"); continue; }
                        Err(error) => { eprintln!("{error}"); continue; }
                        Ok(false) => {}
                    }
                    match decorations_command(&line, &mut decorations_override) {
                        Ok(true) => { println!("decorations: current={}", decorations_label(decorations_override.unwrap_or(decorations_default))); continue; }
                        Err(error) => { eprintln!("{error}"); continue; }
                        Ok(false) => {}
                    }
                    match reasoning_command(&line, &mut reasoning) {
                        Ok(true) => { println!("reasoning: effective={}", reasoning_label(&conversation.reasoning, reasoning)); continue; }
                        Err(error) => { eprintln!("{error}"); continue; }
                        Ok(false) => {}
                    }
                    if line.starts_with('/') { eprintln!("unknown command; use /help"); continue; }
                    if active.is_some() { queue.push(line); }
                    else {
                        renderer.activity = "Generating".to_owned();
                        active = Some(spawn_tty_run(client, conversation.id, line, reasoning, decorations_override.unwrap_or(decorations_default), theme, tool_output, tool_frame, model_generation_details, active_id.clone()));
                        renderer.repaint()?;
                    }
                }
                },
                None => return Ok(()),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_tty_run(
    client: &AgentClient,
    conversation: ConversationId,
    prompt: String,
    reasoning: Option<agl_core::agent::ReasoningEffort>,
    decorations: Decorations,
    theme: &TerminalTheme,
    output: agl_core::agent::ToolOutputPresentation,
    frame: bool,
    details: bool,
    active_id: Arc<Mutex<Option<AgentRunId>>>,
) -> tokio::task::JoinHandle<Result<agl_core::agent::AgentRunView>> {
    let client = client.clone();
    let theme = theme.clone();
    tokio::spawn(async move {
        foreground_turn(
            &client,
            conversation,
            prompt,
            true,
            reasoning,
            decorations,
            output,
            frame,
            &theme,
            details,
            Some(&active_id),
        )
        .await
    })
}

async fn chat_loop_linear(
    client: &AgentClient,
    conversation: ConversationView,
    mut reasoning: Option<agl_core::agent::ReasoningEffort>,
    decorations_default: Decorations,
    theme: &TerminalTheme,
) -> Result<()> {
    let mut decorations_override: Option<Decorations> = None;
    let tool_output_defaults = conversation.presentation.tool_output;
    let mut tool_output = tool_output_defaults;
    let tool_frame_default = conversation.presentation.tool.frame;
    let mut tool_frame = tool_frame_default;
    let model_generation_default = conversation.presentation.model_generation.details;
    let mut model_generation_details = model_generation_default;
    let mut active_run: Option<AgentRunId> = None;
    let mut run_queue = RunQueue::default();
    let mut input = BufReader::new(tokio::io::stdin());
    let rendered = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let history_path = application_roots()?.state.join("repl/history.reedline");
    let mut editor = if rendered {
        let parent = history_path
            .parent()
            .context("REPL history path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let history = FileBackedHistory::with_file(1000, history_path.clone())
            .context("failed to initialize REPL history")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            if history_path.is_file() {
                std::fs::set_permissions(&history_path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        let mut keybindings = default_emacs_keybindings();
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::Menu("search_menu".into()),
        );
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Char('/'),
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::Edit(vec![reedline::EditCommand::InsertChar('/')]),
                ReedlineEvent::Menu("search_menu".into()),
            ]),
        );
        Some(
            Reedline::create()
                .with_completer(Box::new(SlashCompleter))
                .with_menu(reedline::ReedlineMenu::EngineCompleter(Box::new(
                    reedline::ListMenu::default()
                        .with_name("search_menu")
                        .with_only_buffer_difference(false),
                )))
                .with_edit_mode(Box::new(Emacs::new(keybindings)))
                .with_quick_completions(true)
                .with_ansi_colors(theme.enabled)
                .with_highlighter(input_highlighter(theme)?)
                .with_visual_selection_style(input_selection_style(theme)?)
                .with_hinter(Box::new(DockHinter::new(theme)))
                .with_history(Box::new(history)),
        )
    } else {
        None
    };
    loop {
        let mut prompt = match read_repl_line(&mut editor, &mut input, "> ", theme).await? {
            ReplRead::Line(line) => line,
            ReplRead::Eof | ReplRead::Interrupted => {
                run_queue.clear();
                if rendered {
                    println!();
                }
                return Ok(());
            }
        };
        while prompt.ends_with('\\') && !prompt.ends_with("\\\\") {
            prompt.pop();
            prompt.push('\n');
            match read_repl_line(&mut editor, &mut input, "... ", theme).await? {
                ReplRead::Line(line) => prompt.push_str(&line),
                ReplRead::Eof | ReplRead::Interrupted => return Ok(()),
            }
        }
        if prompt.trim().is_empty() {
            continue;
        }
        let prompt = prompt.as_str();
        if is_quit_command(prompt) {
            return Ok(());
        }
        if prompt.trim() == "/help" {
            println!(
                "/help  /status  /reasoning [default|low|medium|xhigh]  /decorations [off|default|full|reset]  /tool-output [lines N|chars N|reset]  /tool-frame [on|off|reset]  /model-generation [details on|off|reset]  /tool N [input|result]  /quit  /exit"
            );
            continue;
        }
        if prompt.trim() == "/status" {
            println!(
                "conversation_id={} display_name={} reasoning_default={} reasoning_effective={} decorations_configured={} decorations_current={} tool_output_lines={} tool_output_chars={} tool_frame={} model_generation_details={} active_run={} queued={}",
                conversation.id,
                conversation.display_name.as_deref().unwrap_or("-"),
                reasoning_label(&conversation.reasoning, None),
                reasoning_label(&conversation.reasoning, reasoning),
                decorations_label(decorations_default),
                decorations_label(decorations_override.unwrap_or(decorations_default)),
                tool_output.lines,
                tool_output.chars,
                tool_frame,
                model_generation_details,
                active_run.map_or_else(|| "none".to_owned(), |run| run.to_string()),
                run_queue.len(),
            );
            continue;
        }
        if prompt == "/tool" || prompt.starts_with("/tool ") {
            if let Err(error) = view_tool(
                client,
                active_run,
                prompt.strip_prefix("/tool").unwrap_or("").trim(),
                theme,
            )
            .await
            {
                eprintln!("tool viewer: {error:#}");
            }
            continue;
        }
        match tool_output_command(prompt, &mut tool_output, tool_output_defaults) {
            Ok(true) => {
                println!(
                    "tool-output: lines={}, chars={}",
                    tool_output.lines, tool_output.chars
                );
                continue;
            }
            Err(error) => {
                eprintln!("{error}");
                continue;
            }
            Ok(false) => {}
        }
        match tool_frame_command(prompt, &mut tool_frame, tool_frame_default) {
            Ok(true) => {
                println!("tool-frame: enabled={tool_frame}");
                continue;
            }
            Err(error) => {
                eprintln!("{error}");
                continue;
            }
            Ok(false) => {}
        }
        match model_generation_command(
            prompt,
            &mut model_generation_details,
            model_generation_default,
        ) {
            Ok(true) => {
                println!("model-generation: details={model_generation_details}");
                continue;
            }
            Err(error) => {
                eprintln!("{error}");
                continue;
            }
            Ok(false) => {}
        }
        match decorations_command(prompt, &mut decorations_override) {
            Ok(true) => {
                println!(
                    "decorations: configured={}, current={}",
                    decorations_label(decorations_default),
                    decorations_label(decorations_override.unwrap_or(decorations_default))
                );
                continue;
            }
            Err(error) => {
                eprintln!("{error}");
                continue;
            }
            Ok(false) => {}
        }
        match reasoning_command(prompt, &mut reasoning) {
            Ok(true) => {
                println!(
                    "reasoning: default={}, effective={}",
                    reasoning_label(&conversation.reasoning, None),
                    reasoning_label(&conversation.reasoning, reasoning)
                );
                continue;
            }
            Err(error) => {
                eprintln!("{error}");
                continue;
            }
            Ok(false) => {}
        }
        if prompt.trim_start().starts_with('/') {
            eprintln!("unknown command; use /help");
            continue;
        }
        if active_run.is_some() {
            run_queue.push(prompt.to_owned());
            continue;
        }
        match foreground_turn(
            client,
            conversation.id,
            prompt.to_owned(),
            rendered,
            reasoning,
            decorations_override.unwrap_or(decorations_default),
            tool_output,
            tool_frame,
            theme,
            model_generation_details,
            None,
        )
        .await
        {
            Ok(view) => {
                active_run = Some(view.id);
                if !view.status.is_terminal() {
                    eprintln!("{}", terminal_summary(&view));
                }
                if view.status.is_terminal() {
                    active_run = None;
                    if let Some(next) = run_queue.pop() {
                        // The next queued message is submitted only after the
                        // previous terminal state, preserving FIFO ordering.
                        match foreground_turn(
                            client,
                            conversation.id,
                            next,
                            rendered,
                            reasoning,
                            decorations_override.unwrap_or(decorations_default),
                            tool_output,
                            tool_frame,
                            theme,
                            model_generation_details,
                            None,
                        )
                        .await
                        {
                            Ok(next_view) => active_run = Some(next_view.id),
                            Err(error) => eprintln!("turn failed: {error:#}"),
                        }
                    }
                }
            }
            Err(error) => eprintln!("turn failed: {error:#}"),
        }
    }
}

enum ReplRead {
    Line(String),
    Eof,
    Interrupted,
}

async fn read_repl_line(
    editor: &mut Option<Reedline>,
    input: &mut BufReader<tokio::io::Stdin>,
    prompt: &'static str,
    theme: &TerminalTheme,
) -> Result<ReplRead> {
    if let Some(current) = editor.take() {
        let theme = theme.clone();
        let (returned, result) = tokio::task::spawn_blocking(move || {
            let mut editor = current;
            let prompt = DockPrompt::new(
                activity_text(false, None, false, 0),
                prompt == "... ",
                &theme,
            );
            let result = editor.read_line(&prompt);
            (editor, result)
        })
        .await
        .context("line editor task failed")?;
        *editor = Some(returned);
        return match result {
            Ok(Signal::Success(line)) => Ok(ReplRead::Line(line)),
            Ok(Signal::CtrlD) => Ok(ReplRead::Eof),
            Ok(Signal::CtrlC) => Ok(ReplRead::Interrupted),
            Err(error) => Err(anyhow::anyhow!(error)).context("line editor failed"),
        };
    }

    let mut line = String::new();
    let read = input.read_line(&mut line);
    tokio::pin!(read);
    let bytes = tokio::select! {
        result = &mut read => result?,
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for Ctrl-C")?;
            return Ok(ReplRead::Interrupted);
        }
    };
    if bytes == 0 {
        Ok(ReplRead::Eof)
    } else {
        Ok(ReplRead::Line(
            line.trim_end_matches(['\r', '\n']).to_owned(),
        ))
    }
}

fn is_quit_command(input: &str) -> bool {
    matches!(input.trim(), "/quit" | "/exit")
}

fn slash_suggestions() -> Vec<String> {
    vec![
        "/help".into(),
        "/status".into(),
        "/reasoning".into(),
        "/reasoning default".into(),
        "/reasoning low".into(),
        "/reasoning medium".into(),
        "/reasoning xhigh".into(),
        "/decorations".into(),
        "/decorations off".into(),
        "/decorations default".into(),
        "/decorations full".into(),
        "/decorations reset".into(),
        "/tool-output".into(),
        "/tool-output lines".into(),
        "/tool-output chars".into(),
        "/tool-output reset".into(),
        "/tool-frame".into(),
        "/tool-frame on".into(),
        "/tool-frame off".into(),
        "/tool-frame reset".into(),
        "/model-generation".into(),
        "/model-generation details on".into(),
        "/model-generation details off".into(),
        "/model-generation details reset".into(),
        "/tool".into(),
        "/quit".into(),
        "/exit".into(),
    ]
}

struct SlashCompleter;

impl Completer for SlashCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        if !line.starts_with('/') {
            return Vec::new();
        }
        let end = pos.min(line.len());
        let (start, candidates): (usize, Vec<String>) = if let Some(space) = line[..end].find(' ') {
            let command = &line[..space];
            let args = slash_suggestions()
                .into_iter()
                .filter_map(|value| {
                    value
                        .strip_prefix(&format!("{command} "))
                        .map(str::to_owned)
                })
                .collect();
            (space + 1, args)
        } else {
            (0, slash_suggestions())
        };
        let prefix = &line[start..end];
        candidates
            .into_iter()
            .filter(|value| value.starts_with(prefix))
            .map(|value| Suggestion {
                description: command_description(value.split_whitespace().next().unwrap_or(""))
                    .map(str::to_owned),
                value,
                span: Span::new(start, end),
                append_whitespace: true,
                ..Suggestion::default()
            })
            .collect()
    }
}

fn command_description(command: &str) -> Option<&'static str> {
    match command.split_whitespace().next()? {
        "/help" => Some("show commands"),
        "/status" => Some("show current Run"),
        "/reasoning" => Some("set reasoning effort"),
        "/decorations" => Some("set transcript rendering"),
        "/tool-output" => Some("set automatic Tool preview limits"),
        "/tool-frame" => Some("set Tool card framing"),
        "/model-generation" => Some("set Model generation details"),
        "/tool" => Some("inspect the last Run operation"),
        "/quit" | "/exit" => Some("close the REPL"),
        _ => None,
    }
}

/// Remove terminal controls from untrusted model and Tool text while retaining
/// ordinary whitespace and line structure.
fn safe_terminal_text(text: &str) -> String {
    text.chars()
        .filter(|ch| *ch == '\n' || *ch == '\r' || *ch == '\t' || !ch.is_control())
        .collect()
}

fn ansi_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM")
            .map(|term| term != "dumb")
            .unwrap_or(true)
}

fn truecolor_enabled() -> bool {
    if !ansi_enabled() || !std::io::stdout().is_terminal() {
        return false;
    }
    std::env::var("COLORTERM")
        .map(|value| matches!(value.to_ascii_lowercase().as_str(), "truecolor" | "24bit"))
        .unwrap_or(false)
}

#[derive(Clone, Debug)]
struct TerminalStyle {
    sgr: Option<String>,
}

impl TerminalStyle {
    #[cfg(test)]
    fn parse(spec: &str) -> Result<Self> {
        Self::parse_mode(spec, false)
    }

    fn parse_mode(spec: &str, background: bool) -> Result<Self> {
        let mut rest = spec.trim();
        if rest == "none" {
            return Ok(Self { sgr: None });
        }
        let mut codes: Vec<String> = Vec::new();
        loop {
            let mut consumed_attribute = false;
            for (name, code) in [
                ("bold", "1"),
                ("dim", "2"),
                ("italic", "3"),
                ("underline", "4"),
            ] {
                if rest == name {
                    codes.push(code.into());
                    rest = "";
                    consumed_attribute = true;
                    break;
                }
                if let Some(next) = rest
                    .strip_prefix(name)
                    .and_then(|next| next.strip_prefix(char::is_whitespace))
                {
                    codes.push(code.into());
                    rest = next.trim_start();
                    consumed_attribute = true;
                    break;
                }
            }
            if !consumed_attribute {
                break;
            }
        }
        if rest.is_empty() {
            ensure!(!codes.is_empty(), "style cannot be empty");
            return Ok(Self {
                sgr: Some(codes.join(";")),
            });
        }
        let (red, green, blue) = parse_terminal_color(rest)?;
        codes.extend([
            if background { "48" } else { "38" }.into(),
            "2".into(),
            red.to_string(),
            green.to_string(),
            blue.to_string(),
        ]);
        Ok(Self {
            sgr: Some(codes.join(";")),
        })
    }

    fn start(&self, enabled: bool) -> String {
        match (enabled, self.sgr.as_deref()) {
            (true, Some(sgr)) => format!("\x1b[{sgr}m"),
            _ => String::new(),
        }
    }

    fn paint(&self, enabled: bool, text: &str) -> String {
        let start = self.start(enabled);
        if start.is_empty() {
            text.to_owned()
        } else {
            format!("{start}{text}\x1b[0m")
        }
    }
}

#[derive(Clone, Debug)]
struct TerminalTheme {
    enabled: bool,
    styles: BTreeMap<&'static str, TerminalStyle>,
    input_text_spec: String,
    input_background_spec: String,
    input_selected_spec: String,
}

impl TerminalTheme {
    fn from_colors(colors: &agl_core::agent::PresentationColors) -> Result<Self> {
        let specs = [
            ("rule", colors.rule.as_str()),
            ("run", colors.run.as_str()),
            ("run_id", colors.run_id.as_str()),
            ("status_success", colors.status_success.as_str()),
            ("status_failure", colors.status_failure.as_str()),
            ("status_pending", colors.status_pending.as_str()),
            ("operation", colors.operation.as_str()),
            ("tool", colors.tool.as_str()),
            ("ordinal", colors.ordinal.as_str()),
            ("field", colors.field.as_str()),
            ("muted", colors.muted.as_str()),
            ("json_key", colors.json_key.as_str()),
            ("json_string", colors.json_string.as_str()),
            ("json_number", colors.json_number.as_str()),
            ("json_boolean", colors.json_boolean.as_str()),
            ("json_null", colors.json_null.as_str()),
            ("markdown_heading", colors.markdown_heading.as_str()),
            ("markdown_code", colors.markdown_code.as_str()),
            ("markdown_inline_code", colors.markdown_inline_code.as_str()),
            ("markdown_strong", colors.markdown_strong.as_str()),
            ("markdown_emphasis", colors.markdown_emphasis.as_str()),
            ("markdown_link", colors.markdown_link.as_str()),
            ("markdown_quote", colors.markdown_quote.as_str()),
            ("markdown_bullet", colors.markdown_bullet.as_str()),
            ("markdown_rule", colors.markdown_rule.as_str()),
            ("input_rule", colors.input_rule.as_str()),
            ("input_background", colors.input_background.as_str()),
            ("input_prompt", colors.input_prompt.as_str()),
            ("input_hint", colors.input_hint.as_str()),
            ("input_text", colors.input_text.as_str()),
            ("input_activity", colors.input_activity.as_str()),
            ("input_selected", colors.input_selected.as_str()),
        ];
        let styles = specs
            .into_iter()
            .map(|(role, spec)| {
                TerminalStyle::parse_mode(spec, role == "input_background")
                    .map(|style| (role, style))
                    .with_context(|| format!("invalid presentation.colors.{role}"))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok(Self {
            enabled: truecolor_enabled(),
            styles,
            input_text_spec: colors.input_text.clone(),
            input_background_spec: colors.input_background.clone(),
            input_selected_spec: colors.input_selected.clone(),
        })
    }

    fn start(&self, role: &'static str) -> String {
        self.styles
            .get(role)
            .expect("known presentation color role")
            .start(self.enabled)
    }

    fn paint(&self, role: &'static str, text: &str) -> String {
        self.styles
            .get(role)
            .expect("known presentation color role")
            .paint(self.enabled, text)
    }

    fn reset(&self) -> &'static str {
        if self.enabled { "\x1b[0m" } else { "" }
    }
}

fn input_highlighter(theme: &TerminalTheme) -> Result<Box<dyn Highlighter>> {
    let style = parse_ansi_style(&theme.input_text_spec, Some(&theme.input_background_spec))?;
    Ok(Box::new(InputHighlighter { style }))
}

fn input_selection_style(theme: &TerminalTheme) -> Result<AnsiStyle> {
    parse_ansi_style(
        &theme.input_selected_spec,
        Some(&theme.input_background_spec),
    )
}

fn parse_ansi_style(foreground: &str, background: Option<&str>) -> Result<AnsiStyle> {
    type ParsedAnsiStyle = (AnsiStyle, Option<(u8, u8, u8)>);

    fn parse_component(spec: &str) -> Result<ParsedAnsiStyle> {
        let mut rest = spec.trim();
        let mut style = AnsiStyle::new();
        if rest == "none" {
            return Ok((style, None));
        }
        loop {
            let mut consumed = false;
            for name in ["bold", "dim", "italic", "underline"] {
                if rest == name {
                    style = match name {
                        "bold" => style.bold(),
                        "dim" => style.dimmed(),
                        "italic" => style.italic(),
                        _ => style.underline(),
                    };
                    rest = "";
                    consumed = true;
                    break;
                }
                if let Some(next) = rest
                    .strip_prefix(name)
                    .and_then(|next| next.strip_prefix(char::is_whitespace))
                {
                    style = match name {
                        "bold" => style.bold(),
                        "dim" => style.dimmed(),
                        "italic" => style.italic(),
                        _ => style.underline(),
                    };
                    rest = next.trim_start();
                    consumed = true;
                    break;
                }
            }
            if !consumed {
                break;
            }
        }
        if rest.is_empty() {
            ensure!(spec.trim() != "", "style cannot be empty");
            return Ok((style, None));
        }
        Ok((style, Some(parse_terminal_color(rest)?)))
    }

    let (mut style, foreground) = parse_component(foreground)?;
    if let Some(color) = foreground {
        style = style.fg(AnsiColor::Rgb(color.0, color.1, color.2));
    }
    if let Some(background) = background {
        let (_, color) = parse_component(background)?;
        if let Some(color) = color {
            style = style.on(AnsiColor::Rgb(color.0, color.1, color.2));
        }
    }
    Ok(style)
}

fn parse_terminal_color(value: &str) -> Result<(u8, u8, u8)> {
    if let Some(hex) = value.strip_prefix('#') {
        ensure!(hex.len() == 6, "hex color must use #RRGGBB");
        return Ok((
            u8::from_str_radix(&hex[0..2], 16)?,
            u8::from_str_radix(&hex[2..4], 16)?,
            u8::from_str_radix(&hex[4..6], 16)?,
        ));
    }
    let payload = value
        .strip_prefix("oklch(")
        .and_then(|value| value.strip_suffix(')'))
        .context("color must use #RRGGBB or oklch(L C H)")?;
    let values = payload
        .split_whitespace()
        .map(|value| value.parse::<f32>())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(values.len() == 3, "OKLCH requires three values");
    ensure!(
        values.iter().all(|value| value.is_finite())
            && (0.0..=1.0).contains(&values[0])
            && values[1] >= 0.0,
        "OKLCH lightness must be 0..1 and chroma non-negative"
    );
    Ok(oklch_to_srgb(values[0], values[1], values[2]))
}

fn oklch_to_srgb(lightness: f32, chroma: f32, hue: f32) -> (u8, u8, u8) {
    let hue = hue.to_radians();
    let a = chroma * hue.cos();
    let b = chroma * hue.sin();
    let l_ = lightness + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = lightness - 0.105_561_35 * a - 0.063_854_17 * b;
    let s_ = lightness - 0.089_484_18 * a - 1.291_485_5 * b;
    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;
    let red = 4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s;
    let green = -1.268_438 * l + 2.609_757_4 * m - 0.341_319_4 * s;
    let blue = -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s;
    fn encode(value: f32) -> u8 {
        let value = if value <= 0.003_130_8 {
            12.92 * value
        } else {
            1.055 * value.clamp(0.0, 1.0).powf(1.0 / 2.4) - 0.055
        };
        (value.clamp(0.0, 1.0) * 255.0).round() as u8
    }
    (encode(red), encode(green), encode(blue))
}

fn answer_rule(theme: &TerminalTheme) -> String {
    answer_rule_for_width(theme, answer_width())
}

fn answer_footer(theme: &TerminalTheme, elapsed: Duration) -> String {
    answer_footer_for_width(theme, elapsed, answer_width())
}

fn answer_rule_for_width(theme: &TerminalTheme, width: usize) -> String {
    theme.paint("input_rule", &"─".repeat(width.max(1)))
}

fn answer_footer_for_width(theme: &TerminalTheme, elapsed: Duration, width: usize) -> String {
    let prefix = format!("─ Worked for {} ", format_worked_for(elapsed));
    let fill = width.saturating_sub(prefix.chars().count());
    theme.paint("input_rule", &format!("{prefix}{}", "─".repeat(fill)))
}

fn answer_width() -> usize {
    terminal::size()
        .map(|(columns, _)| usize::from(columns))
        .ok()
        .filter(|width| *width > 0)
        .unwrap_or(FALLBACK_ANSWER_WIDTH)
}

fn render_answer_block(text: &str, elapsed: Duration, theme: &TerminalTheme) {
    if send_tty(TtyMessage::AnswerFinal {
        text: text.to_owned(),
        elapsed,
    }) {
        return;
    }
    begin_external_block();
    let rendered = markdown_to_terminal(text, theme);
    // Transcript cards share the terminal width. The previous content-sized
    // rule made a later repaint wrap the already styled body and produced an
    // extra trailing `──` line.
    let width = answer_width();
    println!();
    println!("{}", answer_rule_for_width(theme, width));
    println!();
    println!("{}", rendered);
    println!("{}", answer_footer_for_width(theme, elapsed, width));
    println!();
}

fn terminal_visible_width(text: &str) -> usize {
    text.split('\n')
        .map(|line| {
            let mut width = 0;
            let mut cursor = 0;
            while cursor < line.len() {
                if line.as_bytes()[cursor] == 0x1b {
                    cursor = ansi_escape_end(line, cursor);
                    continue;
                }
                let ch = line[cursor..]
                    .chars()
                    .next()
                    .expect("cursor always points at a character boundary");
                width += UnicodeWidthChar::width(ch).unwrap_or(0);
                cursor += ch.len_utf8();
            }
            width
        })
        .max()
        .unwrap_or(0)
}

/// Return the end of one ANSI/ECMA-48 control sequence. In particular, the
/// `[` byte introduces a CSI sequence and is not its final byte; treating it
/// as the terminator leaves truecolor parameters counted as visible text.
fn ansi_escape_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut cursor = (start + 1).min(bytes.len());
    if cursor >= bytes.len() {
        return cursor;
    }
    match bytes[cursor] {
        b'[' => {
            cursor += 1;
            while cursor < bytes.len() {
                let byte = bytes[cursor];
                cursor += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
        }
        b']' => {
            cursor += 1;
            while cursor < bytes.len() {
                if bytes[cursor] == 0x07 {
                    cursor += 1;
                    break;
                }
                if bytes[cursor] == 0x1b && bytes.get(cursor + 1) == Some(&b'\\') {
                    cursor += 2;
                    break;
                }
                cursor += 1;
            }
        }
        _ => cursor += 1,
    }
    cursor
}

fn format_worked_for(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds == 0 {
        return "<1s".to_owned();
    }
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{}m {}s", minutes, seconds % 60);
    }
    format!("{}h {}m", minutes / 60, minutes % 60)
}

fn elapsed_between_ms(started_at_ms: i64, completed_at_ms: i64) -> Duration {
    let elapsed_ms = completed_at_ms.saturating_sub(started_at_ms).max(0) as u64;
    Duration::from_millis(elapsed_ms)
}

fn json_to_terminal(value: &serde_json::Value, theme: &TerminalTheme) -> String {
    fn render(value: &serde_json::Value, depth: usize, output: &mut String, theme: &TerminalTheme) {
        let indent = |level: usize| "  ".repeat(level);
        match value {
            serde_json::Value::Object(entries) => {
                output.push_str(&theme.paint("muted", "{"));
                if !entries.is_empty() {
                    output.push('\n');
                }
                for (index, (key, value)) in entries.iter().enumerate() {
                    output.push_str(&indent(depth + 1));
                    let key = serde_json::to_string(key).unwrap_or_else(|_| "\"?\"".into());
                    output.push_str(&theme.paint("json_key", &key));
                    output.push_str(&theme.paint("muted", ": "));
                    render(value, depth + 1, output, theme);
                    if index + 1 != entries.len() {
                        output.push_str(&theme.paint("muted", ","));
                    }
                    output.push('\n');
                }
                if !entries.is_empty() {
                    output.push_str(&indent(depth));
                }
                output.push_str(&theme.paint("muted", "}"));
            }
            serde_json::Value::Array(items) => {
                output.push_str(&theme.paint("muted", "["));
                if !items.is_empty() {
                    output.push('\n');
                }
                for (index, item) in items.iter().enumerate() {
                    output.push_str(&indent(depth + 1));
                    render(item, depth + 1, output, theme);
                    if index + 1 != items.len() {
                        output.push_str(&theme.paint("muted", ","));
                    }
                    output.push('\n');
                }
                if !items.is_empty() {
                    output.push_str(&indent(depth));
                }
                output.push_str(&theme.paint("muted", "]"));
            }
            serde_json::Value::String(value) => {
                let value = serde_json::to_string(value).unwrap_or_else(|_| "\"?\"".into());
                output.push_str(&theme.paint("json_string", &safe_terminal_text(&value)));
            }
            serde_json::Value::Number(value) => {
                output.push_str(&theme.paint("json_number", &value.to_string()))
            }
            serde_json::Value::Bool(value) => {
                output.push_str(&theme.paint("json_boolean", &value.to_string()))
            }
            serde_json::Value::Null => output.push_str(&theme.paint("json_null", "null")),
        }
    }

    let mut output = String::new();
    render(value, 0, &mut output, theme);
    output
}

async fn view_tool(
    client: &AgentClient,
    run: Option<AgentRunId>,
    args: &str,
    theme: &TerminalTheme,
) -> Result<()> {
    let run = run.context("нет активного Run; сначала выполните сообщение")?;
    let mut words = args.split_whitespace();
    let ordinal: u32 = words
        .next()
        .context("использование: /tool N [input|result]")?
        .parse()
        .context("ordinal операции должен быть числом")?;
    let mode = words.next().unwrap_or("result");
    ensure!(
        matches!(mode, "input" | "result"),
        "использование: /tool N [input|result]"
    );
    ensure!(
        words.next().is_none(),
        "использование: /tool N [input|result]"
    );
    let key = agl_core::agent::AgentOperationKey {
        run_id: run,
        ordinal: std::num::NonZeroU32::new(ordinal).context("ordinal должен быть больше нуля")?,
    };
    let operation = client
        .operation_view(key)
        .await
        .context("не удалось получить операцию")?;
    if mode == "input" {
        show_full_text(
            &safe_terminal_text(&serde_json::to_string_pretty(&operation.request)?),
            theme,
        );
        return Ok(());
    }
    if let Some(result) = operation.result {
        if let agl_core::agent::AgentOperationResult::Tool(result) = result {
            let (tool_id, input) = match &operation.request {
                agl_core::agent::AgentOperationRequest::Tool(tool) => {
                    (Some(tool.tool_id.as_str()), Some(&tool.input))
                }
                _ => (None, None),
            };
            show_full_text(
                &format_tool_content_for(tool_id, result.content.as_text(), input, theme),
                theme,
            );
        } else {
            println!(
                "{}",
                safe_terminal_text(&serde_json::to_string_pretty(&result)?)
            );
        }
    } else if let Some(failure) = operation.failure {
        println!(
            "operation failed: {}",
            safe_terminal_text(&serde_json::to_string_pretty(&failure)?)
        );
    } else {
        println!(
            "operation state: {:?} (result not available)",
            operation.state
        );
    }
    Ok(())
}

fn format_tool_content_for(
    tool_id: Option<&str>,
    text: &str,
    input: Option<&serde_json::Value>,
    theme: &TerminalTheme,
) -> String {
    let value = serde_json::from_str::<serde_json::Value>(text);
    if let Ok(value) = &value {
        match tool_id.unwrap_or("") {
            "agentlibre.execution:command.exec" => {
                let state = value
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let outcome = value
                    .get("outcome")
                    .map(format_execution_outcome)
                    .unwrap_or_else(|| "pending".to_owned());
                let mut result = format!("state: {state}\noutcome: {outcome}");
                for name in ["stdout", "stderr"] {
                    if let Some(text) = value
                        .get(name)
                        .and_then(|v| v.as_str())
                        .filter(|v| !v.is_empty())
                    {
                        result.push_str(&format!("\n{name}:\n{}", safe_terminal_text(text)));
                    }
                }
                if value
                    .get("truncated")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
                {
                    result.push_str("\noutput truncated: true");
                }
                return result;
            }
            "agentlibre.builtins:fs_read" => {
                let path = value.get("path").and_then(|v| v.as_str()).unwrap_or("-");
                let mut result = format!("path: {path}\n");
                if let Some(lines) = value.get("lines").and_then(|v| v.as_array()) {
                    for line in lines {
                        let number = line.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
                        let text = line.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        result.push_str(&format!("{number:>6}  {}\n", safe_terminal_text(text)));
                    }
                }
                return result.trim_end().to_owned();
            }
            "agentlibre.builtins:fs_apply_patch" => {
                let status = value
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let mut result = format!("status: {status}");
                if let Some(input) = input {
                    result.push_str(&format!(
                        "\nrequest: {}",
                        safe_terminal_text(&input.to_string())
                    ));
                }
                return result;
            }
            "agentlibre.searxng:search" => {
                if let Some(results) = value.get("results").and_then(|v| v.as_array()) {
                    return results
                        .iter()
                        .map(|item| {
                            format!(
                                "{}\n{}",
                                item.get("title").and_then(|v| v.as_str()).unwrap_or("-"),
                                item.get("url").and_then(|v| v.as_str()).unwrap_or("-")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");
                }
            }
            _ => {}
        }
    }
    match value {
        Ok(value) => markdown_to_terminal(
            &serde_json::to_string_pretty(&value).unwrap_or_else(|_| safe_terminal_text(text)),
            theme,
        ),
        Err(_) => markdown_to_terminal(text, theme),
    }
}

fn format_execution_outcome(value: &serde_json::Value) -> String {
    if value.is_null() {
        return "pending".to_owned();
    }
    let Some(kind) = value.get("type").and_then(|value| value.as_str()) else {
        return safe_terminal_text(&value.to_string());
    };
    match kind {
        "exit" => value
            .get("code")
            .and_then(|value| value.as_i64())
            .map(|code| format!("exit {code}"))
            .unwrap_or_else(|| "exit".to_owned()),
        "signal" => value
            .get("signal")
            .and_then(|value| value.as_i64())
            .map(|signal| format!("signal {signal}"))
            .unwrap_or_else(|| "signal".to_owned()),
        "timed_out" => "timed out".to_owned(),
        "terminated" => "terminated".to_owned(),
        "unknown_after_service_restart" => "unknown after service restart".to_owned(),
        other => safe_terminal_text(other),
    }
}

fn preview_tool_output(
    text: &str,
    limits: agl_core::agent::ToolOutputPresentation,
    ordinal: u32,
    mode: &str,
    theme: &TerminalTheme,
) -> String {
    let mut output = String::new();
    let mut chars = 0_u32;
    let mut lines = 1_u32;
    let mut truncated = false;
    let mut iter = text.chars().peekable();
    while let Some(ch) = iter.next() {
        if ch == '\x1b' && iter.peek() == Some(&'[') {
            output.push(ch);
            for control in iter.by_ref() {
                output.push(control);
                if control.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if chars == limits.chars || (ch == '\n' && lines == limits.lines) {
            truncated = true;
            break;
        }
        output.push(ch);
        chars += 1;
        if ch == '\n' {
            lines += 1;
        }
    }
    if iter.next().is_some() {
        truncated = true;
    }
    if truncated {
        output.push_str(theme.reset());
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&theme.paint(
            "muted",
            &format!(
                "… limited to {} lines / {} chars; /tool {ordinal} {mode}",
                limits.lines, limits.chars
            ),
        ));
    }
    output
}

fn frame_tool_output(text: &str, theme: &TerminalTheme) -> String {
    let gutter = theme.paint("input_rule", "  │ ");
    text.split('\n')
        .map(|line| format!("{gutter}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn show_full_text(text: &str, theme: &TerminalTheme) {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        println!("{text}");
        return;
    }
    use crossterm::{event, execute, terminal};
    let lines: Vec<_> = text.lines().collect();
    let page = terminal::size()
        .map(|(_, height)| height.saturating_sub(2) as usize)
        .unwrap_or(20)
        .max(1);
    let _ = terminal::enable_raw_mode();
    let mut offset = 0;
    while offset < lines.len() {
        print!("\x1b[2J\x1b[H");
        for line in lines.iter().skip(offset).take(page) {
            println!("{line}");
        }
        println!(
            "{}",
            theme.paint(
                "muted",
                &format!(
                    "-- {}/{} lines; Space/PageDown next, PageUp previous, q/Esc exit --",
                    (offset + page).min(lines.len()),
                    lines.len()
                )
            )
        );
        let Ok(event::Event::Key(key)) = event::read() else {
            break;
        };
        match key.code {
            event::KeyCode::Char('q') | event::KeyCode::Esc => break,
            event::KeyCode::Char(' ') | event::KeyCode::PageDown | event::KeyCode::Down => {
                offset = (offset + page).min(lines.len())
            }
            event::KeyCode::PageUp | event::KeyCode::Up => offset = offset.saturating_sub(page),
            _ => {}
        }
    }
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        std::io::stdout(),
        terminal::Clear(terminal::ClearType::FromCursorDown)
    );
}

fn markdown_to_terminal(markdown: &str, theme: &TerminalTheme) -> String {
    let mut output = String::new();
    let mut line_start = true;
    let mut code = false;
    let mut link_destinations = Vec::new();
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    for event in MarkdownParser::new_ext(markdown, options) {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                if !line_start {
                    output.push('\n');
                }
                output.push_str(&theme.start("markdown_heading"));
            }
            Event::End(TagEnd::Heading(_)) => {
                output.push_str(theme.reset());
                output.push_str("\n\n");
                line_start = true;
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                output.push_str("\n\n");
                line_start = true;
            }
            Event::Start(Tag::Item) => {
                if !line_start {
                    output.push('\n');
                }
                output.push_str(&theme.paint("markdown_bullet", "• "));
                line_start = false;
            }
            Event::End(TagEnd::Item) => {
                output.push('\n');
                line_start = true;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                if !line_start {
                    output.push('\n');
                }
                output.push_str(&theme.paint("markdown_quote", "│ "));
                line_start = false;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                output.push('\n');
                line_start = true;
            }
            Event::Start(Tag::CodeBlock(_)) => {
                if !line_start {
                    output.push('\n');
                }
                output.push_str(&theme.start("markdown_code"));
                code = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                output.push_str(theme.reset());
                output.push('\n');
                line_start = true;
                code = false;
            }
            Event::Code(text) => {
                output.push_str(&theme.paint("markdown_inline_code", &safe_terminal_text(&text)));
            }
            Event::Text(text) => {
                output.push_str(&safe_terminal_text(&text));
                line_start = false;
            }
            Event::SoftBreak | Event::HardBreak => {
                output.push('\n');
                line_start = true;
            }
            Event::Rule => {
                output.push_str(&theme.paint("input_rule", "────────────────\n"));
                line_start = true;
            }
            Event::Start(Tag::Strong) => {
                output.push_str(&theme.start("markdown_strong"));
            }
            Event::End(TagEnd::Strong) => {
                output.push_str(theme.reset());
            }
            Event::Start(Tag::Emphasis) => {
                output.push_str(&theme.start("markdown_emphasis"));
            }
            Event::End(TagEnd::Emphasis) => {
                output.push_str(theme.reset());
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                link_destinations.push(safe_terminal_text(&dest_url));
                output.push_str(&theme.start("markdown_link"));
                output.push('[');
            }
            Event::End(TagEnd::Link) => {
                output.push(']');
                if let Some(destination) = link_destinations.pop()
                    && !destination.is_empty()
                {
                    output.push_str(" (");
                    output.push_str(&destination);
                    output.push(')');
                }
                output.push_str(theme.reset());
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                output.push_str(&safe_terminal_text(&text))
            }
            _ => {}
        }
        if code && output.ends_with('\n') {
            line_start = true;
        }
    }
    while output.ends_with("\n\n") {
        output.pop();
    }
    output
}

fn reasoning_command(
    input: &str,
    selected: &mut Option<agl_core::agent::ReasoningEffort>,
) -> Result<bool, String> {
    let mut words = input.split_whitespace();
    if words.next() != Some("/reasoning") {
        return Ok(false);
    }
    let value = words.next();
    if words.next().is_some() {
        return Err("usage: /reasoning [default|low|medium|xhigh]".into());
    }
    match value {
        None => {}
        Some("default") => *selected = None,
        Some(value) => *selected = Some(parse_reasoning(value)?),
    }
    Ok(true)
}

fn decorations_command(
    input: &str,
    override_value: &mut Option<Decorations>,
) -> Result<bool, String> {
    let mut words = input.split_whitespace();
    if words.next() != Some("/decorations") {
        return Ok(false);
    }
    let value = words.next();
    if words.next().is_some() {
        return Err("usage: /decorations [off|default|full|reset]".into());
    }
    match value {
        None => {}
        Some("off") => *override_value = Some(Decorations::Off),
        Some("default") => *override_value = Some(Decorations::Default),
        Some("full") => *override_value = Some(Decorations::Full),
        Some("reset") => *override_value = None,
        Some(_) => return Err("usage: /decorations [off|default|full|reset]".into()),
    }
    Ok(true)
}

fn tool_output_command(
    input: &str,
    current: &mut agl_core::agent::ToolOutputPresentation,
    defaults: agl_core::agent::ToolOutputPresentation,
) -> Result<bool, String> {
    let mut words = input.split_whitespace();
    if words.next() != Some("/tool-output") {
        return Ok(false);
    }
    let field = words.next();
    let value = words.next();
    if words.next().is_some() {
        return Err("usage: /tool-output [lines N|chars N|reset]".into());
    }
    match (field, value) {
        (None, None) => {}
        (Some("reset"), None) => *current = defaults,
        (Some("lines"), Some(value)) => {
            current.lines = parse_tool_output_limit("lines", value)?;
        }
        (Some("chars"), Some(value)) => {
            current.chars = parse_tool_output_limit("chars", value)?;
        }
        _ => return Err("usage: /tool-output [lines N|chars N|reset]".into()),
    }
    Ok(true)
}

fn tool_frame_command(input: &str, current: &mut bool, default: bool) -> Result<bool, String> {
    let mut words = input.split_whitespace();
    if words.next() != Some("/tool-frame") {
        return Ok(false);
    }
    let value = words.next();
    if words.next().is_some() {
        return Err("usage: /tool-frame [on|off|reset]".into());
    }
    match value {
        None => {}
        Some("on") => *current = true,
        Some("off") => *current = false,
        Some("reset") => *current = default,
        Some(_) => return Err("usage: /tool-frame [on|off|reset]".into()),
    }
    Ok(true)
}

fn parse_tool_output_limit(field: &str, value: &str) -> Result<u32, String> {
    let value = value
        .parse::<u32>()
        .map_err(|_| format!("tool-output {field} must be a positive integer"))?;
    if value == 0 {
        return Err(format!("tool-output {field} must be a positive integer"));
    }
    Ok(value)
}

fn model_generation_command(
    input: &str,
    current: &mut bool,
    default: bool,
) -> Result<bool, String> {
    let mut words = input.split_whitespace();
    if words.next() != Some("/model-generation") {
        return Ok(false);
    }
    let field = words.next();
    let value = words.next();
    if words.next().is_some() {
        return Err("usage: /model-generation [details on|off|reset]".into());
    }
    match (field, value) {
        (None, None) => {}
        (Some("details"), Some("on")) => *current = true,
        (Some("details"), Some("off")) => *current = false,
        (Some("details"), Some("reset")) => *current = default,
        _ => return Err("usage: /model-generation [details on|off|reset]".into()),
    }
    Ok(true)
}

fn decorations_label(value: Decorations) -> &'static str {
    match value {
        Decorations::Off => "off",
        Decorations::Default => "default",
        Decorations::Full => "full",
    }
}

fn status_role(status: AgentRunStatus) -> &'static str {
    match status {
        AgentRunStatus::Completed => "status_success",
        AgentRunStatus::Failed | AgentRunStatus::Cancelled => "status_failure",
        AgentRunStatus::Pending | AgentRunStatus::Running => "status_pending",
    }
}

fn operation_status_role(status: &str) -> &'static str {
    match status {
        "Succeeded" => "status_success",
        "Failed" | "Cancelled" => "status_failure",
        _ => "status_pending",
    }
}

fn reasoning_label(
    base: &agl_core::agent::ReasoningSelection,
    selected: Option<agl_core::agent::ReasoningEffort>,
) -> &'static str {
    use agl_core::agent::{ReasoningEffort, ReasoningSelection};
    match selected.or(match base {
        ReasoningSelection::Enabled { effort, .. } => *effort,
        ReasoningSelection::Disabled => None,
    }) {
        Some(ReasoningEffort::Low) => "low",
        Some(ReasoningEffort::Medium) => "medium",
        Some(ReasoningEffort::Xhigh) => "xhigh",
        None if matches!(base, ReasoningSelection::Disabled) => "disabled",
        None => "model default",
    }
}

#[allow(clippy::too_many_arguments)]
async fn foreground_turn(
    client: &AgentClient,
    conversation_id: ConversationId,
    prompt: String,
    interactive: bool,
    reasoning: Option<agl_core::agent::ReasoningEffort>,
    decorations: Decorations,
    tool_output: agl_core::agent::ToolOutputPresentation,
    tool_frame: bool,
    theme: &TerminalTheme,
    model_generation_details: bool,
    active_id: Option<&Arc<Mutex<Option<AgentRunId>>>>,
) -> Result<agl_core::agent::AgentRunView> {
    let started = Instant::now();
    let message_id = MessageId::generate();
    let run_id = client
        .start_run(AgentRunSpec {
            reasoning,
            origin: AgentRunOrigin::User {
                conversation_id,
                message_id: message_id.clone(),
            },
            input: Content::text(prompt)?,
        })
        .await?;
    if let Some(active_id) = active_id {
        *active_id.lock().expect("active run lock poisoned") = Some(run_id);
    }
    if interactive && decorations != Decorations::Off {
        begin_external_block();
        println!(
            "{} {}",
            theme.paint("run", "RUN"),
            theme.paint("run_id", &run_id.to_string())
        );
    } else if interactive {
        eprintln!("run {run_id}");
    }
    let (view, streamed) = stream_until_terminal(
        client,
        run_id,
        interactive,
        decorations,
        tool_output,
        tool_frame,
        theme,
        model_generation_details,
        started,
    )
    .await?;
    if view.status == AgentRunStatus::Completed {
        let page = client
            .messages(conversation_id, Some(message_id), 1_000)
            .await?;
        if let Some(message) = page.messages.into_iter().find(|message| {
            message.run_id == Some(run_id) && message.role == MessageRole::Assistant
        }) {
            let durable = message.content.into_text();
            if decorations != Decorations::Off && interactive {
                render_answer_block(&durable, started.elapsed(), theme);
            } else if !interactive || streamed != durable {
                println!("{}", safe_terminal_text(&durable));
            }
        }
    } else if interactive && !streamed.is_empty() {
        println!("\n[ответ прерван]");
    }
    if interactive && decorations != Decorations::Off {
        let status = format!("{:?}", view.status);
        println!(
            "{} {}",
            theme.paint("run", "RUN"),
            theme.paint(status_role(view.status), &status)
        );
    }
    if interactive && decorations == Decorations::Full {
        let status = format!("{:?}", view.status);
        println!(
            "{} {}  {}  {}",
            theme.paint("run", "RUN SUMMARY"),
            theme.paint("run_id", &run_id.to_string()),
            theme.paint(status_role(view.status), &status),
            theme.paint("muted", &format!("{} ms", started.elapsed().as_millis()))
        );
    } else if interactive && decorations == Decorations::Off {
        eprintln!(
            "run_id={run_id} status={:?} elapsed_ms={}",
            view.status,
            started.elapsed().as_millis()
        );
    }
    Ok(view)
}

#[allow(clippy::too_many_arguments)]
async fn stream_until_terminal(
    client: &AgentClient,
    run_id: AgentRunId,
    render: bool,
    decorations: Decorations,
    tool_output: agl_core::agent::ToolOutputPresentation,
    tool_frame: bool,
    theme: &TerminalTheme,
    model_generation_details: bool,
    started: Instant,
) -> Result<(agl_core::agent::AgentRunView, String)> {
    let decorated = decorations != Decorations::Off;
    let mut subscription = client.subscribe(run_id).await?;
    if subscription.initial_view().status.is_terminal() {
        return Ok((subscription.initial_view().clone(), String::new()));
    }
    let mut streamed = String::new();
    let mut streaming_region_active = false;
    let mut operation_started_at_ms = BTreeMap::new();
    let mut cancellation_requested = false;
    loop {
        let frame = if cancellation_requested {
            subscription.next().await?
        } else {
            tokio::select! {
                frame = subscription.next() => frame?,
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for Ctrl-C")?;
                    if let Err(error) = client.cancel(run_id).await {
                        let view = client.run_view(run_id).await.with_context(|| {
                            format!("cancellation of AgentRun {run_id} failed ({error}); state is unknown")
                        })?;
                        if view.status.is_terminal() {
                            return Ok((view, streamed));
                        }
                        return Err(error).with_context(|| {
                            format!("could not confirm cancellation for AgentRun {run_id}; it is still active")
                        });
                    }
                    cancellation_requested = true;
                    finish_streaming_region(
                        render,
                        decorated,
                        &mut streaming_region_active,
                        theme,
                        started.elapsed(),
                    )?;
                    if render && decorated {
                        println!("cancellation requested");
                    } else if render {
                        eprintln!("cancellation requested");
                    }
                    continue;
                }
            }
        };
        match frame {
            AgentSubscriptionFrame::Progress { progress } => {
                render_progress(
                    progress,
                    render,
                    decorated,
                    &mut streamed,
                    &mut streaming_region_active,
                    started,
                    theme,
                )?;
            }
            AgentSubscriptionFrame::Event { event } => {
                finish_streaming_region(
                    render,
                    decorated,
                    &mut streaming_region_active,
                    theme,
                    started.elapsed(),
                )?;
                let terminal = event_ends_run(&event);
                render_event(
                    client,
                    &event,
                    render,
                    decorated,
                    tool_output,
                    tool_frame,
                    &mut operation_started_at_ms,
                    theme,
                    model_generation_details,
                )
                .await;
                if terminal {
                    return Ok((client.run_view(run_id).await?, streamed));
                }
            }
            AgentSubscriptionFrame::Lagged { .. } => {
                finish_streaming_region(
                    render,
                    decorated,
                    &mut streaming_region_active,
                    theme,
                    started.elapsed(),
                )?;
                let snapshot = subscription.resynchronize(client).await?;
                for event in &snapshot.events {
                    render_event(
                        client,
                        event,
                        render,
                        decorated,
                        tool_output,
                        tool_frame,
                        &mut operation_started_at_ms,
                        theme,
                        model_generation_details,
                    )
                    .await;
                }
                if snapshot.view.status.is_terminal() {
                    return Ok((snapshot.view, streamed));
                }
            }
            AgentSubscriptionFrame::Ended { run_view, .. } => {
                finish_streaming_region(
                    render,
                    decorated,
                    &mut streaming_region_active,
                    theme,
                    started.elapsed(),
                )?;
                return Ok((run_view, streamed));
            }
            AgentSubscriptionFrame::Subscribed { .. } => unreachable!(),
        }
    }
}

fn render_progress(
    progress: AgentProgress,
    render: bool,
    full: bool,
    streamed: &mut String,
    _streaming_region_active: &mut bool,
    _started: Instant,
    _theme: &TerminalTheme,
) -> Result<()> {
    match progress {
        AgentProgress::ModelOutputDelta { content, .. } => {
            let delta = content.as_text();
            streamed.push_str(delta);
            if render && full {
                let _ = send_tty(TtyMessage::ModelDelta(delta.to_owned()));
            }
        }
        AgentProgress::OperationStatus { operation, status } if render => {
            finish_streaming_region(
                render,
                full,
                _streaming_region_active,
                _theme,
                _started.elapsed(),
            )?;
            match status {
                AgentProgressStatus::Running if full => {}
                AgentProgressStatus::Running => {
                    begin_external_block();
                    eprintln!("operation {} running", operation.ordinal)
                }
                AgentProgressStatus::Terminal(_) if full => {}
                AgentProgressStatus::Terminal(status) => {
                    begin_external_block();
                    eprintln!("operation {} {status:?}", operation.ordinal)
                }
            }
        }
        AgentProgress::OperationStatus { .. } => {}
    }
    Ok(())
}

fn finish_streaming_region(
    render: bool,
    full: bool,
    active: &mut bool,
    theme: &TerminalTheme,
    elapsed: Duration,
) -> Result<()> {
    if render && full && *active {
        println!();
        println!("{}", answer_footer(theme, elapsed));
        println!();
        std::io::stdout().flush()?;
        *active = false;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn render_event(
    client: &AgentClient,
    event: &AgentEvent,
    render: bool,
    full: bool,
    tool_output: agl_core::agent::ToolOutputPresentation,
    tool_frame: bool,
    operation_started_at_ms: &mut BTreeMap<agl_core::agent::AgentOperationKey, i64>,
    theme: &TerminalTheme,
    model_generation_details: bool,
) {
    if !render {
        return;
    }
    match &event.data {
        AgentEventData::OperationCreated {
            key: _,
            kind: AgentOperationKind::Tool,
            tool_id: Some(tool_id),
            ..
        } => {
            let _ = send_tty(TtyMessage::Activity(format!("Running {tool_id}")));
            if !full
                && let AgentEventData::OperationCreated {
                    key,
                    tool_id: Some(tool_id),
                    ..
                } = &event.data
            {
                eprintln!("tool {} ({})", tool_id, key.ordinal);
            }
        }
        AgentEventData::OperationCreated {
            kind: AgentOperationKind::ModelGeneration,
            ..
        } => {
            let _ = send_tty(TtyMessage::Activity("Generating".to_owned()));
        }
        AgentEventData::OperationCreated {
            kind: AgentOperationKind::Compaction,
            ..
        } => {
            let _ = send_tty(TtyMessage::Activity("Compacting".to_owned()));
        }
        AgentEventData::OperationCreated { .. } if full => {
            // Details are emitted once, when the operation reaches a terminal state.
        }
        AgentEventData::OperationStarted { key, .. } => {
            operation_started_at_ms
                .entry(key.clone())
                .or_insert(event.committed_at_ms);
            // OperationCreated establishes the activity (Tool or model). Do
            // not blindly turn a running Tool into Generating here.
        }
        AgentEventData::OperationCompleted { key, status, .. } if full => {
            let tool_elapsed = operation_started_at_ms
                .remove(key)
                .map(|started| elapsed_between_ms(started, event.committed_at_ms));
            render_operation_detail(
                client,
                key,
                None,
                None,
                tool_output,
                tool_frame,
                tool_elapsed,
                theme,
                model_generation_details,
            )
            .await;
            let _ = status;
        }
        AgentEventData::OperationRetryScheduled {
            key,
            delivery_attempt,
            ..
        } => {
            let _ = send_tty(TtyMessage::Activity("Retrying".to_owned()));
            let line = format!(
                "operation {} retry scheduled after attempt {}",
                key.ordinal, delivery_attempt
            );
            begin_external_block();
            if full {
                println!("{line}");
            } else {
                eprintln!("{line}");
            }
        }
        AgentEventData::OperationOutcomeUnknown { key, .. } => {
            begin_external_block();
            if full {
                println!("operation {} outcome unknown", key.ordinal)
            } else {
                eprintln!("operation {} outcome unknown", key.ordinal)
            }
        }
        AgentEventData::RunStatusChanged { to, .. } if to.is_terminal() => {
            let _ = to;
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
async fn render_operation_detail(
    client: &AgentClient,
    key: &agl_core::agent::AgentOperationKey,
    kind: Option<AgentOperationKind>,
    delivery: Option<agl_core::agent::DeliveryClass>,
    tool_output: agl_core::agent::ToolOutputPresentation,
    tool_frame: bool,
    tool_elapsed: Option<Duration>,
    theme: &TerminalTheme,
    model_generation_details: bool,
) {
    match client.operation_view(key.clone()).await {
        Ok(operation) => match operation_request_decoration(&operation.request) {
            Ok(request) => {
                begin_external_block();
                if let agl_core::agent::AgentOperationRequest::Tool(tool) = &operation.request {
                    if tool_frame {
                        println!();
                        println!("{}", answer_rule(theme));
                        println!();
                    }
                    let state = format!("{:?}", operation.state);
                    println!(
                        "{} {}  {}  {}",
                        theme.paint("tool", "TOOL"),
                        theme.paint("tool", tool.tool_id.as_str()),
                        theme.paint("ordinal", &format!("#{}", key.ordinal)),
                        theme.paint(operation_status_role(&state), &state),
                    );
                    if tool.tool_id.as_str() == "agentlibre.execution:command.exec"
                        && let Some(argv) = tool.input.get("argv")
                    {
                        println!(
                            "  {} {}",
                            theme.paint("muted", "command"),
                            json_to_terminal(argv, theme)
                        );
                    }
                    let input = json_to_terminal(&tool.input, theme);
                    println!(
                        "  {}\n{}",
                        theme.paint("field", "INPUT"),
                        preview_tool_output(&input, tool_output, key.ordinal.get(), "input", theme)
                    );
                } else {
                    let kind = kind.unwrap_or_else(|| operation_request_kind(&operation.request));
                    let kind = format!("{kind:?}");
                    let state = format!("{:?}", operation.state);
                    let compaction_required = operation.failure.as_ref().is_some_and(|failure| {
                        matches!(
                            failure.kind,
                            agl_core::agent::AgentOperationFailureKind::CompactionRequired(_)
                        )
                    });
                    if compaction_required {
                        println!(
                            "{} {}  {}",
                            theme.paint("operation", "MODEL GENERATION"),
                            theme.paint("ordinal", &format!("#{}", key.ordinal)),
                            theme.paint("muted", "COMPACTION REQUIRED"),
                        );
                        return;
                    }
                    if !model_generation_details && kind == "ModelGeneration" {
                        println!(
                            "{} {}  {}",
                            theme.paint("operation", "MODEL GENERATION"),
                            theme.paint("ordinal", &format!("#{}", key.ordinal)),
                            theme.paint(operation_status_role(&state), &state),
                        );
                        return;
                    }
                    let label = match kind.as_str() {
                        "ModelGeneration" => "MODEL GENERATION",
                        "Compaction" => "COMPACTION",
                        _ => "OPERATION",
                    };
                    println!(
                        "{} {}  {}",
                        theme.paint("operation", label),
                        theme.paint("ordinal", &format!("#{}", key.ordinal)),
                        theme.paint(operation_status_role(&state), &state),
                    );
                    let delivery = delivery.unwrap_or(operation.delivery);
                    println!(
                        "  {} {}  {} {}",
                        theme.paint("muted", "delivery"),
                        theme.paint("operation", &format!("{delivery:?}")),
                        theme.paint("muted", "attempt"),
                        theme.paint("status_pending", &operation.delivery_attempt.to_string()),
                    );
                    let request = serde_json::from_str(&request)
                        .map(|value| json_to_terminal(&value, theme))
                        .unwrap_or(request);
                    println!("  {}\n{}", theme.paint("field", "REQUEST"), request);
                }
                if let Some(result) = operation.result.as_ref() {
                    let variant = match result {
                        agl_core::agent::AgentOperationResult::ModelGeneration(_) => {
                            "model_generation"
                        }
                        agl_core::agent::AgentOperationResult::Compaction(_) => "compaction",
                        agl_core::agent::AgentOperationResult::Tool(_) => "tool",
                    };
                    if let agl_core::agent::AgentOperationResult::Tool(result) = result {
                        println!(
                            "  {}\n{}",
                            theme.paint("field", "RESULT"),
                            frame_tool_output(
                                &preview_tool_output(
                                    &format_tool_content_for(
                                        match &operation.request {
                                            agl_core::agent::AgentOperationRequest::Tool(tool) =>
                                                Some(tool.tool_id.as_str()),
                                            _ => None,
                                        },
                                        result.content.as_text(),
                                        match &operation.request {
                                            agl_core::agent::AgentOperationRequest::Tool(tool) =>
                                                Some(&tool.input),
                                            _ => None,
                                        },
                                        theme,
                                    ),
                                    tool_output,
                                    key.ordinal.get(),
                                    "result",
                                    theme,
                                ),
                                theme
                            )
                        );
                    } else {
                        println!(
                            "  {} {}",
                            theme.paint("field", "RESULT"),
                            theme.paint("json_string", variant)
                        );
                    }
                }
                if let Some(failure) = operation.failure.as_ref() {
                    let bytes = serde_json::to_vec(failure)
                        .map(|value| value.len())
                        .unwrap_or(0);
                    let failure = serde_json::to_value(&failure.kind)
                        .map(|value| json_to_terminal(&value, theme))
                        .unwrap_or_else(|_| safe_terminal_text(&format!("{:?}", failure.kind)));
                    println!("  {}", theme.paint("status_failure", "FAILURE"));
                    println!("{failure}");
                    println!(
                        "  {}",
                        theme.paint("muted", &format!("diagnostic bytes: {bytes}"))
                    );
                }
                if tool_frame
                    && matches!(
                        &operation.request,
                        agl_core::agent::AgentOperationRequest::Tool(_)
                    )
                {
                    println!("{}", answer_footer(theme, tool_elapsed.unwrap_or_default()));
                    println!();
                }
            }
            Err(error) => eprintln!("operation {} decoration diagnostic: {error}", key.ordinal),
        },
        Err(error) => eprintln!("operation {} decoration diagnostic: {error}", key.ordinal),
    }
}

fn operation_request_decoration(
    request: &agl_core::agent::AgentOperationRequest,
) -> Result<String, serde_json::Error> {
    use agl_core::agent::AgentOperationRequest;
    let value = match request {
        AgentOperationRequest::ModelGeneration(request) => serde_json::json!({
            "type": "model_generation",
            "context_messages": request.context.len(),
            "max_output_tokens": request.max_output_tokens,
        }),
        AgentOperationRequest::Compaction(request) => serde_json::json!({
            "type": "compaction",
            "context_messages": request.context.len(),
            "before": request.before,
            "correction_of": request.correction_of,
        }),
        AgentOperationRequest::Tool(request) => serde_json::json!({
            "type": "tool",
            "tool_id": request.tool_id,
            "input": request.input,
        }),
    };
    serde_json::to_string(&value)
}

fn operation_request_kind(request: &agl_core::agent::AgentOperationRequest) -> AgentOperationKind {
    match request {
        agl_core::agent::AgentOperationRequest::ModelGeneration(_) => {
            AgentOperationKind::ModelGeneration
        }
        agl_core::agent::AgentOperationRequest::Compaction(_) => AgentOperationKind::Compaction,
        agl_core::agent::AgentOperationRequest::Tool(_) => AgentOperationKind::Tool,
    }
}

fn event_ends_run(event: &AgentEvent) -> bool {
    matches!(
        event.data,
        AgentEventData::RunStatusChanged { to, .. } if to.is_terminal()
    )
}

fn terminal_summary(view: &agl_core::agent::AgentRunView) -> String {
    let mut summary = format!("AgentRun ended with {:?}", view.status);
    if let Some(failure) = &view.failure {
        match failure {
            agl_core::agent::AgentRunFailureView::Run { kind } => {
                summary.push_str(&format!(": Run failed with {}", enum_name(kind)));
            }
            agl_core::agent::AgentRunFailureView::Operation { kind, tool_id, .. } => {
                let kind = match kind {
                    agl_core::agent::AgentOperationFailureKind::ContextExhausted(failure) => {
                        let capacity = &failure.capacity;
                        let mut diagnostic = format!(
                            "context_exhausted (prompt_tokens={}, reserved_output_tokens={}, context_capacity_tokens={}, trigger_threshold_tokens={})",
                            capacity.prompt_tokens,
                            capacity.reserved_output_tokens,
                            capacity.context_capacity_tokens,
                            capacity.trigger_threshold_tokens,
                        );
                        if let Some(compaction) = &failure.compaction {
                            diagnostic.push_str(&format!(
                                " compaction={}",
                                serde_json::to_string(compaction)
                                    .expect("typed compaction diagnostic")
                            ));
                        }
                        diagnostic
                    }
                    _ => enum_name(kind),
                };
                match tool_id {
                    Some(tool_id) => {
                        summary.push_str(&format!(": Tool {tool_id} failed with {kind}"));
                    }
                    None => summary.push_str(&format!(": operation failed with {kind}")),
                }
            }
        }
    }
    summary
}

fn enum_name<T: serde::Serialize + std::fmt::Debug>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{value:?}"))
}

async fn select_resume_conversation(
    client: &AgentClient,
    args: ResumeArgs,
) -> Result<ConversationView> {
    if let Some(selector) = args.conversation {
        ensure!(
            !args.all,
            "--all cannot be used with an explicit Conversation"
        );
        return client
            .resolve_conversation(selector)
            .await
            .map_err(Into::into);
    }
    let workspace = if args.all {
        None
    } else {
        Some(utf8_path(
            &std::env::current_dir()?.canonicalize()?,
            "workspace",
        )?)
    };
    let conversations = client.conversations(workspace, 1_000).await?;
    ensure!(
        !conversations.is_empty(),
        "no resumable Conversations found"
    );
    if args.last {
        return Ok(conversations[0].clone());
    }
    for (index, conversation) in conversations.iter().enumerate() {
        println!(
            "{}\t{}\t{}\t{}@{}",
            index + 1,
            conversation.id,
            conversation.display_name.as_deref().unwrap_or("-"),
            conversation.function.id,
            conversation.function.version,
        );
    }
    print!("select> ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    BufReader::new(tokio::io::stdin())
        .read_line(&mut line)
        .await?;
    let selected: usize = line.trim().parse().context("selection is not a number")?;
    conversations
        .get(selected.saturating_sub(1))
        .cloned()
        .context("selection is outside the displayed range")
}

struct ApplicationRoots {
    config: PathBuf,
    data: PathBuf,
    state: PathBuf,
}

fn application_roots() -> Result<ApplicationRoots> {
    if let Some(root) = nonempty_env_path("AGL_HOME") {
        let root = absolute_root(root, "AGL_HOME")?;
        return Ok(ApplicationRoots {
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
        });
    }
    let home = nonempty_env_path("HOME");
    let config = nonempty_env_path("XDG_CONFIG_HOME")
        .or_else(|| home.as_ref().map(|path| path.join(".config")))
        .context("HOME or XDG_CONFIG_HOME is required")?;
    let data = nonempty_env_path("XDG_DATA_HOME")
        .or_else(|| home.as_ref().map(|path| path.join(".local/share")))
        .context("HOME or XDG_DATA_HOME is required")?;
    let state = nonempty_env_path("XDG_STATE_HOME")
        .or_else(|| home.map(|path| path.join(".local/state")))
        .context("HOME or XDG_STATE_HOME is required")?;
    Ok(ApplicationRoots {
        config: absolute_root(config, "configuration root")?.join("agentLIBRE"),
        data: absolute_root(data, "data root")?.join("agentLIBRE"),
        state: absolute_root(state, "state root")?.join("agentLIBRE"),
    })
}

fn resolve_function_locator(
    locator: &config::FunctionLocator,
    active: &config::ActiveConfig,
) -> Result<PathBuf> {
    match locator {
        config::FunctionLocator::Directory(path) => active
            .functions
            .iter()
            .find(|function| function.directory == *path)
            .map(|_| path.clone())
            .context("explicit Function has not been activated; run `agl config apply --function <FUNCTION>`"),
        config::FunctionLocator::Requirement(requirement) => {
            let mut matches = active
                .functions
                .iter()
                .filter_map(|function| {
                    let id = agl_runtime::package::PackageId::new(function.id.clone()).ok()?;
                    if id != requirement.id {
                        return None;
                    }
                    let version = semver::Version::parse(&function.version).ok()?;
                    requirement.version.matches(&version).then_some((version, function))
                })
                .collect::<Vec<_>>();
            matches.sort_by(|left, right| right.0.cmp(&left.0));
            let best = matches.first().context(
                "typed Function is not active; run `agl config apply --function <FUNCTION>`",
            )?;
            ensure!(
                matches.iter().filter(|item| item.0 == best.0).count() == 1,
                "typed Function resolves ambiguously in active configuration"
            );
            Ok(best.1.directory.clone())
        }
    }
}

fn print_config_plan(plan: &config::ConfigPlan) {
    println!("source_digest={}", plan.source_digest);
    println!("executable={}", plan.executable.display());
    println!(
        "keep_free_ram={}",
        plan.keep_free_ram
            .map_or_else(|| "automatic".into(), |value| value.to_string())
    );
    println!(
        "keep_free_vram={}",
        plan.keep_free_vram
            .map_or_else(|| "automatic".into(), |value| value.to_string())
    );
    for function in &plan.functions {
        println!(
            "function={}@{}\t{}\tlock_entities={}",
            function.id,
            function.version,
            function.directory.display(),
            function.digest
        );
    }
}

fn apply_config(roots: &ApplicationRoots, socket: &Path, plan: &config::ConfigPlan) -> Result<()> {
    let unfinished = agl_daemon::unfinished_agent_runs(&roots.data)?;
    ensure!(
        unfinished.is_empty(),
        "configuration is busy with unfinished Runs: {}",
        unfinished
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let active_path = config::active_path(&roots.data);
    let old_active = std::fs::read(&active_path).ok();
    let result = (|| {
        config::write_active(&roots.data, &config::active(plan))?;
        controlled_reload(socket)?;
        println!("applied {}", active_path.display());
        Ok::<(), anyhow::Error>(())
    })();
    if let Err(error) = result {
        match old_active {
            Some(bytes) => std::fs::write(active_path, bytes)?,
            None if active_path.try_exists()? => std::fs::remove_file(active_path)?,
            None => {}
        }
        return Err(error);
    }
    Ok(())
}

fn controlled_reload(socket: &Path) -> Result<()> {
    if !socket.try_exists()? {
        return Ok(());
    }
    let status = ProcessCommand::new("systemctl")
        .args(["--user", "reload-or-restart", "agentlibre-daemon.service"])
        .status()
        .context("daemon is active but systemctl --user is unavailable")?;
    ensure!(
        status.success(),
        "controlled daemon reload failed; generated state remains on disk"
    );
    Ok(())
}

fn print_doctor(roots: &ApplicationRoots) -> Result<()> {
    let active = config::read_active(&roots.data)
        .context("no generated configuration; run `agl config apply` first")?;
    println!("active_source_digest={}", active.source_digest);
    println!("active_executable={}", active.executable.display());
    println!("active_functions={}", active.functions.len());
    for function in &active.functions {
        println!(
            "function={}@{}\t{}\tdigest={}",
            function.id,
            function.version,
            function.directory.display(),
            function.digest
        );
    }
    let unfinished = agl_daemon::unfinished_agent_runs(&roots.data)?;
    println!("unfinished_runs={}", unfinished.len());
    for run in unfinished {
        println!("run={run}");
    }
    Ok(())
}

fn read_regular_file(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "{} must be a regular non-symlink file",
        path.display()
    );
    ensure!(metadata.len() <= maximum, "{} is oversized", path.display());
    std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))
}

fn nonempty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn absolute_root(path: PathBuf, name: &str) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "{name} must be absolute");
    Ok(path)
}

fn utf8_path(path: &Path, name: &str) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{name} is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::time::{SystemTime, UNIX_EPOCH};

    use agl_core::ToolId;
    use agl_core::agent::{
        AgentEventId, AgentOperationFailureKind, AgentOperationKey, AgentRunFailureView,
        AgentRunOrigin, AgentRunUsage, AgentRunView,
    };
    use clap::CommandFactory as _;
    use tokio::io::{AsyncWriteExt as _, BufReader};

    use super::*;

    #[test]
    fn terminal_summary_names_the_failed_tool_and_kind() {
        let run_id = AgentRunId::generate();
        let operation = AgentOperationKey {
            run_id,
            ordinal: NonZeroU32::new(2).unwrap(),
        };
        let mut view = AgentRunView {
            id: run_id,
            origin: AgentRunOrigin::User {
                conversation_id: ConversationId::generate(),
                message_id: MessageId::generate(),
            },
            status: AgentRunStatus::Failed,
            usage: AgentRunUsage::default(),
            current_operation: Some(operation.clone()),
            failure: Some(AgentRunFailureView::Operation {
                operation,
                kind: AgentOperationFailureKind::InvalidInput,
                tool_id: Some(ToolId::new("agentlibre.builtins:fs_read").unwrap()),
            }),
            last_event_id: AgentEventId(4),
        };
        assert_eq!(
            terminal_summary(&view),
            "AgentRun ended with Failed: Tool agentlibre.builtins:fs_read failed with invalid_input"
        );
        view.failure = Some(AgentRunFailureView::Operation {
            operation: view.current_operation.clone().unwrap(),
            kind: AgentOperationFailureKind::ContextExhausted(
                agl_core::agent::ContextCapacity::new(81_921, 49_152, 131_072).into(),
            ),
            tool_id: None,
        });
        assert_eq!(
            terminal_summary(&view),
            "AgentRun ended with Failed: operation failed with context_exhausted (prompt_tokens=81921, reserved_output_tokens=49152, context_capacity_tokens=131072, trigger_threshold_tokens=91751)"
        );
        let source = agl_core::MessageId::generate();
        let Some(AgentRunFailureView::Operation {
            kind: AgentOperationFailureKind::ContextExhausted(failure),
            ..
        }) = &mut view.failure
        else {
            unreachable!()
        };
        failure.compaction = Some(Box::new(agl_core::agent::CompactionFailure {
            stage: agl_core::agent::CompactionFailureStage::SummarySource,
            model_called: false,
            usage: None,
            summary_source_start: Some(source.clone()),
            summary_source_end: Some(source.clone()),
            source_contributions: vec![agl_core::agent::SourceTokenContribution {
                source_start: source.clone(),
                source_end: source.clone(),
                isolated_prompt_tokens: 81921,
            }],
        }));
        let diagnostic = terminal_summary(&view);
        assert!(diagnostic.contains("context_exhausted"));
        assert!(diagnostic.contains("\"stage\":\"summary_source\""));
        assert!(diagnostic.contains(source.as_str()));
        assert!(diagnostic.contains("\"isolated_prompt_tokens\":81921"));
    }

    #[test]
    fn terminal_summary_exposes_run_level_limits() {
        let run_id = AgentRunId::generate();
        let view = AgentRunView {
            id: run_id,
            origin: AgentRunOrigin::User {
                conversation_id: ConversationId::generate(),
                message_id: MessageId::generate(),
            },
            status: AgentRunStatus::Failed,
            usage: AgentRunUsage::default(),
            current_operation: None,
            failure: Some(AgentRunFailureView::Run {
                kind: agl_core::agent::AgentRunFailureKind::LimitsExceeded,
            }),
            last_event_id: AgentEventId(5),
        };
        assert_eq!(
            terminal_summary(&view),
            "AgentRun ended with Failed: Run failed with limits_exceeded"
        );
    }

    #[test]
    fn subscription_renderer_accumulates_deltas_and_detects_terminal_events() {
        let run_id = AgentRunId::generate();
        let operation = AgentOperationKey {
            run_id,
            ordinal: NonZeroU32::new(1).unwrap(),
        };
        let mut streamed = String::new();
        let mut streaming_region_active = false;
        let theme = TerminalTheme::from_colors(&Default::default()).unwrap();
        render_progress(
            AgentProgress::ModelOutputDelta {
                operation,
                content: Content::text("hello").unwrap(),
            },
            false,
            false,
            &mut streamed,
            &mut streaming_region_active,
            Instant::now(),
            &theme,
        )
        .unwrap();
        assert_eq!(streamed, "hello");
        let event = AgentEvent {
            id: AgentEventId(2),
            agent_run_id: run_id,
            operation: None,
            run_revision: 2,
            operation_revision: None,
            committed_at_ms: 1,
            data: AgentEventData::RunStatusChanged {
                from: AgentRunStatus::Running,
                to: AgentRunStatus::Completed,
                failure_kind: None,
            },
        };
        assert!(event_ends_run(&event));
    }

    #[test]
    fn markdown_renderer_keeps_link_destination_visible() {
        let theme = TerminalTheme::from_colors(&Default::default()).unwrap();
        assert_eq!(
            markdown_to_terminal("See [docs](https://example.test/path).", &theme),
            "See [docs] (https://example.test/path).\n"
        );
    }

    #[test]
    fn worked_for_uses_compact_units() {
        assert_eq!(format_worked_for(Duration::from_millis(400)), "<1s");
        assert_eq!(format_worked_for(Duration::from_secs(7)), "7s");
        assert_eq!(format_worked_for(Duration::from_secs(125)), "2m 5s");
        assert_eq!(format_worked_for(Duration::from_secs(3720)), "1h 2m");
    }

    #[test]
    fn presentation_styles_accept_truecolor_and_oklch() {
        let hex = TerminalStyle::parse("underline bold #12ABef").unwrap();
        assert_eq!(hex.sgr.as_deref(), Some("4;1;38;2;18;171;239"));
        let oklch = TerminalStyle::parse("italic oklch(0.7 0.15 30)").unwrap();
        assert!(
            oklch
                .sgr
                .as_deref()
                .is_some_and(|value| value.starts_with("3;38;2;"))
        );
        assert!(TerminalStyle::parse("#1234").is_err());
        assert!(TerminalStyle::parse("oklch(1.2 0.1 20)").is_err());
        assert!(TerminalStyle::parse("").is_err());
    }

    #[test]
    fn disabled_theme_emits_plain_text_without_ansi() {
        let mut theme = TerminalTheme::from_colors(&Default::default()).unwrap();
        theme.enabled = false;
        assert_eq!(theme.paint("tool", "TOOL #1"), "TOOL #1");
        assert_eq!(theme.reset(), "");
    }

    #[test]
    fn input_status_row_keeps_function_and_cwd_within_terminal_width() {
        let mut theme = TerminalTheme::from_colors(&Default::default()).unwrap();
        theme.enabled = false;
        let renderer = TtyRenderer::new(
            theme,
            TtyEditor::new(Path::new("/definitely/missing/history")).unwrap(),
            "function-with-a-long-name@1.2.3",
            "/workspace/with/a/long/path",
        );
        let row = renderer.status_row(20);
        assert_eq!(terminal_visible_width(&row), 20);
        assert!(!row.contains("function-with-a-long-name"));
        assert!(row.contains("path"));
    }

    #[test]
    fn submitted_message_background_covers_wrapped_and_multiline_rows() {
        let theme = TerminalTheme::from_colors(&Default::default()).unwrap();
        let frame = submitted_message_frame(&theme, "абвгде\nhello world", 10);
        let background = theme.start("input_background");
        let rows = frame.split("\r\n").collect::<Vec<_>>();
        assert!(rows.len() >= 3);
        assert!(
            rows.iter()
                .all(|row| { terminal_visible_width(row) == 10 && row.contains(&background) })
        );
        assert!(frame.contains(&theme.paint("input_prompt", "› ")));
    }

    #[test]
    fn prompt_frame_and_separator_are_exact_terminal_width() {
        let mut theme = TerminalTheme::from_colors(&Default::default()).unwrap();
        theme.enabled = false;
        let renderer = TtyRenderer::new(
            theme,
            TtyEditor::new(Path::new("/definitely/missing/history")).unwrap(),
            "function@1.0.0",
            "/workspace",
        );
        assert_eq!(terminal_visible_width(&renderer.prompt_line(24)), 24);
        assert_eq!(terminal_visible_width(&renderer.separator(24)), 24);
        assert_eq!(terminal_visible_width(&renderer.prompt_line(1)), 1);
    }

    #[test]
    fn tty_editor_preserves_multiline_bracketed_paste() {
        let mut editor = TtyEditor::new(Path::new("/definitely/missing/history")).unwrap();
        editor.insert_paste("first\r\n\tsecond\rthird\u{1b}");
        assert_eq!(editor.buffer, "first\n\tsecond\nthird");
        assert_eq!(editor.cursor, editor.buffer.len());
    }

    #[test]
    fn visible_tail_renders_only_the_current_pasted_line() {
        let text = "first\n\tsecond\nthird";
        let visible = visible_tail(text, "first\n\tsec".len(), 20);
        assert_eq!(visible.text, "    second");
        assert_eq!(visible.cursor_cells, 7);
    }

    #[test]
    fn terminal_width_ignores_truecolor_csi_and_uses_unicode_cells() {
        let styled = "\x1b[38;2;18;171;239m界é🙂\x1b[0m";
        assert_eq!(terminal_visible_width(styled), 5);
        assert_eq!(terminal_visible_width("abc\ndefgh"), 5);
        assert_eq!(
            wrap_external_text(styled, 3),
            "\x1b[38;2;18;171;239m界é\n🙂\x1b[0m"
        );
    }

    #[test]
    fn external_text_wrap_preserves_ansi_sequences() {
        let styled = "\x1b[1;38;2;100;120;140mhello\x1b[0m";
        assert_eq!(terminal_visible_width(styled), 5);
        assert_eq!(
            wrap_external_text(styled, 4),
            "\x1b[1;38;2;100;120;140mhell\no\x1b[0m"
        );
    }

    #[test]
    fn tool_elapsed_uses_non_negative_committed_time_delta() {
        assert_eq!(elapsed_between_ms(100, 250), Duration::from_millis(150));
        assert_eq!(elapsed_between_ms(250, 100), Duration::ZERO);
    }

    #[tokio::test]
    async fn repl_turn_consumes_progress_from_a_fake_daemon_endpoint() {
        let stem = format!(
            "agl-daemon-cli-stream-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = (0..100)
            .map(|suffix| std::env::temp_dir().join(format!("{stem}-{suffix}")))
            .find(|candidate| std::fs::create_dir(candidate).is_ok())
            .expect("could not allocate a unique temporary directory");
        let socket = root.join("agl.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        let run_id = AgentRunId::generate();
        let conversation_id = ConversationId::generate();
        let message_id = MessageId::generate();
        let server = tokio::spawn(fake_streaming_daemon(
            listener,
            run_id,
            conversation_id,
            message_id,
        ));

        let (view, streamed) = stream_until_terminal(
            &AgentClient::new(&socket),
            run_id,
            false,
            Decorations::Off,
            agl_core::agent::ToolOutputPresentation::default(),
            false,
            &TerminalTheme::from_colors(&Default::default()).unwrap(),
            false,
            Instant::now(),
        )
        .await
        .unwrap();
        assert_eq!(view.status, AgentRunStatus::Completed);
        assert_eq!(streamed, "streamed answer");
        server.await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    async fn fake_streaming_daemon(
        listener: tokio::net::UnixListener,
        run_id: AgentRunId,
        conversation_id: ConversationId,
        message_id: MessageId,
    ) {
        let operation = AgentOperationKey {
            run_id,
            ordinal: NonZeroU32::MIN,
        };
        let running = run_view(
            run_id,
            conversation_id,
            message_id.clone(),
            AgentRunStatus::Running,
            AgentEventId(0),
        );
        let completed = run_view(
            run_id,
            conversation_id,
            message_id,
            AgentRunStatus::Completed,
            AgentEventId(1),
        );
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut line = Vec::new();
        BufReader::new(reader)
            .read_until(b'\n', &mut line)
            .await
            .unwrap();
        let request: agl_daemon_api::AgentProtocolRequest = serde_json::from_slice(&line).unwrap();
        assert!(matches!(
            request.command,
            agl_daemon_api::AgentCommand::Subscribe { run_id: requested } if requested == run_id
        ));
        let frames = [
            AgentSubscriptionFrame::Subscribed {
                run_view: running,
                cursor: AgentEventId(0),
            },
            AgentSubscriptionFrame::Progress {
                progress: AgentProgress::ModelOutputDelta {
                    operation,
                    content: Content::text("streamed answer").unwrap(),
                },
            },
            AgentSubscriptionFrame::Event {
                event: AgentEvent {
                    id: AgentEventId(1),
                    agent_run_id: run_id,
                    operation: None,
                    run_revision: 2,
                    operation_revision: None,
                    committed_at_ms: 1,
                    data: AgentEventData::RunStatusChanged {
                        from: AgentRunStatus::Running,
                        to: AgentRunStatus::Completed,
                        failure_kind: None,
                    },
                },
            },
        ];
        for frame in frames {
            let mut bytes = serde_json::to_vec(&agl_daemon_api::AgentProtocolStreamFrame::new(
                request.request_id.clone(),
                frame,
            ))
            .unwrap();
            bytes.push(b'\n');
            writer.write_all(&bytes).await.unwrap();
        }

        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut line = Vec::new();
        BufReader::new(reader)
            .read_until(b'\n', &mut line)
            .await
            .unwrap();
        let request: agl_daemon_api::AgentProtocolRequest = serde_json::from_slice(&line).unwrap();
        assert!(matches!(
            request.command,
            agl_daemon_api::AgentCommand::RunView { run_id: requested } if requested == run_id
        ));
        let mut bytes = serde_json::to_vec(&agl_daemon_api::AgentProtocolResponse::ok(
            request.request_id,
            agl_daemon_api::AgentResponse::RunView { view: completed },
        ))
        .unwrap();
        bytes.push(b'\n');
        writer.write_all(&bytes).await.unwrap();
    }

    fn run_view(
        run_id: AgentRunId,
        conversation_id: ConversationId,
        message_id: MessageId,
        status: AgentRunStatus,
        last_event_id: AgentEventId,
    ) -> AgentRunView {
        AgentRunView {
            id: run_id,
            origin: AgentRunOrigin::User {
                conversation_id,
                message_id,
            },
            status,
            usage: AgentRunUsage::default(),
            current_operation: None,
            failure: None,
            last_event_id,
        }
    }

    #[test]
    fn reasoning_control_is_process_local_and_strict() {
        use agl_core::agent::{ReasoningEffort, ReasoningSelection};
        let base = ReasoningSelection::Enabled {
            max_tokens: 32768,
            effort: Some(ReasoningEffort::Xhigh),
            preserve: true,
        };
        let mut selected = None;
        assert!(reasoning_command("/reasoning", &mut selected).unwrap());
        assert_eq!(reasoning_label(&base, selected), "xhigh");
        assert!(reasoning_command("/reasoning low", &mut selected).unwrap());
        assert_eq!(selected, Some(ReasoningEffort::Low));
        assert_eq!(reasoning_label(&base, selected), "low");
        assert_eq!(reasoning_label(&base, None), "xhigh");
        for invalid in ["/reasoning high", "/reasoning low extra"] {
            assert!(reasoning_command(invalid, &mut selected).is_err());
            assert_eq!(selected, Some(ReasoningEffort::Low));
        }
        assert!(!reasoning_command("explain /reasoning", &mut selected).unwrap());
        assert!(reasoning_command("/reasoning default", &mut selected).unwrap());
        assert_eq!(selected, None);
        for argv in [
            vec!["agl", "--reasoning", "medium"],
            vec!["agl", "--reasoning", "medium", "resume", "CHAT"],
            vec!["agl", "run", "FUNCTION", "PROMPT", "--reasoning", "medium"],
        ] {
            assert_eq!(
                Cli::try_parse_from(argv).unwrap().reasoning,
                Some(ReasoningEffort::Medium)
            );
        }
        assert!(Cli::try_parse_from(["agl", "--reasoning", "high"]).is_err());
    }

    #[test]
    fn decorations_control_is_process_local_and_strict() {
        let mut selected = None;
        assert!(decorations_command("/decorations", &mut selected).unwrap());
        assert_eq!(selected, None);
        assert!(decorations_command("/decorations default", &mut selected).unwrap());
        assert_eq!(selected, Some(Decorations::Default));
        assert!(decorations_command("/decorations full", &mut selected).unwrap());
        assert_eq!(selected, Some(Decorations::Full));
        assert!(decorations_command("/decorations reset", &mut selected).unwrap());
        assert_eq!(selected, None);
        assert!(decorations_command("/decorations minimal", &mut selected).is_err());
        assert_eq!(selected, None);
    }

    #[test]
    fn tool_output_limits_are_session_local_and_reset_to_function_values() {
        let defaults = agl_core::agent::ToolOutputPresentation {
            lines: 10,
            chars: 500,
        };
        let mut current = defaults;
        assert!(tool_output_command("/tool-output", &mut current, defaults).unwrap());
        assert!(tool_output_command("/tool-output lines 20", &mut current, defaults).unwrap());
        assert_eq!(current.lines, 20);
        assert!(tool_output_command("/tool-output chars 900", &mut current, defaults).unwrap());
        assert_eq!(current.chars, 900);
        assert!(tool_output_command("/tool-output reset", &mut current, defaults).unwrap());
        assert_eq!(current, defaults);
        for invalid in ["/tool-output lines 0", "/tool-output chars nope"] {
            assert!(tool_output_command(invalid, &mut current, defaults).is_err());
        }
    }

    #[test]
    fn tool_frame_is_session_local_and_resets_to_function_value() {
        let mut current = true;
        assert!(tool_frame_command("/tool-frame", &mut current, true).unwrap());
        assert!(current);
        assert!(tool_frame_command("/tool-frame off", &mut current, true).unwrap());
        assert!(!current);
        assert!(tool_frame_command("/tool-frame reset", &mut current, true).unwrap());
        assert!(current);
        assert!(tool_frame_command("/tool-frame on", &mut current, false).unwrap());
        assert!(current);
        assert!(tool_frame_command("/tool-frame reset", &mut current, false).unwrap());
        assert!(!current);
        for invalid in ["/tool-frame maybe", "/tool-frame on extra"] {
            assert!(tool_frame_command(invalid, &mut current, true).is_err());
        }
    }

    #[test]
    fn model_generation_details_are_session_local_and_reset_to_function_value() {
        let mut current = false;
        assert!(model_generation_command("/model-generation", &mut current, false).unwrap());
        assert!(!current);
        assert!(
            model_generation_command("/model-generation details on", &mut current, false).unwrap()
        );
        assert!(current);
        assert!(
            model_generation_command("/model-generation details off", &mut current, true).unwrap()
        );
        assert!(!current);
        assert!(
            model_generation_command("/model-generation details reset", &mut current, true)
                .unwrap()
        );
        assert!(current);
        for invalid in [
            "/model-generation details maybe",
            "/model-generation details on extra",
        ] {
            assert!(model_generation_command(invalid, &mut current, false).is_err());
        }
    }

    #[test]
    fn tool_output_preview_stops_at_the_first_limit() {
        let theme = TerminalTheme::from_colors(&Default::default()).unwrap();
        let preview = preview_tool_output(
            "alpha\nbeta\ngamma",
            agl_core::agent::ToolOutputPresentation {
                lines: 2,
                chars: 500,
            },
            4,
            "result",
            &theme,
        );
        assert!(preview.starts_with("alpha\nbeta"));
        assert!(preview.contains("/tool 4 result"));

        let preview = preview_tool_output(
            "абвгдеж",
            agl_core::agent::ToolOutputPresentation {
                lines: 10,
                chars: 4,
            },
            2,
            "input",
            &theme,
        );
        assert!(preview.starts_with("абвг\n"));
        assert!(preview.contains("/tool 2 input"));
    }

    #[test]
    fn public_command_surface_is_the_selected_minimum() {
        let command = Cli::command();
        let names = command
            .get_subcommands()
            .map(|subcommand| subcommand.get_name())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "plan",
                "store",
                "resume",
                "chat",
                "conversation",
                "serve",
                "config",
                "doctor",
                "function",
                "artifact",
                "run",
                "view",
                "cancel"
            ]
        );
        for removed in [
            ["agl", "events", "--help"].as_slice(),
            ["agl", "messages", "--help"].as_slice(),
            ["agl", "serve", "--config", "/tmp/daemon.json"].as_slice(),
            [
                "agl",
                "run",
                "/tmp/function",
                "hello",
                "--workspace",
                "/tmp",
            ]
            .as_slice(),
        ] {
            assert!(Cli::try_parse_from(removed).is_err());
        }
    }

    #[test]
    fn root_function_override_and_chat_subcommand_select_functions() {
        let parsed = Cli::try_parse_from(["agl", "--function", "/tmp/functions/chat"]).unwrap();
        assert_eq!(parsed.function.as_deref(), Some("/tmp/functions/chat"));
        assert!(parsed.command.is_none());

        let parsed = Cli::try_parse_from(["agl", "chat", "/tmp/functions/chat"]).unwrap();
        let Some(Command::Chat(args)) = parsed.command else {
            panic!("expected chat command");
        };
        assert_eq!(
            args.function_directory.as_deref(),
            Some("/tmp/functions/chat")
        );
        assert_eq!(args.function, None);

        let parsed = Cli::try_parse_from([
            "agl",
            "chat",
            "--function",
            "function:agentlibre.coder@^1.0",
            "--reasoning",
            "medium",
        ])
        .unwrap();
        let Some(Command::Chat(args)) = parsed.command else {
            panic!("expected chat command");
        };
        assert_eq!(
            args.function.as_deref(),
            Some("function:agentlibre.coder@^1.0")
        );
        assert_eq!(
            parsed.reasoning,
            Some(agl_core::agent::ReasoningEffort::Medium)
        );

        assert!(
            Cli::try_parse_from([
                "agl",
                "chat",
                "/tmp/functions/chat",
                "--function",
                "/tmp/functions/other",
            ])
            .is_err()
        );
    }

    #[test]
    fn chat_removes_only_the_line_ending_from_prompt_content() {
        let line = "  preserve me  \r\n";
        assert_eq!(line.trim_end_matches(['\r', '\n']), "  preserve me  ");
    }

    #[test]
    fn repl_accepts_both_exact_quit_commands() {
        assert!(is_quit_command("/quit"));
        assert!(is_quit_command("  /exit \t"));
        assert!(!is_quit_command("/quit now"));
        assert!(!is_quit_command("please /exit"));
    }

    #[test]
    fn slash_completion_filters_commands_and_arguments_with_descriptions() {
        let mut completer = SlashCompleter;
        let commands = completer.complete("/dec", 4);
        assert_eq!(commands.len(), 5);
        assert_eq!(commands[0].value, "/decorations");
        assert!(commands.iter().all(|item| item.description.is_some()));

        let arguments = completer.complete("/decorations f", 14);
        assert_eq!(arguments.len(), 1);
        assert_eq!(arguments[0].value, "full");
    }

    #[test]
    fn command_renderer_understands_typed_execution_outcomes() {
        let content = serde_json::json!({
            "state": "exited",
            "outcome": {"type": "exit", "code": 7},
            "stdout": "hello\n",
            "stderr": "",
            "truncated": true
        })
        .to_string();
        let theme = TerminalTheme::from_colors(&Default::default()).unwrap();
        let rendered = format_tool_content_for(
            Some("agentlibre.execution:command.exec"),
            &content,
            None,
            &theme,
        );
        assert!(rendered.contains("state: exited"));
        assert!(rendered.contains("outcome: exit 7"));
        assert!(rendered.contains("stdout:\nhello"));
        assert!(rendered.contains("output truncated: true"));
    }

    #[test]
    fn searxng_daemon_section_is_strict_and_optional() {
        let valid: SearxngConfig = serde_json::from_value(serde_json::json!({
            "client_certificate": "/run/credentials/client.crt",
            "client_private_key": "/run/credentials/client.pk8",
            "private_ca": "/run/credentials/ca.crt"
        }))
        .unwrap();
        assert_eq!(
            valid.client_certificate,
            PathBuf::from("/run/credentials/client.crt")
        );
        assert!(
            serde_json::from_value::<SearxngConfig>(serde_json::json!({
                "client_certificate": "/run/credentials/client.crt",
                "client_private_key": "/run/credentials/client.pk8"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<SearxngConfig>(serde_json::json!({
                "client_certificate": "/run/credentials/client.crt",
                "client_private_key": "/run/credentials/client.pk8",
                "private_ca": "/run/credentials/ca.crt",
                "endpoint": "https://attacker.invalid"
            }))
            .is_err()
        );
    }
}
