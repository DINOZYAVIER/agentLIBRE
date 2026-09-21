use super::super::render::*;
use super::*;

pub(crate) async fn chat_loop(
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

pub(crate) fn is_quit_command(input: &str) -> bool {
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

pub(crate) struct SlashCompleter;

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
