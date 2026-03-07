//! Governance decision polling.
//!
//! Shared logic for polling pending Cedar governance decisions until a
//! terminal status (approved, denied, or timeout). Used by both the
//! MCP sandbox dispatch and the agent-runtime authorization flow.

use std::time::Duration;

use serde_json::Value;

/// Outcome of a governance decision poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionOutcome {
    /// Decision was approved.
    Approved,
    /// Decision was denied or rejected.
    Denied,
    /// Decision is still pending.
    Pending,
}

/// Classify a decision status string into a canonical outcome.
///
/// Case-insensitive matching of common status values.
pub fn classify_decision_status(status: &str) -> DecisionOutcome {
    let lower = status.to_ascii_lowercase();
    match lower.as_str() {
        "approved" => DecisionOutcome::Approved,
        "denied" | "rejected" => DecisionOutcome::Denied,
        "pending" => DecisionOutcome::Pending,
        // Unknown statuses are treated as terminal (not pending).
        _ => DecisionOutcome::Denied,
    }
}

/// Configuration for decision polling.
pub struct PollConfig {
    /// Maximum time to wait before declaring timeout.
    pub timeout: Duration,
    /// Sleep interval between poll attempts.
    pub interval: Duration,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            interval: Duration::from_millis(2000),
        }
    }
}

/// Find a decision by ID in a JSON response body.
///
/// Handles both `{"decisions": [...]}` and bare array `[...]` formats.
pub fn find_decision_in_response(body: &Value, decision_id: &str) -> Option<(Value, String)> {
    let decisions = body
        .get("decisions")
        .and_then(Value::as_array)
        .or_else(|| body.as_array());

    let decisions = decisions?;
    for d in decisions {
        if d.get("id").and_then(Value::as_str) == Some(decision_id) {
            let status = d
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return Some((d.clone(), status));
        }
    }
    None
}

/// Poll a governance decision until it reaches a terminal status.
///
/// Returns the full decision object on success, or an error string on
/// timeout or network failure.
///
/// The `fetch_decisions` callback should query the decisions endpoint
/// and return the parsed JSON body. This keeps HTTP details out of
/// the polling logic.
pub async fn poll_decision<F, Fut>(
    decision_id: &str,
    config: &PollConfig,
    fetch_decisions: F,
) -> Result<(Value, DecisionOutcome), String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    let start = std::time::Instant::now(); // determinism-ok: wall-clock for timeout only
    loop {
        let body = fetch_decisions().await?;

        if let Some((decision, status)) = find_decision_in_response(&body, decision_id) {
            let outcome = classify_decision_status(&status);
            if outcome != DecisionOutcome::Pending {
                return Ok((decision, outcome));
            }
        }

        if start.elapsed() > config.timeout {
            return Err(format!(
                "Decision {decision_id} still pending after {:.0}s. \
                 Ask the human to approve via the Observe UI or `temper decide` CLI, then retry.",
                config.timeout.as_secs_f64(),
            ));
        }

        tokio::time::sleep(config.interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classify_status_values() {
        assert_eq!(
            classify_decision_status("Approved"),
            DecisionOutcome::Approved
        );
        assert_eq!(
            classify_decision_status("approved"),
            DecisionOutcome::Approved
        );
        assert_eq!(
            classify_decision_status("Denied"),
            DecisionOutcome::Denied
        );
        assert_eq!(
            classify_decision_status("denied"),
            DecisionOutcome::Denied
        );
        assert_eq!(
            classify_decision_status("Rejected"),
            DecisionOutcome::Denied
        );
        assert_eq!(
            classify_decision_status("rejected"),
            DecisionOutcome::Denied
        );
        assert_eq!(
            classify_decision_status("pending"),
            DecisionOutcome::Pending
        );
        assert_eq!(
            classify_decision_status("Pending"),
            DecisionOutcome::Pending
        );
    }

    #[test]
    fn find_decision_in_wrapper_format() {
        let body = json!({
            "decisions": [
                {"id": "d-1", "status": "approved"},
                {"id": "d-2", "status": "pending"},
            ]
        });
        let (d, status) = find_decision_in_response(&body, "d-1").unwrap();
        assert_eq!(status, "approved");
        assert_eq!(d.get("id").unwrap(), "d-1");
    }

    #[test]
    fn find_decision_in_bare_array() {
        let body = json!([
            {"id": "d-1", "status": "denied"},
        ]);
        let (_, status) = find_decision_in_response(&body, "d-1").unwrap();
        assert_eq!(status, "denied");
    }

    #[test]
    fn find_decision_not_found() {
        let body = json!({"decisions": []});
        assert!(find_decision_in_response(&body, "d-missing").is_none());
    }
}
