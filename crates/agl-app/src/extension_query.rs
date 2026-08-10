use agl_kernel::{CatalogDigest, DeclarationDigest, ExtensionId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionState {
    pub id: ExtensionId,
    pub declaration_digest: DeclarationDigest,
    pub compiled: bool,
    pub selected: bool,
    pub admitted: bool,
    pub unavailable_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ExtensionQueryError {
    #[error("Extension catalog query is unavailable: {reason}")]
    Unavailable { reason: String },
}

pub trait ExtensionQueryPort: Send + Sync {
    fn extensions(&self) -> Result<Vec<ExtensionState>, ExtensionQueryError>;

    fn catalog_digest(&self) -> Result<CatalogDigest, ExtensionQueryError>;
}

impl ExtensionQueryPort for agl_runtime::RuntimeExtensionCatalog {
    fn extensions(&self) -> Result<Vec<ExtensionState>, ExtensionQueryError> {
        Ok(self
            .query()
            .values()
            .map(|state| ExtensionState {
                id: state.id.clone(),
                declaration_digest: state.declaration_digest.clone(),
                compiled: state.compiled,
                selected: state.selected,
                admitted: state.admitted,
                unavailable_reason: state.unavailable_reason.clone(),
            })
            .collect())
    }

    fn catalog_digest(&self) -> Result<CatalogDigest, ExtensionQueryError> {
        Ok(agl_runtime::RuntimeExtensionCatalog::catalog_digest(self).clone())
    }
}
