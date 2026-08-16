use agl_exec::{
    CallerNamespace, CallerOwner, CallerOwnerId, CallerOwnerKind, CallerRole, ExecutionOwner,
    LifecycleScopeId,
};
use agl_ids::{RunId, SessionId};

fn namespace() -> CallerNamespace {
    CallerNamespace::new("agentlibre", 1).expect("static caller namespace is valid")
}

pub(crate) fn session_owner(session_id: &SessionId, root_run_id: &RunId) -> ExecutionOwner {
    ExecutionOwner::new(
        CallerOwner::new(
            namespace(),
            CallerOwnerId::new(session_id.as_str()).unwrap(),
            CallerOwnerKind::Persistent,
            CallerRole::Agent,
        ),
        LifecycleScopeId::new(root_run_id.as_str()).unwrap(),
    )
}

pub(crate) fn run_owner(run_id: &RunId, root_run_id: &RunId) -> ExecutionOwner {
    ExecutionOwner::new(
        CallerOwner::new(
            namespace(),
            CallerOwnerId::new(run_id.as_str()).unwrap(),
            CallerOwnerKind::Ephemeral,
            CallerRole::Agent,
        ),
        LifecycleScopeId::new(root_run_id.as_str()).unwrap(),
    )
}
