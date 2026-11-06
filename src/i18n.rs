//! Languages of CLI messages (D65-D68).

use std::ffi::OsString;

use crate::error::{
    CredentialProblem, Error, Expected, FunctionProblem, ServerOwner, TemplateProblem,
};
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
Usage: agl [-p <message> [--attach <path>]...] [--resume [<id>|last]] [--function <name>]
           [--think <level>] [--yes]
       agl init
       agl server start [--yes] [--function <name>]|stop|status [--function <name>]
       agl server command [--function <name>]
       agl function list|use <name> [--user]
       agl search login
       agl forge list|show|new|status|range|check|log|diff|rehome

Without -p, agl starts an interactive session.

Options:
  -p <message>        send one message, print the answer and exit
  --resume <id>       continue session <id>; `last` is the last one of this
                      project; with no id, it lists the latest ones
  --function <name>   use Function <name>
  --think <level>     the model's reasoning level: off, low, medium or xhigh
  --attach <path>     with -p: attach an image, a video or an audio; repeatable
  --yes               start or switch the server of the project without
                      asking when another server runs
  --version           print the version and the commit of this agl
  -h, --help          print this help

Project (agentLIBRE/<project> under XDG_DATA_HOME and its settings.nix):
  init                register the current directory as a project now; a
                      directory is registered at its first write otherwise

Server (systemd user unit agentLIBRE-server-<project>; the model of the
Function, the url of its kind in `server` of settings.nix, the project's over
the user's):
  server start        start the server and wait until it is ready; with
                      --function, the server of that Function's model
  server stop         stop it and the whisper server of the project
  server command      print the line that starts the server of a Function,
                      and the links to its files, for a server outside agl
  server status       show whether it runs and is ready (--function: the
                      server of that Function)

Functions (the workspace's functions/*.nix, then the user's, then the
built-ins; a Function's model names its server kind and weights):
  function list               the Functions, each with its kind and model;
                              * marks the one the runs here load
  function use <name>         the runs here load <name>: `function` in the
                              project's settings.nix (--user: the user's)

Search (`search` of the user's settings.nix):
  search login        a client certificate of the search from the step-ca of
                      `search.ca`, through a login in the browser; its files
                      go into `search`

Forge (the artifact store of the workspace, agentLIBRE/<workspace>/forge
under XDG_DATA_HOME):
  forge list                      the artifacts of the workspace
  forge show <name>               the rendered header and the body
  forge new task-spec --title <t> a new task-spec ([--name <n>])
  forge status <name> <state>     open, in-progress, done, backlog or canceled
  forge range <name> [--anchor <id>] [--last <id>]
                                  show or set the commits of a task
  forge check <name>              the structural checks and the verification cmds;
                                  it changes nothing
  forge log <name>                the commits of the artifact's file
  forge diff <name> [commit]      the diff of the artifact's file
  forge rehome [<store>] [--new]  move the workspace store",
        uk: "\
Використання: agl [-p <повідомлення> [--attach <шлях>]...] [--resume [<id>|last]] [--function <назва>]
              [--think <level>] [--yes]
              agl init
              agl server start [--yes] [--function <назва>]|stop|status [--function <назва>]
              agl server command [--function <назва>]
              agl function list|use <назва> [--user]
              agl search login
              agl forge list|show|new|status|range|check|log|diff|rehome

Без -p agl запускає інтерактивну сесію.

Параметри:
  -p <повідомлення>   надіслати одне повідомлення, вивести відповідь і вийти
  --resume <id>       продовжити сесію <id>; `last` це остання сесія цього
                      проєкту; без id показує список останніх
  --function <назва>  використати Function <назва>
  --think <level>     рівень міркувань моделі: off, low, medium або xhigh
  --attach <шлях>     разом з -p: прикріпити зображення, відео чи аудіо; можна
                      повторювати
  --yes               запустити чи перемкнути сервер проєкту без питання,
                      коли працює інший сервер
  --version           показати версію і коміт цього agl
  -h, --help          показати цю довідку

Проєкт (agentLIBRE/<проєкт> у XDG_DATA_HOME і його settings.nix):
  init                зареєструвати поточний каталог як проєкт зараз; інакше
                      каталог реєструється при першому записі

Сервер (user-unit systemd agentLIBRE-server-<проєкт>; модель Функції, url її
виду в `server` у settings.nix, проєкту поверх користувача):
  server start        запустити сервер і дочекатися готовності; з
                      --function — сервер моделі цієї Функції
  server stop         зупинити його і whisper-сервер проєкту
  server command      надрукувати рядок запуску сервера функції і посилання
                      на її файли, для сервера поза agl
  server status       показати, чи він працює і чи готовий (--function:
                      сервер цієї Функції)

Функції (functions/*.nix робочої області, потім користувача, потім
вбудовані; модель Функції називає вид її сервера і ваги):
  function list               Функції, кожна з видом і моделлю; * позначає
                              ту, яку тут завантажують запуски
  function use <назва>        запуски тут завантажують <назва>: `function` у
                              settings.nix проєкту (--user: користувача)

Пошук (`search` у settings.nix користувача):
  search login        клієнтський сертифікат пошуку від step-ca із
                      `search.ca` через вхід у браузері; його файли
                      записуються в `search`

Forge (сховище артефактів робочої області, agentLIBRE/<робоча область>/forge
у XDG_DATA_HOME):
  forge list                      артефакти робочої області
  forge show <назва>              відтворений заголовок і тіло
  forge new task-spec --title <t> новий task-spec ([--name <n>])
  forge status <назва> <стан>     open, in-progress, done, backlog або canceled
  forge range <назва> [--anchor <id>] [--last <id>]
                                  показати або задати коміти задачі
  forge check <назва>             структурні перевірки і команди verification;
                                  нічого не змінює
  forge log <назва>               коміти файлу артефакту
  forge diff <назва> [коміт]     розбіжності файлу артефакту
  forge rehome [<сховище>] [--new]   перемістити сховище робочої області",
    };

    /// `agl function list`: a Function of the workspace's `functions/`
    /// (AGL-036, D3).
    pub const FUNCTION_SOURCE_WORKSPACE: Msg = Msg {
        en: "workspace",
        uk: "воркспейс",
    };

    /// `agl function list`: a Function of the user's `functions/`.
    pub const FUNCTION_SOURCE_USER: Msg = Msg {
        en: "user",
        uk: "користувач",
    };

    /// `agl function list`: a built-in Function.
    pub const FUNCTION_SOURCE_BUILTIN: Msg = Msg {
        en: "built-in",
        uk: "вбудована",
    };

    /// `agl function list`: the server names no model.
    pub const FUNCTION_NO_MODEL: Msg = Msg {
        en: "no model",
        uk: "без моделі",
    };

    /// `agl function list`: the Function hides others of the same name.
    pub const FUNCTION_SHADOWS: Msg = Msg {
        en: "over {sources}",
        uk: "замість: {sources}",
    };

    /// `agl function use` wrote the key (AGL-036, D7).
    pub const FUNCTION_SET: Msg = Msg {
        en: "the runs load the Function {name} (function in {path})",
        uk: "запуски завантажують Function {name} (function у {path})",
    };

    /// `agl function use --user` while the project names its own.
    pub const FUNCTION_PROJECT_WINS: Msg = Msg {
        en: "in this project {path} names {name}, which wins over the user's file",
        uk: "у цьому проєкті {path} називає {name}, і він важливіший за файл користувача",
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

    /// `agl --resume` in a project with no session (AGL-058, D20).
    pub const NO_SESSIONS: Msg = Msg {
        en: "no sessions of this project",
        uk: "у цього проєкту немає сесій",
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
        en: "unsupported server address `{url}` in {path}; http:// and https:// are supported",
        uk: "непідтримувана адреса сервера `{url}` у {path}; підтримуються http:// і https://",
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

    /// `Error::SearchCaNotSet` (AGL-043 F20).
    pub const SEARCH_CA_NOT_SET: Msg = Msg {
        en: "agl search login needs `search.ca` with url, root and provisioner in {user_path}; `{key}` is not set:\n\nsearch.ca = {{ url = \"https://ca.example\"; root = \"/path/to/root.crt\"; provisioner = \"NAME\"; }};",
        uk: "agl search login потребує `search.ca` з url, root і provisioner у {user_path}; `{key}` не задано:\n\nsearch.ca = {{ url = \"https://ca.example\"; root = \"/path/to/root.crt\"; provisioner = \"NAME\"; }};",
    };

    /// `Error::SearchLogin` (AGL-043 F20).
    pub const SEARCH_LOGIN: Msg = Msg {
        en: "agl search login failed: {detail}",
        uk: "agl search login не вдався: {detail}",
    };

    /// `agl search login` is done (AGL-043 F20).
    pub const SEARCH_LOGIN_DONE: Msg = Msg {
        en: "the search certificate is {cert}, valid until {end}",
        uk: "сертифікат пошуку — {cert}, чинний до {end}",
    };

    /// A run whose search certificate has expired (AGL-043 F20).
    pub const SEARCH_CERT_EXPIRED: Msg = Msg {
        en: "the search certificate {cert} has expired: the search is off for this run (agl search login)",
        uk: "сертифікат пошуку {cert} прострочений: пошук у цьому запуску вимкнено (agl search login)",
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

    /// `Error::InvalidName`.
    pub const INVALID_NAME: Msg = Msg {
        en: "invalid name `{name}`: a name is not empty, has no `/` and does not start with `.`",
        uk: "некоректна назва `{name}`: назва непорожня, без `/` і не починається з `.`",
    };

    /// A template or Function problem at `key` of `path` (`Error::Template`,
    /// `Error::Function`).
    pub const PROBLEM_IN_PATH: Msg = Msg {
        en: "`{key}` in {path}: {problem}",
        uk: "`{key}` у {path}: {problem}",
    };

    /// `Error::FunctionNotFound`.
    pub const FUNCTION_NOT_FOUND: Msg = Msg {
        en: "Function `{name}` not found: no {paths} and no built-in Function of that name",
        uk: "Function `{name}` не знайдено: немає {paths} і вбудованої Function з такою назвою",
    };

    /// `Error::ServerUnreachable`.
    pub const SERVER_UNREACHABLE: Msg = Msg {
        en: "cannot reach {server} at {url}: {detail}\nTo start it: agl server start",
        uk: "не вдалося з'єднатися з {server} за адресою {url}: {detail}\nЩоб запустити: agl server start",
    };

    /// `Error::ServerStatus`.
    pub const SERVER_STATUS: Msg = Msg {
        en: "{server} at {url} answered {status}: {message}",
        uk: "{server} за адресою {url} відповів {status}: {message}",
    };

    /// `Error::ServerReply`.
    pub const SERVER_REPLY: Msg = Msg {
        en: "unexpected reply from {server} at {url}: {detail}",
        uk: "неочікувана відповідь: {server} за адресою {url}: {detail}",
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

    /// `Error::WeightsFileMissing` (AGL-043 F5).
    pub const WEIGHTS_FILE_MISSING: Msg = Msg {
        en: "{key}: no file {path}",
        uk: "{key}: немає файлу {path}",
    };

    /// `Error::MissingSpecType`.
    pub const MISSING_SPEC_TYPE: Msg = Msg {
        en: "the draft {draft} is set but the spec_type is not: set the spec_type too",
        uk: "draft {draft} задано, але spec_type ні: задано spec_type теж",
    };

    /// `Error::WslNotMirrored`.
    pub const WSL_NOT_MIRRORED: Msg = Msg {
        en: "agl in WSL reaches the Windows llama-server only in the mirrored network, and this one is {mode}: put `networkingMode=mirrored` under `[wsl2]` in %UserProfile%\\.wslconfig, then run `wsl --shutdown`",
        uk: "agl у WSL бачить llama-server на Windows лише в дзеркальній мережі, а ця мережа {mode}: додай `networkingMode=mirrored` у `[wsl2]` файлу %UserProfile%\\.wslconfig і виконай `wsl --shutdown`",
    };

    /// `Error::WslNoLocalAppData`.
    pub const WSL_NO_LOCAL_APP_DATA: Msg = Msg {
        en: "Windows does not say its %LOCALAPPDATA%, where the weights go; cmd.exe must run from WSL",
        uk: "Windows не повідомляє свій %LOCALAPPDATA%, куди йдуть ваги; cmd.exe має запускатися з WSL",
    };

    /// `Error::WslPath`.
    pub const WSL_PATH: Msg = Msg {
        en: "wslpath did not convert {path}",
        uk: "wslpath не перетворив {path}",
    };

    /// A server on Windows, left by a WSL that stopped, was ended (AGL-058, D24).
    pub const WINDOWS_SERVER_LEFT: Msg = Msg {
        en: "a llama-server left on Windows held port {port}: it is stopped",
        uk: "llama-server, що лишився на Windows, тримав порт {port}: його зупинено",
    };

    /// `Error::ServerCa` (AGL-058, D12).
    pub const SERVER_CA: Msg = Msg {
        en: "the CA file {path} of `private_ca` cannot be used: {detail}",
        uk: "файл CA {path} із `private_ca` не можна використати: {detail}",
    };

    /// `Error::ServerCertificate` (AGL-058, D12).
    pub const SERVER_CERTIFICATE: Msg = Msg {
        en: "the certificate of the server at {url} is not trusted: {detail}\nIf a private CA signs it, name its certificate in `server.llama.private_ca`",
        uk: "сертифікату сервера за {url} немає довіри: {detail}\nЯкщо його підписує приватний CA, вкажи сертифікат CA у `server.llama.private_ca`",
    };

    /// `Error::OutsideServerDown`.
    pub const OUTSIDE_SERVER_DOWN: Msg = Msg {
        en: "the server outside agl does not answer at {url}: start it there (`agl server command` prints the line), or check the address",
        uk: "сервер поза agl не відповідає за {url}: запусти його там (рядок друкує `agl server command`) або перевір адресу",
    };

    /// `Error::OutsideModelDiffers`.
    pub const OUTSIDE_MODEL_DIFFERS: Msg = Msg {
        en: "the server at {url} serves {running}, and this Function needs {expected}: agl does not restart a server outside it",
        uk: "сервер за {url} обслуговує {running}, а цій функції потрібна {expected}: agl не перезапускає сервер поза собою",
    };

    /// `Error::ServerCommandNotLlama`.
    pub const SERVER_COMMAND_NOT_LLAMA: Msg = Msg {
        en: "the Function {function} does not run on llama-server, so there is no line to print",
        uk: "функція {function} працює не на llama-server, тож рядка для неї немає",
    };

    /// `agl server stop` with `server.llama.external` (AGL-052, D4).
    pub const SERVER_OUTSIDE_NOT_STOPPED: Msg = Msg {
        en: "the server at {url} runs outside agl, and agl does not stop it",
        uk: "сервер за {url} працює поза agl, і agl його не зупиняє",
    };

    /// `Error::NotLocalServer`.
    pub const NOT_LOCAL_SERVER: Msg = Msg {
        en: "agl starts the server only on 127.0.0.1 or localhost, not at {url}",
        uk: "agl запускає сервер лише на 127.0.0.1 або localhost, не за {url}",
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
        en: "{project} is not a registered project, so it has no server of its own; run `agl init` there first",
        uk: "{project} не є зареєстрованим проєктом, тож не має власного сервера; спершу виконай там `agl init`",
    };

    /// `Error::PortBusy` without a running unit (D7).
    pub const PORT_BUSY: Msg = Msg {
        en: "the address {url} is already in use by something that is not a unit of agl; free it or set another `server.llama.url` (or `server.strata.url`) in the project's settings.nix",
        uk: "адреса {url} уже зайнята чимось, що не є unit-ом agl; звільни її або задай інший `server.llama.url` (чи `server.strata.url`) у settings.nix проєкту",
    };

    /// `Error::PortBusy` with running units (D7).
    pub const PORT_BUSY_UNITS: Msg = Msg {
        en: "the address {url} is already in use; the server units that run: {units}. Stop the one that holds it (agl server stop) or set another `server.llama.url` (or `server.strata.url`) in the project's settings.nix",
        uk: "адреса {url} уже зайнята; працюють unit-и серверів: {units}. Зупини той, що її тримає (agl server stop), або задай інший `server.llama.url` (чи `server.strata.url`) у settings.nix проєкту",
    };

    /// `Error::ServerRunning` (D13).
    pub const SERVER_RUNNING: Msg = Msg {
        en: "a server of another project is running ({units}); to start one more, use --yes",
        uk: "працює сервер іншого проєкту ({units}); щоб запустити ще один, додай --yes",
    };

    /// `Error::StartDeclined` (D9).
    pub const START_DECLINED: Msg = Msg {
        en: "the server was not started",
        uk: "сервер не запущено",
    };

    /// `Error::RestartDeclined` (AGL-033, D2).
    pub const RESTART_DECLINED: Msg = Msg {
        en: "the server was not restarted",
        uk: "сервер не перезапущено",
    };

    /// The Strata server, as the texts name it (AGL-038, D1).
    pub const STRATA_SERVER: Msg = Msg {
        en: "the Strata server",
        uk: "сервер Strata",
    };

    /// `Error::SwitchDeclined` (AGL-036, D10).
    pub const SWITCH_DECLINED: Msg = Msg {
        en: "the server of the other kind was not stopped, so the new one was not started",
        uk: "сервер іншого виду не зупинено, тож новий не запущено",
    };

    /// `Error::StrataConfig` (AGL-033, D2).
    pub const STRATA_CONFIG: Msg = Msg {
        en: "the Strata config {path} cannot be read or parsed: {detail}",
        uk: "JSON-конфіг Strata {path} не можна прочитати або розібрати: {detail}",
    };

    /// `Error::StrataNoCwd` (AGL-038, D3, D7).
    pub const STRATA_NO_CWD: Msg = Msg {
        en: "the Strata config {path} names no cwd (the directory of the Strata checkout, where .venv and serve/server.py are)",
        uk: "конфіг Strata {path} не називає cwd (каталог checkout Strata, де лежать .venv і serve/server.py)",
    };

    /// `Error::StrataPython` (AGL-033, D2; AGL-038, D3).
    pub const STRATA_PYTHON: Msg = Msg {
        en: "no venv python at {path} (the .venv in the cwd of the Strata config)",
        uk: "немає venv python за {path} (.venv у cwd конфігу Strata)",
    };

    /// `Error::StrataServerPy` (AGL-033, D2; AGL-038, D3).
    pub const STRATA_SERVER_PY: Msg = Msg {
        en: "no serve/server.py at {path} (in the cwd of the Strata config)",
        uk: "немає serve/server.py за {path} (у cwd конфігу Strata)",
    };

    /// The question of D9: another server runs, start one more?
    pub const OTHERS_RUNNING: Msg = Msg {
        en: "A server of another project is running ({units}). Start one more? [y/N]",
        uk: "Уже працює сервер іншого проєкту ({units}). Запустити ще один? [y/N]",
    };

    /// `Error::WhisperNotSet` (AGL-014, D22).
    pub const WHISPER_NOT_SET: Msg = Msg {
        en: "an audio needs the whisper server, and `url` and `model` of `whisper` are not both set; add to {project_path} (this project) or {user_path} (the default of every project):\n\nwhisper = {{ url = \"http://127.0.0.1:18081\"; model = \"ggml-large-v3-q8_0.bin\"; }};",
        uk: "аудіо потребує whisper-сервера, а `url` і `model` у `whisper` задано не обидва; додай у {project_path} (цей проєкт) або {user_path} (типове для всіх проєктів):\n\nwhisper = {{ url = \"http://127.0.0.1:18081\"; model = \"ggml-large-v3-q8_0.bin\"; }};",
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
        en: "the server has no multimodal projector, so it cannot take an image; set `mmproj` in the model of the Function and restart it (agl server stop)",
        uk: "у сервера немає мультимодального проєктора, тож він не приймає зображень; задай `mmproj` у моделі Функції і перезапусти його (agl server stop)",
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

    /// `Error::ServerFailed` of a unit.
    pub const SERVER_FAILED: Msg = Msg {
        en: "the server stopped before it was ready; see journalctl --user -u {unit}",
        uk: "сервер зупинився, не встигнувши стати готовим; див. journalctl --user -u {unit}",
    };

    /// `Error::ServerTimeout` of a unit.
    pub const SERVER_TIMEOUT: Msg = Msg {
        en: "the server was not ready after {seconds} s; it keeps loading in the background (agl server status, journalctl --user -u {unit})",
        uk: "сервер не став готовим за {seconds} с; він вантажиться далі у фоні (agl server status, journalctl --user -u {unit})",
    };

    /// `Error::ServerFailed` of agl's child (AGL-024, D2).
    pub const ENGINE_DIED: Msg = Msg {
        en: "the engine stopped before it was ready; see {log}",
        uk: "движок зупинився, не встигнувши стати готовим; див. {log}",
    };

    /// `Error::ServerTimeout` of agl's child (AGL-024, D2).
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
        en: "a transition is `next.<tool>.ok`/`.fail`/`.timeout`/`.any` of a tool of the state, or `next.otherwise`",
        uk: "перехід — це `next.<tool>.ok`/`.fail`/`.timeout`/`.any` для tool цього стану або `next.otherwise`",
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
    /// The `model` of a Function of the llama kind has no `weights`
    /// (AGL-033, D1; AGL-039, D19).
    pub const FUNC_SERVER_MODEL_MISSING: Msg = Msg {
        en: "the model of the llama kind needs `weights`",
        uk: "модель llama-виду потребує `weights`",
    };
    /// The `model` of a Function of the strata kind has no
    /// `strata_config` (AGL-033, D1).
    pub const FUNC_SERVER_STRATA_CONFIG_MISSING: Msg = Msg {
        en: "the model of the strata kind needs `strata_config`",
        uk: "модель strata-виду потребує `strata_config`",
    };
    /// A path of the Function's `model` that must be absolute
    /// (`strata_config`).
    pub const FUNC_NOT_ABSOLUTE: Msg = Msg {
        en: "the path must be absolute",
        uk: "шлях має бути абсолютним",
    };

    pub const FUNC_BAD_WEIGHTS_NAME: Msg = Msg {
        en: "a weights file is a file name (looked for in the weights folders) or an absolute path",
        uk: "файл ваг — це ім'я файлу (шукається в теках ваг) або абсолютний шлях",
    };

    pub const FUNC_SOURCE_WITHOUT_FILE: Msg = Msg {
        en: "`source` names a file the model does not set",
        uk: "`source` називає файл, якого модель не задає",
    };

    /// A key of the Function's `model` that does not apply to its kind
    /// (AGL-033, D1; AGL-034, D4).
    pub const FUNC_SERVER_KEY_OF_OTHER_KIND: Msg = Msg {
        en: "{key} belongs to the {kind} kind, not to the model's `kind`",
        uk: "{key} належить до {kind}-виду, не до `kind` цієї моделі",
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
        en: "stopped: the server failed",
        uk: "зупинено: помилка сервера",
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

    /// The dock's state: the turn's start brings the server up
    /// (AGL-043 F2).
    pub const STARTING_SERVER: Msg = Msg {
        en: "Starting the server",
        uk: "Запуск сервера",
    };

    /// The dock's state: an audio of the message is transcribed.
    pub const TRANSCRIBING_ACTIVITY: Msg = Msg {
        en: "Transcribing",
        uk: "Розпізнавання",
    };

    /// The dock's state: a step's prompt is read, until its first token.
    pub const PREFILL: Msg = Msg {
        en: "Reading the prompt",
        uk: "Читання промпту",
    };

    /// The dock's state: the model thinks.
    pub const THINKING_ACTIVITY: Msg = Msg {
        en: "Thinking",
        uk: "Роздуми",
    };

    /// The dock's state: the model writes its action or answer.
    pub const WRITING: Msg = Msg {
        en: "Writing",
        uk: "Написання",
    };

    /// The dock's state: a stir of the stirred mode retries the step.
    pub const RETRY: Msg = Msg {
        en: "Retry {attempt}/{max}",
        uk: "Повтор {attempt}/{max}",
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

    /// `/function` (AGL-036, D3, D4, D9).
    pub const CMD_FUNCTION: Msg = Msg {
        en: "the Functions; /function <name> starts a new session with it",
        uk: "Функції; /function <назва> починає з нею нову сесію",
    };

    /// `/function <name>` leaves the session for a new one (AGL-036, D4).
    pub const FUNCTION_SWITCH: Msg = Msg {
        en: "the Function {name}: a new session",
        uk: "Function {name}: нова сесія",
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

    /// The tokens before and after a compaction, on its row (AGL-046, D13).
    pub const COMPACTED_TOKENS: Msg = Msg {
        en: "{before} → {after} tokens",
        uk: "{before} → {after} токенів",
    };

    /// The tokens after a compaction when those before it are not known.
    pub const COMPACTED_AFTER: Msg = Msg {
        en: "{after} tokens",
        uk: "{after} токенів",
    };

    /// The mark of a transition that must not happen (AGL-046, D11).
    pub const PHASE_VIOLATION: Msg = Msg {
        en: "[fsm: `{event}` must not happen in {from}]",
        uk: "[fsm: `{event}` не має ставатися в {from}]",
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

    /// `agl server start` begins (D146); `what` is the server, in a line
    /// (AGL-033, D2).
    pub const SERVER_STARTING: Msg = Msg {
        en: "starting {what} at {url} as unit {unit}…",
        uk: "запускаю {what} за {url} як unit {unit}…",
    };

    /// `agl server start` is ready (D149); `what` is the server, in a
    /// line (AGL-033, D2).
    pub const SERVER_READY: Msg = Msg {
        en: "the server is ready at {url}: {what}",
        uk: "сервер готов за {url}: {what}",
    };

    /// `agl server start` finds the server up (D146); `what` is the
    /// server, in a line (AGL-033, D2).
    pub const SERVER_ALREADY_RUNNING: Msg = Msg {
        en: "the server is already running at {url} ({what})",
        uk: "сервер уже працює за {url} ({what})",
    };

    /// The question of the mismatch of D2 (AGL-033): the running server
    /// does not serve the profile's effective settings; `what` is the
    /// server, in a line.
    pub const SERVER_RESTART_QUESTION: Msg = Msg {
        en: "the running server does not serve this profile ({what}). Restart it? [y/N]",
        uk: "сервер, що працює, не відповідає цьому профілю ({what}). Перезапустити? [y/N]",
    };

    /// The question of D10 (AGL-036): the server agl owns runs elsewhere —
    /// the other kind's — and holds the GPU; `old` and `url` are it, `what`
    /// the server of the run.
    pub const SERVER_SWITCH_QUESTION: Msg = Msg {
        en: "{old} runs at {url}; stop it to start {what}? [y/N]",
        uk: "{old} працює за {url}; зупинити його, щоб запустити {what}? [y/N]",
    };

    /// The server stopped: the one of the other kind (AGL-036, D10), or
    /// `agl server stop`'s (D146; AGL-038, D1).
    pub const SERVER_SWITCH_STOPPED: Msg = Msg {
        en: "{old} stopped",
        uk: "{old} зупинено",
    };

    /// The server agl owns, when no record says what it runs (AGL-036,
    /// D10).
    pub const SERVER_OF_THE_PROJECT: Msg = Msg {
        en: "the server of the project",
        uk: "сервер проєкту",
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

    /// `agl server status`: the model's `weights` are not set.
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

    /// `agl server status` (D150); `running` is the actually-running
    /// model from `/v1/models` (AGL-033, D5).
    pub const SERVER_STATUS_BLOCK: Msg = Msg {
        en: "unit: {unit} ({active})\nurl: {url}\nmodel: {model}\nrunning: {running}\nhealth: {health}",
        uk: "unit: {unit} ({active})\nадреса: {url}\nмодель: {model}\nпрацює: {running}\nстан: {health}",
    };

    /// `agl server status`: the server does not answer, so there is no
    /// running model (AGL-033, D5).
    pub const SERVER_RUNNING_NOT_SET: Msg = Msg {
        en: "not running",
        uk: "не працює",
    };

    /// `agl server status`: the running model deviates from the
    /// settings' model (AGL-033, D5).
    pub const SERVER_STATUS_MISMATCH: Msg = Msg {
        en: " (mismatch)",
        uk: " (незбігається)",
    };

    /// The question of a weights file found nowhere (AGL-039, D24c).
    pub const WEIGHTS_ASK: Msg = Msg {
        en: "{name} is in none of the weights folders. In which folder is it? (an absolute path; empty stops)",
        uk: "{name} немає в жодній теці ваг. У якій теці він? (абсолютний шлях; порожньо — зупинити)",
    };

    /// `FunctionProblem::BadSourceUrl` (AGL-050, D8).
    pub const FUNC_BAD_SOURCE_URL: Msg = Msg {
        en: "a source is an https url",
        uk: "джерело — це https-адреса",
    };

    /// `FunctionProblem::BadSourceSha256` (AGL-050, D8).
    pub const FUNC_BAD_SOURCE_SHA256: Msg = Msg {
        en: "a sha256 is 64 lowercase hex digits",
        uk: "sha256 — це 64 шістнадцяткові цифри в нижньому регістрі",
    };

    /// `FunctionProblem::BadSourceSize` (AGL-050, D8).
    pub const FUNC_BAD_SOURCE_SIZE: Msg = Msg {
        en: "a size is a positive number of bytes",
        uk: "розмір — це додатне число байтів",
    };

    /// The offer of the weights a Function needs (AGL-050, D9, D10).
    pub const DOWNLOAD_OFFER: Msg = Msg {
        en: "This Function needs files that are not here:\n{files}\nDownload {total} to {dir}? [y/N]",
        uk: "Для цієї Function бракує файлів:\n{files}\nЗавантажити {total} у {dir}? [y/N]",
    };

    /// The offer of the pinned version over a file of another size
    /// (AGL-050, D17).
    pub const DOWNLOAD_REPLACE: Msg = Msg {
        en: "{path} is not the version this Function names ({have}, it names {want}). Download that one and replace it? [y/N]",
        uk: "{path} не та версія, яку називає ця Function ({have}, а названо {want}). Завантажити її і замінити цей файл? [y/N]",
    };

    /// The start of a download (AGL-050).
    /// Before the sha256 of a downloaded file, which takes a while (AGL-058, D26).
    pub const DOWNLOAD_CHECK: Msg = Msg {
        en: "Checking {name}",
        uk: "Перевірка {name}",
    };

    pub const DOWNLOAD_START: Msg = Msg {
        en: "Downloading {name} ({size})",
        uk: "Завантажую {name} ({size})",
    };

    /// `Error::WeightsNotDownloaded` (AGL-050, D11).
    pub const WEIGHTS_NOT_DOWNLOADED: Msg = Msg {
        en: "{name} ({size}) is not downloaded: run `agl` in a terminal, it offers to download it",
        uk: "{name} ({size}) не завантажено: запусти `agl` у терміналі, він запропонує завантажити",
    };

    /// `Error::DownloadFailed` (AGL-050, D17).
    pub const DOWNLOAD_FAILED: Msg = Msg {
        en: "the download of {name} failed: {detail}",
        uk: "завантаження {name} не вдалось: {detail}",
    };

    /// `Error::DownloadNoSpace` (AGL-050, D17).
    pub const DOWNLOAD_NO_SPACE: Msg = Msg {
        en: "{name} needs {needed} in {dir}, and {available} is free",
        uk: "для {name} потрібно {needed} у {dir}, а вільно {available}",
    };

    /// `Error::DownloadMismatch` (AGL-050, D17).
    pub const DOWNLOAD_MISMATCH: Msg = Msg {
        en: "the sha256 of the downloaded {name} is {got}, not {expected}; it was not kept",
        uk: "sha256 завантаженого {name} — {got}, а не {expected}; файл не збережено",
    };

    /// `Error::WeightsNotFound` (AGL-039, D24c).
    pub const WEIGHTS_NOT_FOUND: Msg = Msg {
        en: "{name} is in none of the weights folders ({folders}); put it in one of them, add its folder to `models` of {user_path}, or name it by an absolute path",
        uk: "{name} немає в жодній теці ваг ({folders}); поклади його в одну з них, додай його теку до `models` у {user_path} або вкажи його абсолютним шляхом",
    };

    /// `Error::NixNotFound` (AGL-039, D38).
    pub const NIX_NOT_FOUND: Msg = Msg {
        en: "cannot run nix-instantiate: {source} (agl needs Nix)",
        uk: "не вдалося запустити nix-instantiate: {source} (agl потребує Nix)",
    };

    /// `Error::NixEval` (AGL-039).
    pub const NIX_EVAL: Msg = Msg {
        en: "{path} does not evaluate:\n{detail}",
        uk: "{path} не обчислюється:\n{detail}",
    };

    /// `Error::NixNotAFunction` (AGL-039, D8).
    pub const NIX_NOT_A_FUNCTION: Msg = Msg {
        en: "{path} is not a function of {{ agl, ... }}: write it as `{{ agl, ... }}: {{ … }}`",
        uk: "{path} не є функцією від {{ agl, ... }}: пиши його як `{{ agl, ... }}: {{ … }}`",
    };

    /// `Error::ToolsDirMissing` (AGL-040).
    pub const TOOLS_DIR_MISSING: Msg = Msg {
        en: "{var} is not set: run agl from its Nix package or `nix develop`",
        uk: "{var} не задано: запускай agl з його Nix-пакета або в `nix develop`",
    };

    /// `Error::ToolMissing` (AGL-040).
    pub const TOOL_MISSING: Msg = Msg {
        en: "{name} is not in {dir}",
        uk: "{name} немає в {dir}",
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

    /// `Error::MarkerProject`.
    pub const MARKER_PROJECT: Msg = Msg {
        en: "{path} names the project {project}, which is not an absolute path",
        uk: "{path} називає проєкт {project}, а це не абсолютний шлях",
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

    /// `Error::InsideDataDir`.
    pub const INSIDE_DATA_DIR: Msg = Msg {
        en: "{dir} is inside agl's data directory {data}, which is not a project: run agl from a project",
        uk: "{dir} лежить у каталозі даних agl {data}, а це не проєкт: запускай agl із проєкту",
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
        en: "{path} has schema {schema}, expected forge/v2",
        uk: "у {path} схема {schema}, очікується forge/v2",
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

    /// `Error::TerminalState` (D3 of AGL-032): the refusal points to the
    /// new-spec convention with a `supersedes` link.
    pub const TERMINAL_STATE: Msg = Msg {
        en: "{name} is {from}; {from} is a terminal state — to continue the work, make a new spec with `supersedes = [{name}]`",
        uk: "{name} — {from}; {from} — термінальний стан — щоб продовжити роботу, створіть нову спеку з `supersedes = [{name}]`",
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

    /// The broken range of a task-spec (`GitFlag::Broken`): its `anchor` is
    /// not an ancestor of its `last`.
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
        en: "adopted the store {old} as {new} (forge.auto_rehome = false; asks instead)",
        uk: "усиновлено сховище {old} як {new} (forge.auto_rehome = false; запитує натомість)",
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

    /// The range of a task-spec, printed by `forge range` and `forge status`
    /// (AGL-044).
    pub const FORGE_RANGE: Msg = Msg {
        en: "{name}: anchor {anchor}, last {last}",
        uk: "{name}: anchor {anchor}, last {last}",
    };

    /// A value of the range that is not set.
    pub const FORGE_RANGE_NOT_SET: Msg = Msg {
        en: "none",
        uk: "немає",
    };

    /// `forge status` changed the state.
    pub const FORGE_STATUS_CHANGED: Msg = Msg {
        en: "{name}: {from} to {to}",
        uk: "{name}: з {from} у {to}",
    };

    // The in-turn `forge` tool (AGL-008): the refusals are the
    // silly-mistakes protection.

    /// The forge tool: an action outside the eight.
    pub const FORGE_TOOL_UNKNOWN_ACTION: Msg = Msg {
        en: "unknown action {action}; the actions are list, show, new, edit, append, status, rename, remove",
        uk: "невідомий action {action}; action'и: list, show, new, edit, append, status, rename, remove",
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

    /// The forge tool: `edit` on a closed record (D4, AGL-032): the
    /// alive states (`open`, `in-progress`, `backlog`) take the edit.
    pub const FORGE_TOOL_EDIT_CLOSED: Msg = Msg {
        en: "{name} is {status}; edit is allowed while open, in-progress or backlog",
        uk: "{name} — {status}; edit дозволено, поки open, in-progress або backlog",
    };

    /// The forge tool: `edit` touches the append-only `## Progress`
    /// section (D4, AGL-032).
    pub const FORGE_TOOL_EDIT_PROGRESS: Msg = Msg {
        en: "the `## Progress` section of {name} is append-only; use the append action",
        uk: "секція `## Progress` у {name} — append-only; скористайтеся append",
    };

    /// The forge tool: `append` outside the working states (`open`,
    /// `in-progress`) (D4, AGL-032).
    pub const FORGE_TOOL_APPEND_NOT_ACTIVE: Msg = Msg {
        en: "{name} is {status}; append is allowed while open or in-progress",
        uk: "{name} — {status}; append дозволено, поки open або in-progress",
    };

    /// The forge tool: `remove` on an `in-progress` spec (D5, AGL-032):
    /// the path is `status canceled` first, then remove.
    pub const FORGE_TOOL_REMOVE_IN_PROGRESS: Msg = Msg {
        en: "{name} is in-progress; cancel it first (`status canceled`), then remove",
        uk: "{name} — in-progress; спочатку скасуйте (`status canceled`), потім remove",
    };

    /// The forge tool: `remove` on a `done` spec (D5, AGL-032): a
    /// finished task is a historical fact.
    pub const FORGE_TOOL_REMOVE_DONE: Msg = Msg {
        en: "{name} is done; a finished task is a historical fact and stays",
        uk: "{name} — done; завершена задача — історичний факт і залишається",
    };

    /// The forge tool: `rename`'s new name is not a name (D6, AGL-032):
    /// the slug charset — letters, digits and `-`, no leading or
    /// trailing `-`.
    pub const FORGE_TOOL_RENAME_INVALID_NAME: Msg = Msg {
        en: "{new} is not a name: letters, digits and `-`, no leading or trailing `-`",
        uk: "{new} — не ім'я: літери, цифри та `-`, без `-` на початку або в кінці",
    };

    /// The forge tool: `rename` to the name the artifact already has.
    pub const FORGE_TOOL_RENAME_SAME_NAME: Msg = Msg {
        en: "the name of {name} is already {new}",
        uk: "ім'я {name} вже {new}",
    };

    /// The forge tool: `append`'s section is not in the body.
    pub const FORGE_TOOL_NO_SECTION: Msg = Msg {
        en: "the body of {name} has no `## {section}` heading",
        uk: "у тілі {name} немає заголовка `## {section}`",
    };

    /// The forge tool: the append text is not one line (AGL-031).
    pub const FORGE_TOOL_APPEND_MULTI_LINE: Msg = Msg {
        en: "a bullet is one line: the append text of {name} has {n} newlines — split the bullet or describe the content indirectly; nothing was written",
        uk: "bullet — один рядок: текст append для {name} містить {n} переноси рядків — розбий bullet або опиши зміст непрямо; нічого не записано",
    };

    /// The forge tool: the append-only check failed (never expected).
    pub const FORGE_TOOL_APPEND_VIOLATION: Msg = Msg {
        en: "append to {name} would change more than the {section} bullet and updated; nothing was written",
        uk: "append до {name} змінив би більше, ніж bullet {section} і updated; нічого не записано",
    };

    // The agent's forge writes (AGL-008): the `[forge: …]` marker
    // line, muted in the REPL and on stderr in `-p`.

    /// A discarded generation (D154; AGL-043 F8): the tokens of the
    /// action and what it was heading for (`CUT_OFF_CALL`, `CUT_OFF_TEXT`).
    pub const CUT_OFF_EVENT: Msg = Msg {
        en: "[cut off: {tokens} tokens of {what}]",
        uk: "[cut off: {tokens} токенів, {what}]",
    };

    /// The same in the stirred mode, with the tokens of the thinking.
    pub const CUT_OFF_EVENT_THINKING: Msg = Msg {
        en: "[cut off: {thinking} tokens of thinking, {tokens} of {what}]",
        uk: "[cut off: {thinking} токенів думання, {tokens} — {what}]",
    };

    pub const CUT_OFF_CALL: Msg = Msg {
        en: "a tool call",
        uk: "виклик tool",
    };

    pub const CUT_OFF_TEXT: Msg = Msg {
        en: "text",
        uk: "текст",
    };

    /// A stir of the stirred mode (AGL-030, D4; AGL-043 F8): its number
    /// in the step and the parse error.
    pub const STIR_EVENT: Msg = Msg {
        en: "[stir {attempt}: {error}]",
        uk: "[stir {attempt}: {error}]",
    };

    /// The replay of a video medium (AGL-043 F17): its head and the
    /// count of its frames.
    pub const MEDIA_EVENT_FRAMES: Msg = Msg {
        en: "{head} — frames: {frames}",
        uk: "{head} — кадрів: {frames}",
    };

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

    /// The agent's `forge rename` renamed a task-spec (D6, AGL-032): the
    /// old name rides the section slot.
    pub const FORGE_EVENT_RENAMED: Msg = Msg {
        en: "[forge: {name} renamed from {old}]",
        uk: "[forge: {name} перейменовано з {old}]",
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
            Error::SearchCaNotSet { user_path, key } => tr!(
                lang,
                m::SEARCH_CA_NOT_SET,
                user_path = user_path.display(),
                key = key
            ),
            Error::SearchLogin { detail } => tr!(lang, m::SEARCH_LOGIN, detail = detail),
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
            Error::InvalidName(name) => tr!(lang, m::INVALID_NAME, name = name),
            Error::Template { path, key, problem } => {
                tr!(
                    lang,
                    m::PROBLEM_IN_PATH,
                    path = path.display(),
                    key = key,
                    problem = template_problem(problem, lang)
                )
            }
            Error::FunctionNotFound { name, paths } => {
                let paths = paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                tr!(
                    lang,
                    m::FUNCTION_NOT_FOUND,
                    name = name,
                    paths = &paths
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
            Error::ServerUnreachable { kind, url, detail } => tr!(
                lang,
                m::SERVER_UNREACHABLE,
                server = server::name(*kind, lang),
                url = url,
                detail = detail
            ),
            Error::WeightsFileMissing { key, path } => {
                tr!(lang, m::WEIGHTS_FILE_MISSING, path = path.display(), key = key)
            }
            Error::MissingSpecType { draft } => {
                tr!(lang, m::MISSING_SPEC_TYPE, draft = draft)
            }
            Error::NotLocalServer { url } => tr!(lang, m::NOT_LOCAL_SERVER, url = url),
            Error::ServerCa { path, detail } => {
                tr!(lang, m::SERVER_CA, path = path.display(), detail = detail)
            }
            Error::ServerCertificate { url, detail } => {
                tr!(lang, m::SERVER_CERTIFICATE, url = url, detail = detail)
            }
            Error::OutsideServerDown { url } => tr!(lang, m::OUTSIDE_SERVER_DOWN, url = url),
            Error::WslNotMirrored { mode } => tr!(lang, m::WSL_NOT_MIRRORED, mode = mode),
            Error::WslNoLocalAppData => tr!(lang, m::WSL_NO_LOCAL_APP_DATA).to_string(),
            Error::WslPath { path } => tr!(lang, m::WSL_PATH, path = path.display()),
            Error::OutsideModelDiffers { url, expected, running } => tr!(
                lang,
                m::OUTSIDE_MODEL_DIFFERS,
                url = url,
                expected = expected,
                running = running
            ),
            Error::ServerCommandNotLlama { function } => {
                tr!(lang, m::SERVER_COMMAND_NOT_LLAMA, function = function)
            }
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
            Error::Transcribe { url, detail } => {
                tr!(lang, m::TRANSCRIBE, url = url, detail = detail)
            }
            Error::MediaChanged { path } => tr!(lang, m::MEDIA_CHANGED, path = path.display()),
            Error::MediaKind { path } => tr!(lang, m::MEDIA_KIND, path = path.display()),
            Error::Ffmpeg { detail } => tr!(lang, m::FFMPEG, detail = detail),
            Error::ServerFailed { owner } => match owner {
                ServerOwner::Unit(unit) => tr!(lang, m::SERVER_FAILED, unit = unit),
                ServerOwner::Child(log) => tr!(lang, m::ENGINE_DIED, log = log.display()),
            },
            Error::ServerTimeout { seconds, owner } => match owner {
                ServerOwner::Unit(unit) => {
                    tr!(lang, m::SERVER_TIMEOUT, seconds = seconds, unit = unit)
                }
                ServerOwner::Child(log) => {
                    tr!(lang, m::ENGINE_TIMEOUT, seconds = seconds, log = log.display())
                }
            },
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
            Error::RestartDeclined => tr!(lang, m::RESTART_DECLINED).to_string(),
            Error::SwitchDeclined => tr!(lang, m::SWITCH_DECLINED).to_string(),
            Error::StrataConfig { path, detail } => {
                tr!(lang, m::STRATA_CONFIG, path = path.display(), detail = detail)
            }
            Error::StrataNoCwd { path } => {
                tr!(lang, m::STRATA_NO_CWD, path = path.display()).to_string()
            }
            Error::StrataPython { path } => {
                tr!(lang, m::STRATA_PYTHON, path = path.display()).to_string()
            }
            Error::StrataServerPy { path } => {
                tr!(lang, m::STRATA_SERVER_PY, path = path.display()).to_string()
            }
            Error::ServerStatus {
                kind,
                url,
                status,
                message,
            } => {
                tr!(
                    lang,
                    m::SERVER_STATUS,
                    server = server::name(*kind, lang),
                    url = url,
                    status = status,
                    message = message
                )
            }
            Error::ServerReply { kind, url, detail } => tr!(
                lang,
                m::SERVER_REPLY,
                server = server::name(*kind, lang),
                url = url,
                detail = detail
            ),
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
            Error::WeightsNotFound { name, folders } => {
                let folders: Vec<String> =
                    folders.iter().map(|folder| folder.display().to_string()).collect();
                let user_path = crate::paths::Paths::from_env(|name| std::env::var_os(name))
                    .map(|paths| paths.settings_file().display().to_string())
                    .unwrap_or_else(|_| crate::settings::SETTINGS_FILE.into());
                tr!(
                    lang,
                    m::WEIGHTS_NOT_FOUND,
                    name = name,
                    folders = folders.join(", "),
                    user_path = user_path
                )
            }
            Error::WeightsNotDownloaded { name, size } => tr!(
                lang,
                m::WEIGHTS_NOT_DOWNLOADED,
                name = name,
                size = crate::download::gigabytes(*size)
            ),
            Error::DownloadFailed { name, detail } => {
                tr!(lang, m::DOWNLOAD_FAILED, name = name, detail = detail)
            }
            Error::DownloadNoSpace { name, needed, available, dir } => tr!(
                lang,
                m::DOWNLOAD_NO_SPACE,
                name = name,
                needed = crate::download::gigabytes(*needed),
                dir = dir.display(),
                available = crate::download::gigabytes(*available)
            ),
            Error::DownloadMismatch { name, expected, got } => {
                tr!(lang, m::DOWNLOAD_MISMATCH, name = name, got = got, expected = expected)
            }
            Error::NixDirMissing => {
                tr!(lang, m::TOOLS_DIR_MISSING, var = crate::nix::NIX_VAR)
            }
            Error::FunctionsDirMissing => {
                tr!(lang, m::TOOLS_DIR_MISSING, var = crate::nix::FUNCTIONS_VAR)
            }
            Error::NixNotFound { source } => tr!(lang, m::NIX_NOT_FOUND, source = source),
            Error::NixEval { path, detail } => {
                tr!(lang, m::NIX_EVAL, path = path.display(), detail = detail)
            }
            Error::NixNotAFunction { path } => {
                tr!(lang, m::NIX_NOT_A_FUNCTION, path = path.display())
            }
            Error::ToolsDirMissing => {
                tr!(lang, m::TOOLS_DIR_MISSING, var = crate::programs::TOOLS_VAR)
            }
            Error::ToolMissing { name, dir } => {
                tr!(lang, m::TOOL_MISSING, name = name, dir = dir.display())
            }
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
            Error::MarkerProject { path, project } => {
                tr!(lang, m::MARKER_PROJECT, path = path.display(), project = project.display())
            }
            Error::MarkerIdentity { path, value } => {
                tr!(lang, m::MARKER_IDENTITY, path = path.display(), value = value)
            }
            Error::WorkspaceAmbiguous { dirs } => {
                tr!(lang, m::WORKSPACE_AMBIGUOUS, dirs = dirs.join(", "))
            }
            Error::InsideDataDir { dir, data } => {
                tr!(lang, m::INSIDE_DATA_DIR, dir = dir.display(), data = data.display())
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
            Error::TerminalState { name, from } => {
                tr!(lang, m::TERMINAL_STATE, name = name, from = from)
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
        P::ServerModelMissing => tr!(lang, m::FUNC_SERVER_MODEL_MISSING).to_string(),
        P::ServerStrataConfigMissing => {
            tr!(lang, m::FUNC_SERVER_STRATA_CONFIG_MISSING).to_string()
        }
        P::ServerKeyOfOtherKind(key) => {
            let kind = if key == "strata_config" { "strata" } else { "llama" };
            tr!(lang, m::FUNC_SERVER_KEY_OF_OTHER_KIND, key = key, kind = kind)
        }
        P::NotAbsolute => tr!(lang, m::FUNC_NOT_ABSOLUTE).to_string(),
        P::BadWeightsName => tr!(lang, m::FUNC_BAD_WEIGHTS_NAME).to_string(),
        P::SourceWithoutFile => tr!(lang, m::FUNC_SOURCE_WITHOUT_FILE).to_string(),
        P::BadSourceUrl => tr!(lang, m::FUNC_BAD_SOURCE_URL).to_string(),
        P::BadSourceSha256 => tr!(lang, m::FUNC_BAD_SOURCE_SHA256).to_string(),
        P::BadSourceSize => tr!(lang, m::FUNC_BAD_SOURCE_SIZE).to_string(),
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
    fn the_errors_of_a_backend_name_its_kind() {
        // AGL-038, D1: the Strata server is not called llama-server.
        let reply = |kind| Error::ServerReply {
            kind,
            url: "http://127.0.0.1:18088".into(),
            detail: "the engine stopped".into(),
        };
        let strata = reply(crate::settings::ServerKind::Strata);
        assert_eq!(
            strata.message(Lang::En),
            "unexpected reply from the Strata server at http://127.0.0.1:18088: the engine stopped"
        );
        assert!(strata.message(Lang::Uk).contains("сервер Strata"));
        // `Display` is the same text (AGL-058, D22).
        assert_eq!(strata.to_string(), strata.message(Lang::En));
        let llama = reply(crate::settings::ServerKind::Llama);
        assert!(llama.message(Lang::En).contains("from llama-server at"));
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
                paths: vec!["/p".into()]
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
    use crate::settings::ServerKind;

    /// `agl server start` begins (D146); `what` is the server, in a line
    /// (AGL-033, D2).
    pub fn starting(what: &str, url: &str, unit: &str, lang: Lang) -> String {
        tr!(lang, m::SERVER_STARTING, what = what, url = url, unit = unit)
    }

    /// `agl server start` finds the server up (D146); `what` is the
    /// server, in a line (AGL-033, D2).
    pub fn already_running(what: &str, url: &str, lang: Lang) -> String {
        tr!(lang, m::SERVER_ALREADY_RUNNING, what = what, url = url)
    }

    /// The question of the mismatch of D2 (AGL-033): the running server
    /// does not serve the profile's effective settings.
    pub fn restart_question(what: &str, lang: Lang) -> String {
        tr!(lang, m::SERVER_RESTART_QUESTION, what = what)
    }

    /// The server of `kind` (AGL-038, D1).
    pub fn name(kind: ServerKind, lang: Lang) -> &'static str {
        match kind {
            ServerKind::Llama => "llama-server",
            ServerKind::Strata => tr!(lang, m::STRATA_SERVER),
        }
    }

    /// The question of D10 (AGL-036): `old` at `url` stops for `what`.
    pub fn switch_question(old: &str, url: &str, what: &str, lang: Lang) -> String {
        tr!(lang, m::SERVER_SWITCH_QUESTION, old = old, url = url, what = what)
    }

    pub fn not_running(unit: &str, lang: Lang) -> String {
        tr!(lang, m::SERVER_NOT_RUNNING, unit = unit)
    }

    /// The question of D9 with the units that run.
    pub fn others_running(units: &[String], lang: Lang) -> String {
        tr!(lang, m::OTHERS_RUNNING, units = units.join(", "))
    }

    /// `agl server status` (D150); `running` is the actually-running
    /// model from `/v1/models` (AGL-033, D5), `None` when the server
    /// does not answer.
    pub fn status(
        unit: &str,
        active: bool,
        url: &str,
        model: Option<&str>,
        running: Option<&str>,
        health: Health,
        lang: Lang,
    ) -> String {
        let running = match (model, running) {
            // The running model deviates from the settings': the marker
            // makes it visible at a glance (AGL-033, D5).
            (Some(model), Some(running)) if model != running => {
                format!("{running}{}", tr!(lang, m::SERVER_STATUS_MISMATCH))
            }
            (_, Some(running)) => running.to_owned(),
            _ => tr!(lang, m::SERVER_RUNNING_NOT_SET).to_string(),
        };
        let model = model.unwrap_or_else(|| tr!(lang, m::SERVER_MODEL_NOT_SET));
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
            model = model,
            running = running,
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
            "/function" => tr!(lang, m::CMD_FUNCTION),
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
