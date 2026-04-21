//! Exponential-backoff retry for transient Turso/libSQL write errors.
//!
//! Motivated by the 2026-04-20 Turso free-tier write-block incident
//! (see ADR-0056). When Hrana returns `BLOCKED` / stream-error / timeout,
//! the caller's action dispatch fails and the entity can be left in an
//! ambiguous intermediate state. Retrying the write with backoff gives
//! Turso time to recover (or the operator time to unblock the quota)
//! before propagating the error.
//!
//! Event sourcing is already idempotent per `(entity_type, entity_id,
//! sequence_nr)` via a UNIQUE constraint, so retrying a write that
//! actually succeeded but whose ACK was lost is safe: the second attempt
//! returns `ConcurrencyViolation` which the caller handles via the
//! normal event-store contract.

use std::time::Duration;

use temper_runtime::persistence::PersistenceError;

use crate::metrics::record_turso_write_retry;

/// Backoff delays between attempts in milliseconds. The first attempt
/// has no preceding delay; subsequent attempts use these values in
/// order. Total worst-case wait before exhaustion: 3.75 s across 4
/// retries, which is below typical HTTP/gRPC client timeouts and above
/// typical Turso transient-error recovery windows.
pub(crate) const RETRY_DELAYS_MS: &[u64] = &[250, 500, 1000, 2000];

/// Returns true if `err_msg` looks like a transient Turso/libSQL failure
/// that retry with backoff can reasonably recover from.
///
/// Matched patterns:
/// - Hrana `BLOCKED` code (free-tier write quota, operator quota hold,
///   maintenance pause). The actual wire error shape from libsql is:
///   `Hrana: stream error: Error { message: "Operation was blocked: …",
///   code: "BLOCKED" }`.
/// - Generic `stream error` from Hrana without an explicit code —
///   usually transient socket/TLS issues.
/// - Connection reset / broken pipe / timeout.
///
/// Non-transient (returns false): SQL syntax errors, UNIQUE constraint
/// violations (caller-semantic, not retry-fixable), permission denials.
pub(crate) fn is_transient_write_error(err_msg: &str) -> bool {
    let lower = err_msg.to_ascii_lowercase();
    if lower.contains("unique constraint") {
        // Caller-semantic duplicate; the outer event-store contract
        // translates this to ConcurrencyViolation. Never retry.
        return false;
    }
    lower.contains("blocked")
        || lower.contains("operation was blocked")
        || lower.contains("stream error")
        || lower.contains("connection reset")
        || lower.contains("broken pipe")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("try again")
}

/// Extract the stringified cause from a `PersistenceError` for transient
/// detection. Non-`Storage` variants (Serialization, ConcurrencyViolation,
/// etc.) are never transient.
fn error_text(err: &PersistenceError) -> Option<&str> {
    match err {
        PersistenceError::Storage(msg) => Some(msg.as_str()),
        _ => None,
    }
}

/// Retry a persistence operation with exponential backoff on transient
/// errors. `operation` is a static label used for the
/// `temper_turso_write_retries_total` metric.
///
/// Behaviour:
/// - Runs `f` up to `RETRY_DELAYS_MS.len() + 1` times total (1 attempt
///   + N retries).
/// - On transient error, sleeps the next delay and retries.
/// - On non-transient error, propagates immediately.
/// - On success after any retries, records `outcome=succeeded` with the
///   attempt number at which it succeeded.
/// - On exhaustion, records `outcome=exhausted` and returns the last
///   transient error.
///
/// Not intended for reads — those are typically idempotent but also
/// rarely benefit from retry in the same way (they're not the root of
/// entity-state correctness). Scoped to write paths explicitly.
pub(crate) async fn retry_persistence<F, Fut, T>(
    operation: &'static str,
    mut f: F,
) -> Result<T, PersistenceError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, PersistenceError>>,
{
    let total_attempts = RETRY_DELAYS_MS.len() + 1;
    let mut last_err: Option<PersistenceError> = None;

    for attempt in 0..total_attempts {
        if attempt > 0 {
            let delay_ms = RETRY_DELAYS_MS[attempt - 1];
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        match f().await {
            Ok(v) => {
                if attempt > 0 {
                    record_turso_write_retry(operation, attempt as u64, "succeeded");
                }
                return Ok(v);
            }
            Err(err) => {
                let is_transient = error_text(&err)
                    .map(is_transient_write_error)
                    .unwrap_or(false);
                if !is_transient {
                    return Err(err);
                }
                last_err = Some(err);
            }
        }
    }

    record_turso_write_retry(operation, total_attempts as u64, "exhausted");
    Err(last_err.expect("retry loop must have captured at least one error before exhaustion"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn is_transient_matches_hrana_blocked() {
        assert!(is_transient_write_error(
            "Hrana: stream error: Error { message: \"Operation was blocked: SQL write operations are forbidden\", code: \"BLOCKED\" }"
        ));
    }

    #[test]
    fn is_transient_matches_stream_error() {
        assert!(is_transient_write_error(
            "Hrana: stream error: connection reset by peer"
        ));
    }

    #[test]
    fn is_transient_matches_timeout() {
        assert!(is_transient_write_error("operation timed out after 10s"));
    }

    #[test]
    fn is_transient_rejects_unique_constraint() {
        // Caller-semantic — retrying would produce the same error.
        assert!(!is_transient_write_error(
            "UNIQUE constraint failed: events.sequence_nr"
        ));
    }

    #[test]
    fn is_transient_rejects_syntax_error() {
        assert!(!is_transient_write_error(
            "near \"SELET\": syntax error"
        ));
    }

    #[tokio::test]
    async fn retry_succeeds_on_first_attempt_without_recording_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let result: Result<i32, PersistenceError> = retry_persistence("test.op", move || {
            let calls = calls_clone.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(42)
            }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_succeeds_on_third_attempt_after_two_transient_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let result: Result<&'static str, PersistenceError> =
            retry_persistence("test.op", move || {
                let calls = calls_clone.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        Err(PersistenceError::Storage(
                            "stream error: Operation was blocked".into(),
                        ))
                    } else {
                        Ok("ok")
                    }
                }
            })
            .await;
        assert_eq!(result.unwrap(), "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_propagates_non_transient_errors_immediately() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let result: Result<i32, PersistenceError> = retry_persistence("test.op", move || {
            let calls = calls_clone.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(PersistenceError::Storage(
                    "UNIQUE constraint failed: events.sequence_nr".into(),
                ))
            }
        })
        .await;
        assert!(matches!(result, Err(PersistenceError::Storage(_))));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "non-transient errors must not retry"
        );
    }

    #[tokio::test]
    async fn retry_exhausts_after_all_delays_on_persistent_transient_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let result: Result<i32, PersistenceError> = retry_persistence("test.op", move || {
            let calls = calls_clone.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(PersistenceError::Storage(
                    "Hrana: BLOCKED — quota exceeded".into(),
                ))
            }
        })
        .await;
        assert!(result.is_err());
        // 1 initial attempt + 4 retries = 5 total.
        assert_eq!(calls.load(Ordering::SeqCst), RETRY_DELAYS_MS.len() + 1);
    }

    #[tokio::test]
    async fn retry_propagates_non_storage_persistence_errors() {
        // Serialization errors are never transient.
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let result: Result<i32, PersistenceError> = retry_persistence("test.op", move || {
            let calls = calls_clone.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(PersistenceError::Serialization("bad json".into()))
            }
        })
        .await;
        assert!(matches!(result, Err(PersistenceError::Serialization(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
