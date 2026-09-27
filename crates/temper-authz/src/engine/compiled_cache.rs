//! Bounded reuse of pure Cedar compilation, never of tenant activation or decisions.
//!
//! Recovery still reads durable policy rows before calling the engine. An exact
//! source match can reuse an immutable policy set and candidate index; changed
//! source, named policy IDs, and per-engine activation remain independent.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use cedar_policy::PolicySet;

use super::{CompiledPolicies, merge_system_platform_policy};
use crate::error::AuthzError;

const ENTRY_BUDGET: usize = 64;
const TOTAL_SOURCE_BYTE_BUDGET: usize = 1024 * 1024;
const MAX_CACHED_SOURCE_BYTES: usize = 64 * 1024;

struct CompiledPolicyCache {
    entries: BTreeMap<Arc<str>, Arc<CompiledPolicies>>,
    insertion_order: VecDeque<Arc<str>>,
    source_bytes: usize,
    entry_budget: usize,
    source_byte_budget: usize,
}

impl CompiledPolicyCache {
    fn new(entry_budget: usize, source_byte_budget: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            insertion_order: VecDeque::new(),
            source_bytes: 0,
            entry_budget,
            source_byte_budget,
        }
    }

    fn get(&self, source: &str) -> Option<Arc<CompiledPolicies>> {
        self.entries.get(source).map(Arc::clone)
    }

    fn insert(&mut self, source: &str, policies: Arc<CompiledPolicies>) -> Arc<CompiledPolicies> {
        // A concurrent miss may already have compiled and inserted this source.
        if let Some(existing) = self.get(source) {
            return existing;
        }
        if self.entry_budget == 0
            || source.len() > MAX_CACHED_SOURCE_BYTES
            || source.len() > self.source_byte_budget
        {
            return policies;
        }

        // FIFO uses neither wall time nor request/tenant state. Evicting a cache
        // reference cannot alter any active engine's immutable Arc snapshot.
        while self.entries.len() >= self.entry_budget
            || source.len() > self.source_byte_budget - self.source_bytes
        {
            let Some(oldest) = self.insertion_order.pop_front() else {
                return policies;
            };
            self.entries.remove(oldest.as_ref());
            self.source_bytes -= oldest.len();
        }

        let source: Arc<str> = Arc::from(source);
        self.source_bytes += source.len();
        self.insertion_order.push_back(Arc::clone(&source));
        self.entries.insert(source, Arc::clone(&policies));
        policies
    }
}

pub(super) fn compiled_raw_policies(source: &str) -> Result<Arc<CompiledPolicies>, AuthzError> {
    static CACHE: OnceLock<Mutex<CompiledPolicyCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        Mutex::new(CompiledPolicyCache::new(
            ENTRY_BUDGET,
            TOTAL_SOURCE_BYTE_BUDGET,
        ))
    });
    compile_with_cache(source, cache)
}

fn compile_with_cache(
    source: &str,
    cache: &Mutex<CompiledPolicyCache>,
) -> Result<Arc<CompiledPolicies>, AuthzError> {
    if let Some(policies) = cache.lock().ok().and_then(|cache| cache.get(source)) {
        return Ok(policies);
    }

    // Parsing and indexing happen outside the cache lock. Failed parses are not
    // cached, and cache poisoning only disables this optional acceleration.
    let mut policy_set = source
        .parse::<PolicySet>()
        .map_err(|error| AuthzError::PolicyParse(error.to_string()))?;
    merge_system_platform_policy(&mut policy_set);
    let policies = Arc::new(CompiledPolicies::new(policy_set));
    Ok(match cache.lock() {
        Ok(mut cache) => cache.insert(source, policies),
        Err(_) => policies,
    })
}

#[cfg(test)]
mod tests;
