//! FoundationDB-style BUGGIFY fault injection.
//!
//! The `buggify!` macro provides probabilistic fault injection at specific
//! code points inside production code. In production builds (`#[cfg(not(test))]`),
//! it compiles to `false` — zero overhead. In test builds, it checks a
//! thread-local seeded RNG for deterministic fault injection.
//!
//! # Usage
//!
//! ```rust,ignore
//! use temper_runtime::buggify::buggify;
//!
//! if buggify!(0.01) {
//!     return Err(PersistenceError::Storage("injected failure".into()));
//! }
//! ```
//!
//! # Determinism
//!
//! The RNG is seeded from the simulation context. Same seed → same faults.
//! Install the context with [`install_buggify_context`] before running tests.

use std::cell::RefCell;

/// Thread-local buggify RNG state.
///
/// When installed (via `install_buggify_context`), `buggify_check` uses this
/// to produce deterministic fault injection decisions.
#[derive(Default)]
struct BuggifyContext {
    enabled: bool,
    rng_state: u64,
}

thread_local! { // determinism-ok: deliberate FoundationDB BUGGIFY pattern
    static BUGGIFY_CTX: RefCell<BuggifyContext> = RefCell::new(BuggifyContext::default());
}

/// RAII guard that disables buggify on drop.
pub struct BuggifyGuard {
    _private: (),
}

impl Drop for BuggifyGuard {
    fn drop(&mut self) {
        BUGGIFY_CTX.with(|ctx| {
            *ctx.borrow_mut() = BuggifyContext::default();
        });
    }
}

/// Install a buggify context with the given seed.
///
/// Returns a guard that resets the context on drop. All `buggify!()` calls
/// on this thread will use the seeded RNG until the guard is dropped.
pub fn install_buggify_context(seed: u64) -> BuggifyGuard {
    BUGGIFY_CTX.with(|ctx| {
        *ctx.borrow_mut() = BuggifyContext {
            enabled: true,
            rng_state: if seed == 0 { 1 } else { seed },
        };
    });
    BuggifyGuard { _private: () }
}

/// Check if a buggify fault should fire at the given probability.
///
/// Returns `false` when buggify is not installed (production default).
/// Returns a deterministic result based on the seeded RNG when installed.
#[inline]
pub fn buggify_check(prob: f64) -> bool {
    if prob <= 0.0 {
        return false;
    }

    BUGGIFY_CTX.with(|ctx| {
        let mut ctx = ctx.borrow_mut();
        if !ctx.enabled {
            return false;
        }

        // xorshift64
        let mut x = ctx.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        ctx.rng_state = x;

        if prob >= 1.0 {
            return true;
        }

        let threshold = (prob * u64::MAX as f64) as u64;
        x < threshold
    })
}

/// Probabilistic fault injection macro.
///
/// In test builds: checks thread-local buggify RNG. Returns `true` with
/// the given probability when a buggify context is installed.
///
/// In non-test builds: compiles to `false`. Zero overhead.
///
/// # Examples
///
/// ```rust,ignore
/// if buggify!(0.01) {
///     // 1% chance of injecting a delay
///     // inject a simulated delay // determinism-ok: doc example only
/// }
/// ```
#[macro_export]
macro_rules! buggify {
    ($prob:expr) => {{
        #[cfg(test)]
        {
            $crate::buggify::buggify_check($prob)
        }
        #[cfg(not(test))]
        {
            // Suppress unused variable warning without evaluating at runtime.
            if false {
                let _ = $prob;
            }
            false
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buggify_disabled_by_default() {
        // No context installed — should always return false.
        assert!(!buggify_check(1.0));
        assert!(!buggify_check(0.5));
    }

    #[test]
    fn buggify_enabled_with_context() {
        let _guard = install_buggify_context(42);

        // With prob 1.0, should always return true.
        assert!(buggify_check(1.0));
    }

    #[test]
    fn buggify_zero_prob_never_fires() {
        let _guard = install_buggify_context(42);
        for _ in 0..1000 {
            assert!(!buggify_check(0.0));
        }
    }

    #[test]
    fn buggify_deterministic_across_seeds() {
        let results1: Vec<bool> = {
            let _guard = install_buggify_context(42);
            (0..100).map(|_| buggify_check(0.5)).collect()
        };

        let results2: Vec<bool> = {
            let _guard = install_buggify_context(42);
            (0..100).map(|_| buggify_check(0.5)).collect()
        };

        assert_eq!(
            results1, results2,
            "Same seed must produce same buggify decisions"
        );
    }

    #[test]
    fn buggify_guard_resets_context() {
        {
            let _guard = install_buggify_context(42);
            assert!(buggify_check(1.0));
        }
        // Guard dropped — should be disabled.
        assert!(!buggify_check(1.0));
    }

    #[test]
    fn buggify_macro_works() {
        let _guard = install_buggify_context(42);

        // The macro should compile and work.
        let _fired = buggify!(0.5);
        // Just verify it compiles — value depends on RNG.
    }

    #[test]
    fn buggify_half_probability_fires_roughly_half() {
        let _guard = install_buggify_context(42);
        let mut fired = 0;
        let total = 10_000;

        for _ in 0..total {
            if buggify_check(0.5) {
                fired += 1;
            }
        }

        // Should be roughly 50% — allow wide margin for xorshift.
        let ratio = fired as f64 / total as f64;
        assert!(
            (0.35..=0.65).contains(&ratio),
            "Expected ~50% fire rate, got {ratio:.2} ({fired}/{total})"
        );
    }
}
