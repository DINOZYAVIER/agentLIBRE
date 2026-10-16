//! Languages of CLI messages (D65-D68).

use std::ffi::OsString;

use crate::error::{Error, Expected, FunctionProblem, TemplateProblem};
use crate::function::Function;
use crate::grammar::pattern::{PatternError, PatternErrorKind};
use crate::session::StopReason;
use crate::theme::StyleError;

/// A language of CLI messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Uk,
}

impl Lang {
    /// Parses a `language` setting value.
    pub fn from_code(code: &str) -> Option<Lang> {
        match code {
            "en" => Some(Lang::En),
            "uk" => Some(Lang::Uk),
            _ => None,
        }
    }

    /// The language of the first non-empty `LC_ALL`, `LC_MESSAGES` or
    /// `LANG`, or English when that language is not supported.
    pub fn from_locale(env: impl Fn(&str) -> Option<OsString>) -> Lang {
        let Some(locale) = ["LC_ALL", "LC_MESSAGES", "LANG"]
            .into_iter()
            .filter_map(&env)
            .find(|value| !value.is_empty())
        else {
            return Lang::En;
        };
        let locale = locale.to_str().unwrap_or_default();
        let code = locale.split(['_', '.', '@']).next().unwrap_or_default();
        Lang::from_code(code).unwrap_or(Lang::En)
    }
}

/// One message: its texts in the supported languages, defined once.
#[derive(Clone, Copy)]
pub struct Msg {
    pub en: &'static str,
    pub uk: &'static str,
}

impl Msg {
    /// The text of this message in `lang`.
    pub fn text(self, lang: Lang) -> &'static str {
        match lang {
            Lang::En => self.en,
            Lang::Uk => self.uk,
        }
    }
}

/// An argument of a `tr!` with placeholders: its name in the template and
/// the value to show.
#[derive(Clone, Copy)]
pub struct Arg<'a> {
    name: &'static str,
    display: &'a dyn std::fmt::Display,
    debug: &'a dyn std::fmt::Debug,
}

impl<'a> Arg<'a> {
    /// Binds the template field `name` to `value`.
    pub fn new<T: std::fmt::Display + std::fmt::Debug>(
        name: &'static str,
        value: &'a T,
    ) -> Arg<'a> {
        Arg {
            name,
            display: value,
            debug: value,
        }
    }
}

/// Renders a catalog template the way `format!` renders its strings:
/// `{name}` shows the argument named `name`, `{name:?}` its debug form,
/// `{{` and `}}` a literal brace. `format!` takes only literal strings,
/// so the catalog templates are rendered here at run time.
pub fn render(template: &str, args: &[Arg<'_>]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(template.len() + 16);
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    out.push('{');
                    continue;
                }
                let mut field = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some('{') => panic!("i18n: nested `{{` in message {template:?}"),
                        Some(c) => field.push(c),
                        None => panic!("i18n: unmatched `{{` in message {template:?}"),
                    }
                }
                let (name, debug) = match field.rsplit_once(':') {
                    Some((name, "?")) => (name, true),
                    Some((_, _)) => {
                        panic!("i18n: unsupported `{{:...}}` in message {template:?}")
                    }
                    None => (field.as_str(), false),
                };
                let arg = args.iter().find(|arg| arg.name == name).unwrap_or_else(|| {
                    panic!("i18n: no argument `{{name}}` for message {template:?}")
                });
                if debug {
                    let _ = write!(out, "{:?}", arg.debug);
                } else {
                    let _ = write!(out, "{}", arg.display);
                }
            }
            '}' => {
                if chars.peek() == Some(&'}') {
                    chars.next();
                    out.push('}');
                } else {
                    panic!("i18n: unmatched `}}` in message {template:?}")
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// The message of the `m` catalog in `lang`. With placeholders, the
/// arguments are `name = value` pairs named by the template's fields.
#[macro_export]
macro_rules! tr {
    ($lang:expr, $msg:expr) => {
        $msg.text($lang)
    };
    ($lang:expr, $msg:expr, $($name:ident = $val:expr),+ $(,)?) => {{
        use $crate::i18n::Arg;
        $crate::i18n::render($msg.text($lang), &[$(Arg::new(stringify!($name), &$val)),*])
    }};
}

/// Every CLI message of the program, defined once per language.
pub mod m {
    use super::Msg;

    /// The `--help` text.
    pub const HELP: Msg = Msg {
        en: "\
Usage: agentLIBRE [-p <message>] [--resume <id>] [--function <name>]
       agentLIBRE server start|stop|status

Without -p, agentLIBRE starts an interactive session.

Options:
  -p <message>        send one message, print the answer and exit
  --resume <id>       continue session <id>
  --function <name>   use Function <name>
  -h, --help          print this help

Server (systemd user unit agentLIBRE-llama-server, [server] in agentLIBRE.toml):
  server start        start llama-server and wait until it is ready
  server stop         stop it
  server status       show whether it runs and is ready",
        uk: "\
Використання: agentLIBRE [-p <повідомлення>] [--resume <id>] [--function <назва>]
              agentLIBRE server start|stop|status

Без -p agentLIBRE запускає інтерактивну сесію.

Параметри:
  -p <повідомлення>   надіслати одне повідомлення, вивести відповідь і вийти
  --resume <id>       продовжити сесію <id>
  --function <назва>  використати Function <назва>
  -h, --help          показати цю довідку

Сервер (user-unit systemd agentLIBRE-llama-server, [server] в agentLIBRE.toml):
  server start        запустити llama-server і дочекатися готовності
  server stop         зупинити його
  server status       показати, чи він працює і чи готовий",
    };

    /// The pointer appended to usage errors.
    pub const USAGE_POINTER: Msg = Msg {
        en: "Run `agentLIBRE --help` for usage.",
        uk: "Див. `agentLIBRE --help`.",
    };

    /// `Error::InvalidUnicode`.
    pub const INVALID_UNICODE: Msg = Msg {
        en: "arguments must be valid UTF-8",
        uk: "аргументи мають бути коректним UTF-8",
    };

    /// `Error::UnknownOption`.
    pub const UNKNOWN_OPTION: Msg = Msg {
        en: "unknown option `{arg}`",
        uk: "невідомий параметр `{arg}`",
    };

    /// `Error::MissingValue`.
    pub const MISSING_VALUE: Msg = Msg {
        en: "option `{arg}` needs a value",
        uk: "параметру `{arg}` потрібне значення",
    };

    /// `Error::RepeatedOption`.
    pub const REPEATED_OPTION: Msg = Msg {
        en: "option `{arg}` is given twice",
        uk: "параметр `{arg}` вказано двічі",
    };

    /// `Error::FunctionWithResume`.
    pub const FUNCTION_WITH_RESUME: Msg = Msg {
        en: "`--function` cannot go with `--resume`: the session keeps its Function",
        uk: "`--function` не можна разом із `--resume`: сесія має свою Function",
    };

    /// `Error::NoHome`.
    pub const NO_HOME: Msg = Msg {
        en: "HOME is not set and {var} is not an absolute path",
        uk: "HOME не задано, а {var} не є абсолютним шляхом",
    };

    /// `Error::CurrentDir`.
    pub const CURRENT_DIR: Msg = Msg {
        en: "cannot read the current directory: {source}",
        uk: "не вдалося визначити поточну теку: {source}",
    };

    /// `Error::Read`.
    pub const READ: Msg = Msg {
        en: "cannot read {path}: {source}",
        uk: "не вдалося прочитати {path}: {source}",
    };

    /// `Error::Parse`.
    pub const PARSE: Msg = Msg {
        en: "invalid TOML in {path}:\n{detail}",
        uk: "некоректний TOML у {path}:\n{detail}",
    };

    /// `Error::UnknownKey`.
    pub const UNKNOWN_KEY: Msg = Msg {
        en: "unknown key `{key}` in {path}; allowed: {allowed}",
        uk: "невідомий ключ `{key}` у {path}; дозволені: {allowed}",
    };

    /// `Error::MissingKey`.
    pub const MISSING_KEY: Msg = Msg {
        en: "`{key}` is missing in {path}",
        uk: "у {path} немає `{key}`",
    };

    /// `Error::WrongType`.
    pub const WRONG_TYPE: Msg = Msg {
        en: "`{key}` in {path} must be {expected}",
        uk: "`{key}` у {path} має бути {expected}",
    };

    /// What a value must be, for `WRONG_TYPE`.
    pub const EXPECTED_STRING: Msg = Msg {
        en: "a string",
        uk: "рядком",
    };

    /// What a value must be, for `WRONG_TYPE`.
    pub const EXPECTED_TABLE: Msg = Msg {
        en: "a table",
        uk: "таблицею",
    };

    /// What a value must be, for `WRONG_TYPE`.
    pub const EXPECTED_BOOL: Msg = Msg {
        en: "true or false",
        uk: "true або false",
    };

    /// What a value must be, for `WRONG_TYPE`.
    pub const EXPECTED_INTEGER: Msg = Msg {
        en: "an integer",
        uk: "цілим числом",
    };

    /// What a value must be, for `WRONG_TYPE`.
    pub const EXPECTED_NUMBER: Msg = Msg {
        en: "a number",
        uk: "числом",
    };

    /// What a value must be, for `WRONG_TYPE`.
    pub const EXPECTED_STRINGS: Msg = Msg {
        en: "an array of strings",
        uk: "масивом рядків",
    };

    /// `Error::InvalidStyle`.
    pub const INVALID_STYLE: Msg = Msg {
        en: "invalid style `{value}` for `{key}` in {path}: {problem}",
        uk: "некоректний стиль `{value}` для `{key}` у {path}: {problem}",
    };

    /// What is wrong with a style, for `INVALID_STYLE`.
    pub const STYLE_EMPTY: Msg = Msg {
        en: "a style cannot be empty",
        uk: "стиль не може бути порожнім",
    };

    /// What is wrong with a style, for `INVALID_STYLE`.
    pub const STYLE_COLOR: Msg = Msg {
        en: "`{color}` is not #RRGGBB or oklch(L C H)",
        uk: "`{color}` не є #RRGGBB або oklch(L C H)",
    };

    /// What is wrong with a style, for `INVALID_STYLE`.
    pub const STYLE_OKLCH: Msg = Msg {
        en: "oklch needs three numbers, lightness 0..1 and chroma >= 0",
        uk: "oklch потребує трьох чисел, яскравість 0..1, насиченість >= 0",
    };

    /// `Error::NotPositive`.
    pub const NOT_POSITIVE: Msg = Msg {
        en: "`{key}` in {path} must be at least 1",
        uk: "`{key}` у {path} має бути не менше 1",
    };

    /// `Error::UnsupportedLanguage`.
    pub const UNSUPPORTED_LANGUAGE: Msg = Msg {
        en: "unsupported language `{value}` in {path}; supported: en, uk",
        uk: "непідтримувана мова `{value}` у {path}; підтримуються: en, uk",
    };

    /// `Error::UnsupportedUrl`.
    pub const UNSUPPORTED_URL: Msg = Msg {
        en: "unsupported server address `{url}` in {path}; only http:// is supported",
        uk: "непідтримувана адреса сервера `{url}` у {path}; підтримується лише http://",
    };

    /// `Error::ServerInProject`.
    pub const SERVER_IN_PROJECT: Msg = Msg {
        en: "{path}: a project file cannot set [server]; the server address belongs in {user_path}",
        uk: "{path}: файл проєкту не може задавати [server]; адреса сервера задається в {user_path}",
    };

    /// `Error::MissingServerUrl`.
    pub const MISSING_SERVER_URL: Msg = Msg {
        en: "the llama-server address is not set; add to {user_path}:\n\n[server]\nurl = \"http://HOST:PORT\"",
        uk: "адресу llama-server не задано; додай у {user_path}:\n\n[server]\nurl = \"http://HOST:PORT\"",
    };

    /// `Error::InvalidName`.
    pub const INVALID_NAME: Msg = Msg {
        en: "invalid name `{name}`: a name is not empty, has no `/` and does not start with `.`",
        uk: "некоректна назва `{name}`: назва непорожня, без `/` і не починається з `.`",
    };

    /// `Error::FormatNotFound`.
    pub const FORMAT_NOT_FOUND: Msg = Msg {
        en: "model format `{name}` not found: no {path} and no built-in format of that name",
        uk: "формат моделі `{name}` не знайдено: немає {path} і вбудованого формату з такою назвою",
    };

    /// A template or Function problem at `key` of `path` (`Error::Template`,
    /// `Error::Function`).
    pub const PROBLEM_IN_PATH: Msg = Msg {
        en: "`{key}` in {path}: {problem}",
        uk: "`{key}` у {path}: {problem}",
    };

    /// `Error::FunctionNotFound`.
    pub const FUNCTION_NOT_FOUND: Msg = Msg {
        en: "Function `{name}` not found: no {path} and no built-in Function of that name",
        uk: "Function `{name}` не знайдено: немає {path} і вбудованої Function з такою назвою",
    };

    /// `Error::ServerUnreachable`.
    pub const SERVER_UNREACHABLE: Msg = Msg {
        en: "cannot reach llama-server at {url}: {detail}\nTo start it: agentLIBRE server start",
        uk: "не вдалося з'єднатися з llama-server за адресою {url}: {detail}\nЩоб запустити: agentLIBRE server start",
    };

    /// `Error::ServerStatus`.
    pub const SERVER_STATUS: Msg = Msg {
        en: "llama-server at {url} answered {status}: {message}",
        uk: "llama-server за адресою {url} відповів {status}: {message}",
    };

    /// `Error::ServerReply`.
    pub const SERVER_REPLY: Msg = Msg {
        en: "unexpected reply from llama-server at {url}: {detail}",
        uk: "неочікувана відповідь llama-server за адресою {url}: {detail}",
    };

    /// `Error::SessionNotFound`.
    pub const SESSION_NOT_FOUND: Msg = Msg {
        en: "no session `{id}` in {dir}",
        uk: "сесії `{id}` немає в {dir}",
    };

    /// `Error::SessionAmbiguous`.
    pub const SESSION_AMBIGUOUS: Msg = Msg {
        en: "{count} sessions start with `{id}`; give more of the id",
        uk: "сесій, що починаються з `{id}`: {count}; вкажи довшу частину id",
    };

    /// `Error::SessionLine`.
    pub const SESSION_LINE: Msg = Msg {
        en: "line {line} of {path}: {detail}",
        uk: "рядок {line} у {path}: {detail}",
    };

    /// `Error::SessionWrite`.
    pub const SESSION_WRITE: Msg = Msg {
        en: "cannot write {path}: {source}",
        uk: "не вдалося записати {path}: {source}",
    };

    /// `Error::SessionState`.
    pub const SESSION_STATE: Msg = Msg {
        en: "the session continues in state `{state}`, which Function `{function}` no longer has",
        uk: "сесія продовжується в стані `{state}`, якого Function `{function}` вже не має",
    };

    /// `Error::Terminal`.
    pub const TERMINAL: Msg = Msg {
        en: "cannot switch the terminal to raw mode: {source}",
        uk: "не вдалося перемкнути термінал у raw-режим: {source}",
    };

    /// `Error::Interrupted`; a turn stopped by Ctrl-C (`StopReason::Interrupted`).
    pub const INTERRUPTED: Msg = Msg {
        en: "interrupted",
        uk: "перервано",
    };

    /// `Error::OutsideGrammar`.
    pub const OUTSIDE_GRAMMAR: Msg = Msg {
        en: "llama-server generated text outside the grammar: {text:?}",
        uk: "llama-server згенерував текст поза граматикою: {text:?}",
    };

    /// `Error::MissingModel`.
    pub const MISSING_MODEL: Msg = Msg {
        en: "the model is not set; add the name of a file in {models} to {user_path}:\n\n[server]\nmodel = \"NAME.gguf\"",
        uk: "модель не задано; додай назву файлу з {models} у {user_path}:\n\n[server]\nmodel = \"NAME.gguf\"",
    };

    /// `Error::ModelNotFound`.
    pub const MODEL_NOT_FOUND: Msg = Msg {
        en: "no model file {path}",
        uk: "немає файлу моделі {path}",
    };

    /// `Error::NotLocalServer`.
    pub const NOT_LOCAL_SERVER: Msg = Msg {
        en: "agentLIBRE server start runs llama-server only on 127.0.0.1 or localhost, not at {url}",
        uk: "agentLIBRE server start запускає llama-server лише на 127.0.0.1 або localhost, не за {url}",
    };

    /// `Error::LlamaServerNotFound`.
    pub const LLAMA_SERVER_NOT_FOUND: Msg = Msg {
        en: "llama-server is not in PATH",
        uk: "llama-server немає в PATH",
    };

    /// `Error::Systemd`.
    pub const SYSTEMD: Msg = Msg {
        en: "systemd failed: {detail}",
        uk: "помилка systemd: {detail}",
    };

    /// `Error::ServerFailed`.
    pub const SERVER_FAILED: Msg = Msg {
        en: "llama-server stopped before it was ready; see journalctl --user -u {unit}",
        uk: "llama-server зупинився, не встигнувши стати готовим; див. journalctl --user -u {unit}",
    };

    /// `Error::ServerTimeout`.
    pub const SERVER_TIMEOUT: Msg = Msg {
        en: "llama-server was not ready after {seconds} s; it keeps loading in the background (agentLIBRE server status, journalctl --user -u {unit})",
        uk: "llama-server не став готовим за {seconds} с; він вантажиться далі у фоні (agentLIBRE server status, journalctl --user -u {unit})",
    };

    /// `TemplateProblem::Missing`.
    pub const TEMPLATE_MISSING: Msg = Msg {
        en: "needs {p}",
        uk: "потрібен {p}",
    };

    /// `TemplateProblem::Repeated`.
    pub const TEMPLATE_REPEATED: Msg = Msg {
        en: "{p} appears more than once",
        uk: "{p} трапляється більше одного разу",
    };

    /// `TemplateProblem::Unexpected`.
    pub const TEMPLATE_UNEXPECTED: Msg = Msg {
        en: "{p} does not belong here",
        uk: "{p} тут не дозволений",
    };

    /// `TemplateProblem::ValueBeforeName`.
    pub const TEMPLATE_VALUE_BEFORE_NAME: Msg = Msg {
        en: "{value} comes before {name}",
        uk: "{value} стоїть перед {name}",
    };

    /// `TemplateProblem::NothingBeforeName`.
    pub const TEMPLATE_NOTHING_BEFORE_NAME: Msg = Msg {
        en: "needs text before {name}, or answers cannot be told apart from calls",
        uk: "потрібен текст перед {name}, інакше відповідь не відрізнити від виклику",
    };

    /// `TemplateProblem::NothingAfterValue`.
    pub const TEMPLATE_NOTHING_AFTER_VALUE: Msg = Msg {
        en: "needs text after {value}, or a value has no end",
        uk: "потрібен текст після {value}, інакше значення не має кінця",
    };

    /// `TemplateProblem::Empty`.
    pub const TEMPLATE_EMPTY: Msg = Msg {
        en: "must not be empty",
        uk: "не може бути порожнім",
    };

    /// `FunctionProblem::UnknownState`.
    pub const FUNC_UNKNOWN_STATE: Msg = Msg {
        en: "there is no state `{name}`",
        uk: "стану `{name}` немає",
    };

    /// `FunctionProblem::BadTransitionKey`.
    pub const FUNC_BAD_TRANSITION_KEY: Msg = Msg {
        en: "a transition key is a tool of the state, `tool.ok`/`.fail`/`.timeout`, or `else`",
        uk: "ключ переходу — tool цього стану, `tool.ok`/`.fail`/`.timeout` або `else`",
    };

    /// `FunctionProblem::MissingTransition`.
    pub const FUNC_MISSING_TRANSITION: Msg = Msg {
        en: "no transition for `{tool}.{outcome}`; add it, `{tool}` or `else` to `next`",
        uk: "немає переходу для `{tool}.{outcome}`; додай його, `{tool}` або `else` у `next`",
    };

    /// `FunctionProblem::NoActions`.
    pub const FUNC_NO_ACTIONS: Msg = Msg {
        en: "the state has no tools and no `answer = true`",
        uk: "стан не має ні tools, ні `answer = true`",
    };

    /// `FunctionProblem::NextWithoutTools`.
    pub const FUNC_NEXT_WITHOUT_TOOLS: Msg = Msg {
        en: "`next` needs tools; an answer ends the turn",
        uk: "`next` потребує tools; відповідь завершує хід",
    };

    /// `FunctionProblem::OnLimitWithoutLimit`.
    pub const FUNC_ON_LIMIT_WITHOUT_LIMIT: Msg = Msg {
        en: "`on_limit` needs `max_steps` or `max_visits`",
        uk: "`on_limit` потребує `max_steps` або `max_visits`",
    };

    /// `FunctionProblem::OnLimitCycle`.
    pub const FUNC_ON_LIMIT_CYCLE: Msg = Msg {
        en: "`on_limit` states lead back to this state",
        uk: "стани `on_limit` ведуть назад у цей стан",
    };

    /// `FunctionProblem::Unreachable`.
    pub const FUNC_UNREACHABLE: Msg = Msg {
        en: "no transition leads to this state",
        uk: "у цей стан не веде жоден перехід",
    };

    /// `FunctionProblem::NoThinking`.
    pub const FUNC_NO_THINKING: Msg = Msg {
        en: "model format `{format}` has no thinking; remove `think = true`",
        uk: "формат моделі `{format}` не має thinking; прибери `think = true`",
    };

    /// `FunctionProblem::PatternMayEndValue`.
    pub const FUNC_PATTERN_MAY_END_VALUE: Msg = Msg {
        en: "the pattern may produce {end:?}, which ends a value in this model format",
        uk: "шаблон може дати {end:?}, а це кінець значення у форматі моделі",
    };

    /// `FunctionProblem::EmptyEnum`.
    pub const FUNC_EMPTY_ENUM: Msg = Msg {
        en: "`enum` needs at least one value",
        uk: "`enum` потребує хоча б одного значення",
    };

    /// `FunctionProblem::BadRange`.
    pub const FUNC_BAD_RANGE: Msg = Msg {
        en: "needs `min <= max`, and `min >= 1` for `timeout`",
        uk: "потрібно `min <= max`, а для `timeout` ще `min >= 1`",
    };

    /// `FunctionProblem::ConstraintKeys`.
    pub const FUNC_CONSTRAINT_KEYS: Msg = Msg {
        en: "a constraint has exactly one key",
        uk: "обмеження має рівно один ключ",
    };

    /// `FunctionProblem::NotPositive`.
    pub const FUNC_NOT_POSITIVE: Msg = Msg {
        en: "must be at least 1",
        uk: "має бути щонайменше 1",
    };

    /// `PatternErrorKind::Anchor`.
    pub const PATTERN_ANCHOR: Msg = Msg {
        en: "pattern byte {at}: remove `^`/`$`; a pattern covers the whole value",
        uk: "шаблон, байт {at}: прибери `^`/`$`; шаблон і так охоплює все значення",
    };

    /// `PatternErrorKind::GroupSyntax`.
    pub const PATTERN_GROUP_SYNTAX: Msg = Msg {
        en: "pattern byte {at}: `(?` groups are not supported",
        uk: "шаблон, байт {at}: групи `(?` не підтримуються",
    };

    /// `PatternErrorKind::Escape`.
    pub const PATTERN_ESCAPE: Msg = Msg {
        en: "pattern byte {at}: `\\{c}` is not supported",
        uk: "шаблон, байт {at}: `\\{c}` не підтримується",
    };

    /// `PatternErrorKind::TrailingBackslash`.
    pub const PATTERN_TRAILING_BACKSLASH: Msg = Msg {
        en: "pattern byte {at}: `\\` at the end",
        uk: "шаблон, байт {at}: `\\` у кінці",
    };

    /// `PatternErrorKind::NothingToRepeat`.
    pub const PATTERN_NOTHING_TO_REPEAT: Msg = Msg {
        en: "pattern byte {at}: nothing to repeat",
        uk: "шаблон, байт {at}: нема чого повторювати",
    };

    /// `PatternErrorKind::BadRepetition`.
    pub const PATTERN_BAD_REPETITION: Msg = Msg {
        en: "pattern byte {at}: use `{{n}}`, `{{n,}}` or `{{n,m}}` with n <= m",
        uk: "шаблон, байт {at}: потрібно `{{n}}`, `{{n,}}` або `{{n,m}}` з n <= m",
    };

    /// `PatternErrorKind::UnbalancedGroup`.
    pub const PATTERN_UNBALANCED_GROUP: Msg = Msg {
        en: "pattern byte {at}: unbalanced parenthesis",
        uk: "шаблон, байт {at}: незакрита або зайва дужка",
    };

    /// `PatternErrorKind::BadClass`.
    pub const PATTERN_BAD_CLASS: Msg = Msg {
        en: "pattern byte {at}: bad class; escape `[` and `]` inside it",
        uk: "шаблон, байт {at}: некоректний клас; екрануй `[` і `]` усередині",
    };

    /// `PatternErrorKind::BadRange`.
    pub const PATTERN_BAD_RANGE: Msg = Msg {
        en: "pattern byte {at}: bad class range",
        uk: "шаблон, байт {at}: некоректний діапазон у класі",
    };

    /// The label of streamed thinking (D109).
    pub const THINKING: Msg = Msg {
        en: "thinking",
        uk: "міркування",
    };

    /// One line of output (`lines`).
    pub const LINE_ONE: Msg = Msg {
        en: "1 line",
        uk: "1 рядок",
    };

    /// `n` lines of output, `n != 1` (`lines`).
    pub const LINES: Msg = Msg {
        en: "{n} lines",
        uk: "{n} рядків",
    };

    /// The Ukrainian word for a line, `n % 10 == 1` (`lines`).
    pub const LINES_ONE: Msg = Msg {
        en: "рядок",
        uk: "рядок",
    };

    /// The Ukrainian word for a line, `n % 10 in 2..=4` (`lines`).
    pub const LINES_FEW: Msg = Msg {
        en: "рядки",
        uk: "рядки",
    };

    /// The Ukrainian word for a line, otherwise (`lines`).
    pub const LINES_MANY: Msg = Msg {
        en: "рядків",
        uk: "рядків",
    };

    /// `n` lines with the chosen word (`lines`).
    pub const LINES_NOUN: Msg = Msg {
        en: "{n} {word}",
        uk: "{n} {word}",
    };

    /// The session line at the start of a run (D109).
    pub const SESSION: Msg = Msg {
        en: "session {id}",
        uk: "сесія {id}",
    };

    /// `StopReason::StepLimit`.
    pub const STOP_STEP_LIMIT: Msg = Msg {
        en: "stopped: the turn took max_steps_total ({steps}) steps",
        uk: "зупинено: хід зробив max_steps_total ({steps}) кроків",
    };

    /// `StopReason::ContextFull`.
    pub const STOP_CONTEXT_FULL: Msg = Msg {
        en: "stopped: the conversation no longer fits the model context; start a new session",
        uk: "зупинено: розмова вже не вміщується в контекст моделі; почни нову сесію",
    };

    /// `StopReason::CutOff`.
    pub const STOP_CUT_OFF: Msg = Msg {
        en: "stopped: the action hit max_action_tokens ({tokens})",
        uk: "зупинено: дія дійшла до max_action_tokens ({tokens})",
    };

    /// `StopReason::ServerError`.
    pub const STOP_SERVER: Msg = Msg {
        en: "stopped: llama-server failed",
        uk: "зупинено: помилка llama-server",
    };

    /// The second line of `stopped`: how to go on (D113).
    pub const STOPPED_RESUME: Msg = Msg {
        en: "to continue: agentLIBRE --resume {id} -p \"...\"",
        uk: "щоб продовжити: agentLIBRE --resume {id} -p \"...\"",
    };

    /// The hint of an empty prompt.
    pub const HINT: Msg = Msg {
        en: "Type a message or / for commands",
        uk: "Введіть повідомлення або / для команд",
    };

    /// The dock's state.
    pub const READY: Msg = Msg {
        en: "Ready",
        uk: "Готово",
    };

    /// The dock's state.
    pub const GENERATING: Msg = Msg {
        en: "Generating",
        uk: "Генерація",
    };

    /// The dock's state.
    pub const RUNNING: Msg = Msg {
        en: "Running {tool}",
        uk: "Виконується {tool}",
    };

    /// The dock's state.
    pub const COMPACTING: Msg = Msg {
        en: "Compacting",
        uk: "Стиснення",
    };

    /// Waiting messages in the dock.
    pub const QUEUED: Msg = Msg {
        en: "queued {n}",
        uk: "у черзі {n}",
    };

    /// The description of a `/` command (D118).
    pub const CMD_QUIT: Msg = Msg {
        en: "close agentLIBRE",
        uk: "закрити agentLIBRE",
    };

    /// The description of a `/` command (D118).
    pub const CMD_EXIT: Msg = Msg {
        en: "same as /quit",
        uk: "те саме, що /quit",
    };

    /// The description of a `/` command (D118).
    pub const CMD_HELP: Msg = Msg {
        en: "show commands and keys",
        uk: "показати команди й клавіші",
    };

    /// The description of a `/` command (D118).
    pub const CMD_STATUS: Msg = Msg {
        en: "show the session",
        uk: "показати сесію",
    };

    /// The description of a `/` command (D118).
    pub const CMD_NEW: Msg = Msg {
        en: "start a new session",
        uk: "почати нову сесію",
    };

    /// The description of a `/` command (D118).
    pub const CMD_COMPACT: Msg = Msg {
        en: "replace the history with a summary",
        uk: "замінити історію резюме",
    };

    /// The keys of the interactive session (D120).
    pub const KEYS: Msg = Msg {
        en: "Enter sends; Shift-Enter or Alt-Enter adds a line; Up/Down walk the history; \
             Ctrl-C stops the turn (or clears the line, or quits when it is empty); \
             Ctrl-D quits",
        uk: "Enter надсилає; Shift-Enter або Alt-Enter додає рядок; Вгору/Вниз — історія; \
             Ctrl-C зупиняє хід (або очищає рядок, або виходить, якщо він порожній); \
             Ctrl-D виходить",
    };

    /// A `/` command that is not known.
    pub const UNKNOWN_COMMAND: Msg = Msg {
        en: "unknown command {name}; try /help",
        uk: "невідома команда {name}; спробуй /help",
    };

    /// A `/` command during a turn (D120).
    pub const BUSY: Msg = Msg {
        en: "{name} is not available during a turn; stop it with Ctrl-C first",
        uk: "{name} недоступна під час ходу; спершу зупини його Ctrl-C",
    };

    /// A label of one line of `/status`.
    pub const LABEL_SESSION: Msg = Msg {
        en: "session",
        uk: "сесія",
    };

    /// A label of one line of `/status`.
    pub const LABEL_FUNCTION: Msg = Msg {
        en: "Function",
        uk: "Function",
    };

    /// A label of one line of `/status`.
    pub const LABEL_WORKSPACE: Msg = Msg {
        en: "workspace",
        uk: "робоча тека",
    };

    /// A label of one line of `/status`.
    pub const LABEL_SERVER: Msg = Msg {
        en: "server",
        uk: "сервер",
    };

    /// A label of one line of `/status`.
    pub const LABEL_CONTEXT: Msg = Msg {
        en: "context",
        uk: "контекст",
    };

    /// A label of one line of `/status`.
    pub const LABEL_STATE: Msg = Msg {
        en: "state",
        uk: "стан",
    };

    /// One line of `/status`.
    pub const STATUS_LINE: Msg = Msg {
        en: "{label}: {value}",
        uk: "{label}: {value}",
    };

    /// The context usage of `/status`.
    pub const TOKENS: Msg = Msg {
        en: "{n} tokens",
        uk: "{n} токенів",
    };

    /// The state line of `/status` when the turn is idle.
    pub const IDLE: Msg = Msg {
        en: "waiting for a message",
        uk: "чекає повідомлення",
    };

    /// The state line of `/status` after an interrupted turn.
    pub const UNFINISHED: Msg = Msg {
        en: "an interrupted turn in state {state}; the next message continues it",
        uk: "обірваний хід у стані {state}; наступне повідомлення продовжить його",
    };

    /// The note under a cut-short card (D138).
    pub const LIMITED: Msg = Msg {
        en: "… limited to {lines} lines / {chars} chars",
        uk: "… обмежено {lines} рядками / {chars} символами",
    };

    /// The time a turn took.
    pub const WORKED_FOR: Msg = Msg {
        en: "Worked for {time}",
        uk: "Працював {time}",
    };

    /// Ctrl-C on an empty line (D120).
    pub const INTERRUPTED_CLEARED: Msg = Msg {
        en: "interrupted; queued input cleared",
        uk: "перервано; чергу введення очищено",
    };

    /// A failed turn clears the queue (D127).
    pub const ERROR_CLEARED: Msg = Msg {
        en: "queued input cleared",
        uk: "чергу введення очищено",
    };

    /// `/compact` with nothing to summarize (D140).
    pub const NOTHING_TO_COMPACT: Msg = Msg {
        en: "Nothing to compact.",
        uk: "Нічого стискати.",
    };

    /// `/compact` could not make a summary (D140).
    pub const COMPACTION_FAILED: Msg = Msg {
        en: "cannot compact: the history and max_compact_tokens ({limit}) do not fit the \
             model context, or the summary was cut off or empty",
        uk: "не вдалося стиснути: історія й max_compact_tokens ({limit}) не вміщуються в \
             контекст моделі, або резюме обрізане чи порожнє",
    };

    /// A new session started (D126).
    pub const NEW_SESSION: Msg = Msg {
        en: "new session {id}",
        uk: "нова сесія {id}",
    };

    /// The label of a tool call in the transcript.
    pub const TOOL_LABEL: Msg = Msg {
        en: "TOOL",
        uk: "TOOL",
    };

    /// `agentLIBRE server start` begins (D146).
    pub const SERVER_STARTING: Msg = Msg {
        en: "starting llama-server ({model}) at {url} as unit {unit}…",
        uk: "запускаю llama-server ({model}) за {url} як unit {unit}…",
    };

    /// `agentLIBRE server start` is ready (D149).
    pub const SERVER_READY: Msg = Msg {
        en: "llama-server is ready at {url}",
        uk: "llama-server готовий за {url}",
    };

    /// `agentLIBRE server start` finds the server up (D146).
    pub const SERVER_ALREADY_RUNNING: Msg = Msg {
        en: "llama-server is already running at {url}",
        uk: "llama-server уже працює за {url}",
    };

    /// `agentLIBRE server stop` stopped it (D146).
    pub const SERVER_STOPPED: Msg = Msg {
        en: "llama-server stopped",
        uk: "llama-server зупинено",
    };

    /// `agentLIBRE server stop` finds it down (D146).
    pub const SERVER_NOT_RUNNING: Msg = Msg {
        en: "unit {unit} is not running",
        uk: "unit {unit} не працює",
    };

    /// `agentLIBRE server status`: the unit is active.
    pub const SERVER_ACTIVE: Msg = Msg {
        en: "active",
        uk: "активний",
    };

    /// `agentLIBRE server status`: the unit is not active.
    pub const SERVER_INACTIVE: Msg = Msg {
        en: "inactive",
        uk: "неактивний",
    };

    /// `agentLIBRE server status`: `[server] model` is not set.
    pub const SERVER_MODEL_NOT_SET: Msg = Msg {
        en: "not set",
        uk: "не задано",
    };

    /// `agentLIBRE server status`: `/health` is ready.
    pub const SERVER_HEALTH_READY: Msg = Msg {
        en: "ready",
        uk: "готовий",
    };

    /// `agentLIBRE server status`: `/health` answers while loading.
    pub const SERVER_HEALTH_LOADING: Msg = Msg {
        en: "loading",
        uk: "завантажується",
    };

    /// `agentLIBRE server status`: `/health` does not answer.
    pub const SERVER_HEALTH_DOWN: Msg = Msg {
        en: "not responding",
        uk: "не відповідає",
    };

    /// `agentLIBRE server status` (D150).
    pub const SERVER_STATUS_BLOCK: Msg = Msg {
        en: "unit: {unit} ({active})\nurl: {url}\nmodel: {model}\nhealth: {health}",
        uk: "unit: {unit} ({active})\nадреса: {url}\nмодель: {model}\nстан: {health}",
    };
}

impl Error {
    /// The message for this error in `lang`. Usage errors end with a
    /// pointer to `--help`.
    pub fn message(&self, lang: Lang) -> String {
        let message = self.text(lang);
        if !self.is_usage() {
            return message;
        }
        format!("{message}\n{}", tr!(lang, m::USAGE_POINTER))
    }

    fn text(&self, lang: Lang) -> String {
        match self {
            Error::InvalidUnicode => tr!(lang, m::INVALID_UNICODE).to_string(),
            Error::UnknownOption(arg) => tr!(lang, m::UNKNOWN_OPTION, arg = arg),
            Error::MissingValue(arg) => tr!(lang, m::MISSING_VALUE, arg = arg),
            Error::RepeatedOption(arg) => tr!(lang, m::REPEATED_OPTION, arg = arg),
            Error::FunctionWithResume => tr!(lang, m::FUNCTION_WITH_RESUME).to_string(),
            Error::NoHome { var } => tr!(lang, m::NO_HOME, var = var),
            Error::CurrentDir(source) => tr!(lang, m::CURRENT_DIR, source = source),
            Error::Read { path, source } => {
                tr!(lang, m::READ, path = path.display(), source = source)
            }
            Error::Parse { path, detail } => {
                tr!(lang, m::PARSE, path = path.display(), detail = detail)
            }
            Error::UnknownKey { path, key, allowed } => {
                tr!(
                    lang,
                    m::UNKNOWN_KEY,
                    path = path.display(),
                    key = key,
                    allowed = allowed.join(", ")
                )
            }
            Error::MissingKey { path, key } => {
                tr!(lang, m::MISSING_KEY, path = path.display(), key = key)
            }
            Error::WrongType {
                path,
                key,
                expected,
            } => {
                tr!(
                    lang,
                    m::WRONG_TYPE,
                    path = path.display(),
                    key = key,
                    expected = match expected {
                        Expected::String => tr!(lang, m::EXPECTED_STRING),
                        Expected::Table => tr!(lang, m::EXPECTED_TABLE),
                        Expected::Bool => tr!(lang, m::EXPECTED_BOOL),
                        Expected::Integer => tr!(lang, m::EXPECTED_INTEGER),
                        Expected::Number => tr!(lang, m::EXPECTED_NUMBER),
                        Expected::Strings => tr!(lang, m::EXPECTED_STRINGS),
                    }
                )
            }
            Error::InvalidStyle {
                path,
                key,
                value,
                problem,
            } => {
                tr!(
                    lang,
                    m::INVALID_STYLE,
                    path = path.display(),
                    key = key,
                    value = value,
                    problem = match problem {
                        StyleError::Empty => tr!(lang, m::STYLE_EMPTY).to_string(),
                        StyleError::Color(color) => tr!(lang, m::STYLE_COLOR, color = color),
                        StyleError::Oklch => tr!(lang, m::STYLE_OKLCH).to_string(),
                    }
                )
            }
            Error::NotPositive { path, key } => {
                tr!(lang, m::NOT_POSITIVE, path = path.display(), key = key)
            }
            Error::UnsupportedLanguage { path, value } => {
                tr!(
                    lang,
                    m::UNSUPPORTED_LANGUAGE,
                    path = path.display(),
                    value = value
                )
            }
            Error::UnsupportedUrl { path, url } => {
                tr!(lang, m::UNSUPPORTED_URL, path = path.display(), url = url)
            }
            Error::ServerInProject { path, user_path } => {
                tr!(
                    lang,
                    m::SERVER_IN_PROJECT,
                    path = path.display(),
                    user_path = user_path.display()
                )
            }
            Error::MissingServerUrl { user_path } => {
                tr!(lang, m::MISSING_SERVER_URL, user_path = user_path.display())
            }
            Error::InvalidName(name) => tr!(lang, m::INVALID_NAME, name = name),
            Error::FormatNotFound { name, path } => {
                tr!(
                    lang,
                    m::FORMAT_NOT_FOUND,
                    name = name,
                    path = path.display()
                )
            }
            Error::Template { path, key, problem } => {
                tr!(
                    lang,
                    m::PROBLEM_IN_PATH,
                    path = path.display(),
                    key = key,
                    problem = template_problem(problem, lang)
                )
            }
            Error::FunctionNotFound { name, path } => {
                tr!(
                    lang,
                    m::FUNCTION_NOT_FOUND,
                    name = name,
                    path = path.display()
                )
            }
            Error::Function { path, key, problem } => {
                tr!(
                    lang,
                    m::PROBLEM_IN_PATH,
                    path = path.display(),
                    key = key,
                    problem = function_problem(problem, lang)
                )
            }
            Error::ServerUnreachable { url, detail } => {
                tr!(lang, m::SERVER_UNREACHABLE, url = url, detail = detail)
            }
            Error::MissingModel { user_path, models } => {
                tr!(
                    lang,
                    m::MISSING_MODEL,
                    user_path = user_path.display(),
                    models = models.display()
                )
            }
            Error::ModelNotFound { path } => {
                tr!(lang, m::MODEL_NOT_FOUND, path = path.display())
            }
            Error::NotLocalServer { url } => tr!(lang, m::NOT_LOCAL_SERVER, url = url),
            Error::LlamaServerNotFound => tr!(lang, m::LLAMA_SERVER_NOT_FOUND).to_string(),
            Error::Systemd { detail } => tr!(lang, m::SYSTEMD, detail = detail),
            Error::ServerFailed => tr!(lang, m::SERVER_FAILED, unit = crate::cli::serve::UNIT),
            Error::ServerTimeout { seconds } => {
                tr!(
                    lang,
                    m::SERVER_TIMEOUT,
                    seconds = seconds,
                    unit = crate::cli::serve::UNIT
                )
            }
            Error::ServerStatus {
                url,
                status,
                message,
            } => {
                tr!(
                    lang,
                    m::SERVER_STATUS,
                    url = url,
                    status = status,
                    message = message
                )
            }
            Error::ServerReply { url, detail } => {
                tr!(lang, m::SERVER_REPLY, url = url, detail = detail)
            }
            Error::SessionNotFound { id, dir } => {
                tr!(lang, m::SESSION_NOT_FOUND, id = id, dir = dir.display())
            }
            Error::SessionAmbiguous { id, count } => {
                tr!(lang, m::SESSION_AMBIGUOUS, id = id, count = count)
            }
            Error::SessionLine { path, line, detail } => {
                tr!(
                    lang,
                    m::SESSION_LINE,
                    path = path.display(),
                    line = line,
                    detail = detail
                )
            }
            Error::SessionWrite { path, source } => {
                tr!(
                    lang,
                    m::SESSION_WRITE,
                    path = path.display(),
                    source = source
                )
            }
            Error::SessionState { state, function } => {
                tr!(lang, m::SESSION_STATE, state = state, function = function)
            }
            Error::Terminal { source } => tr!(lang, m::TERMINAL, source = source),
            Error::Interrupted => tr!(lang, m::INTERRUPTED).to_string(),
            Error::OutsideGrammar(text) => tr!(lang, m::OUTSIDE_GRAMMAR, text = text),
        }
    }
}

/// `n` lines of output.
pub fn lines(n: usize, lang: Lang) -> String {
    match lang {
        Lang::En if n == 1 => tr!(lang, m::LINE_ONE).to_string(),
        Lang::En => tr!(lang, m::LINES, n = n),
        Lang::Uk => {
            let word = match (n % 10, n % 100) {
                (1, 11) | (2..=4, 12..=14) | (0 | 5..=9, _) => tr!(lang, m::LINES_MANY),
                (1, _) => tr!(lang, m::LINES_ONE),
                (2..=4, _) => tr!(lang, m::LINES_FEW),
                _ => tr!(lang, m::LINES_MANY),
            };
            tr!(lang, m::LINES_NOUN, n = n, word = word)
        }
    }
}

/// Why a turn stopped (D113).
pub fn stop_reason(reason: StopReason, function: &Function, lang: Lang) -> String {
    match reason {
        StopReason::StepLimit => {
            tr!(lang, m::STOP_STEP_LIMIT, steps = function.max_steps_total)
        }
        StopReason::ContextFull => tr!(lang, m::STOP_CONTEXT_FULL).to_string(),
        StopReason::CutOff => {
            tr!(lang, m::STOP_CUT_OFF, tokens = function.max_action_tokens)
        }
        StopReason::ServerError => tr!(lang, m::STOP_SERVER).to_string(),
        StopReason::Interrupted => tr!(lang, m::INTERRUPTED).to_string(),
    }
}

/// Why a turn stopped, and how to go on (D113).
pub fn stopped(reason: StopReason, function: &Function, id: &str, lang: Lang) -> String {
    let why = stop_reason(reason, function, lang);
    format!("{why}\n{}", tr!(lang, m::STOPPED_RESUME, id = id))
}

fn template_problem(problem: &TemplateProblem, lang: Lang) -> String {
    match problem {
        TemplateProblem::Missing(p) => tr!(lang, m::TEMPLATE_MISSING, p = p),
        TemplateProblem::Repeated(p) => tr!(lang, m::TEMPLATE_REPEATED, p = p),
        TemplateProblem::Unexpected(p) => tr!(lang, m::TEMPLATE_UNEXPECTED, p = p),
        TemplateProblem::ValueBeforeName => tr!(lang, m::TEMPLATE_VALUE_BEFORE_NAME).to_string(),
        TemplateProblem::NothingBeforeName => {
            tr!(lang, m::TEMPLATE_NOTHING_BEFORE_NAME).to_string()
        }
        TemplateProblem::NothingAfterValue => {
            tr!(lang, m::TEMPLATE_NOTHING_AFTER_VALUE).to_string()
        }
        TemplateProblem::Empty => tr!(lang, m::TEMPLATE_EMPTY).to_string(),
    }
}

fn function_problem(problem: &FunctionProblem, lang: Lang) -> String {
    use FunctionProblem as P;
    match problem {
        P::UnknownState(name) => tr!(lang, m::FUNC_UNKNOWN_STATE, name = name),
        P::BadTransitionKey => tr!(lang, m::FUNC_BAD_TRANSITION_KEY).to_string(),
        P::MissingTransition { tool, outcome } => {
            tr!(
                lang,
                m::FUNC_MISSING_TRANSITION,
                tool = tool,
                outcome = outcome
            )
        }
        P::NoActions => tr!(lang, m::FUNC_NO_ACTIONS).to_string(),
        P::NextWithoutTools => tr!(lang, m::FUNC_NEXT_WITHOUT_TOOLS).to_string(),
        P::OnLimitWithoutLimit => tr!(lang, m::FUNC_ON_LIMIT_WITHOUT_LIMIT).to_string(),
        P::OnLimitCycle => tr!(lang, m::FUNC_ON_LIMIT_CYCLE).to_string(),
        P::Unreachable => tr!(lang, m::FUNC_UNREACHABLE).to_string(),
        P::NoThinking { format } => tr!(lang, m::FUNC_NO_THINKING, format = format),
        P::Pattern(error) => pattern_error(error, lang),
        P::PatternMayEndValue { end } => tr!(lang, m::FUNC_PATTERN_MAY_END_VALUE, end = end),
        P::EmptyEnum => tr!(lang, m::FUNC_EMPTY_ENUM).to_string(),
        P::BadRange => tr!(lang, m::FUNC_BAD_RANGE).to_string(),
        P::ConstraintKeys => tr!(lang, m::FUNC_CONSTRAINT_KEYS).to_string(),
        P::NotPositive => tr!(lang, m::FUNC_NOT_POSITIVE).to_string(),
    }
}

fn pattern_error(error: &PatternError, lang: Lang) -> String {
    use PatternErrorKind as K;
    let at = error.offset;
    match &error.kind {
        K::Anchor => tr!(lang, m::PATTERN_ANCHOR, at = at),
        K::GroupSyntax => tr!(lang, m::PATTERN_GROUP_SYNTAX, at = at),
        K::Escape(c) => tr!(lang, m::PATTERN_ESCAPE, at = at, c = c),
        K::TrailingBackslash => tr!(lang, m::PATTERN_TRAILING_BACKSLASH, at = at),
        K::NothingToRepeat => tr!(lang, m::PATTERN_NOTHING_TO_REPEAT, at = at),
        K::BadRepetition => tr!(lang, m::PATTERN_BAD_REPETITION, at = at),
        K::UnbalancedGroup => tr!(lang, m::PATTERN_UNBALANCED_GROUP, at = at),
        K::BadClass => tr!(lang, m::PATTERN_BAD_CLASS, at = at),
        K::BadRange => tr!(lang, m::PATTERN_BAD_RANGE, at = at),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locale(vars: &[(&str, &str)]) -> Lang {
        Lang::from_locale(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.into())
        })
    }

    #[test]
    fn locale_selects_ukrainian() {
        assert_eq!(locale(&[("LANG", "uk_UA.UTF-8")]), Lang::Uk);
        assert_eq!(locale(&[("LANG", "uk")]), Lang::Uk);
    }

    #[test]
    fn locale_falls_back_to_english() {
        assert_eq!(locale(&[]), Lang::En);
        assert_eq!(locale(&[("LANG", "C")]), Lang::En);
        assert_eq!(locale(&[("LANG", "de_DE.UTF-8")]), Lang::En);
    }

    #[test]
    fn locale_variables_have_precedence() {
        let all = [("LC_ALL", "uk_UA.UTF-8"), ("LC_MESSAGES", "en_US.UTF-8")];
        assert_eq!(locale(&all), Lang::Uk);
        let messages = [("LC_MESSAGES", "uk_UA.UTF-8"), ("LANG", "en_US.UTF-8")];
        assert_eq!(locale(&messages), Lang::Uk);
        let empty_all = [("LC_ALL", ""), ("LANG", "uk_UA.UTF-8")];
        assert_eq!(locale(&empty_all), Lang::Uk);
    }

    #[test]
    fn usage_errors_point_to_help() {
        let error = Error::UnknownOption("--url".into());
        assert_eq!(
            error.message(Lang::En),
            "unknown option `--url`\nRun `agentLIBRE --help` for usage."
        );
        assert_eq!(
            error.message(Lang::Uk),
            "невідомий параметр `--url`\nДив. `agentLIBRE --help`."
        );
        assert!(
            !Error::FunctionNotFound {
                name: "x".into(),
                path: "/p".into()
            }
            .message(Lang::En)
            .contains("--help")
        );
    }

    #[test]
    fn ukrainian_line_counts() {
        let words: Vec<String> = [0, 1, 2, 5, 11, 12, 21, 22, 25, 101, 112]
            .iter()
            .map(|&n| lines(n, Lang::Uk))
            .collect();
        assert_eq!(
            words,
            [
                "0 рядків",
                "1 рядок",
                "2 рядки",
                "5 рядків",
                "11 рядків",
                "12 рядків",
                "21 рядок",
                "22 рядки",
                "25 рядків",
                "101 рядок",
                "112 рядків"
            ]
        );
        assert_eq!(lines(1, Lang::En), "1 line");
    }

    #[test]
    fn language_codes() {
        assert_eq!(Lang::from_code("en"), Some(Lang::En));
        assert_eq!(Lang::from_code("uk"), Some(Lang::Uk));
        assert_eq!(Lang::from_code("uk_UA"), None);
    }
}

/// Texts of `agentLIBRE server` (D146-D150).
pub mod server {
    use super::Lang;
    use super::m;
    use crate::cli::serve::{Health, UNIT};

    pub fn starting(model: &str, url: &str, lang: Lang) -> String {
        tr!(
            lang,
            m::SERVER_STARTING,
            model = model,
            url = url,
            unit = UNIT
        )
    }

    pub fn not_running(lang: Lang) -> String {
        tr!(lang, m::SERVER_NOT_RUNNING, unit = UNIT)
    }

    /// `agentLIBRE server status`.
    pub fn status(
        active: bool,
        url: &str,
        model: Option<&str>,
        health: Health,
        lang: Lang,
    ) -> String {
        tr!(
            lang,
            m::SERVER_STATUS_BLOCK,
            unit = UNIT,
            active = if active {
                tr!(lang, m::SERVER_ACTIVE)
            } else {
                tr!(lang, m::SERVER_INACTIVE)
            },
            url = url,
            model = model.unwrap_or_else(|| tr!(lang, m::SERVER_MODEL_NOT_SET)),
            health = match health {
                Health::Ready => tr!(lang, m::SERVER_HEALTH_READY),
                Health::Loading => tr!(lang, m::SERVER_HEALTH_LOADING),
                Health::Down => tr!(lang, m::SERVER_HEALTH_DOWN),
            }
        )
    }
}

/// Texts of the interactive session (D118, D120, D124, D134, D135, D138).
pub mod repl {
    use super::Lang;
    use super::m;
    use crate::session::StopReason;

    /// The description of a `/` command in the menu and in `/help`.
    pub fn command(name: &str, lang: Lang) -> &'static str {
        match name {
            "/quit" => tr!(lang, m::CMD_QUIT),
            "/exit" => tr!(lang, m::CMD_EXIT),
            "/help" => tr!(lang, m::CMD_HELP),
            "/status" => tr!(lang, m::CMD_STATUS),
            "/new" => tr!(lang, m::CMD_NEW),
            "/compact" => tr!(lang, m::CMD_COMPACT),
            _ => "",
        }
    }

    /// One line of `/status`.
    pub fn status_line(label: &str, value: &str, lang: Lang) -> String {
        let label = match label {
            "session" => tr!(lang, m::LABEL_SESSION),
            "function" => tr!(lang, m::LABEL_FUNCTION),
            "workspace" => tr!(lang, m::LABEL_WORKSPACE),
            "server" => tr!(lang, m::LABEL_SERVER),
            "context" => tr!(lang, m::LABEL_CONTEXT),
            "state" => tr!(lang, m::LABEL_STATE),
            other => other,
        };
        tr!(lang, m::STATUS_LINE, label = label, value = value)
    }

    /// Time as `5s`, `2m 3s` or `1h 4m`.
    pub fn duration(seconds: u64) -> String {
        match seconds {
            0 => "<1s".into(),
            1..=59 => format!("{seconds}s"),
            60..=3599 => format!("{}m {}s", seconds / 60, seconds % 60),
            _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
        }
    }

    /// The line that ends a turn that stopped (D127).
    pub fn stopped(reason: StopReason, function: &crate::function::Function, lang: Lang) -> String {
        super::stop_reason(reason, function, lang)
    }
}
