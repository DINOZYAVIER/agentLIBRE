//! The HTTP the backends share (AGL-067, F8): the client of a server, the
//! errors it names and the reply it reads.

use std::time::Duration;

use serde_json::Value as Json;

use crate::error::Error;
use crate::backend::error::Error as BackendError;
use crate::settings::ServerKind;

/// How long connecting may take (D97).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The settings of every client agl talks to its servers with, in one
/// place (AGL-121, D19): a status is a reply, not an error; connecting may
/// take [`CONNECT_TIMEOUT`] and nothing else is timed, as a long prefill is
/// not a dead server; a redirect is not followed; an `https://` server is
/// spoken to over https only.
fn config(url: &str) -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .max_redirects(0)
        .https_only(url.starts_with("https://"))
}

/// A client of the server at `url`, which trusts only the CA in the file
/// `ca` when one is named (AGL-058, D12): the agent of [`Http`] and of the
/// whisper server's transcription (AGL-121, D19).
pub(crate) fn agent(url: &str, ca: Option<&str>) -> Result<ureq::Agent, Error> {
    Ok(match ca {
        Some(ca) => config(url).tls_config(super::private_ca(ca)?).build().into(),
        None => config(url).build().into(),
    })
}

/// A client of the server of `kind` at `url`.
pub(super) struct Http {
    kind: ServerKind,
    /// The setting that names a private CA for this server, which a
    /// certificate failure points to (AGL-121, D4).
    ca_key: &'static str,
    pub(super) url: String,
    pub(super) agent: ureq::Agent,
}

impl Http {
    /// A client for the server at `url`, such as `http://127.0.0.1:18080`.
    pub(super) fn new(kind: ServerKind, url: &str) -> Http {
        Http::of(kind, url, config(url).build().into())
    }

    /// A client that trusts only the CA in the file `ca` (AGL-058, D12).
    pub(super) fn with_ca(kind: ServerKind, url: &str, ca: &str) -> Result<Http, Error> {
        Ok(Http::of(kind, url, agent(url, Some(ca))?))
    }

    fn of(kind: ServerKind, url: &str, agent: ureq::Agent) -> Http {
        Http {
            kind,
            ca_key: match kind {
                ServerKind::Llama => "server.llama.private_ca",
                ServerKind::Strata => "server.strata.private_ca",
            },
            url: url.trim_end_matches('/').into(),
            agent,
        }
    }

    /// The same client, its certificate failures pointing to `key`: a
    /// whisper server is probed by a llama client (AGL-121, D4).
    pub(super) fn naming_ca(mut self, key: &'static str) -> Http {
        self.ca_key = key;
        self
    }

    /// A server that does not answer, or one whose certificate is not
    /// trusted (AGL-058, D12), named with the setting of its CA (AGL-121,
    /// D4).
    pub(super) fn unreachable(&self, error: impl std::fmt::Display) -> Error {
        let detail = error.to_string();
        if super::is_certificate_problem(&detail) {
            return BackendError::ServerCertificate {
                url: self.url.clone(),
                detail,
                key: self.ca_key,
            }
            .into();
        }
        BackendError::ServerUnreachable {
            kind: self.kind,
            url: self.url.clone(),
            detail,
        }
        .into()
    }

    pub(super) fn bad_reply(&self, detail: impl Into<String>) -> Error {
        BackendError::ServerReply {
            kind: self.kind,
            url: self.url.clone(),
            detail: detail.into(),
        }
        .into()
    }

    /// Sends a request and returns the response if the status is 200.
    pub(super) fn send(
        &self,
        path: &str,
        body: Option<&Json>,
    ) -> Result<ureq::http::Response<ureq::Body>, Error> {
        let url = format!("{}{path}", self.url);
        let response = match body {
            Some(body) => self
                .agent
                .post(&url)
                .header("Content-Type", "application/json")
                .send(body.to_string()),
            None => self.agent.get(&url).call(),
        }
        .map_err(|error| self.unreachable(error))?;
        let status = response.status().as_u16();
        if status == 200 {
            return Ok(response);
        }
        let text = response.into_body().read_to_string().unwrap_or_default();
        // Both servers explain errors as {"error": {"message": ...}}.
        let message = serde_json::from_str::<Json>(&text)
            .ok()
            .and_then(|reply| reply["error"]["message"].as_str().map(str::to_owned))
            .unwrap_or(text);
        Err(BackendError::ServerStatus {
            kind: self.kind,
            url: self.url.clone(),
            status,
            message,
        }
        .into())
    }

    /// Sends a request and parses the JSON reply.
    pub(super) fn json(&self, path: &str, body: Option<&Json>) -> Result<Json, Error> {
        let text = self
            .send(path, body)?
            .into_body()
            .read_to_string()
            .map_err(|error| self.unreachable(error))?;
        serde_json::from_str(&text).map_err(|error| self.bad_reply(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentlibre_common::i18n::Lang;

    #[test]
    fn a_redirect_is_not_followed_and_https_stays_https() {
        // AGL-121, D19.
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let elsewhere = TcpListener::bind("127.0.0.1:0").unwrap();
        elsewhere.set_nonblocking(true).unwrap();
        let target = format!("http://{}/", elsewhere.local_addr().unwrap());
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.local_addr().unwrap());
        let answer = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            let _ = stream.read(&mut [0u8; 1024]);
            let _ = write!(
                stream,
                "HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\n\
                 Connection: close\r\n\r\n"
            );
        });
        let response = agent(&url, None).unwrap().get(format!("{url}/props")).call().unwrap();
        answer.join().unwrap();
        assert_eq!(response.status().as_u16(), 302);
        assert!(elsewhere.accept().is_err(), "the redirect was followed");
        assert!(agent("https://gpu.lan:18080", None).unwrap().config().https_only());
        assert!(!agent(&url, None).unwrap().config().https_only());
        assert_eq!(agent(&url, None).unwrap().config().max_redirects(), 0);
    }

    #[test]
    fn a_certificate_failure_names_the_ca_setting_of_its_server() {
        // AGL-121, D4: the key of the server that failed, not llama's alone.
        let refused = |http: Http| http.unreachable("invalid peer certificate: UnknownIssuer");
        let key = |http: Http| match refused(http) {
            Error::Backend(BackendError::ServerCertificate { key, .. }) => key,
            other => panic!("{other:?}"),
        };
        assert_eq!(key(Http::new(ServerKind::Llama, "https://a")), "server.llama.private_ca");
        assert_eq!(key(Http::new(ServerKind::Strata, "https://a")), "server.strata.private_ca");
        let whisper = Http::new(ServerKind::Llama, "https://a").naming_ca("whisper.private_ca");
        let text = refused(whisper).message(Lang::En);
        assert!(text.contains("`whisper.private_ca`"), "{text}");
        assert!(matches!(
            Http::new(ServerKind::Strata, "http://a").unreachable("connection refused"),
            Error::Backend(BackendError::ServerUnreachable { .. })
        ));
    }
}
