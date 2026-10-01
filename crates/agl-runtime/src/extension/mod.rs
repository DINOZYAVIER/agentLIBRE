//! Declarative Extension parsing.

#![forbid(unsafe_code)]

mod manifest;

pub use manifest::{
    EXTENSION_FILE_NAME, EXTENSION_SCHEMA, ExtensionEffectManifest, ExtensionManifest,
    ExtensionPackage, ExtensionToolManifest, parse_package_view,
};
