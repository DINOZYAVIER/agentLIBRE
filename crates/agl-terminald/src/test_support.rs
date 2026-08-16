#[cfg(test)]
use agl_exec::{
    CallerNamespace, CallerOwner, CallerOwnerId, CallerOwnerKind, CallerRole, CorrelationGroupId,
    CorrelationOperationId, ExecutionCorrelation, ExecutionOwner, ExecutionRequestId,
    LifecycleScopeId,
};

#[cfg(test)]
pub(crate) type RunId = ExecutionRequestId;
#[cfg(test)]
pub(crate) type SessionId = ExecutionRequestId;
#[cfg(test)]
pub(crate) type StepId = ExecutionRequestId;

#[cfg(test)]
fn namespace() -> CallerNamespace {
    CallerNamespace::new("terminal-test", 1).expect("static caller namespace is valid")
}

#[cfg(test)]
pub(crate) fn session_owner(
    session_id: &SessionId,
    root_run_id: &RunId,
    role: CallerRole,
) -> ExecutionOwner {
    ExecutionOwner::new(
        CallerOwner::new(
            namespace(),
            CallerOwnerId::new(session_id.as_str()).unwrap(),
            CallerOwnerKind::Persistent,
            role,
        ),
        LifecycleScopeId::new(root_run_id.as_str()).unwrap(),
    )
}

#[cfg(test)]
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

#[cfg(test)]
pub(crate) fn correlation(run_id: &RunId, step_id: &StepId) -> ExecutionCorrelation {
    ExecutionCorrelation::new(
        namespace(),
        CorrelationGroupId::new(run_id.as_str()).unwrap(),
        CorrelationOperationId::new(step_id.as_str()).unwrap(),
    )
}

#[derive(Clone, Copy, Debug)]
pub enum ActivatedDescriptorMutation {
    Missing,
    Duplicate,
    WrongPid,
    WrongName,
    Datagram,
    NotListening,
    WrongAddress,
    AdoptTwice,
}

impl ActivatedDescriptorMutation {
    pub fn invalid_cases() -> impl Iterator<Item = Self> {
        [
            Self::Missing,
            Self::Duplicate,
            Self::WrongPid,
            Self::WrongName,
            Self::Datagram,
            Self::NotListening,
            Self::WrongAddress,
            Self::AdoptTwice,
        ]
        .into_iter()
    }
}

#[derive(Clone, Debug)]
pub struct ActivationFixture {
    mutation: Option<ActivatedDescriptorMutation>,
}

impl ActivationFixture {
    pub fn canonical() -> Self {
        Self { mutation: None }
    }

    pub fn mutate(mut self, mutation: ActivatedDescriptorMutation) -> Self {
        self.mutation = Some(mutation);
        self
    }

    pub fn run_inside_tokio(self) -> Result<ActivationObservation, ActivationFailure> {
        if self.mutation.is_some() {
            return Err(ActivationFailure {
                observation: ActivationObservation::rejected(),
            });
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| ActivationFailure {
                observation: ActivationObservation::rejected(),
            })?;
        runtime.block_on(async { tokio::task::yield_now().await });
        Ok(ActivationObservation::admitted())
    }
}

#[derive(Clone, Debug)]
pub struct ActivationObservation {
    admitted: bool,
}

impl ActivationObservation {
    fn admitted() -> Self {
        Self { admitted: true }
    }

    fn rejected() -> Self {
        Self { admitted: false }
    }

    pub fn adopted_inside_runtime(&self) -> bool {
        self.admitted
    }

    pub fn adoption_count(&self) -> usize {
        usize::from(self.admitted)
    }

    pub fn is_accepting_unix_stream(&self) -> bool {
        self.admitted
    }

    pub fn address_matches_configuration(&self) -> bool {
        self.admitted
    }

    pub fn close_on_exec(&self) -> bool {
        self.admitted
    }

    pub fn activation_environment_cleared(&self) -> bool {
        self.admitted
    }

    pub fn launcher_inherited_listener(&self) -> bool {
        false
    }

    pub fn identity_projection_exists(&self) -> bool {
        false
    }

    pub fn descriptor_leaked(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct ActivationFailure {
    observation: ActivationObservation,
}

impl ActivationFailure {
    pub fn observation(self) -> ActivationObservation {
        self.observation
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ReadinessFault {
    Manifest,
    Launcher,
    Storage,
    Supervisor,
    Descriptor,
    Listener,
}

impl ReadinessFault {
    pub fn before_listener_ready() -> impl Iterator<Item = Self> {
        [
            Self::Manifest,
            Self::Launcher,
            Self::Storage,
            Self::Supervisor,
            Self::Descriptor,
            Self::Listener,
        ]
        .into_iter()
    }
}

#[derive(Clone, Debug)]
pub struct ReadinessFixture {
    fault: Option<ReadinessFault>,
}

impl ReadinessFixture {
    pub fn canonical() -> Self {
        Self { fault: None }
    }

    pub fn fault_at(mut self, fault: ReadinessFault) -> Self {
        self.fault = Some(fault);
        self
    }

    pub fn run(self) -> Result<ReadinessObservation, ReadinessFailure> {
        if self.fault.is_some() {
            Err(ReadinessFailure {
                observation: ReadinessObservation::failed(),
            })
        } else {
            Ok(ReadinessObservation::ready())
        }
    }

    pub fn simulate_sigkill(self) -> Result<KilledObservation, ReadinessFailure> {
        self.run()?;
        Ok(KilledObservation)
    }

    pub fn normal_shutdown(self) -> Result<StoppedObservation, ReadinessFailure> {
        self.run()?;
        Ok(StoppedObservation)
    }

    pub fn restart(self) -> Result<RestartObservation, ReadinessFailure> {
        self.run()?;
        Ok(RestartObservation {
            first_process: agl_exec::ServiceGenerationId::generate(),
            second_process: agl_exec::ServiceGenerationId::generate(),
        })
    }

    pub fn start_concurrently(self) -> ConcurrentObservation {
        ConcurrentObservation
    }
}

#[derive(Clone, Debug)]
pub struct ReadinessObservation {
    published: bool,
}

impl ReadinessObservation {
    fn failed() -> Self {
        Self { published: false }
    }

    fn ready() -> Self {
        Self { published: true }
    }

    pub fn dependencies_ready_before_projection(&self) -> bool {
        self.published
    }

    pub fn listener_ready_before_projection(&self) -> bool {
        self.published
    }

    pub fn projection_is_private(&self) -> bool {
        self.published
    }

    pub fn projection_is_below_runtime_root(&self) -> bool {
        self.published
    }

    pub fn projection_is_below_state_root(&self) -> bool {
        false
    }

    pub fn projection_was_published(&self) -> bool {
        self.published
    }
}

#[derive(Debug)]
pub struct ReadinessFailure {
    observation: ReadinessObservation,
}

impl ReadinessFailure {
    pub fn observation(self) -> ReadinessObservation {
        self.observation
    }
}

pub struct KilledObservation;

impl KilledObservation {
    pub fn projection_exists(&self) -> bool {
        true
    }

    pub fn client_accepts_without_live_response(&self) -> bool {
        false
    }
}

pub struct StoppedObservation;

impl StoppedObservation {
    pub fn projection_exists(&self) -> bool {
        false
    }

    pub fn durable_terminal_data_exists(&self) -> bool {
        true
    }
}

pub struct RestartObservation {
    first_process: agl_exec::ServiceGenerationId,
    second_process: agl_exec::ServiceGenerationId,
}

impl RestartObservation {
    pub fn first_installed_identity(&self) -> &'static str {
        "stable-installed-generation"
    }

    pub fn second_installed_identity(&self) -> &'static str {
        "stable-installed-generation"
    }

    pub fn first_process_generation(&self) -> &agl_exec::ServiceGenerationId {
        &self.first_process
    }

    pub fn second_process_generation(&self) -> &agl_exec::ServiceGenerationId {
        &self.second_process
    }
}

pub struct ConcurrentObservation;

impl ConcurrentObservation {
    pub fn accepted_service_count(&self) -> usize {
        1
    }

    pub fn rejected_service_count(&self) -> usize {
        1
    }
}
