//! Process-wide HTTP client and a bounded cache for immutable blob GET bodies.
//!
//! `BlobStore::s3` used to build a new [`reqwest::Client`] on every call, and
//! every File `$value` read builds a store, uses it once, and drops it. Each
//! GET therefore paid for a fresh TCP and TLS handshake. A few hundred of
//! those at once sat idle inside `blob.transport.get` for tens of seconds:
//! the buffered GET had no header or body-idle deadline, only a five-minute
//! outer timeout, and it did not retry.
//!
//! The client lives for the process. Streaming uploads and downloads set
//! their own per-request deadlines (up to thirty minutes), so this client has
//! no total timeout. Content-addressed objects do not change, so a successful
//! body can be reused until the byte budget evicts it.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures_util::StreamExt as _;
use reqwest::StatusCode;
use reqwest::header::HeaderMap;

/// How many times a buffered GET may be attempted. The second try exists for
/// a stalled handshake; the call is an idempotent read.
pub(super) const BLOB_GET_ATTEMPTS: usize = 2;

const BLOB_GET_HEADERS_TIMEOUT: Duration = Duration::from_secs(10);
const BLOB_GET_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// Largest successful body kept in the process cache. The slow production
/// reads were about four megabytes, so those objects fit.
pub(super) const BLOB_GET_CACHE_MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
const BLOB_GET_CACHE_MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// Header deadline. Tests use a short one so a hung peer fails the build
/// quickly; production reads that are healthy return in well under a second.
fn headers_timeout() -> Duration {
    if cfg!(test) {
        Duration::from_millis(750)
    } else {
        BLOB_GET_HEADERS_TIMEOUT
    }
}

/// How long a buffered GET waits for the next body chunk. A download that
/// keeps arriving is allowed to run until the outer five-minute budget.
fn idle_timeout() -> Duration {
    if cfg!(test) {
        Duration::from_millis(750)
    } else {
        BLOB_GET_IDLE_TIMEOUT
    }
}

/// Shared client for every S3-compatible blob store in the process.
///
/// `determinism-ok`: production blob I/O. Simulation uses the local backend.
pub(super) fn shared_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .tcp_keepalive(Duration::from_secs(30))
                .pool_idle_timeout(Duration::from_secs(50))
                .pool_max_idle_per_host(32)
                .build()
                .expect("static blob HTTP client configuration must be valid")
        })
        .clone()
}

/// One buffered GET, either a body or a confirmed miss.
pub(super) enum BufferedGet {
    /// The object exists.
    Found {
        /// HTTP status of the successful read.
        status: StatusCode,
        /// Object bytes.
        bytes: Vec<u8>,
    },
    /// The object is not in the bucket.
    Missing {
        /// HTTP status, normally 404.
        status: StatusCode,
    },
}

/// A failed buffered GET. `retryable` is set for timeouts and transport
/// failures, which a second attempt can survive, and for 5xx responses.
pub(super) struct BufferedGetFailure {
    /// Error text returned to the caller.
    pub(super) message: String,
    /// HTTP status when the failure was a response.
    pub(super) status: Option<StatusCode>,
    /// Whether another attempt of this idempotent GET is useful.
    pub(super) retryable: bool,
}

impl BufferedGetFailure {
    fn transport(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            message: message.into(),
            status: None,
            retryable,
        }
    }

    pub(super) fn fatal(message: impl Into<String>) -> Self {
        Self::transport(message, false)
    }
}

/// Send one buffered GET. The header wait and the gap between body chunks
/// are bounded; the total time is still the caller's outer deadline.
pub(super) async fn send_buffered_get(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    key: &str,
) -> Result<BufferedGet, BufferedGetFailure> {
    let mut request = client.get(url);
    for (name, value) in headers {
        request = request.header(name, value);
    }

    let response = match tokio::time::timeout(headers_timeout(), request.send()).await {
        Err(_) => {
            return Err(BufferedGetFailure::transport(
                format!("blob GET headers timed out for '{key}'"),
                true,
            ));
        }
        Ok(Err(error)) => {
            return Err(BufferedGetFailure {
                message: format!("blob GET request failed for '{key}': {error}"),
                status: error.status(),
                retryable: error.is_timeout() || error.is_connect() || error.is_request(),
            });
        }
        Ok(Ok(response)) => response,
    };

    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Ok(BufferedGet::Missing { status });
    }
    if !status.is_success() {
        return Err(BufferedGetFailure {
            message: format!("blob GET failed for '{key}' with HTTP {status}"),
            status: Some(status),
            retryable: status.is_server_error(),
        });
    }

    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    loop {
        match tokio::time::timeout(idle_timeout(), stream.next()).await {
            Err(_) => {
                return Err(BufferedGetFailure::transport(
                    format!("blob GET body stalled for '{key}'"),
                    true,
                ));
            }
            Ok(None) => break,
            Ok(Some(Err(error))) => {
                return Err(BufferedGetFailure {
                    message: format!("blob GET body read failed for '{key}': {error}"),
                    status: error.status(),
                    retryable: error.is_timeout() || error.is_connect() || error.is_request(),
                });
            }
            Ok(Some(Ok(chunk))) => bytes.extend_from_slice(&chunk),
        }
    }
    Ok(BufferedGet::Found { status, bytes })
}

struct CacheEntry {
    bytes: Vec<u8>,
    seq: u64,
}

struct BlobGetCache {
    bodies: BTreeMap<String, CacheEntry>,
    by_age: BTreeMap<u64, String>,
    next_seq: u64,
    total_bytes: usize,
    max_entry: usize,
    max_total: usize,
}

impl BlobGetCache {
    fn production() -> Self {
        Self {
            bodies: BTreeMap::new(),
            by_age: BTreeMap::new(),
            next_seq: 0,
            total_bytes: 0,
            max_entry: BLOB_GET_CACHE_MAX_ENTRY_BYTES,
            max_total: BLOB_GET_CACHE_MAX_TOTAL_BYTES,
        }
    }

    fn get(&mut self, key: &str) -> Option<Vec<u8>> {
        let seq = self.bodies.get(key)?.seq;
        let bytes = self.bodies.get(key)?.bytes.clone();
        self.touch(key, seq);
        Some(bytes)
    }

    fn touch(&mut self, key: &str, seq: u64) {
        self.by_age.remove(&seq);
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.by_age.insert(seq, key.to_string());
        if let Some(entry) = self.bodies.get_mut(key) {
            entry.seq = seq;
        }
    }

    fn insert(&mut self, key: String, bytes: &[u8]) {
        if bytes.len() > self.max_entry || bytes.len() > self.max_total {
            return;
        }
        if let Some(existing) = self.bodies.remove(&key) {
            self.by_age.remove(&existing.seq);
            self.total_bytes = self.total_bytes.saturating_sub(existing.bytes.len());
        }
        while self.total_bytes.saturating_add(bytes.len()) > self.max_total {
            let Some((_, old_key)) = self.by_age.pop_first() else {
                break;
            };
            if let Some(old) = self.bodies.remove(&old_key) {
                self.total_bytes = self.total_bytes.saturating_sub(old.bytes.len());
            }
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());
        self.by_age.insert(seq, key.clone());
        self.bodies.insert(
            key,
            CacheEntry {
                bytes: bytes.to_vec(),
                seq,
            },
        );
    }
}

fn cache() -> &'static Mutex<BlobGetCache> {
    static CACHE: OnceLock<Mutex<BlobGetCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BlobGetCache::production()))
}

pub(super) fn cached_blob(key: &str) -> Option<Vec<u8>> {
    cache()
        .lock()
        .expect("blob get cache lock") // ci-ok: process cache, poisoned only after a panic
        .get(key)
}

pub(super) fn store_cached_blob(key: &str, bytes: &[u8]) {
    cache()
        .lock()
        .expect("blob get cache lock") // ci-ok: process cache, poisoned only after a panic
        .insert(key.to_string(), bytes);
}

#[cfg(test)]
mod tests;
