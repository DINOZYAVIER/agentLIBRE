//! What the transcript shows: user messages, states, thinking, streamed
//! text restyled as Markdown, tool cards, answers and stops (D123-D127,
//! D138). Drawing goes through [`Screen`].

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::card::{self, Fold, Render, block_lines, notice_block_lines, summary, user_lines};
use super::dock::grouped;
use super::reading::{MIN_TOKENS_OF_A_SPEED, Reading};
use crate::records::{self, Record, ToolResult};
use super::text::StreamWrap;
use agentlibre_client::Surface;
use crate::i18n::repl as texts;
use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::settings::{Detail, ReplSettings};
use agentlibre_common::style::Role;
use crate::theme::Theme;
use agentlibre_common::tr;
use agentlibre_proto::{
    CutOff, Event, MemoryKind, Phase, PhaseTime, Prefill, PromptProgress, Report, ToolCall, Transition,
};

/// A phase's name, in the dock and on the rule of a step (D135; AGL-045).
pub fn label(phase: &Phase, lang: Lang) -> String {
    match phase {
        Phase::Ready => tr!(lang, m::READY).to_string(),
        Phase::StartingServer => tr!(lang, m::STARTING_SERVER).to_string(),
        Phase::Transcribing => tr!(lang, m::TRANSCRIBING_ACTIVITY).to_string(),
        Phase::Prefill => tr!(lang, m::PREFILL).to_string(),
        Phase::Thinking => tr!(lang, m::THINKING_ACTIVITY).to_string(),
        Phase::Writing => tr!(lang, m::WRITING).to_string(),
        Phase::Retry { attempt, max } => {
            tr!(lang, m::RETRY, attempt = attempt, max = max)
        }
        Phase::Running(tool) => tr!(lang, m::RUNNING, tool = tool),
        Phase::Compacting => tr!(lang, m::COMPACTING).to_string(),
    }
}

/// Where the transcript is drawn: above the dock.
pub trait Screen: Send + Sync {
    fn columns(&self) -> usize;
    /// The rows above the dock that the live block may take (AGL-092, D3).
    fn room(&self) -> usize;
    /// The reading of the prompt going on (AGL-046, D19).
    fn reading(&self, _reading: Option<Reading>) {}
    /// Appends finished lines.
    fn print(&self, lines: &[String]);
    /// The unfinished lines below the printed ones, above the dock; empty
    /// clears them.
    fn live(&self, lines: &[String]);
    /// Erases the last `count` printed lines if they are all still on
    /// the screen; false when they are not.
    fn erase(&self, count: usize) -> bool;
    fn activity(&self, phase: Phase);
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
    lang: Lang,
    turn_started: Instant,
    /// The finished phases of the step, with their times, from the FSM of
    /// the turn (AGL-045, AGL-046).
    phases: Vec<(Phase, Duration)>,
    /// What the dock says now.
    activity: Phase,
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
    /// The call whose result is next, and its one line (the daemon's summary,
    /// AGL-130, D9).
    pending_call: Option<(ToolCall, String)>,
    /// The thinking being decoded, as its pieces arrive (D3, D19): the
    /// transcript shows its line with the record of its step.
    thinking: Option<String>,
    /// How much of the thinking, of a call and of a result the transcript
    /// shows (AGL-092, D2; AGL-095, D1).
    thinking_detail: Detail,
    tool_detail: Detail,
    result_detail: Detail,
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
        lang: Lang,
        settings: ReplSettings,
    ) -> Presenter {
        Presenter {
            screen,
            theme,
            lang,
            turn_started: Instant::now(),
            phases: Vec::new(),
            activity: Phase::Ready,
            tool_time: Duration::ZERO,
            compact_time: None,
            prefill: None,
            reading: None,
            last_read_speed: None,
            context: None,
            compact_before: None,
            context_fresh: false,
            state_entered: false,
            pending_call: None,
            thinking: None,
            thinking_detail: settings.thinking,
            tool_detail: settings.tool,
            result_detail: settings.result,
            action: None,
            generation: None,
            turn_tokens: 0,
            turn_decoding: Duration::ZERO,
        }
    }

    /// The dock is idle again.
    pub fn set_ready(&self) {
        self.screen.activity(Phase::Ready);
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
                self.screen.print(&[self.theme.paint(Role::InputText, &rest)]);
            }
            self.screen.live(&[]);
        }
        self.generation_done();
    }

    // Whole blocks, also used to show a resumed session (D126).

    /// The user's message on the input background, with the blank row that
    /// ends its block (AGL-085, D1) — the row a card and an answer end with
    /// too, so one blank divides one block from the next and never two.
    pub fn show_user(&self, message: &str) {
        let columns = self.screen.columns();
        let mut lines = user_lines(&self.theme, message, columns, Role::InputText);
        lines.push(String::new());
        self.screen.print(&lines);
    }

    /// A command of the human on the input background, in the tool's
    /// color as the menu paints it (AGL-101): a slash says that the
    /// model never saw the line.
    pub fn show_command(&self, line: &str) {
        let columns = self.screen.columns();
        let mut lines = user_lines(&self.theme, line, columns, Role::Tool);
        lines.push(String::new());
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
        self.screen.print(&notice_block_lines(
            &self.theme,
            self.lang,
            self.screen.columns(),
            &line,
            Role::Muted,
        ));
    }

    /// A stir of the stirred mode: its number and the parse error
    /// (AGL-030, D4; AGL-043 F8).
    pub fn show_stir(&self, attempt: u32, error: &str) {
        self.screen.print(&notice_block_lines(
            &self.theme,
            self.lang,
            self.screen.columns(),
            &crate::status::stir_line(attempt, error, self.lang),
            Role::Muted,
        ));
    }

    /// `[memory: +name]` where the memory was written, then the text that
    /// was written, muted (AGL-006, M28).
    pub fn show_memory(&self, name: &str, kind: MemoryKind, text: Option<&str>) {
        let mut lines = notice_block_lines(
            &self.theme,
            self.lang,
            self.screen.columns(),
            &records::memory_line(name, kind, self.lang),
            Role::Muted,
        );
        if let Some(text) = text {
            lines.extend(notice_block_lines(
                &self.theme,
                self.lang,
                self.screen.columns(),
                text,
                Role::Muted,
            ));
        }
        self.screen.print(&lines);
    }

    /// The step's thinking's lines (D3, D19, D24): like the tool's card —
    /// the label in the tool's color, and the thinking's rows under the
    /// gutter, muted: all of them, or its head and its tail (AGL-092, D2).
    /// The time is in the step's rule, the full text in the inspect mode.
    /// AGL-121, D14: the drawing is `card::thinking`'s, asked at the
    /// transcript's level of detail and with no focus.
    pub fn show_thinking(&self, thinking: &str) {
        let lines = card::thinking(
            &self.theme,
            self.lang,
            self.screen.columns(),
            thinking,
            self.thinking_detail,
            false,
        );
        if !lines.is_empty() {
            self.screen.print(&lines);
        }
    }

    /// The text before a call (D9): its first three lines; the rest is in
    /// the inspect mode.
    pub fn show_text(&self, text: &str) {
        self.screen.print(&card::text(
            &self.theme,
            self.screen.columns(),
            text,
            false,
        ));
    }

    /// The answer's block, with the row of its time when that is known.
    pub fn show_answer(&self, text: &str, elapsed: Option<Duration>) {
        let items: Vec<String> = elapsed.iter().map(|time| self.worked_for(*time)).collect();
        // AGL-080, D2; AGL-121, D16 d: the times of the answer in the
        // summary's role; no time, no row.
        let time = (!items.is_empty()).then(|| self.summary(None, &items));
        self.screen.print(&self.answer_lines(text, time));
    }

    /// A line of the session itself: help, a stop, an error.
    pub fn show_notice(&self, text: &str, role: Role) {
        self.screen.print(&notice_block_lines(
            &self.theme,
            self.lang,
            self.screen.columns(),
            text,
            role,
        ));
    }

    /// Shows what a session already holds (D126), through its records
    /// (AGL-066): compact by default, in full in the inspect mode.
    /// `transitions` carry the phases' times of the steps.
    pub fn replay(&self, events: &[Event], transitions: &[PhaseTime]) {
        for record in records::group(events, transitions, self.lang) {
            match &record {
                Record::User(message) => self.show_user(message),
                Record::Wake { flow, text } => self.show_user(&format!("[{flow}] {text}")),
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
                    if let Some(thinking) = thinking {
                        self.show_thinking(thinking);
                    }
                    self.show_answer(text, *time);
                }
                Record::Notice { line, role, .. } => {
                    // A command of the human takes the message's look;
                    // of the session's lines only a command's starts at
                    // the slash (AGL-101).
                    if line.starts_with('/') {
                        self.show_command(line);
                    } else {
                        self.show_notice(line, *role);
                    }
                }
            }
        }
    }

    // Lines.

    fn answer_lines(&self, text: &str, time: Option<String>) -> Vec<String> {
        // AGL-097: the answer is set off by a blank row before it — it is
        // the main thing, and it takes no gutter. The dock drops this row
        // when the block before it already ended with one.
        let mut lines = vec![String::new()];
        lines.extend(card::answer(
            &self.theme,
            self.lang,
            self.screen.columns(),
            text,
            time,
            false,
        ));
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
    fn phase_items(&self, phases: &[(Phase, Duration)]) -> Vec<String> {
        phases
            .iter()
            .map(|(phase, time)| {
                let mut item = format!(
                    "{} {}",
                    label(phase, self.lang),
                    texts::duration(time.as_secs())
                );
                // The speed of the reading (AGL-046): the server's own, so a
                // cached part of the prompt does not count.
                if *phase == Phase::Prefill
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

    /// A step's row: its status and items (AGL-045, AGL-080).
    fn summary(&self, status: Option<(&str, Role)>, items: &[String]) -> String {
        summary(&self.theme, status, items)
    }

    /// The compact card of a finished step (AGL-066, D1, D2, D6; AGL-080,
    /// D1; AGL-095, D2, D5; AGL-121, D14): `card::tool`'s drawing at the
    /// settings' levels of detail, with the phases' row under it and the
    /// blank that ends the block.
    fn card(
        &self,
        call: &ToolCall,
        result: Option<&ToolResult>,
        hint: &str,
        items: &[String],
    ) -> Vec<String> {
        let mut lines = card::tool(
            &self.theme,
            self.lang,
            self.screen.columns(),
            call,
            result,
            hint,
            self.tool_detail,
            self.result_detail,
            false,
        );
        // D27, AGL-080; AGL-121, D16 d: the phases in the summary's role,
        // with no rule; with no phase to name the row says nothing, and none.
        if !items.is_empty() {
            let (status_text, status_role) =
                records::status(result.map(|result| result.outcome));
            lines.push(self.summary(
                Some((status_text.as_str(), status_role)),
                items,
            ));
        }
        // AGL-085, D1: the card ends its block, and a blank row divides one
        // block from the next.
        lines.push(String::new());
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
            self.show_thinking(&thinking);
            self.screen.live(&[]);
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
            self.screen.live(&[]);
            if streamed.columns == self.screen.columns() {
                self.screen.erase(streamed.lines);
            }
        }
        self.screen.print(&block);
    }
}

impl Surface for Presenter {
    /// Takes what the turn tells (AGL-067, D3).
    fn report(&mut self, report: Report) {
        self.take(report);
    }
}

impl Presenter {
    fn take(&mut self, report: Report) {
        match report {
            // A message another client sent (AGL-126, D11).
            Report::Event(Event::User(message)) => {
                self.close();
                self.show_user(&message);
            }
            Report::Event(Event::State(state)) => self.state(&state),
            Report::Thinking(piece) => self.thinking(&piece),
            Report::ThinkingDone => self.thinking_done(),
            Report::ActionText(piece) => self.action_text(&piece),
            Report::Event(Event::Call { text, call, summary }) => {
                if let Some(text) = text {
                    self.text(&text);
                }
                self.call();
                self.pending_call = Some((call, summary.unwrap_or_default()));
            }
            Report::Event(Event::Result {
                outcome, content, ..
            }) => {
                if let Some((call, hint)) = self.pending_call.take() {
                    self.result(&call, &hint, &ToolResult { outcome, content });
                }
            }
            Report::Event(Event::Answer(text)) => self.answer(&text),
            // AGL-068: the forge's and the memory's rows ride the card's
            // hint row; the events stay in the session's record.
            Report::Event(Event::CutOff(cut)) => self.cut_off(&cut),
            Report::Stirred {
                attempt,
                max,
                error,
            } => self.stirred(attempt, max, &error),
            Report::Compacting => self.compacting(),
            Report::Compacted => self.compacted(),
            Report::ContextUsed(tokens) => self.context_used(tokens),
            Report::Decoded(tokens) => self.decoded(tokens),
            Report::PromptProgress(progress) => self.prompt_progress(progress),
            Report::PromptRead(prefill) => self.prompt_read(prefill),
            Report::Transition(transition) => self.transition(&transition),
            Report::Event(_)
            | Report::Prefill
            | Report::ActionStarted
            | Report::CompactFailed
            | Report::StepStarted(_) => {}
        }
    }

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
        // D29, AGL-092, D3: while the thinking is decoded the live's rows
        // are all of its rows — the label in the tool's color, the rows
        // under the gutter — clipped to what fits above the dock, which
        // keeps the newest of them: what the model thinks now.
        if self.thinking_detail == Detail::Off {
            self.screen.live(&[]);
        } else {
            let room = self.screen.room();
            let lines = block_lines(
                &self.theme.gray(),
                self.lang,
                self.screen.columns(),
                tr!(self.lang, m::THINKING),
                thinking,
                Role::Thinking,
                Fold::Whole,
                Render::Plain,
            );
            self.screen.live(&clip_live(&lines, room));
        }
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
        // The streamed text wears the message's color from the first
        // piece, as the block replacing it will (AGL-068; AGL-121, D82).
        let lines: Vec<String> = lines
            .into_iter()
            .map(|line| self.theme.paint(Role::InputText, &line))
            .collect();
        if !lines.is_empty() {
            self.screen.print(&lines);
        }
        self.screen.live(&[self.theme.paint(Role::InputText, streamed.wrap.live())]);
        self.piece();
    }

    fn text(&mut self, text: &str) {
        self.generation_done();
        self.replace_streamed(card::text(&self.theme, self.screen.columns(), text, false));
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
        let status = Some(("OK", Role::StatusSuccess));
        // AGL-121, D16 d: the items in the summary's role.
        let time = Some(self.summary(status, &items));
        let block = self.answer_lines(text, time);
        self.replace_streamed(block);
        self.prefill = None;
    }

    fn call(&mut self) {
        // AGL-099, D2: a bare step prints no thinking placeholder in the
        // transcript — an empty THINKING under a filled one is noise. The
        // triple lives on in the inspect mode, where the placeholder
        // keeps its place.
        self.close();
    }

    fn result(&mut self, call: &ToolCall, hint: &str, result: &ToolResult) {
        let phases = std::mem::take(&mut self.phases);
        let mut items = self.phase_items(&phases);
        items.push(format!(
            "{} {}",
            call.name,
            texts::duration(self.tool_time.as_secs())
        ));
        self.screen
            .print(&self.card(call, Some(result), hint, &items));
        self.prefill = None;
    }

    fn compacting(&mut self) {
        self.close();
        self.compact_before = self.context;
        self.context_fresh = false;
    }

    fn compacted(&mut self) {
        let mut items: Vec<String> = Vec::new();
        let label = label(&Phase::Compacting, self.lang);
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
        self.screen.print(&[self.summary(
            Some(("OK", Role::StatusSuccess)),
            &items,
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
            Phase::Running(_) => self.tool_time = transition.time(),
            Phase::Compacting => self.compact_time = Some(transition.time()),
            _ => self
                .phases
                .push((self.activity.clone(), transition.time())),
        }
        self.activity = transition.to.clone();
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

/// The live block of a thinking longer than the room above the dock
/// (AGL-092, D3): its last rows, with the label's row kept on top — what
/// the model thinks now is worth more than what it thought first.
pub fn clip_live(lines: &[String], room: usize) -> Vec<String> {
    if lines.len() <= room {
        return lines.to_vec();
    }
    if room == 0 {
        return Vec::new();
    }
    let mut kept = vec![lines[0].clone()];
    let keep = (room - 1).min(lines.len() - 1);
    kept.extend_from_slice(&lines[lines.len() - keep..]);
    kept
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::action::{Outcome, Value};
    use crate::agent::{Report, Sink};
    use crate::backend::{Prefill, PromptProgress, ToolCall};
    use crate::phase::{Observer, PhaseEvent, Reports};
    use crate::repl::text::width;
    use crate::session::{Event, ForgeKind};
    use crate::tools::ToolResult;
    use agentlibre_proto as proto;

    /// `replay` of the core's events and phase lines, which cross the mapping
    /// to the wire's (AGL-130, D12).
    fn replay(presenter: &Presenter, events: &[Event], transitions: &[crate::session::TransitionLine]) {
        let events: Vec<_> = events.iter().cloned().map(crate::plane::wire::event).collect();
        let transitions: Vec<_> = transitions
            .iter()
            .cloned()
            .map(crate::plane::wire::phase_time)
            .collect();
        presenter.replay(&events, &transitions);
    }

    /// `result` of a core call and result, as the live path gives them.
    fn result(presenter: &mut Presenter, call: &ToolCall, result: &ToolResult) {
        let hint = crate::tools::spec::call_summary(call);
        let call = crate::plane::wire::tool_call(call.clone());
        let result = records::ToolResult {
            outcome: crate::plane::wire::outcome(result.outcome),
            content: result.content.clone(),
        };
        presenter.result(&call, &hint, &result);
    }

    /// The presenter as the run's sink (AGL-130, D12): the core's reports
    /// cross the mapping of `plane::wire` to the wire's, so that these tests
    /// check the mapping too.
    struct Wired<'a>(&'a mut Presenter);

    impl Sink for Wired<'_> {
        fn send(&mut self, report: Report) {
            self.0.report(crate::plane::wire::report(report));
        }
    }

    /// What a test tells the presenter, in a word each: the reports of a
    /// turn as the agent sends them (AGL-067).
    trait Say {
        fn prefill(&mut self);
        fn thinking(&mut self, piece: &str);
        fn thinking_done(&mut self);
        fn action_text(&mut self, piece: &str);
        fn answer(&mut self, text: &str);
        fn call(&mut self, call: &ToolCall);
        fn call_with_text(&mut self, text: &str, call: &ToolCall);
        fn result(&mut self, call: &ToolCall, result: &ToolResult);
        fn stirred(&mut self, attempt: u32, max: u32, error: &str);
        fn context_used(&mut self, tokens: usize);
        fn compacting(&mut self);
        fn compacted(&mut self);
        fn prompt_read(&mut self, prefill: Prefill);
        fn prompt_progress(&mut self, progress: PromptProgress);
    }

    impl Say for Reports<'_> {
        fn prefill(&mut self) {
            self.send(Report::Prefill);
        }
        fn thinking(&mut self, piece: &str) {
            self.send(Report::Thinking(piece.into()));
        }
        fn thinking_done(&mut self) {
            self.send(Report::ThinkingDone);
        }
        fn action_text(&mut self, piece: &str) {
            self.send(Report::ActionText(piece.into()));
        }
        fn answer(&mut self, text: &str) {
            self.send(Report::Event(Event::Answer(text.into())));
        }
        fn call(&mut self, call: &ToolCall) {
            self.send(Report::Event(Event::Call {
                text: None,
                call: call.clone(),
            }));
        }
        fn call_with_text(&mut self, text: &str, call: &ToolCall) {
            self.send(Report::Event(Event::Call {
                text: Some(text.into()),
                call: call.clone(),
            }));
        }
        fn result(&mut self, call: &ToolCall, result: &ToolResult) {
            self.send(Report::Event(Event::Result {
                id: call.id.clone(),
                outcome: result.outcome,
                content: result.content.clone(),
            }));
        }
        fn stirred(&mut self, attempt: u32, max: u32, error: &str) {
            self.send(Report::Stirred {
                attempt,
                max,
                error: error.into(),
            });
        }
        fn context_used(&mut self, tokens: usize) {
            self.send(Report::ContextUsed(tokens));
        }
        fn compacting(&mut self) {
            self.send(Report::Compacting);
        }
        fn compacted(&mut self) {
            self.send(Report::Compacted);
        }
        fn prompt_read(&mut self, prefill: Prefill) {
            self.send(Report::PromptRead(prefill));
        }
        fn prompt_progress(&mut self, progress: PromptProgress) {
            self.send(Report::PromptProgress(progress));
        }
    }

    /// A screen that records what was drawn.
    #[derive(Default)]
    struct Mock {
        ops: Mutex<Vec<String>>,
        printed: Mutex<Vec<String>>,
        live: Mutex<Vec<String>>,
        room: Mutex<usize>,
    }

    impl Screen for Mock {
        fn columns(&self) -> usize {
            40
        }
        fn room(&self) -> usize {
            *self.room.lock().unwrap()
        }
        fn print(&self, lines: &[String]) {
            self.printed.lock().unwrap().extend_from_slice(lines);
            self.ops
                .lock()
                .unwrap()
                .push(format!("print {}", lines.len()));
        }
        fn live(&self, lines: &[String]) {
            *self.live.lock().unwrap() = lines.to_vec();
            self.ops.lock().unwrap().push(format!("live {}", lines.len()));
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
        fn activity(&self, activity: Phase) {
            self.ops.lock().unwrap().push(format!("{activity:?}"));
        }
        fn speed(&self, speed: Option<f64>) {
            self.ops.lock().unwrap().push(format!("speed {speed:?}"));
        }
    }

    fn presenter() -> (Presenter, Arc<Mock>) {
        presenter_at(Detail::Flexible)
    }

    /// A presenter of the thinking's detail `detail`, on a screen 24 rows
    /// above the dock.
    fn presenter_at(detail: Detail) -> (Presenter, Arc<Mock>) {
        presenter_with(ReplSettings {
            thinking: detail,
            ..Default::default()
        })
    }

    /// A presenter of the settings `settings`, on a screen 24 rows above
    /// the dock.
    fn presenter_with(settings: ReplSettings) -> (Presenter, Arc<Mock>) {
        let mock = Arc::new(Mock::default());
        *mock.room.lock().unwrap() = 24;
        let presenter = Presenter::new(mock.clone(), Theme::plain(), Lang::En, settings);
        (presenter, mock)
    }

    fn bash(command: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: vec![("command".into(), Value::Text(command.into()))],
        }
    }

    /// The compact card of `call` with its `items` (D1, D2, D6).
    fn show_card(presenter: &Presenter, call: &ToolCall, result: &ToolResult, items: &[String]) {
        let hint = crate::tools::spec::call_summary(call);
        let result = records::ToolResult {
            outcome: crate::plane::wire::outcome(result.outcome),
            content: result.content.clone(),
        };
        let call = crate::plane::wire::tool_call(call.clone());
        presenter
            .screen
            .print(&presenter.card(&call, Some(&result), &hint, items));
    }

    #[test]
    fn a_card_is_its_tool_its_input_and_its_times() {
        // AGL-066, D1, D6; AGL-080, D1; AGL-095, D2: the tool's line, one
        // line of the input, the result's block at the tool's own level and
        // the row of times, with no rule around them.
        let (presenter, mock) = presenter();
        let result = ToolResult {
            outcome: Outcome::Fail,
            content: "one\ntwo\nthree\nexit code: 1".into(),
        };
        show_card(&presenter, &bash("cargo test"), &result, &["bash 1m 15s".to_owned()]);
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(
            printed,
            [
                "TOOL bash  FAIL",
                "⎿ cargo test",
                "RESULT",
                "⎿ one",
                "  two",
                "  three",
                "  exit code: 1",
                "FAIL · bash 1m 15s",
                "",
            ]
        );
    }

    #[test]
    fn a_card_shows_one_line_of_its_input() {
        // AGL-066, D6: one line of the input — the file of an edit; with no
        // phase to name there is no row of times (AGL-080, D1).
        let (presenter, mock) = presenter();
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
        assert_eq!(
            printed,
            [
                "TOOL edit  OK",
                "⎿ a.rs",
                "RESULT",
                "⎿ Replaced 1 occurrence in a.rs.",
                "",
            ]
        );
    }

    #[test]
    fn a_result_folds_like_the_thinking() {
        // AGL-095, D2: the same head, the same `⋯ n hidden`, the same tail.
        let (presenter, mock) = presenter();
        let content = (1..=20).map(|n| n.to_string()).collect::<Vec<_>>().join("\n");
        show_card(
            &presenter,
            &bash("seq"),
            &ToolResult {
                outcome: Outcome::Ok,
                content,
            },
            &[],
        );
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(printed[2], "RESULT");
        assert_eq!(printed[3], "⎿ 1");
        assert!(printed[6].contains("hidden"), "{printed:?}");
        assert_eq!(printed[printed.len() - 2], "  20");
    }

    #[test]
    fn an_empty_result_keeps_its_place() {
        // AGL-095, D4: a tool that answered nothing still has a result block.
        let (presenter, mock) = presenter();
        show_card(
            &presenter,
            &bash("true"),
            &ToolResult {
                outcome: Outcome::Ok,
                content: String::new(),
            },
            &[],
        );
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(
            printed,
            ["TOOL bash  OK", "⎿ true", "RESULT", "⎿ (empty)", ""]
        );
    }

    #[test]
    fn the_levels_of_the_tool_and_the_result() {
        // AGL-095, D1 and D5: `off` shows neither the input nor the answer,
        // `flexible` the one line of it, `full` the whole of what was sent.
        let args = vec![
            ("command".into(), Value::Text("ls\ncat a".into())),
            ("path".into(), Value::Text("a.rs".into())),
        ];
        let call = ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args,
        };
        let result = ToolResult {
            outcome: Outcome::Ok,
            content: "out".into(),
        };
        for (level, rows) in [
            (Detail::Off, &["TOOL bash  OK", ""][..]),
            (Detail::Flexible, &["TOOL bash  OK", "⎿ ls", ""][..]),
            (
                Detail::Full,
                &["TOOL bash  OK", "⎿ ls", "  cat a", ""][..],
            ),
        ] {
            let (presenter, mock) = presenter_with(ReplSettings {
                tool: level,
                result: Detail::Off,
                ..Default::default()
            });
            show_card(&presenter, &call, &result, &[]);
            let printed = mock.printed.lock().unwrap().clone();
            assert_eq!(printed, rows, "{level:?}");
        }
        // `repl.result = off` keeps the tool's answer out of the card.
        let (presenter, mock) = presenter_with(ReplSettings {
            result: Detail::Off,
            ..Default::default()
        });
        show_card(&presenter, &call, &result, &[]);
        assert_eq!(
            mock.printed.lock().unwrap().clone(),
            ["TOOL bash  OK", "⎿ ls", ""]
        );
    }

    #[test]
    fn the_answer_carries_its_time() {
        // AGL-066, D19; AGL-080, D1: the answer is its text and the row of
        // its time, with no rule; no time, no row.
        let (presenter, mock) = presenter();
        presenter.show_answer("**Done**", Some(Duration::from_secs(5)));
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(
            printed,
            [
                String::new(),
                "Done".to_owned(),
                "Worked for 5s".to_owned(),
                String::new()
            ]
        );
        mock.printed.lock().unwrap().clear();
        presenter.show_answer("old", None);
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(printed, ["".to_owned(), "old".to_owned(), "".to_owned()]);
    }

    #[test]
    fn streamed_text_is_replaced_by_the_rendered_answer() {
        let (mut presenter, mock) = presenter();
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
        // AGL-066, D19, D24; AGL-080, D1: the step's block begins with the
        // thinking like the tool's card (the label, the line under the
        // gutter); the answer follows in the same block.
        assert_eq!(printed[0], "THINKING");
        assert_eq!(printed[1], "⎿ hmm ok");
        assert!(printed.contains(&"Hi there more".to_owned()));
        assert!(!printed.contains(&"**Hi** there".to_owned()));
    }

    #[test]
    fn the_thinking_is_like_the_tools_card() {
        // AGL-066, D24, AGL-068: the thinking is the label's line and its
        // rows under the gutter, as the tool's card; no rule of its own, the
        // time of its phase is in the step's rule.
        let (first, mock) = presenter();
        first.show_thinking("hm\nmore\n\nthird");
        // AGL-093, D1: the two lines of one paragraph flow into one row.
        assert_eq!(
            mock.printed.lock().unwrap().clone(),
            vec!["THINKING", "⎿ hm more", "  third"]
        );
        let (second, mock) = presenter();
        second.show_thinking("a\n\nb\n\nc\n\nd\n\ne\n\nf");
        // AGL-092, D4: six rows are fewer than a fold costs, so all of them
        // stay — the fold begins where it pays.
        assert_eq!(
            mock.printed.lock().unwrap().clone(),
            vec![
                "THINKING",
                "⎿ a",
                "  b",
                "  c",
                "  d",
                "  e",
                "  f"
            ]
        );
    }

    #[test]
    fn the_cards_thinking_is_markdown_and_the_lives_is_not() {
        // AGL-093, D1: the printed card renders the thinking's Markdown;
        // the live block, redrawn with every piece, shows it as it came.
        let (mut presenter, mock) = presenter();
        presenter.thinking("use **bash** here");
        assert_eq!(mock.live.lock().unwrap()[1], "⎿ use **bash** here");
        presenter.thinking_done();
        assert_eq!(mock.printed.lock().unwrap()[1], "⎿ use bash here");
    }

    #[test]
    fn a_long_thinking_shows_its_head_and_its_tail() {
        // AGL-092, D4: the first paragraph's rows, `⋯ n` for what lies
        // between, and the last rows, which reach back to the start of
        // their paragraph because one row buys that.
        let (first, mock) = presenter();
        first.show_thinking(
            "one\n\ntwo\n\np3\n\np4\n\np5\n\np6\n\np7\n\np8\n\np9\n\nlast\ntail",
        );
        assert_eq!(
            mock.printed.lock().unwrap().clone(),
            vec![
                "THINKING",
                "⎿ one",
                "  ⋯ 3 hidden",
                "  p5",
                "  p6",
                "  p7",
                "  p8",
                "  p9",
                "  last tail"
            ]
        );
    }

    #[test]
    fn the_levels_of_the_thinking() {
        // AGL-092, D2: `off` shows nothing of it, `full` shows all of it
        // and keeps it — no `⋯` row anywhere.
        let (mut off, mock) = presenter_at(Detail::Off);
        off.thinking("hm\n");
        off.thinking_done();
        assert!(mock.live.lock().unwrap().is_empty());
        assert!(mock.printed.lock().unwrap().is_empty());
        let (mut full, mock) = presenter_at(Detail::Full);
        full.thinking(&"a\n\n".repeat(20));
        full.thinking_done();
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(printed.len(), 21, "{printed:?}");
        assert!(!printed.iter().any(|line| line.contains('⋯')), "{printed:?}");
    }

    #[test]
    fn a_thinking_block_has_no_accent_of_its_own() {
        // AGL-105, D1; AGL-107, D1: in the transcript only the thinking's
        // head keeps a color — the elbow and the Markdown of its body are
        // gray.
        let style = |hex| agentlibre_common::style::Style::parse(hex, false).expect("a color");
        let theme = Theme::new(
            true,
            &[
                (Role::Tool, style("#FF0000")),
                (Role::MarkdownHeading, style("#0000FF")),
                (Role::MarkdownInlineCode, style("#00FF00")),
            ],
        );
        let (presenter, mock) = presenter();
        let presenter = Presenter {
            theme: theme.clone(),
            ..presenter
        };
        presenter.show_thinking("# Heading\n\nwith `code` in it\n");
        let printed = mock.printed.lock().unwrap().join("\n");
        assert!(printed.starts_with(&theme.start(Role::Tool)), "{printed:?}");
        assert!(
            !printed.contains(&theme.start(Role::MarkdownHeading)),
            "{printed:?}"
        );
        assert!(printed.contains(&theme.start(Role::Muted)), "{printed:?}");
    }

    #[test]
    fn the_elbow_is_dark_and_an_unlabelled_notice_hangs_under_it() {
        // AGL-106, D1, D2: the elbow is muted on every block, the label
        // keeps its own color; a notice with no label of its own — the
        // server's line — is a block too.
        let theme = Theme::new(true, &[]);
        let (presenter, mock) = presenter();
        let presenter = Presenter {
            theme: theme.clone(),
            ..presenter
        };
        presenter.show_notice("the server is ready at http://x", Role::Muted);
        show_card(
            &presenter,
            &bash("ls"),
            &ToolResult {
                outcome: Outcome::Ok,
                content: "a".into(),
            },
            &[],
        );
        let printed = mock.printed.lock().unwrap().clone();
        let elbow = format!("{}⎿ {}", theme.start(Role::Muted), theme.reset());
        assert!(printed[0].starts_with(&elbow), "{printed:?}");
        assert!(printed[2].starts_with(&elbow), "{printed:?}");
        assert!(printed[4].starts_with(&elbow), "{printed:?}");
        // The labels above them keep the tool's color.
        for label in [&printed[1], &printed[3]] {
            assert!(label.starts_with(&theme.start(Role::Tool)), "{label:?}");
        }
    }

    #[test]
    fn the_body_of_a_block_hangs_under_the_elbow() {
        // AGL-104, D1: the label stands alone, `⎿ ` begins the body and
        // the rows that follow stand under it; nothing passes the screen.
        let (mut presenter, mock) = presenter();
        presenter.thinking(&"a".repeat(120));
        presenter.thinking_done();
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(printed[0], "THINKING", "{printed:?}");
        assert!(printed[1].starts_with("⎿ "), "{printed:?}");
        assert!(printed[2].starts_with("  "), "{printed:?}");
        for line in &printed[1..] {
            assert!(width(line) <= 40, "{line}");
        }
    }

    #[test]
    fn the_live_thinking_shows_all_of_it_and_then_its_newest_rows() {
        // AGL-092, D3: while the thinking is decoded the live block takes
        // all of it; past the room above the dock its newest rows stay,
        // with the label's row on top.
        let (mut presenter, mock) = presenter();
        for n in 0..30 {
            presenter.thinking(&format!("line {n}\n"));
        }
        let lines = mock.live.lock().unwrap().clone();
        assert_eq!(lines.len(), 24, "{lines:?}");
        assert_eq!(lines[0], "THINKING");
        assert_eq!(lines[23], "  line 29");
    }

    #[test]
    fn the_live_thinking_is_the_card_from_the_first_piece() {
        // AGL-066, D29: while the thinking streams, the live's lines are
        // the card's lines — the label and its rows under the elbow (
        // AGL-104, D1) — as they are printed when the thinking ends; each
        // fits the screen.
        let (mut presenter, mock) = presenter();
        presenter.thinking(&"a".repeat(80));
        let lines = mock.live.lock().unwrap().clone();
        // AGL-080, D2: the line wraps to the terminal's width, so the
        // thinking's one line is three of the screen's, each of them short.
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_eq!(lines[0], "THINKING", "{lines:?}");
        for line in &lines[1..] {
            assert!(line.starts_with("⎿ ") || line.starts_with("  "), "{lines:?}");
            assert!(width(line) <= 40, "{line:?}");
        }
    }

    /// The row of `printed` that starts with `start`.
    fn row(printed: &[String], start: &str) -> String {
        printed
            .iter()
            .find(|line| line.starts_with(start))
            .expect("the row")
            .to_owned()
    }

    #[test]
    fn the_rule_of_a_step_names_its_phases() {
        // AGL-045: the phases in order, each with its time; then the tool
        // on a card, the turn's time on the answer.
        let (mut presenter, mock) = presenter();
        presenter.begin_turn();
        let mut observer = Observer::new();
        observer.enter(PhaseEvent::ServerStart, &mut Wired(&mut presenter));
        let mut report = Reports {
            observer: &mut observer,
            sink: &mut Wired(&mut presenter),
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
            row(&mock.printed.lock().unwrap(), "OK"),
            "OK · Starting the server <1s · Reading the prompt <1s · Thinking <1s · \
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
            row(&mock.printed.lock().unwrap(), "OK"),
            "OK · Reading the prompt <1s · Writing <1s · Retry 1/2 <1s · Writing <1s · \
             Worked for <1s"
        );
    }

    #[test]
    fn a_new_turn_forgets_the_phases_of_the_last() {
        // AGL-045: an interrupted turn leaves its phases behind.
        let (mut presenter, mock) = presenter();
        presenter.begin_turn();
        let mut first = Observer::new();
        let mut report = Reports {
            observer: &mut first,
            sink: &mut Wired(&mut presenter),
        };
        report.prefill();
        report.thinking("hmm");
        report.end(PhaseEvent::Failed);
        presenter.close();
        presenter.begin_turn();
        let mut second = Observer::new();
        let mut report = Reports {
            observer: &mut second,
            sink: &mut Wired(&mut presenter),
        };
        report.prefill();
        report.answer("Done");
        assert_eq!(
            row(&mock.printed.lock().unwrap(), "OK"),
            "OK · Reading the prompt <1s · Worked for <1s"
        );
    }

    #[test]
    fn a_compaction_has_its_own_row_and_leaves_the_steps_row() {
        // AGL-046, D13: its time and the tokens before and after; the next
        // step's row does not repeat it.
        let (mut presenter, mock) = presenter();
        presenter.begin_turn();
        let mut observer = Observer::new();
        let mut report = Reports {
            observer: &mut observer,
            sink: &mut Wired(&mut presenter),
        };
        report.prefill();
        report.context_used(52_400);
        report.compacting();
        report.context_used(3_100);
        report.compacted();
        assert_eq!(
            row(&mock.printed.lock().unwrap(), "OK"),
            "OK · Compacting <1s · 52\u{a0}400 → 3\u{a0}100 tokens"
        );
        mock.printed.lock().unwrap().clear();
        report.action_text("Done");
        report.answer("Done");
        let answer = row(&mock.printed.lock().unwrap(), "OK");
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
            assert_eq!(row(&printed, "OK"), "OK · Compacting <1s");
        }
        mock.printed.lock().unwrap().clear();
        replay(
            &presenter,
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
        let (mut presenter, mock) = presenter();
        presenter.begin_turn();
        let mut observer = Observer::new();
        let mut report = Reports {
            observer: &mut observer,
            sink: &mut Wired(&mut presenter),
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
            row(&mock.printed.lock().unwrap(), "OK"),
            "OK · Reading the prompt <1s (1125.0 tok/s) · bash <1s"
        );
        mock.printed.lock().unwrap().clear();
        report.prefill();
        report.prompt_read(Prefill {
            tokens: 18,
            ms: 304.0,
        });
        report.answer("Done");
        assert_eq!(
            row(&mock.printed.lock().unwrap(), "OK"),
            "OK · Reading the prompt <1s · Worked for <1s"
        );
    }

    #[test]
    fn the_reading_is_on_the_dock_only_while_the_prompt_is_read() {
        // AGL-046, D19.
        let (mut presenter, mock) = presenter();
        presenter.begin_turn();
        let mut observer = Observer::new();
        let mut report = Reports {
            observer: &mut observer,
            sink: &mut Wired(&mut presenter),
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
        let (mut presenter, mock) = presenter();
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
        let (mut presenter, mock) = presenter();
        presenter.transition(&Transition {
            from: Phase::Writing,
            to: Phase::Thinking,
            event: proto::PhaseEvent::FirstThinking,
            micros: 0,
            violation: true,
        });
        // AGL-104, D1: the body hangs under the elbow, and the rows that
        // follow it stand under that.
        let printed = mock.printed.lock().unwrap().clone();
        assert_eq!(printed[0], "fsm", "{printed:?}");
        assert!(
            printed[1].starts_with("⎿ `first_thinking` must not happen in"),
            "{printed:?}"
        );
        assert!(printed[2].starts_with("  "), "{printed:?}");
        // The word the wrap cut in two comes back when the prefixes go.
        let body: String = printed[1..]
            .iter()
            .map(|row| row.chars().skip(2).collect::<String>())
            .collect();
        assert!(body.contains("Writing"), "{printed:?}");
    }

    #[test]
    fn the_status_on_a_rule_is_painted_in_its_role() {
        // AGL-045: `ok` keeps the color of `status_success`.
        let theme = Theme::new(true, &[]);
        let (presenter, mock) = presenter();
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
        let green = theme.paint(Role::StatusSuccess, "OK");
        let printed = mock.printed.lock().unwrap();
        // AGL-085, D1: a card ends its block, so the last row is blank.
        let row = printed
            .iter()
            .rev()
            .find(|line| !line.is_empty())
            .expect("the row");
        assert!(row.contains(&green), "{row:?}");
        assert!(row.contains("bash 1s"), "{row:?}");
    }

    #[test]
    fn the_tool_row_is_muted_like_the_thinking() {
        // AGL-066, D26: the line under the gutter, the tool's input, is
        // in the muted color, as the thinking's line; the tool's line
        // keeps the tool's and the status's colors.
        let red = agentlibre_common::style::Style::parse("#FF0000", false).expect("a color");
        let theme = Theme::new(true, &[(Role::Muted, red)]);
        let (presenter, mock) = presenter();
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
        let printed = mock.printed.lock().unwrap().clone();
        let row = printed
            .iter()
            .find(|line| line.contains("ls") && !line.contains("TOOL"))
            .expect("the tool's row");
        assert!(row.contains("\x1b[38;2;255;0;0m"), "{row:?}");
    }

    #[test]
    fn the_answer_time_is_the_summarys_color() {
        // AGL-080 D2; AGL-121, D16 d: the answer's row of times — the
        // phases, the time, the speed — takes the summary's role, not the
        // tool's.
        let red = agentlibre_common::style::Style::parse("#FF0000", false).expect("a color");
        let green = agentlibre_common::style::Style::parse("#00FF00", false).expect("a color");
        let theme = Theme::new(true, &[(Role::Summary, red), (Role::Tool, green)]);
        let (presenter, mock) = presenter();
        let presenter = Presenter {
            theme: theme.clone(),
            ..presenter
        };
        presenter.show_answer("done", Some(Duration::from_secs(2)));
        let printed = mock.printed.lock().unwrap().clone();
        let row = printed
            .iter()
            .find(|line| line.contains("Worked for"))
            .expect("the row");
        assert!(row.contains("\x1b[38;2;255;0;0m"), "{row:?}");
        assert!(!row.contains("\x1b[38;2;0;255;0m"), "{row:?}");
    }

    #[test]
    fn the_dock_follows_the_phases_of_a_turn() {
        // AGL-043 F2: the start, the prefill, the thinking, the writing,
        // the call, the next prefill, a stir and the answer.
        let (mut presenter, mock) = presenter();
        presenter.begin_turn();
        let mut observer = Observer::new();
        observer.enter(PhaseEvent::ServerStart, &mut Wired(&mut presenter));
        observer.enter(PhaseEvent::Transcribe, &mut Wired(&mut presenter));
        let mut report = Reports {
            observer: &mut observer,
            sink: &mut Wired(&mut presenter),
        };
        report.prefill();
        report.thinking("hmm ");
        report.thinking_done();
        report.action_text("Looking.\n");
        report.call_with_text("Looking.", &bash("ls"));
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
        let (mut presenter, mock) = presenter();
        presenter.action_text("Looking at it.\n");
        presenter.text("Looking at it.");
        presenter.call();
        result(
            &mut presenter,
            &bash("ls"),
            &ToolResult {
                outcome: Outcome::Ok,
                content: "a".into(),
            },
        );
        let printed = mock.printed.lock().unwrap().clone();
        // AGL-066, D19, AGL-080, D1: the text before the call (the streamed
        // line is replaced, not repeated) and the tool's card, with no rule
        // between them; AGL-085, D1: no blank row inside a block either.
        assert_eq!(printed[0], "Looking at it.");
        assert!(printed[1].starts_with("TOOL bash"), "{printed:?}");
        assert!(printed.contains(&"TOOL bash  OK".to_owned()));
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
        let (mut presenter, mock) = presenter();
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
        let (mut presenter, mock) = presenter();
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
        let row = printed
            .iter()
            .find(|line| line.contains("Worked for"))
            .expect("the answer's row")
            .clone();
        assert!(row.contains("tok/s"), "{row:?}");
        let figure: f64 = row
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
        let row = mock
            .printed
            .lock()
            .unwrap()
            .iter()
            .find(|line| line.contains("Worked for"))
            .expect("the replayed row")
            .clone();
        assert!(!row.contains("tok/s"), "{row:?}");
    }

    #[test]
    fn user_messages_fill_the_row() {
        let rows = user_lines(&Theme::plain(), "hi\nthere", 10, Role::InputText);
        assert_eq!(rows, ["› hi      ", "  there   "]);
        let rows = user_lines(&Theme::plain(), "abcdefghijkl", 10, Role::InputText);
        assert_eq!(rows, ["› abcdefgh", "  ijkl    "]);
        let rows = user_lines(&Theme::plain(), "one two three", 10, Role::InputText);
        assert_eq!(rows, ["› one two ", "  three   "]);
    }

    #[test]
    fn a_command_takes_the_message_look() {
        // AGL-101: a run command echoes like a sent message, in the tool's
        // color as the menu paints it.
        let red = agentlibre_common::style::Style::parse("#FF0000", false).expect("a color");
        let theme = Theme::new(true, &[(Role::Tool, red)]);
        let (presenter, mock) = presenter();
        let presenter = Presenter {
            theme: theme.clone(),
            ..presenter
        };
        presenter.show_command("/quit");
        let printed = mock.printed.lock().unwrap().clone();
        let row = printed.first().expect("a row");
        assert!(row.contains('›'), "{row:?}");
        assert!(row.contains("\x1b[38;2;255;0;0m/quit"), "{row:?}");
    }

    #[test]
    fn a_resumed_session_is_replayed() {
        let (presenter, mock) = presenter();
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
                kind: agentlibre_common::media::Kind::Audio,
                number: 1,
                sha256: "a".repeat(64),
                ext: "wav".into(),
                name: "voice.wav".into(),
                transcript: Some("Check the logs.".into()),
                frames: Vec::new(),
            }),
            Event::Media(crate::media::Media {
                kind: agentlibre_common::media::Kind::Video,
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
            Event::CutOff(crate::session::CutOff {
                limit: 12288,
                thinking: Some(4000),
                discarded: 8288,
                call: true,
            }),
            Event::Answer("Done".into()),
        ];
        replay(&presenter, &events, &[]);
        let printed = mock.printed.lock().unwrap().clone();
        // The state a turn starts in is not shown (AGL-046, D13).
        assert!(!printed.contains(&"[work]".to_owned()));
        // AGL-068: a forge or a memory write rides the card of its call;
        // no row of its own, and the text written is not printed.
        assert!(!printed.iter().any(|line| line.starts_with("[memory:")));
        assert!(!printed.iter().any(|line| line.starts_with("[forge:")));
        assert!(!printed.contains(&"Run cargo test.".to_owned()));
        // AGL-066, D17, D24: the thinking like the tool's card, its first
        // line under the gutter; no time, as it is replayed.
        let thinking = printed
            .iter()
            .position(|line| line == "THINKING")
            .expect("the thinking's label");
        assert_eq!(printed[thinking + 1], "⎿ hm");
        assert!(printed.contains(&"Running.".to_owned()));
        // The tool's card: its name, its status, one line of the input; no
        // output (D1, D6).
        let tool = printed
            .iter()
            .position(|line| line == "TOOL bash  OK")
            .expect("the tool's line");
        assert_eq!(printed[tool + 1], "⎿ ls");
        assert!(!printed.contains(&"a".to_owned()));
        // A line per medium (AGL-043 F17).
        // AGL-106, D2: a notice with no label of its own hangs under the
        // elbow too; the line that does not fit wraps under it.
        assert!(
            printed
                .windows(2)
                .any(|rows| rows[0] == "⎿ [Audio#1] voice.wav — “Check the logs."
                    && rows[1] == "  ”"),
            "{printed:?}"
        );
        assert!(printed.contains(&"⎿ [Video#1] clip.mp4 — frames: 2".to_owned()));
        // The stir and the cut-off as blocks (AGL-043 F8; AGL-099, D1):
        // the label, and the body under the gutter, wrapped at the 36
        // columns inside the 40 columns of the test screen.
        let stir = printed
            .iter()
            .position(|line| line == "stir 1")
            .expect("the stir's label");
        assert_eq!(
            printed[stir + 1],
            "⎿ `nope` is not a tool of this state"
        );
        assert!(printed.contains(&"cut off".to_owned()));
        assert!(printed
            .iter()
            .any(|line| line.starts_with("⎿ 4000 tokens of thinking")));
        assert!(printed.contains(&"Done".to_owned()));
        // No time is shown for what is replayed.
        assert!(!printed.iter().any(|line| line.contains("Worked for")));
    }
}

