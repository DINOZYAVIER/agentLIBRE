//! Declarative Extension parsing and native process-local bindings.

#![forbid(unsafe_code)]

mod manifest;
mod native;

pub use manifest::{
    EXTENSION_FILE_NAME, EXTENSION_SCHEMA, ExtensionEffectManifest, ExtensionManifest,
    ExtensionPackage, ExtensionToolManifest, parse_package_view,
};
pub use native::{
    ExtensionBindings, ToolBinding, ToolCancellation, ToolContext, ToolFuture, ToolHandler,
};
