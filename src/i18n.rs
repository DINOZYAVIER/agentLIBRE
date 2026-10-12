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

/// The `--help` text.
pub fn help(lang: Lang) -> &'static str {
    match lang {
        Lang::En => {
            "\
Usage: agl-lite [-p <message>] [--resume <id>] [--function <name>]

Without -p, agl-lite starts an interactive session.

Options:
  -p <message>        send one message, print the answer and exit
  --resume <id>       continue session <id>
  --function <name>   use Function <name>
  -h, --help          print this help"
        }
        Lang::Uk => {
            "\
Використання: agl-lite [-p <повідомлення>] [--resume <id>] [--function <назва>]

Без -p agl-lite запускає інтерактивну сесію.

Параметри:
  -p <повідомлення>   надіслати одне повідомлення, вивести відповідь і вийти
  --resume <id>       продовжити сесію <id>
  --function <назва>  використати Function <назва>
  -h, --help          показати цю довідку"
        }
    }
}

impl Error {
    /// The message for this error in `lang`. Usage errors end with a
    /// pointer to `--help`.
    pub fn message(&self, lang: Lang) -> String {
        let message = self.text(lang);
        if !self.is_usage() {
            return message;
        }
        match lang {
            Lang::En => format!("{message}\nRun `agl-lite --help` for usage."),
            Lang::Uk => format!("{message}\nДив. `agl-lite --help`."),
        }
    }

    fn text(&self, lang: Lang) -> String {
        use Lang::{En, Uk};
        match (self, lang) {
            (Error::InvalidUnicode, En) => "arguments must be valid UTF-8".into(),
            (Error::InvalidUnicode, Uk) => "аргументи мають бути коректним UTF-8".into(),
            (Error::UnknownOption(arg), En) => format!("unknown option `{arg}`"),
            (Error::UnknownOption(arg), Uk) => format!("невідомий параметр `{arg}`"),
            (Error::MissingValue(arg), En) => format!("option `{arg}` needs a value"),
            (Error::MissingValue(arg), Uk) => format!("параметру `{arg}` потрібне значення"),
            (Error::RepeatedOption(arg), En) => format!("option `{arg}` is given twice"),
            (Error::RepeatedOption(arg), Uk) => format!("параметр `{arg}` вказано двічі"),
            (Error::FunctionWithResume, En) => {
                "`--function` cannot go with `--resume`: the session keeps its Function".into()
            }
            (Error::FunctionWithResume, Uk) => {
                "`--function` не можна разом із `--resume`: сесія має свою Function".into()
            }
            (Error::NoHome { var }, En) => {
                format!("HOME is not set and {var} is not an absolute path")
            }
            (Error::NoHome { var }, Uk) => {
                format!("HOME не задано, а {var} не є абсолютним шляхом")
            }
            (Error::CurrentDir(source), En) => {
                format!("cannot read the current directory: {source}")
            }
            (Error::CurrentDir(source), Uk) => {
                format!("не вдалося визначити поточну теку: {source}")
            }
            (Error::Read { path, source }, En) => {
                format!("cannot read {}: {source}", path.display())
            }
            (Error::Read { path, source }, Uk) => {
                format!("не вдалося прочитати {}: {source}", path.display())
            }
            (Error::Parse { path, detail }, En) => {
                format!("invalid TOML in {}:\n{detail}", path.display())
            }
            (Error::Parse { path, detail }, Uk) => {
                format!("некоректний TOML у {}:\n{detail}", path.display())
            }
            (Error::UnknownKey { path, key, allowed }, En) => format!(
                "unknown key `{key}` in {}; allowed: {}",
                path.display(),
                allowed.join(", ")
            ),
            (Error::UnknownKey { path, key, allowed }, Uk) => format!(
                "невідомий ключ `{key}` у {}; дозволені: {}",
                path.display(),
                allowed.join(", ")
            ),
            (
                Error::WrongType {
                    path,
                    key,
                    expected,
                },
                En,
            ) => {
                let expected = match expected {
                    Expected::String => "a string",
                    Expected::Table => "a table",
                    Expected::Bool => "true or false",
                    Expected::Integer => "an integer",
                    Expected::Number => "a number",
                    Expected::Strings => "an array of strings",
                };
                format!("`{key}` in {} must be {expected}", path.display())
            }
            (
                Error::WrongType {
                    path,
                    key,
                    expected,
                },
                Uk,
            ) => {
                let expected = match expected {
                    Expected::String => "рядком",
                    Expected::Table => "таблицею",
                    Expected::Bool => "true або false",
                    Expected::Integer => "цілим числом",
                    Expected::Number => "числом",
                    Expected::Strings => "масивом рядків",
                };
                format!("`{key}` у {} має бути {expected}", path.display())
            }
            (
                Error::InvalidStyle {
                    path,
                    key,
                    value,
                    problem,
                },
                lang,
            ) => {
                let problem = match (problem, lang) {
                    (StyleError::Empty, En) => "a style cannot be empty".to_owned(),
                    (StyleError::Empty, Uk) => "стиль не може бути порожнім".to_owned(),
                    (StyleError::Color(color), En) => {
                        format!("`{color}` is not #RRGGBB or oklch(L C H)")
                    }
                    (StyleError::Color(color), Uk) => {
                        format!("`{color}` не є #RRGGBB або oklch(L C H)")
                    }
                    (StyleError::Oklch, En) => {
                        "oklch needs three numbers, lightness 0..1 and chroma >= 0".to_owned()
                    }
                    (StyleError::Oklch, Uk) => {
                        "oklch потребує трьох чисел, яскравість 0..1, насиченість >= 0".to_owned()
                    }
                };
                match lang {
                    En => format!(
                        "invalid style `{value}` for `{key}` in {}: {problem}",
                        path.display()
                    ),
                    Uk => format!(
                        "некоректний стиль `{value}` для `{key}` у {}: {problem}",
                        path.display()
                    ),
                }
            }
            (Error::NotPositive { path, key }, En) => {
                format!("`{key}` in {} must be at least 1", path.display())
            }
            (Error::NotPositive { path, key }, Uk) => {
                format!("`{key}` у {} має бути не менше 1", path.display())
            }
            (Error::UnsupportedLanguage { path, value }, En) => format!(
                "unsupported language `{value}` in {}; supported: en, uk",
                path.display()
            ),
            (Error::UnsupportedLanguage { path, value }, Uk) => format!(
                "непідтримувана мова `{value}` у {}; підтримуються: en, uk",
                path.display()
            ),
            (Error::UnsupportedUrl { path, url }, En) => format!(
                "unsupported server address `{url}` in {}; only http:// is supported",
                path.display()
            ),
            (Error::UnsupportedUrl { path, url }, Uk) => format!(
                "непідтримувана адреса сервера `{url}` у {}; підтримується лише http://",
                path.display()
            ),
            (Error::ServerInProject { path, user_path }, En) => format!(
                "{}: a project file cannot set [server]; the server address belongs in {}",
                path.display(),
                user_path.display()
            ),
            (Error::ServerInProject { path, user_path }, Uk) => format!(
                "{}: файл проєкту не може задавати [server]; адреса сервера задається в {}",
                path.display(),
                user_path.display()
            ),
            (Error::MissingServerUrl { user_path }, En) => format!(
                "the llama-server address is not set; add to {}:\n\n[server]\nurl = \"http://HOST:PORT\"",
                user_path.display()
            ),
            (Error::MissingServerUrl { user_path }, Uk) => format!(
                "адресу llama-server не задано; додай у {}:\n\n[server]\nurl = \"http://HOST:PORT\"",
                user_path.display()
            ),
            (Error::MissingKey { path, key }, En) => {
                format!("`{key}` is missing in {}", path.display())
            }
            (Error::MissingKey { path, key }, Uk) => {
                format!("у {} немає `{key}`", path.display())
            }
            (Error::InvalidName(name), En) => format!(
                "invalid name `{name}`: a name is not empty, has no `/` and does not start with `.`"
            ),
            (Error::InvalidName(name), Uk) => format!(
                "некоректна назва `{name}`: назва непорожня, без `/` і не починається з `.`"
            ),
            (Error::FormatNotFound { name, path }, En) => format!(
                "model format `{name}` not found: no {} and no built-in format of that name",
                path.display()
            ),
            (Error::FormatNotFound { name, path }, Uk) => format!(
                "формат моделі `{name}` не знайдено: немає {} і вбудованого формату з такою назвою",
                path.display()
            ),
            (Error::Template { path, key, problem }, lang) => {
                let problem = template_problem(problem, lang);
                match lang {
                    En => format!("`{key}` in {}: {problem}", path.display()),
                    Uk => format!("`{key}` у {}: {problem}", path.display()),
                }
            }
            (Error::FunctionNotFound { name, path }, En) => format!(
                "Function `{name}` not found: no {} and no built-in Function of that name",
                path.display()
            ),
            (Error::FunctionNotFound { name, path }, Uk) => format!(
                "Function `{name}` не знайдено: немає {} і вбудованої Function з такою назвою",
                path.display()
            ),
            (Error::Function { path, key, problem }, lang) => {
                let problem = function_problem(problem, lang);
                match lang {
                    En => format!("`{key}` in {}: {problem}", path.display()),
                    Uk => format!("`{key}` у {}: {problem}", path.display()),
                }
            }
            (Error::ServerUnreachable { url, detail }, En) => {
                format!("cannot reach llama-server at {url}: {detail}")
            }
            (Error::ServerUnreachable { url, detail }, Uk) => {
                format!("не вдалося з'єднатися з llama-server за адресою {url}: {detail}")
            }
            (
                Error::ServerStatus {
                    url,
                    status,
                    message,
                },
                En,
            ) => {
                format!("llama-server at {url} answered {status}: {message}")
            }
            (
                Error::ServerStatus {
                    url,
                    status,
                    message,
                },
                Uk,
            ) => {
                format!("llama-server за адресою {url} відповів {status}: {message}")
            }
            (Error::ServerReply { url, detail }, En) => {
                format!("unexpected reply from llama-server at {url}: {detail}")
            }
            (Error::ServerReply { url, detail }, Uk) => {
                format!("неочікувана відповідь llama-server за адресою {url}: {detail}")
            }
            (Error::SessionNotFound { id, dir }, En) => {
                format!("no session `{id}` in {}", dir.display())
            }
            (Error::SessionNotFound { id, dir }, Uk) => {
                format!("сесії `{id}` немає в {}", dir.display())
            }
            (Error::SessionAmbiguous { id, count }, En) => {
                format!("{count} sessions start with `{id}`; give more of the id")
            }
            (Error::SessionAmbiguous { id, count }, Uk) => {
                format!("сесій, що починаються з `{id}`: {count}; вкажи довшу частину id")
            }
            (Error::SessionLine { path, line, detail }, En) => {
                format!("line {line} of {}: {detail}", path.display())
            }
            (Error::SessionLine { path, line, detail }, Uk) => {
                format!("рядок {line} у {}: {detail}", path.display())
            }
            (Error::SessionWrite { path, source }, En) => {
                format!("cannot write {}: {source}", path.display())
            }
            (Error::SessionWrite { path, source }, Uk) => {
                format!("не вдалося записати {}: {source}", path.display())
            }
            (Error::SessionState { state, function }, En) => format!(
                "the session continues in state `{state}`, which Function `{function}` no longer has"
            ),
            (Error::SessionState { state, function }, Uk) => format!(
                "сесія продовжується в стані `{state}`, якого Function `{function}` вже не має"
            ),
            (Error::OutsideGrammar(text), En) => {
                format!("llama-server generated text outside the grammar: {text:?}")
            }
            (Error::OutsideGrammar(text), Uk) => {
                format!("llama-server згенерував текст поза граматикою: {text:?}")
            }
            (Error::Interrupted, En) => "interrupted".into(),
            (Error::Interrupted, Uk) => "перервано".into(),
            (Error::Terminal { source }, En) => {
                format!("cannot switch the terminal to raw mode: {source}")
            }
            (Error::Terminal { source }, Uk) => {
                format!("не вдалося перемкнути термінал у raw-режим: {source}")
            }
        }
    }
}

/// The label of streamed thinking (D109).
pub fn thinking(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "thinking",
        Lang::Uk => "міркування",
    }
}

/// `n` lines of output.
pub fn lines(n: usize, lang: Lang) -> String {
    match lang {
        Lang::En if n == 1 => "1 line".into(),
        Lang::En => format!("{n} lines"),
        Lang::Uk => {
            let word = match (n % 10, n % 100) {
                (1, 11) | (2..=4, 12..=14) | (0 | 5..=9, _) => "рядків",
                (1, _) => "рядок",
                (2..=4, _) => "рядки",
                _ => "рядків",
            };
            format!("{n} {word}")
        }
    }
}

/// The session line at the start of a run (D109).
pub fn session(id: &str, lang: Lang) -> String {
    match lang {
        Lang::En => format!("session {id}"),
        Lang::Uk => format!("сесія {id}"),
    }
}

/// Why a turn stopped (D113).
pub fn stop_reason(reason: StopReason, function: &Function, lang: Lang) -> String {
    use Lang::{En, Uk};
    match (reason, lang) {
        (StopReason::StepLimit, En) => format!(
            "stopped: the turn took max_steps_total ({}) steps",
            function.max_steps_total
        ),
        (StopReason::StepLimit, Uk) => format!(
            "зупинено: хід зробив max_steps_total ({}) кроків",
            function.max_steps_total
        ),
        (StopReason::ContextFull, En) => {
            "stopped: the conversation no longer fits the model context; start a new session".into()
        }
        (StopReason::ContextFull, Uk) => {
            "зупинено: розмова вже не вміщується в контекст моделі; почни нову сесію".into()
        }
        (StopReason::CutOff, En) => format!(
            "stopped: the action hit max_action_tokens ({})",
            function.max_action_tokens
        ),
        (StopReason::CutOff, Uk) => format!(
            "зупинено: дія дійшла до max_action_tokens ({})",
            function.max_action_tokens
        ),
        (StopReason::ServerError, En) => "stopped: llama-server failed".into(),
        (StopReason::ServerError, Uk) => "зупинено: помилка llama-server".into(),
        (StopReason::Interrupted, En) => "interrupted".into(),
        (StopReason::Interrupted, Uk) => "перервано".into(),
    }
}

/// Why a turn stopped, and how to go on (D113).
pub fn stopped(reason: StopReason, function: &Function, id: &str, lang: Lang) -> String {
    let why = stop_reason(reason, function, lang);
    let next = match lang {
        Lang::En => "to continue",
        Lang::Uk => "щоб продовжити",
    };
    format!(
        "{why}
{next}: agl-lite --resume {id} -p \"...\""
    )
}

fn template_problem(problem: &TemplateProblem, lang: Lang) -> String {
    use Lang::{En, Uk};
    match (problem, lang) {
        (TemplateProblem::Missing(p), En) => format!("needs {p}"),
        (TemplateProblem::Missing(p), Uk) => format!("потрібен {p}"),
        (TemplateProblem::Repeated(p), En) => format!("{p} appears more than once"),
        (TemplateProblem::Repeated(p), Uk) => format!("{p} трапляється більше одного разу"),
        (TemplateProblem::Unexpected(p), En) => format!("{p} does not belong here"),
        (TemplateProblem::Unexpected(p), Uk) => format!("{p} тут не дозволений"),
        (TemplateProblem::ValueBeforeName, En) => "{value} comes before {name}".into(),
        (TemplateProblem::ValueBeforeName, Uk) => "{value} стоїть перед {name}".into(),
        (TemplateProblem::NothingBeforeName, En) => {
            "needs text before {name}, or answers cannot be told apart from calls".into()
        }
        (TemplateProblem::NothingBeforeName, Uk) => {
            "потрібен текст перед {name}, інакше відповідь не відрізнити від виклику".into()
        }
        (TemplateProblem::NothingAfterValue, En) => {
            "needs text after {value}, or a value has no end".into()
        }
        (TemplateProblem::NothingAfterValue, Uk) => {
            "потрібен текст після {value}, інакше значення не має кінця".into()
        }
        (TemplateProblem::Empty, En) => "must not be empty".into(),
        (TemplateProblem::Empty, Uk) => "не може бути порожнім".into(),
    }
}

fn function_problem(problem: &FunctionProblem, lang: Lang) -> String {
    use FunctionProblem as P;
    use Lang::{En, Uk};
    match (problem, lang) {
        (P::UnknownState(name), En) => format!("there is no state `{name}`"),
        (P::UnknownState(name), Uk) => format!("стану `{name}` немає"),
        (P::BadTransitionKey, En) => {
            "a transition key is a tool of the state, `tool.ok`/`.fail`/`.timeout`, or `else`"
                .into()
        }
        (P::BadTransitionKey, Uk) => {
            "ключ переходу — tool цього стану, `tool.ok`/`.fail`/`.timeout` або `else`".into()
        }
        (P::MissingTransition { tool, outcome }, En) => {
            format!("no transition for `{tool}.{outcome}`; add it, `{tool}` or `else` to `next`")
        }
        (P::MissingTransition { tool, outcome }, Uk) => {
            format!(
                "немає переходу для `{tool}.{outcome}`; додай його, `{tool}` або `else` у `next`"
            )
        }
        (P::NoActions, En) => "the state has no tools and no `answer = true`".into(),
        (P::NoActions, Uk) => "стан не має ні tools, ні `answer = true`".into(),
        (P::NextWithoutTools, En) => "`next` needs tools; an answer ends the turn".into(),
        (P::NextWithoutTools, Uk) => "`next` потребує tools; відповідь завершує хід".into(),
        (P::OnLimitWithoutLimit, En) => "`on_limit` needs `max_steps` or `max_visits`".into(),
        (P::OnLimitWithoutLimit, Uk) => "`on_limit` потребує `max_steps` або `max_visits`".into(),
        (P::OnLimitCycle, En) => "`on_limit` states lead back to this state".into(),
        (P::OnLimitCycle, Uk) => "стани `on_limit` ведуть назад у цей стан".into(),
        (P::Unreachable, En) => "no transition leads to this state".into(),
        (P::Unreachable, Uk) => "у цей стан не веде жоден перехід".into(),
        (P::NoThinking { format }, En) => {
            format!("model format `{format}` has no thinking; remove `think = true`")
        }
        (P::NoThinking { format }, Uk) => {
            format!("формат моделі `{format}` не має thinking; прибери `think = true`")
        }
        (P::Pattern(error), lang) => pattern_error(error, lang),
        (P::PatternMayEndValue { end }, En) => {
            format!("the pattern may produce {end:?}, which ends a value in this model format")
        }
        (P::PatternMayEndValue { end }, Uk) => {
            format!("шаблон може дати {end:?}, а це кінець значення у форматі моделі")
        }
        (P::EmptyEnum, En) => "`enum` needs at least one value".into(),
        (P::EmptyEnum, Uk) => "`enum` потребує хоча б одного значення".into(),
        (P::BadRange, En) => "needs `min <= max`, and `min >= 1` for `timeout`".into(),
        (P::BadRange, Uk) => "потрібно `min <= max`, а для `timeout` ще `min >= 1`".into(),
        (P::ConstraintKeys, En) => "a constraint has exactly one key".into(),
        (P::ConstraintKeys, Uk) => "обмеження має рівно один ключ".into(),
        (P::NotPositive, En) => "must be at least 1".into(),
        (P::NotPositive, Uk) => "має бути щонайменше 1".into(),
    }
}

fn pattern_error(error: &PatternError, lang: Lang) -> String {
    use Lang::{En, Uk};
    use PatternErrorKind as K;
    let at = error.offset;
    match (&error.kind, lang) {
        (K::Anchor, En) => {
            format!("pattern byte {at}: remove `^`/`$`; a pattern covers the whole value")
        }
        (K::Anchor, Uk) => {
            format!("шаблон, байт {at}: прибери `^`/`$`; шаблон і так охоплює все значення")
        }
        (K::GroupSyntax, En) => format!("pattern byte {at}: `(?` groups are not supported"),
        (K::GroupSyntax, Uk) => format!("шаблон, байт {at}: групи `(?` не підтримуються"),
        (K::Escape(c), En) => format!("pattern byte {at}: `\\{c}` is not supported"),
        (K::Escape(c), Uk) => format!("шаблон, байт {at}: `\\{c}` не підтримується"),
        (K::TrailingBackslash, En) => format!("pattern byte {at}: `\\` at the end"),
        (K::TrailingBackslash, Uk) => format!("шаблон, байт {at}: `\\` у кінці"),
        (K::NothingToRepeat, En) => format!("pattern byte {at}: nothing to repeat"),
        (K::NothingToRepeat, Uk) => format!("шаблон, байт {at}: нема чого повторювати"),
        (K::BadRepetition, En) => {
            format!("pattern byte {at}: use `{{n}}`, `{{n,}}` or `{{n,m}}` with n <= m")
        }
        (K::BadRepetition, Uk) => {
            format!("шаблон, байт {at}: потрібно `{{n}}`, `{{n,}}` або `{{n,m}}` з n <= m")
        }
        (K::UnbalancedGroup, En) => format!("pattern byte {at}: unbalanced parenthesis"),
        (K::UnbalancedGroup, Uk) => format!("шаблон, байт {at}: незакрита або зайва дужка"),
        (K::BadClass, En) => format!("pattern byte {at}: bad class; escape `[` and `]` inside it"),
        (K::BadClass, Uk) => {
            format!("шаблон, байт {at}: некоректний клас; екрануй `[` і `]` усередині")
        }
        (K::BadRange, En) => format!("pattern byte {at}: bad class range"),
        (K::BadRange, Uk) => format!("шаблон, байт {at}: некоректний діапазон у класі"),
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
            "unknown option `--url`\nRun `agl-lite --help` for usage."
        );
        assert_eq!(
            error.message(Lang::Uk),
            "невідомий параметр `--url`\nДив. `agl-lite --help`."
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

/// Texts of the interactive session (D118, D120, D124, D134, D135, D138).
pub mod repl {
    use super::Lang::{self, En, Uk};
    use crate::session::StopReason;

    /// The hint of an empty prompt.
    pub fn hint(lang: Lang) -> &'static str {
        match lang {
            En => "Type a message or / for commands",
            Uk => "Введіть повідомлення або / для команд",
        }
    }

    /// The dock's state.
    pub fn ready(lang: Lang) -> &'static str {
        match lang {
            En => "Ready",
            Uk => "Готово",
        }
    }

    pub fn generating(lang: Lang) -> &'static str {
        match lang {
            En => "Generating",
            Uk => "Генерація",
        }
    }

    pub fn running(tool: &str, lang: Lang) -> String {
        match lang {
            En => format!("Running {tool}"),
            Uk => format!("Виконується {tool}"),
        }
    }

    pub fn queued(n: usize, lang: Lang) -> String {
        match lang {
            En => format!("queued {n}"),
            Uk => format!("у черзі {n}"),
        }
    }

    /// The description of a `/` command in the menu and in `/help`.
    pub fn command(name: &str, lang: Lang) -> &'static str {
        match (name, lang) {
            ("/quit", En) => "close agl-lite",
            ("/quit", Uk) => "закрити agl-lite",
            ("/exit", En) => "same as /quit",
            ("/exit", Uk) => "те саме, що /quit",
            ("/help", En) => "show commands and keys",
            ("/help", Uk) => "показати команди й клавіші",
            ("/status", En) => "show the session",
            ("/status", Uk) => "показати сесію",
            ("/new", En) => "start a new session",
            ("/new", Uk) => "почати нову сесію",
            _ => "",
        }
    }

    pub fn keys(lang: Lang) -> &'static str {
        match lang {
            En => {
                "Enter sends; Shift-Enter or Alt-Enter adds a line; Up/Down walk the history; \
                 Ctrl-C stops the turn (or clears the line, or quits when it is empty); \
                 Ctrl-D quits"
            }
            Uk => {
                "Enter надсилає; Shift-Enter або Alt-Enter додає рядок; Вгору/Вниз — історія; \
                 Ctrl-C зупиняє хід (або очищає рядок, або виходить, якщо він порожній); \
                 Ctrl-D виходить"
            }
        }
    }

    pub fn unknown_command(name: &str, lang: Lang) -> String {
        match lang {
            En => format!("unknown command {name}; try /help"),
            Uk => format!("невідома команда {name}; спробуй /help"),
        }
    }

    pub fn busy(name: &str, lang: Lang) -> String {
        match lang {
            En => format!("{name} is not available during a turn; stop it with Ctrl-C first"),
            Uk => format!("{name} недоступна під час ходу; спершу зупини його Ctrl-C"),
        }
    }

    /// One line of `/status`.
    pub fn status_line(label: &str, value: &str, lang: Lang) -> String {
        let label = match (label, lang) {
            ("session", En) => "session",
            ("session", Uk) => "сесія",
            ("function", En) => "Function",
            ("function", Uk) => "Function",
            ("workspace", En) => "workspace",
            ("workspace", Uk) => "робоча тека",
            ("server", En) => "server",
            ("server", Uk) => "сервер",
            ("context", En) => "context",
            ("context", Uk) => "контекст",
            ("state", En) => "state",
            ("state", Uk) => "стан",
            (other, _) => other,
        };
        format!("{label}: {value}")
    }

    pub fn tokens(n: usize, lang: Lang) -> String {
        match lang {
            En => format!("{n} tokens"),
            Uk => format!("{n} токенів"),
        }
    }

    pub fn idle(lang: Lang) -> &'static str {
        match lang {
            En => "waiting for a message",
            Uk => "чекає повідомлення",
        }
    }

    pub fn unfinished(state: &str, lang: Lang) -> String {
        match lang {
            En => format!("an interrupted turn in state {state}; the next message continues it"),
            Uk => format!("обірваний хід у стані {state}; наступне повідомлення продовжить його"),
        }
    }

    /// The note under a cut-short card (D138).
    pub fn limited(lines: usize, chars: usize, lang: Lang) -> String {
        match lang {
            En => format!("… limited to {lines} lines / {chars} chars"),
            Uk => format!("… обмежено {lines} рядками / {chars} символами"),
        }
    }

    pub fn worked_for(time: &str, lang: Lang) -> String {
        match lang {
            En => format!("Worked for {time}"),
            Uk => format!("Працював {time}"),
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

    pub fn interrupted_cleared(lang: Lang) -> &'static str {
        match lang {
            En => "interrupted; queued input cleared",
            Uk => "перервано; чергу введення очищено",
        }
    }

    pub fn error_cleared(lang: Lang) -> &'static str {
        match lang {
            En => "queued input cleared",
            Uk => "чергу введення очищено",
        }
    }

    /// The line that ends a turn that stopped (D127).
    pub fn stopped(reason: StopReason, function: &crate::function::Function, lang: Lang) -> String {
        super::stop_reason(reason, function, lang)
    }

    pub fn new_session(id: &str, lang: Lang) -> String {
        match lang {
            En => format!("new session {id}"),
            Uk => format!("нова сесія {id}"),
        }
    }

    pub fn tool_label(lang: Lang) -> &'static str {
        match lang {
            En => "TOOL",
            Uk => "TOOL",
        }
    }
}
