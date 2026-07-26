mod adapter;
mod catalog;
mod checkpoint;
mod install;
mod lifecycle;
mod planner;
mod source;
mod status;
mod worker;

pub use adapter::{
    MODEL_FILE_NAME, MODEL_PAYLOAD_SCHEMA, ModelArtifactAdapter, builtin_model_source,
    model_adapter_registry, parse_model_package,
};
pub use catalog::{
    CatalogCapability, CatalogRuntimeProfile, ModelArtifact, ModelArtifactRole, ModelCatalog,
    ModelPackage, ModelPackageId, ModelPackageProvenance, ProfileDevice,
};
pub use checkpoint::{
    PlannedArtifactRole, SetupCheckpoint, SetupCheckpointStore, SetupPhase,
    canonical_workspace_digest, setup_plan_hash,
};
pub use install::{
    ImportedModel, InstallRecordState, InstallSource, InstalledArtifactFile, ModelBindingPatch,
    ModelInstallCommitReceipt, ModelInstallRecord, ModelInstallStore, derive_hf_model_id,
    derive_model_id_from_path, import_local_model, validate_gguf,
};
pub use lifecycle::{
    ModelLifecycleOperation, ModelLifecyclePlan, ModelLifecycleService, ModelLifecycleTarget,
    ModelPruneBlob, ModelPruneEntry,
};
pub use planner::{
    CpuFallbackOffer, CpuResources, DiskResources, HostResources, LlamaDeviceInfo, LlamaDeviceKind,
    ModelFit, ModelFitKind, RuntimePlan, RuntimePlanSet, RuntimePlanner,
};
pub use source::{HfSource, HfSourceKind};
pub use status::{
    ModelFileObservation, ModelInspector, ModelStatusReport, ModelVerificationReport,
};
pub use worker::{
    ArtifactDownloadSpec, ArtifactFileDownloadSpec, DownloadedArtifact, DownloadedArtifactFile,
    HubFileCandidate, HubInspection, ModelCacheStatus, ModelDownloadError, ModelDownloadHandle,
    ModelDownloadJob, ModelDownloadRequest, ModelDownloadResult, ModelDownloadWorker,
    ModelProgressEvent,
};

pub fn hugging_face_cache_dir() -> std::path::PathBuf {
    hf_hub::resolve_cache_dir()
}

pub fn hugging_face_offline() -> bool {
    std::env::var("HF_HUB_OFFLINE")
        .ok()
        .is_some_and(|value| hf_truthy(&value))
}

fn hf_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_uppercase().as_str(),
        "1" | "ON" | "YES" | "TRUE"
    )
}

#[cfg(test)]
mod tests {
    use super::hf_truthy;

    #[test]
    fn standard_hf_boolean_values_are_recognized() {
        for value in ["1", "on", "YES", " true "] {
            assert!(hf_truthy(value), "{value}");
        }
        for value in ["", "0", "off", "false", "auto"] {
            assert!(!hf_truthy(value), "{value}");
        }
    }
}
