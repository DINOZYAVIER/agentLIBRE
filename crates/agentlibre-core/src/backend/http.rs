//! The HTTP the backends share (AGL-067, F8): the client of a server, the
//! errors it names and the reply it reads.

use std::time::Duration;

use serde_json::Value as Json;

use crate::error::Error;
use crate::settings::ServerKind;

/// How long connecting may take (D97).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A client of the server of `kind` at `url`.
pub(super) struct Http {
    kind: ServerKind,
    pub(super) url: String,
    pub(super) agent: ureq::Agent,
}

impl Http {
    /// A client for the server at `url`, such as `http://127.0.0.1:18080`.
    pub(super) fn new(kind: ServerKind, url: &str) -> Http {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .build()
            .into();
        Http::of(kind, url, agent)
    }

    /// A client that trusts only the CA in the file `ca` (AGL-058, D12).
    pub(super) fn with_ca(kind: ServerKind, url: &str, ca: &str) -> Result<Http, Error> {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .tls_config(super::private_ca(ca)?)
            .build()
            .into();
        Ok(Http::of(kind, url, agent))
    }

    fn of(kind: ServerKind, url: &str, agent: ureq::Agent) -> Http {
        Http {
            kind,
            url: url.trim_end_matches('/').into(),
            agent,
        }
    }

    /// A server that does not answer; for the llama-server, one whose
    /// certificate is not trusted (AGL-058, D12: the message names
    /// `server.llama.private_ca`, which Strata has not).
    pub(super) fn unreachable(&self, error: impl std::fmt::Display) -> Error {
        let detail = error.to_string();
        if self.kind == ServerKind::Llama && super::is_certificate_problem(&detail) {
            return Error::ServerCertificate {
                url: self.url.clone(),
                detail,
            };
        }
        Error::ServerUnreachable {
            kind: self.kind,
            url: self.url.clone(),
            detail,
        }
    }

    pub(super) fn bad_reply(&self, detail: impl Into<String>) -> Error {
        Error::ServerReply {
            kind: self.kind,
            url: self.url.clone(),
            detail: detail.into(),
        }
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
        Err(Error::ServerStatus {
            kind: self.kind,
            url: self.url.clone(),
            status,
            message,
        })
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
