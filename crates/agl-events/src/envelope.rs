use std::error::Error;
use std::fmt::{self, Display, Formatter};

use agl_ids::{AttemptId, EventId, RequestId, RunId, SessionId, StepId, TurnId};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

pub const EVENT_SCHEMA: &str = "agentlibre.event.v1alpha";

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct EventScope {
    run_id: RunId,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_id: Option<TurnId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    step_id: Option<StepId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt_id: Option<AttemptId>,
}

impl EventScope {
    pub fn builder(run_id: RunId) -> EventScopeBuilder {
        EventScopeBuilder::new(run_id)
    }

    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    pub fn turn_id(&self) -> Option<&TurnId> {
        self.turn_id.as_ref()
    }

    pub fn step_id(&self) -> Option<&StepId> {
        self.step_id.as_ref()
    }

    pub fn attempt_id(&self) -> Option<&AttemptId> {
        self.attempt_id.as_ref()
    }
}

impl<'de> Deserialize<'de> for EventScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            run_id: RunId,
            session_id: Option<SessionId>,
            turn_id: Option<TurnId>,
            step_id: Option<StepId>,
            attempt_id: Option<AttemptId>,
        }

        let fields = Fields::deserialize(deserializer)?;
        EventScopeBuilder {
            run_id: fields.run_id,
            session_id: fields.session_id,
            turn_id: fields.turn_id,
            step_id: fields.step_id,
            attempt_id: fields.attempt_id,
        }
        .build()
        .map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug)]
pub struct EventScopeBuilder {
    run_id: RunId,
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
    step_id: Option<StepId>,
    attempt_id: Option<AttemptId>,
}

impl EventScopeBuilder {
    pub fn new(run_id: RunId) -> Self {
        Self {
            run_id,
            session_id: None,
            turn_id: None,
            step_id: None,
            attempt_id: None,
        }
    }

    pub fn session_id(mut self, session_id: SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub fn turn_id(mut self, turn_id: TurnId) -> Self {
        self.turn_id = Some(turn_id);
        self
    }

    pub fn step_id(mut self, step_id: StepId) -> Self {
        self.step_id = Some(step_id);
        self
    }

    pub fn attempt_id(mut self, attempt_id: AttemptId) -> Self {
        self.attempt_id = Some(attempt_id);
        self
    }

    pub fn build(self) -> Result<EventScope, EventScopeError> {
        if self.attempt_id.is_some() && self.turn_id.is_none() {
            return Err(EventScopeError::AttemptWithoutTurn);
        }

        Ok(EventScope {
            run_id: self.run_id,
            session_id: self.session_id,
            turn_id: self.turn_id,
            step_id: self.step_id,
            attempt_id: self.attempt_id,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventScopeError {
    AttemptWithoutTurn,
}

impl Display for EventScopeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::AttemptWithoutTurn => {
                formatter.write_str("an attempt ID requires a turn ID in the event scope")
            }
        }
    }
}

impl Error for EventScopeError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventDraft<P> {
    pub scope: EventScope,
    pub request_id: Option<RequestId>,
    pub caused_by: Option<EventId>,
    pub payload: P,
}

impl<P> EventDraft<P> {
    pub fn new(scope: EventScope, payload: P) -> Self {
        Self {
            scope,
            request_id: None,
            caused_by: None,
            payload,
        }
    }

    pub fn with_request_id(mut self, request_id: RequestId) -> Self {
        self.request_id = Some(request_id);
        self
    }

    pub fn with_causation(mut self, caused_by: EventId) -> Self {
        self.caused_by = Some(caused_by);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EventEnvelope<P> {
    pub schema: String,
    pub event_id: EventId,
    pub sequence: u64,
    pub occurred_at_unix_ms: u64,
    pub scope: EventScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<EventId>,
    pub payload: P,
}

impl<P> EventEnvelope<P> {
    pub fn validate(&self) -> Result<(), EnvelopeValidationError> {
        if self.schema != EVENT_SCHEMA {
            return Err(EnvelopeValidationError::UnsupportedSchema(
                self.schema.clone(),
            ));
        }
        if self.sequence == 0 {
            return Err(EnvelopeValidationError::ZeroSequence);
        }
        Ok(())
    }

    pub fn map_payload<Q>(self, map: impl FnOnce(P) -> Q) -> EventEnvelope<Q> {
        EventEnvelope {
            schema: self.schema,
            event_id: self.event_id,
            sequence: self.sequence,
            occurred_at_unix_ms: self.occurred_at_unix_ms,
            scope: self.scope,
            request_id: self.request_id,
            caused_by: self.caused_by,
            payload: map(self.payload),
        }
    }
}

impl<'de, P> Deserialize<'de> for EventEnvelope<P>
where
    P: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields<P> {
            schema: String,
            event_id: EventId,
            sequence: u64,
            occurred_at_unix_ms: u64,
            scope: EventScope,
            request_id: Option<RequestId>,
            caused_by: Option<EventId>,
            payload: P,
        }

        let fields = Fields::deserialize(deserializer)?;
        let envelope = Self {
            schema: fields.schema,
            event_id: fields.event_id,
            sequence: fields.sequence,
            occurred_at_unix_ms: fields.occurred_at_unix_ms,
            scope: fields.scope,
            request_id: fields.request_id,
            caused_by: fields.caused_by,
            payload: fields.payload,
        };
        envelope.validate().map_err(D::Error::custom)?;
        Ok(envelope)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnvelopeValidationError {
    UnsupportedSchema(String),
    ZeroSequence,
}

impl Display for EnvelopeValidationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema(schema) => write!(
                formatter,
                "unsupported event schema `{schema}`; expected `{EVENT_SCHEMA}`"
            ),
            Self::ZeroSequence => formatter.write_str("event sequence must be greater than zero"),
        }
    }
}

impl Error for EnvelopeValidationError {}
