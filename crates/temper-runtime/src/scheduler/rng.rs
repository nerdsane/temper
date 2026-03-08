//! Seeded pseudo-random number generator (xorshift64) for deterministic simulation.

/// A seeded pseudo-random number generator (xorshift64).
/// Deterministic, fast, no external dependencies.
#[derive(Debug, Clone)]
pub struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    /// Create a new PRNG with the given seed. A zero seed is replaced with 1.
    pub fn new(seed: u64) -> Self {
        // Ensure non-zero state
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
    }

    /// Generate next pseudo-random u64.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Generate a random number in [0, bound).
    pub fn next_bound(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() as usize) % bound
    }

    /// Return true with probability `p` (0.0 to 1.0).
    pub fn chance(&mut self, p: f64) -> bool {
        let threshold = (p * u64::MAX as f64) as u64;
        self.next_u64() < threshold
    }

    /// Get the current seed state (for logging/replay).
    pub fn seed_state(&self) -> u64 {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic_rng_is_reproducible() {
        let mut rng1 = DeterministicRng::new(42);
        let mut rng2 = DeterministicRng::new(42);

        let seq1: Vec<u64> = (0..10).map(|_| rng1.next_u64()).collect();
        let seq2: Vec<u64> = (0..10).map(|_| rng2.next_u64()).collect();
        assert_eq!(seq1, seq2, "Same seed must produce same sequence");
    }

    #[test]
    fn test_different_seeds_produce_different_sequences() {
        let mut rng1 = DeterministicRng::new(42);
        let mut rng2 = DeterministicRng::new(123);

        let v1 = rng1.next_u64();
        let v2 = rng2.next_u64();
        assert_ne!(v1, v2);
    }
}
