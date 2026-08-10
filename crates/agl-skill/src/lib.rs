mod adapter;
mod context;
mod manifest;
mod pack;
mod registry;
mod trust;

#[cfg(test)]
mod audit;

pub use adapter::{
    SkillPackageAdapter, builtin_source, directory_skill_source, parse_skill_envelope,
};
pub use context::{
    SkillContextBlock, SkillContextBundle, SkillContextError, SkillContextEvidence,
    SkillContextReferenceEvidence, SkillPermissionRequestTemplateEvidence, SkillToolRouting,
    SkillToolRoutingView, SkillUnavailableToolEvidence, build_verified_context_bundle,
};
pub use manifest::{
    MemoryPermissionScope, SkillHarness, SkillManifestError, SkillMemoryPermissions,
    SkillNotesPermissions, SkillPermissionRequestTemplate, SkillPermissions, SkillReference,
    SkillReferencePolicy, SkillSource,
};
pub use pack::{
    SkillPackEntry, SkillPackManifest, SkillPackSubmodule, ValidatedSkillPack, validate_skill_pack,
};
pub use registry::{
    RegisteredSkill, SkillRegistry, SkillRegistryError, SkillTrustState, builtin_registry,
};
pub use trust::{SkillTrustStore, SkillTrustStoreError, skill_identity};
