use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs::OpenOptions, io::Read as _};

use agl_core::agent::{
    EffectReceipt, ToolDefinition, ToolDefinitionDigest, ToolFailure, ToolFailureKind, ToolResult,
};
use agl_runtime::extension::{
    ExtensionBindings, ToolBinding, ToolContext, ToolFuture, ToolHandler, parse_package_view,
};
use agl_runtime::package::{InMemoryPackageView, PackageRelativePath, compute_package_digest};
use anyhow::{Context as _, Result, ensure};
use chrono::DateTime;
use futures_util::StreamExt as _;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use url::{Host, Url};

const SEARCH_EFFECT: &str = "agentlibre.searxng:query";
const SEARCH_TOOL: &str = "agentlibre.searxng:search";
const SEARCH_SERVICE: &str = "ayeque-search";
const SEARCH_ENDPOINT: &str = "https://agent-search.ayeque.art/v1/search";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REQUEST_BYTES: usize = 4 * 1024;
const MAX_RESPONSE_BYTES: usize = 48 * 1024;
const MAX_CREDENTIAL_BYTES: u64 = 1024 * 1024;
const RESULT_SCHEMA: &str = "ayeque.agent-search-result/v1";
const ERROR_SCHEMA: &str = "ayeque.agent-search-error/v1";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearxngConfig {
    pub client_certificate: PathBuf,
    pub client_private_key: PathBuf,
    pub private_ca: PathBuf,
}

pub(crate) fn bindings(config: SearxngConfig) -> Result<ExtensionBindings> {
    let client = build_client(&config)?;
    Ok(bindings_with_client(client, SEARCH_ENDPOINT.to_owned()))
}

fn bindings_with_client(client: reqwest::Client, endpoint: String) -> ExtensionBindings {
    let declaration = declaration_view();
    let package = parse_package_view(&declaration).expect("embedded SearXNG Extension");
    let definition = package
        .definition
        .tools
        .iter()
        .find(|definition| definition.id.as_str() == SEARCH_TOOL)
        .expect("embedded search Tool")
        .clone();
    let binding = ToolBinding {
        tool_id: definition.id.clone(),
        definition_digest: definition_digest(&definition),
        handler: Arc::new(SearchTool { client, endpoint }),
    };
    ExtensionBindings {
        version: package.manifest.version,
        content_digest: compute_package_digest(&declaration)
            .expect("embedded SearXNG Extension digest"),
        definition: package.definition,
        tools: vec![binding],
        allows_authority: Arc::new(valid_search_scope),
    }
}

fn build_client(config: &SearxngConfig) -> Result<reqwest::Client> {
    let certificate = read_credential(&config.client_certificate, CredentialKind::Certificate)?;
    let key = read_credential(&config.client_private_key, CredentialKind::Pkcs8Key)?;
    let ca = read_credential(&config.private_ca, CredentialKind::Certificate)?;
    let mut identity_pem = Vec::with_capacity(certificate.len() + key.len() + 1);
    identity_pem.extend_from_slice(&certificate);
    identity_pem.push(b'\n');
    identity_pem.extend_from_slice(&key);
    let identity =
        reqwest::Identity::from_pem(&identity_pem).context("invalid SearXNG client identity")?;
    let roots =
        reqwest::Certificate::from_pem_bundle(&ca).context("invalid SearXNG private CA bundle")?;
    ensure!(!roots.is_empty(), "empty SearXNG private CA bundle");
    reqwest::Client::builder()
        .identity(identity)
        .tls_certs_only(roots)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .context("failed to build SearXNG client")
}

#[derive(Clone, Copy)]
enum CredentialKind {
    Certificate,
    Pkcs8Key,
}

fn read_credential(path: &Path, kind: CredentialKind) -> Result<Vec<u8>> {
    ensure!(
        path.is_absolute(),
        "SearXNG credential path must be absolute"
    );
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .context("failed to open a SearXNG credential")?;
    let metadata = file
        .metadata()
        .context("failed to inspect a SearXNG credential")?;
    // SAFETY: geteuid has no preconditions and does not mutate process state.
    let expected_uid = unsafe { libc::geteuid() };
    ensure!(
        metadata.is_file()
            && metadata.uid() == expected_uid
            && metadata.nlink() == 1
            && metadata.mode() & 0o077 == 0
            && metadata.len() > 0
            && metadata.len() <= MAX_CREDENTIAL_BYTES,
        "SearXNG credential file violates the private-file policy"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_CREDENTIAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("failed to read a SearXNG credential")?;
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_CREDENTIAL_BYTES,
        "SearXNG credential file violates the private-file policy"
    );
    let valid_marker = match kind {
        CredentialKind::Certificate => contains(&bytes, b"-----BEGIN CERTIFICATE-----"),
        CredentialKind::Pkcs8Key => {
            contains(&bytes, b"-----BEGIN PRIVATE KEY-----")
                && !contains(&bytes, b"ENCRYPTED PRIVATE KEY")
        }
    };
    ensure!(valid_marker, "SearXNG credential has the wrong PEM type");
    Ok(bytes)
}

fn contains(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|window| window == needle)
}

fn valid_search_scope(grant: &agl_core::AuthorityGrant) -> bool {
    if grant.effect.as_str() != SEARCH_EFFECT {
        return false;
    }
    let scope = grant.scope.as_value();
    if scope.get("service").and_then(Value::as_str) != Some(SEARCH_SERVICE) {
        return false;
    }
    let Some(sources) = scope.get("sources").and_then(Value::as_array) else {
        return false;
    };
    !sources.is_empty()
        && sources.len() <= 2
        && sources
            .windows(2)
            .all(|pair| pair[0].as_str() < pair[1].as_str())
        && sources
            .iter()
            .all(|source| matches!(source.as_str(), Some("web" | "wikipedia")))
}

fn declaration_view() -> InMemoryPackageView {
    InMemoryPackageView::new([
        embedded(
            "EXTENSION.toml",
            include_bytes!("../../../../../extensions/agentlibre-searxng/EXTENSION.toml"),
        ),
        embedded(
            "schemas/query-scope.json",
            include_bytes!("../../../../../extensions/agentlibre-searxng/schemas/query-scope.json"),
        ),
        embedded(
            "schemas/search.json",
            include_bytes!("../../../../../extensions/agentlibre-searxng/schemas/search.json"),
        ),
        embedded(
            "fixtures/v1.json",
            include_bytes!("../../../../../extensions/agentlibre-searxng/fixtures/v1.json"),
        ),
    ])
    .expect("embedded SearXNG package")
}

fn embedded(path: &str, bytes: &[u8]) -> (PackageRelativePath, Vec<u8>) {
    (
        PackageRelativePath::new(path).expect("static package path"),
        bytes.to_vec(),
    )
}

fn definition_digest(definition: &ToolDefinition) -> ToolDefinitionDigest {
    ToolDefinitionDigest::from_bytes(
        Sha256::digest(serde_json::to_vec(definition).expect("ToolDefinition serializes")).into(),
    )
}

struct SearchTool {
    client: reqwest::Client,
    endpoint: String,
}

impl ToolHandler for SearchTool {
    fn call(&self, context: ToolContext, input: Value) -> ToolFuture {
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        Box::pin(async move { execute(&client, &endpoint, context, input).await })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum SearchSource {
    Web,
    Wikipedia,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum SearchLanguage {
    #[default]
    En,
    Fr,
    Ru,
    Uk,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    query: String,
    source: SearchSource,
    #[serde(default)]
    language: SearchLanguage,
    #[serde(default = "default_limit")]
    limit: u8,
}

fn default_limit() -> u8 {
    8
}

#[derive(Serialize)]
struct SearchRequest<'a> {
    query: &'a str,
    source: SearchSource,
    language: SearchLanguage,
    limit: u8,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SearchResponse {
    schema: String,
    source: SearchSource,
    language: SearchLanguage,
    results: Vec<SearchResult>,
    suggestions: Vec<String>,
    truncated: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
    engine: String,
    published_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchError {
    schema: String,
    code: SearchErrorCode,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SearchErrorCode {
    InvalidRequest,
    AuthenticationFailed,
    RateLimited,
    UpstreamUnavailable,
    Timeout,
    InvalidUpstreamResponse,
}

async fn execute(
    client: &reqwest::Client,
    endpoint: &str,
    context: ToolContext,
    input: Value,
) -> Result<ToolResult, ToolFailure> {
    let input: SearchInput =
        serde_json::from_value(input).map_err(|_| failure(ToolFailureKind::InvalidInput))?;
    let query = input.query.trim();
    if query.is_empty()
        || query.chars().count() > 512
        || query.len() > 2048
        || !(1..=10).contains(&input.limit)
        || has_control(query)
    {
        return Err(failure(ToolFailureKind::InvalidInput));
    }
    let receipt = admitted_receipt(&context, input.source)?;
    let request = SearchRequest {
        query,
        source: input.source,
        language: input.language,
        limit: input.limit,
    };
    let encoded =
        serde_json::to_vec(&request).map_err(|_| failure(ToolFailureKind::InvalidInput))?;
    if encoded.len() > MAX_REQUEST_BYTES {
        return Err(failure(ToolFailureKind::InvalidInput));
    }
    let remaining = remaining_duration(context.deadline_at_ms)?;
    let response = send_cancellable(
        client
            .post(endpoint)
            .header(CONTENT_TYPE, "application/json")
            .timeout(remaining.min(REQUEST_TIMEOUT))
            .body(encoded),
        &context,
        remaining.min(REQUEST_TIMEOUT),
    )
    .await?;
    if response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some("application/json")
    {
        return Err(failure(ToolFailureKind::InvalidResult));
    }
    let status = response.status();
    let bytes = read_bounded(response, &context).await?;
    if !status.is_success() {
        return Err(map_error_response(status.as_u16(), &bytes));
    }
    let result: SearchResponse =
        serde_json::from_slice(&bytes).map_err(|_| failure(ToolFailureKind::InvalidResult))?;
    validate_response(&result, &request)?;
    let text =
        serde_json::to_string(&result).map_err(|_| failure(ToolFailureKind::InvalidResult))?;
    super::committed_result(text, receipt, context.result_bytes)
}

async fn send_cancellable(
    request: reqwest::RequestBuilder,
    context: &ToolContext,
    timeout: Duration,
) -> Result<reqwest::Response, ToolFailure> {
    let send = request.send();
    tokio::pin!(send);
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if context.is_cancelled() {
            return Err(failure(ToolFailureKind::Cancelled));
        }
        tokio::select! {
            response = &mut send => {
                return response.map_err(|error| {
                    if error.is_timeout() {
                        failure(ToolFailureKind::Deadline)
                    } else if is_tls_failure(&error) {
                        failure(ToolFailureKind::Unauthorized)
                    } else {
                        failure(ToolFailureKind::Unavailable)
                    }
                });
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Err(failure(ToolFailureKind::Deadline));
            }
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
        }
    }
}

async fn read_bounded(
    response: reqwest::Response,
    context: &ToolContext,
) -> Result<Vec<u8>, ToolFailure> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(failure(ToolFailureKind::InvalidResult));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    loop {
        if context.is_cancelled() {
            return Err(failure(ToolFailureKind::Cancelled));
        }
        let remaining = remaining_duration(context.deadline_at_ms)?;
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            _ = tokio::time::sleep(remaining) => {
                return Err(failure(ToolFailureKind::Deadline));
            }
            _ = tokio::time::sleep(Duration::from_millis(25)) => {
                continue;
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(|_| failure(ToolFailureKind::Unavailable))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(failure(ToolFailureKind::InvalidResult));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn is_tls_failure(error: &reqwest::Error) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        if current.downcast_ref::<rustls::Error>().is_some() {
            return true;
        }
        if current
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            .is_some_and(|inner| inner.downcast_ref::<rustls::Error>().is_some())
        {
            return true;
        }
        source = current.source();
    }
    false
}

fn validate_response(
    response: &SearchResponse,
    request: &SearchRequest<'_>,
) -> Result<(), ToolFailure> {
    if response.schema != RESULT_SCHEMA
        || response.source != request.source
        || response.language != request.language
        || response.results.len() > usize::from(request.limit)
        || response.results.len() > 10
        || response.suggestions.len() > 8
    {
        return Err(failure(ToolFailureKind::InvalidResult));
    }
    for result in &response.results {
        bounded_text(&result.title, 512, 2048, true)?;
        bounded_text(&result.snippet, 2048, 8192, false)?;
        bounded_text(&result.engine, 64, 256, true)?;
        validate_url(&result.url)?;
        if let Some(timestamp) = &result.published_at
            && (timestamp.len() > 64 || DateTime::parse_from_rfc3339(timestamp).is_err())
        {
            return Err(failure(ToolFailureKind::InvalidResult));
        }
    }
    for suggestion in &response.suggestions {
        bounded_text(suggestion, 256, 1024, true)?;
    }
    Ok(())
}

fn bounded_text(
    value: &str,
    max_scalars: usize,
    max_bytes: usize,
    nonempty: bool,
) -> Result<(), ToolFailure> {
    if (nonempty && value.trim().is_empty())
        || value.chars().count() > max_scalars
        || value.len() > max_bytes
        || has_control(value)
        || value.contains(['<', '>'])
    {
        return Err(failure(ToolFailureKind::InvalidResult));
    }
    Ok(())
}

fn has_control(value: &str) -> bool {
    value.chars().any(char::is_control)
}

fn validate_url(value: &str) -> Result<(), ToolFailure> {
    if value.len() > 4096 || has_control(value) {
        return Err(failure(ToolFailureKind::InvalidResult));
    }
    let url = Url::parse(value).map_err(|_| failure(ToolFailureKind::InvalidResult))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host().is_none_or(internal_host)
    {
        return Err(failure(ToolFailureKind::InvalidResult));
    }
    Ok(())
}

fn internal_host(host: Host<&str>) -> bool {
    match host {
        Host::Ipv4(address) => {
            let octets = address.octets();
            address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_broadcast()
                || address.is_documentation()
                || address.is_unspecified()
                || address.is_multicast()
                || octets[0] == 0
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
                || octets[0] >= 240
        }
        Host::Ipv6(address) => {
            address
                .to_ipv4()
                .is_some_and(|address| internal_host(Host::Ipv4(address)))
                || address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || (address.segments()[0] & 0xfe00) == 0xfc00
                || (address.segments()[0] & 0xffc0) == 0xfe80
        }
        Host::Domain(host) => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            host == "localhost"
                || host.ends_with(".localhost")
                || host.ends_with(".local")
                || !host.contains('.')
        }
    }
}

fn admitted_receipt(
    context: &ToolContext,
    source: SearchSource,
) -> Result<EffectReceipt, ToolFailure> {
    let source = match source {
        SearchSource::Web => "web",
        SearchSource::Wikipedia => "wikipedia",
    };
    context
        .authority
        .0
        .iter()
        .find(|grant| {
            grant.effect.as_str() == SEARCH_EFFECT
                && grant
                    .scope
                    .as_value()
                    .get("service")
                    .and_then(Value::as_str)
                    == Some(SEARCH_SERVICE)
                && grant
                    .scope
                    .as_value()
                    .get("sources")
                    .and_then(Value::as_array)
                    .is_some_and(|sources| {
                        sources.iter().any(|value| value.as_str() == Some(source))
                    })
        })
        .map(|grant| EffectReceipt {
            effect: grant.effect.clone(),
            scope: grant.scope.clone(),
        })
        .ok_or_else(|| failure(ToolFailureKind::Unauthorized))
}

fn map_error_response(status: u16, body: &[u8]) -> ToolFailure {
    let Ok(error) = serde_json::from_slice::<SearchError>(body) else {
        return failure(ToolFailureKind::InvalidResult);
    };
    if error.schema != ERROR_SCHEMA {
        return failure(ToolFailureKind::InvalidResult);
    }
    let kind = match (status, error.code) {
        (400, SearchErrorCode::InvalidRequest) => ToolFailureKind::InvalidResult,
        (401 | 403, SearchErrorCode::AuthenticationFailed) => ToolFailureKind::Unauthorized,
        (429, SearchErrorCode::RateLimited) | (503, SearchErrorCode::UpstreamUnavailable) => {
            ToolFailureKind::Unavailable
        }
        (504, SearchErrorCode::Timeout) => ToolFailureKind::Deadline,
        (502, SearchErrorCode::InvalidUpstreamResponse) => ToolFailureKind::InvalidResult,
        _ => ToolFailureKind::InvalidResult,
    };
    failure(kind)
}

fn remaining_duration(deadline_at_ms: i64) -> Result<Duration, ToolFailure> {
    let remaining = deadline_at_ms.saturating_sub(now_ms());
    u64::try_from(remaining)
        .ok()
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .ok_or_else(|| failure(ToolFailureKind::Deadline))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn failure(kind: ToolFailureKind) -> ToolFailure {
    // Failures after sending the HTTP request do not prove that the network
    // effect was absent. Only local input validation is a no-effect rejection.
    if kind == ToolFailureKind::InvalidInput {
        ToolFailure::no_effect(
            kind,
            Some("input"),
            &["Correct query, source, language or limit according to the admitted schema."],
        )
    } else {
        ToolFailure::unknown(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agl_core::agent::{
        AbsolutePath, AgentOperationKey, AuthorityGrantSet, RelativePath, WorkspaceScope,
    };
    use agl_core::{AgentRunId, AuthorityGrant, CanonicalJson, EffectId};
    use agl_runtime::extension::{ToolCancellation, ToolContext};
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
        KeyUsagePurpose,
    };
    use serde_json::json;
    use std::num::NonZeroU32;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio_rustls::rustls::server::WebPkiClientVerifier;
    use tokio_rustls::rustls::{RootCertStore, ServerConfig};

    fn context(scope: Value) -> ToolContext {
        context_with(scope, now_ms() + 5_000, 65_536, ToolCancellation::new())
    }

    fn context_with(
        scope: Value,
        deadline_at_ms: i64,
        result_bytes: u64,
        cancellation: ToolCancellation,
    ) -> ToolContext {
        ToolContext::new(
            AgentOperationKey {
                run_id: AgentRunId::generate(),
                ordinal: NonZeroU32::new(2).unwrap(),
            },
            None,
            WorkspaceScope {
                root: AbsolutePath::try_from("/tmp".to_owned()).unwrap(),
                working_directory: RelativePath::try_from(".".to_owned()).unwrap(),
            },
            AuthorityGrantSet(vec![AuthorityGrant {
                effect: EffectId::new(SEARCH_EFFECT).unwrap(),
                scope: CanonicalJson::new(scope).unwrap(),
            }]),
            deadline_at_ms,
            result_bytes,
            cancellation,
        )
    }

    fn response() -> SearchResponse {
        SearchResponse {
            schema: RESULT_SCHEMA.to_owned(),
            source: SearchSource::Web,
            language: SearchLanguage::En,
            results: vec![SearchResult {
                title: "Rust cancellation".to_owned(),
                url: "https://example.org/rust".to_owned(),
                snippet: "Bounded cancellation".to_owned(),
                engine: "bing".to_owned(),
                published_at: Some("2026-09-02T00:00:00Z".to_owned()),
            }],
            suggestions: vec![],
            truncated: false,
        }
    }

    async fn synthetic_endpoint(
        response: Vec<u8>,
    ) -> (String, tokio::sync::oneshot::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (send, receive) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 2048];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&buffer[..read]);
                if let Some(header_end) = request.windows(4).position(|value| value == b"\r\n\r\n")
                {
                    let header_end = header_end + 4;
                    let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                    let content_length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse::<usize>()
                        .unwrap();
                    while request.len() < header_end + content_length {
                        let read = stream.read(&mut buffer).await.unwrap();
                        assert_ne!(read, 0);
                        request.extend_from_slice(&buffer[..read]);
                    }
                    let _ = send.send(request[header_end..header_end + content_length].to_vec());
                    break;
                }
            }
            stream.write_all(&response).await.unwrap();
        });
        (format!("http://{address}/v1/search"), receive)
    }

    struct TestIdentity {
        ca_pem: String,
        client_pem: String,
        client_key_pem: String,
        acceptor: TlsAcceptor,
    }

    fn test_identity() -> TestIdentity {
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        let ca_key = KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let ca_pem = ca_cert.pem();
        let ca_der = ca_cert.der().clone();
        let issuer = Issuer::new(ca_params, ca_key);

        let mut server_params = CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
        server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_key = KeyPair::generate().unwrap();
        let server_cert = server_params.signed_by(&server_key, &issuer).unwrap();

        let mut client_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        client_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let client_key = KeyPair::generate().unwrap();
        let client_cert = client_params.signed_by(&client_key, &issuer).unwrap();

        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(ca_der.to_vec())).unwrap();
        let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .unwrap();
        let server = ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![CertificateDer::from(server_cert.der().to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
            )
            .unwrap();
        TestIdentity {
            ca_pem,
            client_pem: client_cert.pem(),
            client_key_pem: client_key.serialize_pem(),
            acceptor: TlsAcceptor::from(Arc::new(server)),
        }
    }

    async fn synthetic_mtls_endpoint(response: Vec<u8>, acceptor: TlsAcceptor) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let Ok(mut stream) = acceptor.accept(stream).await else {
                return;
            };
            let mut request = Vec::new();
            let mut buffer = [0_u8; 2048];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|value| value == b"\r\n\r\n") {
                    break;
                }
            }
            stream.write_all(&response).await.unwrap();
        });
        format!("https://localhost:{}/v1/search", address.port())
    }

    #[test]
    fn embedded_declaration_and_authority_are_exact() {
        let declaration = declaration_view();
        let package = parse_package_view(&declaration).unwrap();
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../extensions/agentlibre-searxng");
        let source = agl_runtime::package::DirectoryPackageView::new(root).unwrap();
        let bindings = bindings_with_client(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            SEARCH_ENDPOINT.to_owned(),
        );
        assert_eq!(package.definition.id.as_str(), "agentlibre.searxng");
        assert_eq!(package.definition.tools.len(), 1);
        assert_eq!(bindings.version, package.manifest.version);
        assert_eq!(bindings.definition, package.definition);
        assert_eq!(
            bindings.content_digest,
            compute_package_digest(&source).unwrap()
        );
        assert_eq!(bindings.tools.len(), 1);
        assert_eq!(
            bindings.tools[0].definition_digest,
            definition_digest(&package.definition.tools[0])
        );
        assert!(valid_search_scope(&AuthorityGrant {
            effect: EffectId::new(SEARCH_EFFECT).unwrap(),
            scope: CanonicalJson::new(json!({
                "service": SEARCH_SERVICE,
                "sources": ["web", "wikipedia"]
            }))
            .unwrap(),
        }));
        assert!(!valid_search_scope(&AuthorityGrant {
            effect: EffectId::new(SEARCH_EFFECT).unwrap(),
            scope: CanonicalJson::new(json!({
                "service": SEARCH_SERVICE,
                "sources": ["wikipedia", "web"]
            }))
            .unwrap(),
        }));
    }

    #[test]
    fn synchronized_fixture_corpus_matches_the_closed_contract() {
        let corpus: Value = serde_json::from_slice(include_bytes!(
            "../../../../../extensions/agentlibre-searxng/fixtures/v1.json"
        ))
        .unwrap();
        assert_eq!(corpus["schema"], "ayeque.agent-search-fixtures/v1");
        assert_eq!(corpus["requests"].as_array().unwrap().len(), 2);
        for fixture in corpus["successes"].as_array().unwrap() {
            let response: SearchResponse = serde_json::from_value(fixture.clone()).unwrap();
            let request = SearchRequest {
                query: "fixture",
                source: response.source,
                language: response.language,
                limit: 10,
            };
            validate_response(&response, &request).unwrap();
        }
        for fixture in corpus["errors"].as_array().unwrap() {
            let status = fixture["status"].as_u64().unwrap() as u16;
            let body = serde_json::to_vec(&fixture["body"]).unwrap();
            assert_eq!(
                format!("{:?}", map_error_response(status, &body).kind).to_ascii_lowercase(),
                fixture["agl_kind"].as_str().unwrap().replace('_', "")
            );
        }
        for fixture in corpus["invalid_successes"].as_array().unwrap() {
            if let Ok(response) = serde_json::from_value::<SearchResponse>(fixture.clone()) {
                let request = SearchRequest {
                    query: "fixture",
                    source: response.source,
                    language: response.language,
                    limit: 10,
                };
                assert_eq!(
                    validate_response(&response, &request).unwrap_err().kind,
                    ToolFailureKind::InvalidResult
                );
            }
        }
    }

    #[test]
    fn result_validation_is_exact_and_rejects_internal_urls() {
        let request = SearchRequest {
            query: "rust",
            source: SearchSource::Web,
            language: SearchLanguage::En,
            limit: 8,
        };
        let mut value = response();
        validate_response(&value, &request).unwrap();
        value.results[0].url = "http://127.0.0.1/private".to_owned();
        assert_eq!(
            validate_response(&value, &request).unwrap_err().kind,
            ToolFailureKind::InvalidResult
        );
        value = response();
        value.results[0].snippet = "<script>bad</script>".to_owned();
        assert_eq!(
            validate_response(&value, &request).unwrap_err().kind,
            ToolFailureKind::InvalidResult
        );
    }

    #[test]
    fn error_envelope_and_status_pairs_are_closed() {
        let error = br#"{"schema":"ayeque.agent-search-error/v1","code":"rate_limited"}"#;
        assert_eq!(
            map_error_response(429, error).kind,
            ToolFailureKind::Unavailable
        );
        assert_eq!(
            map_error_response(503, error).kind,
            ToolFailureKind::InvalidResult
        );
        assert_eq!(
            map_error_response(429, b"{}").kind,
            ToolFailureKind::InvalidResult
        );
    }

    #[test]
    fn source_requires_exact_admitted_scope() {
        let context = context(json!({"service": SEARCH_SERVICE, "sources": ["web"]}));
        assert!(admitted_receipt(&context, SearchSource::Web).is_ok());
        assert_eq!(
            admitted_receipt(&context, SearchSource::Wikipedia)
                .unwrap_err()
                .kind,
            ToolFailureKind::Unauthorized
        );
    }

    #[tokio::test]
    async fn request_defaults_and_normalized_result_are_exact() {
        let body = serde_json::to_vec(&response()).unwrap();
        let wire = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        let mut response_wire = wire;
        response_wire.extend_from_slice(&body);
        let (endpoint, request) = synthetic_endpoint(response_wire).await;
        let result = execute(
            &reqwest::Client::builder().no_proxy().build().unwrap(),
            &endpoint,
            context(json!({"service": SEARCH_SERVICE, "sources": ["web"]})),
            json!({"query":"  rust  ","source":"web"}),
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&request.await.unwrap()).unwrap(),
            json!({"query":"rust","source":"web","language":"en","limit":8})
        );
        let visible: Value = serde_json::from_str(result.content.as_text()).unwrap();
        assert!(visible.get("query").is_none());
        assert_eq!(result.effect_receipts.len(), 1);
    }

    #[tokio::test]
    async fn configured_client_completes_mutual_tls() {
        let identity = test_identity();
        let root = std::env::temp_dir().join(format!(
            "agl-searxng-mtls-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let certificate = root.join("client.crt");
        let key = root.join("client.pk8");
        let ca = root.join("ca.crt");
        for (path, content) in [
            (&certificate, identity.client_pem.as_bytes()),
            (&key, identity.client_key_pem.as_bytes()),
            (&ca, identity.ca_pem.as_bytes()),
        ] {
            std::fs::write(path, content).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let body = serde_json::to_vec(&response()).unwrap();
        let mut wire = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        wire.extend_from_slice(&body);
        let endpoint = synthetic_mtls_endpoint(wire, identity.acceptor).await;
        let client = build_client(&SearxngConfig {
            client_certificate: certificate,
            client_private_key: key,
            private_ca: ca,
        })
        .unwrap();
        execute(
            &client,
            &endpoint,
            context(json!({"service": SEARCH_SERVICE, "sources": ["web"]})),
            json!({"query":"rust","source":"web"}),
        )
        .await
        .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn mutual_tls_rejection_is_unauthorized() {
        let identity = test_identity();
        let endpoint = synthetic_mtls_endpoint(Vec::new(), identity.acceptor).await;
        let roots = reqwest::Certificate::from_pem(identity.ca_pem.as_bytes()).unwrap();
        let client = reqwest::Client::builder()
            .tls_certs_only(vec![roots])
            .no_proxy()
            .build()
            .unwrap();
        let failure = execute(
            &client,
            &endpoint,
            context(json!({"service": SEARCH_SERVICE, "sources": ["web"]})),
            json!({"query":"rust","source":"web"}),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.kind, ToolFailureKind::Unauthorized);
    }

    #[tokio::test]
    async fn authority_denial_causes_no_outbound_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/search", listener.local_addr().unwrap());
        let failure = execute(
            &reqwest::Client::builder().no_proxy().build().unwrap(),
            &endpoint,
            context(json!({"service": SEARCH_SERVICE, "sources": ["wikipedia"]})),
            json!({"query":"rust","source":"web"}),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.kind, ToolFailureKind::Unauthorized);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unknown_response_fields_fail_closed() {
        let mut unknown = serde_json::to_value(response()).unwrap();
        unknown["query"] = json!("must not be echoed");
        let body = serde_json::to_vec(&unknown).unwrap();
        let mut wire = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        wire.extend_from_slice(&body);
        let (endpoint, _) = synthetic_endpoint(wire).await;
        let failure = execute(
            &reqwest::Client::builder().no_proxy().build().unwrap(),
            &endpoint,
            context(json!({"service": SEARCH_SERVICE, "sources": ["web"]})),
            json!({"query":"rust","source":"web"}),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.kind, ToolFailureKind::InvalidResult);
    }

    #[tokio::test]
    async fn completed_search_keeps_receipt_when_result_exceeds_budget() {
        let mut response = response();
        response.results[0].snippet = "x".repeat(500);
        let body = serde_json::to_vec(&response).unwrap();
        let mut wire = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        wire.extend_from_slice(&body);
        let (endpoint, _) = synthetic_endpoint(wire).await;
        let context = context_with(
            json!({"service": SEARCH_SERVICE, "sources": ["web"]}),
            now_ms() + 5_000,
            512,
            ToolCancellation::new(),
        );
        let expected_receipt = admitted_receipt(&context, SearchSource::Web).unwrap();
        let result = execute(
            &reqwest::Client::builder().no_proxy().build().unwrap(),
            &endpoint,
            context,
            json!({"query":"rust","source":"web"}),
        )
        .await
        .unwrap();
        assert_eq!(result.effect_receipts, vec![expected_receipt]);
        assert!(serde_json::to_vec(&result).unwrap().len() <= 512);
        let notice: Value = serde_json::from_str(result.content.as_text()).unwrap();
        assert_eq!(notice["effect"], "committed");
        assert_eq!(notice["output"], "omitted");
    }

    #[tokio::test]
    async fn local_input_bounds_deadline_and_cancellation_are_typed() {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for input in [
            json!({"query":" ","source":"web"}),
            json!({"query":"x".repeat(513),"source":"web"}),
            json!({"query":"rust","source":"web","limit":11}),
            json!({"query":"rust","source":"web","extra":true}),
        ] {
            let failure = execute(
                &client,
                "http://127.0.0.1:1/v1/search",
                context(json!({"service": SEARCH_SERVICE, "sources": ["web"]})),
                input,
            )
            .await
            .unwrap_err();
            assert_eq!(failure.kind, ToolFailureKind::InvalidInput);
        }

        let failure = execute(
            &client,
            "http://127.0.0.1:1/v1/search",
            context_with(
                json!({"service": SEARCH_SERVICE, "sources": ["web"]}),
                now_ms(),
                65_536,
                ToolCancellation::new(),
            ),
            json!({"query":"rust","source":"web"}),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.kind, ToolFailureKind::Deadline);

        let cancellation = ToolCancellation::new();
        cancellation.cancel();
        let failure = execute(
            &client,
            "http://127.0.0.1:1/v1/search",
            context_with(
                json!({"service": SEARCH_SERVICE, "sources": ["web"]}),
                now_ms() + 5_000,
                65_536,
                cancellation,
            ),
            json!({"query":"rust","source":"web"}),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.kind, ToolFailureKind::Cancelled);
    }

    #[tokio::test]
    async fn wrong_content_type_and_oversized_response_fail_closed() {
        let response = b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}".to_vec();
        let (endpoint, _) = synthetic_endpoint(response).await;
        let failure = execute(
            &reqwest::Client::builder().no_proxy().build().unwrap(),
            &endpoint,
            context(json!({"service": SEARCH_SERVICE, "sources": ["web"]})),
            json!({"query":"rust","source":"web"}),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.kind, ToolFailureKind::InvalidResult);

        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        )
        .into_bytes();
        let (endpoint, _) = synthetic_endpoint(response).await;
        let failure = execute(
            &reqwest::Client::builder().no_proxy().build().unwrap(),
            &endpoint,
            context(json!({"service": SEARCH_SERVICE, "sources": ["web"]})),
            json!({"query":"rust","source":"web"}),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.kind, ToolFailureKind::InvalidResult);
    }

    #[test]
    fn credential_files_are_private_regular_single_link_pem() {
        let root = std::env::temp_dir().join(format!(
            "agl-searxng-credential-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let certificate = root.join("client.crt");
        std::fs::write(
            &certificate,
            b"-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        std::fs::set_permissions(&certificate, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_credential(&certificate, CredentialKind::Certificate).is_ok());

        std::fs::set_permissions(&certificate, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(read_credential(&certificate, CredentialKind::Certificate).is_err());
        std::fs::set_permissions(&certificate, std::fs::Permissions::from_mode(0o600)).unwrap();
        let hardlink = root.join("client-hardlink.crt");
        std::fs::hard_link(&certificate, &hardlink).unwrap();
        assert!(read_credential(&certificate, CredentialKind::Certificate).is_err());
        std::fs::remove_file(&hardlink).unwrap();
        let link = root.join("client-link.crt");
        symlink(&certificate, &link).unwrap();
        assert!(read_credential(&link, CredentialKind::Certificate).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
