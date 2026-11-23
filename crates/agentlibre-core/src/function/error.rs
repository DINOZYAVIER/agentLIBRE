//! What can go wrong with a Function: not found, built in, existing
//! already, or wrong at a key of its file (AGL-121, D66).

use std::path::PathBuf;

use crate::grammar::pattern::PatternError;
use crate::i18n::{Lang, Msg, m};
use crate::tr;

/// A Function's own error, wrapped by the core's `Error::Function`
/// (AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// Neither the looked-in `paths` nor a built-in Function is called
    /// `name`.
    NotFound { name: String, paths: Vec<PathBuf> },
    /// `/function edit` was asked of a built-in (AGL-102, D3): its file
    /// belongs to the Functions' repository.
    BuiltIn { name: String },
    /// `/function new` took a name one of the sources already has
    /// (AGL-102, D3); nothing was written.
    Exists { name: String },
    /// The schema of a session's `run` snapshot is not the one this build
    /// reads (AGL-063, D4).
    RunSchema { expected: u64, found: String },
    /// A session's `run` snapshot is not a snapshot (AGL-063, D4).
    RunSnapshot { detail: String },
    /// A Function is unusable; `key` locates the problem (D21, D42, D72,
    /// D78, D82-D93).
    Invalid {
        path: PathBuf,
        key: String,
        problem: FunctionProblem,
    },
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::NotFound { name, paths } => {
                let paths = paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                tr!(lang, FUNCTION_NOT_FOUND, name = name, paths = &paths)
            }
            Self::BuiltIn { name } => tr!(lang, FUNCTION_BUILTIN, name = name),
            Self::Exists { name } => tr!(lang, FUNCTION_EXISTS, name = name),
            Self::RunSchema { expected, found } => {
                tr!(lang, RUN_SCHEMA, expected = expected, found = found)
            }
            Self::RunSnapshot { detail } => tr!(lang, RUN_SNAPSHOT, detail = detail),
            Self::Invalid { path, key, problem } => tr!(
                lang,
                m::PROBLEM_IN_PATH,
                path = path.display(),
                key = key,
                problem = problem_message(problem, lang)
            ),
        }
    }
}

/// What is wrong with a Function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FunctionProblem {
    /// The value names a state that does not exist.
    UnknownState(String),
    /// A transition key is not a tool of the state, with or without an
    /// outcome, nor `else`.
    BadTransitionKey,
    /// No transition for a tool and outcome of the state (D21).
    MissingTransition { tool: String, outcome: String },
    /// The state has neither tools nor `answer` (D87).
    NoActions,
    /// The state has `next` but no tools.
    NextWithoutTools,
    /// `on_limit` without `max_steps` or `max_visits`.
    OnLimitWithoutLimit,
    /// `on_limit` references lead back to this state (D83).
    OnLimitCycle,
    /// No transition leads to the state (D91).
    Unreachable,
    /// `think = true` with a format without thinking (D78).
    NoThinking { format: String },
    /// The pattern is not in the supported subset (D69, D75).
    Pattern(PatternError),
    /// The pattern may produce `end`, which ends values (D72).
    PatternMayEndValue { end: char },
    /// An empty `enum`.
    EmptyEnum,
    /// `min > max`, or `min < 1` for `timeout` (D93).
    BadRange,
    /// A constraint needs exactly one key (D88).
    ConstraintKeys,
    /// A limit below 1.
    NotPositive,
    /// The `model` of a Function of the llama kind has no `weights`
    /// (AGL-033, D1; AGL-039, D19).
    ServerModelMissing,
    /// The `model` of a Function of the strata kind lacks one of
    /// `strata`, `name`, `gguf` (AGL-089, D5).
    ServerStrataKeyMissing(String),
    /// A key of the Function's `model` that does not apply to its kind
    /// (AGL-033, D1; AGL-034, D4).
    ServerKeyOfOtherKind(String),
    /// A weights file is neither a file name nor an absolute path
    /// (AGL-039, D24a).
    BadWeightsName,
    /// A `source` names a file the model does not set (AGL-039, D5).
    SourceWithoutFile,
    /// `source.gguf` does not have one source for each of `gguf`
    /// (AGL-089, D4).
    SourceShardsMismatch,
    /// A `source` url is not https (AGL-050, D8).
    BadSourceUrl,
    /// A `source` sha256 is not 64 lowercase hex digits (AGL-050, D8).
    BadSourceSha256,
    /// A `source` size is not a positive number of bytes (AGL-050, D8).
    BadSourceSize,
}

/// The text of a Function's `problem` in `lang`.
fn problem_message(problem: &FunctionProblem, lang: Lang) -> String {
    use FunctionProblem as P;
    match problem {
        P::UnknownState(name) => tr!(lang, FUNC_UNKNOWN_STATE, name = name),
        P::BadTransitionKey => tr!(lang, FUNC_BAD_TRANSITION_KEY).to_string(),
        P::MissingTransition { tool, outcome } => {
            tr!(
                lang,
                FUNC_MISSING_TRANSITION,
                tool = tool,
                outcome = outcome
            )
        }
        P::NoActions => tr!(lang, FUNC_NO_ACTIONS).to_string(),
        P::NextWithoutTools => tr!(lang, FUNC_NEXT_WITHOUT_TOOLS).to_string(),
        P::OnLimitWithoutLimit => tr!(lang, FUNC_ON_LIMIT_WITHOUT_LIMIT).to_string(),
        P::OnLimitCycle => tr!(lang, FUNC_ON_LIMIT_CYCLE).to_string(),
        P::Unreachable => tr!(lang, FUNC_UNREACHABLE).to_string(),
        P::NoThinking { format } => tr!(lang, FUNC_NO_THINKING, format = format),
        P::Pattern(error) => error.message(lang),
        P::PatternMayEndValue { end } => tr!(lang, FUNC_PATTERN_MAY_END_VALUE, end = end),
        P::EmptyEnum => tr!(lang, FUNC_EMPTY_ENUM).to_string(),
        P::BadRange => tr!(lang, FUNC_BAD_RANGE).to_string(),
        P::ConstraintKeys => tr!(lang, FUNC_CONSTRAINT_KEYS).to_string(),
        P::NotPositive => tr!(lang, FUNC_NOT_POSITIVE).to_string(),
        P::ServerModelMissing => tr!(lang, FUNC_SERVER_MODEL_MISSING).to_string(),
        P::ServerStrataKeyMissing(key) => tr!(lang, FUNC_SERVER_STRATA_KEY_MISSING, key = key),
        P::ServerKeyOfOtherKind(key) => {
            let kind = if crate::function::STRATA_MODEL_KEYS.contains(&key.as_str()) {
                "strata"
            } else {
                "llama"
            };
            tr!(lang, FUNC_SERVER_KEY_OF_OTHER_KIND, key = key, kind = kind)
        }
        P::BadWeightsName => tr!(lang, FUNC_BAD_WEIGHTS_NAME).to_string(),
        P::SourceWithoutFile => tr!(lang, FUNC_SOURCE_WITHOUT_FILE).to_string(),
        P::SourceShardsMismatch => tr!(lang, FUNC_SOURCE_SHARDS_MISMATCH).to_string(),
        P::BadSourceUrl => tr!(lang, FUNC_BAD_SOURCE_URL).to_string(),
        P::BadSourceSha256 => tr!(lang, FUNC_BAD_SOURCE_SHA256).to_string(),
        P::BadSourceSize => tr!(lang, FUNC_BAD_SOURCE_SIZE).to_string(),
    }
}

/// `FunctionProblem::UnknownState`.
const FUNC_UNKNOWN_STATE: Msg = Msg {
    en: "there is no state `{name}`",
    uk: "стану `{name}` немає",
};

/// `FunctionProblem::BadTransitionKey`.
const FUNC_BAD_TRANSITION_KEY: Msg = Msg {
    en: "a transition is `next.<tool>.ok`/`.fail`/`.timeout`/`.any` of a tool of the state, or `next.otherwise`",
    uk: "перехід — це `next.<tool>.ok`/`.fail`/`.timeout`/`.any` для tool цього стану або `next.otherwise`",
};

/// `FunctionProblem::MissingTransition`.
const FUNC_MISSING_TRANSITION: Msg = Msg {
    en: "no transition for `{tool}.{outcome}`; add it, `{tool}` or `else` to `next`",
    uk: "немає переходу для `{tool}.{outcome}`; додай його, `{tool}` або `else` у `next`",
};

/// `FunctionProblem::NoActions`.
const FUNC_NO_ACTIONS: Msg = Msg {
    en: "the state has no tools and no `answer = true`",
    uk: "стан не має ні tools, ні `answer = true`",
};

/// `FunctionProblem::NextWithoutTools`.
const FUNC_NEXT_WITHOUT_TOOLS: Msg = Msg {
    en: "`next` needs tools; an answer ends the turn",
    uk: "`next` потребує tools; відповідь завершує хід",
};

/// `FunctionProblem::OnLimitWithoutLimit`.
const FUNC_ON_LIMIT_WITHOUT_LIMIT: Msg = Msg {
    en: "`on_limit` needs `max_steps` or `max_visits`",
    uk: "`on_limit` потребує `max_steps` або `max_visits`",
};

/// `FunctionProblem::OnLimitCycle`.
const FUNC_ON_LIMIT_CYCLE: Msg = Msg {
    en: "`on_limit` states lead back to this state",
    uk: "стани `on_limit` ведуть назад у цей стан",
};

/// `FunctionProblem::Unreachable`.
const FUNC_UNREACHABLE: Msg = Msg {
    en: "no transition leads to this state",
    uk: "у цей стан не веде жоден перехід",
};

/// `FunctionProblem::NoThinking`.
const FUNC_NO_THINKING: Msg = Msg {
    en: "model format `{format}` has no thinking; remove `think = true`",
    uk: "формат моделі `{format}` не має thinking; прибери `think = true`",
};

/// `FunctionProblem::PatternMayEndValue`.
const FUNC_PATTERN_MAY_END_VALUE: Msg = Msg {
    en: "the pattern may produce {end:?}, which ends a value in this model format",
    uk: "шаблон може дати {end:?}, а це кінець значення у форматі моделі",
};

/// `FunctionProblem::EmptyEnum`.
const FUNC_EMPTY_ENUM: Msg = Msg {
    en: "`enum` needs at least one value",
    uk: "`enum` потребує хоча б одного значення",
};

/// `FunctionProblem::BadRange`.
const FUNC_BAD_RANGE: Msg = Msg {
    en: "needs `min <= max`, and `min >= 1` for `timeout`",
    uk: "потрібно `min <= max`, а для `timeout` ще `min >= 1`",
};

/// `FunctionProblem::ConstraintKeys`.
const FUNC_CONSTRAINT_KEYS: Msg = Msg {
    en: "a constraint has exactly one key",
    uk: "обмеження має рівно один ключ",
};

/// `FunctionProblem::NotPositive`.
const FUNC_NOT_POSITIVE: Msg = Msg {
    en: "must be at least 1",
    uk: "має бути щонайменше 1",
};

/// The `model` of a Function of the llama kind has no `weights`
/// (AGL-033, D1; AGL-039, D19).
const FUNC_SERVER_MODEL_MISSING: Msg = Msg {
    en: "the model of the llama kind needs `weights`",
    uk: "модель llama-виду потребує `weights`",
};

/// The `model` of a Function of the strata kind lacks a key
/// (AGL-089, D5).
const FUNC_SERVER_STRATA_KEY_MISSING: Msg = Msg {
    en: "the model of the strata kind needs `{key}`",
    uk: "модель strata-виду потребує `{key}`",
};

/// `source.gguf` against `gguf` (AGL-089, D4).
const FUNC_SOURCE_SHARDS_MISMATCH: Msg = Msg {
    en: "`source.gguf` needs one source for each file of `gguf`, in their order",
    uk: "`source.gguf` потребує по одному джерелу на кожен файл `gguf`, у тому ж порядку",
};

const FUNC_BAD_WEIGHTS_NAME: Msg = Msg {
    en: "a weights file is a file name (looked for in the weights folders) or an absolute path",
    uk: "файл ваг — це ім'я файлу (шукається в теках ваг) або абсолютний шлях",
};

const FUNC_SOURCE_WITHOUT_FILE: Msg = Msg {
    en: "`source` names a file the model does not set",
    uk: "`source` називає файл, якого модель не задає",
};

/// A key of the Function's `model` that does not apply to its kind
/// (AGL-033, D1; AGL-034, D4).
const FUNC_SERVER_KEY_OF_OTHER_KIND: Msg = Msg {
    en: "{key} belongs to the {kind} kind, not to the model's `kind`",
    uk: "{key} належить до {kind}-виду, не до `kind` цієї моделі",
};

/// `FunctionProblem::BadSourceUrl` (AGL-050, D8).
const FUNC_BAD_SOURCE_URL: Msg = Msg {
    en: "a source is an https url",
    uk: "джерело — це https-адреса",
};

/// `FunctionProblem::BadSourceSha256` (AGL-050, D8).
const FUNC_BAD_SOURCE_SHA256: Msg = Msg {
    en: "a sha256 is 64 lowercase hex digits",
    uk: "sha256 — це 64 шістнадцяткові цифри в нижньому регістрі",
};

/// `FunctionProblem::BadSourceSize` (AGL-050, D8).
const FUNC_BAD_SOURCE_SIZE: Msg = Msg {
    en: "a size is a positive number of bytes",
    uk: "розмір — це додатне число байтів",
};

/// `Error::RunSchema`.
const RUN_SCHEMA: Msg = Msg {
    en: "the session's `run` snapshot is of the schema {found}, this build reads {expected}",
    uk: "снапшот `run` сесії зі схемою {found}, цей збір читає {expected}",
};

/// `Error::RunSnapshot`.
const RUN_SNAPSHOT: Msg = Msg {
    en: "the session's `run` snapshot is unreadable: {detail}",
    uk: "снапшот `run` сесії нечитабельний: {detail}",
};

/// `Error::NotFound`.
const FUNCTION_NOT_FOUND: Msg = Msg {
    en: "Function `{name}` not found: no {paths} and no built-in Function of that name",
    uk: "Function `{name}` не знайдено: немає {paths} і вбудованої Function з такою назвою",
};

/// `Error::BuiltIn`.
const FUNCTION_BUILTIN: Msg = Msg {
    en: "Function `{name}` is built-in: its file belongs to the repository; make your own with `/function new <name> {name}`",
    uk: "Function `{name}` вбудована: її файл належить репозиторію; зробіть власну через `/function new <назва> {name}`",
};

/// `Error::Exists`.
const FUNCTION_EXISTS: Msg = Msg {
    en: "Function `{name}` already exists: nothing was written",
    uk: "Function `{name}` уже є: нічого не записано",
};
