//! Lightweight metrics collector for the /observe endpoints.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

/// Lightweight metrics collector for the /observe endpoints.
///
/// Uses atomic counters for totals and a `RwLock<BTreeMap>` for per-label
/// breakdowns. BTreeMap ensures deterministic iteration order (DST-safe).
pub struct MetricsCollector {
    /// Per-label transition counter: key = "entity_type:action:true|false".
    pub transitions: RwLock<BTreeMap<String, u64>>,
    /// Total successful + failed transitions.
    pub transitions_total: AtomicU64,
    /// Total failed transitions (guard not met, unknown action).
    pub errors_total: AtomicU64,
    /// Cross-invariant check outcomes: key = "tenant:entity_type:result".
    pub cross_invariant_checks: RwLock<BTreeMap<String, u64>>,
    /// Cross-invariant violations: key = "tenant:invariant:kind".
    pub cross_invariant_violations: RwLock<BTreeMap<String, u64>>,
    /// Relation integrity violations: key = "tenant:entity_type:operation".
    pub relation_integrity_violations: RwLock<BTreeMap<String, u64>>,
    /// Cross-invariant evaluation latency histogram buckets (ms).
    pub cross_invariant_eval_duration_ms_bucket: RwLock<BTreeMap<String, u64>>,
    /// Enforcement bypass count.
    pub cross_invariant_bypass_total: AtomicU64,
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsCollector {
    /// Create a new, empty collector.
    pub fn new() -> Self {
        Self {
            transitions: RwLock::new(BTreeMap::new()),
            transitions_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            cross_invariant_checks: RwLock::new(BTreeMap::new()),
            cross_invariant_violations: RwLock::new(BTreeMap::new()),
            relation_integrity_violations: RwLock::new(BTreeMap::new()),
            cross_invariant_eval_duration_ms_bucket: RwLock::new(BTreeMap::new()),
            cross_invariant_bypass_total: AtomicU64::new(0),
        }
    }

    /// Record a transition result.
    pub fn record_transition(&self, entity_type: &str, action: &str, success: bool) {
        let label = if success {
            format!("{entity_type}:{action}:true")
        } else {
            format!("{entity_type}:{action}:false")
        };
        if let Ok(mut map) = self.transitions.write() {
            *map.entry(label).or_insert(0) += 1;
        }
        self.transitions_total.fetch_add(1, Ordering::Relaxed);
        if !success {
            self.errors_total.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record outcome of a cross-invariant check.
    pub fn record_cross_invariant_check(&self, tenant: &str, entity_type: &str, result: &str) {
        let key = format!("{tenant}:{entity_type}:{result}");
        if let Ok(mut map) = self.cross_invariant_checks.write() {
            *map.entry(key).or_insert(0) += 1;
        }
    }

    /// Record a cross-invariant violation.
    pub fn record_cross_invariant_violation(&self, tenant: &str, invariant: &str, kind: &str) {
        let key = format!("{tenant}:{invariant}:{kind}");
        if let Ok(mut map) = self.cross_invariant_violations.write() {
            *map.entry(key).or_insert(0) += 1;
        }
    }

    /// Record a relation integrity violation.
    pub fn record_relation_integrity_violation(
        &self,
        tenant: &str,
        entity_type: &str,
        operation: &str,
    ) {
        let key = format!("{tenant}:{entity_type}:{operation}");
        if let Ok(mut map) = self.relation_integrity_violations.write() {
            *map.entry(key).or_insert(0) += 1;
        }
    }

    /// Record cross-invariant evaluation latency into Prometheus-style buckets.
    pub fn record_cross_eval_duration_ms(&self, duration_ms: u64) {
        const BUCKETS: &[u64] = &[1, 2, 5, 10, 20, 50, 100, 250, 500, 1000, 2500, 5000];
        let bucket = BUCKETS
            .iter()
            .find(|&&b| duration_ms <= b)
            .map(|b| b.to_string())
            .unwrap_or_else(|| "+Inf".to_string());
        if let Ok(mut map) = self.cross_invariant_eval_duration_ms_bucket.write() {
            *map.entry(bucket).or_insert(0) += 1;
        }
    }

    /// Record enforcement bypass usage.
    pub fn record_cross_bypass(&self) {
        self.cross_invariant_bypass_total
            .fetch_add(1, Ordering::Relaxed);
    }
}
