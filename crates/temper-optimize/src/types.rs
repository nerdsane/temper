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
