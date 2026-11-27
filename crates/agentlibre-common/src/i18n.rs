//! Languages of CLI messages (D65-D68): the language, the message and its
//! rendering, and the catalog of texts (AGL-121, D78).

use std::ffi::OsString;

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

    /// The code of `language`, as the daemon's `hello` and `agl-notify`
    /// take it (AGL-121, D23, D29).
    pub fn code(&self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Uk => "uk",
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

    /// The pointer appended to usage errors.
    pub const USAGE_POINTER: Msg = Msg {
        en: "Run `agl --help` for usage.",
        uk: "Див. `agl --help`.",
    };

    /// `Error::FunctionWithResume`.
    pub const FUNCTION_WITH_RESUME: Msg = Msg {
        en: "`--function` cannot go with `--resume`: the session keeps its Function",
        uk: "`--function` не можна разом із `--resume`: сесія має свою Function",
    };

    /// `Error::PlaneWithResume`.
    pub const PLANE_WITH_RESUME: Msg = Msg {
        en: "`--plane` cannot go with `--resume`: the session keeps its plane",
        uk: "`--plane` не можна разом із `--resume`: сесія має свій plane",
    };
    use super::Msg;

    /// The word before the lines of usage of `--help` (AGL-061, D3); the
    /// lines after the first are indented by its width.
    pub const HELP_USAGE: Msg = Msg {
        en: "Usage: ",
        uk: "Використання: ",
    };

    /// The usage of a run of `agl`.
    pub const HELP_RUN_USAGE: Msg = Msg {
        en: "\
agl [-p <message> [--attach <path>]...] [--resume [<id>|last]] [--function <name>]
    [--plane <name>] [--think <level>] [--yes]",
        uk: "\
agl [-p <повідомлення> [--attach <шлях>]...] [--resume [<id>|last]] [--function <назва>]
[--plane <назва>] [--think <level>] [--yes]",
    };

    /// The options of a run of `agl`, in the help of the program.
    pub const HELP_RUN: Msg = Msg {
        en: "\
Without -p, agl starts an interactive session; its `/help` lists the
commands and the keys.

Options:
  -p <message>        send one message, print the answer and exit
  --resume <id>       continue session <id>; `last` is the last one of this
                      project; with no id, it lists the latest ones
  --function <name>   use Function <name>
  --plane <name>      work in the plane <name> (`agl plane --help`)
  --think <level>     the model's reasoning level: off, low, medium or xhigh
  --attach <path>     with -p: attach an image, a video or an audio; repeatable
  --yes               start or switch the server of the project without
                      asking when another server runs
  --version           print the version and the commit of this agl
  -h, --help          print this help",
        uk: "\
Без -p agl запускає інтерактивну сесію; її `/help` показує команди та
клавіші.

Параметри:
  -p <повідомлення>   надіслати одне повідомлення, вивести відповідь і вийти
  --resume <id>       продовжити сесію <id>; `last` це остання сесія цього
                      проєкту; без id показує список останніх
  --function <назва>  використати Function <назва>
  --plane <назва>     працювати в plane <назва> (`agl plane --help`)
  --think <level>     рівень міркувань моделі: off, low, medium або xhigh
  --attach <шлях>     разом з -p: прикріпити зображення, відео чи аудіо; можна
                      повторювати
  --yes               запустити чи перемкнути сервер проєкту без питання,
                      коли працює інший сервер
  --version           показати версію і коміт цього agl
  -h, --help          показати цю довідку",
    };

    /// The usage of `agl init`.
    pub const HELP_INIT_USAGE: Msg = Msg {
        en: "agl init",
        uk: "agl init",
    };

    /// The section of `agl init` in the help (AGL-061, D3).
    pub const HELP_INIT: Msg = Msg {
        en: "\
Project (agentLIBRE/<project> under XDG_DATA_HOME and its settings.nix):
  init                register the current directory as a project now; a
                      directory is registered at its first write otherwise",
        uk: "\
Проєкт (agentLIBRE/<проєкт> у XDG_DATA_HOME і його settings.nix):
  init                зареєструвати поточний каталог як проєкт зараз; інакше
                      каталог реєструється при першому записі",
    };

    /// The usage of `agl server`.
    pub const HELP_SERVER_USAGE: Msg = Msg {
        en: "\
agl server start [--yes] [--function <name>]|stop|status [--function <name>]
agl server command [--function <name>]",
        uk: "\
agl server start [--yes] [--function <назва>]|stop|status [--function <назва>]
agl server command [--function <назва>]",
    };

    /// The section of `agl server` in the help (AGL-061, D3).
    pub const HELP_SERVER: Msg = Msg {
        en: "\
Server (systemd user unit agl-engine-<id of the model>; the model of the
Function, the url of its kind in `server` of settings.nix, the project's over
the user's):
  server start        start the server and wait until it is ready; with
                      --function, the server of that Function's model
  server stop         stop it and the whisper server of the project
  server command      print the line that starts the server of a Function,
                      and the links to its files, for a server outside agl
  server status       show whether it runs and is ready (--function: the
                      server of that Function)",
        uk: "\
Сервер (user-unit systemd agl-engine-<id моделі>; модель Функції, url її
виду в `server` у settings.nix, проєкту поверх користувача):
  server start        запустити сервер і дочекатися готовності; з
                      --function — сервер моделі цієї Функції
  server stop         зупинити його і whisper-сервер проєкту
  server command      надрукувати рядок запуску сервера функції і посилання
                      на її файли, для сервера поза agl
  server status       показати, чи він працює і чи готовий (--function:
                      сервер цієї Функції)",
    };

    /// The usage of `agl function`.
    pub const HELP_FUNCTION_USAGE: Msg = Msg {
        en: "agl function list|use <name> [--user]|show <name>|new <name> [from]|check <name|file> [--plane <name>]",
        uk: "agl function list|use <назва> [--user]|show <назва>|new <назва> [зразок]|check <назва|файл> [--plane <назва>]",
    };

    /// The section of `agl function` in the help (AGL-061, D3).
    pub const HELP_FUNCTION: Msg = Msg {
        en: "\
Functions (the workspace's functions/*.nix, then the user's, then the
built-ins; a Function's model names its server kind and weights):
  function list               the Functions, each with its kind and model;
                              * marks the one the runs here load
  function use <name>         the runs here load <name>: `function` in the
                              project's settings.nix (--user: the user's)
  function show <name>        the source of <name> and the file it loads from
  function new <name> [from]  a copy of <name> — of `from` when given —
                              into the workspace's functions/
  function check <name|file>  whether it loads, and what runs: the kind,
                              the model, the context; for a file, the
                              editor's own bytes, not the saved name",
        uk: "\
Функції (functions/*.nix робочої області, потім користувача, потім
вбудовані; модель Функції називає вид її сервера і ваги):
  function list               Функції, кожна з видом і моделлю; * позначає
                              ту, яку тут завантажують запуски
  function use <назва>        запуски тут завантажують <назва>: `function` у
                              settings.nix проєкту (--user: користувача)
  function show <назва>       код <назва> і файл, з якого її завантажують
  function new <назва> [зраз] копію <назва> — або <зраз>, якщо вказано — у
                              functions/ робочої області
  function check <назва|файл> чи завантажується і що запускається: вид,
                              модель, контекст; для файла — його власні
                              байти, не збережена назва",
    };

    /// The usage of `agl search`.
    pub const HELP_SEARCH_USAGE: Msg = Msg {
        en: "agl search login",
        uk: "agl search login",
    };

    /// The section of `agl search` in the help (AGL-061, D3).
    pub const HELP_SEARCH: Msg = Msg {
        en: "\
Search (`search` of the user's settings.nix):
  search login        a client certificate of the search from the step-ca of
                      `search.ca`, through a login in the browser; its files
                      go into `search`",
        uk: "\
Пошук (`search` у settings.nix користувача):
  search login        клієнтський сертифікат пошуку від step-ca із
                      `search.ca` через вхід у браузері; його файли
                      записуються в `search`",
    };

    /// The usage of `agl memory`.
    pub const HELP_MEMORY_USAGE: Msg = Msg {
        en: "agl memory list|show <name>|remove <name> [--yes] [--plane <name>]",
        uk: "agl memory list|show <назва>|remove <назва> [--yes] [--plane <назва>]",
    };

    /// The usage of `agl plane` (AGL-077, D11).
    pub const HELP_PLANE_USAGE: Msg = Msg {
        en: "agl plane new <name> [--empty]|list|show [<name>]|rename <old> <new>\n\
             agl plane add|remove <name> [<dir>]|delete <name> [--yes]\n\
             agl plane flow list|status|start|stop|reset [<flow>]",
        uk: "agl plane new <назва> [--empty]|list|show [<назва>]|rename <стара> <нова>\n\
             agl plane add|remove <назва> [<тека>]|delete <назва> [--yes]\n\
             agl plane flow list|status|start|stop|reset [<flow>]",
    };

    /// The section of `agl plane` in the help (AGL-077, D11).
    pub const HELP_PLANE: Msg = Msg {
        en: "\
Planes (where a project is worked on: its sessions, its forge, memory,
settings and Functions, and any directories; `planes/<name>/` of the data):
  plane new <name> [--empty]    make a plane with the current directory, or none
  plane list                    the planes; `*` marks those of the current directory
  plane show [<name>]           its store, id, directories and sessions; with no
                                name, the planes of the current directory
  plane rename <old> <new>      a new name; the id and the sessions stay
  plane add <name> [<dir>]      add a directory (the current one when not named)
  plane remove <name> [<dir>]   take a directory out; its files are not touched
  plane delete <name> [--yes]   remove the plane's store; it asks first; the
                                sessions keep their files
  plane flow list               the plane's flows and where each stands
  plane flow start <flow>       go on with the flow; `stop` leaves it where it
                                stood, `status` answers from its journal,
                                `reset` begins again and keeps the old journal
  agl --plane <name>            a run in the plane",
        uk: "\
Plane (де йде робота над проєктом: його сесії, forge, пам'ять, налаштування
і Functions, і будь-які теки; `planes/<назва>/` у даних):
  plane new <назва> [--empty]    створити plane з поточною текою або без тек
  plane list                     усі plane; `*` позначає plane поточної теки
  plane show [<назва>]           сховище, id, теки і сесії; без назви — plane
                                 поточної теки
  plane rename <стара> <нова>    нова назва; id і сесії лишаються
  plane add <назва> [<тека>]     додати теку (поточну, коли не названо)
  plane remove <назва> [<тека>]  прибрати теку з plane; її файли не чіпає
  plane delete <назва> [--yes]   видалити сховище plane; спершу питає; файли
                                 сесій лишаються
  plane flow list                flows plane та де кожен стоїть
  plane flow start <flow>        рухатися flow; `stop` лишає його на місці,
                                 `status` відповідає з журналу, `reset`
                                 починає знову й ховає старий журнал
  agl --plane <назва>            запуск у plane",
    };

    pub const PLANE_MADE: Msg = Msg {
        en: "made the plane `{name}`: {dir}",
        uk: "створено plane `{name}`: {dir}",
    };

    pub const PLANE_RENAMED: Msg = Msg {
        en: "the plane `{old}` is `{new}` now: {dir}",
        uk: "plane `{old}` тепер `{new}`: {dir}",
    };

    pub const PLANE_ADDED: Msg = Msg {
        en: "{directory} is in the plane `{name}`",
        uk: "{directory} тепер у plane `{name}`",
    };

    pub const PLANE_HAD: Msg = Msg {
        en: "{directory} was in the plane `{name}` already",
        uk: "{directory} вже був у plane `{name}`",
    };

    pub const PLANE_TAKEN_OUT: Msg = Msg {
        en: "{directory} is no longer in the plane `{name}`; its files are as they were",
        uk: "{directory} більше не в plane `{name}`; його файли не змінено",
    };

    pub const PLANE_DELETE_ASK: Msg = Msg {
        en: "delete the plane `{name}` and its store {dir} (its forge, memory, settings and Functions)? [y/N]",
        uk: "видалити plane `{name}` і його сховище {dir} (forge, пам'ять, налаштування і Functions)? [y/N]",
    };

    pub const PLANE_DELETED: Msg = Msg {
        en: "deleted the plane `{name}`; its sessions keep their files",
        uk: "plane `{name}` видалено; файли його сесій лишилися",
    };

    pub const PLANE_KEPT: Msg = Msg {
        en: "the plane `{name}` is kept",
        uk: "plane `{name}` лишається",
    };

    pub const PLANE_NONE: Msg = Msg {
        en: "no plane; `agl plane new <name>` makes one",
        uk: "жодного plane; `agl plane new <назва>` створить",
    };

    /// A flow of a session that is in no plane (AGL-118, D1).
    pub const FLOW_NO_PLANE: Msg = Msg {
        en: "flows live in a plane; this session is in none",
        uk: "flows живуть у plane; ця сесія — у жодному",
    };

    /// A plane with no `flow.nix`, or one that declares nothing (AGL-118, D1).
    pub const FLOW_NONE: Msg = Msg {
        en: "no flows; `{file}` declares none",
        uk: "жодних flows; `{file}` нічого не оголошує",
    };

    /// A flow the plane does not declare (AGL-118, D2).
    pub const FLOW_UNKNOWN: Msg = Msg {
        en: "no flow `{name}` in `{file}`",
        uk: "немає flow `{name}` у `{file}`",
    };

    /// A flow whose journal has no lines yet (AGL-118, D3).
    pub const FLOW_NOT_STARTED: Msg = Msg {
        en: "{name}: not started",
        uk: "{name}: не стартував",
    };

    /// A flow and the state it stands in (AGL-118, D7).
    pub const FLOW_AT: Msg = Msg {
        en: "{name}: {state}",
        uk: "{name}: {state}",
    };

    /// The mark of a flow a human stopped (AGL-118, D7).
    pub const FLOW_STOPPED: Msg = Msg {
        en: "stopped",
        uk: "зупинено",
    };

    /// What a flow's state waits for from a session (AGL-118, D6).
    pub const FLOW_WAITS: Msg = Msg {
        en: "waits for",
        uk: "чекає на",
    };

    /// A driver error dropped and kept for the human (AGL-121, D37).
    pub const FLOW_WARN: Msg = Msg {
        en: "warning: {text}",
        uk: "попередження: {text}",
    };

    /// The flow reset: the old journal kept beside the new one (AGL-118, D11).
    pub const FLOW_RESET: Msg = Msg {
        en: "{name}: reset to `{state}`, the old journal is `{file}`",
        uk: "{name}: скинуто до `{state}`, старий журнал — `{file}`",
    };

    pub const PLANE_NONE_HERE: Msg = Msg {
        en: "the current directory is in no plane",
        uk: "поточна тека не входить у жоден plane",
    };

    pub const PLANE_SHOW: Msg = Msg {
        en: "plane: {name}\nid: {id}\nstore: {dir}\nsessions: {sessions}\ndirectories:",
        uk: "plane: {name}\nid: {id}\nсховище: {dir}\nсесій: {sessions}\nтеки:",
    };

    /// The section of `agl memory` in the help (AGL-061, D3).
    pub const HELP_MEMORY: Msg = Msg {
        en: "\
Memory (the entries of the project, `memory/` of its store):
  memory list                   the entries, with their titles and hooks; marks
                                what the prompt does not carry and what is out
                                of step with MEMORY.md
  memory show <name>            the whole text of an entry
  memory remove <name> [--yes]  delete an entry and its line; it asks first,
                                as nothing can bring it back",
        uk: "\
Пам'ять (записи проєкту, `memory/` у його сховищі):
  memory list                    записи, з назвами і підказками; позначає те, чого
                                 немає в промпті, і розбіжності з MEMORY.md
  memory show <назва>            увесь текст запису
  memory remove <назва> [--yes]  видалити запис і його рядок; спершу питає, бо
                                 нічим його не повернути",
    };

    /// The usage of `agl send` (AGL-125, D5).
    pub const HELP_SEND_USAGE: Msg = Msg {
        en: "agl send <id|name|last> <message>",
        uk: "agl send <id|назва|last> <повідомлення>",
    };

    /// The section of `agl send` in the help (AGL-125, D5).
    pub const HELP_SEND: Msg = Msg {
        en: "\
Sending (a message into a session, which lives on its own):
  send <id|name|last> <message>  post the message into the session and exit;
                                 its turn runs in the daemon, which loads the
                                 session and starts the server of its model
                                 when they are not up; the answer is in the
                                 session for whoever opens it next
  agl -p <message> --resume <id|name|last>
                                 post it and wait for the answer",
        uk: "\
Надсилання (повідомлення в сесію, яка живе сама):
  send <id|назва|last> <повідомлення>
                                 надіслати повідомлення в сесію й вийти; хід
                                 виконує демон: завантажує сесію та запускає
                                 сервер її моделі, якщо вони не працюють;
                                 відповідь чекає в сесії того, хто її відкриє
  agl -p <повідомлення> --resume <id|назва|last>
                                 надіслати й дочекатися відповіді",
    };

    /// The usage of `agl daemon` (AGL-076, D6).
    pub const HELP_DAEMON_USAGE: Msg = Msg {
        en: "agl daemon",
        uk: "agl daemon",
    };

    /// The section of `agl daemon` in the help (AGL-076, D6).
    pub const HELP_DAEMON: Msg = Msg {
        en: "\
The daemon (the planes of the sessions, served on one socket):
  daemon   holds the planes its clients open and serves them on
           $XDG_RUNTIME_DIR/agentLIBRE/agl.sock; agl starts it when none
           runs. It runs until stopped, as a user unit does.",
        uk: "\
Демон (plane сесій, на одному сокеті):
  daemon   тримає plane, які відкривають його клієнти, і обслуговує їх на
           $XDG_RUNTIME_DIR/agentLIBRE/agl.sock; agl запускає його, коли
           жодного немає. Працює, доки його не зупинять, як юніт.",
    };

    pub const HELP_NOTIFY_USAGE: Msg = Msg {
        en: "agl notify",
        uk: "agl notify",
    };

    /// The section of `agl notify` in the help (AGL-074, D2).
    pub const HELP_NOTIFY: Msg = Msg {
        en: "\
Notifications (the desktop's, for every running session):
  notify   tells what each session of the daemon does, a card a session:
           thinking, a tool, writing the answer, with Stop; at a turn's end
           the answer's beginning, a field to answer in, Terminal and
           Session; a stop as a critical card answered from it (the field,
           Continue, Terminal, Session), a cut-off as a critical card.
           It runs until stopped, as a user unit does.",
        uk: "\
Сповіщення (стільниці, для кожної запущеної сесії):
  notify   каже, що робить кожна сесія демона, картка на сесію:
           думає, інструмент, пише відповідь, з кнопкою Зупинити; в кінці
           ходу початок відповіді, поле для відповіді, Термінал і Сесія;
           зупинка як критична картка з полем, Продовжити, Терміналом і
           Сесією, обрізання як критична картка.
           Працює, доки його не зупинять, як user unit.",
    };

    /// The usage of `agl bridge` (AGL-083, D19).
    pub const HELP_BRIDGE_USAGE: Msg = Msg {
        en: "\
agl bridge start|stop|status
agl bridge pair <name>|revoke <name>|list",
        uk: "\
agl bridge start|stop|status
agl bridge pair <назва>|revoke <назва>|list",
    };

    /// The section of `agl bridge` in the help (AGL-083, D10, D18 to D21).
    pub const HELP_BRIDGE: Msg = Msg {
        en: "\
The bridge (the daemon for the iPhone and the watch; systemd user unit
agentLIBRE-bridge; `bridge.address` and `bridge.port` of the user's
settings.nix, reached through WireGuard; TLS with its own certificate, a token
a device):
  bridge start          start the daemon if none runs, then the bridge
  bridge stop           stop the bridge
  bridge status         show whether it runs, and how many devices are paired
  bridge pair <name>    pair a device: a QR code to scan with the app, which
                        holds the address, the device's token and the
                        certificate's sha256; the token is shown this once
  bridge revoke <name>  unpair a device: its token opens nothing any more
  bridge list           the paired devices",
        uk: "\
Міст (демон для iPhone і годинника; user-unit systemd agentLIBRE-bridge;
`bridge.address` і `bridge.port` у settings.nix користувача, доступ через
WireGuard; TLS з власним сертифікатом, токен на пристрій):
  bridge start          запустити демон, якщо жоден не працює, потім міст
  bridge stop           зупинити міст
  bridge status         показати, чи він працює і скільки пристроїв спарено
  bridge pair <назва>   спарити пристрій: QR-код для застосунку з адресою,
                        токеном пристрою і sha256 сертифіката; токен
                        показано лише цей раз
  bridge revoke <назва> розірвати пару: токен пристрою більше нічого не
                        відкриває
  bridge list           спарені пристрої",
    };

    /// `agl bridge start`, the bridge started (AGL-083, D18).
    pub const BRIDGE_STARTED: Msg = Msg {
        en: "the bridge listens at {url}",
        uk: "міст слухає за {url}",
    };

    /// `agl bridge start` of a bridge that runs.
    pub const BRIDGE_ALREADY_RUNNING: Msg = Msg {
        en: "the bridge already runs ({url})",
        uk: "міст уже працює ({url})",
    };

    /// `agl bridge stop`.
    pub const BRIDGE_STOPPED: Msg = Msg {
        en: "the bridge is stopped",
        uk: "міст зупинено",
    };

    /// `agl bridge stop` of a bridge that does not run.
    pub const BRIDGE_NOT_RUNNING: Msg = Msg {
        en: "the bridge does not run",
        uk: "міст не працює",
    };

    /// `agl bridge status` of a bridge that runs.
    pub const BRIDGE_STATUS_RUNNING: Msg = Msg {
        en: "the bridge runs at {url}; paired devices: {devices}",
        uk: "міст працює за {url}; спарених пристроїв: {devices}",
    };

    /// `agl bridge status` of a bridge that does not run.
    pub const BRIDGE_STATUS_STOPPED: Msg = Msg {
        en: "the bridge does not run; paired devices: {devices}",
        uk: "міст не працює; спарених пристроїв: {devices}",
    };

    /// `agl bridge pair <name>`, under the QR code (AGL-083, D10, D21).
    pub const BRIDGE_PAIRED: Msg = Msg {
        en: "{name} is paired: scan the code with the app on the iPhone. It holds the device's token, shown this once; the bridge at {url} keeps only its sha256.",
        uk: "{name} спарено: відскануй код застосунком на iPhone. У ньому токен пристрою, показаний лише цей раз; міст за {url} зберігає тільки його sha256.",
    };

    /// `agl bridge revoke <name>`.
    pub const BRIDGE_REVOKED: Msg = Msg {
        en: "{name} is unpaired: its token opens nothing any more",
        uk: "пару з {name} розірвано: його токен більше нічого не відкриває",
    };

    /// `agl bridge list` with no device.
    pub const BRIDGE_NO_DEVICES: Msg = Msg {
        en: "no device is paired (agl bridge pair <name>)",
        uk: "жодного пристрою не спарено (agl bridge pair <назва>)",
    };

    pub const HELP_SESSION_USAGE: Msg = Msg {
        en: "agl session show <id|name|last> [--verbose] [--plane <name>]",
        uk: "agl session show <id|назва|last> [--verbose] [--plane <назва>]",
    };

    /// The section of `agl session` in the help (AGL-063, D3).
    pub const HELP_SESSION: Msg = Msg {
        en: "\
Sessions (the files of `sessions/`, per project):
  session show <id|name|last>   the transcript of a session: its header and
                                the events in the order of the file; `last` is
                                the newest of the project, a name is looked up
                                in the project first, an id or the start of one
                                reaches any project
  --verbose                     the lines the transcript skips: the observer's
                                and the header's",
        uk: "\
Сесії (файли `sessions/`, за проєктом):
  session show <id|назва|last>   транскрипт сесії: її заголовок і події в
                                 порядку файлу; `last` — остання проєкту, назву
                                 шукає спершу в проєкті, id або початок id
                                 досягає будь-якого проєкту
  --verbose                      рядки, які транскрипт пропускає: спостерігача
                                 і заголовкові",
    };

    /// The usage of `agl forge`.
    pub const HELP_FORGE_USAGE: Msg = Msg {
        en: "agl forge list|show|new|status|range|check|log|diff|rehome [--plane <name>]",
        uk: "agl forge list|show|new|status|range|check|log|diff|rehome [--plane <назва>]",
    };

    /// The section of `agl forge` in the help (AGL-061, D3).
    pub const HELP_FORGE: Msg = Msg {
        en: "\
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
    pub const FUNCTION_SOURCE_PLANE: Msg = Msg {
        en: "plane",
        uk: "plane",
    };

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

    /// `agl --resume` in a project with no session (AGL-058, D20).
    pub const NO_SESSIONS: Msg = Msg {
        en: "no sessions of this project",
        uk: "у цього проєкту немає сесій",
    };

    /// A resumed session whose plane's store is gone (AGL-077, D14).
    pub const PLANE_GONE_LOOSE: Msg = Msg {
        en: "the plane `{name}` of this session is gone; the session goes on in no plane",
        uk: "plane `{name}` цієї сесії вже немає; сесія триває без plane",
    };

    /// `Error::Read`.
    pub const READ: Msg = Msg {
        en: "cannot read {path}: {source}",
        uk: "не вдалося прочитати {path}: {source}",
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

    /// `Error::InvalidName`.
    pub const INVALID_NAME: Msg = Msg {
        en: "invalid name `{name}`: a name is not empty, has no `/` and does not start with `.`",
        uk: "некоректна назва `{name}`: назва непорожня, без `/` і не починається з `.`",
    };

    /// A template or Function problem at `key` of `path` (`format::Error`,
    /// `function::error::Error::Invalid`).
    pub const PROBLEM_IN_PATH: Msg = Msg {
        en: "`{key}` in {path}: {problem}",
        uk: "`{key}` у {path}: {problem}",
    };

    /// `agl function new` copied a Function (AGL-102, D3).
    pub const FUNCTION_NEW: Msg = Msg {
        en: "Function {name}: a copy of {source} in {path}",
        uk: "Function {name}: копія {source} у {path}",
    };

    /// `agl function check` loaded a Function (AGL-109, D5).
    pub const FUNCTION_CHECK: Msg = Msg {
        en: "{name}: loads, {kind}, {model}, {context} of context",
        uk: "{name}: завантажується, {kind}, {model}, {context} контексту",
    };

    /// `/function edit`, whose line editor is gone (AGL-109, D2).
    pub const FUNCTION_EDIT_GONE: Msg = Msg {
        en: "the line editor is gone: Ctrl-F picks a Function and opens it in $EDITOR",
        uk: "построчний редактор пішов: Ctrl-F вибере Функцію і відкриє її в $EDITOR",
    };

    /// `/function show` without a name (AGL-102, D2).
    pub const FUNCTION_SHOW_USAGE: Msg = Msg {
        en: "usage: /function show <name>",
        uk: "використання: /function show <назва>",
    };

    /// `/function new` without a name (AGL-102, D3).
    pub const FUNCTION_NEW_USAGE: Msg = Msg {
        en: "usage: /function new <name> [from]",
        uk: "використання: /function new <назва> [зразок]",
    };

    /// The Function browser on Ctrl-F: the line of keys over the list
    /// (AGL-109, D1).
    pub const BROWSER_KEYS: Msg = Msg {
        en: "↑↓ pick · Enter source · e edit · c clone · s switch · d delete · Tab settings · Esc leave",
        uk: "↑↓ вибір · Enter код · e редагувати · c копія · s перемкнути · d видалити · Tab налаштування · Esc вийти",
    };

    /// The browser's `d`: the row asks the second press (AGL-109, D3).
    pub const BROWSER_DELETE_ASK: Msg = Msg {
        en: "delete {name}? d erases the file, any other key keeps it",
        uk: "видалити {name}? d зітре файл, будь-яка інша клавіша лишить його",
    };

    /// The browser's `d` erased the file (AGL-109, D3).
    pub const BROWSER_DELETED: Msg = Msg {
        en: "deleted {name}",
        uk: "видалено {name}",
    };

    /// The browser's `d` on a file it may not erase (AGL-109, D3).
    pub const BROWSER_DELETE_BUILTIN: Msg = Msg {
        en: "`{name}` belongs to the repository: nothing to delete here",
        uk: "`{name}` належить репозиторію: тут нема чого видаляти",
    };

    /// The line of keys over the settings' list (AGL-109, D6).
    pub const BROWSER_SETTINGS_KEYS: Msg = Msg {
        en: "↑↓ pick · Enter source · e edit · Tab functions · Esc leave",
        uk: "↑↓ вибір · Enter код · e редагувати · Tab функції · Esc вийти",
    };

    /// The note of the user's `settings.nix` (AGL-109, D6): its
    /// `server.context` beats every Function's.
    pub const BROWSER_SETTINGS_USER: Msg = Msg {
        en: "the user's settings: `server.context` here beats every Function",
        uk: "налаштування користувача: `server.context` тут перебиває будь-яку Функцію",
    };

    /// The note of the workspace's `settings.nix` (AGL-109, D6).
    pub const BROWSER_SETTINGS_PROJECT: Msg = Msg {
        en: "this workspace's settings: they beat the user's",
        uk: "налаштування цієї робочої області: вони перебивають користувачеві",
    };

    /// The browser's `e` on a settings file that is not there (AGL-109, D6).
    pub const BROWSER_SETTINGS_MISSING: Msg = Msg {
        en: "{path} is not there: create it, and it joins the settings",
        uk: "{path} немає: створи його, і воно станеться з налаштуваннями",
    };

    /// What the check of a settings file says after the editor (AGL-109, D6).
    pub const BROWSER_SETTINGS_SAVED: Msg = Msg {
        en: "saved {name}: it loads with these settings",
        uk: "збережено {name}: завантажується з цими налаштуваннями",
    };

    /// The browser's `e` on a built-in Function (AGL-109, D3).
    pub const BROWSER_BUILTIN: Msg = Msg {
        en: "`{name}` is built-in: c clones it, and the copy is editable",
        uk: "`{name}` вбудована: c зробить її редаговану копію",
    };

    /// The browser's `c`: the name is asked on the keys' row (AGL-109, D3).
    pub const BROWSER_CLONE_ASK: Msg = Msg {
        en: "clone `{name}` as: {typed}▏ (Enter writes, Esc cancels)",
        uk: "копія `{name}` як: {typed}▏ (Enter пише, Esc скасовує)",
    };

    /// The browser's `c` wrote the copy (AGL-109, D3).
    pub const BROWSER_CLONED: Msg = Msg {
        en: "cloned to {name}: {path}",
        uk: "скопійовано до {name}: {path}",
    };

    /// The browser's `e`: the editor's command did not start (AGL-109, D4).
    pub const BROWSER_EDITOR_FAILED: Msg = Msg {
        en: "{editor} did not start: {reason}",
        uk: "{editor} не запустився: {reason}",
    };

    /// The browser's `e`, after the editor left the file as it was
    /// (AGL-109, D4).
    pub const BROWSER_UNCHANGED: Msg = Msg {
        en: "{name}: the file is as it was",
        uk: "{name}: файл лишився як був",
    };

    /// What `agl function check` says of the saved file, which the
    /// browser shows too (AGL-109, D4, D5).
    pub const BROWSER_SAVED: Msg = Msg {
        en: "saved {name}: loads, {kind}, {model}, {context} of context",
        uk: "збережено {name}: завантажується, {kind}, {model}, {context} контексту",
    };

    /// A resume whose Function is not found continues loose (AGL-063, D4).
    pub const LOOSE_CONTINUE: Msg = Msg {
        en: "the Function `{function}` of this session is not available; continuing loose: the tools it used, no grammar, the budgets of its last step",
        uk: "Function `{function}` цієї сесії недоступна; продовжую вільно: її інструменти, без грамматики, бюджети її останнього кроку",
    };

    /// The model of the session's snapshot differs from the model that runs
    /// (AGL-063, D5).
    pub const MODEL_DIFFERS: Msg = Msg {
        en: "this session was made with the model `{recorded}`, the model that runs is `{running}`",
        uk: "цю сесію зроблено з моделлю `{recorded}`, модель що працює — `{running}`",
    };

    /// `Error::Interrupted`; a turn stopped by Ctrl-C (`StopReason::Interrupted`).
    pub const INTERRUPTED: Msg = Msg {
        en: "interrupted",
        uk: "перервано",
    };

    /// `agl server stop` with `server.llama.external` (AGL-052, D4).
    pub const SERVER_OUTSIDE_NOT_STOPPED: Msg = Msg {
        en: "the server at {url} runs outside agl, and agl does not stop it",
        uk: "сервер за {url} працює поза agl, і agl його не зупиняє",
    };

    pub const DAEMON_NO_RUNTIME: Msg = Msg {
        en: "agl daemon: XDG_RUNTIME_DIR is not set, so it has no place for its socket",
        uk: "agl daemon: XDG_RUNTIME_DIR не задано, тож сокету ніде бути",
    };

    /// `agl daemon` where a daemon already answers (AGL-120, D4): served,
    /// not broken — it says so and exits 0.
    pub const DAEMON_SERVED: Msg = Msg {
        en: "agl is served: another daemon answers on {path}",
        uk: "agl уже обслуговується: на {path} відповідає інший демон",
    };

    pub const DAEMON_FAILED: Msg = Msg {
        en: "the daemon: {detail}",
        uk: "демон: {detail}",
    };

    pub const DAEMON_VERSION: Msg = Msg {
        en: "this client speaks the protocol {theirs}, the daemon {ours}: run the same agl",
        uk: "цей клієнт говорить протоколом {theirs}, демон — {ours}: запусти той самий agl",
    };

    pub const DAEMON_LINE_TOO_LONG: Msg = Msg {
        en: "the line of the request is over {max} bytes; it was dropped",
        uk: "рядок запиту більший за {max} байтів; його відкинуто",
    };

    pub const PLANE_GONE: Msg = Msg {
        en: "the run of the session ended before it answered; the session's file keeps what it did",
        uk: "виконання сесії скінчилося, не відповівши; файл сесії зберігає зроблене",
    };

    /// The cards of `agl notify` (AGL-074, D5, D8): what a turn does.
    pub const NOTIFY_THINKING: Msg = Msg {
        en: "thinking",
        uk: "думає",
    };
    pub const NOTIFY_WRITING: Msg = Msg {
        en: "writing the answer",
        uk: "пише відповідь",
    };
    pub const NOTIFY_COMPACTING: Msg = Msg {
        en: "compacting",
        uk: "стискає історію",
    };
    /// The live card's button.
    pub const NOTIFY_STOP: Msg = Msg {
        en: "Stop",
        uk: "Зупинити",
    };
    /// The end card's buttons and its field's placeholder.
    pub const NOTIFY_TERMINAL: Msg = Msg {
        en: "Terminal",
        uk: "Термінал",
    };
    pub const NOTIFY_SESSION: Msg = Msg {
        en: "Session",
        uk: "Сесія",
    };
    pub const NOTIFY_ANSWER: Msg = Msg {
        en: "Answer the agent",
        uk: "Відповісти агентові",
    };
    /// An answer sent while a turn runs (D4).
    pub const NOTIFY_QUEUED: Msg = Msg {
        en: "queued: {text}",
        uk: "у черзі: {text}",
    };
    /// The critical cards.
    pub const NOTIFY_STOPPED: Msg = Msg {
        en: "stopped: {reason}",
        uk: "зупинено: {reason}",
    };
    /// The stop card's button, and the message it sends (AGL-074, D10).
    pub const NOTIFY_CONTINUE: Msg = Msg {
        en: "Continue",
        uk: "Продовжити",
    };
    pub const NOTIFY_CONTINUE_TEXT: Msg = Msg {
        en: "Continue.",
        uk: "Продовжуй.",
    };
    pub const NOTIFY_CUT_OFF: Msg = Msg {
        en: "a generation was cut off at its token budget",
        uk: "генерацію обрізано на її ліміті токенів",
    };

    /// The Strata server, as the texts name it (AGL-038, D1).
    pub const STRATA_SERVER: Msg = Msg {
        en: "the Strata server",
        uk: "сервер Strata",
    };

    /// The first preparation of a Strata model, said before each step
    /// (AGL-089, D9, D13).
    pub const STRATA_PREPARING: Msg = Msg {
        en: "preparing the Strata model, once: {step} ...",
        uk: "готую модель Strata, один раз: {step} ...",
    };

    /// The steps of the preparation.
    pub const STRATA_STEP_PACK: Msg = Msg {
        en: "its pack (a few minutes)",
        uk: "її pack (кілька хвилин)",
    };
    pub const STRATA_STEP_MTP_FETCH: Msg = Msg {
        en: "the MTP head's tensors from Hugging Face (about 5 GB)",
        uk: "тензори MTP-голови з Hugging Face (близько 5 ГБ)",
    };
    pub const STRATA_STEP_MTP_BUILD: Msg = Msg {
        en: "the MTP head",
        uk: "MTP-голову",
    };

    /// The question of D9: another server runs, start one more?
    pub const OTHERS_RUNNING: Msg = Msg {
        en: "A server of another project is running ({units}). Start one more? [y/N]",
        uk: "Уже працює сервер іншого проєкту ({units}). Запустити ще один? [y/N]",
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

    /// The label of streamed thinking (D109), in caps as the tool's
    /// label (AGL-066, D21).
    pub const THINKING: Msg = Msg {
        en: "THINKING",
        uk: "МІРКУВАННЯ",
    };

    /// The row between the head and the tail of a folded thinking: how many
    /// of its rows are hidden (AGL-092, D4).
    pub const THINKING_FOLDED: Msg = Msg {
        en: "⋯ {n} hidden",
        uk: "⋯ {n} приховано",
    };

    /// The label of a tool's answer, as `TOOL` and `THINKING` are
    /// (AGL-095, D2).
    pub const RESULT_LABEL: Msg = Msg {
        en: "RESULT",
        uk: "РЕЗУЛЬТАТ",
    };

    /// The label of the answer's block in the inspect mode, where every
    /// block starts with a head for the focus to stand on (AGL-100).
    pub const ANSWER_LABEL: Msg = Msg {
        en: "ANSWER",
        uk: "ВІДПОВІДЬ",
    };

    /// The row a block shows when it has no text of its own (AGL-095, D4).
    pub const EMPTY: Msg = Msg {
        en: "(empty)",
        uk: "(порожньо)",
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

    /// The session's control socket is another agl's (AGL-074, D3).
    pub const SOCKET_TAKEN: Msg = Msg {
        en: "no control socket: another agl answers on {path}",
        uk: "без сокета керування: на {path} відповідає інший agl",
    };

    /// The session's control socket could not be made (AGL-074, D3).
    pub const SOCKET_FAILED: Msg = Msg {
        en: "no control socket: {path}: {error}",
        uk: "без сокета керування: {path}: {error}",
    };

    /// The splash's welcome phrase, above its version (AGL-065, D7,
    /// D8): what agentLIBRE is, as the repository's README says.
    pub const SPLASH_TAGLINE: Msg = Msg {
        en: "agentLIBRE is a local-first agentic system.",
        uk: "agentLIBRE — агентна система, що живе на вашому комп'ютері.",
    };

    /// The repository's address under the splash's tagline (AGL-065, D7).
    pub const SPLASH_URL: Msg = Msg {
        en: "github.com/dinozyavier/agentLIBRE",
        uk: "github.com/dinozyavier/agentLIBRE",
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
    pub const CMD_PLANE: Msg = Msg {
        en: "the plane and the planes of the directory; /plane <name> starts a new session in it",
        uk: "plane і plane цієї теки; /plane <назва> починає в ньому нову сесію",
    };

    /// A switch to another plane (AGL-077, D12).
    pub const PLANE_SWITCH: Msg = Msg {
        en: "a new session in the plane {name}",
        uk: "нова сесія в plane {name}",
    };

    /// `/plane` without a name (AGL-077, D12).
    pub const PLANE_HERE: Msg = Msg {
        en: "plane: {plane}\nthe planes of this directory: {planes}",
        uk: "plane: {plane}\nplane цієї теки: {planes}",
    };

    pub const CMD_FUNCTION: Msg = Msg {
        en: "the Functions; /function <name> switches; show and new work here, Ctrl-F browses and edits",
        uk: "Функції; /function <назва> перемикає; show і new працюють тут, Ctrl-F переглядає й редагує",
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

    pub const CMD_RENAME: Msg = Msg {
        en: "name this session, so that `agl --resume <name>` finds it: <name>",
        uk: "дати цій сесії ім'я, щоб `agl --resume <ім'я>` її знаходило: <ім'я>",
    };

    /// `/rename` without a name (AGL-062, D7).
    pub const RENAME_USAGE: Msg = Msg {
        en: "usage: /rename <name>",
        uk: "використання: /rename <ім'я>",
    };

    /// The session has the name (AGL-062, D7).
    pub const RENAMED: Msg = Msg {
        en: "this session is named `{name}`: agl --resume \"{name}\"",
        uk: "ця сесія має ім'я `{name}`: agl --resume \"{name}\"",
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

    pub const CMD_MEMORY: Msg = Msg {
        en: "the workspace memory: list, show <name> or remove <name>",
        uk: "пам'ять робочої області: list, show <назва> або remove <назва>",
    };

    /// `/memory` with no action, or with one it does not take (AGL-060).
    pub const MEMORY_USAGE: Msg = Msg {
        en: "usage: /memory list, /memory show <name> or /memory remove <name> [--yes]",
        uk: "використання: /memory list, /memory show <назва> або /memory remove <назва> [--yes]",
    };

    /// `agl memory list` over no entries (AGL-060).
    pub const MEMORY_EMPTY: Msg = Msg {
        en: "the memory has no entries",
        uk: "у пам'яті немає записів",
    };

    /// The legend of the mark `~` (AGL-060, D4).
    pub const MEMORY_MARK_CUT: Msg = Msg {
        en: "~  not in the prompt, which holds at most 4 KB of MEMORY.md, the newest lines",
        uk: "~  немає в промпті: він містить щонайбільше 4 КБ MEMORY.md, найновіші рядки",
    };

    /// The legend of the mark `?` (AGL-060, D4).
    pub const MEMORY_MARK_NO_LINE: Msg = Msg {
        en: "?  a file with no line in MEMORY.md",
        uk: "?  файл без рядка в MEMORY.md",
    };

    /// The legend of the mark `!` (AGL-060, D4).
    pub const MEMORY_MARK_NO_FILE: Msg = Msg {
        en: "!  a line in MEMORY.md with no file",
        uk: "!  рядок у MEMORY.md без файлу",
    };

    /// The question of a removal (AGL-060, D3, D7).
    pub const MEMORY_REMOVE_ASK: Msg = Msg {
        en: "Remove the memory entry {name}? [y/N]",
        uk: "Видалити запис пам'яті {name}? [y/N]",
    };

    /// A removal with no terminal to ask on and no `--yes` (AGL-060, D3, D7):
    /// the forge's error and the stdin session's refusal say it alike
    /// (AGL-127, D8).
    pub const REMOVE_NEEDS_YES: Msg = Msg {
        en: "{name} not removed: there is no terminal to ask on; add --yes",
        uk: "{name} не видалено: немає термінала, щоб спитати; додай --yes",
    };

    /// A removal the human said no to (AGL-060).
    pub const MEMORY_NOT_REMOVED: Msg = Msg {
        en: "not removed: {name}",
        uk: "не видалено: {name}",
    };

    /// `agl memory remove` is done (AGL-060).
    pub const MEMORY_REMOVED: Msg = Msg {
        en: "removed: {name}",
        uk: "видалено: {name}",
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
             Ctrl-D quits; Ctrl-O reads the session, Ctrl-F browses the functions \
             (while a mode stands, the mouse selects only with Shift)",
        uk: "Enter надсилає; Shift-Enter або Alt-Enter додає рядок; Вгору/Вниз — історія; \
             Ctrl-V прикріплює буфер обміну; Ctrl-R записує голос і вписує сказане \
             (ще раз Ctrl-R зупиняє, Escape скасовує); \
             Ctrl-C зупиняє хід (або очищає рядок, або виходить, якщо він порожній); \
             Ctrl-D виходить; Ctrl-O читає сесію, Ctrl-F переглядає функції \
             (доки режим стоїть, миша виділяє лише з Shift)",
    };

    /// The separator row while the voice is recorded (AGL-016, D9; AGL-123):
    /// `{mode}` is the send mode, auto or ask.
    pub const VOICE_RECORDING: Msg = Msg {
        en: "Recording {time} · {mode}",
        uk: "Запис {time} · {mode}",
    };

    /// The separator row while the recording is turned into text (AGL-123).
    pub const VOICE_TRANSCRIBING: Msg = Msg {
        en: "Transcribing · {mode}…",
        uk: "Розпізнаю · {mode}…",
    };

    /// The send mode of the dictation: the transcript sends itself (AGL-123, D1).
    pub const VOICE_MODE_AUTO: Msg = Msg {
        en: "auto",
        uk: "авто",
    };

    /// The send mode of the dictation: the transcript waits in the line (AGL-123, D1).
    pub const VOICE_MODE_ASK: Msg = Msg {
        en: "ask",
        uk: "питати",
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
    pub const LABEL_PLANE: Msg = Msg {
        en: "plane",
        uk: "plane",
    };

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

    /// The time a turn took.
    pub const WORKED_FOR: Msg = Msg {
        en: "Worked for {time}",
        uk: "Працював {time}",
    };

    /// The header of a step's thinking in the chat, before its time
    /// (AGL-139, D4).
    pub const THOUGHT: Msg = Msg {
        en: "thought",
        uk: "думав",
    };

    /// The chat's line when the model's server answered, with its context
    /// (AGL-139, D2).
    pub const SERVER_ANSWERED: Msg = Msg {
        en: "the server answered: {size} of context",
        uk: "сервер відповів: контекст {size}",
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

    /// The keys of the inspect mode (AGL-066, D8; AGL-098, D1, D5).
    pub const INSPECT_HINT: Msg = Msg {
        en: "⇧↑↓ block · ⌃↑↓/wheel page · ⌥↑↓ neighbour · ⌥u/U user · ⌥g top · ⌥G end · esc exit · ⇧mouse selects",
        uk: "⇧↑↓ блок · ⌃↑↓/колесо сторінка · ⌥↑↓ сусід · ⌥u/U повідомлення · ⌥g згори · ⌥G знизу · esc вихід · ⇧миша виділяє",
    };

    /// The inspect mode keeps the tail of the session on screen (AGL-066,
    /// D8).
    pub const INSPECT_FOLLOW: Msg = Msg {
        en: "follow",
        uk: "хвіст",
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

    /// `Error::Write`.
    pub const FORGE_WRITE: Msg = Msg {
        en: "cannot write {path}: {source}",
        uk: "не вдалося записати {path}: {source}",
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
        en: "this directory gets a new store {dir}; the existing stores are {names} (forge rehome reconnects a moved workspace)",
        uk: "ця тека отримує нове сховище {dir}; наявні сховища: {names} (forge rehome повертає переміщену робочу область)",
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

    /// The forge tool: an action in a status its kind does not allow it
    /// in (AGL-124, D13); `statuses` are the ones it does.
    pub const FORGE_TOOL_NOT_ALLOWED: Msg = Msg {
        en: "{name} is {status}; {action} is allowed while {statuses}",
        uk: "{name} — {status}; {action} дозволено, поки {statuses}",
    };

    /// The forge tool: an action its kind allows in no status (AGL-124,
    /// D13).
    pub const FORGE_TOOL_NEVER_ALLOWED: Msg = Msg {
        en: "{action} is allowed in no status of a {type_name}",
        uk: "{action} не дозволено в жодному статусі {type_name}",
    };

    /// The forge tool: `edit` touches an append-only section (D4,
    /// AGL-032; AGL-124, D13).
    pub const FORGE_TOOL_EDIT_APPEND_ONLY: Msg = Msg {
        en: "the `## {section}` section of {name} is append-only; use the append action",
        uk: "секція `## {section}` у {name} — append-only; скористайтеся append",
    };

    /// The last link of a list of alternatives: `a, b or c`.
    pub const OR: Msg = Msg {
        en: "or",
        uk: "або",
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

}
