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
    /// The step-ca that issues the client certificates (AGL-043 F20); not
    /// one of the four keys of the search itself.
    pub ca: SearchCaFile,
}

/// `search.ca` (AGL-043 F20): the step-ca of `agl search login` — its
/// `https://` address, its root certificate (an absolute path) and the
/// name of its OIDC provisioner. A key that is not set is `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchCaFile {
    pub url: Option<String>,
    pub root: Option<String>,
    pub provisioner: Option<String>,
}

/// A complete `search.ca`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchCa {
    pub url: String,
    pub root: PathBuf,
    pub provisioner: String,
}

impl SearchCaFile {
    const KEYS: &'static [&'static str] = &["url", "root", "provisioner"];

    fn parse(section: Option<Section>, path: &Path) -> Result<SearchCaFile, Error> {
        let Some(mut ca) = section else {
            return Ok(SearchCaFile::default());
        };
        ca.check_keys(Self::KEYS)?;
        let url = ca.string("url")?;
        if let Some(url) = &url
            && !url.starts_with("https://")
        {
            return Err(Error::UnsupportedSearchUrl {
                path: path.into(),
                url: url.clone(),
            });
        }
        let root = ca.string("root")?;
        if let Some(root) = &root
            && !Path::new(root).is_absolute()
        {
            return Err(Error::SearchPathNotAbsolute {
                path: path.into(),
                key: ca.key("root"),
                value: root.clone(),
            });
        }
        Ok(SearchCaFile {
            url,
            root,
            provisioner: ca.string("provisioner")?,
        })
    }

    /// The keys set here, the others from `defaults`.
    fn over(&self, defaults: &SearchCaFile) -> SearchCaFile {
        SearchCaFile {
            url: self.url.clone().or_else(|| defaults.url.clone()),
            root: self.root.clone().or_else(|| defaults.root.clone()),
            provisioner: self
                .provisioner
                .clone()
                .or_else(|| defaults.provisioner.clone()),
        }
    }

    /// The step-ca when all three keys are set; the first missing key's
    /// name otherwise.
    pub fn complete(&self) -> Result<SearchCa, &'static str> {
        Ok(SearchCa {
            url: self.url.clone().ok_or("url")?,
            root: self.root.clone().ok_or("root")?.into(),
            provisioner: self.provisioner.clone().ok_or("provisioner")?,
        })
    }
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
        &["url", "client_certificate", "client_private_key", "private_ca", "ca"];

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
        let client_certificate = file("client_certificate")?;
        let client_private_key = file("client_private_key")?;
        let private_ca = file("private_ca")?;
        Ok(SearchFile {
            url,
            client_certificate,
            client_private_key,
            private_ca,
            ca: SearchCaFile::parse(search.section("ca")?, path)?,
        })
    }

    /// Whether none of the four keys of the search is set; `ca` does not
    /// count (AGL-043 F20): a step-ca set up before the first login is no
    /// half-set search.
    pub fn is_empty(&self) -> bool {
        SearchFile {
            ca: SearchCaFile::default(),
            ..self.clone()
        } == SearchFile::default()
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
            ca: self.ca.over(&defaults.ca),
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
