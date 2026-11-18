//! `repl` of the user's settings: the voice recording, the server's start
//! and the splash (AGL-016; AGL-039, D20; AGL-046; AGL-065, D5; AGL-080).

use std::path::Path;

use crate::error::Error;
use crate::nix::Section;

/// The longest voice recording of the REPL's Ctrl-R, in seconds, when the
/// settings do not set `repl.max_record_seconds` (AGL-016, D5, D8;
/// AGL-039, D20; AGL-043 F18).
const DEFAULT_MAX_RECORD_SECONDS: u32 = 300;

/// How much of the thinking the REPL shows (AGL-092, D2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThinkingDetail {
    /// No thinking at all: the row of the phase says that it was.
    Off,
    /// All of it while it is decoded, the first paragraph and the last rows
    /// after it ends.
    #[default]
    Flexible,
    /// All of it, and it stays.
    Full,
}

impl ThinkingDetail {
    /// The level named `name`; none when there is no such level.
    pub fn parse(name: &str) -> Option<ThinkingDetail> {
        match name {
            "off" => Some(ThinkingDetail::Off),
            "flexible" => Some(ThinkingDetail::Flexible),
            "full" => Some(ThinkingDetail::Full),
            _ => None,
        }
    }
}

/// `[repl]`: the voice recording, the server's start and the splash.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReplFile {
    /// The longest voice recording, in seconds (AGL-039, D20).
    pub max_record_seconds: Option<u32>,
    /// Whether the session starts the server as it starts (AGL-046).
    pub start_server: Option<bool>,
    /// Whether the REPL shows the splash at its start (AGL-065, D5).
    pub splash: Option<bool>,
    /// How much of the thinking the transcript shows (AGL-092, D2).
    pub thinking: Option<ThinkingDetail>,
}

/// `[repl]` with the defaults of the old agl (D130).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplSettings {
    /// The longest voice recording of Ctrl-R, in seconds (AGL-016;
    /// AGL-039, D20).
    pub max_record_seconds: u32,
    /// Whether the session starts the server as it starts, not at the first
    /// message (AGL-046).
    pub start_server: bool,
    /// Whether the REPL shows the splash at its start (AGL-065, D5).
    pub splash: bool,
    /// How much of the thinking the transcript shows (AGL-092, D2).
    pub thinking: ThinkingDetail,
}

impl ReplSettings {
    /// The user's `[repl]` over the defaults: the REPL's settings are the
    /// user's alone (AGL-076: the terminal reads them itself).
    pub fn of(user: &super::UserFile) -> ReplSettings {
        let defaults = ReplSettings::default();
        ReplSettings {
            max_record_seconds: user.repl.max_record_seconds.unwrap_or(defaults.max_record_seconds),
            start_server: user.repl.start_server.unwrap_or(defaults.start_server),
            splash: user.repl.splash.unwrap_or(defaults.splash),
            thinking: user.repl.thinking.unwrap_or(defaults.thinking),
        }
    }
}

impl Default for ReplSettings {
    fn default() -> ReplSettings {
        ReplSettings {
            max_record_seconds: DEFAULT_MAX_RECORD_SECONDS,
            start_server: true,
            splash: true,
            thinking: ThinkingDetail::default(),
        }
    }
}

impl ReplFile {
    /// Reads `repl` of the settings file `path` (D130; AGL-039, D20).
    pub fn parse(section: Option<Section>, path: &Path) -> Result<ReplFile, Error> {
        Ok(match section {
            None => ReplFile::default(),
            Some(mut repl) => {
                repl.check_keys(&["max_record_seconds", "start_server", "splash", "thinking"])?;
                let mut count = |key: &str| -> Result<Option<usize>, Error> {
                    repl.integer(key)?
                        .map(|n| {
                            usize::try_from(n).ok().filter(|n| *n >= 1).ok_or_else(|| {
                                Error::NotPositive {
                                    path: path.into(),
                                    key: repl.key(key),
                                }
                            })
                        })
                        .transpose()
                };
                let max_record_seconds = count("max_record_seconds")?
                    .map(|n| u32::try_from(n).unwrap_or(u32::MAX));
                ReplFile {
                    max_record_seconds,
                    start_server: repl.bool("start_server")?,
                    splash: repl.bool("splash")?,
                    thinking: repl.string("thinking")?.map(|name| {
                        ThinkingDetail::parse(&name).ok_or_else(|| Error::UnknownThinkingLevel {
                            path: path.into(),
                            value: name,
                        })
                    }).transpose()?,
                }
            }
        })
    }
}
