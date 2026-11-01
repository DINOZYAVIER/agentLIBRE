//! The `search` tool: the human's search service, asked over mutual TLS by
//! running `curl` (AGL-019, D2-D7).

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value as Json;

use crate::action::{Outcome, Value};
use crate::error::{CredentialProblem, Error};
use crate::i18n::Lang;
use crate::interrupt::{self, signal_group};
use crate::session::INTERRUPTED;
use crate::settings::Search;
use crate::tools::ToolResult;

/// The sources and languages the service takes, and the bounds of a call
/// (D6, the old tool's).
pub const SOURCES: [&str; 2] = ["web", "wikipedia"];
pub const LANGUAGES: [&str; 4] = ["en", "fr", "ru", "uk"];
pub const DEFAULT_LANGUAGE: &str = "en";
pub const DEFAULT_LIMIT: i64 = 8;
pub const MAX_LIMIT: i64 = 10;
/// The longest query, in characters (D6).
pub const MAX_QUERY: usize = 512;

/// The most the service's answer may take (D6).
const MAX_RESPONSE: usize = 48 * 1024;
/// Seconds to connect and in all (D6).
const CONNECT_TIMEOUT: &str = "3";
const MAX_TIME: &str = "20";
/// How long `curl` gets after its own limit before it is killed.
const GRACE: Duration = Duration::from_secs(3);
/// The largest credential file (D4).
const MAX_CREDENTIAL: u64 = 1024 * 1024;

const RESULT_SCHEMA: &str = "ayeque.agent-search-result/v1";
const ERROR_SCHEMA: &str = "ayeque.agent-search-error/v1";

/// A search a call asks for, checked.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub query: String,
    pub source: String,
    pub language: String,
    pub limit: u32,
}

impl Request {
    /// The request of a call's arguments; the error is the sentence the
    /// model reads (D7). The grammar already holds the enums and the
    /// range; the query is checked here.
    pub fn from_args(args: &[(String, Value)]) -> Result<Request, String> {
        let text = |name: &str| {
            args.iter().find_map(|(n, value)| match value {
                Value::Text(text) if n == name => Some(text.as_str()),
                _ => None,
            })
        };
        let query = text("query").unwrap_or_default().trim();
        if query.is_empty() {
            return Err("search needs a query.".into());
        }
        if query.chars().count() > MAX_QUERY {
            return Err(format!("The query is longer than {MAX_QUERY} characters."));
        }
        if query.chars().any(char::is_control) {
            return Err("The query must not contain control characters.".into());
        }
        let source = text("source").unwrap_or_default();
        if !SOURCES.contains(&source) {
            return Err(format!("The source must be one of: {}.", SOURCES.join(", ")));
        }
        let language = text("language").unwrap_or(DEFAULT_LANGUAGE);
        if !LANGUAGES.contains(&language) {
            return Err(format!("The language must be one of: {}.", LANGUAGES.join(", ")));
        }
        let limit = args
            .iter()
            .find_map(|(n, value)| match value {
                Value::Int(limit) if n == "limit" => Some(*limit),
                _ => None,
            })
            .unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(format!("The limit must be from 1 to {MAX_LIMIT}."));
        }
        Ok(Request {
            query: query.into(),
            source: source.into(),
            language: language.into(),
            limit: limit as u32,
        })
    }

    /// The JSON the service takes.
    fn json(&self) -> String {
        serde_json::json!({
            "query": self.query,
            "source": self.source,
            "language": self.language,
            "limit": self.limit,
        })
        .to_string()
    }
}

/// What the agent holds to run `search`; tests give it a fake.
pub trait Searcher {
    fn search(&self, request: &Request) -> ToolResult;
}

/// The searcher of a profile without `[search]`: the tool is not in its
/// list, so this is never asked (D5).
pub struct NoSearch;

impl Searcher for NoSearch {
    fn search(&self, _: &Request) -> ToolResult {
        fail("Search is not set up.".into())
    }
}

fn fail(content: String) -> ToolResult {
    ToolResult {
        outcome: Outcome::Fail,
        content,
    }
}

/// The type of PEM a credential file holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pem {
    /// One or more `CERTIFICATE`s: the client's, or the private CA bundle.
    Certificate,
    /// One unencrypted PKCS#8 `PRIVATE KEY`.
    PrivateKey,
}

impl Pem {
    fn label(self) -> &'static str {
        match self {
            Pem::Certificate => "CERTIFICATE",
            Pem::PrivateKey => "PRIVATE KEY",
        }
    }
}

/// The credential policy (D4): a regular file of the user's own, not
/// accessible by group or others, with a single link, not empty, at most
/// 1 MiB, of the right PEM type.
pub fn check_credential(path: &Path, kind: Pem) -> Result<(), Error> {
    let refuse = |problem| Error::SearchCredential {
        file: path.into(),
        problem,
    };
    // `O_NOFOLLOW` refuses a link; `O_NONBLOCK` keeps a FIFO from
    // blocking the open.
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| match error.raw_os_error() {
            Some(libc::ELOOP) => refuse(CredentialProblem::NotRegular),
            _ => refuse(CredentialProblem::Unreadable(error.to_string())),
        })?;
    let meta = file
        .metadata()
        .map_err(|error| refuse(CredentialProblem::Unreadable(error.to_string())))?;
    if !meta.file_type().is_file() {
        return Err(refuse(CredentialProblem::NotRegular));
    }
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(refuse(CredentialProblem::NotOwn));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(refuse(CredentialProblem::OthersAccess));
    }
    if meta.nlink() > 1 {
        return Err(refuse(CredentialProblem::OtherLinks));
    }
    if meta.len() == 0 {
        return Err(refuse(CredentialProblem::Empty));
    }
    if meta.len() > MAX_CREDENTIAL {
        return Err(refuse(CredentialProblem::TooLarge));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_CREDENTIAL + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| refuse(CredentialProblem::Unreadable(error.to_string())))?;
    if bytes.len() as u64 > MAX_CREDENTIAL {
        return Err(refuse(CredentialProblem::TooLarge));
    }
    let text = String::from_utf8_lossy(&bytes);
    let labels = pem_labels(&text);
    let right = !labels.is_empty()
        && labels.iter().all(|label| *label == kind.label())
        && (kind == Pem::Certificate || labels.len() == 1);
    if !right {
        return Err(refuse(CredentialProblem::WrongType {
            expected: kind.label(),
        }));
    }
    Ok(())
}

/// The labels of the PEM blocks in `text`: `CERTIFICATE` in
/// `-----BEGIN CERTIFICATE-----`.
fn pem_labels(text: &str) -> Vec<&str> {
    text.lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("-----BEGIN ")
                .and_then(|rest| rest.strip_suffix("-----"))
        })
        .collect()
}

/// Asks the service by running `curl` (D2).
pub struct Curl {
    search: Search,
    program: OsString,
}

impl Curl {
    /// With the tools' `curl` (AGL-040).
    pub fn new(search: Search) -> Result<Curl, crate::error::Error> {
        Ok(Curl::with_program(search, crate::programs::program("curl")?.into()))
    }

    /// With another program in place of `curl`, for the tests.
    pub fn with_program(search: Search, program: OsString) -> Curl {
        Curl { search, program }
    }

    /// The arguments of `curl`; the request goes on its stdin. `-q` keeps
    /// a `.curlrc` out, `--noproxy` and `--proto` keep the call on the
    /// service's own https address, and no `-L` follows no redirect (D2).
    fn args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = [
            "-q",
            "--silent",
            "--max-time",
            MAX_TIME,
            "--connect-timeout",
            CONNECT_TIMEOUT,
            "--noproxy",
            "*",
            "--proto",
            "=https",
            "--header",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
            "--write-out",
            "\n%{http_code}",
            "--cert",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        args.push(self.search.client_certificate.clone().into());
        args.push("--key".into());
        args.push(self.search.client_private_key.clone().into());
        args.push("--cacert".into());
        args.push(self.search.private_ca.clone().into());
        args.push(self.search.url.clone().into());
        args
    }

    /// Runs `curl` with `body` and returns its stdout, capped.
    fn run(&self, body: &str) -> Result<Output, Failure> {
        let mut child = Command::new(&self.program)
            .args(self.args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(Failure::Spawn)?;
        let group = child.id();
        // The body is small, far below the pipe's size.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(body.as_bytes());
        }
        let mut stdout = child.stdout.take().expect("stdout is piped");
        // Set when the answer is over the cap: the reading stops there.
        let exceeded = Arc::new(AtomicBool::new(false));
        let reading = {
            let exceeded = Arc::clone(&exceeded);
            thread::spawn(move || {
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 8192];
                while let Ok(n) = stdout.read(&mut chunk)
                    && n > 0
                {
                    bytes.extend_from_slice(&chunk[..n]);
                    // The status line follows the body: a little room for it.
                    if bytes.len() > MAX_RESPONSE + 16 {
                        exceeded.store(true, Ordering::SeqCst);
                        break;
                    }
                }
                bytes
            })
        };
        let deadline = Instant::now() + Duration::from_secs(MAX_TIME.parse().unwrap()) + GRACE;
        let mut status = None;
        let mut stopped = None;
        loop {
            match child.try_wait() {
                Ok(Some(done)) => {
                    status = Some(done);
                    break;
                }
                Ok(None) => {}
                Err(_) => break,
            }
            if interrupt::interrupted() {
                stopped = Some(Failure::Interrupted);
            } else if exceeded.load(Ordering::SeqCst) {
                stopped = Some(Failure::TooLarge);
            } else if Instant::now() >= deadline {
                stopped = Some(Failure::TimedOut);
            }
            if stopped.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        if status.is_none() {
            end(&mut child, group);
        }
        let bytes = reading.join().unwrap_or_default();
        if let Some(failure) = stopped {
            return Err(failure);
        }
        if exceeded.load(Ordering::SeqCst) {
            return Err(Failure::TooLarge);
        }
        Ok(Output {
            code: status.and_then(|status| status.code()),
            bytes,
        })
    }
}

/// Ends `child`, gently first.
fn end(child: &mut Child, group: u32) {
    signal_group(group, libc::SIGTERM);
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    signal_group(group, libc::SIGKILL);
    let _ = child.wait();
}

/// What `curl` left: its exit code and its stdout.
struct Output {
    code: Option<i32>,
    bytes: Vec<u8>,
}

enum Failure {
    Spawn(std::io::Error),
    Interrupted,
    TimedOut,
    TooLarge,
}

impl Searcher for Curl {
    fn search(&self, request: &Request) -> ToolResult {
        for (path, kind) in [
            (&self.search.client_certificate, Pem::Certificate),
            (&self.search.client_private_key, Pem::PrivateKey),
            (&self.search.private_ca, Pem::Certificate),
        ] {
            if let Err(error) = check_credential(path, kind) {
                return fail(error.message(Lang::En));
            }
        }
        let output = match self.run(&request.json()) {
            Ok(output) => output,
            Err(Failure::Interrupted) => return fail(INTERRUPTED.into()),
            Err(Failure::TimedOut) => return fail("The search timed out.".into()),
            Err(Failure::TooLarge) => {
                return fail("The search service's answer is too large.".into());
            }
            Err(Failure::Spawn(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return fail("Cannot search: curl is not installed.".into());
            }
            Err(Failure::Spawn(error)) => return fail(format!("Cannot run curl: {error}")),
        };
        match output.code {
            Some(0) => {}
            Some(code) => return fail(curl_failure(code)),
            None => return fail("curl was killed.".into()),
        }
        answer(request, &output.bytes)
    }
}

/// The sentence for a `curl` exit code.
fn curl_failure(code: i32) -> String {
    match code {
        6 | 7 => "Cannot reach the search service.".into(),
        28 => "The search timed out.".into(),
        35 | 51 | 53 | 54 | 58 | 59 | 60 | 64 | 66 | 77 | 80 | 82 | 83 | 90 | 91 => {
            "The TLS handshake with the search service failed; check the search credentials."
                .into()
        }
        code => format!("The search failed (curl exit code {code})."),
    }
}

/// The result of the service's answer to `request`: the numbered text of
/// D6, or a short sentence for an error (D7).
fn answer(request: &Request, bytes: &[u8]) -> ToolResult {
    let text = String::from_utf8_lossy(bytes);
    let Some((body, status)) = text.trim_end().rsplit_once('\n') else {
        return fail("The search service returned no answer.".into());
    };
    let Ok(status) = status.trim().parse::<u16>() else {
        return fail("The search service returned no status.".into());
    };
    let Ok(json) = serde_json::from_str::<Json>(body) else {
        return fail(format!("The search service returned an unexpected answer (HTTP {status})."));
    };
    if status != 200 {
        return fail(error_sentence(status, &json));
    }
    match render(request, &json) {
        Some(content) => ToolResult {
            outcome: Outcome::Ok,
            content,
        },
        None => fail("The search service returned an invalid result.".into()),
    }
}

/// The sentence for the service's error answer (D7).
fn error_sentence(status: u16, json: &Json) -> String {
    let code = (json["schema"] == ERROR_SCHEMA)
        .then(|| json["code"].as_str())
        .flatten();
    match code {
        Some("invalid_request") => {
            "The search service refused the request as invalid; correct the query, the source, the language or the limit."
                .into()
        }
        Some("authentication_failed") => {
            "The search service refused the credentials; the human must check them.".into()
        }
        Some("rate_limited") => "The search service is rate limited; try again later.".into(),
        Some("upstream_unavailable") => {
            "The search engines are unavailable; try again later.".into()
        }
        Some("timeout") => "The search timed out.".into(),
        Some("invalid_upstream_response") => {
            "The search engines returned an invalid answer; try again later.".into()
        }
        _ => format!("The search service returned an unexpected answer (HTTP {status})."),
    }
}

/// A text of at most `chars` characters and `bytes` bytes, without control
/// characters other than whitespace (a line break in a snippet is fine, and
/// read as a space); `None` when it is not, or when `nonempty` and it is
/// blank.
fn bounded(value: &Json, chars: usize, bytes: usize, nonempty: bool) -> Option<&str> {
    let text = value.as_str()?;
    let ok = text.chars().count() <= chars
        && text.len() <= bytes
        && !text.chars().any(|c| c.is_control() && !c.is_whitespace())
        && !(nonempty && text.trim().is_empty());
    ok.then_some(text)
}

/// `text` on one line: every run of whitespace is one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The numbered list of D6 for a valid answer, or `None` for an invalid
/// one: the schema, the source and the language must be the request's,
/// the lists must fit their bounds and every text its own.
fn render(request: &Request, json: &Json) -> Option<String> {
    if json["schema"] != RESULT_SCHEMA
        || json["source"] != request.source.as_str()
        || json["language"] != request.language.as_str()
    {
        return None;
    }
    let results = json["results"].as_array()?;
    let suggestions = json["suggestions"].as_array()?;
    let truncated = json["truncated"].as_bool()?;
    if results.len() > request.limit as usize || results.len() > MAX_LIMIT as usize {
        return None;
    }
    if suggestions.len() > 8 {
        return None;
    }
    let mut out = String::new();
    for (i, result) in results.iter().enumerate() {
        let title = bounded(&result["title"], 512, 2048, true)?;
        let url = bounded(&result["url"], 4096, 4096, true)?;
        let snippet = bounded(&result["snippet"], 2048, 8192, false)?;
        bounded(&result["engine"], 64, 256, true)?;
        let date = match &result["published_at"] {
            Json::Null => None,
            value => Some(bounded(value, 64, 64, true)?),
        };
        out.push_str(&format!("{}. {}", i + 1, one_line(title)));
        if let Some(date) = date {
            // The day of an RFC 3339 time.
            out.push_str(&format!(" ({})", date.split('T').next().unwrap_or(date)));
        }
        out.push_str(&format!("\n   {url}\n"));
        let snippet = one_line(snippet);
        if !snippet.is_empty() {
            out.push_str(&format!("   {snippet}\n"));
        }
    }
    if results.is_empty() {
        out.push_str("No results.\n");
    }
    if !suggestions.is_empty() {
        let items = suggestions
            .iter()
            .map(|suggestion| bounded(suggestion, 256, 1024, true))
            .collect::<Option<Vec<_>>>()?;
        out.push_str(&format!("Suggestions: {}\n", items.join("; ")));
    }
    if truncated {
        out.push_str("The result was truncated.\n");
    }
    Some(out.trim_end().to_owned())
}

#[cfg(test)]
mod tests;
