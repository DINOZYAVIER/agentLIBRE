//! Languages of CLI messages (D65-D68).

use std::ffi::OsString;

use crate::error::{CredentialProblem, Error, Expected, FunctionProblem, TemplateProblem};
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
Usage: agl [-p <message> [--attach <path>]...] [--resume <id>] [--function <name>]
           [--think <level>] [--yes]
       agl init
       agl server start [--yes]|stop|status
       agl forge list|show|new|status|check|log|diff|rehome

Without -p, agl starts an interactive session.

Options:
  -p <message>        send one message, print the answer and exit
  --resume <id>       continue session <id>
  --function <name>   use Function <name>
  --think <level>     the model's reasoning level: off, low, medium or xhigh
  --attach <path>     with -p: attach an image, a video or an audio; repeatable
  --yes               start the llama-server of the project without asking
                      when the server of another project runs
  -h, --help          print this help

Project (agentLIBRE/<project> under XDG_DATA_HOME, its agentLIBRE.toml):
  init                register the current directory as a project now; a
                      directory is registered at its first write otherwise

Server (systemd user unit agentLIBRE-llama-server-<project>; [server] in the
project's agentLIBRE.toml over the one of the user):
  server start        start llama-server and wait until it is ready
  server stop         stop it and the whisper server of the project
  server status       show whether it runs and is ready

Forge (the artifact store of the workspace, agentLIBRE/<workspace>/forge
under XDG_DATA_HOME):
  forge list                      the artifacts of the workspace
  forge show <name>               the rendered header and the body
  forge new task-spec --title <t> a new task-spec ([--name <n>])
  forge status <name> <state>     open, in-progress, done, backlog or canceled
  forge check <name>              the structural checks and the verification cmds
  forge log <name>                the commits of the artifact's file
  forge diff <name> [commit]      the diff of the artifact's file
  forge rehome [<store>] [--new]  move the workspace store",
        uk: "\
Використання: agl [-p <повідомлення> [--attach <шлях>]...] [--resume <id>] [--function <назва>]
              [--think <level>] [--yes]
              agl init
              agl server start [--yes]|stop|status
              agl forge list|show|new|status|check|log|diff|rehome

Без -p agl запускає інтерактивну сесію.

Параметри:
  -p <повідомлення>   надіслати одне повідомлення, вивести відповідь і вийти
  --resume <id>       продовжити сесію <id>
  --function <назва>  використати Function <назва>
  --think <level>     рівень міркувань моделі: off, low, medium або xhigh
  --attach <шлях>     разом з -p: прикріпити зображення, відео чи аудіо; можна
                      повторювати
  --yes               запустити llama-server проєкту без питання, коли працює
                      сервер іншого проєкту
  -h, --help          показати цю довідку

Проєкт (agentLIBRE/<проєкт> у XDG_DATA_HOME, його agentLIBRE.toml):
  init                зареєструвати поточний каталог як проєкт зараз; інакше
                      каталог реєструється при першому записі

Сервер (user-unit systemd agentLIBRE-llama-server-<проєкт>; [server] у
agentLIBRE.toml проєкту поверх файлу користувача):
  server start        запустити llama-server і дочекатися готовності
  server stop         зупинити його і whisper-сервер проєкту
  server status       показати, чи він працює і чи готовий

Forge (сховище артефактів робочої області, agentLIBRE/<робоча область>/forge
у XDG_DATA_HOME):
  forge list                      артефакти робочої області
  forge show <назва>              відтворений заголовок і тіло
  forge new task-spec --title <t> новий task-spec ([--name <n>])
  forge status <назва> <стан>     open, in-progress, done, backlog або canceled
  forge check <назва>             структурні перевірки і команди verification
  forge log <назва>               коміти файлу артефакту
  forge diff <назва> [коміт]     розбіжності файлу артефакту
  forge rehome [<сховище>] [--new]   перемістити сховище робочої області",
    };

    /// The pointer appended to usage errors.
    pub const USAGE_POINTER: Msg = Msg {
        en: "Run `agl --help` for usage.",
        uk: "Див. `agl --help`.",
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

    /// `Error::BadReasoningLevel`.
    pub const BAD_REASONING_LEVEL: Msg = Msg {
        en: "invalid reasoning level `{value}` for `--think`: \
             the levels are off, low, medium and xhigh",
        uk: "некоректний рівень міркувань `{value}` для `--think`: \
             рівні — off, low, medium та xhigh",
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

    /// `Error::UnknownServerKind`.
    pub const UNKNOWN_SERVER_KIND: Msg = Msg {
        en: "`kind` in {path} is not `llama` or `strata`: `{value}`",
        uk: "`kind` у {path} не `llama` і не `strata`: `{value}`",
    };

    /// `Error::UnsupportedSearchUrl`.
    pub const UNSUPPORTED_SEARCH_URL: Msg = Msg {
        en: "unsupported search address `{url}` in {path}; only https:// is supported",
        uk: "непідтримувана адреса пошуку `{url}` у {path}; підтримується лише https://",
    };

    /// `Error::SearchPathNotAbsolute`.
    pub const SEARCH_PATH_NOT_ABSOLUTE: Msg = Msg {
        en: "`{key}` in {path} must be an absolute path, not `{value}`",
        uk: "`{key}` у {path} має бути абсолютним шляхом, а не `{value}`",
    };

    /// `Error::SearchCredential`.
    pub const SEARCH_CREDENTIAL: Msg = Msg {
        en: "the search credential {file} is refused: {problem}",
        uk: "облікові дані пошуку {file} відхилено: {problem}",
    };

    /// What is wrong with a credential file, for `SEARCH_CREDENTIAL`.
    pub const CREDENTIAL_UNREADABLE: Msg = Msg {
        en: "it cannot be read ({detail})",
        uk: "його не можна прочитати ({detail})",
    };
    pub const CREDENTIAL_NOT_REGULAR: Msg = Msg {
        en: "it is not a regular file (a link does not count)",
        uk: "це не звичайний файл (посилання не підходить)",
    };
    pub const CREDENTIAL_NOT_OWN: Msg = Msg {
        en: "it is not owned by you",
        uk: "він вам не належить",
    };
    pub const CREDENTIAL_OTHERS_ACCESS: Msg = Msg {
        en: "the group or others can access it; make it mode 0600",
        uk: "група або інші мають до нього доступ; зробіть режим 0600",
    };
    pub const CREDENTIAL_OTHER_LINKS: Msg = Msg {
        en: "it has more than one hard link",
        uk: "він має більше одного жорсткого посилання",
    };
    pub const CREDENTIAL_EMPTY: Msg = Msg {
        en: "it is empty",
        uk: "він порожній",
    };
    pub const CREDENTIAL_TOO_LARGE: Msg = Msg {
        en: "it is larger than 1 MiB",
        uk: "він більший за 1 МіБ",
    };
    pub const CREDENTIAL_WRONG_TYPE: Msg = Msg {
        en: "it is not a PEM of the type {expected}",
        uk: "це не PEM типу {expected}",
    };

    /// `Error::MissingServerUrl`.
    pub const MISSING_SERVER_URL: Msg = Msg {
        en: "the llama-server address is not set; add to {project_path} (this project) or {user_path} (the default of every project):\n\n[server]\nurl = \"http://HOST:PORT\"",
        uk: "адресу llama-server не задано; додай у {project_path} (цей проєкт) або {user_path} (типове для всіх проєктів):\n\n[server]\nurl = \"http://HOST:PORT\"",
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
        en: "cannot reach llama-server at {url}: {detail}\nTo start it: agl server start",
        uk: "не вдалося з'єднатися з llama-server за адресою {url}: {detail}\nЩоб запустити: agl server start",
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
        en: "the model is not set; add the name of a file in {models} to {project_path} (this project) or {user_path} (the default of every project):\n\n[server]\nmodel = \"NAME.gguf\"",
        uk: "модель не задано; додай назву файлу з {models} у {project_path} (цей проєкт) або {user_path} (типове для всіх проєктів):\n\n[server]\nmodel = \"NAME.gguf\"",
    };

    /// `Error::ModelNotFound`.
    pub const MODEL_NOT_FOUND: Msg = Msg {
        en: "no model file {path}",
        uk: "немає файлу моделі {path}",
    };

    /// `Error::NotLocalServer`.
    pub const NOT_LOCAL_SERVER: Msg = Msg {
        en: "agl server start runs llama-server only on 127.0.0.1 or localhost, not at {url}",
        uk: "agl server start запускає llama-server лише на 127.0.0.1 або localhost, не за {url}",
    };

    /// `Error::EngineNotFound` (AGL-024, D5).
    pub const ENGINE_NOT_FOUND: Msg = Msg {
        en: "{engine} is not in PATH",
        uk: "{engine} немає в PATH",
    };

    /// `Error::EngineSpawn` (AGL-024, D2).
    pub const ENGINE_SPAWN: Msg = Msg {
        en: "cannot run {engine}: {source}",
        uk: "не вдалося запустити {engine}: {source}",
    };

    /// `Error::Systemd`.
    pub const SYSTEMD: Msg = Msg {
        en: "systemd failed: {detail}",
        uk: "помилка systemd: {detail}",
    };

    /// `Error::ProjectNotRegistered` (AGL-015, D14).
    pub const PROJECT_NOT_REGISTERED: Msg = Msg {
        en: "{project} is not a registered project, so it has no llama-server of its own; run `agl init` there first",
        uk: "{project} не є зареєстрованим проєктом, тож не має власного llama-server; спершу виконай там `agl init`",
    };

    /// `Error::PortBusy` without a running unit (D7).
    pub const PORT_BUSY: Msg = Msg {
        en: "the address {url} is already in use by something that is not a unit of agl; free it or set another `url` in the project's [server]",
        uk: "адреса {url} уже зайнята чимось, що не є unit-ом agl; звільни її або задай інший `url` у [server] проєкту",
    };

    /// `Error::PortBusy` with running units (D7).
    pub const PORT_BUSY_UNITS: Msg = Msg {
        en: "the address {url} is already in use; the llama-server units that run: {units}. Stop the one that holds it (agl server stop) or set another `url` in the project's [server]",
        uk: "адреса {url} уже зайнята; працюють unit-и llama-server: {units}. Зупини той, що її тримає (agl server stop), або задай інший `url` у [server] проєкту",
    };

    /// `Error::ServerRunning` (D13).
    pub const SERVER_RUNNING: Msg = Msg {
        en: "a llama-server of another project is running ({units}); to start one more, use --yes",
        uk: "працює llama-server іншого проєкту ({units}); щоб запустити ще один, додай --yes",
    };

    /// `Error::StartDeclined` (D9).
    pub const START_DECLINED: Msg = Msg {
        en: "llama-server was not started",
        uk: "llama-server не запущено",
    };

    /// The question of D9: another server runs, start one more?
    pub const OTHERS_RUNNING: Msg = Msg {
        en: "A llama-server is running ({units}). Start one more? [y/N]",
        uk: "llama-server уже працює ({units}). Запустити ще один? [y/N]",
    };

    /// `Error::WhisperNotSet` (AGL-014, D22).
    pub const WHISPER_NOT_SET: Msg = Msg {
        en: "an audio needs the whisper server, and `url` and `model` of [whisper] are not both set; add to {project_path} (this project) or {user_path} (the default of every project):\n\n[whisper]\nurl = \"http://127.0.0.1:18081\"\nmodel = \"ggml-large-v3-q8_0.bin\"",
        uk: "аудіо потребує whisper-сервера, а `url` і `model` у [whisper] задано не обидва; додай у {project_path} (цей проєкт) або {user_path} (типове для всіх проєктів):\n\n[whisper]\nurl = \"http://127.0.0.1:18081\"\nmodel = \"ggml-large-v3-q8_0.bin\"",
    };

    /// `Error::WhisperServerNotFound`.
    pub const WHISPER_SERVER_NOT_FOUND: Msg = Msg {
        en: "whisper-server is not in PATH (build whisper.cpp and put its whisper-server in PATH)",
        uk: "whisper-server немає в PATH (збери whisper.cpp і поклади його whisper-server у PATH)",
    };

    /// `Error::Transcribe`.
    pub const TRANSCRIBE: Msg = Msg {
        en: "the whisper server at {url} could not transcribe: {detail}",
        uk: "whisper-сервер за {url} не зміг розпізнати: {detail}",
    };

    /// whisper-server begins (AGL-014).
    pub const WHISPER_STARTING: Msg = Msg {
        en: "starting whisper-server ({model}) at {url} as unit {unit}…",
        uk: "запускаю whisper-server ({model}) за {url} як unit {unit}…",
    };

    /// whisper-server is ready.
    pub const WHISPER_READY: Msg = Msg {
        en: "whisper-server is ready at {url}",
        uk: "whisper-server готовий за {url}",
    };

    /// An audio goes to whisper.
    pub const TRANSCRIBING: Msg = Msg {
        en: "transcribing {name}…",
        uk: "розпізнаю {name}…",
    };

    /// `Error::AttachWithoutPrompt`.
    pub const ATTACH_WITHOUT_PROMPT: Msg = Msg {
        en: "--attach goes with -p; in a session use /attach <path>",
        uk: "--attach працює разом з -p; у сесії використовуй /attach <шлях>",
    };

    /// `Error::NoMultimodal` (AGL-014).
    pub const NO_MULTIMODAL: Msg = Msg {
        en: "the server has no multimodal projector, so it cannot take an image; set `mmproj` in [server] of the project and restart it (agl server stop)",
        uk: "у сервера немає мультимодального проєктора, тож він не приймає зображень; задай `mmproj` у [server] проєкту і перезапусти його (agl server stop)",
    };

    /// `Error::MediaChanged` (AGL-014, D7).
    pub const MEDIA_CHANGED: Msg = Msg {
        en: "{path} is not what its name says (the copy of a medium of the session was changed)",
        uk: "{path} не те, що в його назві (копію медіа сесії змінено)",
    };

    /// `Error::MediaKind`.
    pub const MEDIA_KIND: Msg = Msg {
        en: "{path} is not an image, a video or an audio",
        uk: "{path} не є зображенням, відео чи аудіо",
    };

    /// `Error::Ffmpeg`.
    pub const FFMPEG: Msg = Msg {
        en: "ffmpeg failed: {detail}",
        uk: "помилка ffmpeg: {detail}",
    };

    /// `Error::ServerFailed`.
    pub const SERVER_FAILED: Msg = Msg {
        en: "the server stopped before it was ready; see journalctl --user -u {unit}",
        uk: "сервер зупинився, не встигнувши стати готовим; див. journalctl --user -u {unit}",
    };

    /// `Error::ServerTimeout`.
    pub const SERVER_TIMEOUT: Msg = Msg {
        en: "the server was not ready after {seconds} s; it keeps loading in the background (agl server status, journalctl --user -u {unit})",
        uk: "сервер не став готовим за {seconds} с; він вантажиться далі у фоні (agl server status, journalctl --user -u {unit})",
    };

    /// `Error::EngineDied` (AGL-024, D2).
    pub const ENGINE_DIED: Msg = Msg {
        en: "the engine stopped before it was ready; see {log}",
        uk: "движок зупинився, не встигнувши стати готовим; див. {log}",
    };

    /// `Error::EngineTimeout` (AGL-024, D2).
    pub const ENGINE_TIMEOUT: Msg = Msg {
        en: "the engine was not ready after {seconds} s; see {log}",
        uk: "движок не став готовим за {seconds} с; див. {log}",
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

    /// `StopReason::ServerError`.
    pub const STOP_SERVER: Msg = Msg {
        en: "stopped: llama-server failed",
        uk: "зупинено: помилка llama-server",
    };

    /// `StopReason::StirLimit`.
    pub const STOP_STIR_LIMIT: Msg = Msg {
        en: "stopped: the model's output was not an action of the state after the stirs",
        uk: "зупинено: вихід моделі не був дією стану після перемішувань",
    };

    /// The second line of `stopped`: how to go on (D113).
    pub const STOPPED_RESUME: Msg = Msg {
        en: "to continue: agl --resume {id} -p \"...\"",
        uk: "щоб продовжити: agl --resume {id} -p \"...\"",
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
        en: "close agl",
        uk: "закрити agl",
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

    /// The description of a `/` command (AGL-013).
    pub const CMD_ATTACH: Msg = Msg {
        en: "attach an image, a video or an audio to the next message",
        uk: "прикріпити зображення, відео чи аудіо до наступного повідомлення",
    };

    /// `/attach` without a path.
    pub const ATTACH_USAGE: Msg = Msg {
        en: "usage: /attach <path of an image, a video or an audio>",
        uk: "використання: /attach <шлях до зображення, відео чи аудіо>",
    };

    /// `/attach` took a file.
    pub const ATTACHED: Msg = Msg {
        en: "attached {placeholder} ({name})",
        uk: "прикріплено {placeholder} ({name})",
    };

    /// Ctrl-V found nothing to attach or paste.
    pub const CLIPBOARD_EMPTY: Msg = Msg {
        en: "the clipboard holds nothing to paste (an image needs wl-paste)",
        uk: "у буфері обміну нема чого вставити (для зображення потрібен wl-paste)",
    };

    pub const CMD_REMEMBER: Msg = Msg {
        en: "keep a note in the workspace memory: <name> | <text>",
        uk: "зберегти нотатку в пам'яті робочої області: <назва> | <текст>",
    };

    /// `/remember` without a text, or with an empty name or text.
    pub const REMEMBER_USAGE: Msg = Msg {
        en: "usage: /remember <name> | <text> (the entry is written at once), or /remember <text> (the model names it)",
        uk: "використання: /remember <назва> | <текст> (запис одразу), або /remember <текст> (назву дає модель)",
    };

    /// A `/remember` that could not write the entry.
    pub const REMEMBER_FAILED: Msg = Msg {
        en: "not remembered: {reason}",
        uk: "не збережено: {reason}",
    };

    pub const CMD_THINK: Msg = Msg {
        en: "the reasoning level: off, low, medium or xhigh",
        uk: "рівень міркувань: off, low, medium або xhigh",
    };

    /// The keys of the interactive session (D120).
    pub const KEYS: Msg = Msg {
        en: "Enter sends; Shift-Enter or Alt-Enter adds a line; Up/Down walk the history; \
             Ctrl-V attaches the clipboard; Ctrl-R records the voice and writes what is said \
             (Ctrl-R again stops, Escape cancels); \
             Ctrl-C stops the turn (or clears the line, or quits when it is empty); \
             Ctrl-D quits",
        uk: "Enter надсилає; Shift-Enter або Alt-Enter додає рядок; Вгору/Вниз — історія; \
             Ctrl-V прикріплює буфер обміну; Ctrl-R записує голос і вписує сказане \
             (ще раз Ctrl-R зупиняє, Escape скасовує); \
             Ctrl-C зупиняє хід (або очищає рядок, або виходить, якщо він порожній); \
             Ctrl-D виходить",
    };

    /// The separator row while the voice is recorded (AGL-016, D9).
    pub const VOICE_RECORDING: Msg = Msg {
        en: "Recording {time} · Ctrl-R to stop",
        uk: "Запис {time} · Ctrl-R, щоб зупинити",
    };

    /// The separator row while the recording is turned into text.
    pub const VOICE_TRANSCRIBING: Msg = Msg {
        en: "Transcribing…",
        uk: "Розпізнаю…",
    };

    /// The recording had no words.
    pub const VOICE_NOTHING_HEARD: Msg = Msg {
        en: "nothing was heard in the recording",
        uk: "у записі нічого не почути",
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

    /// A label of one line of `/status` (AGL-013).
    pub const LABEL_REASONING: Msg = Msg {
        en: "reasoning",
        uk: "міркування",
    };

    /// The reasoning level of `/status` and `/think` when unset (AGL-013).
    pub const REASONING_DEFAULT: Msg = Msg {
        en: "default",
        uk: "за замовчуванням",
    };

    /// The reply of `/think` (AGL-013).
    pub const THINK_LEVEL: Msg = Msg {
        en: "reasoning: {level}",
        uk: "міркування: {level}",
    };

    /// A reasoning level outside the four (AGL-013).
    pub const REASONING_BAD: Msg = Msg {
        en: "invalid reasoning level `{value}`; the levels are off, low, medium and xhigh",
        uk: "некоректний рівень міркувань `{value}`; рівні — off, low, medium та xhigh",
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

    /// The decoding speed (AGL-012).
    pub const SPEED: Msg = Msg {
        en: "{speed} tok/s",
        uk: "{speed} ток/с",
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

    /// `agl server start` begins (D146).
    pub const SERVER_STARTING: Msg = Msg {
        en: "starting llama-server ({model}) at {url} as unit {unit}…",
        uk: "запускаю llama-server ({model}) за {url} як unit {unit}…",
    };

    /// `agl server start` is ready (D149).
    pub const SERVER_READY: Msg = Msg {
        en: "llama-server is ready at {url}",
        uk: "llama-server готовий за {url}",
    };

    /// `agl server start` finds the server up (D146).
    pub const SERVER_ALREADY_RUNNING: Msg = Msg {
        en: "llama-server is already running at {url}",
        uk: "llama-server уже працює за {url}",
    };

    /// `agl server stop` stopped it (D146).
    pub const SERVER_STOPPED: Msg = Msg {
        en: "llama-server stopped",
        uk: "llama-server зупинено",
    };

    /// `agl server stop` stopped the whisper server too (AGL-014).
    pub const WHISPER_STOPPED: Msg = Msg {
        en: "whisper-server stopped",
        uk: "whisper-server зупинено",
    };

    /// `agl server stop` finds it down (D146).
    pub const SERVER_NOT_RUNNING: Msg = Msg {
        en: "unit {unit} is not running",
        uk: "unit {unit} не працює",
    };

    /// `agl server status`: the unit is active.
    pub const SERVER_ACTIVE: Msg = Msg {
        en: "active",
        uk: "активний",
    };

    /// `agl server status`: the unit is not active.
    pub const SERVER_INACTIVE: Msg = Msg {
        en: "inactive",
        uk: "неактивний",
    };

    /// `agl server status`: `[server] model` is not set.
    pub const SERVER_MODEL_NOT_SET: Msg = Msg {
        en: "not set",
        uk: "не задано",
    };

    /// `agl server status`: `/health` is ready.
    pub const SERVER_HEALTH_READY: Msg = Msg {
        en: "ready",
        uk: "готовий",
    };

    /// `agl server status`: `/health` answers while loading.
    pub const SERVER_HEALTH_LOADING: Msg = Msg {
        en: "loading",
        uk: "завантажується",
    };

    /// `agl server status`: `/health` does not answer.
    pub const SERVER_HEALTH_DOWN: Msg = Msg {
        en: "not responding",
        uk: "не відповідає",
    };

    /// `agl server status` (D150).
    pub const SERVER_STATUS_BLOCK: Msg = Msg {
        en: "unit: {unit} ({active})\nurl: {url}\nmodel: {model}\nhealth: {health}",
        uk: "unit: {unit} ({active})\nадреса: {url}\nмодель: {model}\nстан: {health}",
    };

    /// `Error::GitSpawn`.
    pub const GIT_SPAWN: Msg = Msg {
        en: "cannot run git: {source}",
        uk: "не вдалося запустити git: {source}",
    };

    /// `Error::GitFailed`.
    pub const GIT_FAILED: Msg = Msg {
        en: "git {args} failed: {detail}",
        uk: "git {args} завершився з помилкою: {detail}",
    };

    /// `Error::NotInRepository`.
    pub const NOT_IN_REPOSITORY: Msg = Msg {
        en: "{cwd} is not in a git repository; the forge works on the workspace of the current directory",
        uk: "{cwd} не є git-репозиторієм; forge працює з робочою областю поточної теки",
    };

    /// `Error::StoreNotRepository`.
    pub const STORE_NOT_REPOSITORY: Msg = Msg {
        en: "the store directory is not a git repository: {dir}",
        uk: "каталог сховища не є git-репозиторієм: {dir}",
    };

    /// `Error::Write`.
    pub const FORGE_WRITE: Msg = Msg {
        en: "cannot write {path}: {source}",
        uk: "не вдалося записати {path}: {source}",
    };

    /// `Error::MarkerFormat`.
    pub const MARKER_FORMAT: Msg = Msg {
        en: "{path}: the marker format is {format}, expected 1",
        uk: "{path}: формат маркера {format}, очікується 1",
    };

    /// `Error::MarkerIdentity`.
    pub const MARKER_IDENTITY: Msg = Msg {
        en: "{path} lists {value}, which is not a sha256 identity",
        uk: "{path} містить {value}, а це не sha256-ідентифікатор",
    };

    /// `Error::WorkspaceAmbiguous`.
    pub const WORKSPACE_AMBIGUOUS: Msg = Msg {
        en: "the workspace identity is in the markers of {dirs}; a store was copied by hand, remove the copy",
        uk: "ідентифікатор робочої області є в маркерах {dirs}; сховище скопійовано вручну, прибери копію",
    };

    /// `Error::RehomeNoStore`.
    pub const REHOME_NO_STORE: Msg = Msg {
        en: "a store cannot be chosen without a terminal; stores: {stores} (run `agl forge rehome <store>`)",
        uk: "без термінала не можна вибрати сховище; сховища: {stores} (запусти `agl forge rehome <store>`)",
    };

    /// `Error::StoreNotFound`.
    pub const STORE_NOT_FOUND: Msg = Msg {
        en: "no store is named {name}; stores: {stores}",
        uk: "немає сховища на ім'я {name}; сховища: {stores}",
    };

    /// `Error::ArtifactFrontMatter`.
    pub const ARTIFACT_FRONT_MATTER: Msg = Msg {
        en: "{path} has no `---` front-matter",
        uk: "у {path} немає `---` заголовка",
    };

    /// `Error::ArtifactUnknownSchema`.
    pub const ARTIFACT_UNKNOWN_SCHEMA: Msg = Msg {
        en: "{path} has schema {schema}, expected forge/v1",
        uk: "у {path} схема {schema}, очікується forge/v1",
    };

    /// `Error::ArtifactTypeTableMissing`.
    pub const ARTIFACT_TYPE_TABLE_MISSING: Msg = Msg {
        en: "{path} has no [{type_name}] table",
        uk: "у {path} немає таблиці [{type_name}]",
    };

    /// `Error::ArtifactKey`.
    pub const ARTIFACT_KEY: Msg = Msg {
        en: "{path} has an unknown key `{key}`",
        uk: "у {path} невідомий ключ `{key}`",
    };

    /// `Error::ArtifactDate`.
    pub const ARTIFACT_DATE: Msg = Msg {
        en: "{path} has {key} = {value}, not a YYYY-MM-DD date",
        uk: "у {path} {key} = {value}, а не дата YYYY-MM-DD",
    };

    /// `Error::ArtifactNotFound`.
    pub const ARTIFACT_NOT_FOUND: Msg = Msg {
        en: "no artifact is named {name} in {dir}",
        uk: "артефакту на ім'я {name} немає в {dir}",
    };

    /// `Error::BadState`.
    pub const BAD_STATE: Msg = Msg {
        en: "{state} is not a task-spec state (open, in-progress, done, backlog, canceled)",
        uk: "{state} не є станом task-spec (open, in-progress, done, backlog, canceled)",
    };

    /// `Error::BadTransition`.
    pub const BAD_TRANSITION: Msg = Msg {
        en: "a task-spec cannot go from {from} to {to}",
        uk: "task-spec не може перейти з {from} у {to}",
    };

    /// `Error::NewType`.
    pub const NEW_TYPE: Msg = Msg {
        en: "forge new does not scaffold {type_name}",
        uk: "forge new не створює {type_name}",
    };

    /// `Error::CommitNotFound`.
    pub const COMMIT_NOT_FOUND: Msg = Msg {
        en: "{id} is not a commit in {repo}",
        uk: "{id} не є комітом в {repo}",
    };

    /// `Error::BrokenRange`.
    pub const BROKEN_RANGE: Msg = Msg {
        en: "{anchor} is not an ancestor of {last}",
        uk: "{anchor} не є предком {last}",
    };

    /// A workspace store was synced to its current home.
    pub const FORGE_REHOMED: Msg = Msg {
        en: "workspace store moved: {old} is now {new}",
        uk: "сховище робочої області переміщено: {old} тепер {new}",
    };

    /// The unique same-named store was adopted for a new location.
    pub const FORGE_ADOPTED: Msg = Msg {
        en: "adopted the store {old} as {new} ([forge] auto_rehome = false asks instead)",
        uk: "усиновлено сховище {old} як {new} ([forge] auto_rehome = false запитує натомість)",
    };

    /// A fresh store was created for a new location; the others are named.
    pub const FORGE_FRESH_STORE: Msg = Msg {
        en: "created a new store {dir}; the existing stores are {names} (forge rehome reconnects a moved workspace)",
        uk: "створено нове сховище {dir}; наявні сховища: {names} (forge rehome повертає переміщену робочу область)",
    };

    /// The new-store choice of the store ask.
    pub const FORGE_NEW_STORE: Msg = Msg {
        en: "a new store",
        uk: "нове сховище",
    };

    /// `forge rehome --new` created a fresh single-hash store.
    pub const FORGE_STORE_NEW: Msg = Msg {
        en: "created a new store {dir}",
        uk: "створено нове сховище {dir}",
    };

    /// `agl init` in a directory that is a project already.
    pub const INIT_REGISTERED: Msg = Msg {
        en: "{project} is a project already: {dir}",
        uk: "{project} вже є проєктом: {dir}",
    };

    /// The marker was written when the workspace store was materialized.
    pub const FORGE_MARKER_WRITTEN: Msg = Msg {
        en: "created the workspace store {path}",
        uk: "створено сховище робочої області {path}",
    };

    /// `Error::NameTaken`.
    pub const NAME_TAKEN: Msg = Msg {
        en: "an artifact named {name} is already in {dir}",
        uk: "артефакт на ім'я {name} вже є в {dir}",
    };

    /// `Error::NotTaskSpec`.
    pub const NOT_TASK_SPEC: Msg = Msg {
        en: "{name} is a {type_name}, not a task-spec; status applies to task-specs only",
        uk: "{name} — це {type_name}, а не task-spec; status стосується лише task-spec",
    };

    /// `Error::RehomeInUse`.
    pub const REHOME_IN_USE: Msg = Msg {
        en: "the identity is already in the store {current}; it cannot also be in {target}",
        uk: "ідентичність вже є в сховищі {current}; її не можна ще й в {target}",
    };

    /// `Error::Remove`.
    pub const REMOVE: Msg = Msg {
        en: "cannot remove {path}: {source}",
        uk: "не вдалося прибрати {path}: {source}",
    };

    /// `Error::CmdSpawn`.
    pub const CMD_SPAWN: Msg = Msg {
        en: "the check command cannot be run: {source}",
        uk: "команду перевірки не можна запустити: {source}",
    };

    /// The store ask of a forge command (a terminal only).
    pub const FORGE_ASK_STORE: Msg = Msg {
        en: "which store is this workspace?",
        uk: "яке сховище належить цій робочій області?",
    };

    /// The store ask, an answer that is not a number of the list.
    pub const FORGE_ASK_INVALID: Msg = Msg {
        en: "enter a number from 1 to {last}",
        uk: "введи число від 1 до {last}",
    };

    /// The rehome ask of `forge rehome` without a store (a terminal only).
    pub const FORGE_ASK_REHOME: Msg = Msg {
        en: "which store should this workspace rehome to?",
        uk: "до якого сховища слід повернути цю робочу область?",
    };

    /// `forge check`: the `name` does not match the file it is in.
    pub const FORGE_CHECK_NAME_MISMATCH: Msg = Msg {
        en: "the name {name} does not match the file {file}",
        uk: "ім'я {name} не збігається з файлом {file}",
    };

    /// `forge check`: a `deps` or `supersedes` entry is not in the store.
    pub const FORGE_CHECK_NOT_IN_STORE: Msg = Msg {
        en: "{kind} {name} is not in the store",
        uk: "{kind} {name} немає в сховищі",
    };

    /// `forge check`: a verification cmd failed.
    pub const FORGE_CHECK_FAILED: Msg = Msg {
        en: "check failed: {cmd}",
        uk: "перевірка не пройшла: {cmd}",
    };

    /// `forge new` wrote the scaffold.
    pub const FORGE_NEW_CREATED: Msg = Msg {
        en: "created {path}",
        uk: "створено {path}",
    };

    /// `forge status` changed the state.
    pub const FORGE_STATUS_CHANGED: Msg = Msg {
        en: "{name}: {from} to {to}",
        uk: "{name}: з {from} у {to}",
    };

    // The in-turn `forge` tool (AGL-008): the refusals are the
    // silly-mistakes protection.

    /// The forge tool: an action outside the seven.
    pub const FORGE_TOOL_UNKNOWN_ACTION: Msg = Msg {
        en: "unknown action {action}; the actions are list, show, new, edit, append, status, remove",
        uk: "невідомий action {action}; action'и: list, show, new, edit, append, status, remove",
    };

    /// The forge tool: an action's field is missing.
    pub const FORGE_TOOL_MISSING_FIELD: Msg = Msg {
        en: "forge {action}: {field} is missing; {action} takes {fields}",
        uk: "forge {action}: не вказано {field}; {action} приймає {fields}",
    };

    /// The forge tool: the write is not on a `task-spec`.
    pub const FORGE_TOOL_NOT_TASK_SPEC: Msg = Msg {
        en: "{name} is a {type_name}, not a task-spec; {action} applies to task-specs only",
        uk: "{name} — це {type_name}, а не task-spec; {action} стосується лише task-spec",
    };

    /// The forge tool: `edit` is allowed while `open` only.
    pub const FORGE_TOOL_EDIT_NOT_OPEN: Msg = Msg {
        en: "{name} is {status}; edit is allowed while open (the draft)",
        uk: "{name} — {status}; edit дозволено, поки open (чернетка)",
    };

    /// The forge tool: `append` is allowed while `in-progress` only.
    pub const FORGE_TOOL_APPEND_NOT_IN_PROGRESS: Msg = Msg {
        en: "{name} is {status}; append is allowed while in-progress",
        uk: "{name} — {status}; append дозволено, поки in-progress",
    };

    /// The forge tool: `remove` is allowed while `open` only.
    pub const FORGE_TOOL_REMOVE_NOT_OPEN: Msg = Msg {
        en: "{name} is {status}; remove is allowed while open (a draft)",
        uk: "{name} — {status}; remove дозволено, поки open (чернетка)",
    };

    /// The forge tool: `append`'s section is not in the body.
    pub const FORGE_TOOL_NO_SECTION: Msg = Msg {
        en: "the body of {name} has no `## {section}` heading",
        uk: "у тілі {name} немає заголовка `## {section}`",
    };

    /// The forge tool: the append-only check failed (never expected).
    pub const FORGE_TOOL_APPEND_VIOLATION: Msg = Msg {
        en: "append to {name} would change more than the {section} bullet and updated; nothing was written",
        uk: "append до {name} змінив би більше, ніж bullet {section} і updated; нічого не записано",
    };

    // The agent's forge writes (AGL-008): the `[forge: …]` marker
    // line, muted in the REPL and on stderr in `-p`.

    /// The `memory` tool or `/remember` wrote an entry: `+name` added,
    /// `~name` edited, `-name` removed (AGL-006, M28, M36).
    pub const MEMORY_EVENT: Msg = Msg {
        en: "[memory: {change}]",
        uk: "[memory: {change}]",
    };

    /// The agent's `forge new` created a task-spec.
    pub const FORGE_EVENT_CREATED: Msg = Msg {
        en: "[forge: {name} created]",
        uk: "[forge: {name} створено]",
    };

    /// The agent's `forge edit` changed the draft.
    pub const FORGE_EVENT_EDITED: Msg = Msg {
        en: "[forge: {name} edited]",
        uk: "[forge: {name} заредаговано]",
    };

    /// The agent's `forge append` added a bullet under a section.
    pub const FORGE_EVENT_APPENDED: Msg = Msg {
        en: "[forge: {name} +{section}]",
        uk: "[forge: {name} +{section}]",
    };

    /// The agent's `forge remove` deleted the draft.
    pub const FORGE_EVENT_REMOVED: Msg = Msg {
        en: "[forge: {name} removed]",
        uk: "[forge: {name} видалено]",
    };

    /// The agent's `forge status` moved a task-spec: the target state
    /// rides the section slot.
    pub const FORGE_EVENT_STATUS: Msg = Msg {
        en: "[forge: {name} status {to}]",
        uk: "[forge: {name} статус {to}]",
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
            Error::BadReasoningLevel { value } => {
                tr!(lang, m::BAD_REASONING_LEVEL, value = value)
            }
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
            Error::UnknownServerKind { path, value } => {
                tr!(lang, m::UNKNOWN_SERVER_KIND, path = path.display(), value = value)
            }
            Error::UnsupportedSearchUrl { path, url } => {
                tr!(lang, m::UNSUPPORTED_SEARCH_URL, path = path.display(), url = url)
            }
            Error::SearchPathNotAbsolute { path, key, value } => {
                tr!(
                    lang,
                    m::SEARCH_PATH_NOT_ABSOLUTE,
                    path = path.display(),
                    key = key,
                    value = value
                )
            }
            Error::SearchCredential { file, problem } => {
                let problem = match problem {
                    CredentialProblem::Unreadable(detail) => {
                        tr!(lang, m::CREDENTIAL_UNREADABLE, detail = detail)
                    }
                    CredentialProblem::NotRegular => tr!(lang, m::CREDENTIAL_NOT_REGULAR).to_string(),
                    CredentialProblem::NotOwn => tr!(lang, m::CREDENTIAL_NOT_OWN).to_string(),
                    CredentialProblem::OthersAccess => {
                        tr!(lang, m::CREDENTIAL_OTHERS_ACCESS).to_string()
                    }
                    CredentialProblem::OtherLinks => {
                        tr!(lang, m::CREDENTIAL_OTHER_LINKS).to_string()
                    }
                    CredentialProblem::Empty => tr!(lang, m::CREDENTIAL_EMPTY).to_string(),
                    CredentialProblem::TooLarge => tr!(lang, m::CREDENTIAL_TOO_LARGE).to_string(),
                    CredentialProblem::WrongType { expected } => {
                        tr!(lang, m::CREDENTIAL_WRONG_TYPE, expected = expected)
                    }
                };
                tr!(lang, m::SEARCH_CREDENTIAL, file = file.display(), problem = problem)
            }
            Error::MissingServerUrl {
                user_path,
                project_path,
            } => {
                tr!(
                    lang,
                    m::MISSING_SERVER_URL,
                    user_path = user_path.display(),
                    project_path = project_path.display()
                )
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
            Error::MissingModel {
                user_path,
                project_path,
                models,
            } => {
                tr!(
                    lang,
                    m::MISSING_MODEL,
                    user_path = user_path.display(),
                    project_path = project_path.display(),
                    models = models.display()
                )
            }
            Error::ModelNotFound { path } => {
                tr!(lang, m::MODEL_NOT_FOUND, path = path.display())
            }
            Error::NotLocalServer { url } => tr!(lang, m::NOT_LOCAL_SERVER, url = url),
            Error::EngineNotFound { engine } => {
                tr!(lang, m::ENGINE_NOT_FOUND, engine = engine).to_string()
            }
            Error::EngineSpawn { engine, source } => {
                tr!(lang, m::ENGINE_SPAWN, engine = engine, source = source).to_string()
            }
            Error::Systemd { detail } => tr!(lang, m::SYSTEMD, detail = detail),
            Error::AttachWithoutPrompt => tr!(lang, m::ATTACH_WITHOUT_PROMPT).to_string(),
            Error::NoMultimodal => tr!(lang, m::NO_MULTIMODAL).to_string(),
            Error::WhisperNotSet {
                user_path,
                project_path,
            } => tr!(
                lang,
                m::WHISPER_NOT_SET,
                user_path = user_path.display(),
                project_path = project_path.display()
            ),
            Error::WhisperServerNotFound => tr!(lang, m::WHISPER_SERVER_NOT_FOUND).to_string(),
            Error::Transcribe { url, detail } => {
                tr!(lang, m::TRANSCRIBE, url = url, detail = detail)
            }
            Error::MediaChanged { path } => tr!(lang, m::MEDIA_CHANGED, path = path.display()),
            Error::MediaKind { path } => tr!(lang, m::MEDIA_KIND, path = path.display()),
            Error::Ffmpeg { detail } => tr!(lang, m::FFMPEG, detail = detail),
            Error::ServerFailed { unit } => tr!(lang, m::SERVER_FAILED, unit = unit),
            Error::ServerTimeout { seconds, unit } => {
                tr!(lang, m::SERVER_TIMEOUT, seconds = seconds, unit = unit)
            }
            Error::EngineDied { log } => {
                tr!(lang, m::ENGINE_DIED, log = log.display()).to_string()
            }
            Error::EngineTimeout { seconds, log } => {
                tr!(
                    lang,
                    m::ENGINE_TIMEOUT,
                    seconds = seconds,
                    log = log.display()
                )
                .to_string()
            }
            Error::ProjectNotRegistered { project } => {
                tr!(lang, m::PROJECT_NOT_REGISTERED, project = project.display())
            }
            Error::PortBusy { url, units } => {
                if units.is_empty() {
                    tr!(lang, m::PORT_BUSY, url = url)
                } else {
                    tr!(lang, m::PORT_BUSY_UNITS, url = url, units = units.join(", "))
                }
            }
            Error::ServerRunning { units } => {
                tr!(lang, m::SERVER_RUNNING, units = units.join(", "))
            }
            Error::StartDeclined => tr!(lang, m::START_DECLINED).to_string(),
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
            Error::GitSpawn { source } => tr!(lang, m::GIT_SPAWN, source = source),
            Error::GitFailed { args, detail } => {
                tr!(lang, m::GIT_FAILED, args = args, detail = detail)
            }
            Error::NotInRepository { cwd } => {
                tr!(lang, m::NOT_IN_REPOSITORY, cwd = cwd.display())
            }
            Error::StoreNotRepository { dir } => {
                tr!(lang, m::STORE_NOT_REPOSITORY, dir = dir.display())
            }
            Error::Write { path, source } => {
                tr!(lang, m::FORGE_WRITE, path = path.display(), source = source)
            }
            Error::MarkerFormat { path, format } => {
                tr!(lang, m::MARKER_FORMAT, path = path.display(), format = format)
            }
            Error::MarkerIdentity { path, value } => {
                tr!(lang, m::MARKER_IDENTITY, path = path.display(), value = value)
            }
            Error::WorkspaceAmbiguous { dirs } => {
                tr!(lang, m::WORKSPACE_AMBIGUOUS, dirs = dirs.join(", "))
            }
            Error::RehomeNoStore { stores } => {
                let stores = if stores.is_empty() {
                    "none".to_string()
                } else {
                    stores.join(", ")
                };
                tr!(lang, m::REHOME_NO_STORE, stores = stores)
            }
            Error::StoreNotFound { name, stores } => {
                let stores = if stores.is_empty() {
                    "none".to_string()
                } else {
                    stores.join(", ")
                };
                tr!(lang, m::STORE_NOT_FOUND, name = name, stores = stores)
            }
            Error::ArtifactFrontMatter { path } => {
                tr!(lang, m::ARTIFACT_FRONT_MATTER, path = path.display())
            }
            Error::ArtifactUnknownSchema { path, schema } => {
                tr!(lang, m::ARTIFACT_UNKNOWN_SCHEMA, path = path.display(), schema = schema)
            }
            Error::ArtifactTypeTableMissing { path, type_name } => {
                tr!(
                    lang,
                    m::ARTIFACT_TYPE_TABLE_MISSING,
                    path = path.display(),
                    type_name = type_name
                )
            }
            Error::ArtifactKey { path, key } => {
                tr!(lang, m::ARTIFACT_KEY, path = path.display(), key = key)
            }
            Error::ArtifactDate { path, key, value } => {
                tr!(
                    lang,
                    m::ARTIFACT_DATE,
                    path = path.display(),
                    key = key,
                    value = value
                )
            }
            Error::ArtifactNotFound { name, dir } => {
                tr!(
                    lang,
                    m::ARTIFACT_NOT_FOUND,
                    name = name,
                    dir = dir.display()
                )
            }
            Error::BadState { state } => tr!(lang, m::BAD_STATE, state = state),
            Error::BadTransition { from, to } => {
                tr!(lang, m::BAD_TRANSITION, from = from, to = to)
            }
            Error::NewType { type_name } => tr!(lang, m::NEW_TYPE, type_name = type_name),
            Error::NameTaken { name, dir } => {
                tr!(lang, m::NAME_TAKEN, name = name, dir = dir.display())
            }
            Error::NotTaskSpec { name, type_name } => {
                tr!(lang, m::NOT_TASK_SPEC, name = name, type_name = type_name)
            }
            Error::RehomeInUse { current, target } => {
                tr!(lang, m::REHOME_IN_USE, current = current, target = target)
            }
            Error::CmdSpawn { source } => tr!(lang, m::CMD_SPAWN, source = source),
            Error::CommitNotFound { id, repo } => {
                tr!(lang, m::COMMIT_NOT_FOUND, id = id, repo = repo.display())
            }
            Error::BrokenRange { anchor, last } => {
                tr!(lang, m::BROKEN_RANGE, anchor = anchor, last = last)
            }
            Error::Remove { path, source } => {
                tr!(lang, m::REMOVE, path = path.display(), source = source)
            }
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
            // The machine stops only at a set limit (D33, AGL-021, D3).
            tr!(
                lang,
                m::STOP_STEP_LIMIT,
                steps = function
                    .max_steps_total
                    .expect("a set limit stops the turn")
            )
        }
        StopReason::ContextFull => tr!(lang, m::STOP_CONTEXT_FULL).to_string(),
        StopReason::ServerError => tr!(lang, m::STOP_SERVER).to_string(),
        StopReason::Interrupted => tr!(lang, m::INTERRUPTED).to_string(),
        StopReason::StirLimit => tr!(lang, m::STOP_STIR_LIMIT).to_string(),
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
            "unknown option `--url`\nRun `agl --help` for usage."
        );
        assert_eq!(
            error.message(Lang::Uk),
            "невідомий параметр `--url`\nДив. `agl --help`."
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
    fn the_forge_marker_lines() {
        let name = "agl-009";
        let section = "Progress";
        assert_eq!(
            tr!(Lang::En, m::FORGE_EVENT_CREATED, name = name),
            "[forge: agl-009 created]"
        );
        assert_eq!(
            tr!(Lang::En, m::FORGE_EVENT_EDITED, name = name),
            "[forge: agl-009 edited]"
        );
        assert_eq!(
            tr!(
                Lang::En,
                m::FORGE_EVENT_APPENDED,
                name = name,
                section = section
            ),
            "[forge: agl-009 +Progress]"
        );
        assert_eq!(
            tr!(Lang::En, m::FORGE_EVENT_REMOVED, name = name),
            "[forge: agl-009 removed]"
        );
        assert_eq!(
            tr!(
                Lang::En,
                m::FORGE_EVENT_STATUS,
                name = name,
                to = "in-progress"
            ),
            "[forge: agl-009 status in-progress]"
        );
        assert_eq!(
            tr!(Lang::Uk, m::FORGE_EVENT_CREATED, name = name),
            "[forge: agl-009 створено]"
        );
        assert_eq!(
            tr!(Lang::Uk, m::FORGE_EVENT_EDITED, name = name),
            "[forge: agl-009 заредаговано]"
        );
        assert_eq!(
            tr!(
                Lang::Uk,
                m::FORGE_EVENT_APPENDED,
                name = name,
                section = section
            ),
            "[forge: agl-009 +Progress]"
        );
        assert_eq!(
            tr!(Lang::Uk, m::FORGE_EVENT_REMOVED, name = name),
            "[forge: agl-009 видалено]"
        );
        assert_eq!(
            tr!(
                Lang::Uk,
                m::FORGE_EVENT_STATUS,
                name = name,
                to = "in-progress"
            ),
            "[forge: agl-009 статус in-progress]"
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

    #[test]
    fn the_decoding_speed_is_formatted_per_language() {
        assert_eq!(repl::speed(46.3, Lang::En), "46.3 tok/s");
        assert_eq!(repl::speed(46.26, Lang::En), "46.3 tok/s");
        assert_eq!(repl::speed(46.3, Lang::Uk), "46,3 ток/с");
        assert_eq!(repl::speed(120.0, Lang::Uk), "120,0 ток/с");
    }
}

/// Texts of `agl server` (D146-D150).
pub mod server {
    use super::Lang;
    use super::m;
    use crate::cli::serve::Health;

    pub fn starting(model: &str, url: &str, unit: &str, lang: Lang) -> String {
        tr!(
            lang,
            m::SERVER_STARTING,
            model = model,
            url = url,
            unit = unit
        )
    }

    pub fn not_running(unit: &str, lang: Lang) -> String {
        tr!(lang, m::SERVER_NOT_RUNNING, unit = unit)
    }

    /// The question of D9 with the units that run.
    pub fn others_running(units: &[String], lang: Lang) -> String {
        tr!(lang, m::OTHERS_RUNNING, units = units.join(", "))
    }

    /// `agl server status`.
    pub fn status(
        unit: &str,
        active: bool,
        url: &str,
        model: Option<&str>,
        health: Health,
        lang: Lang,
    ) -> String {
        tr!(
            lang,
            m::SERVER_STATUS_BLOCK,
            unit = unit,
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

/// Texts of the interactive session (D118, D120, D124, D134, D135, D138,
/// AGL-013).
pub mod repl {
    use super::Lang;
    use super::m;
    use crate::backend::ReasoningLevel;
    use crate::session::StopReason;

    /// The description of a `/` command in the menu and in `/help`.
    pub fn command(name: &str, lang: Lang) -> &'static str {
        match name {
            "/quit" => tr!(lang, m::CMD_QUIT),
            "/exit" => tr!(lang, m::CMD_EXIT),
            "/help" => tr!(lang, m::CMD_HELP),
            "/status" => tr!(lang, m::CMD_STATUS),
            "/think" => tr!(lang, m::CMD_THINK),
            "/attach" => tr!(lang, m::CMD_ATTACH),
            "/remember" => tr!(lang, m::CMD_REMEMBER),
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
            "reasoning" => tr!(lang, m::LABEL_REASONING),
            "state" => tr!(lang, m::LABEL_STATE),
            other => other,
        };
        tr!(lang, m::STATUS_LINE, label = label, value = value)
    }

    /// The reasoning level to show (AGL-013): the level's name, or the
    /// template default when unset.
    pub fn reasoning_level(level: Option<ReasoningLevel>, lang: Lang) -> String {
        match level {
            None => tr!(lang, m::REASONING_DEFAULT).to_string(),
            Some(level) => level.name().to_string(),
        }
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

    /// The decoding speed (AGL-012): `46.3 tok/s` in English and
    /// `46,3 ток/с` in Ukrainian.
    pub fn speed(tokens_per_second: f64, lang: Lang) -> String {
        let number = format!("{tokens_per_second:.1}");
        let number = if lang == Lang::Uk {
            number.replace('.', ",")
        } else {
            number
        };
        tr!(lang, m::SPEED, speed = number)
    }

    /// The line that ends a turn that stopped (D127).
    pub fn stopped(reason: StopReason, function: &crate::function::Function, lang: Lang) -> String {
        super::stop_reason(reason, function, lang)
    }
}
