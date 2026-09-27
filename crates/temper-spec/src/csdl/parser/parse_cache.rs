//! Bounded reuse of successful CSDL parsing, independent of tenant state.
//!
//! Only exact XML source bytes share an immutable document. Record annotations
//! are never retained: their HashMaps must still be constructed on each parse
//! so cloning a cached map cannot change the emitter's iteration behavior.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use super::{AnnotationValue, CsdlDocument, CsdlParseError, parse_uncached};

const ENTRY_BUDGET: usize = 128;
const TOTAL_SOURCE_BYTE_BUDGET: usize = 2 * 1024 * 1024;
const MAX_CACHED_SOURCE_BYTES: usize = 128 * 1024;

struct ParsedCsdlCache {
    entries: BTreeMap<Arc<str>, Arc<CsdlDocument>>,
    insertion_order: VecDeque<Arc<str>>,
    source_bytes: usize,
    entry_budget: usize,
    source_byte_budget: usize,
}

impl ParsedCsdlCache {
    fn new(entry_budget: usize, source_byte_budget: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            insertion_order: VecDeque::new(),
            source_bytes: 0,
            entry_budget,
            source_byte_budget,
        }
    }

    fn get(&self, source: &str) -> Option<Arc<CsdlDocument>> {
        self.entries.get(source).map(Arc::clone)
    }

    fn insert(&mut self, source: &str, document: Arc<CsdlDocument>) -> Arc<CsdlDocument> {
        // Concurrent misses may parse the same source independently.
        if let Some(existing) = self.get(source) {
            return existing;
        }
        if self.entry_budget == 0
            || source.len() > MAX_CACHED_SOURCE_BYTES
            || source.len() > self.source_byte_budget
        {
            return document;
        }

        // FIFO does not depend on clocks, request state, or hit frequency.
        while self.entries.len() >= self.entry_budget
            || source.len() > self.source_byte_budget - self.source_bytes
        {
            let Some(oldest) = self.insertion_order.pop_front() else {
                return document;
            };
            self.entries.remove(oldest.as_ref());
            self.source_bytes -= oldest.len();
        }

        let source: Arc<str> = Arc::from(source);
        self.source_bytes += source.len();
        self.insertion_order.push_back(Arc::clone(&source));
        self.entries.insert(source, Arc::clone(&document));
        document
    }
}

fn contains_record_annotations(document: &CsdlDocument) -> bool {
    document.schemas.iter().any(|schema| {
        // These are all annotation-bearing types in CsdlDocument. Record
        // fields and Collection items are strings, so neither nests records.
        schema
            .annotations
            .iter()
            .chain(
                schema
                    .entity_types
                    .iter()
                    .flat_map(|entity| &entity.annotations),
            )
            .chain(schema.actions.iter().flat_map(|action| &action.annotations))
            .chain(
                schema
                    .functions
                    .iter()
                    .flat_map(|function| &function.annotations),
            )
            .chain(
                schema
                    .targeted_annotations
                    .iter()
                    .flat_map(|block| &block.annotations),
            )
            .any(|annotation| matches!(annotation.value, AnnotationValue::Record(_)))
    })
}

pub(super) fn parse_cached(source: &str) -> Result<CsdlDocument, CsdlParseError> {
    static CACHE: OnceLock<Mutex<ParsedCsdlCache>> = OnceLock::new();
    let cache = CACHE
        .get_or_init(|| Mutex::new(ParsedCsdlCache::new(ENTRY_BUDGET, TOTAL_SOURCE_BYTE_BUDGET)));
    parse_with_cache(source, cache)
}

fn parse_with_cache(
    source: &str,
    cache: &Mutex<ParsedCsdlCache>,
) -> Result<CsdlDocument, CsdlParseError> {
    let cached = cache.lock().ok().and_then(|cache| cache.get(source));
    let document = match cached {
        Some(document) => document,
        None => {
            // Parse errors and record documents cannot insert or evict entries.
            // Parsing and annotation inspection happen outside the cache lock.
            let document = Arc::new(parse_uncached(source)?);
            if contains_record_annotations(&document) {
                document
            } else {
                match cache.lock() {
                    Ok(mut cache) => cache.insert(source, document),
                    Err(_) => document, // Poisoning disables only this acceleration.
                }
            }
        }
    };

    // Clone outside the lock. Declined retention returns the original owned
    // document, including freshly constructed Record maps, without cloning.
    Ok(Arc::try_unwrap(document).unwrap_or_else(|shared| (*shared).clone()))
}

#[cfg(test)]
mod tests;
