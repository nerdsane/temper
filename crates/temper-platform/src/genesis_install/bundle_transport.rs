//! Bounded transport decoding for remote Genesis bundle responses.

use std::time::Duration;

use futures::StreamExt as _;

use super::GenesisRegistryBundleResponse;
use super::bundles::{
    MAX_GENESIS_BUNDLE_APPS, MAX_GENESIS_BUNDLE_FILES, MAX_GENESIS_BUNDLE_TOTAL_BYTES,
};

/// Not a limit of its own: the install budget in the form it travels in, so
/// every bundle `validate_registry_bundle` would accept also arrives. It still
/// bounds what a broken or hostile registry can make the installer buffer.
const MAX_BUNDLE_RESPONSE_BYTES: usize = bundle_response_allowance(
    MAX_GENESIS_BUNDLE_TOTAL_BYTES,
    MAX_GENESIS_BUNDLE_FILES,
    MAX_GENESIS_BUNDLE_APPS,
);
const MAX_ERROR_RESPONSE_BYTES: usize = 64 * 1024;
const BUNDLE_RESPONSE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const BUNDLE_RESPONSE_TOTAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The most bytes a JSON bundle response can take to carry `content_bytes` of
/// file content in `files` files across `apps` apps:
///
/// - content travels as base64, 4 bytes for every 3, which is also how
///   `validate_registry_bundle` counts it against the budget;
/// - each file adds its JSON keys and quotes, and its path. A path the
///   installer can write is shorter than Linux's PATH_MAX (4096 bytes), and
///   JSON can escape one byte to six (`\u001f`);
/// - each app adds its keys, an owner and a name of at most 128 bytes each,
///   and a 40-character hash;
/// - the envelope holds the root app ref and the registry tenant's id.
const fn bundle_response_allowance(content_bytes: u64, files: usize, apps: usize) -> usize {
    const PATH_MAX: usize = 4096;
    const FILE_JSON_BYTES: usize = 64 + 6 * PATH_MAX;
    const APP_JSON_BYTES: usize = 512;
    const ENVELOPE_JSON_BYTES: usize = 64 * 1024;
    content_bytes.div_ceil(3) as usize * 4
        + files * FILE_JSON_BYTES
        + apps * APP_JSON_BYTES
        + ENVELOPE_JSON_BYTES
}

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

#[cfg(test)]
#[path = "bundle_transport_test.rs"]
mod tests;
