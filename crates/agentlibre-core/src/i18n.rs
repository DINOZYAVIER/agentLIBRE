//! The core's texts that need its types: an error's message, a stop's
//! reason, a server's name, the REPL's words (D65-D68; AGL-121, D78).

use crate::error::Error;
use crate::function::Function;
use crate::session::StopReason;
use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::tr;

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
            Error::FunctionWithResume => tr!(lang, m::FUNCTION_WITH_RESUME).to_string(),
            Error::PlaneWithResume => tr!(lang, m::PLANE_WITH_RESUME).to_string(),
            Error::Paths(error) => error.message(lang),
            Error::Common(error) => error.message(lang),
            Error::Settings(error) => error.message(lang),
            Error::Forge(error) => error.message(lang),
            Error::Engine(error) => error.message(lang),
            Error::Backend(error) => error.message(lang),
            Error::Search(error) => error.message(lang),
            Error::Format(error) => error.message(lang),
            Error::Function(error) => error.message(lang),
            Error::Wsl(error) => error.message(lang),
            Error::PlaneGone => tr!(lang, m::PLANE_GONE).to_string(),
            Error::Said { text } => text.clone(),
            Error::DaemonNoRuntime => tr!(lang, m::DAEMON_NO_RUNTIME).to_string(),
            Error::Daemon { detail } => tr!(lang, m::DAEMON_FAILED, detail = detail),
            Error::DaemonServer(error) => error.message(lang),
            Error::Plane(error) => error.message(lang),
            Error::Whisper(error) => error.message(lang),
            Error::Media(error) => error.message(lang),
            Error::Strata(error) => error.message(lang),
            Error::Session(error) => error.message(lang),
            Error::Machine(error) => error.message(lang),
            Error::Weights(error) => error.message(lang),
            Error::Download(error) => error.message(lang),
            Error::Nix(error) => error.message(lang),
            Error::Memory(error) => error.message(lang),
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
    stopped_why(&stop_reason(reason, function, lang), id, lang)
}

/// [`stopped`] from the reason's text a run told (AGL-075, D7).
pub fn stopped_why(why: &str, id: &str, lang: Lang) -> String {
    format!("{why}\n{}", tr!(lang, m::STOPPED_RESUME, id = id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_decoding_speed_is_formatted_per_language() {
        assert_eq!(repl::speed(46.3, Lang::En), "46.3 tok/s");
        assert_eq!(repl::speed(46.26, Lang::En), "46.3 tok/s");
        assert_eq!(repl::speed(46.3, Lang::Uk), "46,3 ток/с");
        assert_eq!(repl::speed(120.0, Lang::Uk), "120,0 ток/с");
    }

    #[test]
    fn the_errors_of_a_backend_name_its_kind() {
        // AGL-038, D1: the Strata server is not called llama-server.
        let reply = |kind| {
            Error::from(crate::backend::error::Error::ServerReply {
                kind,
                url: "http://127.0.0.1:18088".into(),
                detail: "the engine stopped".into(),
            })
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

}

/// Texts of `agl server` (D146-D150).
pub mod server {
    use super::Lang;
    use super::m;
    use agentlibre_common::tr;
    use crate::engine::Health;
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
    use agentlibre_common::tr;
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
            "/memory" => tr!(lang, m::CMD_MEMORY),
            "/rename" => tr!(lang, m::CMD_RENAME),
            "/new" => tr!(lang, m::CMD_NEW),
            "/compact" => tr!(lang, m::CMD_COMPACT),
            "/function" => tr!(lang, m::CMD_FUNCTION),
            "/plane" => tr!(lang, m::CMD_PLANE),
            _ => "",
        }
    }

    /// One line of `/status`.
    pub fn status_line(label: &str, value: &str, lang: Lang) -> String {
        let label = match label {
            "session" => tr!(lang, m::LABEL_SESSION),
            "function" => tr!(lang, m::LABEL_FUNCTION),
            "workspace" => tr!(lang, m::LABEL_WORKSPACE),
            "plane" => tr!(lang, m::LABEL_PLANE),
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
