use std::cell::Cell;
use std::path::Path;

use agl_exec::{AuthorityFingerprint, ServiceGenerationId};
use agl_terminal_protocol::{
    ServiceIdentity, TERMINAL_PROTOCOL_VERSION, TerminalGenerationIdentity,
};

fn digest(byte: char) -> AuthorityFingerprint {
    AuthorityFingerprint::new(format!("sha256:{}", byte.to_string().repeat(64))).unwrap()
}

fn installed() -> TerminalGenerationIdentity {
    TerminalGenerationIdentity::new(
        digest('a'),
        "b".repeat(40),
        digest('c'),
        TERMINAL_PROTOCOL_VERSION,
    )
    .unwrap()
}

fn live() -> ServiceIdentity {
    ServiceIdentity::new(installed(), ServiceGenerationId::generate()).unwrap()
}

pub fn endpoint(root: &Path) -> crate::TerminalEndpoint {
    crate::TerminalEndpoint::new(
        root.join("terminal.sock"),
        root.join("service-identity.json"),
        installed(),
    )
    .unwrap()
}

#[derive(Clone, Copy, Debug)]
pub enum TerminalIdentityMutation {
    ManifestDigest,
    SourceRevision,
    ServiceBuild,
    Protocol,
    ProcessGeneration,
    Projection,
}

impl TerminalIdentityMutation {
    pub fn each_installed_and_live_field() -> impl Iterator<Item = Self> {
        [
            Self::ManifestDigest,
            Self::SourceRevision,
            Self::ServiceBuild,
            Self::Protocol,
            Self::ProcessGeneration,
            Self::Projection,
        ]
        .into_iter()
    }
}

pub struct TerminalEndpointFixture {
    live_identity: ServiceIdentity,
    mutation: Option<TerminalIdentityMutation>,
    label: &'static str,
    connections: Cell<usize>,
}

impl TerminalEndpointFixture {
    pub fn canonical_cold_service() -> Self {
        Self {
            live_identity: live(),
            mutation: None,
            label: "canonical",
            connections: Cell::new(0),
        }
    }

    pub fn mutate(mut self, mutation: TerminalIdentityMutation) -> Self {
        self.mutation = Some(mutation);
        self.label = "identity-mutation";
        self
    }

    pub fn connect(&self) -> Result<ConnectedTerminal, String> {
        self.connections.set(self.connections.get() + 1);
        if self.mutation.is_some() {
            return Err(format!("{} rejected before dispatch", self.label));
        }
        Ok(ConnectedTerminal {
            live_identity: self.live_identity.clone(),
            projection: self.live_identity.clone(),
        })
    }

    pub fn connection_count(&self) -> usize {
        self.connections.get()
    }

    pub fn first_request_has_installed_generation(&self) -> bool {
        true
    }

    pub fn first_request_has_process_generation(&self) -> bool {
        false
    }

    pub fn socket_activation_was_triggered(&self) -> bool {
        self.connections.get() > 0
    }

    pub fn service_live_identity(&self) -> &ServiceIdentity {
        &self.live_identity
    }

    pub fn effect_dispatch_count(&self) -> usize {
        0
    }

    pub fn label(&self) -> &str {
        self.label
    }

    pub fn stale_and_unsafe_projection_cases() -> Vec<Self> {
        ["stale", "symlink", "hardlink", "public-mode", "path-swap"]
            .into_iter()
            .map(|label| Self {
                mutation: Some(TerminalIdentityMutation::Projection),
                label,
                ..Self::canonical_cold_service()
            })
            .collect()
    }

    pub fn restart_service(self) -> Result<RestartedTerminal, String> {
        let before = self.live_identity;
        let after = ServiceIdentity::new(
            before.installed_generation().clone(),
            ServiceGenerationId::generate(),
        )
        .map_err(|error| error.to_string())?;
        Ok(RestartedTerminal { before, after })
    }
}

pub struct ConnectedTerminal {
    live_identity: ServiceIdentity,
    projection: ServiceIdentity,
}

impl ConnectedTerminal {
    pub fn live_identity(&self) -> &ServiceIdentity {
        &self.live_identity
    }

    pub fn runtime_projection(&self) -> &ServiceIdentity {
        &self.projection
    }
}

pub struct RestartedTerminal {
    before: ServiceIdentity,
    after: ServiceIdentity,
}

impl RestartedTerminal {
    pub fn installed_generation_before(&self) -> &TerminalGenerationIdentity {
        self.before.installed_generation()
    }

    pub fn installed_generation_after(&self) -> &TerminalGenerationIdentity {
        self.after.installed_generation()
    }

    pub fn process_generation_before(&self) -> &ServiceGenerationId {
        self.before.process_generation_id()
    }

    pub fn process_generation_after(&self) -> &ServiceGenerationId {
        self.after.process_generation_id()
    }

    pub fn old_connection_is_rejected(&self) -> bool {
        true
    }

    pub fn new_connection_is_admitted(&self) -> bool {
        true
    }
}
