//! Common optimization types shared across all optimizer actors.

use serde::{Deserialize, Serialize};

/// An optimization recommendation produced by an optimizer actor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizationRecommendation {
    /// Which optimizer produced this recommendation.
    pub optimizer: String,
    /// Human-readable description of the recommendation.
    pub description: String,
    /// The category of optimization.
    pub category: OptCategory,
    /// Estimated improvement factor (0.0 -- 1.0).
    pub estimated_improvement: f64,
    /// The risk level of applying this optimization.
    pub risk: Risk,
    /// The concrete action to apply.
    pub action: OptAction,
}

/// Categories of optimization that the system can perform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OptCategory {
    /// Query plan optimization (rewriting OData queries).
    QueryPlan,
    /// Cache policy optimization (TTL adjustments, eviction).
    CachePolicy,
    /// Actor placement optimization (shard rebalancing).
    ActorPlacement,
    /// Batch strategy optimization (batch size tuning).
    BatchStrategy,
    /// Policy evaluation ordering optimization.
    PolicyEval,
}

/// Risk level associated with an optimization recommendation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Risk {
    /// No risk -- purely additive, cannot affect correctness.
    None,
    /// Low risk -- minor behavioural change, easily reversible.
    Low,
    /// Medium risk -- requires shadow testing before production rollout.
    Medium,
}

/// A concrete optimization action to be applied to the running system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OptAction {
    /// Replace the query plan for an entity set.
    UpdateQueryPlan {
        /// The entity set whose query plan should change.
        entity_set: String,
        /// The new query plan (opaque string representation).
        new_plan: String,
    },
    /// Adjust the TTL for a cache key pattern.
    UpdateCacheTtl {
        /// Glob-style pattern matching cache keys.
        key_pattern: String,
        /// New time-to-live in seconds.
        ttl_seconds: u64,
    },
    /// Rebalance a shard to a different node.
    RebalanceShard {
        /// The shard to move.
        shard_id: u32,
        /// The target node identifier.
        target_node: String,
    },
    /// Update the batch size for an entity type.
    UpdateBatchSize {
        /// The entity type whose batch size should change.
        entity_type: String,
        /// The new batch size.
        new_size: usize,
    },
    /// Reorder policy evaluation to improve throughput.
    ReorderPolicies {
        /// New ordering of policy identifiers (most selective first).
        policy_ids: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_category_serde_roundtrip() {
        for cat in [
            OptCategory::QueryPlan,
            OptCategory::CachePolicy,
            OptCategory::ActorPlacement,
            OptCategory::BatchStrategy,
            OptCategory::PolicyEval,
        ] {
            let json = serde_json::to_string(&cat).unwrap();
            let back: OptCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(back, cat);
        }
    }

    #[test]
    fn risk_serde_roundtrip() {
        for risk in [Risk::None, Risk::Low, Risk::Medium] {
            let json = serde_json::to_string(&risk).unwrap();
            let back: Risk = serde_json::from_str(&json).unwrap();
            assert_eq!(back, risk);
        }
    }

    #[test]
    fn opt_action_update_query_plan() {
        let action = OptAction::UpdateQueryPlan {
            entity_set: "Orders".into(),
            new_plan: "idx_scan".into(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let back: OptAction = serde_json::from_str(&json).unwrap();
        match back {
            OptAction::UpdateQueryPlan {
                entity_set,
                new_plan,
            } => {
                assert_eq!(entity_set, "Orders");
                assert_eq!(new_plan, "idx_scan");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn opt_action_update_cache_ttl() {
        let action = OptAction::UpdateCacheTtl {
            key_pattern: "entity:*".into(),
            ttl_seconds: 300,
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains("300"));
    }

    #[test]
    fn recommendation_serde_roundtrip() {
        let rec = OptimizationRecommendation {
            optimizer: "query".into(),
            description: "Add index".into(),
            category: OptCategory::QueryPlan,
            estimated_improvement: 0.5,
            risk: Risk::Low,
            action: OptAction::UpdateQueryPlan {
                entity_set: "Orders".into(),
                new_plan: "new".into(),
            },
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: OptimizationRecommendation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.optimizer, "query");
        assert_eq!(back.category, OptCategory::QueryPlan);
        assert_eq!(back.risk, Risk::Low);
    }
}
