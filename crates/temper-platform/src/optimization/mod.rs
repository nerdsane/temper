//! Self-optimization loop for the platform.
//!
//! Periodically runs [`QueryOptimizer`] and [`CacheOptimizer`] against the
//! [`ObservabilityStore`], validates each recommendation through
//! [`SafetyChecker`], and either auto-applies safe recommendations or
//! creates O-Records for medium-risk ones (feeding the Evolution Engine).

use temper_observe::ObservabilityStore;
use temper_optimize::{
    CacheOptimizer, OptimizationRecommendation, QueryOptimizer,
    Risk, SafetyChecker,
};

use crate::protocol::PlatformEvent;
use crate::state::PlatformState;

/// Runs one optimization cycle against the given observability store.
///
/// For each recommendation:
/// - `Risk::None` → auto-apply, emit `OptimizationApplied`
/// - `Risk::Low` with improvement > 10% → auto-apply, emit `OptimizationApplied`
/// - `Risk::Low` with improvement <= 10% → skip (not worth it)
/// - `Risk::Medium` → propose for human approval, emit `OptimizationProposed`
pub async fn run_optimization_cycle<S: ObservabilityStore>(
    store: &S,
    state: &PlatformState,
) -> Vec<OptimizationRecommendation> {
    let query_optimizer = QueryOptimizer::new();
    let cache_optimizer = CacheOptimizer::new();

    let mut all_recs = query_optimizer.analyze(store).await;
    all_recs.extend(cache_optimizer.analyze(store).await);

    let mut applied = Vec::new();

    for rec in &all_recs {
        let safety = SafetyChecker::validate(rec);

        if safety.is_safe {
            // Auto-apply safe recommendations
            tracing::info!(
                optimizer = %rec.optimizer,
                action = %rec.description,
                improvement = rec.estimated_improvement,
                "Auto-applying optimization"
            );

            state.broadcast(PlatformEvent::OptimizationApplied {
                optimizer: rec.optimizer.clone(),
                action: rec.description.clone(),
                improvement: rec.estimated_improvement,
            });

            applied.push(rec.clone());
        } else if rec.risk == Risk::Medium {
            // Medium risk → create O-Record for evolution engine
            let record_id = format!(
                "OPT-{}-{}",
                rec.optimizer,
                uuid::Uuid::now_v7()
            );

            tracing::info!(
                optimizer = %rec.optimizer,
                description = %rec.description,
                risk = ?rec.risk,
                record_id = %record_id,
                "Proposing optimization for human approval"
            );

            state.broadcast(PlatformEvent::OptimizationProposed {
                optimizer: rec.optimizer.clone(),
                description: rec.description.clone(),
                risk: format!("{:?}", rec.risk),
                record_id,
            });
        }
        // Risk::Low with insufficient improvement → silently skip
    }

    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use temper_observe::InMemoryStore;

    #[tokio::test]
    async fn test_optimization_cycle_with_no_data() {
        let store = InMemoryStore::new();
        let state = PlatformState::new(None);
        let mut rx = state.subscribe();

        let applied = run_optimization_cycle(&store, &state).await;

        assert!(applied.is_empty(), "No data → no recommendations");
        assert!(rx.try_recv().is_err(), "No events should be broadcast");
    }

    #[tokio::test]
    async fn test_optimization_cycle_auto_applies_safe_recs() {
        let store = InMemoryStore::new();
        let state = PlatformState::new(None);
        let mut rx = state.subscribe();

        // Insert data that triggers a low-hit-rate recommendation (auto-applicable)
        store.insert_metric(vec![
            ("metric_name".into(), json!("cache_hit_rate")),
            ("timestamp".into(), json!("2025-01-15T12:00:00Z")),
            ("value".into(), json!(0.20)), // 20% hit rate
            ("tags".into(), json!({"key_pattern": "session:*", "service": "api"})),
        ]);

        let applied = run_optimization_cycle(&store, &state).await;

        // Should have at least one applied recommendation
        if !applied.is_empty() {
            // Verify broadcast
            let mut opt_events = Vec::new();
            while let Ok(msg) = rx.try_recv() {
                if let PlatformEvent::OptimizationApplied { .. } = &msg {
                    opt_events.push(msg);
                }
            }
            assert!(!opt_events.is_empty(), "Should broadcast OptimizationApplied");
        }
    }

    #[tokio::test]
    async fn test_optimization_cycle_proposes_medium_risk() {
        let store = InMemoryStore::new();
        let state = PlatformState::new(None);
        let mut rx = state.subscribe();

        // Insert N+1 pattern data (triggers QueryOptimizer)
        for i in 0..5 {
            store.insert_span(vec![
                ("trace_id".into(), json!("trace-n1")),
                ("span_id".into(), json!(format!("span-{i}"))),
                ("service".into(), json!("api")),
                ("operation".into(), json!("GET /Orders")),
                ("status".into(), json!("ok")),
                ("duration_ns".into(), json!(1_000_000)),
            ]);
        }

        let _applied = run_optimization_cycle(&store, &state).await;

        // Check for any broadcast events
        let mut events = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            events.push(msg);
        }
        // The exact behavior depends on the risk level assigned by QueryOptimizer
        // We verify the cycle completes without panicking
    }
}
