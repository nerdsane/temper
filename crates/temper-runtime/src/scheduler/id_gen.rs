//! Simulation-aware UUID generator.
//!
//! Production code uses [`RealIdGen`] (delegates to `uuid::Uuid::now_v7()`).
//! Simulation uses [`DeterministicIdGen`] (seed + counter → deterministic UUIDs).
//! Both implement [`SimIdGen`] so code that calls `sim_uuid()` works in
//! either context with zero production cost.

/// A UUID generator that can be swapped between real and deterministic.
pub trait SimIdGen: Send + Sync {
    /// Generate the next UUID.
    fn next_uuid(&self) -> uuid::Uuid;
}

/// Production UUID generator — delegates to `uuid::Uuid::now_v7()`.
pub struct RealIdGen;

impl SimIdGen for RealIdGen {
    fn next_uuid(&self) -> uuid::Uuid {
        uuid::Uuid::now_v7()
    }
}

/// Deterministic UUID generator for simulation.
///
/// Generates reproducible UUIDs from a seed and monotonic counter.
/// Given the same seed, the same sequence of UUIDs is produced.
pub struct DeterministicIdGen {
    seed: u64,
    counter: std::sync::atomic::AtomicU64,
}

impl DeterministicIdGen {
    /// Create a deterministic ID generator with the given seed.
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            counter: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl SimIdGen for DeterministicIdGen {
    fn next_uuid(&self) -> uuid::Uuid {
        let count = self.counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Mix seed and counter to produce a deterministic UUID.
        let hi = self.seed ^ (count.wrapping_mul(6364136223846793005));
        let lo = count ^ (self.seed.wrapping_mul(1442695040888963407));
        uuid::Uuid::from_u64_pair(hi, lo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_id_gen_produces_valid_uuids() {
        let id_gen = RealIdGen;
        let id = id_gen.next_uuid();
        assert_eq!(id.get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn deterministic_id_gen_is_reproducible() {
        let g1 = DeterministicIdGen::new(42);
        let g2 = DeterministicIdGen::new(42);

        let ids1: Vec<uuid::Uuid> = (0..10).map(|_| g1.next_uuid()).collect();
        let ids2: Vec<uuid::Uuid> = (0..10).map(|_| g2.next_uuid()).collect();

        assert_eq!(ids1, ids2, "Same seed must produce same UUID sequence");
    }

    #[test]
    fn deterministic_id_gen_different_seeds_diverge() {
        let g1 = DeterministicIdGen::new(42);
        let g2 = DeterministicIdGen::new(99);

        assert_ne!(g1.next_uuid(), g2.next_uuid());
    }

    #[test]
    fn deterministic_ids_are_unique() {
        let id_gen = DeterministicIdGen::new(42);
        let ids: Vec<uuid::Uuid> = (0..100).map(|_| id_gen.next_uuid()).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), unique.len(), "All generated UUIDs should be unique");
    }
}
