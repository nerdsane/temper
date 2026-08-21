//! Bounded transport decoding for remote Genesis bundle responses.

use std::time::Duration;

use futures::StreamExt as _;

use super::GenesisRegistryBundleResponse;

const MAX_BUNDLE_RESPONSE_BYTES: usize = 128 * 1024 * 1024;
const MAX_ERROR_RESPONSE_BYTES: usize = 64 * 1024;
const BUNDLE_RESPONSE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const BUNDLE_RESPONSE_TOTAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub(super) async fn decode_bundle_response(
    response: reqwest::Response,
    url: &str,
) -> Result<GenesisRegistryBundleResponse, String> {
    let status = response.status();
    let max_bytes = if status.is_success() {
        MAX_BUNDLE_RESPONSE_BYTES
    } else {
        MAX_ERROR_RESPONSE_BYTES
    };
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(format!(
            "Genesis bundle response from {url} exceeds {max_bytes} bytes"
        ));
    }

    let deadline = tokio::time::Instant::now() + BUNDLE_RESPONSE_TOTAL_TIMEOUT;
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(format!("Genesis bundle response from {url} timed out"));
        }
        let wait = BUNDLE_RESPONSE_IDLE_TIMEOUT.min(deadline.saturating_duration_since(now));
        let next = tokio::time::timeout(wait, stream.next())
            .await
            .map_err(|_| format!("Genesis bundle response from {url} stalled"))?;
        let Some(chunk) = next else { break };
        let chunk =
            chunk.map_err(|error| format!("read Genesis bundle response {url}: {error}"))?;
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(format!(
                "Genesis bundle response from {url} exceeds {max_bytes} bytes"
            ));
        }
        body.extend_from_slice(&chunk);
    }

    if !status.is_success() {
        return Err(format!(
            "request Genesis bundle {url} returned {status}: {}",
            String::from_utf8_lossy(&body).trim()
        ));
    }
    serde_json::from_slice(&body).map_err(|error| format!("decode Genesis bundle {url}: {error}"))
}
