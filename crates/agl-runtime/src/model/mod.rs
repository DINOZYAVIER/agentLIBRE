//! Model declarations plus explicit validated import/fetch operations.
//!
//! Model execution belongs to `agl-runtime::inference`. This module deliberately has no
//! setup/status/checkpoint lifecycle and writes no binding or configuration files.

mod acquisition;
mod format;
mod manifest;

pub(crate) use acquisition::open_managed_model;
pub use acquisition::{
    FetchProgress, ImportedModel, ModelArtifactDigest, ModelFetchRequest, ModelImportRequest,
    fetch_model, import_model,
};
pub use format::{ModelConfig, ModelDialect, ToolCallFormat};
pub use manifest::{
    MODEL_FILE_NAME, MODEL_SCHEMA, ModelArtifact, ModelArtifactKind, ModelManifest, ModelReasoning,
    parse_package_view,
};
