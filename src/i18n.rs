//! Languages of CLI messages (D65-D68).

use std::ffi::OsString;

use crate::error::{Error, Expected};

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
                };
                format!("`{key}` у {} має бути {expected}", path.display())
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
            (Error::NotImplemented, En) => "the agent loop is not implemented yet".into(),
            (Error::NotImplemented, Uk) => "агентний цикл ще не реалізовано".into(),
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
        assert!(!Error::NotImplemented.message(Lang::En).contains("--help"));
    }

    #[test]
    fn language_codes() {
        assert_eq!(Lang::from_code("en"), Some(Lang::En));
        assert_eq!(Lang::from_code("uk"), Some(Lang::Uk));
        assert_eq!(Lang::from_code("uk_UA"), None);
    }
}
