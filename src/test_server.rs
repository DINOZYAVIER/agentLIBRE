//! Helpers for ignored tests against a live llama-server (D71). Run them
//! with a server that has a model loaded:
//!
//! ```sh
//! AGENTLIBRE_TEST_SERVER=http://127.0.0.1:18080 cargo test -- --ignored
//! ```

use serde_json::Value;

/// The server URL from `AGENTLIBRE_TEST_SERVER`.
pub fn url() -> String {
    std::env::var("AGENTLIBRE_TEST_SERVER").expect("set AGENTLIBRE_TEST_SERVER")
}

/// Posts `body` to `path`; returns the status and the response body.
pub fn post(path: &str, body: &Value) -> (u16, String) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent
        .post(format!("{}{path}", url()))
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .expect("llama-server is not reachable");
    let status = response.status().as_u16();
    (status, response.body_mut().read_to_string().unwrap())
}

/// Runs `f` on a thread whose stack fits the test matcher's recursion over
/// long free text.
pub fn with_big_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}
