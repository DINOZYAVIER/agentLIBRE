//! `search` of a settings file: the search service and its mutual TLS
//! (AGL-019, D3-D5).

use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::nix::Section;

/// `[search]` of a settings file, the user's or the project's
/// (AGL-019, D3): the search service and the credentials of its mutual
/// TLS. A key that is not set is `None`; the table is usable when the
/// merged one sets all four (D5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchFile {
    /// An `https://` address (D3).
    pub url: Option<String>,
    /// Absolute paths of the client certificate, its private key (PKCS#8
    /// PEM) and the private CA bundle (D3, D4).
    pub client_certificate: Option<String>,
    pub client_private_key: Option<String>,
    pub private_ca: Option<String>,
}

/// A complete `[search]`: the search is set up (AGL-019, D5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Search {
    pub url: String,
    pub client_certificate: PathBuf,
    pub client_private_key: PathBuf,
    pub private_ca: PathBuf,
}

impl SearchFile {
    const KEYS: &'static [&'static str] =
        &["url", "client_certificate", "client_private_key", "private_ca"];

    pub fn parse(section: Option<Section>, path: &Path) -> Result<SearchFile, Error> {
        let Some(mut search) = section else {
            return Ok(SearchFile::default());
        };
        search.check_keys(Self::KEYS)?;
        let url = search.string("url")?;
        if let Some(url) = &url
            && !url.starts_with("https://")
        {
            return Err(Error::UnsupportedSearchUrl {
                path: path.into(),
                url: url.clone(),
            });
        }
        let mut file = |key: &str| -> Result<Option<String>, Error> {
            let value = search.string(key)?;
            if let Some(value) = &value
                && !Path::new(value).is_absolute()
            {
                return Err(Error::SearchPathNotAbsolute {
                    path: path.into(),
                    key: search.key(key),
                    value: value.clone(),
                });
            }
            Ok(value)
        };
        Ok(SearchFile {
            url,
            client_certificate: file("client_certificate")?,
            client_private_key: file("client_private_key")?,
            private_ca: file("private_ca")?,
        })
    }

    pub fn is_empty(&self) -> bool {
        *self == SearchFile::default()
    }

    /// The keys set here, the others from `defaults`.
    pub fn over(&self, defaults: &SearchFile) -> SearchFile {
        SearchFile {
            url: self.url.clone().or_else(|| defaults.url.clone()),
            client_certificate: self
                .client_certificate
                .clone()
                .or_else(|| defaults.client_certificate.clone()),
            client_private_key: self
                .client_private_key
                .clone()
                .or_else(|| defaults.client_private_key.clone()),
            private_ca: self.private_ca.clone().or_else(|| defaults.private_ca.clone()),
        }
    }

    /// The search when all four keys are set; `Ok(None)` when none is
    /// (no search); the first missing key's name when some are (D5).
    pub fn complete(&self) -> Result<Option<Search>, &'static str> {
        if self.is_empty() {
            return Ok(None);
        }
        Ok(Some(Search {
            url: self.url.clone().ok_or("url")?,
            client_certificate: self
                .client_certificate
                .clone()
                .ok_or("client_certificate")?
                .into(),
            client_private_key: self
                .client_private_key
                .clone()
                .ok_or("client_private_key")?
                .into(),
            private_ca: self.private_ca.clone().ok_or("private_ca")?.into(),
        }))
    }
}
