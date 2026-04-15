//! Query optimizer actor.
//!
//! Analyzes OData query patterns from observability data and suggests
//! optimized query plans. Detects common anti-patterns such as N+1 query
//! sequences, slow queries, and missing `$expand` opportunities.

use temper_observe::store::ObservabilityStore;

use crate::types::{OptAction, OptCategory, OptimizationRecommendation, Risk};

/// Optimizer that analyzes OData query patterns and recommends plan improvements.
#[derive(Debug, Default)]
pub struct QueryOptimizer;

impl QueryOptimizer {
    /// Create a new query optimizer.
    pub fn new() -> Self {
        Self
    }

    /// Analyze query patterns from the observability store and produce
    /// optimization recommendations.
    ///
    /// The analyzer inspects span data to detect:
    /// - **N+1 patterns**: Repeated identical operations within the same trace
    ///   indicate missing `$expand` or eager-loading opportunities.
    /// - **Slow queries**: Operations whose duration exceeds a threshold
    ///   suggest the query plan should be rewritten.
    /// - **Missing $expand**: Navigation-property access patterns that could
    ///   be collapsed into a single `$expand` query.
    pub async fn analyze(
        &self,
        store: &impl ObservabilityStore,
    ) -> Vec<OptimizationRecommendation> {
        let mut recommendations = Vec::new();

        // Detect N+1 patterns by looking for repeated operations in spans.
        self.detect_n_plus_one(store, &mut recommendations).await;

        // Detect slow queries by examining span durations.
        self.detect_slow_queries(store, &mut recommendations).await;

        recommendations
    }

    /// Detect N+1 query patterns.
    ///
    /// Looks for spans with the same operation repeated many times, which
    /// indicates an entity set is being queried row-by-row instead of batched.
    async fn detect_n_plus_one(
        &self,
        store: &impl ObservabilityStore,
        recommendations: &mut Vec<OptimizationRecommendation>,
    ) {
        // Query all spans; the in-memory store supports SELECT * FROM spans.
        let result = store.query_spans("SELECT * FROM spans", &[]).await;

        let result_set = match result {
            Ok(rs) => rs,
            Err(_) => return,
        };

        // Group operations by trace_id to find repeated patterns.
        let mut trace_ops: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();

        let cols = &result_set.columns;
        for row in &result_set.rows {
            let trace_id = row
                .get_in(cols, "trace_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let operation = row
                .get_in(cols, "operation")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();

            if !trace_id.is_empty() && !operation.is_empty() {
                trace_ops.entry(trace_id).or_default().push(operation);
            }
        }

        // Detect repeated operations (N+1 pattern: same op appears 3+ times
        // in a single trace).
        for ops in trace_ops.values() {
            let mut counts: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for op in ops {
                *counts.entry(op.as_str()).or_insert(0) += 1;
            }

            for (op, count) in counts {
                if count >= 3 {
                    // Extract entity set name from the operation (e.g., "GET /Orders" -> "Orders").
                    let entity_set = op.split('/').next_back().unwrap_or(op).to_string();

                    recommendations.push(OptimizationRecommendation {
                        optimizer: "QueryOptimizer".to_string(),
                        description: format!(
                            "N+1 pattern detected: operation '{op}' repeated {count} times in a single trace. \
                             Consider using $expand to batch-load related entities."
                        ),
                        category: OptCategory::QueryPlan,
                        estimated_improvement: 0.4,
                        risk: Risk::Low,
                        action: OptAction::UpdateQueryPlan {
                            entity_set,
                            new_plan: format!("$expand=* (batch {count} queries into 1)"),
                        },
                    });
                }
            }
        }
    }

    /// Detect slow queries.
    ///
    /// Looks for spans with `duration_ns` above a threshold (50ms = 50_000_000 ns)
    /// and recommends query plan review.
    async fn detect_slow_queries(
        &self,
        store: &impl ObservabilityStore,
        recommendations: &mut Vec<OptimizationRecommendation>,
    ) {
        let result = store.query_spans("SELECT * FROM spans", &[]).await;

        let result_set = match result {
            Ok(rs) => rs,
            Err(_) => return,
        };

        let slow_threshold_ns: f64 = 50_000_000.0; // 50ms

        let cols = &result_set.columns;
        for row in &result_set.rows {
            let duration = row
                .get_in(cols, "duration_ns")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);

            if duration > slow_threshold_ns {
                let operation = row
                    .get_in(cols, "operation")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();

                let entity_set = operation
                    .split('/')
                    .next_back()
                    .unwrap_or(&operation)
                    .to_string();

                let duration_ms = duration / 1_000_000.0;

                recommendations.push(OptimizationRecommendation {
                    optimizer: "QueryOptimizer".to_string(),
                    description: format!(
                        "Slow query detected: '{operation}' took {duration_ms:.1}ms. \
                         Review query plan for optimization opportunities."
                    ),
                    category: OptCategory::QueryPlan,
                    estimated_improvement: 0.3,
                    risk: Risk::Low,
                    action: OptAction::UpdateQueryPlan {
                        entity_set,
                        new_plan: "optimize: add index or rewrite filter".to_string(),
                    },
                });
            }
        }
    }
}
