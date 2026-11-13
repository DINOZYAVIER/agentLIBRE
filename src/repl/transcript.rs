//! What the transcript shows: user messages, states, thinking, streamed
//! text restyled as Markdown, tool cards, answers and stops (D123-D127,
//! D138). Drawing goes through [`Screen`].

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::dock::grouped;
use super::markdown;
use super::reading::{MIN_TOKENS_OF_A_SPEED, Reading};
use super::records::{self, Record};
use super::text::{StreamWrap, clean, hard_wrap, truncate, width, wrap_line};
use crate::agent::Report;
use crate::backend::{Prefill, PromptProgress, ToolCall};
use crate::i18n::{Lang, m, repl as texts};
use crate::phase::{Phase, PhaseEvent, Transition};
use crate::session::{CutOff, Event, ForgeKind, TransitionLine};
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
    /// The reading of the prompt going on (AGL-046, D19).
    fn reading(&self, _reading: Option<Reading>) {}
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
    /// The reading going on, and the real speed of an earlier one, for the
    /// dock (AGL-046, D19).
    reading: Option<Reading>,
    last_read_speed: Option<f64>,
    /// The prompt's last size, the one when a compaction began, and whether
    /// the last size came after that begin (AGL-046, D13).
    context: Option<usize>,
    compact_before: Option<usize>,
    context_fresh: bool,
    /// Whether the turn has entered its first state: only a change of state
    /// is shown (AGL-046, D13).
    state_entered: bool,
    /// The thinking being decoded, as its pieces arrive (D3, D19): the
    /// transcript shows its line with the record of its step.
    thinking: Option<String>,
    /// Whether the step's block has begun: the separator is printed once
    /// per step (D19).
    step_open: bool,
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
            reading: None,
            last_read_speed: None,
            context: None,
            compact_before: None,
            context_fresh: false,
            state_entered: false,
            thinking: None,
            step_open: false,
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
        self.step_open = false;
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

    /// The step's thinking's line (D3, D19, D20): `thinking: <the first
    /// line>`, on the top level; the label in the tool's color, the text
    /// in the terminal's plain color. The time is in the step's rule, the
    /// full text in the inspect mode.
    pub fn show_thinking(&self, thinking: &str) {
        let line = thinking_paint(
            &self.theme,
            self.lang,
            self.screen.columns(),
            thinking,
            Role::Tool,
        );
        self.screen.print(&[line]);
    }

    /// The text before a call (D9): its first three lines; the rest is in
    /// the inspect mode.
    pub fn show_text(&self, text: &str) {
        self.screen.print(&self.text_block(text));
    }

    /// The block of the text before a call (D9): its first three lines,
    /// as Markdown, with `…` when more.
    fn text_block(&self, text: &str) -> Vec<String> {
        let mut lines = markdown::render(text.trim(), self.screen.columns(), &self.theme);
        if lines.len() > 3 {
            lines.truncate(3);
            lines
                .last_mut()
                .expect("three lines")
                .push_str(&self.theme.paint(Role::Muted, "…"));
        }
        lines.push(String::new());
        lines
    }

    /// The answer between rules, with the time when it is known.
    pub fn show_answer(&self, text: &str, elapsed: Option<Duration>) {
        let items: Vec<String> = elapsed.iter().map(|time| self.worked_for(*time)).collect();
        self.screen
            .print(&self.answer_lines(text, self.edge(None, &items, Role::InputRule)));
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

    /// Shows what a session already holds (D126), through its records
    /// (AGL-066): compact by default, in full in the inspect mode.
    /// `transitions` carry the phases' times of the steps.
    pub fn replay(&self, events: &[Event], transitions: &[TransitionLine]) {
        for record in records::group(events, transitions, self.lang) {
            match &record {
                Record::User(message) => self.show_user(message),
                Record::State(state) => self.show_state(state),
                Record::Tool {
                    call,
                    result,
                    hint,
                    phases,
                    time,
                    thinking,
                    text,
                } => {
                    self.screen.print(&self.step_separator());
                    if let Some(thinking) = thinking {
                        self.show_thinking(thinking);
                    }
                    if let Some(text) = text {
                        self.show_text(text);
                    }
                    let mut items = records::phase_items(phases);
                    if let Some(elapsed) = time {
                        items.push(format!(
                            "{} {}",
                            call.name,
                            texts::duration(elapsed.as_secs())
                        ));
                    }
                    self.screen
                        .print(&self.card(call, result.as_ref(), hint, &items));
                }
                Record::Answer {
                    text,
                    time,
                    thinking,
                } => {
                    self.screen.print(&self.step_separator());
                    if let Some(thinking) = thinking {
                        self.show_thinking(thinking);
                    }
                    self.show_answer(text, *time);
                }
                Record::Notice { line, role, .. } => self.show_notice(line, *role),
            }
        }
    }

    // Lines.

    fn answer_lines(&self, text: &str, edge: String) -> Vec<String> {
        let columns = self.screen.columns();
        let mut lines = Vec::new();
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
                    && let Some(prefill) =
                        self.prefill.filter(|p| p.tokens >= MIN_TOKENS_OF_A_SPEED)
                {
                    let speed = prefill.tokens as f64 * 1000.0 / prefill.ms;
                    item.push_str(&format!(" ({})", texts::speed(speed, self.lang)));
                }
                item
            })
            .collect()
    }

    /// A rule that carries a status and the items of the row (AGL-045).
    fn edge(&self, status: Option<(&str, Role)>, items: &[String], role: Role) -> String {
        edge(&self.theme, self.screen.columns(), status, items, role)
    }

    /// The rule that begins a step's block, when the frame is on (D19).
    /// The block is the thinking's line, the text, and the tool's card;
    /// the rule goes before all of it, so the step reads as one unit.
    fn step_separator(&self) -> Vec<String> {
        if !self.settings.frame {
            return Vec::new();
        }
        vec![
            String::new(),
            self.theme
                .paint(Role::Rule, &"─".repeat(self.screen.columns())),
        ]
    }

    /// The compact frame of a finished step (AGL-066, D1, D2, D6): the
    /// tool's name and status, one line of the input, the phases' times.
    /// No output: the full data is in the inspect mode.
    fn card(
        &self,
        call: &ToolCall,
        result: Option<&ToolResult>,
        hint: &str,
        items: &[String],
    ) -> Vec<String> {
        let columns = self.screen.columns();
        let gutter = self.theme.paint(Role::InputRule, "  │ ");
        let inner = columns.saturating_sub(4).max(1);
        let (status_text, status_role) = records::status(result.map(|result| result.outcome));
        let mut lines = Vec::new();
        lines.push(format!(
            "{}  {}",
            self.theme.paint(
                Role::Tool,
                &format!("{} {}", tr!(self.lang, m::TOOL_LABEL), call.name)
            ),
            self.theme.paint(status_role, &status_text)
        ));
        for row in hard_wrap(&clean(hint), inner) {
            lines.push(format!("{gutter}{row}"));
        }
        if self.settings.frame {
            lines.push(self.edge(
                Some((status_text.as_str(), status_role)),
                items,
                Role::Rule,
            ));
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
        if let Some(thinking) = self.thinking.take() {
            self.step_block();
            self.show_thinking(&thinking);
            self.screen.live("");
        }
    }

    /// The block of a step begins (D19): the separator, once per step.
    fn step_block(&mut self) {
        if self.step_open {
            return;
        }
        self.step_open = true;
        self.screen.print(&self.step_separator());
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
            self.thinking = Some(String::new());
        }
        let thinking = self.thinking.as_mut().expect("just set");
        thinking.push_str(piece);
        // D20: the label keeps the muted color, the text is plain.
        let line = thinking_paint(
            &self.theme,
            self.lang,
            self.screen.columns(),
            thinking,
            Role::Muted,
        );
        self.screen.live(&line);
        self.piece();
    }

    fn thinking_done(&mut self) {
        self.close_thinking();
        self.generation_done();
    }

    fn action_text(&mut self, piece: &str) {
        self.close_thinking();
        self.step_block();
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
        self.replace_streamed(self.text_block(text));
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
        self.step_block();
        let block = self.answer_lines(text, edge);
        self.replace_streamed(block);
        self.step_open = false;
        self.prefill = None;
    }

    fn call(&mut self, _call: &ToolCall) {
        self.close();
    }

    fn result(&mut self, call: &ToolCall, result: &ToolResult) {
        let phases = std::mem::take(&mut self.phases);
        let mut items = self.phase_items(&phases);
        items.push(format!(
            "{} {}",
            call.name,
            texts::duration(self.tool_time.as_secs())
        ));
        let hint = records::call_hint(call, Some(result));
        self.step_block();
        self.screen
            .print(&self.card(call, Some(result), &hint, &items));
        self.step_open = false;
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
        // The step goes on as a new block (D19).
        self.step_open = false;
    }

    fn stirred(&mut self, attempt: u32, _max: u32, error: &str) {
        self.close();
        self.show_stir(attempt, error);
        // The retry is a new block of the step (D19).
        self.step_open = false;
    }

    fn prompt_progress(&mut self, progress: PromptProgress) {
        if let Some(reading) = &mut self.reading {
            reading.progress(progress, Instant::now());
            self.screen.reading(self.reading.clone());
        }
    }

    /// The first generation of a step is what the phase of the reading
    /// waited for; a later one reads what the step just wrote.
    fn prompt_read(&mut self, prefill: Prefill) {
        if self.prefill.is_some() {
            return;
        }
        self.prefill = Some(prefill);
        if prefill.tokens >= MIN_TOKENS_OF_A_SPEED {
            self.last_read_speed = Some(prefill.tokens as f64 * 1000.0 / prefill.ms);
        }
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
        self.reading = (transition.to == Phase::Prefill)
            .then(|| Reading::new(Instant::now(), self.last_read_speed));
        self.screen.reading(self.reading.clone());
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

/// A rule that carries a status and the items of the row. A row too long
/// for the width is left to wrap (AGL-045).
pub fn edge(
    theme: &Theme,
    columns: usize,
    status: Option<(&str, Role)>,
    items: &[String],
    role: Role,
) -> String {
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
    parts.push(("─".repeat(columns.saturating_sub(used)), role));
    parts
        .iter()
        .map(|(text, role)| theme.paint(*role, text))
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

/// The line of the thinking, its label painted for `role` and the text in
/// the plain color of the terminal (D20).
pub fn thinking_paint(
    theme: &Theme,
    lang: Lang,
    columns: usize,
    thinking: &str,
    role: Role,
) -> String {
    let label = format!("{}:", tr!(lang, m::THINKING));
    let snippet = thinking.lines().next().unwrap_or_default();
    let text = truncate(
        &format!(" {snippet}"),
        columns.saturating_sub(width(&label)),
    );
    format!("{}{}", theme.paint(role, &label), text)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::action::{Outcome, Value};
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
        fn reading(&self, reading: Option<Reading>) {
            let now = Instant::now();
            let label = reading.map_or("none".to_owned(), |reading| {
                let label = reading.label(now, Lang::En);
                label.split(" · ").next().unwrap().replace('\u{a0}', "")
            });
            self.ops.lock().unwrap().push(format!("reading {label}"));
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
            splash: true,
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

    /// The compact frame of `call` with its `items` (D1, D2, D6).
    fn show_card(presenter: &Presenter, call: &ToolCall, result: &ToolResult, items: &[String]) {
        presenter
            .screen
            .print(&presenter.card(call, Some(result), &records::call_hint(call, None), items));
    }

    #[test]
    fn framed_card() {
        // AGL-066, D1, D6: the frame is the tool's line, one line of the
        // input and the phases' row; no output.
        let (presenter, mock) = presenter(true);
        let result = ToolResult {
            outcome: Outcome::Fail,
            content: "one\ntwo\nthree\nexit code: 1".into(),
        };
        show_card(&presenter, &bash("cargo test"), &result, &["bash 1m 15s".to_owned()]);
        let printed = mock.printed.lock().unwrap().clone();
        // AGL-066, D19: the card is the tool's line, one line of the
        // input and the phases' row; the step's separator goes before the
        // block, not in the card.
        assert_eq!(
            printed,
            [
                "TOOL bash  fail",
                "  │ cargo test",
                &format!("─ fail · bash 1m 15s {}", "─".repeat(19)),
            ]
        );
        assert!(!printed.iter().any(|line| line.contains("one")), "{printed:?}");
    }

    #[test]
    fn unframed_card_and_edit_input() {
        // AGL-066, D6: one line of the input — the file of an edit.
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
        show_card(&presenter, &call, &result, &[]);
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(printed, ["TOOL edit  ok", "  │ a.rs"]);
    }

    #[test]
    fn the_answer_keeps_its_rule_with_the_time() {
        // AGL-066, D19: the step's separator goes before the block; the
        // answer's own rule carries the time of the turn.
        let (presenter, mock) = presenter(true);
        presenter.show_answer("**Done**", Some(Duration::from_secs(5)));
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(
            printed,
            [
                "Done".into(),
                format!("─ Worked for 5s {}", "─".repeat(24)),
                "".into(),
            ]
        );
        mock.printed.lock().unwrap().clear();
        presenter.show_answer("old", None);
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(printed, ["old".to_owned(), "─".repeat(40), "".to_owned()]);
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
        // AGL-066, D19: the step's block — the separator's rule, then the
        // thinking's line on the top level, in the tool's style; the
        // answer follows in the same block.
        assert_eq!(printed[0], "");
        assert_eq!(printed[1], "─".repeat(40));
        assert_eq!(printed[2], "thinking: hmm ok");
        assert!(printed.contains(&"Hi there more".to_owned()));
        assert!(!printed.contains(&"**Hi** there".to_owned()));
    }

    #[test]
    fn the_thinking_line_has_no_rule_of_its_own() {
        // AGL-066, D19: the thinking's line is one line, `thinking: <the
        // first line>`; the time of its phase is in the step's rule.
        let (presenter, mock) = presenter(true);
        presenter.show_thinking("hm\nmore");
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(printed, vec!["thinking: hm"]);
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
        // A count that failed leaves the tokens out (the live row above);
        // the replayed compaction is its one line (AGL-066, D1).
        mock.printed.lock().unwrap().clear();
        presenter.begin_turn();
        presenter.compacting();
        presenter.compacted();
        {
            let printed = mock.printed.lock().unwrap();
            assert!(!printed[0].contains("tokens"), "{:?}", printed[0]);
            assert_eq!(row(&printed, "─ ok"), "─ ok · Compacting <1s");
        }
        mock.printed.lock().unwrap().clear();
        presenter.replay(
            &[Event::Compact {
                summary: "the summary".into(),
                question: None,
            }],
            &[],
        );
        assert_eq!(*mock.printed.lock().unwrap(), ["Compacting"]);
    }

    #[test]
    fn the_reading_of_the_prompt_shows_its_speed() {
        // AGL-046: the server's figure for the step's first generation; a
        // step that read a few tokens shows none.
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
            tokens: 69,
            ms: 463.0,
        });
        report.call(&bash("ls"));
        let ok = ToolResult {
            outcome: Outcome::Ok,
            content: "a".into(),
        };
        report.result(&bash("ls"), &ok);
        assert_eq!(
            row(&mock.printed.lock().unwrap(), "─ ok"),
            "─ ok · Reading the prompt <1s (1125.0 tok/s) · bash <1s"
        );
        mock.printed.lock().unwrap().clear();
        report.prefill();
        report.prompt_read(Prefill {
            tokens: 18,
            ms: 304.0,
        });
        report.answer("Done");
        assert_eq!(
            row(&mock.printed.lock().unwrap(), "─ ok"),
            "─ ok · Reading the prompt <1s · Worked for <1s"
        );
    }

    #[test]
    fn the_reading_is_on_the_dock_only_while_the_prompt_is_read() {
        // AGL-046, D19.
        let (mut presenter, mock) = presenter(true);
        presenter.begin_turn();
        let mut observer = Observer::new();
        let mut report = Observed {
            observer: &mut observer,
            report: &mut presenter,
        };
        report.prefill();
        report.prompt_progress(PromptProgress {
            total: 1000,
            cache: 0,
            processed: 500,
            time_ms: 500,
        });
        let ops = mock.ops.lock().unwrap().clone();
        assert_eq!(ops.iter().filter(|op| *op == "reading 500 / 1000").count(), 1, "{ops:?}");
        report.thinking("hmm");
        // A figure of a later generation of the step is not a reading.
        report.prompt_progress(PromptProgress {
            total: 1000,
            cache: 0,
            processed: 900,
            time_ms: 500,
        });
        let ops = mock.ops.lock().unwrap().clone();
        assert_eq!(ops.iter().filter(|op| op.starts_with("reading 900")).count(), 0);
        assert_eq!(ops.iter().rfind(|op| op.starts_with("reading")).unwrap(), "reading none");
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
        show_card(
            &presenter,
            &bash("ls"),
            &ToolResult {
                outcome: Outcome::Ok,
                content: "a".into(),
            },
            &["bash 1s".to_owned()],
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
                !["erase", "speed", "live", "print", "reading"]
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
        // AGL-066, D19: the step's block — the separator, the text before
        // the call (the streamed line is replaced, not repeated), and the
        // tool's card, one unit.
        assert_eq!(printed[0], "");
        assert_eq!(printed[1], "─".repeat(40));
        assert_eq!(printed[2], "Looking at it.");
        assert_eq!(printed[3], "");
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
        presenter.replay(&events, &[]);
        let printed = mock.printed.lock().unwrap().clone();
        // The state a turn starts in is not shown (AGL-046, D13).
        assert!(!printed.contains(&"[work]".to_owned()));
        // AGL-066, D1: the compact view — the memory's marker alone; the
        // text written is in the inspect mode.
        assert!(printed.contains(&"[memory: +build-and-test]".to_owned()));
        assert!(!printed.contains(&"Run cargo test.".to_owned()));
        // AGL-066, D17: the thinking is a frame, its first line under the
        // gutter; no time, as it is replayed.
        assert!(
            printed.iter().any(|line| line.contains("thinking: hm")),
            "{printed:?}"
        );
        assert!(printed.contains(&"Running.".to_owned()));
        // The tool's frame: its name, its status, one line of the input; no
        // output (D1, D6).
        let tool = printed
            .iter()
            .position(|line| line == "TOOL bash  ok")
            .expect("the tool's line");
        assert_eq!(printed[tool + 1], "  │ ls");
        assert!(!printed.contains(&"a".to_owned()));
        assert!(printed.contains(&"[forge: agl-009 +Progress]".to_owned()));
        // A line per medium (AGL-043 F17).
        assert!(printed.contains(&"[Audio#1] voice.wav — “Check the logs.”".to_owned()));
        assert!(printed.contains(&"[Video#1] clip.mp4 — frames: 2".to_owned()));
        // The stir and the cut-off with what it discarded (AGL-043 F8).
        // Both wrap at the 40 columns of the test screen.
        let all = printed
            .join("\n")
            .replace('\n', " ")
            .replace("  ", " ");
        assert!(all.contains("[stir 1: `nope`"), "{all}");
        assert!(
            all.contains("[cut off: 4000 tokens of thinking, 8288 of a tool call]"),
            "{all}"
        );
        assert!(printed.contains(&"Done".to_owned()));
        // No time is shown for what is replayed.
        assert!(!printed.iter().any(|line| line.contains("Worked for")));
    }
}
