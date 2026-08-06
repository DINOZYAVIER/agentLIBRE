use std::sync::Arc;

use crate::{
    ExtensionDescriptor, HookDeclaration, HookHandler, HookId, HookInvocationError, ToolHandler,
    ToolId,
};

pub struct ToolBinding {
    tool_id: ToolId,
    handler: Arc<dyn ToolHandler>,
}

impl ToolBinding {
    pub fn new(tool_id: ToolId, handler: impl ToolHandler + 'static) -> Self {
        Self {
            tool_id,
            handler: Arc::new(handler),
        }
    }

    pub fn from_shared(tool_id: ToolId, handler: Arc<dyn ToolHandler>) -> Self {
        Self { tool_id, handler }
    }

    pub fn tool_id(&self) -> &ToolId {
        &self.tool_id
    }

    pub fn handler(&self) -> &Arc<dyn ToolHandler> {
        &self.handler
    }

    pub fn into_parts(self) -> (ToolId, Arc<dyn ToolHandler>) {
        (self.tool_id, self.handler)
    }
}

pub struct ExtensionRegistration {
    descriptor: ExtensionDescriptor,
    bindings: Vec<ToolBinding>,
    hook_bindings: Vec<HookBinding>,
}

impl ExtensionRegistration {
    pub fn new(
        descriptor: ExtensionDescriptor,
        bindings: impl IntoIterator<Item = ToolBinding>,
    ) -> Self {
        Self {
            descriptor,
            bindings: bindings.into_iter().collect(),
            hook_bindings: Vec::new(),
        }
    }

    pub fn with_hook_bindings(mut self, bindings: impl IntoIterator<Item = HookBinding>) -> Self {
        self.hook_bindings = bindings.into_iter().collect();
        self
    }

    pub fn descriptor(&self) -> &ExtensionDescriptor {
        &self.descriptor
    }

    pub fn bindings(&self) -> &[ToolBinding] {
        &self.bindings
    }

    pub fn hook_bindings(&self) -> &[HookBinding] {
        &self.hook_bindings
    }

    pub fn into_parts(self) -> (ExtensionDescriptor, Vec<ToolBinding>, Vec<HookBinding>) {
        (self.descriptor, self.bindings, self.hook_bindings)
    }
}

pub struct HookBinding {
    hook_id: HookId,
    handler: Arc<dyn HookHandler>,
}

impl HookBinding {
    pub fn new(hook_id: HookId, handler: impl HookHandler + 'static) -> Self {
        Self {
            hook_id,
            handler: Arc::new(handler),
        }
    }

    pub fn hook_id(&self) -> &HookId {
        &self.hook_id
    }

    pub fn invoke(
        &self,
        declaration: &HookDeclaration,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, HookInvocationError> {
        if self.hook_id != declaration.id {
            return Err(HookInvocationError::BindingMismatch);
        }
        crate::hook_contract::invoke_bound_hook(declaration, self.handler.as_ref(), payload)
    }

    pub fn into_parts(self) -> (HookId, Arc<dyn HookHandler>) {
        (self.hook_id, self.handler)
    }
}
