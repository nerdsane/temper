//! Bounded reuse of pure source preparation, never of liveness decisions.
//!
//! Callers still read their source and enforcement mode before parsing. Only
//! exact source bytes reuse an immutable AST; diagnostics run on every call and
//! each caller receives an independent owned automaton.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use super::parser::{
    AutomatonParseError, LivenessEnforcement, LivenessViolationReporter, check_liveness_coverage,
    liveness_reporter, prepare_automaton,
};
use super::types::Automaton;

const ENTRY_BUDGET: usize = 128;
const TOTAL_SOURCE_BYTE_BUDGET: usize = 2 * 1024 * 1024;
const MAX_CACHED_SOURCE_BYTES: usize = 128 * 1024;

struct PreparedAutomatonCache {
    entries: BTreeMap<Arc<str>, Arc<Automaton>>,
    insertion_order: VecDeque<Arc<str>>,
    source_bytes: usize,
    entry_budget: usize,
    source_byte_budget: usize,
}

impl PreparedAutomatonCache {
    fn new(entry_budget: usize, source_byte_budget: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            insertion_order: VecDeque::new(),
            source_bytes: 0,
            entry_budget,
            source_byte_budget,
        }
    }

    fn get(&self, source: &str) -> Option<Arc<Automaton>> {
        self.entries.get(source).map(Arc::clone)
    }

    fn insert(&mut self, source: &str, automaton: Arc<Automaton>) -> Arc<Automaton> {
        // Concurrent misses may prepare the same source independently.
        if let Some(existing) = self.get(source) {
            return existing;
        }
        if self.entry_budget == 0
            || source.len() > MAX_CACHED_SOURCE_BYTES
            || source.len() > self.source_byte_budget
        {
            return automaton;
        }

        // FIFO depends on neither clocks nor tenant state. Eviction only drops
        // a reusable parse; it cannot alter a caller's independently owned AST.
        while self.entries.len() >= self.entry_budget
            || source.len() > self.source_byte_budget - self.source_bytes
        {
            let Some(oldest) = self.insertion_order.pop_front() else {
                return automaton;
            };
            self.entries.remove(oldest.as_ref());
            self.source_bytes -= oldest.len();
        }

        let source: Arc<str> = Arc::from(source);
        self.source_bytes += source.len();
        self.insertion_order.push_back(Arc::clone(&source));
        self.entries.insert(source, Arc::clone(&automaton));
        automaton
    }
}

pub(super) fn parse_cached(
    source: &str,
    mode: LivenessEnforcement,
) -> Result<Automaton, AutomatonParseError> {
    static CACHE: OnceLock<Mutex<PreparedAutomatonCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        Mutex::new(PreparedAutomatonCache::new(
            ENTRY_BUDGET,
            TOTAL_SOURCE_BYTE_BUDGET,
        ))
    });
    parse_with_cache(source, mode, liveness_reporter, cache)
}

fn parse_with_cache<'a>(
    source: &str,
    mode: LivenessEnforcement,
    reporter: impl FnOnce() -> Option<&'a LivenessViolationReporter>,
    cache: &Mutex<PreparedAutomatonCache>,
) -> Result<Automaton, AutomatonParseError> {
    let cached = cache.lock().ok().and_then(|cache| cache.get(source));
    let was_cached = cached.is_some();
    let automaton = match cached {
        Some(automaton) => automaton,
        None => Arc::new(prepare_automaton(source)?),
    };

    // Modes can change between calls, and reporters/warnings are observable on
    // every parse. Check outside the lock, even on a hit. Rejected parses do not
    // populate or evict cache entries.
    check_liveness_coverage(&automaton, mode, reporter)?;
    let automaton = if was_cached {
        automaton
    } else {
        match cache.lock() {
            Ok(mut cache) => cache.insert(source, automaton),
            Err(_) => automaton, // Poisoning disables only this acceleration.
        }
    };

    // Avoid an extra deep clone when the cache declined retention.
    Ok(Arc::try_unwrap(automaton).unwrap_or_else(|shared| (*shared).clone()))
}

#[cfg(test)]
mod tests;
