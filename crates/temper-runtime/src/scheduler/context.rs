//! Thread-local simulation context.
//!
//! Provides `sim_now()` and `sim_uuid()` — the two functions that production
//! code calls instead of `chrono::Utc::now()` and `uuid::Uuid::now_v7()`.
//!
//! In production (no context installed), they delegate to real wall-clock
//! and UUIDv7. In simulation, `install_sim_context()` swaps in a
//! [`LogicalClock`] and [`DeterministicIdGen`] for full determinism.
//!
//! The swap uses thread-local storage with an RAII guard, so simulation
//! contexts are always cleaned up — even on panic.

use std::cell::RefCell;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use super::clock::{LogicalClock, SimClock, WallClock};
use super::id_gen::{DeterministicIdGen, RealIdGen, SimIdGen};

/// The simulation context holding clock and ID generator.
struct SimContext {
    clock: Arc<dyn SimClock>,
    id_gen: Arc<dyn SimIdGen>,
}

impl Default for SimContext {
    fn default() -> Self {
        Self {
            clock: Arc::new(WallClock),
            id_gen: Arc::new(RealIdGen),
        }
    }
}

thread_local! { // determinism-ok: deliberate FoundationDB pattern for sim context swap
    static SIM_CONTEXT: RefCell<SimContext> = RefCell::new(SimContext::default());
}

/// Get the current timestamp from the simulation context.
///
/// In production: equivalent to `chrono::Utc::now()`.
/// In simulation: returns logical time from [`LogicalClock`].
pub fn sim_now() -> DateTime<Utc> {
    SIM_CONTEXT.with(|ctx| ctx.borrow().clock.now())
}

/// Generate a UUID from the simulation context.
///
/// In production: equivalent to `uuid::Uuid::now_v7()`.
/// In simulation: returns deterministic UUID from [`DeterministicIdGen`].
pub fn sim_uuid() -> uuid::Uuid {
    SIM_CONTEXT.with(|ctx| ctx.borrow().id_gen.next_uuid())
}

/// RAII guard that restores the default simulation context on drop.
pub struct SimContextGuard {
    _private: (),
}

impl Drop for SimContextGuard {
    fn drop(&mut self) {
        SIM_CONTEXT.with(|ctx| {
            *ctx.borrow_mut() = SimContext::default();
        });
    }
}

/// Install a simulation context on the current thread.
///
/// Returns a guard that restores the default context on drop.
/// All calls to `sim_now()` and `sim_uuid()` on this thread will
/// use the provided clock and ID generator until the guard is dropped.
pub fn install_sim_context(clock: Arc<dyn SimClock>, id_gen: Arc<dyn SimIdGen>) -> SimContextGuard {
    SIM_CONTEXT.with(|ctx| {
        *ctx.borrow_mut() = SimContext { clock, id_gen };
    });
    SimContextGuard { _private: () }
}

/// Install a deterministic simulation context with the given seed.
///
/// Convenience function that creates a [`LogicalClock`] and
/// [`DeterministicIdGen`] from the seed.
pub fn install_deterministic_context(
    seed: u64,
) -> (SimContextGuard, Arc<LogicalClock>, Arc<DeterministicIdGen>) {
    let clock = Arc::new(LogicalClock::new());
    let id_gen = Arc::new(DeterministicIdGen::new(seed));
    let guard = install_sim_context(clock.clone(), id_gen.clone());
    (guard, clock, id_gen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sim_now_defaults_to_wall_clock() {
        let before = Utc::now();
        let t = sim_now();
        let after = Utc::now();
        assert!(t >= before && t <= after);
    }

    #[test]
    fn sim_uuid_defaults_to_v7() {
        let id = sim_uuid();
        assert_eq!(id.get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn installed_context_overrides_defaults() {
        let clock = Arc::new(LogicalClock::new());
        let id_gen = Arc::new(DeterministicIdGen::new(42));

        let _guard = install_sim_context(clock.clone(), id_gen.clone());

        // Clock should be at epoch, not wall time
        let t = sim_now();
        assert_eq!(t, clock.now());

        // UUID should be deterministic
        let id1 = sim_uuid();
        let id_gen2 = DeterministicIdGen::new(42);
        let _ = id_gen2.next_uuid(); // skip one (context already consumed one)
        // Just verify it's not a v7 UUID (deterministic UUIDs have no version)
        assert_ne!(id1.get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn guard_restores_defaults() {
        {
            let clock = Arc::new(LogicalClock::new());
            let id_gen = Arc::new(DeterministicIdGen::new(42));
            let _guard = install_sim_context(clock, id_gen);

            // In simulation context
            let t = sim_now();
            let epoch = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap();
            assert_eq!(t, epoch);
        }
        // Guard dropped — back to defaults
        let before = Utc::now();
        let t = sim_now();
        let after = Utc::now();
        assert!(t >= before && t <= after);
    }

    #[test]
    fn deterministic_context_is_reproducible() {
        let (guard1, clock1, _) = install_deterministic_context(42);
        clock1.advance();
        clock1.advance();
        let t1 = sim_now();
        let id1 = sim_uuid();
        drop(guard1);

        let (guard2, clock2, _) = install_deterministic_context(42);
        clock2.advance();
        clock2.advance();
        let t2 = sim_now();
        let id2 = sim_uuid();
        drop(guard2);

        assert_eq!(t1, t2, "Same seed must produce same timestamps");
        assert_eq!(id1, id2, "Same seed must produce same UUIDs");
    }
}
