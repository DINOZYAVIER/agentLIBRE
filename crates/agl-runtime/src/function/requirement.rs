//! Typed `function:ID@VERSIONREQ` Function requirements.

use anyhow::{Context, Result, ensure};
use semver::VersionReq;
use serde::{Deserialize, Deserializer};

use crate::package::PackageId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionRequirement {
    pub id: PackageId,
    pub version: VersionReq,
}

impl<'de> Deserialize<'de> for FunctionRequirement {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        parse_function_requirement(&String::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

pub fn parse_function_requirement(value: &str) -> Result<FunctionRequirement> {
    let body = value
        .strip_prefix("function:")
        .context("Function requirement must begin with function:")?;
    let (id, version) = body
        .split_once('@')
        .context("Function requirement must contain @VERSION")?;
    ensure!(!version.is_empty(), "Function version requirement is empty");
    ensure!(
        !version.contains('@'),
        "Function requirement contains multiple @ separators"
    );
    Ok(FunctionRequirement {
        id: PackageId::new(id)?,
        version: VersionReq::parse(version)?,
    })
}
