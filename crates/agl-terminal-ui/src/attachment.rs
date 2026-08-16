use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) struct AttachmentStarted {
    pub(super) writable: bool,
}

pub(super) struct ExecutionAttachment {
    access: TerminalAccess,
    terminal_id: agl_terminal::TerminalId,
    pub(super) stream_id: agl_terminal::TerminalStreamId,
    next_sequence: u64,
    pending: VecDeque<ExecutionAttachmentEvent>,
    pub(super) started: AttachmentStarted,
}

#[derive(Debug)]
pub(super) enum ExecutionAttachmentEvent {
    Output(AttachmentOutputEvent),
    Finished(AttachmentFinishedEvent),
}

#[derive(Debug)]
pub(super) struct AttachmentOutputEvent {
    pub(super) chunk: agl_exec::ExecutionOutputChunk,
}

#[derive(Debug)]
pub(super) struct AttachmentFinishedEvent {
    pub(super) state: agl_exec::ExecutionState,
    pub(super) last_delivered_sequence: u64,
}

impl ExecutionAttachment {
    pub(super) async fn attach(
        terminal_id: &TerminalId,
        after_sequence: u64,
        writable: bool,
    ) -> Result<Self> {
        let access = terminal_access()?.clone();
        let terminal_id = agl_terminal::TerminalId::parse(terminal_id.as_str())
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let attached = access
            .client()?
            .attach(
                terminal_id.clone(),
                after_sequence,
                writable,
                tokio_util::sync::CancellationToken::new(),
            )
            .await?;
        Ok(Self {
            access,
            terminal_id,
            stream_id: attached.stream_id,
            next_sequence: after_sequence,
            pending: VecDeque::new(),
            started: AttachmentStarted {
                writable: attached.writable,
            },
        })
    }

    pub(super) async fn next(
        &mut self,
    ) -> std::result::Result<Option<ExecutionAttachmentEvent>, agl_terminal_client::ClientError>
    {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            let batch = self
                .access
                .client()
                .map_err(|error| {
                    agl_terminal_client::ClientError::Transport(
                        agl_terminal_client::TransportError::Unavailable(error.to_string()),
                    )
                })?
                .read_events(
                    self.stream_id.clone(),
                    self.next_sequence,
                    64,
                    tokio_util::sync::CancellationToken::new(),
                )
                .await?;
            self.next_sequence = batch.next_sequence;
            for event in batch.events {
                match event.event {
                    TerminalEventKind::Output { bytes } => self.pending.push_back(
                        ExecutionAttachmentEvent::Output(AttachmentOutputEvent {
                            chunk: agl_exec::ExecutionOutputChunk {
                                sequence: event.sequence,
                                channel: agl_exec::ExecutionChannel::Terminal,
                                bytes,
                            },
                        }),
                    ),
                    TerminalEventKind::StateChanged { descriptor }
                        if descriptor.state.is_final() =>
                    {
                        self.pending.push_back(ExecutionAttachmentEvent::Finished(
                            AttachmentFinishedEvent {
                                state: terminal_state_to_execution(descriptor.state),
                                last_delivered_sequence: event.sequence,
                            },
                        ));
                    }
                    TerminalEventKind::StreamClosed => self.pending.push_back(
                        ExecutionAttachmentEvent::Finished(AttachmentFinishedEvent {
                            state: agl_exec::ExecutionState::OutcomeUnknown,
                            last_delivered_sequence: event.sequence,
                        }),
                    ),
                    _ => {}
                }
            }
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            if batch.stream_closed {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub(super) async fn input(&self, bytes: agl_exec::ProcessBytes, _eof: bool) -> Result<()> {
        self.access
            .client()?
            .input(
                self.terminal_id.clone(),
                self.stream_id.clone(),
                bytes,
                tokio_util::sync::CancellationToken::new(),
            )
            .await?;
        Ok(())
    }

    pub(super) async fn resize(&self, columns: u16, rows: u16) -> Result<()> {
        self.access
            .client()?
            .resize(
                self.terminal_id.clone(),
                agl_exec::TerminalSize { columns, rows },
                tokio_util::sync::CancellationToken::new(),
            )
            .await?;
        Ok(())
    }

    pub(super) async fn detach(&self) -> Result<()> {
        self.access
            .client()?
            .detach(
                self.stream_id.clone(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await?;
        Ok(())
    }
}

fn terminal_state_to_execution(state: agl_terminal::TerminalState) -> agl_exec::ExecutionState {
    match state {
        agl_terminal::TerminalState::Reserved => agl_exec::ExecutionState::Admitting,
        agl_terminal::TerminalState::Starting => agl_exec::ExecutionState::Starting,
        agl_terminal::TerminalState::Running => agl_exec::ExecutionState::Running,
        agl_terminal::TerminalState::Stopping => agl_exec::ExecutionState::Running,
        agl_terminal::TerminalState::Exited => agl_exec::ExecutionState::Exited,
        agl_terminal::TerminalState::Failed => agl_exec::ExecutionState::Failed,
        agl_terminal::TerminalState::OutcomeUnknown => agl_exec::ExecutionState::OutcomeUnknown,
    }
}
