use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use uuid::{Uuid, Version};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseIdError {
    PrefixMismatch { expected: &'static str },
    InvalidUuid,
    NonCanonical,
    UnsupportedUuidVersion,
}

impl Display for ParseIdError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrefixMismatch { expected } => {
                write!(formatter, "ID must start with {expected}")
            }
            Self::InvalidUuid => formatter.write_str("ID payload must be a UUID"),
            Self::NonCanonical => {
                formatter.write_str("ID UUID must use canonical lowercase hyphenated form")
            }
            Self::UnsupportedUuidVersion => formatter.write_str("ID payload must be a UUIDv7"),
        }
    }
}

impl Error for ParseIdError {}

fn generate_id(prefix: &str) -> String {
    format!("{prefix}{}", Uuid::now_v7())
}

fn parse_id(value: &str, prefix: &'static str) -> Result<String, ParseIdError> {
    let payload = value
        .strip_prefix(prefix)
        .ok_or(ParseIdError::PrefixMismatch { expected: prefix })?;
    let uuid = Uuid::parse_str(payload).map_err(|_| ParseIdError::InvalidUuid)?;

    if payload != uuid.hyphenated().to_string() {
        return Err(ParseIdError::NonCanonical);
    }
    if uuid.get_version() != Some(Version::SortRand) {
        return Err(ParseIdError::UnsupportedUuidVersion);
    }

    Ok(value.to_owned())
}

macro_rules! define_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn generate() -> Self {
                Self(generate_id($prefix))
            }

            pub fn parse(value: &str) -> Result<Self, ParseIdError> {
                parse_id(value, $prefix).map(Self)
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Display for $name {
            fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(&value).map_err(D::Error::custom)
            }
        }
    };
}

define_id!(SessionId, "ses_");
define_id!(RunId, "run_");
define_id!(TurnId, "turn_");
define_id!(StepId, "step_");
define_id!(AttemptId, "attempt_");
define_id!(EventId, "evt_");
define_id!(RequestId, "req_");
define_id!(MessageId, "msg_");

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ExecutionScope {
    run_id: RunId,
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
    step_id: Option<StepId>,
    attempt_id: Option<AttemptId>,
}

impl ExecutionScope {
    pub fn builder(run_id: RunId) -> ExecutionScopeBuilder {
        ExecutionScopeBuilder::new(run_id)
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

impl<'de> Deserialize<'de> for ExecutionScope {
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
        ExecutionScopeBuilder {
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
pub struct ExecutionScopeBuilder {
    run_id: RunId,
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
    step_id: Option<StepId>,
    attempt_id: Option<AttemptId>,
}

impl ExecutionScopeBuilder {
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

    pub fn build(self) -> Result<ExecutionScope, ExecutionScopeError> {
        if self.attempt_id.is_some() && self.turn_id.is_none() {
            return Err(ExecutionScopeError::AttemptWithoutTurn);
        }

        Ok(ExecutionScope {
            run_id: self.run_id,
            session_id: self.session_id,
            turn_id: self.turn_id,
            step_id: self.step_id,
            attempt_id: self.attempt_id,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionScopeError {
    AttemptWithoutTurn,
}

impl Display for ExecutionScopeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::AttemptWithoutTurn => {
                formatter.write_str("an attempt ID requires a turn ID in the execution scope")
            }
        }
    }
}

impl Error for ExecutionScopeError {}

#[cfg(test)]
mod tests {
    use super::*;

    const EARLY_RUN_ID: &str = "run_01890f17-4a00-7000-8000-000000000001";
    const LATE_RUN_ID: &str = "run_01890f17-4a01-7000-8000-000000000000";
    const TURN_ID: &str = "turn_01890f17-4a00-7000-8000-000000000002";
    const ATTEMPT_ID: &str = "attempt_01890f17-4a00-7000-8000-000000000003";

    macro_rules! assert_id_contract {
        ($type:ty, $prefix:literal) => {{
            let generated = <$type>::generate();
            assert!(generated.as_str().starts_with($prefix));
            assert_eq!(generated.to_string(), generated.as_str());
            assert_eq!(generated.as_str().parse::<$type>().unwrap(), generated);

            let encoded = serde_json::to_string(&generated).unwrap();
            assert_eq!(serde_json::from_str::<$type>(&encoded).unwrap(), generated);
        }};
    }

    #[test]
    fn every_id_type_generates_and_round_trips() {
        assert_id_contract!(SessionId, "ses_");
        assert_id_contract!(RunId, "run_");
        assert_id_contract!(TurnId, "turn_");
        assert_id_contract!(StepId, "step_");
        assert_id_contract!(AttemptId, "attempt_");
        assert_id_contract!(EventId, "evt_");
        assert_id_contract!(RequestId, "req_");
        assert_id_contract!(MessageId, "msg_");
    }

    #[test]
    fn parsing_rejects_wrong_prefix_and_malformed_uuid() {
        assert_eq!(
            RunId::parse("turn_01890f17-4a00-7000-8000-000000000001"),
            Err(ParseIdError::PrefixMismatch { expected: "run_" })
        );
        assert_eq!(
            RunId::parse("run_not-a-uuid"),
            Err(ParseIdError::InvalidUuid)
        );
    }

    #[test]
    fn parsing_rejects_non_canonical_and_non_v7_uuids() {
        assert_eq!(
            RunId::parse("run_01890F17-4A00-7000-8000-000000000001"),
            Err(ParseIdError::NonCanonical)
        );
        assert_eq!(
            RunId::parse("run_01890f174a0070008000000000000001"),
            Err(ParseIdError::NonCanonical)
        );
        assert_eq!(
            RunId::parse("run_550e8400-e29b-41d4-a716-446655440000"),
            Err(ParseIdError::UnsupportedUuidVersion)
        );
    }

    #[test]
    fn deserialization_rejects_invalid_ids() {
        assert!(
            serde_json::from_str::<RunId>(r#""turn_01890f17-4a00-7000-8000-000000000001""#)
                .is_err()
        );
        assert!(serde_json::from_str::<RunId>(r#""run_not-a-uuid""#).is_err());
    }

    #[test]
    fn uuid_v7_timestamp_order_matches_id_order() {
        let early = RunId::parse(EARLY_RUN_ID).unwrap();
        let late = RunId::parse(LATE_RUN_ID).unwrap();

        assert!(early < late);
    }

    #[test]
    fn generated_ids_are_path_segment_safe_uuid_v7_values() {
        let generated = RunId::generate();
        let payload = generated.as_str().strip_prefix("run_").unwrap();
        let uuid = Uuid::parse_str(payload).unwrap();

        assert_eq!(uuid.get_version(), Some(Version::SortRand));
        assert!(
            generated
                .as_str()
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        );
    }

    #[test]
    fn execution_scope_requires_a_run_and_validates_attempt_parentage() {
        let run_id = RunId::parse(EARLY_RUN_ID).unwrap();
        let turn_id = TurnId::parse(TURN_ID).unwrap();
        let attempt_id = AttemptId::parse(ATTEMPT_ID).unwrap();

        assert_eq!(
            ExecutionScope::builder(run_id.clone())
                .attempt_id(attempt_id.clone())
                .build(),
            Err(ExecutionScopeError::AttemptWithoutTurn)
        );

        let scope = ExecutionScope::builder(run_id.clone())
            .turn_id(turn_id.clone())
            .attempt_id(attempt_id.clone())
            .build()
            .unwrap();
        assert_eq!(scope.run_id(), &run_id);
        assert_eq!(scope.turn_id(), Some(&turn_id));
        assert_eq!(scope.attempt_id(), Some(&attempt_id));
    }

    #[test]
    fn execution_scope_serde_round_trip_preserves_validation() {
        let scope = ExecutionScope::builder(RunId::parse(EARLY_RUN_ID).unwrap())
            .turn_id(TurnId::parse(TURN_ID).unwrap())
            .attempt_id(AttemptId::parse(ATTEMPT_ID).unwrap())
            .build()
            .unwrap();
        let encoded = serde_json::to_string(&scope).unwrap();

        assert_eq!(
            serde_json::from_str::<ExecutionScope>(&encoded).unwrap(),
            scope
        );
        assert!(
            serde_json::from_str::<ExecutionScope>(&format!(
                r#"{{"run_id":"{EARLY_RUN_ID}","session_id":null,"turn_id":null,"step_id":null,"attempt_id":"{ATTEMPT_ID}"}}"#
            ))
            .is_err()
        );
    }
}
