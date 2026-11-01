use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// A directory removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("agentLIBRE-search-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    /// A file with `text` and `mode`.
    fn file(&self, name: &str, text: &str, mode: u32) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const CERT: &str = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
const KEY: &str = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n";

fn problem(path: &Path, kind: Pem) -> CredentialProblem {
    match check_credential(path, kind) {
        Err(Error::SearchCredential { problem, file }) => {
            assert_eq!(file, path);
            problem
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn text(name: &str, value: &str) -> (String, Value) {
    (name.into(), Value::Text(value.into()))
}

fn call(query: &str, source: &str) -> Vec<(String, Value)> {
    vec![text("query", query), text("source", source)]
}

// --- the request ---

#[test]
fn a_call_gets_the_defaults_and_a_trimmed_query() {
    let request = Request::from_args(&call("  rust lifetimes ", "web")).unwrap();
    assert_eq!(
        request,
        Request {
            query: "rust lifetimes".into(),
            source: "web".into(),
            language: "en".into(),
            limit: 8,
        }
    );
    let mut args = call("Київ", "wikipedia");
    args.push(text("language", "uk"));
    args.push(("limit".into(), Value::Int(3)));
    let request = Request::from_args(&args).unwrap();
    assert_eq!((request.language.as_str(), request.limit), ("uk", 3));
    assert_eq!(
        serde_json::from_str::<Json>(&request.json()).unwrap(),
        serde_json::json!({"query": "Київ", "source": "wikipedia", "language": "uk", "limit": 3})
    );
}

#[test]
fn a_request_outside_the_bounds_is_refused() {
    let refused = |args: Vec<(String, Value)>| Request::from_args(&args).unwrap_err();
    assert!(refused(call("", "web")).contains("needs a query"));
    assert!(refused(call("   ", "web")).contains("needs a query"));
    assert!(refused(call(&"a".repeat(513), "web")).contains("512"));
    // 512 characters is the longest, whatever their bytes.
    assert!(Request::from_args(&call(&"я".repeat(512), "web")).is_ok());
    assert!(refused(call("a\nb", "web")).contains("control"));
    assert!(refused(call("a\u{7}b", "web")).contains("control"));
    assert!(refused(call("a", "news")).contains("web, wikipedia"));
    let mut args = call("a", "web");
    args.push(text("language", "de"));
    assert!(refused(args).contains("en, fr, ru, uk"));
    for limit in [0, 11, -1] {
        let mut args = call("a", "web");
        args.push(("limit".into(), Value::Int(limit)));
        assert!(refused(args).contains("1 to 10"), "{limit}");
    }
}

// --- the credential policy ---

#[test]
fn a_good_credential_is_taken() {
    let dir = TempDir::new();
    assert!(check_credential(&dir.file("c.crt", CERT, 0o600), Pem::Certificate).is_ok());
    // A chain and a bundle hold several.
    let chain = format!("{CERT}{CERT}");
    assert!(check_credential(&dir.file("ca.crt", &chain, 0o400), Pem::Certificate).is_ok());
    assert!(check_credential(&dir.file("k.pk8", KEY, 0o600), Pem::PrivateKey).is_ok());
}

#[test]
fn a_file_of_group_or_others_access_is_refused() {
    let dir = TempDir::new();
    for mode in [0o640, 0o604, 0o660, 0o644, 0o700 | 0o010] {
        let path = dir.file("c.crt", CERT, mode);
        assert_eq!(problem(&path, Pem::Certificate), CredentialProblem::OthersAccess, "{mode:o}");
    }
}

#[test]
fn a_link_a_second_link_a_directory_and_a_missing_file_are_refused() {
    let dir = TempDir::new();
    let real = dir.file("real.crt", CERT, 0o600);
    let link = dir.0.join("link.crt");
    symlink(&real, &link).unwrap();
    assert_eq!(problem(&link, Pem::Certificate), CredentialProblem::NotRegular);
    // A second hard link to the same file.
    let hard = dir.0.join("hard.crt");
    fs::hard_link(&real, &hard).unwrap();
    assert_eq!(problem(&real, Pem::Certificate), CredentialProblem::OtherLinks);
    assert_eq!(problem(&hard, Pem::Certificate), CredentialProblem::OtherLinks);
    assert_eq!(problem(&dir.0, Pem::Certificate), CredentialProblem::NotRegular);
    assert!(matches!(
        problem(&dir.0.join("none.crt"), Pem::Certificate),
        CredentialProblem::Unreadable(_)
    ));
}

#[test]
fn an_empty_or_a_large_file_is_refused() {
    let dir = TempDir::new();
    assert_eq!(problem(&dir.file("e.crt", "", 0o600), Pem::Certificate), CredentialProblem::Empty);
    let big = format!("{CERT}{}", "#".repeat(1024 * 1024));
    assert_eq!(problem(&dir.file("b.crt", &big, 0o600), Pem::Certificate), CredentialProblem::TooLarge);
    // Exactly 1 MiB is still taken.
    let edge = format!("{CERT}{}", "#".repeat(1024 * 1024 - CERT.len()));
    assert_eq!(edge.len(), 1024 * 1024);
    assert!(check_credential(&dir.file("o.crt", &edge, 0o600), Pem::Certificate).is_ok());
}

#[test]
fn the_wrong_pem_type_is_refused() {
    let dir = TempDir::new();
    let wrong = |expected| CredentialProblem::WrongType { expected };
    // A key where a certificate is needed, and the reverse.
    assert_eq!(problem(&dir.file("a", KEY, 0o600), Pem::Certificate), wrong("CERTIFICATE"));
    assert_eq!(problem(&dir.file("b", CERT, 0o600), Pem::PrivateKey), wrong("PRIVATE KEY"));
    // An encrypted key and an old RSA one are not a PKCS#8 key in the clear.
    let encrypted = "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----\n";
    let rsa = "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\n";
    assert_eq!(problem(&dir.file("c", encrypted, 0o600), Pem::PrivateKey), wrong("PRIVATE KEY"));
    assert_eq!(problem(&dir.file("d", rsa, 0o600), Pem::PrivateKey), wrong("PRIVATE KEY"));
    // Two keys, a key among certificates, and text that is no PEM.
    assert_eq!(problem(&dir.file("e", &format!("{KEY}{KEY}"), 0o600), Pem::PrivateKey), wrong("PRIVATE KEY"));
    assert_eq!(problem(&dir.file("f", &format!("{CERT}{KEY}"), 0o600), Pem::Certificate), wrong("CERTIFICATE"));
    assert_eq!(problem(&dir.file("g", "hello\n", 0o600), Pem::Certificate), wrong("CERTIFICATE"));
}

#[test]
fn the_refusal_reads_in_both_languages() {
    let error = Error::SearchCredential {
        file: "/c/client.crt".into(),
        problem: CredentialProblem::OthersAccess,
    };
    assert_eq!(
        error.message(Lang::En),
        "the search credential /c/client.crt is refused: the group or others can access it; make it mode 0600"
    );
    assert!(error.message(Lang::Uk).contains("/c/client.crt"));
    assert!(error.message(Lang::Uk).contains("0600"));
}

// --- asking the service through a fake `curl` ---

/// A directory with good credentials, a fake `curl` and the `Search`
/// pointing at them.
struct Fake {
    dir: TempDir,
    search: Search,
}

impl Fake {
    fn new() -> Fake {
        let dir = TempDir::new();
        let search = Search {
            url: "https://search.example/v1/search".into(),
            client_certificate: dir.file("client.crt", CERT, 0o600),
            client_private_key: dir.file("client.pk8", KEY, 0o600),
            private_ca: dir.file("ca.crt", CERT, 0o600),
        };
        let script = dir.file(
            "curl",
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\ncat > \"$0.stdin\"\n\
             cat \"$0.reply\"\nexit \"$(cat \"$0.exit\" 2>/dev/null || echo 0)\"\n",
            0o755,
        );
        assert!(script.exists());
        Fake { dir, search }
    }

    /// What the fake prints: the body, then the status on its own line.
    fn replies(&self, body: &str, status: u16) {
        self.dir.file("curl.reply", &format!("{body}\n{status}"), 0o600);
    }

    fn exits(&self, code: i32) {
        self.dir.file("curl.exit", &code.to_string(), 0o600);
    }

    fn curl(&self) -> Curl {
        Curl::with_program(self.search.clone(), self.dir.0.join("curl").into())
    }

    fn ask(&self, args: Vec<(String, Value)>) -> ToolResult {
        self.curl().search(&Request::from_args(&args).unwrap())
    }

    fn args(&self) -> Vec<String> {
        fs::read_to_string(self.dir.0.join("curl.args"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn stdin(&self) -> String {
        fs::read_to_string(self.dir.0.join("curl.stdin")).unwrap()
    }
}

fn result_json(extra: Json) -> String {
    let mut json = serde_json::json!({
        "schema": "ayeque.agent-search-result/v1",
        "source": "web",
        "language": "en",
        "results": [],
        "suggestions": [],
        "truncated": false,
    });
    for (key, value) in extra.as_object().unwrap() {
        json[key] = value.clone();
    }
    json.to_string()
}

fn ok(content: &str) -> ToolResult {
    ToolResult {
        outcome: Outcome::Ok,
        content: content.into(),
    }
}

#[test]
fn curl_gets_the_json_on_stdin_and_the_flags_of_the_decision() {
    let fake = Fake::new();
    fake.replies(&result_json(serde_json::json!({})), 200);
    fake.ask(call("rust lifetimes", "web"));
    assert_eq!(
        serde_json::from_str::<Json>(&fake.stdin()).unwrap(),
        serde_json::json!({"query": "rust lifetimes", "source": "web", "language": "en", "limit": 8})
    );
    let args = fake.args();
    let after = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
    assert_eq!(args[0], "-q");
    assert_eq!(after("--max-time"), "20");
    assert_eq!(after("--connect-timeout"), "3");
    assert_eq!(after("--noproxy"), "*");
    assert_eq!(after("--proto"), "=https");
    assert_eq!(after("--cert"), fake.search.client_certificate.to_str().unwrap());
    assert_eq!(after("--key"), fake.search.client_private_key.to_str().unwrap());
    assert_eq!(after("--cacert"), fake.search.private_ca.to_str().unwrap());
    assert_eq!(after("--data-binary"), "@-");
    assert_eq!(args.last().unwrap(), "https://search.example/v1/search");
    // No redirect is followed, and no flag turns a check off.
    for flag in ["-L", "--location", "-k", "--insecure", "--proxy"] {
        assert!(!args.iter().any(|a| a == flag), "{flag}");
    }
}

#[test]
fn the_answer_is_a_numbered_list_with_dates_suggestions_and_the_truncation() {
    let fake = Fake::new();
    fake.replies(
        &result_json(serde_json::json!({
            "results": [
                {"title": "Rust", "url": "https://rust-lang.org/", "snippet": "A language\n  empowering everyone.",
                 "engine": "brave", "published_at": "2026-09-02T07:52:15Z"},
                {"title": "Book", "url": "https://doc.rust-lang.org/book/", "snippet": "",
                 "engine": "google cse", "published_at": null},
            ],
            "suggestions": ["rust book", "rust std"],
            "truncated": true,
        })),
        200,
    );
    assert_eq!(
        fake.ask(call("rust", "web")),
        ok("1. Rust (2026-09-02)\n   https://rust-lang.org/\n   A language empowering everyone.\n\
            2. Book\n   https://doc.rust-lang.org/book/\n\
            Suggestions: rust book; rust std\nThe result was truncated.")
    );
}

#[test]
fn an_empty_list_says_there_are_no_results() {
    let fake = Fake::new();
    fake.replies(&result_json(serde_json::json!({})), 200);
    assert_eq!(fake.ask(call("zzzz", "web")), ok("No results."));
    // With suggestions: both lines.
    fake.replies(&result_json(serde_json::json!({"suggestions": ["zz"]})), 200);
    assert_eq!(fake.ask(call("zzzz", "web")), ok("No results.\nSuggestions: zz"));
}

#[test]
fn each_error_code_of_the_service_is_a_short_message() {
    let fake = Fake::new();
    let error = |status, code: &str| {
        fake.replies(&format!("{{\"schema\":\"ayeque.agent-search-error/v1\",\"code\":\"{code}\"}}"), status);
        let result = fake.ask(call("a", "web"));
        assert_eq!(result.outcome, Outcome::Fail, "{code}");
        result.content
    };
    assert!(error(400, "invalid_request").contains("invalid"));
    assert!(error(401, "authentication_failed").contains("credentials"));
    assert!(error(403, "authentication_failed").contains("credentials"));
    assert!(error(429, "rate_limited").contains("rate limited"));
    assert!(error(503, "upstream_unavailable").contains("unavailable"));
    assert!(error(504, "timeout").contains("timed out"));
    assert!(error(502, "invalid_upstream_response").contains("invalid answer"));
    // An unknown code and a status without an error body.
    assert!(error(500, "something_else").contains("unexpected answer (HTTP 500)"));
    fake.replies("not json", 502);
    assert!(fake.ask(call("a", "web")).content.contains("unexpected answer (HTTP 502)"));
    fake.replies("", 200);
    assert!(fake.ask(call("a", "web")).content.contains("unexpected answer (HTTP 200)"));
}

#[test]
fn a_curl_that_fails_is_a_short_message() {
    let fake = Fake::new();
    for (code, words) in [
        (7, "Cannot reach"),
        (28, "timed out"),
        (60, "TLS handshake"),
        (58, "TLS handshake"),
        (35, "TLS handshake"),
        (2, "curl exit code 2"),
    ] {
        fake.exits(code);
        let result = fake.ask(call("a", "web"));
        assert_eq!(result.outcome, Outcome::Fail);
        assert!(result.content.contains(words), "{code}: {}", result.content);
    }
}

#[test]
fn an_invalid_answer_is_refused_whole() {
    let fake = Fake::new();
    let one = serde_json::json!({"title": "T", "url": "https://x.example/", "snippet": "s", "engine": "e", "published_at": null});
    let cases: Vec<(&str, Json)> = vec![
        ("schema", serde_json::json!({"schema": "other/v1"})),
        ("source", serde_json::json!({"source": "wikipedia"})),
        ("language", serde_json::json!({"language": "uk"})),
        ("too many results", serde_json::json!({"results": vec![one.clone(); 9]})),
        ("control character", serde_json::json!({"results": [{"title": "a\u{7}", "url": "https://x.example/", "snippet": "", "engine": "e", "published_at": null}]})),
        ("blank title", serde_json::json!({"results": [{"title": " ", "url": "https://x.example/", "snippet": "", "engine": "e", "published_at": null}]})),
        ("long snippet", serde_json::json!({"results": [{"title": "t", "url": "https://x.example/", "snippet": "s".repeat(2049), "engine": "e", "published_at": null}]})),
        ("suggestions", serde_json::json!({"suggestions": vec!["s"; 9]})),
        ("truncated", serde_json::json!({"truncated": "yes"})),
    ];
    for (what, extra) in cases {
        fake.replies(&result_json(extra), 200);
        // The limit is 8 by default, so 9 results do not fit.
        let result = fake.ask(call("a", "web"));
        assert_eq!(result.outcome, Outcome::Fail, "{what}");
        assert!(result.content.contains("invalid result"), "{what}: {}", result.content);
    }
    // The limit asked for bounds the list too: 3 results do not fit 2.
    fake.replies(&result_json(serde_json::json!({"results": vec![one; 3]})), 200);
    let mut args = call("a", "web");
    args.push(("limit".into(), Value::Int(2)));
    assert_eq!(fake.ask(args).outcome, Outcome::Fail);
}

#[test]
fn an_answer_over_48_kib_is_refused() {
    let fake = Fake::new();
    let big = result_json(serde_json::json!({"suggestions": ["x"], "padding": "p".repeat(60 * 1024)}));
    fake.replies(&big, 200);
    let result = fake.ask(call("a", "web"));
    assert_eq!(result.outcome, Outcome::Fail);
    assert!(result.content.contains("too large"), "{}", result.content);
    // Just under the cap, an answer is read.
    fake.replies(&result_json(serde_json::json!({"results": [
        {"title": "T", "url": "https://x.example/", "snippet": "s".repeat(2048), "engine": "e", "published_at": null}
    ]})), 200);
    assert_eq!(fake.ask(call("a", "web")).outcome, Outcome::Ok);
}

#[test]
fn a_credential_the_policy_refuses_stops_the_call_before_curl_runs() {
    let fake = Fake::new();
    fake.replies(&result_json(serde_json::json!({})), 200);
    fs::set_permissions(&fake.search.client_private_key, fs::Permissions::from_mode(0o644)).unwrap();
    let result = fake.ask(call("a", "web"));
    assert_eq!(result.outcome, Outcome::Fail);
    assert!(result.content.contains("client.pk8"), "{}", result.content);
    assert!(result.content.contains("0600"), "{}", result.content);
    assert!(!fake.dir.0.join("curl.args").exists(), "curl ran");
}

#[test]
fn a_missing_curl_is_said() {
    let fake = Fake::new();
    let curl = Curl::with_program(fake.search.clone(), fake.dir.0.join("no-such-curl").into());
    let result = curl.search(&Request::from_args(&call("a", "web")).unwrap());
    assert_eq!(result.outcome, Outcome::Fail);
    assert_eq!(result.content, "Cannot search: curl is not installed.");
}

#[test]
fn a_profile_without_search_has_a_searcher_that_only_fails() {
    let result = NoSearch.search(&Request::from_args(&call("a", "web")).unwrap());
    assert_eq!(result.outcome, Outcome::Fail);
}

/// Against the human's service with the `[search]` of their user file:
/// `cargo test -- --ignored the_live_service`. Not run by default.
#[test]
#[ignore = "needs the human's [search] and the service"]
fn the_live_service_answers_web_and_wikipedia() {
    let home = std::env::var_os("HOME").expect("HOME");
    let path = Path::new(&home).join(".config/agentLIBRE/agentLIBRE.toml");
    let user = crate::settings::UserFile::load(&path).unwrap();
    let search = user.search.complete().unwrap().expect("[search] is set in the user file");
    let curl = Curl::new(search).unwrap();
    for (source, language, query) in [("web", "en", "rust programming language"), ("wikipedia", "en", "Kyiv")] {
        let mut args = call(query, source);
        args.push(text("language", language));
        args.push(("limit".into(), Value::Int(3)));
        let result = curl.search(&Request::from_args(&args).unwrap());
        println!("--- {source}\n{}", result.content);
        assert_eq!(result.outcome, Outcome::Ok, "{}", result.content);
        assert!(result.content.starts_with("1. ") || result.content == "No results.");
    }
}
