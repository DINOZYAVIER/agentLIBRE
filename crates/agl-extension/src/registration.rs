use std::sync::Arc;

use crate::{ExtensionDescriptor, ToolHandler, ToolId};

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
}

impl ExtensionRegistration {
    pub fn new(
        descriptor: ExtensionDescriptor,
        bindings: impl IntoIterator<Item = ToolBinding>,
    ) -> Self {
        Self {
            descriptor,
            bindings: bindings.into_iter().collect(),
        }
    }

    pub fn descriptor(&self) -> &ExtensionDescriptor {
        &self.descriptor
    }

    pub fn bindings(&self) -> &[ToolBinding] {
        &self.bindings
    }

    pub fn into_parts(self) -> (ExtensionDescriptor, Vec<ToolBinding>) {
        (self.descriptor, self.bindings)
    }
}
