//! `whisper` of a settings file (AGL-014, D22).

use std::path::Path;

use super::check_weights_name;
use super::server::is_http_url;
use crate::error::Error;
use crate::settings::error::Error as SettingsError;
use crate::nix::Section;

/// `whisper` of a settings file, the user's or the project's
/// (AGL-014, D22): the whisper.cpp server that turns an audio into text
/// when the model does not hear it. A key that is not set is `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WhisperFile {
    pub url: Option<String>,
    /// A weights file (AGL-039, D24): a file name in the weights folders
    /// or an absolute path.
    pub model: Option<String>,
    /// More whisper-server arguments.
    pub args: Option<Vec<String>>,
    /// The certificate of the CA of an `https://` server (AGL-058, D12).
    pub private_ca: Option<String>,
    /// A draft-engine weights file for the live dictation (AGL-122, D5):
    /// a file name in the weights folders or an absolute path.
    pub draft_model: Option<String>,
    /// The draft engine program (AGL-122, D9): the server binary that
    /// speaks Parakeet, an absolute path or a name in PATH.
    pub draft_program: Option<String>,
    /// The url of the draft server (AGL-122, D9), as `url` for whisper.
    pub draft_url: Option<String>,
}

impl WhisperFile {
    const KEYS: &'static [&'static str] = &[
        "url", "model", "args", "private_ca", "draft_model", "draft_program", "draft_url",
    ];

    pub fn parse(section: Option<Section>, path: &Path) -> Result<WhisperFile, Error> {
        let Some(mut whisper) = section else {
            return Ok(WhisperFile::default());
        };
        whisper.check_keys(Self::KEYS)?;
        let url = whisper.string("url")?;
        if let Some(url) = &url
            && !is_http_url(url)
        {
            return Err(SettingsError::UnsupportedUrl {
                path: path.into(),
                url: url.clone(),
            }
            .into());
        }
        let model = whisper.string("model")?;
        if let Some(model) = &model {
            check_weights_name(model)?;
        }
        let draft_model = whisper.string("draft_model")?;
        if let Some(draft_model) = &draft_model {
            check_weights_name(draft_model)?;
        }
        let draft_url = whisper.string("draft_url")?;
        if let Some(draft_url) = &draft_url
            && !is_http_url(draft_url)
        {
            return Err(SettingsError::UnsupportedUrl {
                path: path.into(),
                url: draft_url.clone(),
            }
            .into());
        }
        Ok(WhisperFile {
            url,
            model,
            args: whisper.strings("args")?,
            private_ca: super::server::read_private_ca(&mut whisper)?,
            draft_model,
            draft_program: whisper.string("draft_program")?,
            draft_url,
        })
    }

    pub fn is_empty(&self) -> bool {
        *self == WhisperFile::default()
    }

    /// The keys set here, the others from `defaults`; `args` is replaced
    /// as a whole (AGL-015, D11).
    pub fn over(&self, defaults: &WhisperFile) -> WhisperFile {
        WhisperFile {
            url: self.url.clone().or_else(|| defaults.url.clone()),
            model: self.model.clone().or_else(|| defaults.model.clone()),
            args: self.args.clone().or_else(|| defaults.args.clone()),
            private_ca: self.private_ca.clone().or_else(|| defaults.private_ca.clone()),
            draft_model: self.draft_model.clone().or_else(|| defaults.draft_model.clone()),
            draft_program: self.draft_program.clone().or_else(|| defaults.draft_program.clone()),
            draft_url: self.draft_url.clone().or_else(|| defaults.draft_url.clone()),
        }
    }
}
