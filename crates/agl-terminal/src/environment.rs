use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Debug, Formatter};
use std::path::{Path, PathBuf};

pub use agl_pty::PrivateEnvironmentValue as TerminalSecretValue;
pub use agl_pty::PrivateLaunchEnvironment as PrivateTerminalEnvironment;
pub use agl_pty::{
    MAX_PRIVATE_ENVIRONMENT_BYTES as MAX_TERMINAL_ENVIRONMENT_BYTES,
    MAX_PRIVATE_ENVIRONMENT_ENTRIES as MAX_TERMINAL_ENVIRONMENT_ENTRIES,
    MAX_PRIVATE_ENVIRONMENT_NAME_BYTES as MAX_TERMINAL_ENVIRONMENT_NAME_BYTES,
    MAX_PRIVATE_ENVIRONMENT_VALUE_BYTES as MAX_TERMINAL_ENVIRONMENT_VALUE_BYTES,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use agl_exec::{EnvironmentOverride, ProcessError, ProcessErrorCode, Result};

const RESERVED_PREFIXES: &[&str] = &["AGL_SHELL_INTEGRATION_", "AGL_TERMINAL_"];
const RESERVED_NAMES: &[&str] = &["BASH_ENV", "ENV", "HISTFILE", "PROMPT_COMMAND", "ZDOTDIR"];

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TerminalEnvironmentDigest(String);

impl TerminalEnvironmentDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Debug for TerminalEnvironmentDigest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("TerminalEnvironmentDigest")
            .field(&self.0)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalSecretReference {
    reference_id: String,
}

impl TerminalSecretReference {
    pub fn new(reference_id: impl Into<String>) -> Result<Self> {
        let reference_id = reference_id.into();
        let reference = Self { reference_id };
        reference.validate()?;
        Ok(reference)
    }

    pub fn as_str(&self) -> &str {
        &self.reference_id
    }

    fn validate(&self) -> Result<()> {
        if self.reference_id.trim().is_empty()
            || self.reference_id.len() > MAX_TERMINAL_ENVIRONMENT_VALUE_BYTES
            || self.reference_id.contains(['\0', '\n', '\r'])
        {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "terminal secret reference must be nonempty, bounded, and single-line",
            ));
        }
        Ok(())
    }
}

impl Debug for TerminalSecretReference {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("TerminalSecretReference(<opaque>)")
    }
}

pub trait TerminalSecretResolver: Send + Sync {
    fn resolve(&self, reference: &TerminalSecretReference) -> Result<TerminalSecretValue>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RejectTerminalSecrets;

impl TerminalSecretResolver for RejectTerminalSecrets {
    fn resolve(&self, _reference: &TerminalSecretReference) -> Result<TerminalSecretValue> {
        Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "terminal secret reference has no admitted private resolver",
        ))
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum TerminalEnvironmentValue {
    Plain(String),
    Secret(TerminalSecretReference),
}

impl Debug for TerminalEnvironmentValue {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain(_) => formatter.write_str("Plain(<private>)"),
            Self::Secret(_) => formatter.write_str("Secret(<opaque-reference>)"),
        }
    }
}

/// Creation-time terminal environment input. Values are intentionally omitted
/// from `Debug`; callers must not put this object in presentation or safe-event
/// payloads.
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalEnvironmentRequest {
    pub admitted_base: BTreeMap<String, String>,
    pub selected_parent: BTreeMap<String, String>,
    pub agl_env: BTreeMap<String, TerminalEnvironmentValue>,
    pub admitted_path_roots: Vec<PathBuf>,
}

impl Debug for TerminalEnvironmentRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TerminalEnvironmentRequest")
            .field("admitted_base_names", &self.admitted_base.keys())
            .field("selected_parent_names", &self.selected_parent.keys())
            .field("agl_env_names", &self.agl_env.keys())
            .field("admitted_path_roots", &self.admitted_path_roots)
            .finish()
    }
}

pub struct ResolvedTerminalEnvironment {
    public_values: BTreeMap<String, String>,
    private_values: PrivateTerminalEnvironment,
    names: BTreeSet<String>,
    digest: TerminalEnvironmentDigest,
    secret_names: BTreeSet<String>,
    total_bytes: usize,
}

impl ResolvedTerminalEnvironment {
    pub fn digest(&self) -> &TerminalEnvironmentDigest {
        &self.digest
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }

    pub fn secret_names(&self) -> impl Iterator<Item = &str> {
        self.secret_names.iter().map(String::as_str)
    }

    pub fn has_secrets(&self) -> bool {
        !self.secret_names.is_empty()
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn into_launch_parts(self) -> (EnvironmentOverride, PrivateTerminalEnvironment) {
        (
            EnvironmentOverride {
                values: self.public_values,
            },
            self.private_values,
        )
    }
}

impl Debug for ResolvedTerminalEnvironment {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedTerminalEnvironment")
            .field("names", &self.names)
            .field("secret_names", &self.secret_names)
            .field("digest", &self.digest)
            .field("total_bytes", &self.total_bytes)
            .finish()
    }
}

pub struct TerminalEnvironmentAdmission {
    values: BTreeMap<String, AdmittedValue>,
    admitted_roots: Vec<PathBuf>,
    digest: TerminalEnvironmentDigest,
}

impl Debug for TerminalEnvironmentAdmission {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TerminalEnvironmentAdmission")
            .field("names", &self.values.keys())
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

impl TerminalEnvironmentAdmission {
    pub fn digest(&self) -> &TerminalEnvironmentDigest {
        &self.digest
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }

    pub fn resolve(
        self,
        secrets: &dyn TerminalSecretResolver,
    ) -> Result<ResolvedTerminalEnvironment> {
        let Self {
            values,
            admitted_roots,
            digest,
        } = self;
        let names = values.keys().cloned().collect::<BTreeSet<_>>();
        let mut secret_names = BTreeSet::new();
        let mut public_values = BTreeMap::new();
        let mut private_values = BTreeMap::new();
        let mut total_bytes = 0usize;
        for (name, admitted) in values {
            let value_bytes = match admitted.value {
                AdmittedEnvironmentValue::Plain(value) => {
                    let value_bytes = value.len();
                    public_values.insert(name.clone(), value);
                    value_bytes
                }
                AdmittedEnvironmentValue::Secret(reference) => {
                    let mut value = secrets.resolve(&reference).map_err(|error| {
                        ProcessError::new(
                            error.code(),
                            "terminal secret reference was rejected by the admitted private resolver",
                        )
                    })?;
                    if is_path_environment_name(&name) {
                        let canonical =
                            canonicalize_path_list(&name, value.expose(), &admitted_roots)?;
                        value = TerminalSecretValue::new(canonical)?;
                    }
                    let value_bytes = value.expose().len();
                    secret_names.insert(name.clone());
                    private_values.insert(name.clone(), value);
                    value_bytes
                }
            };
            total_bytes = total_bytes
                .checked_add(name.len())
                .and_then(|total| total.checked_add(value_bytes))
                .and_then(|total| total.checked_add(2))
                .ok_or_else(environment_too_large)?;
            if total_bytes > MAX_TERMINAL_ENVIRONMENT_BYTES {
                return Err(environment_too_large());
            }
        }
        Ok(ResolvedTerminalEnvironment {
            public_values,
            private_values: PrivateTerminalEnvironment::new(private_values)?,
            names,
            digest,
            secret_names,
            total_bytes,
        })
    }
}

impl TerminalEnvironmentRequest {
    pub fn resolve(
        &self,
        secrets: &dyn TerminalSecretResolver,
    ) -> Result<ResolvedTerminalEnvironment> {
        self.admit()?.resolve(secrets)
    }

    pub fn admit(&self) -> Result<TerminalEnvironmentAdmission> {
        if self
            .admitted_base
            .len()
            .saturating_add(self.selected_parent.len())
            .saturating_add(self.agl_env.len())
            > MAX_TERMINAL_ENVIRONMENT_ENTRIES.saturating_mul(3)
        {
            return Err(environment_too_large());
        }

        let admitted_roots = canonical_admitted_roots(&self.admitted_path_roots)?;
        let mut values = BTreeMap::<String, AdmittedValue>::new();
        for (name, value) in &self.admitted_base {
            insert_admitted_plain(&mut values, name, value, ValueSource::AdmittedBase)?;
        }
        for (name, value) in &self.selected_parent {
            insert_admitted_plain(&mut values, name, value, ValueSource::SelectedParent)?;
        }
        for (name, value) in &self.agl_env {
            validate_user_name(name)?;
            match value {
                TerminalEnvironmentValue::Plain(value) => {
                    validate_value(value)?;
                    values.insert(
                        name.clone(),
                        AdmittedValue {
                            value: AdmittedEnvironmentValue::Plain(value.clone()),
                            source: ValueSource::AglEnv,
                        },
                    );
                }
                TerminalEnvironmentValue::Secret(reference) => {
                    reference.validate()?;
                    values.insert(
                        name.clone(),
                        AdmittedValue {
                            value: AdmittedEnvironmentValue::Secret(reference.clone()),
                            source: ValueSource::SecretReference,
                        },
                    );
                }
            }
        }
        if values.len() > MAX_TERMINAL_ENVIRONMENT_ENTRIES {
            return Err(environment_too_large());
        }

        for name in ["PATH", "MANPATH", "LD_LIBRARY_PATH"] {
            if let Some(AdmittedValue {
                value: AdmittedEnvironmentValue::Plain(value),
                ..
            }) = values.get_mut(name)
            {
                *value = canonicalize_path_list(name, value, &admitted_roots)?;
            }
        }

        let admission_bytes = values.iter().try_fold(0usize, |total, (name, value)| {
            total
                .checked_add(name.len())
                .and_then(|total| total.checked_add(value.fingerprint_value().len()))
                .and_then(|total| total.checked_add(2))
                .ok_or_else(environment_too_large)
        })?;
        if admission_bytes > MAX_TERMINAL_ENVIRONMENT_BYTES {
            return Err(environment_too_large());
        }

        let digest = digest_environment(&values);
        Ok(TerminalEnvironmentAdmission {
            values,
            admitted_roots,
            digest,
        })
    }
}

#[derive(Clone, Copy)]
enum ValueSource {
    AdmittedBase,
    SelectedParent,
    AglEnv,
    SecretReference,
}

impl ValueSource {
    fn marker(self) -> u8 {
        match self {
            Self::AdmittedBase => 1,
            Self::SelectedParent => 2,
            Self::AglEnv => 3,
            Self::SecretReference => 4,
        }
    }
}

struct AdmittedValue {
    value: AdmittedEnvironmentValue,
    source: ValueSource,
}

enum AdmittedEnvironmentValue {
    Plain(String),
    Secret(TerminalSecretReference),
}

impl AdmittedValue {
    fn fingerprint_value(&self) -> &str {
        match &self.value {
            AdmittedEnvironmentValue::Plain(value) => value,
            AdmittedEnvironmentValue::Secret(reference) => reference.as_str(),
        }
    }

    fn is_secret(&self) -> bool {
        matches!(&self.value, AdmittedEnvironmentValue::Secret(_))
    }
}

fn insert_admitted_plain(
    target: &mut BTreeMap<String, AdmittedValue>,
    name: &str,
    value: &str,
    source: ValueSource,
) -> Result<()> {
    validate_user_name(name)?;
    validate_value(value)?;
    target.insert(
        name.to_owned(),
        AdmittedValue {
            value: AdmittedEnvironmentValue::Plain(value.to_owned()),
            source,
        },
    );
    Ok(())
}

fn validate_user_name(name: &str) -> Result<()> {
    if name.len() > MAX_TERMINAL_ENVIRONMENT_NAME_BYTES || !is_posix_name(name) {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "terminal environment names must be bounded POSIX identifiers",
        ));
    }
    if RESERVED_NAMES.contains(&name)
        || RESERVED_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
    {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "terminal environment cannot override AGL-owned integration names",
        ));
    }
    Ok(())
}

fn is_posix_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn validate_value(value: &str) -> Result<()> {
    if value.len() > MAX_TERMINAL_ENVIRONMENT_VALUE_BYTES || value.contains('\0') {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "terminal environment values must be bounded and contain no NUL",
        ));
    }
    Ok(())
}

fn canonical_admitted_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    roots
        .iter()
        .map(|root| {
            let canonical = root.canonicalize().map_err(|error| {
                ProcessError::new(
                    ProcessErrorCode::InvalidRequest,
                    format!("admitted terminal PATH root cannot be canonicalized: {error}"),
                )
            })?;
            if &canonical != root || !canonical.is_dir() {
                return Err(ProcessError::new(
                    ProcessErrorCode::InvalidRequest,
                    "admitted terminal PATH roots must be existing canonical directories",
                ));
            }
            Ok(canonical)
        })
        .collect()
}

fn canonicalize_path_list(name: &str, value: &str, roots: &[PathBuf]) -> Result<String> {
    if value.is_empty() {
        return Ok(String::new());
    }
    if roots.is_empty() {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            format!("{name} requires at least one admitted canonical runtime root"),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut canonical = Vec::new();
    for entry in value.split(':') {
        if entry.is_empty() {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                format!("{name} must not contain implicit current-directory entries"),
            ));
        }
        let path = Path::new(entry).canonicalize().map_err(|error| {
            ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                format!("{name} entry cannot be canonicalized: {error}"),
            )
        })?;
        if !path.is_dir() || !roots.iter().any(|root| path.starts_with(root)) {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                format!("{name} entry is outside admitted runtime roots"),
            ));
        }
        if seen.insert(path.clone()) {
            canonical.push(path);
        }
    }
    Ok(canonical
        .iter()
        .map(|path| path.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join(":"))
}

fn is_path_environment_name(name: &str) -> bool {
    matches!(name, "PATH" | "MANPATH" | "LD_LIBRARY_PATH")
}

fn digest_environment(values: &BTreeMap<String, AdmittedValue>) -> TerminalEnvironmentDigest {
    let mut digest = Sha256::new();
    digest.update(b"agentlibre.terminal-environment.v1\0");
    for (name, value) in values {
        digest.update(name.as_bytes());
        digest.update([0, value.source.marker(), u8::from(value.is_secret()), 0]);
        digest.update(value.fingerprint_value().as_bytes());
        digest.update([0]);
    }
    TerminalEnvironmentDigest(format_digest(digest.finalize()))
}

fn format_digest(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut rendered = String::with_capacity(7 + bytes.len() * 2);
    rendered.push_str("sha256:");
    use std::fmt::Write as _;
    for byte in bytes {
        write!(&mut rendered, "{byte:02x}").expect("writing to a String cannot fail");
    }
    rendered
}

fn environment_too_large() -> ProcessError {
    ProcessError::new(
        ProcessErrorCode::InvalidRequest,
        "terminal environment exceeds its admitted bounds",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Secrets;

    impl TerminalSecretResolver for Secrets {
        fn resolve(&self, reference: &TerminalSecretReference) -> Result<TerminalSecretValue> {
            assert_eq!(reference.as_str(), "vault:item");
            TerminalSecretValue::new("do-not-log-me")
        }
    }

    #[test]
    fn layering_is_deterministic_and_secret_values_are_redacted() {
        let mut request = TerminalEnvironmentRequest::default();
        request
            .admitted_base
            .insert("LANG".to_owned(), "C".to_owned());
        request
            .selected_parent
            .insert("LANG".to_owned(), "C.UTF-8".to_owned());
        request.agl_env.insert(
            "TOKEN".to_owned(),
            TerminalEnvironmentValue::Secret(TerminalSecretReference::new("vault:item").unwrap()),
        );

        let resolved = request.resolve(&Secrets).unwrap();
        let rendered = format!("{request:?} {resolved:?}");
        assert!(!rendered.contains("do-not-log-me"));
        assert!(!rendered.contains("C.UTF-8"));
        assert_eq!(resolved.secret_names().collect::<Vec<_>>(), vec!["TOKEN"]);
        assert_eq!(resolved.total_bytes(), 33);
        assert!(resolved.digest().as_str().starts_with("sha256:"));
        let digest = resolved.digest().clone();

        let (values, private) = resolved.into_launch_parts();
        assert_eq!(values.values["LANG"], "C.UTF-8");
        assert!(!values.values.contains_key("TOKEN"));
        assert_eq!(
            private
                .exposed_values()
                .find(|(name, _)| *name == "TOKEN")
                .map(|(_, value)| value),
            Some("do-not-log-me")
        );

        let mut encoded = Vec::new();
        private.write_launch_transport(&mut encoded).unwrap();
        let decoded =
            PrivateTerminalEnvironment::read_launch_transport(&mut encoded.as_slice()).unwrap();
        assert_eq!(
            decoded
                .exposed_values()
                .find(|(name, _)| *name == "TOKEN")
                .map(|(_, value)| value),
            Some("do-not-log-me")
        );

        struct RotatedSecrets;
        impl TerminalSecretResolver for RotatedSecrets {
            fn resolve(&self, _reference: &TerminalSecretReference) -> Result<TerminalSecretValue> {
                TerminalSecretValue::new("rotated-private-value")
            }
        }
        assert_eq!(request.resolve(&RotatedSecrets).unwrap().digest(), &digest);
    }

    #[test]
    fn reserved_and_non_posix_names_fail_closed() {
        for name in ["1BAD", "HAS-DASH", "HISTFILE", "AGL_SHELL_INTEGRATION_FD"] {
            let mut request = TerminalEnvironmentRequest::default();
            request.agl_env.insert(
                name.to_owned(),
                TerminalEnvironmentValue::Plain("value".to_owned()),
            );
            assert_eq!(
                request.resolve(&Secrets).unwrap_err().code(),
                ProcessErrorCode::InvalidRequest
            );
        }
    }

    #[test]
    fn path_entries_are_canonical_deduplicated_and_bounded_to_admitted_roots() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("agl-terminal-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let root = root.canonicalize().unwrap();
        let bin = root.join("bin");
        let mut request = TerminalEnvironmentRequest {
            admitted_path_roots: vec![root.clone()],
            ..TerminalEnvironmentRequest::default()
        };
        request.admitted_base.insert(
            "PATH".to_owned(),
            format!("{}:{}", bin.display(), bin.display()),
        );

        let values = request
            .resolve(&Secrets)
            .unwrap()
            .into_launch_parts()
            .0
            .values;
        assert_eq!(values["PATH"], bin.to_string_lossy());

        request
            .admitted_base
            .insert("PATH".to_owned(), "/".to_owned());
        assert_eq!(
            request.resolve(&Secrets).unwrap_err().code(),
            ProcessErrorCode::InvalidRequest
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolver_failures_are_sanitized_and_omitted_refs_create_no_private_overlay() {
        const SENTINEL: &str = "secret-value-must-not-escape-resolution";

        struct LeakyReject;

        impl TerminalSecretResolver for LeakyReject {
            fn resolve(&self, _reference: &TerminalSecretReference) -> Result<TerminalSecretValue> {
                Err(ProcessError::new(
                    ProcessErrorCode::InvalidRequest,
                    SENTINEL,
                ))
            }
        }

        let mut unresolved = TerminalEnvironmentRequest::default();
        unresolved.agl_env.insert(
            "TOKEN".to_owned(),
            TerminalEnvironmentValue::Secret(
                TerminalSecretReference::new("sibling:private-item").unwrap(),
            ),
        );
        let error = unresolved.resolve(&LeakyReject).unwrap_err();
        assert_eq!(error.code(), ProcessErrorCode::InvalidRequest);
        assert!(!error.message().contains(SENTINEL));
        assert!(!format!("{error:?}").contains(SENTINEL));

        let omitted = TerminalEnvironmentRequest::default()
            .resolve(&RejectTerminalSecrets)
            .unwrap();
        let (_, private) = omitted.into_launch_parts();
        assert!(private.is_empty());
    }
}
