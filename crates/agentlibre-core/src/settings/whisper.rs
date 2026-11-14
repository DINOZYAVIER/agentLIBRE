//! `whisper` of a settings file (AGL-014, D22).

use std::path::Path;

use super::check_weights_name;
use super::server::is_http_url;
use crate::error::Error;
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
}

impl WhisperFile {
    const KEYS: &'static [&'static str] = &["url", "model", "args", "private_ca"];

    pub fn parse(section: Option<Section>, path: &Path) -> Result<WhisperFile, Error> {
        let Some(mut whisper) = section else {
            return Ok(WhisperFile::default());
        };
        whisper.check_keys(Self::KEYS)?;
        let url = whisper.string("url")?;
        if let Some(url) = &url
            && !is_http_url(url)
        {
            return Err(Error::UnsupportedUrl {
                path: path.into(),
                url: url.clone(),
            });
        }
        let model = whisper.string("model")?;
        if let Some(model) = &model {
            check_weights_name(model)?;
        }
        Ok(WhisperFile {
            url,
            model,
            args: whisper.strings("args")?,
            private_ca: super::server::read_private_ca(&mut whisper)?,
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
        }
    }
}
