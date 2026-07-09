use crate::{
    HookDeclaration, HookEvent, HookId, HookInput, HookResult, ToolCatalog, ToolCatalogError,
    ToolProviderDeclaration, ToolProviderId,
};

mod validators;

pub const PROVIDER_ID: &str = "core-guards";
pub const JSON_VALIDATE_HOOK_ID: &str = "json.validate";
pub const REPO_PATH_VALIDATE_HOOK_ID: &str = "repo_path.validate";
pub const TASK_SPEC_VALIDATE_HOOK_ID: &str = "task_spec.validate";
pub const SECRET_SCAN_VALIDATE_HOOK_ID: &str = "secret_scan.validate";
pub const DIFF_SCOPE_VALIDATE_HOOK_ID: &str = "diff_scope.validate";
pub const VERIFICATION_VALIDATE_HOOK_ID: &str = "verification.validate";
pub const COMMIT_MESSAGE_VALIDATE_HOOK_ID: &str = "commit_message.validate";
pub const SKILL_MANIFEST_VALIDATE_HOOK_ID: &str = "skill_manifest.validate";
pub const REVIEW_PACK_VALIDATE_HOOK_ID: &str = "review_pack.validate";
pub const RUNTIME_IDENTITY_VALIDATE_HOOK_ID: &str = "runtime.identity.validate";
pub const RUNTIME_IDENTITY_REQUIRE_HOOK_ID: &str = "runtime.identity.require";

#[derive(Clone, Debug)]
pub struct CoreGuards {
    declaration: ToolProviderDeclaration,
}

impl Default for CoreGuards {
    fn default() -> Self {
        Self {
            declaration: declaration(),
        }
    }
}

impl CoreGuards {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn declaration(&self) -> &ToolProviderDeclaration {
        &self.declaration
    }

    pub fn run_hook(&self, input: HookInput) -> HookResult {
        match input.hook_id.as_str() {
            JSON_VALIDATE_HOOK_ID => validators::validate_json(input),
            REPO_PATH_VALIDATE_HOOK_ID => validators::validate_repo_path(input),
            TASK_SPEC_VALIDATE_HOOK_ID => validators::validate_task_spec(input),
            SECRET_SCAN_VALIDATE_HOOK_ID => validators::validate_secret_scan(input),
            DIFF_SCOPE_VALIDATE_HOOK_ID => validators::validate_diff_scope(input),
            VERIFICATION_VALIDATE_HOOK_ID => validators::validate_verification(input),
            COMMIT_MESSAGE_VALIDATE_HOOK_ID => validators::validate_commit_message(input),
            SKILL_MANIFEST_VALIDATE_HOOK_ID => validators::validate_skill_manifest(input),
            REVIEW_PACK_VALIDATE_HOOK_ID => validators::validate_review_pack(input),
            RUNTIME_IDENTITY_VALIDATE_HOOK_ID => {
                validators::validate_runtime_identity(input, false)
            }
            RUNTIME_IDENTITY_REQUIRE_HOOK_ID => validators::validate_runtime_identity(input, true),
            _ => validators::fail(
                input.hook_id,
                "unknown_hook",
                "unknown core guard hook",
                None,
            ),
        }
    }
}

pub fn declaration() -> ToolProviderDeclaration {
    let mut declaration = ToolProviderDeclaration::new(
        ToolProviderId::new(PROVIDER_ID).expect("core guard provider id is valid"),
        "Core Guards",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("core guard declaration is valid")
    .with_hook(hook(JSON_VALIDATE_HOOK_ID, HookEvent::ModelResponse, false));
    for id in [
        REPO_PATH_VALIDATE_HOOK_ID,
        TASK_SPEC_VALIDATE_HOOK_ID,
        SECRET_SCAN_VALIDATE_HOOK_ID,
        DIFF_SCOPE_VALIDATE_HOOK_ID,
        VERIFICATION_VALIDATE_HOOK_ID,
        COMMIT_MESSAGE_VALIDATE_HOOK_ID,
        SKILL_MANIFEST_VALIDATE_HOOK_ID,
        REVIEW_PACK_VALIDATE_HOOK_ID,
        RUNTIME_IDENTITY_VALIDATE_HOOK_ID,
        RUNTIME_IDENTITY_REQUIRE_HOOK_ID,
    ] {
        declaration = declaration.with_hook(hook(id, HookEvent::ArtifactWrite, true));
    }
    declaration
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn hook(id: &str, event: HookEvent, required: bool) -> HookDeclaration {
    HookDeclaration {
        id: HookId::new(id).expect("core guard hook id is valid"),
        event,
        required,
    }
}

#[cfg(test)]
mod tests;
