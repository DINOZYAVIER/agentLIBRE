mod context;
mod manifest;
mod pack;
mod registry;
mod workspace;

#[cfg(test)]
mod audit;

pub use context::{
    SkillContextBlock, SkillContextBundle, SkillContextError, SkillContextEvidence,
    SkillContextReferenceEvidence, SkillPermissionRequestTemplateEvidence,
    build_verified_context_bundle,
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
pub use workspace::{
    LockedComponent, LockedSkill, SkillLockOptions, SkillLockReport, SkillReportState,
    SkillTrustAction, SkillTrustOptions, SkillTrustStore, SkillTrustUpdateReport, SkillsLockFile,
    TrustedSkillRecord, WorkspaceSkillReport, WorkspaceSkillStatus, lock_workspace_skills,
    revoke_workspace_skill, trust_workspace_skill, trusted_workspace_registry,
    workspace_skill_report, workspace_skill_report_with_trust,
};
