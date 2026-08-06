use agl_kernel::{
    ExtensionDescriptor, ExtensionId, HookDeclaration, HookEvent, HookHandler, HookHandlerError,
    HookId, HookInput, HookResult,
};

use crate::{ToolCatalog, ToolCatalogError};

mod validators;

pub const PROVIDER_ID: &str = "core";
pub const JSON_VALIDATE_HOOK_ID: &str = "core:json.validate";
pub const REPO_PATH_VALIDATE_HOOK_ID: &str = "core:repo_path.validate";
pub const TASK_SPEC_VALIDATE_HOOK_ID: &str = "core:task_spec.validate";
pub const SECRET_SCAN_VALIDATE_HOOK_ID: &str = "core:secret_scan.validate";
pub const DIFF_SCOPE_VALIDATE_HOOK_ID: &str = "core:diff_scope.validate";
pub const VERIFICATION_VALIDATE_HOOK_ID: &str = "core:verification.validate";
pub const COMMIT_MESSAGE_VALIDATE_HOOK_ID: &str = "core:commit_message.validate";
pub const SKILL_MANIFEST_VALIDATE_HOOK_ID: &str = "core:skill_manifest.validate";
pub const REVIEW_PACK_VALIDATE_HOOK_ID: &str = "core:review_pack.validate";
pub const RUNTIME_IDENTITY_VALIDATE_HOOK_ID: &str = "core:runtime.identity.validate";
pub const RUNTIME_IDENTITY_REQUIRE_HOOK_ID: &str = "core:runtime.identity.require";

#[derive(Clone, Debug)]
pub struct CoreGuards {
    declaration: ExtensionDescriptor,
}

impl HookHandler for CoreGuards {
    fn invoke(&self, input: HookInput) -> Result<serde_json::Value, HookHandlerError> {
        serde_json::to_value(self.run_hook(input))
            .map_err(|error| HookHandlerError::new(error.to_string()))
    }
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

    pub fn declaration(&self) -> &ExtensionDescriptor {
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

pub fn declaration() -> ExtensionDescriptor {
    let mut declaration = ExtensionDescriptor::builtin(
        ExtensionId::new(PROVIDER_ID).expect("core guard extension id is valid"),
        "Core Guards",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("core guard declaration is valid")
    .with_hook(hook(JSON_VALIDATE_HOOK_ID, HookEvent::ModelResponse));
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
        declaration = declaration.with_hook(hook(id, HookEvent::ArtifactWrite));
    }
    declaration
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn hook(id: &str, event: HookEvent) -> HookDeclaration {
    HookDeclaration::new(HookId::new(id).expect("core guard hook id is valid"), event)
}

#[cfg(test)]
mod tests;
