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
    ToolProviderDeclaration::new(
        ToolProviderId::new(PROVIDER_ID).expect("core guard provider id is valid"),
        "Core Guards",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("core guard declaration is valid")
    .with_hook(HookDeclaration {
        id: HookId::new(JSON_VALIDATE_HOOK_ID).expect("json hook id is valid"),
        event: HookEvent::ModelResponse,
        required: false,
    })
    .with_hook(HookDeclaration {
        id: HookId::new(REPO_PATH_VALIDATE_HOOK_ID).expect("repo path hook id is valid"),
        event: HookEvent::ArtifactWrite,
        required: true,
    })
    .with_hook(HookDeclaration {
        id: HookId::new(TASK_SPEC_VALIDATE_HOOK_ID).expect("task spec hook id is valid"),
        event: HookEvent::ArtifactWrite,
        required: true,
    })
    .with_hook(HookDeclaration {
        id: HookId::new(SECRET_SCAN_VALIDATE_HOOK_ID).expect("secret scan hook id is valid"),
        event: HookEvent::ArtifactWrite,
        required: true,
    })
    .with_hook(HookDeclaration {
        id: HookId::new(DIFF_SCOPE_VALIDATE_HOOK_ID).expect("diff scope hook id is valid"),
        event: HookEvent::ArtifactWrite,
        required: true,
    })
    .with_hook(HookDeclaration {
        id: HookId::new(VERIFICATION_VALIDATE_HOOK_ID).expect("verification hook id is valid"),
        event: HookEvent::ArtifactWrite,
        required: true,
    })
    .with_hook(HookDeclaration {
        id: HookId::new(COMMIT_MESSAGE_VALIDATE_HOOK_ID).expect("commit message hook id is valid"),
        event: HookEvent::ArtifactWrite,
        required: true,
    })
    .with_hook(HookDeclaration {
        id: HookId::new(SKILL_MANIFEST_VALIDATE_HOOK_ID).expect("skill manifest hook id is valid"),
        event: HookEvent::ArtifactWrite,
        required: true,
    })
    .with_hook(HookDeclaration {
        id: HookId::new(REVIEW_PACK_VALIDATE_HOOK_ID).expect("review pack hook id is valid"),
        event: HookEvent::ArtifactWrite,
        required: true,
    })
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

#[cfg(test)]
mod tests;
