//! What the transcript shows: user messages, states, thinking, streamed
//! text restyled as Markdown, tool cards, answers and stops (D123-D127,
//! D138). Drawing goes through [`Screen`].

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::dock::grouped;
use super::markdown;
use super::text::{StreamWrap, clean, hard_wrap, width, wrap_line};
use crate::action::{Outcome, Value};
use crate::agent::Report;
use crate::backend::{Prefill, ToolCall};
use crate::i18n::{Lang, m, repl as texts};
use crate::media::{Kind as MediaKind, Media};
use crate::phase::{Phase, PhaseEvent, Transition};
use crate::session::{CutOff, Event, ForgeKind};
use crate::settings::ReplSettings;
use crate::theme::{Role, Theme};
use crate::tools::ToolResult;
use crate::tr;

/// What the dock says the agent is doing (D135; AGL-043 F2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Activity {
    Ready,
    /// The turn's start brings the server up.
    StartingServer,
    /// The turn's start transcribes an audio of the message.
    Transcribing,
    /// A step's prompt is rendered, counted and read by the server, until
    /// its first token.
    Prefill,
    Thinking,
    /// The action or the answer is being written.
    Writing,
    Running(String),
    Compacting,
    /// A stir of the stirred mode: the step is tried again.
    Retry { attempt: u32, max: u32 },
}

impl Activity {
    /// Its name, in the dock and on the rule of a step (AGL-045).
    pub fn label(&self, lang: Lang) -> String {
        match self {
            Activity::Ready => tr!(lang, m::READY).to_string(),
            Activity::StartingServer => tr!(lang, m::STARTING_SERVER).to_string(),
            Activity::Transcribing => tr!(lang, m::TRANSCRIBING_ACTIVITY).to_string(),
            Activity::Prefill => tr!(lang, m::PREFILL).to_string(),
            Activity::Thinking => tr!(lang, m::THINKING_ACTIVITY).to_string(),
            Activity::Writing => tr!(lang, m::WRITING).to_string(),
            Activity::Retry { attempt, max } => {
                tr!(lang, m::RETRY, attempt = attempt, max = max)
            }
            Activity::Running(tool) => tr!(lang, m::RUNNING, tool = tool),
            Activity::Compacting => tr!(lang, m::COMPACTING).to_string(),
        }
    }
}

/// Where the transcript is drawn: above the dock.
pub trait Screen: Send + Sync {
    fn columns(&self) -> usize;
    /// Appends finished lines.
    fn print(&self, lines: &[String]);
    /// The unfinished line below the printed ones; empty clears it.
    fn live(&self, text: &str);
    /// Erases the last `count` printed lines if they are all still on
    /// the screen; false when they are not.
    fn erase(&self, count: usize) -> bool;
    fn activity(&self, activity: Activity);
    /// The tokens of the last prompt (D152).
    fn context(&self, _used: usize) {}
    /// The decoding speed of the current generation, in tokens per
    /// second (AGL-012); none when there is no figure to show.
    fn speed(&self, _speed: Option<f64>) {}
}

/// The streamed text of one action.
struct Streamed {
    wrap: StreamWrap,
    /// Lines printed so far.
    lines: usize,
    columns: usize,
}

/// A generation being decoded: its pieces for the live speed (AGL-012).
struct Generation {
    /// The times of its recent pieces, within the live window.
    window: VecDeque<Instant>,
    /// When its first piece arrived.
    started: Instant,
}

pub struct Presenter {
    screen: Arc<dyn Screen>,
    theme: Theme,
    settings: ReplSettings,
    lang: Lang,
    turn_started: Instant,
    /// The finished phases of the step, with their times, from the FSM of
    /// the turn (AGL-045, AGL-046).
    phases: Vec<(Activity, Duration)>,
    /// What the dock says now.
    activity: Activity,
    /// The time of the last tool call and of the last compaction.
    tool_time: Duration,
    compact_time: Option<Duration>,
    /// What the server read of the step's prompts (AGL-046).
    prefill: Option<Prefill>,
    /// The prompt's last size, the one when a compaction began, and whether
    /// the last size came after that begin (AGL-046, D13).
    context: Option<usize>,
    compact_before: Option<usize>,
    context_fresh: bool,
    /// Whether the turn has entered its first state: only a change of state
    /// is shown (AGL-046, D13).
    state_entered: bool,
    thinking: Option<StreamWrap>,
    action: Option<Streamed>,
    /// The generation being decoded, if any (AGL-012).
    generation: Option<Generation>,
    /// The exact tokens decoded in the turn (AGL-012).
    turn_tokens: usize,
    /// The decoding time of the turn, across its generations (AGL-012).
    turn_decoding: Duration,
}

impl Presenter {
    pub fn new(
        screen: Arc<dyn Screen>,
        theme: Theme,
        settings: ReplSettings,
        lang: Lang,
    ) -> Presenter {
        Presenter {
            screen,
            theme,
            settings,
            lang,
            turn_started: Instant::now(),
            phases: Vec::new(),
            activity: Activity::Ready,
            tool_time: Duration::ZERO,
            compact_time: None,
            prefill: None,
            context: None,
            compact_before: None,
            context_fresh: false,
            state_entered: false,
            thinking: None,
            action: None,
            generation: None,
            turn_tokens: 0,
            turn_decoding: Duration::ZERO,
        }
    }

    /// The dock is idle again.
    pub fn set_ready(&self) {
        self.screen.activity(Activity::Ready);
    }

    /// A user message starts a turn.
    pub fn begin_turn(&mut self) {
        self.turn_started = Instant::now();
        self.phases.clear();
        self.prefill = None;
        self.state_entered = false;
        self.generation = None;
        self.turn_tokens = 0;
        self.turn_decoding = Duration::ZERO;
        self.screen.speed(None);
    }

    /// Ends open streams where they are, as after a stop or an error.
    pub fn close(&mut self) {
        self.close_thinking();
        if let Some(mut streamed) = self.action.take() {
            if let Some(rest) = streamed.wrap.finish() {
                self.screen.print(&[rest]);
            }
            self.screen.live("");
        }
        self.generation_done();
    }

    // Whole blocks, also used to show a resumed session (D126).

    /// The user's message on the input background.
    pub fn show_user(&self, message: &str) {
        let columns = self.screen.columns();
        let mut lines = vec![String::new()];
        lines.extend(user_lines(&self.theme, message, columns));
        self.screen.print(&lines);
    }

    /// `[state]` of a visit (D138).
    pub fn show_state(&self, state: &str) {
        self.screen
            .print(&[self.theme.paint(Role::Muted, &format!("[{state}]"))]);
    }

    /// `[cut off]` where an action hit its token limit (D154).
    pub fn show_cut_off(&self, cut: &CutOff) {
        let line = crate::status::cut_off_line(cut, self.lang);
        self.screen.print(&[self.theme.paint(Role::Muted, &line)]);
    }

    /// A stir of the stirred mode: its number and the parse error
    /// (AGL-030, D4; AGL-043 F8).
    pub fn show_stir(&self, attempt: u32, error: &str) {
        self.screen.print(&notice_lines(
            &self.theme,
            &crate::status::stir_line(attempt, error, self.lang),
            self.screen.columns(),
            Role::Muted,
        ));
    }

    /// A medium the session took (AGL-043 F17): its placeholder and its
    /// file name; the transcript of an audio, the count of the frames of a
    /// video.
    pub fn show_media(&self, medium: &Media) {
        let head = format!("{} {}", medium.placeholder(), medium.name);
        let line = match (&medium.kind, &medium.transcript) {
            (MediaKind::Audio, Some(transcript)) => format!("{head} — “{transcript}”"),
            (MediaKind::Video, _) => tr!(
                self.lang,
                m::MEDIA_EVENT_FRAMES,
                head = head,
                frames = medium.frames.len()
            ),
            _ => head,
        };
        self.screen.print(&notice_lines(
            &self.theme,
            &line,
            self.screen.columns(),
            Role::Muted,
        ));
    }

    /// The rule where a summary replaced the history (D140; AGL-046, D13):
    /// what is replayed knows no time and no tokens.
    pub fn show_compacted(&self) {
        let label = Activity::Compacting.label(self.lang);
        self.screen.print(&[self.edge(
            Some(("ok", Role::StatusSuccess)),
            &[label],
            Role::Rule,
        )]);
    }

    /// `[forge: …]` where the agent wrote to the forge (AGL-008).
    pub fn show_forge(&self, name: &str, kind: ForgeKind, section: Option<&str>) {
        let message = match kind {
            ForgeKind::Created => tr!(self.lang, m::FORGE_EVENT_CREATED, name = name),
            ForgeKind::Edited => tr!(self.lang, m::FORGE_EVENT_EDITED, name = name),
            ForgeKind::Appended => tr!(
                self.lang,
                m::FORGE_EVENT_APPENDED,
                name = name,
                section = section.unwrap_or_default()
            ),
            ForgeKind::Removed => tr!(self.lang, m::FORGE_EVENT_REMOVED, name = name),
            ForgeKind::StatusChanged => tr!(
                self.lang,
                m::FORGE_EVENT_STATUS,
                name = name,
                to = section.unwrap_or_default()
            ),
            ForgeKind::Renamed => tr!(
                self.lang,
                m::FORGE_EVENT_RENAMED,
                name = name,
                old = section.unwrap_or_default()
            ),
        };
        self.screen
            .print(&[self.theme.paint(Role::Muted, &message)]);
    }

    /// `[memory: +name]` where the memory was written, then the text that
    /// was written, muted (AGL-006, M28).
    pub fn show_memory(&self, name: &str, kind: crate::memory::Kind, text: Option<&str>) {
        let change = format!("{}{name}", kind.sign());
        let mut lines = vec![self.theme.paint(
            Role::Muted,
            &tr!(self.lang, m::MEMORY_EVENT, change = change),
        )];
        if let Some(text) = text {
            lines.extend(notice_lines(
                &self.theme,
                text,
                self.screen.columns(),
                Role::Muted,
            ));
        }
        self.screen.print(&lines);
    }

    pub fn show_thinking(&self, thinking: &str) {
        let mut wrap = StreamWrap::new(self.screen.columns());
        let mut lines = wrap.push(&format!("{}: {thinking}", tr!(self.lang, m::THINKING)));
        lines.extend(wrap.finish());
        let lines: Vec<String> = lines
            .iter()
            .map(|line| self.theme.paint(Role::Muted, line))
            .collect();
        self.screen.print(&lines);
    }

    /// Text before a call, rendered as Markdown.
    pub fn show_text(&self, text: &str) {
        let mut lines = markdown::render(text.trim(), self.screen.columns(), &self.theme);
        lines.push(String::new());
        self.screen.print(&lines);
    }

    /// The answer between rules, with the time when it is known.
    pub fn show_answer(&self, text: &str, elapsed: Option<Duration>) {
        let items: Vec<String> = elapsed.iter().map(|time| self.worked_for(*time)).collect();
        self.screen
            .print(&self.answer_lines(text, self.edge(None, &items, Role::InputRule)));
    }

    pub fn show_card(&self, call: &ToolCall, result: &ToolResult, elapsed: Option<Duration>) {
        self.screen.print(&self.card(call, result, elapsed, &[]));
    }

    /// A line of the session itself: help, a stop, an error.
    pub fn show_notice(&self, text: &str, role: Role) {
        self.screen.print(&notice_lines(
            &self.theme,
            text,
            self.screen.columns(),
            role,
        ));
    }

    /// Shows what a session already holds (D126).
    pub fn replay(&self, events: &[Event]) {
        let mut calls: Vec<&ToolCall> = Vec::new();
        let mut state_entered = false;
        for event in events {
            match event {
                Event::User(message) => {
                    state_entered = false;
                    self.show_user(message);
                }
                // Only a change of state is shown (AGL-046, D13).
                Event::State(state) => {
                    if std::mem::replace(&mut state_entered, true) {
                        self.show_state(state);
                    }
                }
                Event::Thinking(thinking) => self.show_thinking(thinking),
                Event::Call { text, call } => {
                    if let Some(text) = text {
                        self.show_text(text);
                    }
                    calls.push(call);
                }
                Event::Result {
                    id,
                    outcome,
                    content,
                } => {
                    if let Some(call) = calls.iter().find(|call| call.id == *id) {
                        let result = ToolResult {
                            outcome: *outcome,
                            content: content.clone(),
                        };
                        self.show_card(call, &result, None);
                    }
                }
                Event::Answer(text) => self.show_answer(text, None),
                Event::Compact { .. } => self.show_compacted(),
                Event::CutOff(cut) => self.show_cut_off(cut),
                Event::Forge {
                    name,
                    kind,
                    section,
                } => self.show_forge(name, *kind, section.as_deref()),
                Event::Memory { name, kind, text } => {
                    self.show_memory(name, *kind, text.as_deref());
                }
                Event::Media(medium) => self.show_media(medium),
                Event::Stir { attempt, error } => self.show_stir(*attempt, error),
                Event::Stop(_) => {}
            }
        }
    }

    // Lines.

    fn answer_lines(&self, text: &str, edge: String) -> Vec<String> {
        let columns = self.screen.columns();
        let rule = self.theme.paint(Role::InputRule, &"─".repeat(columns));
        let mut lines = vec![String::new(), rule];
        lines.push(String::new());
        lines.extend(markdown::render(text.trim(), columns, &self.theme));
        lines.push(edge);
        lines.push(String::new());
        lines
    }

    fn worked_for(&self, elapsed: Duration) -> String {
        tr!(
            self.lang,
            m::WORKED_FOR,
            time = texts::duration(elapsed.as_secs())
        )
    }

    /// The phases of a step as items of its rule (AGL-045).
    fn phase_items(&self, phases: &[(Activity, Duration)]) -> Vec<String> {
        phases
            .iter()
            .map(|(phase, time)| {
                let mut item = format!(
                    "{} {}",
                    phase.label(self.lang),
                    texts::duration(time.as_secs())
                );
                // The speed of the reading (AGL-046): the server's own, so a
                // cached part of the prompt does not count.
                if *phase == Activity::Prefill
                    && let Some(prefill) = self.prefill.filter(|p| p.tokens > 0)
                {
                    let speed = prefill.tokens as f64 * 1000.0 / prefill.ms;
                    item.push_str(&format!(" ({})", texts::speed(speed, self.lang)));
                }
                item
            })
            .collect()
    }

    /// A rule that carries a status and the items of the row. A row too
    /// long for the width is left to wrap (AGL-045).
    fn edge(&self, status: Option<(&str, Role)>, items: &[String], role: Role) -> String {
        let mut parts: Vec<(String, Role)> = vec![("─".into(), role)];
        if status.is_some() || !items.is_empty() {
            parts.push((" ".into(), role));
            if let Some((text, status_role)) = status {
                parts.push((text.into(), status_role));
                if !items.is_empty() {
                    parts.push((" · ".into(), role));
                }
            }
            if !items.is_empty() {
                parts.push((items.join(" · "), role));
            }
            parts.push((" ".into(), role));
        }
        let used: usize = parts.iter().map(|(text, _)| width(text)).sum();
        parts.push(("─".repeat(self.screen.columns().saturating_sub(used)), role));
        parts
            .iter()
            .map(|(text, role)| self.theme.paint(*role, text))
            .collect()
    }

    fn card(
        &self,
        call: &ToolCall,
        result: &ToolResult,
        elapsed: Option<Duration>,
        phases: &[(Activity, Duration)],
    ) -> Vec<String> {
        let columns = self.screen.columns();
        let gutter = self.theme.paint(Role::InputRule, "  │ ");
        let gutter_width = 4;
        let inner = columns.saturating_sub(gutter_width).max(1);
        let (status_text, status_role) = match result.outcome {
            Outcome::Ok => ("ok", Role::StatusSuccess),
            Outcome::Fail => ("fail", Role::StatusFailure),
            Outcome::Timeout => ("timeout", Role::StatusPending),
        };
        let mut lines = Vec::new();
        if self.settings.frame {
            lines.push(String::new());
            lines.push(self.theme.paint(Role::Rule, &"─".repeat(columns)));
            lines.push(String::new());
        }
        lines.push(format!(
            "{}  {}",
            self.theme.paint(
                Role::Tool,
                &format!("{} {}", tr!(self.lang, m::TOOL_LABEL), call.name)
            ),
            self.theme.paint(status_role, status_text)
        ));
        let mut limited = false;
        let sections = [call_input(call), result.content.clone()];
        for (index, section) in sections.iter().enumerate() {
            let (preview, cut) =
                preview(section, self.settings.tool_lines, self.settings.tool_chars);
            limited |= cut;
            if index == 1 && !preview.is_empty() {
                lines.push(gutter.trim_end().to_owned());
            }
            for line in preview {
                for row in hard_wrap(&clean(&line), inner) {
                    lines.push(format!("{gutter}{row}"));
                }
            }
        }
        if limited {
            lines.push(self.theme.paint(
                Role::Muted,
                &tr!(
                    self.lang,
                    m::LIMITED,
                    lines = self.settings.tool_lines,
                    chars = self.settings.tool_chars,
                ),
            ));
        }
        if self.settings.frame {
            let mut items = self.phase_items(phases);
            if let Some(elapsed) = elapsed {
                items.push(format!(
                    "{} {}",
                    call.name,
                    texts::duration(elapsed.as_secs())
                ));
            }
            lines.push(self.edge(Some((status_text, status_role)), &items, Role::Rule));
        }
        lines
    }

    // Streaming.

    /// A piece of the generation being decoded: the live speed for the
    /// dock (AGL-012).
    fn piece(&mut self) {
        let now = Instant::now();
        let generation = self.generation.get_or_insert_with(|| Generation {
            window: VecDeque::new(),
            started: now,
        });
        generation.window.push_back(now);
        trim_window(&mut generation.window, now);
        self.screen.speed(window_speed(&generation.window));
    }

    /// The generation being decoded is done: its time goes to the turn
    /// total and the live speed clears (AGL-012).
    fn generation_done(&mut self) {
        if let Some(generation) = self.generation.take() {
            self.turn_decoding += generation.started.elapsed();
        }
        self.screen.speed(None);
    }

    /// The whole-turn decoding speed for the answer's rule row (AGL-012).
    fn turn_speed(&self) -> Option<f64> {
        let seconds = self.turn_decoding.as_secs_f64();
        (self.turn_tokens > 0 && seconds > 0.0).then(|| self.turn_tokens as f64 / seconds)
    }

    fn close_thinking(&mut self) {
        if let Some(mut wrap) = self.thinking.take() {
            if let Some(rest) = wrap.finish() {
                self.screen.print(&[self.theme.paint(Role::Muted, &rest)]);
            }
            self.screen.live("");
        }
    }

    /// Replaces the streamed lines with the rendered block when they are
    /// still on the screen, else prints the block below them (D123).
    fn replace_streamed(&mut self, block: Vec<String>) {
        if let Some(mut streamed) = self.action.take() {
            if let Some(rest) = streamed.wrap.finish() {
                streamed.lines += 1;
                self.screen.print(&[rest]);
            }
            self.screen.live("");
            if streamed.columns == self.screen.columns() {
                self.screen.erase(streamed.lines);
            }
        }
        self.screen.print(&block);
    }
}

impl Report for Presenter {
    fn state(&mut self, state: &str) {
        self.close();
        if std::mem::replace(&mut self.state_entered, true) {
            self.show_state(state);
        }
    }

    fn thinking(&mut self, piece: &str) {
        if self.thinking.is_none() {
            self.close();
            self.thinking = Some(StreamWrap::new(self.screen.columns()));
            let label = format!("{}: ", tr!(self.lang, m::THINKING));
            let wrap = self.thinking.as_mut().expect("just set");
            let lines = wrap.push(&label);
            debug_assert!(lines.is_empty());
        }
        let wrap = self.thinking.as_mut().expect("open");
        let lines: Vec<String> = wrap
            .push(piece)
            .iter()
            .map(|line| self.theme.paint(Role::Muted, line))
            .collect();
        if !lines.is_empty() {
            self.screen.print(&lines);
        }
        self.screen
            .live(&self.theme.paint(Role::Muted, wrap.live()));
        self.piece();
    }

    fn thinking_done(&mut self) {
        self.close_thinking();
        self.generation_done();
    }

    fn action_text(&mut self, piece: &str) {
        self.close_thinking();
        let columns = self.screen.columns();
        let streamed = self.action.get_or_insert_with(|| Streamed {
            wrap: StreamWrap::new(columns),
            lines: 0,
            columns,
        });
        let lines = streamed.wrap.push(piece);
        streamed.lines += lines.len();
        if !lines.is_empty() {
            self.screen.print(&lines);
        }
        self.screen.live(streamed.wrap.live());
        self.piece();
    }

    fn text(&mut self, text: &str) {
        self.generation_done();
        let mut block = markdown::render(text.trim(), self.screen.columns(), &self.theme);
        block.push(String::new());
        self.replace_streamed(block);
    }

    fn answer(&mut self, text: &str) {
        self.close_thinking();
        self.generation_done();
        let phases = std::mem::take(&mut self.phases);
        let mut items = self.phase_items(&phases);
        items.push(self.worked_for(self.turn_started.elapsed()));
        if let Some(speed) = self.turn_speed() {
            items.push(texts::speed(speed, self.lang));
        }
        let status = Some(("ok", Role::StatusSuccess));
        let edge = self.edge(status, &items, Role::InputRule);
        let block = self.answer_lines(text, edge);
        self.replace_streamed(block);
        self.prefill = None;
    }

    fn call(&mut self, _call: &ToolCall) {
        self.close();
    }

    fn result(&mut self, call: &ToolCall, result: &ToolResult) {
        let phases = std::mem::take(&mut self.phases);
        self.screen
            .print(&self.card(call, result, Some(self.tool_time), &phases));
        self.prefill = None;
    }

    fn forge(&mut self, name: &str, kind: ForgeKind, section: Option<&str>) {
        self.close();
        self.show_forge(name, kind, section);
    }

    fn compacting(&mut self) {
        self.close();
        self.compact_before = self.context;
        self.context_fresh = false;
    }

    fn compacted(&mut self) {
        let mut items: Vec<String> = Vec::new();
        let label = Activity::Compacting.label(self.lang);
        items.push(match self.compact_time {
            Some(time) => format!("{label} {}", texts::duration(time.as_secs())),
            None => label,
        });
        if self.context_fresh
            && let Some(after) = self.context
        {
            let after = grouped(after);
            items.push(match self.compact_before {
                Some(before) => tr!(
                    self.lang,
                    m::COMPACTED_TOKENS,
                    before = grouped(before),
                    after = after
                ),
                None => tr!(self.lang, m::COMPACTED_AFTER, after = after),
            });
        }
        self.screen.print(&[self.edge(
            Some(("ok", Role::StatusSuccess)),
            &items,
            Role::Rule,
        )]);
    }

    fn cut_off(&mut self, cut: &CutOff) {
        self.close();
        self.show_cut_off(cut);
    }

    fn stirred(&mut self, attempt: u32, _max: u32, error: &str) {
        self.close();
        self.show_stir(attempt, error);
    }

    fn prompt_read(&mut self, prefill: Prefill) {
        self.prefill = Some(match self.prefill {
            Some(known) => Prefill {
                tokens: known.tokens + prefill.tokens,
                ms: known.ms + prefill.ms,
            },
            None => prefill,
        });
    }

    fn context_used(&mut self, tokens: usize) {
        self.context = Some(tokens);
        self.context_fresh = true;
        self.screen.context(tokens);
    }

    /// The dock and the rows follow the FSM of the turn (AGL-046). A tool and
    /// a compaction have their own place on a row, and `Ready` is no work.
    fn transition(&mut self, transition: &Transition) {
        match transition.from {
            Phase::Ready => {}
            Phase::Running => self.tool_time = transition.time,
            Phase::Compacting => self.compact_time = Some(transition.time),
            _ => self
                .phases
                .push((self.activity.clone(), transition.time)),
        }
        self.activity = activity_of(transition);
        self.screen.activity(self.activity.clone());
        if transition.violation {
            self.show_notice(
                &tr!(
                    self.lang,
                    m::PHASE_VIOLATION,
                    event = transition.event.name(),
                    from = transition.from.name()
                ),
                Role::StatusFailure,
            );
        }
    }

    fn decoded(&mut self, tokens: usize) {
        self.turn_tokens += tokens;
    }
}

/// What the dock says after `transition`.
fn activity_of(transition: &Transition) -> Activity {
    match (transition.to, &transition.event) {
        (Phase::Ready, _) => Activity::Ready,
        (Phase::StartingServer, _) => Activity::StartingServer,
        (Phase::Transcribing, _) => Activity::Transcribing,
        (Phase::Thinking, _) => Activity::Thinking,
        (Phase::Writing, _) => Activity::Writing,
        (Phase::Compacting, _) => Activity::Compacting,
        (Phase::Running, PhaseEvent::ToolStart { tool }) => Activity::Running(tool.clone()),
        (Phase::Retry, PhaseEvent::Stir { attempt, max }) => Activity::Retry {
            attempt: *attempt,
            max: *max,
        },
        _ => Activity::Prefill,
    }
}

/// Keeps in `window` the pieces of the last second (AGL-012).
fn trim_window(window: &mut VecDeque<Instant>, now: Instant) {
    while window.len() > 1
        && now
            .duration_since(*window.front().expect("checked"))
            .as_secs_f64()
            > 1.0
    {
        window.pop_front();
    }
}

/// The live speed of `window`: pieces per second, none while it holds
/// fewer than two pieces or no time passed (AGL-012).
fn window_speed(window: &VecDeque<Instant>) -> Option<f64> {
    if window.len() < 2 {
        return None;
    }
    let span = window
        .back()
        .expect("two or more")
        .duration_since(*window.front().expect("two or more"))
        .as_secs_f64();
    (span > 0.0).then(|| window.len() as f64 / span)
}

/// `text` as wrapped lines painted with `role`.
pub fn notice_lines(theme: &Theme, text: &str, columns: usize, role: Role) -> Vec<String> {
    text.lines()
        .flat_map(|line| hard_wrap(&clean(line), columns))
        .map(|line| theme.paint(role, &line))
        .collect()
}

/// The user's message as rows on the input background (D138).
pub fn user_lines(theme: &Theme, message: &str, columns: usize) -> Vec<String> {
    let columns = columns.max(4);
    let background = theme.start(Role::InputBackground);
    let reset = theme.reset();
    let mut rows = Vec::new();
    for line in clean(message).trim_end().split('\n') {
        rows.extend(wrap_line(line, columns - 2));
    }
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let marker = if index == 0 { "› " } else { "  " };
            let pad = " ".repeat(columns - 2 - width(row));
            format!(
                "{background}{}{background}{}{background}{pad}{reset}",
                theme.paint(Role::InputPrompt, marker),
                theme.paint(Role::InputText, row)
            )
        })
        .collect()
}

/// What the card shows of a call: the command, or the file and the change.
fn call_input(call: &ToolCall) -> String {
    let text = |name: &str| {
        call.args
            .iter()
            .find_map(|(n, value)| match value {
                Value::Text(text) if n == name => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or_default()
    };
    if call.name == "edit" {
        let mut out = text("path").to_owned();
        for (sign, name) in [('-', "old"), ('+', "new")] {
            for line in text(name).split('\n') {
                out.push_str(&format!("\n{sign} {line}"));
            }
        }
        return out;
    }
    if call.name == "search" {
        return search_input(call);
    }
    if call.name == "memory" {
        return crate::cli::report::call_summary(call);
    }
    text("command").to_owned()
}

/// What a `search` call shows: the source and the query.
fn search_input(call: &ToolCall) -> String {
    let text = |name: &str| {
        call.args.iter().find_map(|(n, value)| match value {
            Value::Text(text) if n == name => Some(text.as_str()),
            _ => None,
        })
    };
    format!(
        "{}: {}",
        text("source").unwrap_or_default(),
        text("query").unwrap_or_default()
    )
}

/// The first `lines` lines of `text`, at most `chars` characters in all;
/// true when something was left out (D124).
fn preview(text: &str, lines: usize, chars: usize) -> (Vec<String>, bool) {
    let text = text.trim_end_matches('\n');
    if text.is_empty() {
        return (Vec::new(), false);
    }
    let mut out = Vec::new();
    let mut left = chars;
    let mut cut = false;
    for (index, line) in text.split('\n').enumerate() {
        if index == lines || left == 0 {
            cut = true;
            break;
        }
        let count = line.chars().count();
        if count > left {
            out.push(line.chars().take(left).collect());
            cut = true;
            break;
        }
        out.push(line.to_owned());
        left -= count;
    }
    (out, cut)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::phase::{Observed, Observer};

    /// A screen that records what was drawn.
    #[derive(Default)]
    struct Mock {
        ops: Mutex<Vec<String>>,
        printed: Mutex<Vec<String>>,
    }

    impl Screen for Mock {
        fn columns(&self) -> usize {
            40
        }
        fn print(&self, lines: &[String]) {
            self.printed.lock().unwrap().extend_from_slice(lines);
            self.ops
                .lock()
                .unwrap()
                .push(format!("print {}", lines.len()));
        }
        fn live(&self, text: &str) {
            self.ops.lock().unwrap().push(format!("live {text:?}"));
        }
        fn erase(&self, count: usize) -> bool {
            let mut printed = self.printed.lock().unwrap();
            let keep = printed.len() - count;
            printed.truncate(keep);
            self.ops.lock().unwrap().push(format!("erase {count}"));
            true
        }
        fn activity(&self, activity: Activity) {
            self.ops.lock().unwrap().push(format!("{activity:?}"));
        }
        fn speed(&self, speed: Option<f64>) {
            self.ops.lock().unwrap().push(format!("speed {speed:?}"));
        }
    }

    fn presenter(frame: bool) -> (Presenter, Arc<Mock>) {
        let mock = Arc::new(Mock::default());
        let settings = ReplSettings {
            tool_lines: 2,
            tool_chars: 20,
            frame,
            max_record_seconds: 60,
            start_server: true,
        };
        let presenter = Presenter::new(mock.clone(), Theme::plain(), settings, Lang::En);
        (presenter, mock)
    }

    fn bash(command: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: vec![("command".into(), Value::Text(command.into()))],
        }
    }

    #[test]
    fn previews_stop_at_the_first_limit() {
        assert_eq!(
            preview("a\nb\nc", 2, 100),
            (vec!["a".into(), "b".into()], true)
        );
        assert_eq!(
            preview("a\nb", 2, 100),
            (vec!["a".into(), "b".into()], false)
        );
        assert_eq!(preview("abcdef\nx", 5, 4), (vec!["abcd".into()], true));
        assert_eq!(
            preview("ab\ncd", 5, 4),
            (vec!["ab".into(), "cd".into()], false)
        );
        assert_eq!(preview("", 5, 4), (Vec::new(), false));
        assert_eq!(preview("a\n\n", 5, 4), (vec!["a".into()], false));
    }

    #[test]
    fn framed_card() {
        let (presenter, mock) = presenter(true);
        let result = ToolResult {
            outcome: Outcome::Fail,
            content: "one\ntwo\nthree\nexit code: 1".into(),
        };
        presenter.show_card(&bash("cargo test"), &result, Some(Duration::from_secs(75)));
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(
            printed,
            [
                "",
                &"─".repeat(40),
                "",
                "TOOL bash  fail",
                "  │ cargo test",
                "  │",
                "  │ one",
                "  │ two",
                "… limited to 2 lines / 20 chars",
                &format!("─ fail · bash 1m 15s {}", "─".repeat(19)),
            ]
        );
    }

    #[test]
    fn unframed_card_and_edit_input() {
        let (presenter, mock) = presenter(false);
        let call = ToolCall {
            id: "c2".into(),
            name: "edit".into(),
            args: vec![
                ("path".into(), Value::Text("a.rs".into())),
                ("old".into(), Value::Text("x".into())),
                ("new".into(), Value::Text("y".into())),
            ],
        };
        let result = ToolResult {
            outcome: Outcome::Ok,
            content: "Replaced 1 occurrence in a.rs.".into(),
        };
        presenter.show_card(&call, &result, None);
        let printed = mock.printed.lock().unwrap().clone();
        // The input is cut at 2 lines; the output follows.
        assert_eq!(printed[0], "TOOL edit  ok");
        assert_eq!(printed[1], "  │ a.rs");
        assert_eq!(printed[2], "  │ - x");
        assert!(printed.contains(&"… limited to 2 lines / 20 chars".to_owned()));
        assert!(!printed.iter().any(|line| line.starts_with('─')));
    }

    #[test]
    fn answer_between_rules() {
        let (presenter, mock) = presenter(true);
        presenter.show_answer("**Done**", Some(Duration::from_secs(5)));
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(
            printed,
            [
                "".to_owned(),
                "─".repeat(40),
                "".into(),
                "Done".into(),
                format!("─ Worked for 5s {}", "─".repeat(24)),
                "".into(),
            ]
        );
        mock.printed.lock().unwrap().clear();
        presenter.show_answer("old", None);
        assert_eq!(mock.printed.lock().unwrap()[4], "─".repeat(40));
    }

    #[test]
    fn streamed_text_is_replaced_by_the_rendered_answer() {
        let (mut presenter, mock) = presenter(true);
        presenter.begin_turn();
        presenter.thinking("hmm ");
        presenter.thinking("ok\n");
        presenter.thinking_done();
        presenter.action_text("**Hi** the");
        presenter.action_text("re\nmore");
        presenter.answer("**Hi** there\nmore");
        let ops = mock.ops.lock().unwrap().clone();
        // The two streamed lines are erased, then the block is printed.
        assert!(ops.contains(&"erase 2".to_owned()), "{ops:?}");
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(printed[0], "thinking: hmm ok");
        assert_eq!(printed[1], "");
        assert_eq!(printed[2], "─".repeat(40));
        assert!(printed.contains(&"Hi there more".to_owned()));
        assert!(!printed.contains(&"**Hi** there".to_owned()));
    }

    /// The row of `printed` that starts with `start`, without its fill.
    fn row(printed: &[String], start: &str) -> String {
        printed
            .iter()
            .find(|line| line.starts_with(start))
            .expect("the row")
            .trim_end_matches('─')
            .trim_end()
            .to_owned()
    }

    #[test]
    fn the_rule_of_a_step_names_its_phases() {
        // AGL-045: the phases in order, each with its time; then the tool
        // on a card, the turn's time on the answer.
        let (mut presenter, mock) = presenter(true);
        presenter.begin_turn();
        let mut observer = Observer::new();
        observer.enter(PhaseEvent::ServerStart, &mut presenter);
        let mut report = Observed {
            observer: &mut observer,
            report: &mut presenter,
        };
        report.prefill();
        report.prefill();
        report.thinking("hmm ");
        report.thinking_done();
        report.action_text("Looking.");
        report.call(&bash("ls"));
        let ok = ToolResult {
            outcome: Outcome::Ok,
            content: "a".into(),
        };
        report.result(&bash("ls"), &ok);
        assert_eq!(
            row(&mock.printed.lock().unwrap(), "─ ok"),
            "─ ok · Starting the server <1s · Reading the prompt <1s · Thinking <1s · \
             Writing <1s · bash <1s"
        );
        // The next step begins with no phase of the last one; a retry
        // keeps the order of what happened.
        mock.printed.lock().unwrap().clear();
        report.action_text("Oops");
        report.stirred(1, 2, "`nope` is not a tool of this state");
        report.action_text("Done");
        report.answer("Done");
        assert_eq!(
            row(&mock.printed.lock().unwrap(), "─ ok"),
            "─ ok · Reading the prompt <1s · Writing <1s · Retry 1/2 <1s · Writing <1s · \
             Worked for <1s"
        );
    }

    #[test]
    fn a_new_turn_forgets_the_phases_of_the_last() {
        // AGL-045: an interrupted turn leaves its phases behind.
        let (mut presenter, mock) = presenter(true);
        presenter.begin_turn();
        let mut first = Observer::new();
        let mut report = Observed {
            observer: &mut first,
            report: &mut presenter,
        };
        report.prefill();
        report.thinking("hmm");
        report.end(PhaseEvent::Failed);
        presenter.close();
        presenter.begin_turn();
        let mut second = Observer::new();
        let mut report = Observed {
            observer: &mut second,
            report: &mut presenter,
        };
        report.prefill();
        report.answer("Done");
        assert_eq!(
            row(&mock.printed.lock().unwrap(), "─ ok"),
            "─ ok · Reading the prompt <1s · Worked for <1s"
        );
    }

    #[test]
    fn a_compaction_has_its_own_row_and_leaves_the_steps_row() {
        // AGL-046, D13: its time and the tokens before and after; the next
        // step's row does not repeat it.
        let (mut presenter, mock) = presenter(true);
        presenter.begin_turn();
        let mut observer = Observer::new();
        let mut report = Observed {
            observer: &mut observer,
            report: &mut presenter,
        };
        report.prefill();
        report.context_used(52_400);
        report.compacting();
        report.context_used(3_100);
        report.compacted();
        assert_eq!(
            row(&mock.printed.lock().unwrap(), "─ ok"),
            "─ ok · Compacting <1s · 52\u{a0}400 → 3\u{a0}100 tokens"
        );
        mock.printed.lock().unwrap().clear();
        report.action_text("Done");
        report.answer("Done");
        let answer = row(&mock.printed.lock().unwrap(), "─ ok");
        assert!(!answer.contains("Compacting"), "{answer}");
        // A count that failed leaves the tokens out; so does a replay.
        mock.printed.lock().unwrap().clear();
        presenter.begin_turn();
        presenter.compacting();
        presenter.compacted();
        presenter.show_compacted();
        let printed = mock.printed.lock().unwrap();
        assert!(!printed[0].contains("tokens"), "{:?}", printed[0]);
        assert_eq!(row(&printed, "─ ok"), "─ ok · Compacting <1s");
        assert_eq!(printed[1].trim_end_matches('─').trim_end(), "─ ok · Compacting");
    }

    #[test]
    fn the_reading_of_the_prompt_shows_its_speed() {
        // AGL-046: the server's figure, over the step's generations; a step
        // whose prompt was all cached shows none.
        let (mut presenter, mock) = presenter(true);
        presenter.begin_turn();
        let mut observer = Observer::new();
        let mut report = Observed {
            observer: &mut observer,
            report: &mut presenter,
        };
        report.prefill();
        report.prompt_read(Prefill {
            tokens: 900,
            ms: 800.0,
        });
        report.prompt_read(Prefill {
            tokens: 100,
            ms: 200.0,
        });
        report.call(&bash("ls"));
        let ok = ToolResult {
            outcome: Outcome::Ok,
            content: "a".into(),
        };
        report.result(&bash("ls"), &ok);
        assert_eq!(
            row(&mock.printed.lock().unwrap(), "─ ok"),
            "─ ok · Reading the prompt <1s (1000.0 tok/s) · bash <1s"
        );
        mock.printed.lock().unwrap().clear();
        report.prefill();
        report.prompt_read(Prefill {
            tokens: 0,
            ms: 3.0,
        });
        report.answer("Done");
        assert_eq!(
            row(&mock.printed.lock().unwrap(), "─ ok"),
            "─ ok · Reading the prompt <1s · Worked for <1s"
        );
    }

    #[test]
    fn only_a_change_of_state_is_shown() {
        // AGL-046, D13.
        let (mut presenter, mock) = presenter(true);
        presenter.begin_turn();
        presenter.state("inference");
        assert!(mock.printed.lock().unwrap().is_empty());
        presenter.state("verify");
        assert_eq!(*mock.printed.lock().unwrap(), ["[verify]"]);
        // A new turn starts over, in a replay too.
        mock.printed.lock().unwrap().clear();
        presenter.begin_turn();
        presenter.state("inference");
        assert!(mock.printed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_transition_that_must_not_happen_is_marked_and_the_turn_goes_on() {
        // AGL-046, D11.
        let (mut presenter, mock) = presenter(true);
        presenter.transition(&Transition {
            from: Phase::Writing,
            to: Phase::Thinking,
            event: PhaseEvent::FirstThinking,
            time: Duration::ZERO,
            violation: true,
        });
        let printed = mock.printed.lock().unwrap().join("\n");
        assert!(printed.contains("first_thinking"), "{printed}");
        assert!(printed.contains("Writing"), "{printed}");
    }

    #[test]
    fn the_status_on_a_rule_is_painted_in_its_role() {
        // AGL-045: `ok` keeps the color of `status_success`.
        let theme = Theme::new(true, &[]);
        let (presenter, mock) = presenter(true);
        let presenter = Presenter {
            theme: theme.clone(),
            ..presenter
        };
        presenter.show_card(
            &bash("ls"),
            &ToolResult {
                outcome: Outcome::Ok,
                content: "a".into(),
            },
            Some(Duration::from_secs(1)),
        );
        let green = theme.paint(Role::StatusSuccess, "ok");
        let printed = mock.printed.lock().unwrap();
        let rule = printed.last().expect("the rule");
        assert!(rule.contains(&green), "{rule:?}");
        assert!(rule.contains("bash 1s"), "{rule:?}");
    }

    #[test]
    fn the_dock_follows_the_phases_of_a_turn() {
        // AGL-043 F2: the start, the prefill, the thinking, the writing,
        // the call, the next prefill, a stir and the answer.
        let (mut presenter, mock) = presenter(true);
        presenter.begin_turn();
        let mut observer = Observer::new();
        observer.enter(PhaseEvent::ServerStart, &mut presenter);
        observer.enter(PhaseEvent::Transcribe, &mut presenter);
        let mut report = Observed {
            observer: &mut observer,
            report: &mut presenter,
        };
        report.prefill();
        report.thinking("hmm ");
        report.thinking_done();
        report.action_text("Looking.\n");
        report.text("Looking.");
        report.call(&bash("ls"));
        report.result(
            &bash("ls"),
            &ToolResult {
                outcome: Outcome::Ok,
                content: "a".into(),
            },
        );
        report.prefill();
        report.stirred(1, 2, "`nope` is not a tool of this state");
        report.action_text("Done");
        report.answer("Done");
        let ops: Vec<String> = mock
            .ops
            .lock()
            .unwrap()
            .iter()
            .filter(|op| {
                !["erase", "speed", "live", "print"]
                    .iter()
                    .any(|prefix| op.starts_with(prefix))
            })
            .cloned()
            .collect();
        assert_eq!(
            ops,
            [
                "StartingServer",
                "Transcribing",
                "Prefill",
                "Thinking",
                "Writing",
                "Running(\"bash\")",
                "Prefill",
                "Retry { attempt: 1, max: 2 }",
                "Writing",
                "Ready"
            ]
        );
    }

    #[test]
    fn text_before_a_call_stays_as_an_explanation() {
        let (mut presenter, mock) = presenter(true);
        presenter.action_text("Looking at it.\n");
        presenter.text("Looking at it.");
        presenter.call(&bash("ls"));
        presenter.result(
            &bash("ls"),
            &ToolResult {
                outcome: Outcome::Ok,
                content: "a".into(),
            },
        );
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(printed[0], "Looking at it.");
        assert_eq!(printed[1], "");
        assert!(printed.contains(&"TOOL bash  ok".to_owned()));
    }

    #[test]
    fn the_live_speed_needs_two_pieces_apart() {
        let t0 = Instant::now();
        assert_eq!(window_speed(&VecDeque::new()), None);
        let one: VecDeque<Instant> = [t0].into_iter().collect();
        assert_eq!(window_speed(&one), None);
        let two: VecDeque<Instant> = [t0, t0 + Duration::from_millis(500)].into_iter().collect();
        assert!((window_speed(&two).unwrap() - 4.0).abs() < 0.5);
    }

    #[test]
    fn the_window_keeps_the_pieces_of_the_last_second() {
        let t0 = Instant::now();
        let mut window: VecDeque<Instant> = [
            t0,
            t0 + Duration::from_millis(400),
            t0 + Duration::from_millis(1200),
        ]
        .into_iter()
        .collect();
        trim_window(&mut window, t0 + Duration::from_millis(1200));
        assert_eq!(window.len(), 2);
        assert_eq!(
            window.front().copied(),
            Some(t0 + Duration::from_millis(400))
        );
        let mut one: VecDeque<Instant> = [t0].into_iter().collect();
        trim_window(&mut one, t0 + Duration::from_secs(5));
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn the_live_speed_shows_from_the_second_piece_and_clears() {
        let (mut presenter, mock) = presenter(true);
        presenter.begin_turn();
        presenter.thinking("a");
        assert_eq!(mock.ops.lock().unwrap().last().unwrap(), "speed None");
        presenter.thinking("b");
        let ops = mock.ops.lock().unwrap().clone();
        assert!(
            ops.iter().any(|op| op.starts_with("speed Some(")),
            "{ops:?}"
        );
        presenter.thinking_done();
        assert_eq!(mock.ops.lock().unwrap().last().unwrap(), "speed None");
    }

    #[test]
    fn the_answer_rule_carries_the_turns_decoding_speed() {
        let (mut presenter, mock) = presenter(true);
        let t0 = Instant::now();
        presenter.begin_turn();
        presenter.thinking("hmm");
        presenter.decoded(30);
        presenter.thinking_done();
        presenter.action_text("Done.");
        presenter.decoded(20);
        presenter.answer("Done.");
        let measured = t0.elapsed().as_secs_f64();
        let printed = mock.printed.lock().unwrap().clone();
        let rule = printed
            .iter()
            .find(|line| line.contains("Worked for"))
            .expect("the answer rule")
            .clone();
        assert!(rule.contains("tok/s"), "{rule:?}");
        let figure: f64 = rule
            .rsplit('·')
            .next()
            .expect("the speed part")
            .split_whitespace()
            .next()
            .expect("the figure")
            .parse()
            .expect("a number");
        // The whole turn decoded 50 tokens; the figure is at least
        // those over everything the turn could have taken.
        if measured > 0.0 {
            assert!(figure >= 50.0 / measured - 1e-9);
        }
        // What is replayed carries no speed.
        mock.printed.lock().unwrap().clear();
        presenter.show_answer("old", Some(Duration::from_secs(5)));
        let rule = mock
            .printed
            .lock()
            .unwrap()
            .iter()
            .find(|line| line.contains("Worked for"))
            .expect("the replay rule")
            .clone();
        assert!(!rule.contains("tok/s"), "{rule:?}");
    }

    #[test]
    fn user_messages_fill_the_row() {
        let rows = user_lines(&Theme::plain(), "hi\nthere", 10);
        assert_eq!(rows, ["› hi      ", "  there   "]);
        let rows = user_lines(&Theme::plain(), "abcdefghijkl", 10);
        assert_eq!(rows, ["› abcdefgh", "  ijkl    "]);
        let rows = user_lines(&Theme::plain(), "one two three", 10);
        assert_eq!(rows, ["› one two ", "  three   "]);
    }

    #[test]
    fn a_resumed_session_is_replayed() {
        let (presenter, mock) = presenter(false);
        let events = [
            Event::User("go".into()),
            Event::State("work".into()),
            Event::Thinking("hm".into()),
            Event::Call {
                text: Some("Running.".into()),
                call: bash("ls"),
            },
            Event::Result {
                id: "c1".into(),
                outcome: Outcome::Ok,
                content: "a".into(),
            },
            Event::Forge {
                name: "agl-009".into(),
                kind: ForgeKind::Appended,
                section: Some("Progress".into()),
            },
            Event::Memory {
                name: "build-and-test".into(),
                kind: crate::memory::Kind::Added,
                text: Some("Run cargo test.".into()),
            },
            Event::Media(crate::media::Media {
                kind: crate::media::Kind::Audio,
                number: 1,
                sha256: "a".repeat(64),
                ext: "wav".into(),
                name: "voice.wav".into(),
                transcript: Some("Check the logs.".into()),
                frames: Vec::new(),
            }),
            Event::Media(crate::media::Media {
                kind: crate::media::Kind::Video,
                number: 1,
                sha256: "b".repeat(64),
                ext: "mp4".into(),
                name: "clip.mp4".into(),
                transcript: None,
                frames: vec![
                    crate::media::Frame { at: 0.0, sha256: "c".repeat(64) },
                    crate::media::Frame { at: 1.0, sha256: "c".repeat(64) },
                ],
            }),
            Event::Stir {
                attempt: 1,
                error: "`nope` is not a tool of this state".into(),
            },
            Event::CutOff(CutOff {
                limit: 12288,
                thinking: Some(4000),
                discarded: 8288,
                call: true,
            }),
            Event::Answer("Done".into()),
        ];
        presenter.replay(&events);
        let printed = mock.printed.lock().unwrap().clone();
        // The state a turn starts in is not shown (AGL-046, D13).
        assert!(!printed.contains(&"[work]".to_owned()));
        // The memory marker, then the text written (AGL-006, M28).
        let marker = printed
            .iter()
            .position(|line| line == "[memory: +build-and-test]")
            .expect("the memory marker");
        assert_eq!(printed[marker + 1], "Run cargo test.");
        assert!(printed.contains(&"thinking: hm".to_owned()));
        assert!(printed.contains(&"Running.".to_owned()));
        assert!(printed.contains(&"TOOL bash  ok".to_owned()));
        assert!(printed.contains(&"[forge: agl-009 +Progress]".to_owned()));
        // A line per medium (AGL-043 F17).
        assert!(printed.contains(&"[Audio#1] voice.wav — “Check the logs.”".to_owned()));
        assert!(printed.contains(&"[Video#1] clip.mp4 — frames: 2".to_owned()));
        // The stir and the cut-off with what it discarded (AGL-043 F8).
        // The stir line wraps at the 40 columns of the test screen.
        let all = printed.join("\n");
        assert!(all.contains("[stir 1: `nope`"), "{all}");
        assert!(printed.contains(
            &"[cut off: 4000 tokens of thinking, 8288 of a tool call]".to_owned()
        ));
        assert!(printed.contains(&"Done".to_owned()));
        // No time is shown for what is replayed.
        assert!(!printed.iter().any(|line| line.contains("Worked for")));
    }
}
