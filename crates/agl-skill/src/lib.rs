mod adapter;
mod context;
mod manifest;
mod pack;
mod registry;
mod workspace;

#[cfg(test)]
mod audit;

pub use adapter::{
    SkillArtifactAdapter, builtin_source, directory_skill_source, parse_skill_envelope,
    skill_adapter_registry,
};
pub use context::{
    SkillContextBlock, SkillContextBundle, SkillContextError, SkillContextEvidence,
    SkillContextReferenceEvidence, SkillPermissionRequestTemplateEvidence, SkillToolRouting,
    SkillToolRoutingView, SkillUnavailableToolEvidence, build_verified_context_bundle,
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
    SkillArtifactFolderReadiness, SkillFolderPrepareOptions, SkillFolderPrepareReport,
    SkillFolderSyncAction, SkillFolderSyncActionKind, SkillFolderSyncOptions,
    SkillFolderSyncReport, SkillReportState, SkillTrustAction, SkillTrustOptions, SkillTrustStore,
    SkillTrustUpdateReport, TrustedSkillRecord, WorkspaceSkillDiagnostic,
    WorkspaceSkillDiagnosticScope, WorkspaceSkillDiagnosticSeverity, WorkspaceSkillReport,
    WorkspaceSkillStatus, prepare_workspace_skill_artifact_write, prepare_workspace_skill_folders,
    revoke_workspace_skill, sync_workspace_skill_folders, trust_workspace_skill,
    trusted_workspace_registry, workspace_skill_report, workspace_skill_report_with_trust,
};
