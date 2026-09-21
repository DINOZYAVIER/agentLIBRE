use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use matrix_sdk::{Client, Error, HttpError, config::SyncSettings};

#[derive(Debug)]
pub struct PermanentMatrixFault;

impl std::fmt::Display for PermanentMatrixFault {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "permanent Matrix fault; correct configuration or authentication before restarting",
        )
    }
}

impl std::error::Error for PermanentMatrixFault {}

pub(crate) async fn sync(client: &Client) -> Result<()> {
    // The SDK persists successful sync batches in its configured SQLite store;
    // sync_stream advances its token only after a successful sync_once.
    let mut stream = Box::pin(client.sync_stream(SyncSettings::default()).await);
    let mut failures = 0_u32;
    while let Some(result) = stream.next().await {
        match result {
            Ok(_) => {
                if failures != 0 {
                    tracing::info!(status = "connected", "Matrix sync recovered");
                }
                failures = 0;
            }
            Err(error) if transient(&error) => {
                failures = failures.saturating_add(1);
                let entropy = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos() as u64
                    ^ u64::from(std::process::id());
                let delay = retry_delay(failures, entropy);
                tracing::warn!(
                    status = "disconnected/retrying",
                    failures,
                    retry_ms = delay.as_millis() as u64,
                    "Matrix sync temporarily unavailable"
                );
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error).context(PermanentMatrixFault),
        }
    }
    Ok(())
}

fn transient(error: &Error) -> bool {
    match error {
        Error::Http(error) => transient_http(error),
        _ => false,
    }
}

fn transient_http(error: &HttpError) -> bool {
    match error {
        HttpError::Reqwest(error) => error.is_connect() || error.is_timeout(),
        HttpError::Cached(error) => transient_http(error),
        _ => error.as_client_api_error().is_some_and(|error| {
            error.status_code.is_server_error() || error.status_code.as_u16() == 429
        }),
    }
}

fn retry_delay(failures: u32, entropy: u64) -> Duration {
    let ceiling_ms = (1_000_u64 << failures.saturating_sub(1).min(6)).min(60_000);
    let floor_ms = ceiling_ms / 2;
    Duration::from_millis(floor_ms + entropy % (ceiling_ms - floor_ms + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::api::{
        client::uiaa::UiaaResponse,
        error::{Error as ApiError, ErrorBody, FromHttpResponseError},
    };

    #[test]
    fn transient_server_failures_do_not_include_authentication_or_local_errors() {
        for (status, expected) in [
            (502, true),
            (503, true),
            (429, true),
            (401, false),
            (403, false),
            (400, false),
        ] {
            let api = ApiError::new(
                status.try_into().unwrap(),
                ErrorBody::Json(serde_json::json!({"proxy": "not a Matrix error envelope"})),
            );
            let error = Error::Http(Box::new(HttpError::Api(Box::new(
                FromHttpResponseError::Server(UiaaResponse::MatrixError(api)),
            ))));
            assert_eq!(transient(&error), expected, "status {status}");
        }
        assert!(!transient(&Error::AuthenticationRequired));
    }

    #[test]
    fn retry_backoff_is_exponential_jittered_and_bounded() {
        assert_eq!(retry_delay(1, 0), Duration::from_millis(500));
        assert_eq!(retry_delay(2, 0), Duration::from_millis(1000));
        assert_ne!(retry_delay(3, 7), retry_delay(3, 8));
        for failures in [1, 2, 10, u32::MAX] {
            for entropy in [0, 500, u64::MAX] {
                let delay = retry_delay(failures, entropy);
                assert!(delay >= Duration::from_millis(500));
                assert!(delay <= Duration::from_secs(60));
            }
        }
    }

    #[tokio::test]
    async fn sdk_retry_keeps_the_durable_token_and_resumes_after_reopen() {
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        };
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let positions = Arc::new(Mutex::new(Vec::new()));
        let observed = positions.clone();
        let count = calls.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut first = String::new();
                stream.read_line(&mut first).await.unwrap();
                let mut content_length = 0;
                loop {
                    let mut header = String::new();
                    stream.read_line(&mut header).await.unwrap();
                    if header == "\r\n" || header.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        content_length = value.trim().parse::<usize>().unwrap();
                    }
                }
                assert!(content_length < 1024 * 1024);
                stream
                    .read_exact(&mut vec![0; content_length])
                    .await
                    .unwrap();
                let (status, body) = if first.contains("/sync?") {
                    observed.lock().unwrap().push(first.clone());
                    match count.fetch_add(1, Ordering::AcqRel) {
                        0 => (
                            200,
                            r#"{"next_batch":"position1","rooms":{},"device_one_time_keys_count":{"signed_curve25519":50}}"#,
                        ),
                        1 => (502, "temporary proxy outage"),
                        2 => (
                            200,
                            r#"{"next_batch":"position2","rooms":{},"device_one_time_keys_count":{"signed_curve25519":50}}"#,
                        ),
                        3 => (
                            401,
                            r#"{"errcode":"M_UNKNOWN_TOKEN","error":"fixture stop"}"#,
                        ),
                        _ => (
                            200,
                            r#"{"next_batch":"position3","rooms":{},"device_one_time_keys_count":{"signed_curve25519":50}}"#,
                        ),
                    }
                } else if first.contains("/keys/upload") {
                    (200, r#"{"one_time_key_counts":{"signed_curve25519":50}}"#)
                } else if first.contains("/keys/query") {
                    (200, r#"{"device_keys":{},"failures":{}}"#)
                } else {
                    (200, "{}")
                };
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            }
        });
        let root = std::env::temp_dir().join(format!(
            "agl-matrix-retry-{}",
            agl_core::ConversationId::generate()
        ));
        std::fs::create_dir(&root).unwrap();
        let make_client = || async {
            let client = Client::builder()
                .homeserver_url(&url)
                .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
                .request_config(
                    matrix_sdk::config::RequestConfig::default()
                        .retry_limit(0)
                        .timeout(Duration::from_secs(2)),
                )
                .sqlite_store(&root, None)
                .build()
                .await
                .unwrap();
            client
                .matrix_auth()
                .restore_session(
                    matrix_sdk::authentication::matrix::MatrixSession {
                        meta: matrix_sdk::SessionMeta {
                            user_id: "@bridge:localhost".parse().unwrap(),
                            device_id: "FIXTURE".into(),
                        },
                        tokens: matrix_sdk::SessionTokens {
                            access_token: "fixture-not-a-real-token".into(),
                            refresh_token: None,
                        },
                    },
                    matrix_sdk::store::RoomLoadSettings::default(),
                )
                .await
                .unwrap();
            client
        };
        let client = make_client().await;
        let error = tokio::time::timeout(Duration::from_secs(20), sync(&client))
            .await
            .unwrap()
            .unwrap_err();
        assert!(crate::is_permanent_matrix_error(&error));
        assert_eq!(calls.load(Ordering::Acquire), 4);
        drop(client);
        let client = make_client().await;
        tokio::time::timeout(
            Duration::from_secs(5),
            client.sync_once(SyncSettings::default()),
        )
        .await
        .unwrap()
        .unwrap();
        let positions = positions.lock().unwrap().clone();
        assert!(!positions[0].contains("since="));
        assert!(positions[1].contains("since=position1"));
        assert!(positions[2].contains("since=position1"));
        assert!(positions[3].contains("since=position2"));
        assert!(positions[4].contains("since=position2"));
        drop(client);
        server.abort();
        let _ = server.await;
        std::fs::remove_dir_all(root).unwrap();
    }
}
