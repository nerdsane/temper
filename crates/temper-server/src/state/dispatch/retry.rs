//! Retry-with-backoff wrapper around `ActorRef::ask`.
//!
//! Provides the dispatch layer with uniform handling of transient actor
//! failures (`AskTimeout`, `MailboxFull`) so call sites do not need to
//! re-implement retry logic. See ADR-0048.
//!
//! ## Policy shape
//!
//! - **Per-attempt timeout** (`per_attempt_timeout`) — the `ActorRef::ask`
//!   budget for one try. Default 5s, matches `TEMPER_ACTION_TIMEOUT_SECS`.
//! - **Total deadline** (`total_deadline`) — wall-clock budget across all
//!   attempts including backoff sleeps. Default 30s.
//! - **Max attempts** (`max_attempts`) — hard cap on attempts regardless of
//!   remaining budget. Default 3.
//! - **Base delays** (`base_delays_ms`) — exponential backoff base per
//!   attempt index (attempt 0 is always instant). Default `[0, 50, 200, 800]`.
//!
//! ## Jitter
//!
//! Full jitter (AWS "Exponential Backoff And Jitter") — actual sleep is
//! `rand(0, base_delays_ms[attempt])`. Randomness is seeded deterministically
//! from `sim_now()` so simulation tests stay reproducible.
//!
//! ## Classification
//!
//! Permanent `ActorError` variants short-circuit immediately — no further
//! attempts. Transient variants consume an attempt and back off.

use std::time::{Duration, Instant};

use temper_runtime::actor::{ActorError, ActorRef, Message};
use temper_runtime::scheduler::sim_now;

/// Retry policy for `ask_with_backoff`.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Wall-clock budget across all attempts including backoff sleeps.
    pub total_deadline: Duration,
    /// Per-attempt `ActorRef::ask` timeout.
    pub per_attempt_timeout: Duration,
    /// Maximum attempts; always `>= 1`.
    pub max_attempts: u32,
    /// Base delay per attempt index before jitter (index 0 is always 0).
    /// The last entry is used for all higher attempt indices.
    pub base_delays_ms: Vec<u64>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            total_deadline: duration_secs_env("TEMPER_DISPATCH_TOTAL_TIMEOUT_SECS", 30),
            per_attempt_timeout: duration_secs_env("TEMPER_ACTION_TIMEOUT_SECS", 5),
            max_attempts: u32_env("TEMPER_DISPATCH_RETRY_MAX_ATTEMPTS", 3).max(1),
            base_delays_ms: vec![0, 50, 200, 800],
        }
    }
}

impl RetryPolicy {
    /// Disable retries entirely — equivalent to a plain `ActorRef::ask`
    /// call with the configured per-attempt timeout.
    #[allow(dead_code)] // kept as a documented escape hatch for tests/tools
    pub fn single_attempt() -> Self {
        Self {
            max_attempts: 1,
            ..Self::default()
        }
    }

    /// Pick the base backoff in ms for a given attempt index. Attempt 0 is
    /// always 0; later attempts use `base_delays_ms[min(attempt, len-1)]`.
    pub(crate) fn base_delay_ms_for(&self, attempt: u32) -> u64 {
        if attempt == 0 || self.base_delays_ms.is_empty() {
            return 0;
        }
        let max_idx = self.base_delays_ms.len() - 1;
        let idx = (attempt as usize).min(max_idx);
        self.base_delays_ms[idx]
    }

    /// Remaining wall-clock budget before the total deadline expires.
    pub(crate) fn remaining(&self, elapsed: Duration) -> Duration {
        self.total_deadline.saturating_sub(elapsed)
    }

    /// Return a copy whose total deadline can accommodate every configured
    /// attempt at `per_attempt_timeout` plus the configured backoff bases.
    pub(crate) fn with_attempt_budget_floor(mut self) -> Self {
        let attempts = self.max_attempts.max(1);
        let attempts_budget = self
            .per_attempt_timeout
            .checked_mul(attempts)
            .unwrap_or(Duration::MAX);
        let backoff_budget = (1..attempts).fold(Duration::ZERO, |acc, attempt| {
            acc.saturating_add(Duration::from_millis(self.base_delay_ms_for(attempt)))
        });
        let minimum_total = attempts_budget.saturating_add(backoff_budget);
        if self.total_deadline < minimum_total {
            self.total_deadline = minimum_total;
        }
        self
    }
}

/// Outcome of `ask_with_backoff`.
#[derive(Debug)]
pub struct AskResult<R> {
    /// Final result — `Ok` on any successful attempt; last-seen `ActorError`
    /// otherwise.
    pub result: Result<R, ActorError>,
    /// Number of attempts made (1..=policy.max_attempts).
    pub attempts: u32,
    /// Total wall-clock time spent across attempts + sleeps.
    pub elapsed: Duration,
    /// `true` if at least one attempt succeeded after a prior transient
    /// failure. Informational (metric-friendly).
    pub retried_after_transient: bool,
}

/// Ask an actor with bounded retry on transient failures.
///
/// `make_msg` is called once per attempt so each attempt builds a fresh
/// `M` — required because `ActorRef::ask` moves its message.
///
/// Returns as soon as any of:
/// - an attempt succeeds,
/// - an attempt returns a permanent `ActorError`,
/// - `policy.max_attempts` is reached, or
/// - `policy.total_deadline` has elapsed.
pub async fn ask_with_backoff<M, R, F>(
    actor_ref: &ActorRef<M>,
    mut make_msg: F,
    policy: &RetryPolicy,
) -> AskResult<R>
where
    M: Message,
    R: Send + 'static,
    F: FnMut() -> M,
{
    let start = Instant::now(); // determinism-ok: production actor deadline and backoff only
    let max_attempts = policy.max_attempts.max(1);
    let mut last_err: Option<ActorError> = None;
    let mut saw_transient = false;

    for attempt in 0..max_attempts {
        // Pre-attempt backoff (skipped on attempt 0).
        if attempt > 0 {
            let remaining = policy.remaining(start.elapsed());
            if remaining.is_zero() {
                break;
            }
            let base = policy.base_delay_ms_for(attempt);
            let jittered_ms = jittered_delay_ms(base, attempt);
            let sleep_dur = Duration::from_millis(jittered_ms).min(remaining);
            if !sleep_dur.is_zero() {
                tokio::time::sleep(sleep_dur).await;
            }
        }

        // Budget-aware per-attempt timeout.
        let remaining = policy.remaining(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        let attempt_timeout = policy.per_attempt_timeout.min(remaining);
        if attempt_timeout.is_zero() {
            break;
        }

        let msg = make_msg();
        match actor_ref.ask::<R>(msg, attempt_timeout).await {
            Ok(value) => {
                return AskResult {
                    result: Ok(value),
                    attempts: attempt + 1,
                    elapsed: start.elapsed(),
                    retried_after_transient: saw_transient,
                };
            }
            Err(err) if err.is_permanent() => {
                return AskResult {
                    result: Err(err),
                    attempts: attempt + 1,
                    elapsed: start.elapsed(),
                    retried_after_transient: saw_transient,
                };
            }
            Err(err) => {
                saw_transient = true;
                last_err = Some(err);
            }
        }
    }

    AskResult {
        result: Err(last_err.unwrap_or_else(|| {
            ActorError::custom("retry policy exhausted without making any attempt")
        })),
        attempts: max_attempts,
        elapsed: start.elapsed(),
        retried_after_transient: saw_transient,
    }
}

/// Deterministic "full jitter" in the range `[0, base_ms]`.
///
/// Randomness is derived from `sim_now()` mixed with the attempt index so the
/// same logical retry attempted at a different time produces a different
/// value — yet simulations replay identically because `sim_now()` is
/// deterministic under DST.
// determinism-ok: derived from sim_now() only.
fn jittered_delay_ms(base_ms: u64, attempt: u32) -> u64 {
    if base_ms == 0 {
        return 0;
    }
    let now_nanos = sim_now().timestamp_nanos_opt().unwrap_or(0) as u128;
    let attempt_mix = (attempt as u128).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mixed = now_nanos
        .wrapping_add(attempt_mix)
        .wrapping_mul(0xBF58_476D_1CE4_E5B9)
        ^ (now_nanos >> 27);
    // Full jitter: uniform in [0, base_ms].
    let range = base_ms as u128 + 1;
    (mixed % range) as u64
}

fn duration_secs_env(name: &str, default_secs: u64) -> Duration {
    let secs = std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0 && *v <= 3600)
        .unwrap_or(default_secs);
    Duration::from_secs(secs)
}

fn u32_env(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v > 0 && *v <= 32)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_with_attempts(max_attempts: u32, delays: Vec<u64>) -> RetryPolicy {
        RetryPolicy {
            total_deadline: Duration::from_secs(30),
            per_attempt_timeout: Duration::from_secs(5),
            max_attempts,
            base_delays_ms: delays,
        }
    }

    #[test]
    fn base_delay_zero_for_first_attempt() {
        let p = policy_with_attempts(4, vec![0, 50, 200, 800]);
        assert_eq!(p.base_delay_ms_for(0), 0);
    }

    #[test]
    fn base_delay_follows_index() {
        let p = policy_with_attempts(4, vec![0, 50, 200, 800]);
        assert_eq!(p.base_delay_ms_for(1), 50);
        assert_eq!(p.base_delay_ms_for(2), 200);
        assert_eq!(p.base_delay_ms_for(3), 800);
    }

    #[test]
    fn base_delay_clamps_to_last_entry() {
        let p = policy_with_attempts(10, vec![0, 50, 200, 800]);
        // Past the end of the table — stays at the last defined value.
        assert_eq!(p.base_delay_ms_for(7), 800);
        assert_eq!(p.base_delay_ms_for(100), 800);
    }

    #[test]
    fn base_delay_empty_table_always_zero() {
        let p = policy_with_attempts(3, vec![]);
        assert_eq!(p.base_delay_ms_for(1), 0);
        assert_eq!(p.base_delay_ms_for(10), 0);
    }

    #[test]
    fn remaining_budget_never_goes_negative() {
        let p = policy_with_attempts(3, vec![0, 50]);
        let long = Duration::from_secs(60);
        assert_eq!(p.remaining(long), Duration::ZERO);
    }

    #[test]
    fn jitter_stays_within_base_bound() {
        // Property check: over many (base, attempt) combos, full-jitter must
        // never exceed the base.
        for base in [0u64, 1, 10, 50, 200, 800, 5000] {
            for attempt in 0u32..16 {
                let d = jittered_delay_ms(base, attempt);
                assert!(
                    d <= base,
                    "jitter {d} exceeded base {base} at attempt {attempt}"
                );
            }
        }
    }

    #[test]
    fn single_attempt_policy_has_one_attempt_and_zero_delays() {
        let p = RetryPolicy::single_attempt();
        assert_eq!(p.max_attempts, 1);
        // Base delay at attempt 0 is always 0 regardless of table contents.
        assert_eq!(p.base_delay_ms_for(0), 0);
    }

    #[test]
    fn max_attempts_coerced_to_at_least_one() {
        // Default() coerces invalid env values upwards; exposed via the
        // `.max(1)` in Default::default(). A direct construction with 0 is
        // allowed so tests can probe edge cases.
        let p = RetryPolicy {
            max_attempts: 0,
            ..RetryPolicy::default()
        };
        // ask_with_backoff internally coerces max_attempts to >= 1 so the
        // loop always runs at least once; this is the contract the dispatch
        // layer depends on.
        let effective = p.max_attempts.max(1);
        assert_eq!(effective, 1);
    }

    #[test]
    fn attempt_budget_floor_prevents_per_attempt_timeout_from_disabling_retry() {
        let p = RetryPolicy {
            total_deadline: Duration::from_secs(30),
            per_attempt_timeout: Duration::from_secs(30),
            max_attempts: 3,
            base_delays_ms: vec![0, 50, 200],
        }
        .with_attempt_budget_floor();

        assert_eq!(p.total_deadline, Duration::from_millis(90_250));
    }
}
