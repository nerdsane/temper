//! Shared webhook HTTP delivery core.
//!
//! Provides a single delivery function used by both the integration engine
//! (retry + DLQ) and the trajectory webhook dispatcher (fire-and-forget).
//! This ensures consistent request construction, header handling, and
//! response classification across all webhook delivery paths.

use std::collections::BTreeMap;
use std::time::Duration;

/// Outcome of a single webhook delivery attempt.
#[derive(Debug)]
pub enum DeliveryOutcome {
    /// The remote returned a 2xx status.
    Success(u16),
    /// The remote returned a non-2xx status.
    HttpError(u16),
    /// A transport-level error (DNS, timeout, connection refused, etc.).
    TransportError(String),
}

impl DeliveryOutcome {
    /// Returns `true` if the delivery succeeded (2xx).
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success(_))
    }
}

/// Deliver a single webhook HTTP request.
///
/// This is the **single source of truth** for constructing and sending
/// webhook requests. Callers layer their own policies (retry, DLQ,
/// fire-and-forget) on top.
pub async fn deliver_webhook(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    headers: &BTreeMap<String, String>,
    body: &str,
    timeout: Duration,
) -> DeliveryOutcome {
    let mut builder = match method.to_uppercase().as_str() {
        "PUT" => client.put(url),
        _ => client.post(url),
    };

    builder = builder
        .timeout(timeout)
        .header("Content-Type", "application/json")
        .body(body.to_owned());

    for (key, value) in headers {
        builder = builder.header(key.as_str(), value.as_str());
    }

    match builder.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if resp.status().is_success() {
                DeliveryOutcome::Success(status)
            } else {
                DeliveryOutcome::HttpError(status)
            }
        }
        Err(e) => DeliveryOutcome::TransportError(e.to_string()),
    }
}

/// Default webhook timeout (10 seconds).
pub const DEFAULT_WEBHOOK_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_outcome_success_classification() {
        assert!(DeliveryOutcome::Success(200).is_success());
        assert!(!DeliveryOutcome::HttpError(500).is_success());
        assert!(!DeliveryOutcome::TransportError("timeout".into()).is_success());
    }
}
