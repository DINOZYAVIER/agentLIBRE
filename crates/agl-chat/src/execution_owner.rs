use agl_exec::{
    CallerNamespace, CallerOwner, CallerOwnerKind, CallerRole, ExecutionOwner, OpaqueOwnerId,
};
use agl_ids::{RunId, SessionId};

fn namespace() -> CallerNamespace {
    CallerNamespace::new("agentlibre", 1).expect("static caller namespace is valid")
}

fn opaque(value: &str) -> OpaqueOwnerId {
    OpaqueOwnerId::new(value).expect("canonical agent ID fits the opaque owner contract")
}

pub(crate) fn session_owner(session_id: &SessionId, root_run_id: &RunId) -> ExecutionOwner {
    ExecutionOwner::new(
        CallerOwner::new(
            namespace(),
            opaque(session_id.as_str()),
            CallerOwnerKind::Persistent,
            CallerRole::Agent,
        ),
        opaque(root_run_id.as_str()),
    )
}

pub(crate) fn run_owner(run_id: &RunId, root_run_id: &RunId) -> ExecutionOwner {
    ExecutionOwner::new(
        CallerOwner::new(
            namespace(),
            opaque(run_id.as_str()),
            CallerOwnerKind::Ephemeral,
            CallerRole::Agent,
        ),
        opaque(root_run_id.as_str()),
    )
}
