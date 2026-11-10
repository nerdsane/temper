//! Shared S3 HTTP client and the bounded GET retry.
//!
//! Every `BlobStore::s3` used to build its own `reqwest::Client`. Callers
//! construct a store per read, so the pool died with the call and every GET
//! paid a fresh TCP and TLS handshake. On 2026-10-23 those handshakes were
//! the blob transport: successful `backend:s3, operation:get` spans were
//! idle for essentially their whole duration (busy time under 10ms), p95
//! about 1.5s, alerted tail about 2.5s, on objects as small as a few tens
//! of kilobytes. Semaphore wait stayed around 5ms, and cache hits stayed
//! near 0ms. Forty-three gets that were still idle at 7.94–7.99s never
//! recorded an outcome: the caller cancelled them first, and the File
//! response failed instead of returning the bytes.
//!
//! The shared client keeps live connections. A stalled attempt is bounded
//! below that cancellation and retried once, so a dead handshake does not
//! spend the whole response budget.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use reqwest::StatusCode;

use super::S3BlobStore;
use crate::blob_transport_observability::BlobTransportError;

/// One attempt, above the 2.5s tail and below the ~8s cancellation.
const S3_GET_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(3);
/// A hung connect should fail inside one attempt, leaving room to retry.
const S3_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const S3_GET_ATTEMPTS: u32 = 2;
/// Two attempts still finish before the observed ~8s cancellation.
const S3_GET_TOTAL_BUDGET: Duration = Duration::from_secs(7);
/// Drop idle pooled sockets before a typical NAT or load-balancer idle
/// timeout blackholes them. Reuse is what removes the handshake; keeping
/// a dead socket would put the stall back.
const S3_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const S3_TCP_KEEPALIVE: Duration = Duration::from_secs(15);
const S3_HTTP2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const S3_HTTP2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Clone, Debug)]
pub(super) struct S3Http {
    client: reqwest::Client,
    get_attempt_timeout: Duration,
    get_attempts: u32,
    get_total_budget: Duration,
}

impl S3Http {
    pub(super) fn production() -> Self {
        Self {
            client: shared_client(),
            get_attempt_timeout: S3_GET_ATTEMPT_TIMEOUT,
            get_attempts: S3_GET_ATTEMPTS,
            get_total_budget: S3_GET_TOTAL_BUDGET,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(
        attempt_timeout: Duration,
        attempts: u32,
        total_budget: Duration,
    ) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(attempt_timeout)
            .pool_max_idle_per_host(0)
            .build()
            .expect("test blob HTTP client configuration must be valid");
        Self {
            client,
            get_attempt_timeout: attempt_timeout,
            get_attempts: attempts.max(1),
            get_total_budget: total_budget,
        }
    }

    pub(super) fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

fn shared_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(S3_CONNECT_TIMEOUT)
                .tcp_keepalive(S3_TCP_KEEPALIVE)
                .pool_idle_timeout(S3_POOL_IDLE_TIMEOUT)
                .pool_max_idle_per_host(32)
                .http2_keep_alive_interval(S3_HTTP2_KEEPALIVE_INTERVAL)
                .http2_keep_alive_timeout(S3_HTTP2_KEEPALIVE_TIMEOUT)
                .http2_keep_alive_while_idle(true)
                .build()
                .expect("static blob HTTP client configuration must be valid")
        })
        .clone()
}

struct AttemptError {
    retryable: bool,
    transport: BlobTransportError,
}

impl S3BlobStore {
    /// Read one object, retrying a stalled or disconnected attempt once.
    ///
    /// HTTP status answers, including 404 and 5xx, are not retried: S3
    /// already responded. Only a timeout or a connection failure is.
    pub(super) async fn read_object(
        &self,
        key: &str,
    ) -> Result<(StatusCode, Option<Vec<u8>>), BlobTransportError> {
        let started = Instant::now(); // determinism-ok: production S3 GET retry budget
        let deadline = started + self.http.get_total_budget;
        let attempts = self.http.get_attempts.max(1);
        let mut last_error = None;

        for attempt in 0..attempts {
            let now = Instant::now(); // determinism-ok: production S3 GET retry budget
            let remaining = deadline.saturating_duration_since(now);
            if attempt > 0 && remaining.is_zero() {
                break;
            }
            let timeout = self
                .http
                .get_attempt_timeout
                .min(remaining.max(Duration::from_millis(1)));
            match self.read_object_once(key, timeout).await {
                Ok(body) => return Ok(body),
                Err(error) if error.retryable && attempt + 1 < attempts => {
                    last_error = Some(error.transport);
                }
                Err(error) => return Err(error.transport),
            }
        }

        Err(last_error.unwrap_or_else(|| {
            BlobTransportError::message(format!("blob GET exhausted its retry budget for '{key}'"))
        }))
    }

    async fn read_object_once(
        &self,
        key: &str,
        timeout: Duration,
    ) -> Result<(StatusCode, Option<Vec<u8>>), AttemptError> {
        let url = self.object_url(key);
        let headers = self
            .signed_headers(reqwest::Method::GET, &url)
            .map_err(|message| AttemptError {
                retryable: false,
                transport: BlobTransportError::message(message),
            })?;
        let mut request = self.http.client().get(&url).timeout(timeout);
        for (header_name, header_value) in &headers {
            request = request.header(header_name, header_value);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                return Err(attempt_transport_error(
                    error,
                    format!("blob GET request failed for '{key}'"),
                ));
            }
        };
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Ok((status, None));
        }
        if !status.is_success() {
            return Err(AttemptError {
                retryable: false,
                transport: BlobTransportError::status(
                    format!("blob GET failed for '{key}' with HTTP {status}"),
                    status,
                ),
            });
        }

        match response.bytes().await {
            Ok(bytes) => Ok((status, Some(bytes.to_vec()))),
            Err(error) => Err(attempt_transport_error(
                error,
                format!("blob GET body read failed for '{key}'"),
            )),
        }
    }
}

fn attempt_transport_error(error: reqwest::Error, context: String) -> AttemptError {
    let retryable =
        error.is_timeout() || error.is_connect() || error.is_request() || error.is_body();
    AttemptError {
        retryable,
        transport: BlobTransportError::message(format!("{context}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::blob_store::BlobStore;

    #[tokio::test]
    async fn slow_s3_get_retries_within_budget_and_returns_bytes() {
        let server = MockServer::start().await;
        // Last mounted mock wins. The stalled response is highest priority
        // for one hit; the retry then matches the immediate body.
        Mock::given(method("GET"))
            .and(path("/bucket/obj"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"object-bytes".to_vec()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/bucket/obj"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(5))
                    .set_body_bytes(b"stalled".to_vec()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let store = BlobStore::s3_with_get_retry(
            server.uri(),
            Duration::from_millis(200),
            Duration::from_secs(2),
        );
        let started = Instant::now();
        let bytes = tokio::time::timeout(Duration::from_secs(2), store.get("obj"))
            .await
            .expect("slow S3 get must finish inside the response budget")
            .expect("retry must turn the stalled attempt into a successful read");
        assert_eq!(bytes.as_deref(), Some(b"object-bytes".as_slice()));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "retried get took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn s3_get_that_responds_within_the_attempt_succeeds_without_retry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bucket/obj"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .set_body_bytes(b"once".to_vec()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let store = BlobStore::s3_with_get_retry(
            server.uri(),
            Duration::from_secs(1),
            Duration::from_secs(2),
        );
        let bytes = tokio::time::timeout(Duration::from_secs(2), store.get("obj"))
            .await
            .expect("in-budget S3 get must not be abandoned")
            .expect("in-budget S3 get must succeed");
        assert_eq!(bytes.as_deref(), Some(b"once".as_slice()));
    }
}
