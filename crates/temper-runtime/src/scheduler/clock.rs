//! Simulation-aware clock abstraction.
//!
//! Production code uses [`WallClock`] (delegates to `chrono::Utc::now()`).
//! Simulation uses [`LogicalClock`] (deterministic, tick-based).
//! Both implement [`SimClock`] so code that calls `sim_now()` works in
//! either context with zero production cost.

use chrono::{DateTime, TimeZone, Utc};

/// A clock that can be swapped between wall-clock and logical time.
pub trait SimClock: Send + Sync {
    /// Current timestamp.
    fn now(&self) -> DateTime<Utc>;
    /// Current logical tick (wall clock always returns 0).
    fn tick(&self) -> u64;
}

/// Production clock — delegates to `chrono::Utc::now()`.
pub struct WallClock;

impl SimClock for WallClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn tick(&self) -> u64 {
        0
    }
}

/// Deterministic logical clock for simulation.
///
/// Starts at a fixed epoch and advances by `delta` on each `advance()` call.
/// All timestamps are reproducible given the same sequence of advances.
pub struct LogicalClock {
    /// Current tick count (atomic for Sync).
    tick: std::sync::atomic::AtomicU64,
    /// Fixed epoch (simulation start time).
    epoch: DateTime<Utc>,
    /// Duration per tick in milliseconds.
    delta_ms: u64,
}

impl LogicalClock {
    /// Create a logical clock starting at a deterministic epoch.
    pub fn new() -> Self {
        Self {
            tick: std::sync::atomic::AtomicU64::new(0),
            // Fixed epoch: 2024-01-01T00:00:00Z
            epoch: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            delta_ms: 100,
        }
    }

    /// Create a logical clock with a custom delta.
    pub fn with_delta_ms(delta_ms: u64) -> Self {
        Self {
            tick: std::sync::atomic::AtomicU64::new(0),
            epoch: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            delta_ms,
        }
    }

    /// Advance the clock by one tick.
    pub fn advance(&self) {
        self.tick.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Advance the clock by N ticks.
    pub fn advance_by(&self, n: u64) {
        self.tick.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Default for LogicalClock {
    fn default() -> Self {
        Self::new()
    }
}

impl SimClock for LogicalClock {
    fn now(&self) -> DateTime<Utc> {
        let offset_ms = self.tick.load(std::sync::atomic::Ordering::Relaxed) * self.delta_ms;
        self.epoch + chrono::Duration::milliseconds(offset_ms as i64)
    }

    fn tick(&self) -> u64 {
        self.tick.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_clock_returns_current_time() {
        let clock = WallClock;
        let before = Utc::now();
        let t = clock.now();
        let after = Utc::now();
        assert!(t >= before && t <= after);
    }

    #[test]
    fn logical_clock_starts_at_epoch() {
        let clock = LogicalClock::new();
        let expected = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(clock.now(), expected);
        assert_eq!(clock.tick(), 0);
    }

    #[test]
    fn logical_clock_advances_deterministically() {
        let clock = LogicalClock::new();
        clock.advance();
        clock.advance();
        assert_eq!(clock.tick(), 2);

        let expected = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
            + chrono::Duration::milliseconds(200);
        assert_eq!(clock.now(), expected);
    }

    #[test]
    fn logical_clock_is_reproducible() {
        let c1 = LogicalClock::new();
        let c2 = LogicalClock::new();

        for _ in 0..10 {
            c1.advance();
            c2.advance();
        }

        assert_eq!(c1.now(), c2.now());
        assert_eq!(c1.tick(), c2.tick());
    }
}
