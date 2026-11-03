//! temper-optimize: Runtime optimization and adaptive tuning for Temper.
//!
//! Analyzes observability data to dynamically optimize query plans,
//! caching strategies, and state machine execution paths.
//!
//! # Architecture
//!
//! Self-driving optimizer actors observe production metrics via the
//! [`ObservabilityStore`](temper_observe::ObservabilityStore) trait and
//! autonomously produce [`OptimizationRecommendation`]s. Every recommendation
//! is validated by the [`SafetyChecker`] before it can be applied, ensuring
//! Tier 3 optimizations remain provably transparent to correctness.
//!
//! ## Modules
//!
//! - [`types`] -- Common optimization types (recommendations, actions, risk).
//! - [`query`] -- Query optimizer: detects N+1 patterns and slow queries.
//! - [`cache`] -- Cache optimizer: analyzes hit/miss rates and adjusts TTLs.
//! - [`safety`] -- Safety invariant checker: validates recommendations.

pub mod cache;
pub mod placement;
pub mod query;
pub mod safety;
pub mod types;

// Re-export the most commonly used types at the crate root.
pub use cache::CacheOptimizer;
pub use placement::PlacementOptimizer;
pub use query::QueryOptimizer;
pub use safety::{SafetyChecker, SafetyResult};
pub use types::{OptAction, OptCategory, OptimizationRecommendation, Risk};

#[cfg(test)]
mod tests {
    use serde_json::json;
    use temper_observe::InMemoryStore;

    use super::*;

    // ---- Safety checker tests ----

    #[test]
    fn test_safety_none_risk_always_safe() {
        let rec = OptimizationRecommendation {
            optimizer: "test".into(),
            description: "safe op".into(),
            category: OptCategory::CachePolicy,
            estimated_improvement: 0.01, // Even tiny improvement is fine.
            risk: Risk::None,
            action: OptAction::UpdateCacheTtl {
                key_pattern: "*".into(),
                ttl_seconds: 60,
            },
        };
        let result = SafetyChecker::validate(&rec);
        assert!(result.is_safe);
        assert!(result.reason.contains("No risk"));
    }

    #[test]
    fn test_safety_low_risk_safe_when_improvement_above_threshold() {
        let rec = OptimizationRecommendation {
            optimizer: "test".into(),
            description: "low risk good improvement".into(),
            category: OptCategory::QueryPlan,
            estimated_improvement: 0.3, // 30% > 10% threshold
            risk: Risk::Low,
            action: OptAction::UpdateQueryPlan {
                entity_set: "Products".into(),
                new_plan: "optimized".into(),
            },
        };
        let result = SafetyChecker::validate(&rec);
        assert!(result.is_safe);
        assert!(result.reason.contains("accepted"));
    }

    #[test]
    fn test_safety_low_risk_unsafe_when_improvement_below_threshold() {
        let rec = OptimizationRecommendation {
            optimizer: "test".into(),
            description: "low risk poor improvement".into(),
            category: OptCategory::QueryPlan,
            estimated_improvement: 0.05, // 5% < 10% threshold
            risk: Risk::Low,
            action: OptAction::UpdateQueryPlan {
                entity_set: "Products".into(),
                new_plan: "marginal".into(),
            },
        };
        let result = SafetyChecker::validate(&rec);
        assert!(!result.is_safe);
        assert!(result.reason.contains("rejected"));
    }

    #[test]
    fn test_safety_medium_risk_always_unsafe() {
        let rec = OptimizationRecommendation {
            optimizer: "test".into(),
            description: "medium risk op".into(),
            category: OptCategory::ActorPlacement,
            estimated_improvement: 0.9, // Even huge improvement is blocked.
            risk: Risk::Medium,
            action: OptAction::RebalanceShard {
                shard_id: 1,
                target_node: "node-2".into(),
            },
        };
        let result = SafetyChecker::validate(&rec);
        assert!(!result.is_safe);
        assert!(result.reason.contains("shadow testing"));
    }

    // ---- Query optimizer tests with mock store data ----

    #[tokio::test]
    async fn test_query_optimizer_detects_n_plus_one() {
        let store = InMemoryStore::new();

        // Simulate N+1 pattern: same operation repeated 5 times in one trace.
        for i in 0..5 {
            store.insert_span(vec![
                ("trace_id".into(), json!("trace-001")),
                ("span_id".into(), json!(format!("span-{i}"))),
                ("service".into(), json!("api")),
                ("operation".into(), json!("GET /Orders")),
                ("status".into(), json!("ok")),
                ("duration_ns".into(), json!(1_000_000)), // 1ms each
            ]);
        }

        let optimizer = QueryOptimizer::new();
        let recs = optimizer.analyze(&store).await;

        // Should produce at least one N+1 recommendation.
        assert!(!recs.is_empty(), "QueryOptimizer should detect N+1 pattern");
        let n_plus_one = recs
            .iter()
            .find(|r| r.description.contains("N+1"))
            .expect("should have an N+1 recommendation");
        assert_eq!(n_plus_one.category, OptCategory::QueryPlan);
        assert_eq!(n_plus_one.optimizer, "QueryOptimizer");
    }

    #[tokio::test]
    async fn test_query_optimizer_detects_slow_query() {
        let store = InMemoryStore::new();

        // Insert a slow span (100ms).
        store.insert_span(vec![
            ("trace_id".into(), json!("trace-002")),
            ("span_id".into(), json!("span-slow")),
            ("service".into(), json!("db")),
            ("operation".into(), json!("SELECT /Products")),
            ("status".into(), json!("ok")),
            ("duration_ns".into(), json!(100_000_000_i64)), // 100ms
        ]);

        let optimizer = QueryOptimizer::new();
        let recs = optimizer.analyze(&store).await;

        let slow = recs
            .iter()
            .find(|r| r.description.contains("Slow query"))
            .expect("should detect slow query");
        assert_eq!(slow.category, OptCategory::QueryPlan);
    }

    // ---- Cache optimizer tests with mock store data ----

    #[tokio::test]
    async fn test_cache_optimizer_detects_low_hit_rate() {
        let store = InMemoryStore::new();

        // Insert a metric with low cache hit rate.
        store.insert_metric(vec![
            ("metric_name".into(), json!("cache_hit_rate")),
            ("timestamp".into(), json!("2025-01-15T12:00:00Z")),
            ("value".into(), json!(0.25)), // 25% hit rate -- bad!
            (
                "tags".into(),
                json!({"key_pattern": "user:*", "service": "api"}),
            ),
        ]);

        let optimizer = CacheOptimizer::new();
        let recs = optimizer.analyze(&store).await;

        assert!(
            !recs.is_empty(),
            "CacheOptimizer should detect low hit rate"
        );
        let low_hit = recs
            .iter()
            .find(|r| r.description.contains("Low cache hit rate"))
            .expect("should have a low hit rate recommendation");
        assert_eq!(low_hit.category, OptCategory::CachePolicy);
        assert_eq!(low_hit.optimizer, "CacheOptimizer");
    }

    #[tokio::test]
    async fn test_cache_optimizer_detects_high_miss_count() {
        let store = InMemoryStore::new();

        // Insert a metric with high miss count.
        store.insert_metric(vec![
            ("metric_name".into(), json!("cache_miss_count")),
            ("timestamp".into(), json!("2025-01-15T12:00:00Z")),
            ("value".into(), json!(500.0)), // 500 misses
            (
                "tags".into(),
                json!({"key_pattern": "product:*", "service": "catalog"}),
            ),
        ]);

        let optimizer = CacheOptimizer::new();
        let recs = optimizer.analyze(&store).await;

        let high_miss = recs
            .iter()
            .find(|r| r.description.contains("High cache miss count"))
            .expect("should detect high miss count");
        assert_eq!(high_miss.category, OptCategory::CachePolicy);
    }
}
