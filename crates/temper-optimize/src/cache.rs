//! Cache optimizer actor.
//!
//! Analyzes cache hit/miss rate metrics from the observability store and
//! suggests TTL adjustments to improve hit rates without serving stale data.

use temper_observe::store::ObservabilityStore;

use crate::types::{OptAction, OptCategory, OptimizationRecommendation, Risk};

/// Optimizer that analyzes cache metrics and recommends TTL adjustments.
#[derive(Debug, Default)]
pub struct CacheOptimizer;

impl CacheOptimizer {
    /// Create a new cache optimizer.
    pub fn new() -> Self {
        Self
    }

    /// Analyze cache metrics from the observability store and produce
    /// optimization recommendations.
    ///
    /// The analyzer inspects metric data to detect:
    /// - **Low hit rates**: Cache key patterns with hit rates below a threshold
    ///   suggest the TTL is too short.
    /// - **High miss counts**: Key patterns with excessive misses that could
    ///   benefit from cache warming or TTL increases.
    pub async fn analyze(
        &self,
        store: &impl ObservabilityStore,
    ) -> Vec<OptimizationRecommendation> {
        let mut recommendations = Vec::new();

        self.detect_low_hit_rates(store, &mut recommendations).await;
        self.detect_high_miss_rates(store, &mut recommendations)
            .await;

        recommendations
    }

    /// Detect cache key patterns with low hit rates by examining metrics.
    ///
    /// Looks for `cache_hit_rate` metrics below 0.5 (50%) and recommends
    /// increasing the TTL.
    async fn detect_low_hit_rates(
        &self,
        store: &impl ObservabilityStore,
        recommendations: &mut Vec<OptimizationRecommendation>,
    ) {
        let result = store
            .query_metrics(
                "SELECT * FROM metrics WHERE metric_name = 'cache_hit_rate'",
                &[],
            )
            .await;

        let result_set = match result {
            Ok(rs) => rs,
            Err(_) => return,
        };

        for row in &result_set.rows {
            let hit_rate = row
                .get_in(&result_set.columns, "value")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(1.0);

            // Extract key pattern from tags if available.
            let key_pattern = row
                .get_in(&result_set.columns, "tags")
                .and_then(|v| v.get("key_pattern"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("*")
                .to_string();

            if hit_rate < 0.5 {
                // Low hit rate: recommend increasing TTL.
                let improvement = 0.5 - hit_rate; // The lower the rate, the more to gain.

                recommendations.push(OptimizationRecommendation {
                    optimizer: "CacheOptimizer".to_string(),
                    description: format!(
                        "Low cache hit rate ({:.1}%) for pattern '{key_pattern}'. \
                         Consider increasing TTL to improve hit rate.",
                        hit_rate * 100.0
                    ),
                    category: OptCategory::CachePolicy,
                    estimated_improvement: improvement.min(0.5),
                    risk: Risk::Low,
                    action: OptAction::UpdateCacheTtl {
                        key_pattern,
                        ttl_seconds: 600, // Default: bump to 10 minutes.
                    },
                });
            }
        }
    }

    /// Detect high miss rates by examining cache miss count metrics.
    ///
    /// Looks for `cache_miss_count` metrics with high values and recommends
    /// cache strategy changes.
    async fn detect_high_miss_rates(
        &self,
        store: &impl ObservabilityStore,
        recommendations: &mut Vec<OptimizationRecommendation>,
    ) {
        let result = store
            .query_metrics(
                "SELECT * FROM metrics WHERE metric_name = 'cache_miss_count'",
                &[],
            )
            .await;

        let result_set = match result {
            Ok(rs) => rs,
            Err(_) => return,
        };

        for row in &result_set.rows {
            let miss_count = row
                .get_in(&result_set.columns, "value")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);

            let key_pattern = row
                .get_in(&result_set.columns, "tags")
                .and_then(|v| v.get("key_pattern"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("*")
                .to_string();

            // Threshold: more than 100 misses in the observation window.
            if miss_count > 100.0 {
                recommendations.push(OptimizationRecommendation {
                    optimizer: "CacheOptimizer".to_string(),
                    description: format!(
                        "High cache miss count ({miss_count:.0}) for pattern '{key_pattern}'. \
                         Consider warming the cache or increasing TTL."
                    ),
                    category: OptCategory::CachePolicy,
                    estimated_improvement: 0.2,
                    risk: Risk::None,
                    action: OptAction::UpdateCacheTtl {
                        key_pattern,
                        ttl_seconds: 900, // 15 minutes.
                    },
                });
            }
        }
    }
}
