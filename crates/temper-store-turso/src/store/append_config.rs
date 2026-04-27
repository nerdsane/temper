//! Append retry configuration.

use std::time::Duration;

use crate::retry::RETRY_DELAYS_MS;

pub(crate) fn append_attempt_timeout() -> Duration {
    const DEFAULT_APPEND_ATTEMPT_TIMEOUT_MS: u64 = 5_000;
    const MIN_APPEND_ATTEMPT_TIMEOUT_MS: u64 = 500;

    let configured = std::env::var("TEMPER_TURSO_APPEND_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_APPEND_ATTEMPT_TIMEOUT_MS);

    Duration::from_millis(configured.max(MIN_APPEND_ATTEMPT_TIMEOUT_MS))
}

pub(crate) fn append_max_attempts() -> usize {
    const DEFAULT_APPEND_MAX_ATTEMPTS: usize = RETRY_DELAYS_MS.len() + 1;

    std::env::var("TEMPER_TURSO_APPEND_MAX_ATTEMPTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_APPEND_MAX_ATTEMPTS)
        .max(1)
}
