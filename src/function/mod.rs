//! Functions: the model format, prompt, sampling and state machine of an
//! agent, checked when loaded (D11, D15, D17, D21, D39, D41-D43, D82-D93,
//! D139, D140).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use toml::Table;

use crate::action::{self, Actions, Constraint, Kind, Outcome, Tool};
use crate::backend::RUNTIME_PREFIX;
use crate::error::{Error, FunctionProblem};
use crate::format::Format;
use crate::grammar::pattern;
use crate::tomlfile::{self, Section};

/// Functions built into the binary; a user file of the same name replaces
/// one (D39).
const BUILTIN: &[(&str, &str)] = &[
    ("minimal", include_str!("../builtin/functions/minimal.toml")),
    ("agentLIBRE", include_str!("../builtin/functions/agentLIBRE.toml")),
];

/// `max_steps_total` when the Function does not set it (D40).
const DEFAULT_MAX_STEPS_TOTAL: u32 = 50;

/// Token limits of one generation when the Function does not set them
/// (D18, D96).
const DEFAULT_MAX_THINK_TOKENS: u32 = 4096;
const DEFAULT_MAX_ACTION_TOKENS: u32 = 8192;
const DEFAULT_MAX_COMPACT_TOKENS: u32 = 4096;

/// The longest voice recording of the REPL's Ctrl-R, in seconds, when the
/// Function does not set it (AGL-016, D5, D8).
const DEFAULT_MAX_RECORD_SECONDS: u32 = 60;

/// The stirs of one step of the stirred mode when the Function does not set
/// them (AGL-030, D6): two stirs, three action attempts in all.
const DEFAULT_MAX_STIRS: u32 = 2;

/// The summary instruction when the Function does not set `compact`
/// (D140).
const DEFAULT_COMPACT: &str = "Summarize the conversation so far so that you can continue the \
     work from this summary alone: the user's requests, what has been done and found, files \
     and commands that matter, and what remains to be done. Write only the summary.";

/// A loaded Function.
#[derive(Debug)]
pub struct Function {
    pub format: Format,
    pub system: String,
    pub preserve_thinking: bool,
    /// The steps of a turn; `None` when unlimited (D40, AGL-021, D2).
    pub max_steps_total: Option<u32>,
    pub max_think_tokens: u32,
    pub max_action_tokens: u32,
    /// The stirs of one step of the stirred mode (AGL-030, D6); the parse
    /// of the action is retried at most this many times after a failure.
    /// `0` means no stirs: a parse failure stops the turn.
    pub max_stirs: u32,
    /// The instruction that asks for a summary of the history (D139).
    pub compact: String,
    pub max_compact_tokens: u32,
    /// The longest voice recording, in seconds (AGL-016).
    pub max_record_seconds: u32,
    pub sampling: Sampling,
    pub initial: usize,
    pub states: Vec<State>,
}

/// Sampling settings; `None` leaves the llama-server default (D89).
#[derive(Debug, Default, PartialEq)]
pub struct Sampling {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<i64>,
    pub min_p: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub repeat_penalty: Option<f64>,
}

/// A state of the Function (D11).
#[derive(Debug)]
pub struct State {
    pub name: String,
    pub instruction: Option<String>,
    pub actions: Actions,
    pub think: bool,
    pub max_steps: Option<u32>,
    pub max_visits: Option<u32>,
    pub on_limit: Option<usize>,
    /// `next` entries other than `else`: tool, outcome (`None` for any)
    /// and target state.
    transitions: Vec<(String, Option<Outcome>, usize)>,
    /// `next.else`.
    otherwise: Option<usize>,
}

impl State {
    /// The state after `tool` ended with `outcome`; the most specific key
    /// wins (D43). Loading guarantees an answer (D21).
    pub fn next(&self, tool: &str, outcome: Outcome) -> usize {
        let find = |wanted: Option<Outcome>| {
            self.transitions
                .iter()
                .find(|(t, o, _)| t == tool && *o == wanted)
                .map(|&(_, _, target)| target)
        };
        find(Some(outcome))
            .or_else(|| find(None))
            .or(self.otherwise)
            .expect("complete transitions")
    }

    /// States the state can lead to.
    fn targets(&self) -> impl Iterator<Item = usize> + '_ {
        self.transitions
            .iter()
            .map(|&(_, _, target)| target)
            .chain(self.otherwise)
            .chain(self.on_limit)
    }
}

impl Function {
    /// Loads Function `name` from `functions_dir/<name>.toml` or, when that
    /// file does not exist, from the built-in Functions. Its model format
    /// comes from `formats_dir`.
    pub fn load(functions_dir: &Path, formats_dir: &Path, name: &str) -> Result<Function, Error> {
        tomlfile::check_name(name)?;
        let path = functions_dir.join(format!("{name}.toml"));
        if let Some(table) = tomlfile::read(&path)? {
            return Function::from_table(&path, table, formats_dir);
        }
        let Some((_, text)) = BUILTIN.iter().find(|(builtin, _)| *builtin == name) else {
            return Err(Error::FunctionNotFound {
                name: name.into(),
                path,
            });
        };
        let path = PathBuf::from(format!("<built-in>/functions/{name}.toml"));
        Function::from_table(&path, tomlfile::parse(&path, text)?, formats_dir)
    }

    /// The Function without the tool `name` in every state: the tool list
    /// and the grammar do not have it, so the model cannot ask for it
    /// (AGL-019, D5). A state left with no tool and no answer would have no
    /// action, so it gets the answer.
    pub fn without_tool(&mut self, name: &str) {
        for state in &mut self.states {
            state.actions.tools.retain(|tool| tool.name != name);
            if state.actions.tools.is_empty() {
                state.actions.answer = true;
            }
        }
    }

    fn from_table(path: &Path, table: Table, formats_dir: &Path) -> Result<Function, Error> {
        let mut file = Section::new(path, table);
        file.check_keys(&[
            "format",
            "system",
            "preserve_thinking",
            "max_steps_total",
            "max_think_tokens",
            "max_action_tokens",
            "max_stirs",
            "compact",
            "max_compact_tokens",
            "max_record_seconds",
            "sampling",
            "initial",
            "state",
        ])?;
        let format_name = file.required_string("format")?;
        let format = Format::load(formats_dir, &format_name)?;
        let system = file.required_string("system")?;
        let preserve_thinking = file.bool("preserve_thinking")?.unwrap_or(true);
        // 0 is the value of the no limit (AGL-021, D2); the absent key keeps
        // the default (D40).
        let max_steps_total = match nonneg(&mut file, "max_steps_total")? {
            None => Some(DEFAULT_MAX_STEPS_TOTAL),
            Some(0) => None,
            Some(n) => Some(n),
        };
        let max_think_tokens =
            positive(&mut file, "max_think_tokens")?.unwrap_or(DEFAULT_MAX_THINK_TOKENS);
        let max_action_tokens =
            positive(&mut file, "max_action_tokens")?.unwrap_or(DEFAULT_MAX_ACTION_TOKENS);
        // 0 is allowed and means no stirs (AGL-030, D6); the absent key keeps
        // the default.
        let max_stirs = nonneg(&mut file, "max_stirs")?.unwrap_or(DEFAULT_MAX_STIRS);
        // The two words the runtime fills in the Function's own text
        // (AGL-017, D8); other brace text stays as written.
        let system = system
            .replace("{runtime_prefix}", RUNTIME_PREFIX)
            .replace("{max_action_tokens}", &max_action_tokens.to_string());
        let compact = file
            .string("compact")?
            .unwrap_or_else(|| DEFAULT_COMPACT.into());
        let max_compact_tokens =
            positive(&mut file, "max_compact_tokens")?.unwrap_or(DEFAULT_MAX_COMPACT_TOKENS);
        let max_record_seconds =
            positive(&mut file, "max_record_seconds")?.unwrap_or(DEFAULT_MAX_RECORD_SECONDS);
        let sampling = match file.section("sampling")? {
            None => Sampling::default(),
            Some(sampling) => read_sampling(sampling)?,
        };
        let initial = file.required_string("initial")?;
        let mut sections = file.required_section("state")?.sections()?;
        let names: Vec<String> = sections.iter().map(|(name, _)| name.clone()).collect();
        let index = |section: &Section, key: &str, name: &str| {
            names
                .iter()
                .position(|n| n == name)
                .ok_or_else(|| problem(section, key, FunctionProblem::UnknownState(name.into())))
        };
        let initial = index(&file, "initial", &initial)?;

        let context = Context {
            format: &format,
            format_name: &format_name,
            index: &index,
        };
        let states = sections
            .iter_mut()
            .enumerate()
            .map(|(i, (name, section))| read_state(name, section, &context, i))
            .collect::<Result<Vec<_>, _>>()?;
        check_graph(path, &states, initial)?;
        Ok(Function {
            format,
            system,
            preserve_thinking,
            max_steps_total,
            max_think_tokens,
            max_action_tokens,
            max_stirs,
            compact,
            max_compact_tokens,
            max_record_seconds,
            sampling,
            initial,
            states,
        })
    }
}

/// What reading a state needs from the rest of the Function.
struct Context<'a> {
    format: &'a Format,
    format_name: &'a str,
    /// Resolves a state name read at `key` of a section.
    index: &'a dyn Fn(&Section, &str, &str) -> Result<usize, Error>,
}

/// The built-in forge tool with unconstrained parameters (AGL-011).
fn builtin_forge() -> Tool {
    action::builtin_tools()
        .into_iter()
        .find(|tool| tool.name == "forge")
        .expect("the built-in forge tool")
}

fn problem(section: &Section, key: &str, problem: FunctionProblem) -> Error {
    Error::Function {
        path: section.path().into(),
        key: section.key(key),
        problem,
    }
}

/// Takes `key` as an integer of at least 1.
fn positive(section: &mut Section, key: &str) -> Result<Option<u32>, Error> {
    match section.integer(key)? {
        None => Ok(None),
        Some(n) if n >= 1 => Ok(Some(u32::try_from(n).unwrap_or(u32::MAX))),
        Some(_) => Err(problem(section, key, FunctionProblem::NotPositive)),
    }
}

/// A non-negative integer: `max_steps_total` alone takes 0, the value of the
/// no limit (AGL-021, D2).
fn nonneg(section: &mut Section, key: &str) -> Result<Option<u32>, Error> {
    match section.integer(key)? {
        None => Ok(None),
        Some(n) if n >= 0 => Ok(Some(u32::try_from(n).unwrap_or(u32::MAX))),
        Some(_) => Err(problem(section, key, FunctionProblem::NotPositive)),
    }
}

fn read_sampling(mut section: Section) -> Result<Sampling, Error> {
    section.check_keys(&[
        "temperature",
        "top_p",
        "top_k",
        "min_p",
        "presence_penalty",
        "repeat_penalty",
    ])?;
    Ok(Sampling {
        temperature: section.number("temperature")?,
        top_p: section.number("top_p")?,
        top_k: section.integer("top_k")?,
        min_p: section.number("min_p")?,
        presence_penalty: section.number("presence_penalty")?,
        repeat_penalty: section.number("repeat_penalty")?,
    })
}

fn read_state(
    name: &str,
    section: &mut Section,
    context: &Context,
    index: usize,
) -> Result<State, Error> {
    section.check_keys(&[
        "instruction",
        "tools",
        "answer",
        "think",
        "max_steps",
        "max_visits",
        "on_limit",
        "next",
    ])?;
    let instruction = section.string("instruction")?;
    let mut tools = match section.section("tools")? {
        None => Vec::new(),
        Some(tools) => read_tools(tools, context.format)?,
    };
    let mut forge_tool = tools.iter().any(|tool| tool.name == "forge");
    if !forge_tool && !tools.is_empty() {
        // The runtime's tool: every state that allows a tool allows the
        // forge too (AGL-011, Q1; AGL-017, D5); an answer-only state stays
        // answer-only.
        tools.push(builtin_forge());
        forge_tool = true;
    }
    let answer = section.bool("answer")?.unwrap_or(false);
    let think = match section.bool("think")? {
        None => context.format.think.is_some(),
        Some(true) if context.format.think.is_none() => {
            return Err(problem(
                section,
                "think",
                FunctionProblem::NoThinking {
                    format: context.format_name.into(),
                },
            ));
        }
        Some(think) => think,
    };
    let max_steps = positive(section, "max_steps")?;
    let max_visits = positive(section, "max_visits")?;
    let on_limit = section
        .string("on_limit")?
        .map(|target| (context.index)(section, "on_limit", &target))
        .transpose()?;
    let limited = max_steps.is_some() || max_visits.is_some();
    if limited && on_limit.is_none() {
        return Err(Error::MissingKey {
            path: section.path().into(),
            key: section.key("on_limit"),
        });
    }
    if !limited && on_limit.is_some() {
        return Err(problem(
            section,
            "on_limit",
            FunctionProblem::OnLimitWithoutLimit,
        ));
    }

    if tools.is_empty() && !answer {
        return Err(problem(section, "", FunctionProblem::NoActions));
    }
    let mut transitions = Vec::new();
    let mut otherwise = None;
    if let Some(mut next) = section.section("next")? {
        if tools.is_empty() {
            return Err(problem(section, "next", FunctionProblem::NextWithoutTools));
        }
        for (key, target) in next.string_entries()? {
            let target = (context.index)(&next, &key, &target)?;
            if key == "else" {
                otherwise = Some(target);
                continue;
            }
            let bad_key = || problem(&next, &key, FunctionProblem::BadTransitionKey);
            let (tool, outcome) = match key.split_once('.') {
                None => (key.as_str(), None),
                Some((tool, name)) => {
                    let outcome = action::outcomes(tool).iter().find(|o| o.name() == name);
                    (tool, Some(*outcome.ok_or_else(bad_key)?))
                }
            };
            if !tools.iter().any(|t| t.name == tool) {
                return Err(bad_key());
            }
            transitions.push((tool.to_owned(), outcome, target));
        }
    }
    // The forge keeps the state unless `next` says otherwise: a step in
    // the same visit, the fallback for its outcomes (AGL-011, Q2).
    if forge_tool
        && !transitions
            .iter()
            .any(|(tool, outcome, _)| tool == "forge" && outcome.is_none())
    {
        transitions.push(("forge".into(), None, index));
    }
    // Every tool and outcome needs a transition (D21).
    for tool in &tools {
        for &outcome in action::outcomes(&tool.name) {
            let covered = otherwise.is_some()
                || transitions
                    .iter()
                    .any(|(t, o, _)| *t == tool.name && o.is_none_or(|o| o == outcome));
            if !covered {
                return Err(problem(
                    section,
                    "next",
                    FunctionProblem::MissingTransition {
                        tool: tool.name.clone(),
                        outcome: outcome.name(),
                    },
                ));
            }
        }
    }
    Ok(State {
        name: name.into(),
        instruction,
        actions: Actions { tools, answer },
        think,
        max_steps,
        max_visits,
        on_limit,
        transitions,
        otherwise,
    })
}

/// Reads `tools`: the built-in tools a state allows and their constraints.
fn read_tools(mut section: Section, format: &Format) -> Result<Vec<Tool>, Error> {
    section.check_keys(&["bash", "edit", "forge", "memory", "search", "view"])?;
    let mut tools = Vec::new();
    for mut tool in action::builtin_tools() {
        let Some(mut constraints) = section.section(&tool.name)? else {
            continue;
        };
        let params: &'static [&'static str] = match tool.name.as_str() {
            "bash" => &["command", "timeout"],
            "forge" => &["action", "name", "title", "old", "new", "section", "text"],
            "view" => &["path"],
            "search" => &["language", "limit", "query", "source"],
            "memory" => &["action", "hook", "name", "new", "old", "text", "title"],
            _ => &["path", "old", "new"],
        };
        constraints.check_keys(params)?;
        for param in &mut tool.params {
            if let Some(section) = constraints.section(&param.name)? {
                let constraint = read_constraint(section, param.kind, &param.name, format)?;
                param.constraint = constraint;
            }
        }
        tools.push(tool);
    }
    Ok(tools)
}

/// Reads one parameter constraint (D41, D69-D72, D88, D93).
fn read_constraint(
    mut section: Section,
    kind: Kind,
    param: &str,
    format: &Format,
) -> Result<Constraint, Error> {
    match kind {
        Kind::Text => {
            section.check_keys(&["pattern", "enum"])?;
            match (section.string("pattern")?, section.strings("enum")?) {
                (Some(source), None) => {
                    let expr = pattern::parse(&source).map_err(|error| {
                        problem(&section, "pattern", FunctionProblem::Pattern(error))
                    })?;
                    let end = format.value_end_start();
                    if expr.may_contain(end) {
                        return Err(problem(
                            &section,
                            "pattern",
                            FunctionProblem::PatternMayEndValue { end },
                        ));
                    }
                    Ok(Constraint::Pattern(expr))
                }
                (None, Some(values)) if values.is_empty() => {
                    Err(problem(&section, "enum", FunctionProblem::EmptyEnum))
                }
                (None, Some(values)) => Ok(Constraint::OneOf(values)),
                _ => Err(problem(&section, "", FunctionProblem::ConstraintKeys)),
            }
        }
        Kind::Int => {
            section.check_keys(&["int"])?;
            let Some(mut range) = section.section("int")? else {
                return Err(problem(&section, "", FunctionProblem::ConstraintKeys));
            };
            range.check_keys(&["min", "max"])?;
            let min = range.integer("min")?;
            let max = range.integer("max")?;
            let (Some(min), Some(max)) = (min, max) else {
                let missing = if min.is_none() { "min" } else { "max" };
                return Err(Error::MissingKey {
                    path: range.path().into(),
                    key: range.key(missing),
                });
            };
            if min > max || (param == "timeout" && min < 1) {
                return Err(problem(&section, "int", FunctionProblem::BadRange));
            }
            Ok(Constraint::Int { min, max })
        }
    }
}

/// Rejects `on_limit` cycles (D83) and unreachable states (D91).
fn check_graph(path: &Path, states: &[State], initial: usize) -> Result<(), Error> {
    let error = |state: &State, key: &str, problem| Error::Function {
        path: path.into(),
        key: format!("state.{}{key}", state.name),
        problem,
    };
    for (start, state) in states.iter().enumerate() {
        let mut current = state.on_limit;
        for _ in 0..states.len() {
            match current {
                Some(next) if next == start => {
                    return Err(error(state, ".on_limit", FunctionProblem::OnLimitCycle));
                }
                Some(next) => current = states[next].on_limit,
                None => break,
            }
        }
    }
    let mut reached = vec![false; states.len()];
    let mut queue = VecDeque::from([initial]);
    while let Some(index) = queue.pop_front() {
        if std::mem::replace(&mut reached[index], true) {
            continue;
        }
        queue.extend(states[index].targets());
    }
    match reached.iter().position(|reached| !reached) {
        Some(index) => Err(error(&states[index], "", FunctionProblem::Unreachable)),
        None => Ok(()),
    }
}

#[cfg(test)]
pub mod tests;
