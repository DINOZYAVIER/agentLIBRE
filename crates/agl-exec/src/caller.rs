use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

pub const MAX_CALLER_NAMESPACE_BYTES: usize = 64;
pub const MAX_CALLER_ID_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallerIdentityError {
    Empty {
        field: &'static str,
    },
    TooLong {
        field: &'static str,
        max_bytes: usize,
    },
    ControlCharacter {
        field: &'static str,
    },
    ZeroNamespaceVersion,
    InvalidAuthorityFingerprint,
}

impl Display for CallerIdentityError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { field } => write!(formatter, "{field} must not be empty"),
            Self::TooLong { field, max_bytes } => {
                write!(formatter, "{field} must not exceed {max_bytes} UTF-8 bytes")
            }
            Self::ControlCharacter { field } => {
                write!(formatter, "{field} must not contain control characters")
            }
            Self::ZeroNamespaceVersion => {
                formatter.write_str("caller namespace version must be nonzero")
            }
            Self::InvalidAuthorityFingerprint => formatter.write_str(
                "authority fingerprint must be sha256: followed by 64 lowercase hex digits",
            ),
        }
    }
}

impl Error for CallerIdentityError {}

fn validate_bounded_text(
    value: &str,
    field: &'static str,
    max_bytes: usize,
) -> Result<(), CallerIdentityError> {
    if value.trim().is_empty() {
        return Err(CallerIdentityError::Empty { field });
    }
    if value.len() > max_bytes {
        return Err(CallerIdentityError::TooLong { field, max_bytes });
    }
    if value.chars().any(char::is_control) {
        return Err(CallerIdentityError::ControlCharacter { field });
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CallerNamespace {
    name: String,
    version: u32,
}

impl CallerNamespace {
    pub fn new(name: impl Into<String>, version: u32) -> Result<Self, CallerIdentityError> {
        let name = name.into();
        validate_bounded_text(&name, "caller namespace", MAX_CALLER_NAMESPACE_BYTES)?;
        if version == 0 {
            return Err(CallerIdentityError::ZeroNamespaceVersion);
        }
        Ok(Self { name, version })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> u32 {
        self.version
    }
}

impl<'de> Deserialize<'de> for CallerNamespace {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            name: String,
            version: u32,
        }

        let fields = Fields::deserialize(deserializer)?;
        Self::new(fields.name, fields.version).map_err(D::Error::custom)
    }
}

macro_rules! define_caller_text_id {
    ($(#[$attribute:meta])* $name:ident, $field:literal) => {
        $(#[$attribute])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, CallerIdentityError> {
                let value = value.into();
                validate_bounded_text(&value, $field, MAX_CALLER_ID_BYTES)?;
                Ok(Self(value))
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
                Self::new(value).map_err(D::Error::custom)
            }
        }
    };
}

define_caller_text_id!(CallerOwnerId, "caller owner ID");
define_caller_text_id!(
    /// Identity used only to fence lifecycle ownership.
    ///
    /// The semantic ID types are intentionally not interchangeable:
    ///
    /// ```compile_fail
    /// use agl_exec::{CallerOwnerId, LifecycleScopeId};
    ///
    /// fn accepts_lifecycle_scope(_: LifecycleScopeId) {}
    /// let owner = CallerOwnerId::new("owner").unwrap();
    /// accepts_lifecycle_scope(owner);
    /// ```
    LifecycleScopeId,
    "lifecycle scope ID"
);
define_caller_text_id!(CorrelationGroupId, "correlation group ID");
define_caller_text_id!(CorrelationOperationId, "correlation operation ID");

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallerOwnerKind {
    Persistent,
    Ephemeral,
    Service,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallerRole {
    Human,
    Agent,
    Service,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CallerOwner {
    namespace: CallerNamespace,
    owner_id: CallerOwnerId,
    owner_kind: CallerOwnerKind,
    role: CallerRole,
}

/// Runtime ownership admitted by a caller. `lifecycle_scope_id` is opaque to the
/// execution service and exists only to fence lifecycle-owner handoff; no
/// authority is inferred from its text.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionOwner {
    caller: CallerOwner,
    lifecycle_scope_id: LifecycleScopeId,
}

impl ExecutionOwner {
    pub fn new(caller: CallerOwner, lifecycle_scope_id: LifecycleScopeId) -> Self {
        Self {
            caller,
            lifecycle_scope_id,
        }
    }

    pub fn caller(&self) -> &CallerOwner {
        &self.caller
    }

    pub fn lifecycle_scope_id(&self) -> &LifecycleScopeId {
        &self.lifecycle_scope_id
    }

    pub fn may_access(&self, requester: &Self) -> bool {
        self.caller.namespace == requester.caller.namespace
            && self.caller.owner_id == requester.caller.owner_id
            && self.caller.owner_kind == requester.caller.owner_kind
    }
}

/// Opaque caller correlation retained for cancellation and recovery mapping.
/// The execution service compares these values but never parses agent IDs.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCorrelation {
    namespace: CallerNamespace,
    group_id: CorrelationGroupId,
    operation_id: CorrelationOperationId,
}

impl ExecutionCorrelation {
    pub fn new(
        namespace: CallerNamespace,
        group_id: CorrelationGroupId,
        operation_id: CorrelationOperationId,
    ) -> Self {
        Self {
            namespace,
            group_id,
            operation_id,
        }
    }

    pub fn namespace(&self) -> &CallerNamespace {
        &self.namespace
    }

    pub fn group_id(&self) -> &CorrelationGroupId {
        &self.group_id
    }

    pub fn operation_id(&self) -> &CorrelationOperationId {
        &self.operation_id
    }
}

impl CallerOwner {
    pub fn new(
        namespace: CallerNamespace,
        owner_id: CallerOwnerId,
        owner_kind: CallerOwnerKind,
        role: CallerRole,
    ) -> Self {
        Self {
            namespace,
            owner_id,
            owner_kind,
            role,
        }
    }

    pub fn namespace(&self) -> &CallerNamespace {
        &self.namespace
    }

    pub fn owner_id(&self) -> &CallerOwnerId {
        &self.owner_id
    }

    pub fn owner_kind(&self) -> CallerOwnerKind {
        self.owner_kind
    }

    pub fn role(&self) -> CallerRole {
        self.role
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AuthorityFingerprint(String);

impl AuthorityFingerprint {
    pub fn new(value: impl Into<String>) -> Result<Self, CallerIdentityError> {
        let value = value.into();
        let digest = value
            .strip_prefix("sha256:")
            .ok_or(CallerIdentityError::InvalidAuthorityFingerprint)?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(CallerIdentityError::InvalidAuthorityFingerprint);
        }
        Ok(Self(value))
    }

    pub fn parse(value: &str) -> Result<Self, CallerIdentityError> {
        Self::new(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for AuthorityFingerprint {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for AuthorityFingerprint {
    type Err = CallerIdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for AuthorityFingerprint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AuthorityFingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint() -> AuthorityFingerprint {
        AuthorityFingerprint::new(format!("sha256:{}", "a".repeat(64))).unwrap()
    }

    #[test]
    fn namespace_enforces_byte_bounds_version_and_control_safety() {
        assert!(CallerNamespace::new("n".repeat(64), 1).is_ok());
        assert!(CallerNamespace::new("é".repeat(32), 1).is_ok());
        assert!(matches!(
            CallerNamespace::new("n".repeat(65), 1),
            Err(CallerIdentityError::TooLong { .. })
        ));
        assert!(matches!(
            CallerNamespace::new("é".repeat(33), 1),
            Err(CallerIdentityError::TooLong { .. })
        ));
        assert!(matches!(
            CallerNamespace::new("agent\nlibre", 1),
            Err(CallerIdentityError::ControlCharacter { .. })
        ));
        assert_eq!(
            CallerNamespace::new("agentlibre", 0),
            Err(CallerIdentityError::ZeroNamespaceVersion)
        );
    }

    #[test]
    fn caller_text_ids_are_bounded_without_parsing_caller_values() {
        let owner = CallerOwnerId::new("run_01890f17-4a00-7000-8000-000000000001").unwrap();
        assert_eq!(owner.as_str(), "run_01890f17-4a00-7000-8000-000000000001");
        assert!(matches!(
            LifecycleScopeId::new("x".repeat(257)),
            Err(CallerIdentityError::TooLong { .. })
        ));
        assert!(matches!(
            CorrelationOperationId::new("owner\0id"),
            Err(CallerIdentityError::ControlCharacter { .. })
        ));
    }

    #[test]
    fn caller_owner_round_trips_and_denies_unknown_fields() {
        let owner = CallerOwner::new(
            CallerNamespace::new("agentlibre", 1).unwrap(),
            CallerOwnerId::new("opaque-owner").unwrap(),
            CallerOwnerKind::Ephemeral,
            CallerRole::Agent,
        );
        let encoded = serde_json::to_value(&owner).unwrap();
        assert_eq!(
            serde_json::from_value::<CallerOwner>(encoded.clone()).unwrap(),
            owner
        );

        let mut unknown = encoded.as_object().unwrap().clone();
        unknown.insert("authority".into(), serde_json::json!(true));
        assert!(serde_json::from_value::<CallerOwner>(unknown.into()).is_err());

        assert!(
            serde_json::from_value::<CallerNamespace>(
                serde_json::json!({"name": "agentlibre", "version": 0})
            )
            .is_err()
        );
    }

    #[test]
    fn execution_owner_and_correlation_are_opaque_exact_and_strict() {
        let namespace = CallerNamespace::new("agentlibre", 1).unwrap();
        let caller = CallerOwner::new(
            namespace.clone(),
            CallerOwnerId::new("opaque-run").unwrap(),
            CallerOwnerKind::Ephemeral,
            CallerRole::Agent,
        );
        let owner = ExecutionOwner::new(
            caller.clone(),
            LifecycleScopeId::new("opaque-lifecycle-scope").unwrap(),
        );
        let peer = ExecutionOwner::new(
            caller,
            LifecycleScopeId::new("another-lifecycle-scope").unwrap(),
        );
        assert!(owner.may_access(&peer));
        assert_ne!(owner.lifecycle_scope_id(), peer.lifecycle_scope_id());
        let encoded_owner = serde_json::to_value(&owner).unwrap();
        assert_eq!(
            encoded_owner["lifecycle_scope_id"],
            "opaque-lifecycle-scope"
        );
        assert_eq!(
            serde_json::from_value::<ExecutionOwner>(encoded_owner.clone()).unwrap(),
            owner
        );
        let mut obsolete_owner = encoded_owner.as_object().unwrap().clone();
        let lifecycle_scope = obsolete_owner.remove("lifecycle_scope_id").unwrap();
        obsolete_owner.insert("authority_scope".to_owned(), lifecycle_scope);
        assert!(serde_json::from_value::<ExecutionOwner>(obsolete_owner.into()).is_err());

        let correlation = ExecutionCorrelation::new(
            namespace,
            CorrelationGroupId::new("opaque-group").unwrap(),
            CorrelationOperationId::new("opaque-operation").unwrap(),
        );
        let encoded = serde_json::to_value(&correlation).unwrap();
        assert_eq!(
            serde_json::from_value::<ExecutionCorrelation>(encoded.clone()).unwrap(),
            correlation
        );
        let mut unknown = encoded.as_object().unwrap().clone();
        unknown.insert("run_id".into(), serde_json::json!("run_opaque"));
        assert!(serde_json::from_value::<ExecutionCorrelation>(unknown.into()).is_err());
    }

    #[test]
    fn authority_fingerprint_is_canonical_and_round_trips() {
        let fingerprint = fingerprint();
        assert_eq!(
            serde_json::from_str::<AuthorityFingerprint>(
                &serde_json::to_string(&fingerprint).unwrap()
            )
            .unwrap(),
            fingerprint
        );
        for invalid in [
            format!("sha256:{}", "A".repeat(64)),
            format!("sha256:{}", "a".repeat(63)),
            format!("sha512:{}", "a".repeat(64)),
            format!("sha256:{}", "g".repeat(64)),
        ] {
            assert_eq!(
                AuthorityFingerprint::new(invalid),
                Err(CallerIdentityError::InvalidAuthorityFingerprint)
            );
        }
    }
}
