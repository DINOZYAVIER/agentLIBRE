use agl_exec::{
    CallerNamespace, CallerOwner, CallerOwnerKind, CallerRole, ExecutionCorrelation,
    ExecutionOwner, ExecutionRequestId, OpaqueOwnerId,
};

pub(crate) type RunId = ExecutionRequestId;
pub(crate) type SessionId = ExecutionRequestId;
pub(crate) type StepId = ExecutionRequestId;

fn namespace() -> CallerNamespace {
    CallerNamespace::new("terminal-test", 1).expect("static caller namespace is valid")
}

fn opaque(value: &str) -> OpaqueOwnerId {
    OpaqueOwnerId::new(value).expect("generated test IDs fit the opaque owner contract")
}

pub(crate) fn session_owner(
    session_id: &SessionId,
    root_run_id: &RunId,
    role: CallerRole,
) -> ExecutionOwner {
    ExecutionOwner::new(
        CallerOwner::new(
            namespace(),
            opaque(session_id.as_str()),
            CallerOwnerKind::Persistent,
            role,
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

pub(crate) fn correlation(run_id: &RunId, step_id: &StepId) -> ExecutionCorrelation {
    ExecutionCorrelation::new(
        namespace(),
        opaque(run_id.as_str()),
        opaque(step_id.as_str()),
    )
}
