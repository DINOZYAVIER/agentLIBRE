use serde::{Deserialize, Serialize};

pub const EXTENSION_CATALOG_SCHEMA: &str = "agentlibre.extension-catalog/v1";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionCatalogDto {
    pub schema: String,
    pub catalog_digest: String,
    pub extensions: Vec<ExtensionStateDto>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionStateDto {
    pub id: String,
    pub declaration_digest: String,
    pub compiled: bool,
    pub selected: bool,
    pub admitted: bool,
    pub unavailable_reason: Option<String>,
}
