//! Functions: the policy and the model of an agent, written in Nix and
//! checked when loaded (D11, D15, D17, D21, D39, D41-D43, D82-D93, D139,
//! D140; AGL-039, D7, D8, D10, D17-D21, D40, D41).

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value as Json, json};

use crate::action::{self, Actions, Constraint, Kind, Tool};
use crate::backend::RUNTIME_PREFIX;
use crate::download::FileSource;
use crate::error::Error;
use crate::function::error::{Error as FunctionError, FunctionProblem};
use crate::format::Format;
use crate::grammar::pattern;
use crate::machine::Transitions;
use crate::media::sha256_hex;
use crate::nix::{self, Section};
use crate::settings::{ServerFile, ServerKind};

/// The version of the `run` snapshot of a session header (AGL-063, D4):
/// the evaluated Function, so a resume runs on the configuration the
/// session was made with, the Function file not needed.
const RUN_SCHEMA: u64 = 1;

/// `max_steps_total` when the Function does not set it (D40).
const DEFAULT_MAX_STEPS_TOTAL: u32 = 50;

/// The thinking's token limit of one generation when the Function does
/// not set it (D18, D96).
const DEFAULT_MAX_THINK_TOKENS: u32 = 4096;

/// The action's limit when the Function does not set it: this share of the
/// context (AGL-043 F14: an eighth, 16384 at 131072).
const ACTION_SHARE: usize = 8;
/// The share of the context a tool's output takes at most, when the Function
/// does not say (AGL-049, D3b): 4096 tokens at 128k, 8192 at 256k.
const TOOL_OUTPUT_SHARE: usize = 32;

/// The summary's limit when the Function does not set it: this share of
/// the context within the bounds (AGL-043 F6: a sixteenth, 4096 to 16384).
const COMPACT_SHARE: usize = 16;
const MIN_COMPACT_TOKENS: usize = 4096;
const MAX_COMPACT_TOKENS: usize = 16384;

/// The stirs of one step of the stirred mode when the Function does not set
/// them (AGL-030, D6): two stirs, three action attempts in all.
const DEFAULT_MAX_STIRS: u32 = 2;

/// The summary instruction when the Function does not set `compact`
/// (D140).
const DEFAULT_COMPACT: &str = "Summarize the conversation so far so that you can continue the \
     work from this summary alone: the user's requests, what has been done and found, files \
     and commands that matter, and what remains to be done. Write only the summary.";

/// The keys of `policy` (AGL-039, D17).
const POLICY_KEYS: &[&str] = &[
    "system",
    "initial",
    "state",
    "max_steps_total",
    "max_think_tokens",
    "max_action_tokens",
    "max_tool_output_tokens",
    "compact",
    "max_compact_tokens",
];

/// The keys of `model` (AGL-039, D2, D5, D17, D19).
const MODEL_KEYS: &[&str] = &[
    "kind",
    "format",
    "weights",
    "context",
    "spec_type",
    "draft",
    "args",
    "mmproj",
    "mmproj_offload",
    "image_max_tokens",
    "strata",
    "name",
    "gguf",
    "env",
    "sampling",
    "compact_early",
    "max_stirs",
    "preserve_thinking",
    "source",
];

/// The keys of the `llama` kind's server in `model` (AGL-033, D1);
/// `context` and `args` are both kinds'.
const LLAMA_MODEL_KEYS: &[&str] = &[
    "weights",
    "spec_type",
    "draft",
    "mmproj",
    "mmproj_offload",
    "image_max_tokens",
];

/// The keys of the `strata` kind's server in `model` (AGL-089, D5): the
/// engine's variant, the model's name, its GGUF shards, the engine's
/// environment.
pub const STRATA_MODEL_KEYS: &[&str] = &["strata", "name", "gguf", "env"];

/// The files a `source` may name (AGL-039, D4, D5, D19); `gguf` is a list,
/// one source a shard (AGL-089, D4).
const SOURCE_KEYS: &[&str] = &["weights", "mmproj", "draft", "gguf"];

/// A loaded Function.
#[derive(Debug)]
pub struct Function {
    /// The file the Function came from; a built-in says so
    /// (`<built-in>/functions/<name>.nix`).
    pub path: PathBuf,
    /// What it is made for, as a human reads it (AGL-089, D12): "for a GPU
    /// with 10 GB of VRAM and more"; every built-in has one (D14).
    pub description: Option<String>,
    /// The evaluated Nix value the Function was read from (AGL-063, D4):
    /// the system, the policy figures, the sampling, the format, the server
    /// profile and the states with their transitions, as Nix wrote them.
    /// It is the content of a session's `run` snapshot.
    value: Json,
    /// Where the files of its model come from (AGL-050, D8).
    pub sources: Vec<FileSource>,
    pub format: Format,
    pub system: String,
    pub preserve_thinking: bool,
    /// The steps of a turn; `None` when unlimited (D40, AGL-021, D2).
    pub max_steps_total: Option<u32>,
    pub max_think_tokens: u32,
    /// The Function's `max_action_tokens`; `None` is a share of the
    /// context (`action_tokens`, AGL-043 F14).
    pub max_action_tokens: Option<u32>,
    /// The Function's `max_tool_output_tokens`; `None` is a share of the
    /// context (`output_tokens`, AGL-049).
    pub max_tool_output_tokens: Option<u32>,
    /// The stirs of one step of the stirred mode (AGL-030, D6); the parse
    /// of the action is retried at most this many times after a failure.
    /// `0` means no stirs: a parse failure stops the turn.
    pub max_stirs: u32,
    /// The instruction that asks for a summary of the history (D139).
    pub compact: String,
    /// The Function's `max_compact_tokens`; `None` is a share of the
    /// context (`compact_tokens`, AGL-043 F6).
    pub max_compact_tokens: Option<u32>,
    /// Compact early (AGL-038, D6, D9): when the history plus the most a
    /// step can add and the summary would not fit, so the summary always
    /// fits at the next step; off, the compaction waits until a step does
    /// not fit.
    pub compact_early: bool,
    pub sampling: Sampling,
    pub initial: usize,
    pub states: Vec<State>,
    /// The server side of `model` (AGL-039, D17, D21): the kind and the
    /// model keys, checked against the kind; the urls and `engine` are the
    /// settings'.
    pub model: ServerFile,
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
    /// The transitions and the limits one reader reads (AGL-118, D4;
    /// AGL-121, D56).
    pub transitions: Transitions,
}

impl crate::machine::State for State {
    fn name(&self) -> &str {
        &self.name
    }

    fn transitions(&self) -> &Transitions {
        &self.transitions
    }
}

/// The names of the built-in Functions (AGL-036, D3; AGL-039, D25, D28):
/// the attributes of `agl.functions`, the Functions of the repository
/// agentLIBRE-functions (AGL-053).
pub fn builtin_names() -> Result<Vec<String>, Error> {
    let path = Path::new("<built-in>/functions");
    let value = nix::eval_with_agl(path, "builtins.attrNames agl.functions")?;
    serde_json::from_value(value).map_err(|error| Error::Nix(nix::Error::Eval {
        path: path.into(),
        detail: error.to_string(),
    }))
}

/// Rejects a Function name that cannot name a file in its directory.
pub fn check_name(name: &str) -> Result<(), Error> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return Err(Error::InvalidName(name.into()));
    }
    Ok(())
}

impl Function {
    /// The table of the policy's states: what a `Machine` runs on
    /// (AGL-118, D4).
    pub fn table(&self) -> crate::machine::Table<'_, State> {
        crate::machine::Table {
            states: &self.states,
            initial: self.initial,
            max_steps_total: self.max_steps_total,
        }
    }

    /// Loads Function `name` (AGL-039, D10): the workspace's
    /// `<name>.nix` (`workspace_functions`), then the user's
    /// (`user_functions`), then the built-in `agl.functions.<name>`. An
    /// absent directory is simply skipped. A file is a function of
    /// `{ agl, ... }` (D8).
    pub fn load(dirs: &[&Path], name: &str) -> Result<Function, Error> {
        check_name(name)?;
        for dir in dirs {
            let path = dir.join(format!("{name}.nix"));
            if path.is_file() {
                return Function::from_value(&path, nix::eval_file(&path)?);
            }
        }
        let path = PathBuf::from(format!("<built-in>/functions/{name}.nix"));
        let value = nix::eval_with_agl(&path, &format!("agl.functions.{} or null", nix::key(name)))?;
        if value.is_null() {
            return Err(FunctionError::NotFound {
                name: name.into(),
                paths: dirs.iter().map(|dir| dir.to_path_buf()).collect(),
            }
            .into());
        }
        Function::from_value(&path, value)
    }

    /// The action's limit at a context of `context` tokens: the Function's
    /// `max_action_tokens`, else an eighth of the context (AGL-043 F14).
    pub fn action_tokens(&self, context: usize) -> u32 {
        self.max_action_tokens
            .unwrap_or_else(|| u32::try_from(context / ACTION_SHARE).unwrap_or(u32::MAX))
    }

    /// How many tokens of a tool's output the model sees at a context of
    /// `context` tokens: the Function's `max_tool_output_tokens`, else a
    /// thirty-second of the context (AGL-049).
    pub fn output_tokens(&self, context: usize) -> usize {
        self.max_tool_output_tokens
            .map_or(context / TOOL_OUTPUT_SHARE, |tokens| tokens as usize)
    }

    /// The summary's limit at a context of `context` tokens: the
    /// Function's `max_compact_tokens`, else a sixteenth of the context
    /// between 4096 and 16384 (AGL-043 F6).
    pub fn compact_tokens(&self, context: usize) -> u32 {
        self.max_compact_tokens.unwrap_or_else(|| {
            let share = (context / COMPACT_SHARE).clamp(MIN_COMPACT_TOKENS, MAX_COMPACT_TOKENS);
            u32::try_from(share).unwrap_or(u32::MAX)
        })
    }

    /// The system text at a context of `context` tokens: the action's
    /// limit in place of `{max_action_tokens}` (AGL-017, D8; AGL-043 F14).
    pub fn system_at(&self, context: usize) -> String {
        self.system
            .replace("{max_action_tokens}", &self.action_tokens(context).to_string())
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

    /// Reads the evaluated Function `value` of the file `path` (AGL-039,
    /// D17): `policy` and `model`, both required (D21), and the optional
    /// `description` (AGL-089, D12).
    pub fn from_value(path: &Path, value: serde_json::Value) -> Result<Function, Error> {
        // Kept for the `run` snapshot (AGL-063, D4); `Section::top` takes
        // the value by value.
        let snapshot = value.clone();
        let mut file = Section::top(path, value)?;
        file.check_keys(&["policy", "model", "description"])?;
        let description = file.string("description")?;
        let mut model = file.required_section("model")?;
        let mut policy = file.required_section("policy")?;

        model.check_keys(MODEL_KEYS)?;
        let format = Format::from_section(model.required_section("format")?)?;
        let preserve_thinking = model.bool("preserve_thinking")?.unwrap_or(true);
        // 0 is allowed and means no stirs (AGL-030, D6); the absent key keeps
        // the default.
        let max_stirs = nonneg(&mut model, "max_stirs")?.unwrap_or(DEFAULT_MAX_STIRS);
        let compact_early = model.bool("compact_early")?.unwrap_or(false);
        let sampling = match model.section("sampling")? {
            None => Sampling::default(),
            Some(sampling) => read_sampling(sampling)?,
        };
        let source = model.section("source")?;
        let server = read_model(&mut model)?;
        let sources = match source {
            Some(source) => read_sources(source, &server)?,
            None => Vec::new(),
        };

        policy.check_keys(POLICY_KEYS)?;
        let system = policy.required_string("system")?;
        // 0 is the value of the no limit (AGL-021, D2); the absent key keeps
        // the default (D40).
        let max_steps_total = match nonneg(&mut policy, "max_steps_total")? {
            None => Some(DEFAULT_MAX_STEPS_TOTAL),
            Some(0) => None,
            Some(n) => Some(n),
        };
        let max_think_tokens =
            positive(&mut policy, "max_think_tokens")?.unwrap_or(DEFAULT_MAX_THINK_TOKENS);
        let max_action_tokens = positive(&mut policy, "max_action_tokens")?;
        let max_tool_output_tokens = positive(&mut policy, "max_tool_output_tokens")?;
        // The runtime fills two words in the Function's own text (AGL-017,
        // D8): the prefix here, the action's limit where the context is
        // known (`system_at`, AGL-043 F14); other brace text stays.
        let system = system.replace("{runtime_prefix}", RUNTIME_PREFIX);
        let compact = policy
            .string("compact")?
            .unwrap_or_else(|| DEFAULT_COMPACT.into());
        let max_compact_tokens = positive(&mut policy, "max_compact_tokens")?;
        let initial = policy.required_string("initial")?;
        let mut sections = policy.required_section("state")?.sections()?;
        let names: Vec<String> = sections.iter().map(|(name, _)| name.clone()).collect();
        let index = |section: &Section, key: &str, name: &str| {
            names
                .iter()
                .position(|n| n == name)
                .ok_or_else(|| problem(section, key, FunctionProblem::UnknownState(name.into())))
        };
        let initial = index(&policy, "initial", &initial)?;

        let context = Context {
            format: &format,
            index: &index,
        };
        let states = sections
            .iter_mut()
            .enumerate()
            .map(|(i, (name, section))| read_state(name, section, &context, i))
            .collect::<Result<Vec<_>, _>>()?;
        check_graph(path, &states, initial)?;
        Ok(Function {
            path: path.to_path_buf(),
            description,
            value: snapshot,
            sources,
            format,
            system,
            preserve_thinking,
            max_steps_total,
            max_think_tokens,
            max_action_tokens,
            max_tool_output_tokens,
            max_stirs,
            compact,
            max_compact_tokens,
            compact_early,
            sampling,
            initial,
            states,
            model: server,
        })
    }

    /// The `run` snapshot for a session header (AGL-063, D4): the Function's
    /// identity and its evaluated value, so a resume reconstructs the
    /// Function in memory without the file.
    pub fn snapshot(&self) -> Json {
        let mut source = json!({ "path": self.path.to_string_lossy() });
        // A real file carries its sha256; a built-in's pseudo path does
        // not, and its name in the path is its identity.
        if self.path.is_file()
            && let Ok(bytes) = fs::read(&self.path)
        {
            source["sha256"] = sha256_hex(&bytes).into();
        }
        json!({
            "schema": RUN_SCHEMA,
            "source": source,
            "value": self.value,
        })
    }

    /// Reconstructs the Function of a session's `run` snapshot (AGL-063,
    /// D4): the evaluated value is read as a Function, the file not needed.
    /// The path is the one the snapshot records, for display and errors.
    pub fn from_snapshot(snapshot: &Json) -> Result<Function, Error> {
        if snapshot.get("schema").and_then(Json::as_u64) != Some(RUN_SCHEMA) {
            return Err(FunctionError::RunSchema {
                expected: RUN_SCHEMA,
                found: snapshot
                    .get("schema")
                    .and_then(Json::as_u64)
                    .map_or_else(|| "unknown".into(), |v| v.to_string()),
            }
            .into());
        }
        let path = snapshot
            .get("source")
            .and_then(|source| source.get("path"))
            .and_then(Json::as_str)
            .unwrap_or("<run>");
        let value = snapshot.get("value").cloned().ok_or_else(|| {
            FunctionError::RunSnapshot {
                detail: "the snapshot has no value".into(),
            }
        })?;
        Self::from_value(Path::new(path), value)
    }

    /// The loose-continue Function of a session whose Function is not found
    /// (AGL-063, D4): one state, the built-in tools the session recorded
    /// (their default parameters; a tool not among them is skipped), the
    /// answer always allowed, the budgets of the last step, the Qwen format,
    /// no grammar (the stirred mode). The server profile is empty, so the
    /// settings of the machine run the server.
    pub fn loose(tools: &[String], budgets: Option<(u32, u32)>) -> Function {
        let builtin = action::builtin_tools();
        let state_tools = tools
            .iter()
            .filter_map(|name| builtin.iter().find(|tool| &tool.name == name).cloned())
            .collect();
        let (max_think_tokens, max_action_tokens) = match budgets {
            Some((think, action)) => (think, Some(action)),
            None => (DEFAULT_MAX_THINK_TOKENS, None),
        };
        Function {
            path: PathBuf::from("<loose-continue>"),
            description: None,
            value: json!({}),
            sources: Vec::new(),
            format: Format::qwen(),
            system: "The Function this session was made with is not available. \
                    Continue the conversation with the tools it used."
                .into(),
            preserve_thinking: true,
            max_steps_total: None,
            max_think_tokens,
            max_action_tokens,
            max_tool_output_tokens: None,
            max_stirs: DEFAULT_MAX_STIRS,
            compact: DEFAULT_COMPACT.into(),
            max_compact_tokens: None,
            compact_early: false,
            sampling: Sampling::default(),
            initial: 0,
            states: vec![State {
                name: "loose".into(),
                instruction: None,
                actions: Actions {
                    tools: state_tools,
                    answer: true,
                },
                think: true,
                transitions: Transitions {
                    max_steps: None,
                    max_visits: None,
                    on_limit: None,
                    next: Vec::new(),
                    otherwise: Some(0),
                },
            }],
            model: ServerFile::default(),
        }
    }
}

/// Reads the server side of `model` (AGL-033, D1; AGL-036, D1, D5;
/// AGL-039, D19, D21, D24): `kind` (`llama` when absent), the weights and
/// the keys of the kind; a key of the other kind is an error, the llama
/// kind needs `weights`, the strata kind `strata`, `name` and `gguf`
/// (AGL-089, D5).
fn read_model(model: &mut Section) -> Result<ServerFile, Error> {
    let kind = match model.string("kind")? {
        None => ServerKind::Llama,
        Some(kind) => ServerKind::parse(&kind).ok_or_else(|| Error::UnknownServerKind {
            path: model.path().into(),
            value: kind,
        })?,
    };
    let other_kind = |key: &str| problem(model, key, FunctionProblem::ServerKeyOfOtherKind(key.into()));
    match kind {
        ServerKind::Llama => {
            if let Some(key) = STRATA_MODEL_KEYS.iter().find(|key| model.has(key)) {
                return Err(other_kind(key));
            }
        }
        ServerKind::Strata => {
            if let Some(key) = LLAMA_MODEL_KEYS.iter().find(|key| model.has(key)) {
                return Err(other_kind(key));
            }
        }
    }
    let weights = weights_name(model, "weights")?;
    let draft = weights_name(model, "draft")?;
    let mmproj = weights_name(model, "mmproj")?;
    let strata = model.string("strata")?;
    let strata_name = model.string("name")?;
    if let Some(name) = &strata_name
        && !is_file_name(name)
    {
        return Err(problem(model, "name", FunctionProblem::BadWeightsName));
    }
    let gguf = model.strings("gguf")?;
    if gguf
        .iter()
        .flatten()
        .any(|name| !Path::new(name).is_absolute() && !is_file_name(name))
    {
        return Err(problem(model, "gguf", FunctionProblem::BadWeightsName));
    }
    let env = model
        .section("env")?
        .map(|mut env| env.string_entries().map(|entries| entries.into_iter().collect()))
        .transpose()?;
    match kind {
        ServerKind::Llama if weights.is_none() => {
            return Err(problem(model, "weights", FunctionProblem::ServerModelMissing));
        }
        ServerKind::Strata => {
            let missing = [
                ("strata", strata.is_none()),
                ("name", strata_name.is_none()),
                ("gguf", gguf.as_ref().is_none_or(Vec::is_empty)),
            ];
            if let Some((key, _)) = missing.iter().find(|(_, missing)| *missing) {
                return Err(problem(
                    model,
                    key,
                    FunctionProblem::ServerStrataKeyMissing((*key).to_owned()),
                ));
            }
        }
        _ => {}
    }
    Ok(ServerFile {
        kind: Some(kind.name().to_owned()),
        model: weights,
        context: positive(model, "context")?,
        spec_type: model.string("spec_type")?,
        draft,
        args: model.strings("args")?,
        mmproj,
        mmproj_offload: model.bool("mmproj_offload")?,
        image_max_tokens: positive(model, "image_max_tokens")?,
        strata,
        strata_name,
        gguf,
        env,
        ..ServerFile::default()
    })
}

/// Whether `name` is a file name: not empty, no folder, not hidden.
fn is_file_name(name: &str) -> bool {
    !(name.is_empty() || name.contains('/') || name.starts_with('.'))
}

/// A weights file of `model` (AGL-039, D24a): a file name, found in the
/// weights folders, or an absolute path, taken as it is.
fn weights_name(model: &mut Section, key: &str) -> Result<Option<String>, Error> {
    let name = model.string(key)?;
    if let Some(name) = &name
        && !Path::new(name).is_absolute()
        && !is_file_name(name)
    {
        return Err(problem(model, key, FunctionProblem::BadWeightsName));
    }
    Ok(name)
}

/// Reads `source` (AGL-039, D4, D5; AGL-050, D8): keyed by a file of the
/// model that the model sets, each a url at a fixed version, its sha256 and
/// its size; `gguf`, one such source for each shard, in their order
/// (AGL-089, D4). The files come back under their local names.
fn read_sources(mut source: Section, server: &ServerFile) -> Result<Vec<FileSource>, Error> {
    source.check_keys(SOURCE_KEYS)?;
    let mut sources = Vec::new();
    if let Some(shards) = source.tables("gguf")? {
        let names = server.gguf.clone().unwrap_or_default();
        if shards.len() != names.len() {
            return Err(problem(&source, "gguf", FunctionProblem::SourceShardsMismatch));
        }
        for (name, file) in names.into_iter().zip(shards) {
            sources.push(read_source(file, name)?);
        }
    }
    for (key, file) in source.sections()? {
        let name = match key.as_str() {
            "weights" => server.model.clone(),
            "mmproj" => server.mmproj.clone(),
            _ => server.draft.clone(),
        };
        let Some(name) = name else {
            return Err(problem(&source, &key, FunctionProblem::SourceWithoutFile));
        };
        sources.push(read_source(file, name)?);
    }
    Ok(sources)
}

/// One source: its url, sha256 and size, for the file `name`.
fn read_source(mut file: Section, name: String) -> Result<FileSource, Error> {
    file.check_keys(&["url", "sha256", "size"])?;
    let url = file.required_string("url")?;
    if !url.starts_with("https://") {
        return Err(problem(&file, "url", FunctionProblem::BadSourceUrl));
    }
    let sha256 = file.required_string("sha256")?;
    if sha256.len() != 64 || !sha256.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
        return Err(problem(&file, "sha256", FunctionProblem::BadSourceSha256));
    }
    let size = file
        .integer("size")?
        .ok_or_else(|| Error::MissingKey {
            path: file.path().into(),
            key: file.key("size"),
        })
        .and_then(|size| {
            u64::try_from(size)
                .ok()
                .filter(|size| *size > 0)
                .ok_or_else(|| problem(&file, "size", FunctionProblem::BadSourceSize))
        })?;
    Ok(FileSource { name, url, sha256, size })
}

/// What reading a state needs from the rest of the Function.
struct Context<'a> {
    format: &'a Format,
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
    FunctionError::Invalid {
        path: section.path().into(),
        key: section.key(key),
        problem,
    }
    .into()
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
        // In the table's order, as the prompt and the grammar list them
        // (AGL-067, D11).
        tools.sort_by_key(|tool| crate::tools::position(&tool.name));
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
                    format: "model.format".into(),
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
        // `next.<tool>.<outcome>`, `next.<tool>.any` and `next.otherwise`
        // (AGL-039, D40); `next.<tool>` is always a set (D41).
        for key in next.keys() {
            if key == "otherwise" {
                let target = next.required_string(&key)?;
                otherwise = Some((context.index)(&next, &key, &target)?);
                continue;
            }
            if !tools.iter().any(|t| t.name == key) {
                return Err(problem(&next, &key, FunctionProblem::BadTransitionKey));
            }
            let mut of_tool = next.required_section(&key)?;
            for name in of_tool.keys() {
                let target = of_tool.required_string(&name)?;
                let target = (context.index)(&of_tool, &name, &target)?;
                if name == "any" {
                    transitions.push((key.clone(), None, target));
                    continue;
                }
                // The name must be one of the tool's outcomes (D43).
                if !crate::tools::outcomes(&key)
                    .iter()
                    .any(|o| o.name() == name)
                {
                    return Err(problem(&of_tool, &name, FunctionProblem::BadTransitionKey));
                }
                transitions.push((key.clone(), Some(name), target));
            }
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
        for &outcome in crate::tools::outcomes(&tool.name) {
            let covered = otherwise.is_some()
                || transitions
                    .iter()
                    .any(|(t, o, _)| {
                        *t == tool.name && o.as_deref().is_none_or(|o| o == outcome.name())
                    });
            if !covered {
                return Err(problem(
                    section,
                    "next",
                    FunctionProblem::MissingTransition {
                        tool: tool.name.clone(),
                        outcome: outcome.name().into(),
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
        transitions: Transitions {
            max_steps,
            max_visits,
            on_limit,
            next: transitions,
            otherwise,
        },
    })
}

/// Reads `tools`: the built-in tools a state allows and their constraints.
fn read_tools(mut section: Section, format: &Format) -> Result<Vec<Tool>, Error> {
    let mut names: Vec<&'static str> = crate::tools::SPECS.iter().map(|spec| spec.name).collect();
    names.sort_unstable();
    section.check_keys(&names)?;
    let mut tools = Vec::new();
    for (spec, mut tool) in crate::tools::SPECS.iter().zip(action::builtin_tools()) {
        let Some(mut constraints) = section.section(&tool.name)? else {
            continue;
        };
        let params: Vec<&'static str> = spec.params.iter().map(|param| param.name).collect();
        constraints.check_keys(&params)?;
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
    let error = |state: &State, key: &str, problem| {
        Error::from(FunctionError::Invalid {
            path: path.into(),
            key: format!("policy.state.{}{key}", state.name),
            problem,
        })
    };
    for (start, state) in states.iter().enumerate() {
        let mut current = state.transitions.on_limit;
        for _ in 0..states.len() {
            match current {
                Some(next) if next == start => {
                    return Err(error(state, ".on_limit", FunctionProblem::OnLimitCycle));
                }
                Some(next) => current = states[next].transitions.on_limit,
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
        queue.extend(states[index].transitions.targets());
    }
    match reached.iter().position(|reached| !reached) {
        Some(index) => Err(error(&states[index], "", FunctionProblem::Unreachable)),
        None => Ok(()),
    }
}

pub mod catalog;
pub mod error;

#[cfg(test)]
pub mod tests;
