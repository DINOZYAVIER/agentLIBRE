use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use agl_exec::{
    AuthorityFingerprint, CallerOwner, ExecutionId, ExecutionListFilter, ExecutionStatus, KillMode,
};
use agl_process::TerminalEndpoint;
use agl_terminal::{TerminalId, TerminalOperation, TerminalRecord, TerminalTopologyId};
use agl_terminal_protocol::TerminalAdmission;
use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

pub(crate) const DAEMON_TERMINAL_AUTHORITY: &str =
    "sha256:ba24b5b6f59bbde628b19fa8a23b6341f6a316a3c869cf4791de1e9269b4b6b6";

#[derive(Clone)]
pub(crate) struct TerminalBridge {
    endpoint: Arc<TerminalEndpoint>,
    authority: AuthorityFingerprint,
}

impl TerminalBridge {
    pub(crate) fn daemon(endpoint: TerminalEndpoint) -> Result<Self> {
        Ok(Self {
            endpoint: Arc::new(endpoint),
            authority: AuthorityFingerprint::new(DAEMON_TERMINAL_AUTHORITY)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?,
        })
    }

    pub(crate) fn authority(&self) -> AuthorityFingerprint {
        self.authority.clone()
    }

    pub(crate) fn with_authority(&self, authority: impl Into<String>) -> Result<Self> {
        Ok(Self {
            endpoint: Arc::clone(&self.endpoint),
            authority: AuthorityFingerprint::new(authority.into())
                .map_err(|error| anyhow::anyhow!(error.to_string()))?,
        })
    }

    pub(crate) fn execution_list(
        &self,
        filter: ExecutionListFilter,
    ) -> Result<Vec<ExecutionStatus>> {
        let endpoint = Arc::clone(&self.endpoint);
        let authority = self.authority.clone();
        run(move || async move {
            endpoint
                .connect(authority)?
                .list_executions(filter, CancellationToken::new())
                .await
                .map_err(Into::into)
        })
    }

    pub(crate) fn execution_status(&self, execution_id: ExecutionId) -> Result<ExecutionStatus> {
        let endpoint = Arc::clone(&self.endpoint);
        let authority = self.authority.clone();
        run(move || async move {
            endpoint
                .connect(authority)?
                .inspect_execution(execution_id, CancellationToken::new())
                .await
                .map_err(Into::into)
        })
    }

    pub(crate) fn terminate_execution(
        &self,
        execution_id: ExecutionId,
        mode: KillMode,
    ) -> Result<()> {
        let endpoint = Arc::clone(&self.endpoint);
        let authority = self.authority.clone();
        run(move || async move {
            endpoint
                .connect(authority)?
                .terminate_execution(execution_id, mode, CancellationToken::new())
                .await
                .map_err(Into::into)
        })
    }

    pub(crate) fn ensure(&self, mut admission: TerminalAdmission) -> Result<TerminalRecord> {
        admission.authority_fingerprint = self.authority.clone();
        admission.operations = all_terminal_operations();
        admission
            .seal_request_fingerprint()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let topology_id = admission.topology_id.clone();
        let endpoint = Arc::clone(&self.endpoint);
        let authority = self.authority.clone();
        run(move || async move {
            let client = endpoint.connect(authority)?;
            let descriptor = client.ensure(admission, CancellationToken::new()).await?;
            let records = client
                .list_topology(topology_id, CancellationToken::new())
                .await?;
            records
                .into_iter()
                .find(|record| record.terminal_id == descriptor.terminal_id)
                .context("terminal service omitted the ensured terminal from its topology")
        })
    }

    pub(crate) fn list_topology(
        &self,
        topology_id: TerminalTopologyId,
    ) -> Result<Vec<TerminalRecord>> {
        let endpoint = Arc::clone(&self.endpoint);
        let authority = self.authority.clone();
        run(move || async move {
            endpoint
                .connect(authority)?
                .list_topology(topology_id, CancellationToken::new())
                .await
                .map_err(Into::into)
        })
    }

    pub(crate) fn record(
        &self,
        topology_id: TerminalTopologyId,
        terminal_id: &TerminalId,
    ) -> Result<TerminalRecord> {
        self.list_topology(topology_id)?
            .into_iter()
            .find(|record| &record.terminal_id == terminal_id)
            .context("terminal is not present in its mapped topology")
    }

    pub(crate) fn retire(&self, terminal_id: TerminalId) -> Result<TerminalRecord> {
        let endpoint = Arc::clone(&self.endpoint);
        let authority = self.authority.clone();
        run(move || async move {
            endpoint
                .connect(authority)?
                .retire(terminal_id, CancellationToken::new())
                .await
                .map_err(Into::into)
        })
    }

    pub(crate) fn promote(
        &self,
        terminal_id: TerminalId,
        topology_id: TerminalTopologyId,
        owner: CallerOwner,
    ) -> Result<TerminalRecord> {
        let endpoint = Arc::clone(&self.endpoint);
        let authority = self.authority.clone();
        run(move || async move {
            endpoint
                .connect(authority)?
                .promote(terminal_id, topology_id, owner, CancellationToken::new())
                .await
                .map_err(Into::into)
        })
    }

    pub(crate) fn terminate(&self, terminal_id: TerminalId) -> Result<()> {
        let endpoint = Arc::clone(&self.endpoint);
        let authority = self.authority.clone();
        run(move || async move {
            endpoint
                .connect(authority)?
                .terminate(terminal_id, CancellationToken::new())
                .await
                .map_err(Into::into)
        })
    }
}

fn all_terminal_operations() -> BTreeSet<TerminalOperation> {
    BTreeSet::from([
        TerminalOperation::Inspect,
        TerminalOperation::Attach,
        TerminalOperation::Read,
        TerminalOperation::Write,
        TerminalOperation::Resize,
        TerminalOperation::Interrupt,
        TerminalOperation::Terminate,
    ])
}

fn run<T, F, Fut>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<T>> + Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to create terminal client runtime")?
            .block_on(operation())
    })
    .join()
    .map_err(|_| anyhow::anyhow!("terminal client thread panicked"))?
}
