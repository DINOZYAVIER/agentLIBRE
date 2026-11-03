//! `repl` of the user's settings: the tool cards and the voice recording
//! (D130; AGL-016; AGL-039, D20).

use std::path::Path;

use crate::error::Error;
use crate::nix::Section;

/// The longest voice recording of the REPL's Ctrl-R, in seconds, when the
/// settings do not set `repl.max_record_seconds` (AGL-016, D5, D8;
/// AGL-039, D20; AGL-043 F18).
const DEFAULT_MAX_RECORD_SECONDS: u32 = 300;

/// `[repl]`: the limits of tool cards and their frame (D130).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReplFile {
    pub tool_lines: Option<usize>,
    pub tool_chars: Option<usize>,
    pub frame: Option<bool>,
    /// The longest voice recording, in seconds (AGL-039, D20).
    pub max_record_seconds: Option<u32>,
}

/// `[repl]` with the defaults of the old agl (D130).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplSettings {
    pub tool_lines: usize,
    pub tool_chars: usize,
    pub frame: bool,
    /// The longest voice recording of Ctrl-R, in seconds (AGL-016;
    /// AGL-039, D20).
    pub max_record_seconds: u32,
}

impl Default for ReplSettings {
    fn default() -> ReplSettings {
        ReplSettings {
            tool_lines: 10,
            tool_chars: 500,
            frame: true,
            max_record_seconds: DEFAULT_MAX_RECORD_SECONDS,
        }
    }
}

impl ReplFile {
    /// Reads `repl` of the settings file `path` (D130; AGL-039, D20).
    pub fn parse(section: Option<Section>, path: &Path) -> Result<ReplFile, Error> {
        Ok(match section {
            None => ReplFile::default(),
            Some(mut repl) => {
                repl.check_keys(&["tool_lines", "tool_chars", "frame", "max_record_seconds"])?;
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
                let tool_lines = count("tool_lines")?;
                let tool_chars = count("tool_chars")?;
                let max_record_seconds = count("max_record_seconds")?
                    .map(|n| u32::try_from(n).unwrap_or(u32::MAX));
                ReplFile {
                    tool_lines,
                    tool_chars,
                    frame: repl.bool("frame")?,
                    max_record_seconds,
                }
            }
        })
    }
}
