//! Whole-cache consistency belongs in tests, not in each write's hot path.

use temper_runtime::scheduler::{DeterministicRng, install_deterministic_context};

use super::*;

fn assert_consistent(cache: &IdempotencyCache, seed: u64, step: usize) {
    let entries = cache.entries.read().unwrap();
    assert_eq!(
        entries.len(),
        entries.by_actor.values().map(BTreeMap::len).sum::<usize>(),
        "seed {seed}, step {step}: each reply has one eviction entry"
    );
    let mut bytes = 0;
    for (actor, replies) in &entries.by_actor {
        assert!(!replies.is_empty(), "seed {seed}, step {step}: empty actor");
        for (key, entry) in replies {
            bytes += entry.bytes;
            let (order, other) = if entry.effects_applied {
                (&entries.applied, &entries.pending)
            } else {
                (&entries.pending, &entries.applied)
            };
            assert_eq!(
                order.get(&entry.seq),
                Some(&(actor.clone(), key.clone())),
                "seed {seed}, step {step}: reply in the correct eviction order"
            );
            assert!(!other.contains_key(&entry.seq));
        }
    }
    assert_eq!(entries.bytes, bytes, "seed {seed}, step {step}: held bytes");
    assert!(bytes <= cache.budget_bytes);
}

#[test]
fn dst_cache_indexes_agree_after_replacement_eviction_and_expiry() {
    for seed in 1..=32 {
        let (_guard, clock, _) = install_deterministic_context(seed);
        let mut rng = DeterministicRng::new(seed);
        let response = make_response("S");
        let one = estimated_entry_bytes("actor-0", "key-0", &response);
        let cache = IdempotencyCache::with_budget_bytes(one * 8);
        for step in 0..128 {
            let actor = format!("actor-{}", rng.next_bound(6));
            let key = format!("key-{}", rng.next_bound(8));
            if step % 31 == 30 {
                clock.advance_by((IDEMPOTENCY_TTL_SECS as u64 + 1) * 10);
            }
            if rng.chance(0.5) {
                cache.put(&actor, &key, &req(), &response);
            } else {
                cache.put_effects_applied(&actor, &key, &req(), &response);
            }
            assert_consistent(&cache, seed, step);
        }
        // Expire both applied and pending replies, then insert one fresh reply.
        clock.advance_by((IDEMPOTENCY_TTL_SECS as u64 + 1) * 10);
        cache.put("actor-0", "key-0", &req(), &response);
        assert_consistent(&cache, seed, 128);
        assert_eq!(cache.entries.read().unwrap().len(), 1, "seed {seed}");
    }
}
