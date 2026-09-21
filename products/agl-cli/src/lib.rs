mod artifacts;
mod cli;
mod config;
mod render;
mod repl;
mod workspace;

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
use artifacts::add_artifact;
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
use render::{
    TerminalStyle, TerminalTheme, ansi_escape_end, foreground_turn, terminal_summary,
    terminal_visible_width,
};
use repl::chat_loop;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use workspace::{
    ApplicationRoots, application_roots, apply_config, print_config_plan, print_doctor,
    read_regular_file, resolve_function_locator, utf8_path,
};

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

    use crate::render::{
        decorations_command, elapsed_between_ms, event_ends_run, format_tool_content_for,
        format_worked_for, markdown_to_terminal, model_generation_command, preview_tool_output,
        reasoning_command, reasoning_label, render_progress, stream_until_terminal,
        tool_frame_command, tool_output_command,
    };
    use crate::repl::{
        SlashCompleter, TtyEditor, TtyRenderer, is_quit_command, submitted_message_frame,
        visible_tail,
    };

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
