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
    MemoryPermissionScope, SkillArtifactAccess, SkillArtifactDeclaration, SkillArtifactKind,
    SkillFolderCreateRule, SkillFolderCreateSituation, SkillHarness, SkillManifestError,
    SkillMemoryPermissions, SkillNotesPermissions, SkillPermissionRequestTemplate,
    SkillPermissions, SkillReference, SkillReferencePolicy, SkillSource,
};
pub use pack::{
    SkillPackEntry, SkillPackManifest, SkillPackSubmodule, ValidatedSkillPack, validate_skill_pack,
};
pub use registry::{
    RegisteredSkill, SkillRegistry, SkillRegistryError, SkillTrustState, builtin_registry,
};
pub use workspace::{
    LockedComponent, LockedSkill, SkillArtifactFolderReadiness, SkillFolderPrepareOptions,
    SkillFolderPrepareReport, SkillFolderSyncAction, SkillFolderSyncActionKind,
    SkillFolderSyncOptions, SkillFolderSyncReport, SkillLockOptions, SkillLockReport,
    SkillReportState, SkillTrustAction, SkillTrustOptions, SkillTrustStore, SkillTrustUpdateReport,
    SkillsLockFile, TrustedSkillRecord, WorkspaceSkillReport, WorkspaceSkillStatus,
    lock_workspace_skills, prepare_workspace_skill_artifact_write, prepare_workspace_skill_folders,
    revoke_workspace_skill, sync_workspace_skill_folders, trust_workspace_skill,
    trusted_workspace_registry, workspace_skill_report, workspace_skill_report_with_trust,
};
